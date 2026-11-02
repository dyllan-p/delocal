//! A fake Tailscale (DESIGN.md §6.1, §14.2), made from a config, for tests.
//!
//! [`FakeTailscale`] answers from a [`Config`]: this machine, its peers
//! with their path info, and a whois table by IP. A test changes the
//! config while the fake is in use ([`FakeTailscale::update`]): a peer's
//! path, to move its tier (§6.4), or `running`, to make Tailscale stop
//! answering (§13).
//!
//! **One host, many machines.** [`loopback`] makes one config per machine,
//! each seeing the others as peers, and gives every machine its own
//! loopback address (127.0.0.2, 127.0.0.3, …) on one port, as Tailscale
//! gives every machine its own IP (§6.1, §14.2). Whois resolves an address
//! by its IP alone: a machine dials from its own address, so the address a
//! connection comes from names the machine, whatever port the system gave
//! it. Linux answers all of 127.0.0.0/8; macOS answers only 127.0.0.1 until
//! the others are aliased, which `scripts/loopback.sh` does.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Answer, PathInfo, Peer, SelfInfo, StableId, Tailscale, TailscaleError, WhoIs};

/// What a [`FakeTailscale`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub self_info: SelfInfo,
    /// Each with its path info.
    pub peers: Vec<Peer>,
    /// Who is at each IP, whatever the port.
    pub whois: BTreeMap<IpAddr, WhoIs>,
    /// False: every method fails as if Tailscale were not running.
    pub running: bool,
}

/// See the module docs.
#[derive(Debug)]
pub struct FakeTailscale {
    config: Mutex<Config>,
}

impl FakeTailscale {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
        }
    }

    /// Change the config. Every answer begun after this returns follows
    /// the change.
    pub fn update(&self, change: impl FnOnce(&mut Config)) {
        change(&mut self.lock());
    }

    /// The config as it stands.
    pub fn config(&self) -> Config {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Config> {
        // A test that panicked while changing the config leaves it as the
        // change left it, which is still a config.
        self.config.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An answer from the config as it stands now, or `Unavailable` if
    /// Tailscale is not running. Read before the future is made, so no
    /// lock is held across an await.
    fn answer<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Config) -> Result<T, TailscaleError>,
    ) -> Answer<'_, T> {
        let config = self.lock();
        let answer = if config.running {
            read(&config)
        } else {
            Err(TailscaleError::Unavailable("not running".to_owned()))
        };
        Box::pin(std::future::ready(answer))
    }
}

impl Tailscale for FakeTailscale {
    fn self_info(&self) -> Answer<'_, SelfInfo> {
        self.answer(|config| Ok(config.self_info.clone()))
    }

    fn peers(&self) -> Answer<'_, Vec<Peer>> {
        self.answer(|config| Ok(config.peers.clone()))
    }

    fn whois(&self, addr: SocketAddr) -> Answer<'_, WhoIs> {
        self.answer(|config| {
            config
                .whois
                .get(&addr.ip())
                .cloned()
                .ok_or(TailscaleError::Unknown(addr))
        })
    }
}

/// One machine of a [`loopback`] tailnet: its hostname, and its own
/// loopback address.
#[derive(Clone, Copy, Debug)]
pub struct Machine<'a> {
    pub hostname: &'a str,
    /// In 127.0.0.0/8, and not 127.0.0.1, which is every machine's.
    pub ip: Ipv4Addr,
}

impl Machine<'_> {
    /// Its stable ID: `fake-` and its hostname.
    pub fn stable_id(&self) -> StableId {
        StableId(format!("fake-{}", self.hostname))
    }

    /// Where its daemon listens, on the tailnet's one port.
    pub fn addr(&self, port: u16) -> SocketAddr {
        SocketAddr::from((self.ip, port))
    }

    /// Its path as a peer sees it: direct, at an address in 10/8 that ends
    /// as its own does, on the same port, so every peer is `lan` (§6.4).
    pub fn lan(&self, port: u16) -> PathInfo {
        let [.., last] = self.ip.octets();
        PathInfo {
            direct: Some(SocketAddr::from(([10, 0, 0, last], port))),
            relay: None,
        }
    }
}

/// One [`Config`] per machine of a tailnet on one host (§14.2), in the
/// order given, every machine at its own address on `port`. Each has its
/// machine as itself and every other as a peer: online, owned by `user`,
/// untagged, and on the same LAN. Each resolves every machine's IP by
/// whois, its own included, as Tailscale does.
pub fn loopback(user: &str, port: u16, machines: &[Machine<'_>]) -> Vec<Config> {
    let whois: BTreeMap<IpAddr, WhoIs> = machines
        .iter()
        .map(|machine| {
            let who = WhoIs {
                stable_id: machine.stable_id(),
                user: user.to_owned(),
                tags: Vec::new(),
            };
            (IpAddr::V4(machine.ip), who)
        })
        .collect();
    machines
        .iter()
        .map(|me| Config {
            self_info: SelfInfo {
                addrs: vec![me.addr(port)],
                stable_id: me.stable_id(),
                user: user.to_owned(),
                hostname: me.hostname.to_owned(),
            },
            peers: machines
                .iter()
                .filter(|peer| peer.ip != me.ip)
                .map(|peer| Peer {
                    hostname: peer.hostname.to_owned(),
                    addrs: vec![peer.addr(port)],
                    stable_id: peer.stable_id(),
                    user: user.to_owned(),
                    online: true,
                    tags: Vec::new(),
                    path: Some(peer.lan(port)),
                })
                .collect(),
            whois: whois.clone(),
            running: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use delocal_engine::Tier;

    use super::*;
    use crate::tailscale::tier::Tiers;

    const PORT: u16 = 41831;

    fn machines() -> [Machine<'static>; 3] {
        [
            Machine {
                hostname: "desktop",
                ip: Ipv4Addr::new(127, 0, 0, 2),
            },
            Machine {
                hostname: "laptop",
                ip: Ipv4Addr::new(127, 0, 0, 3),
            },
            Machine {
                hostname: "pi",
                ip: Ipv4Addr::new(127, 0, 0, 4),
            },
        ]
    }

    /// Three machines on one host, each at its own loopback address on one
    /// port: each sees itself at its address and the others at theirs,
    /// every peer is `lan`, and whois tells them apart by IP.
    #[tokio::test]
    async fn a_loopback_tailnet_gives_each_machine_its_own_address() {
        let machines = machines();
        let fakes: Vec<FakeTailscale> = loopback("me@example.com", PORT, &machines)
            .into_iter()
            .map(FakeTailscale::new)
            .collect();
        for (me, fake) in machines.iter().zip(&fakes) {
            let this = fake.self_info().await.unwrap();
            assert_eq!(this.addrs, [me.addr(PORT)]);
            assert_eq!(this.stable_id, me.stable_id());
            assert_eq!(this.hostname, me.hostname);

            let peers = fake.peers().await.unwrap();
            let others: Vec<&Machine<'_>> = machines.iter().filter(|m| m.ip != me.ip).collect();
            assert_eq!(peers.len(), others.len());
            for (peer, other) in peers.iter().zip(&others) {
                assert_eq!(peer.addrs, [other.addr(PORT)]);
                assert_eq!(peer.stable_id, other.stable_id());
                assert!(peer.online);
                assert_eq!(peer.user, "me@example.com");
            }
            let tiers = Tiers::of(&peers);
            for other in &others {
                assert_eq!(tiers.get(&other.stable_id()), Tier::Lan);
            }

            for machine in &machines {
                let who = fake.whois(machine.addr(PORT)).await.unwrap();
                assert_eq!(who.stable_id, machine.stable_id());
                assert_eq!(who.user, "me@example.com");
            }
        }
        let addrs: Vec<SocketAddr> = machines.iter().map(|m| m.addr(PORT)).collect();
        assert!(addrs.iter().all(|addr| addr.port() == PORT));
        assert_eq!(
            addrs.iter().map(SocketAddr::ip).collect::<Vec<_>>(),
            [[127, 0, 0, 2], [127, 0, 0, 3], [127, 0, 0, 4]].map(IpAddr::from)
        );
    }

    /// Whois resolves by IP: a connection from a machine's address names
    /// it whatever port it came from, and an IP no machine has is
    /// `Unknown`, 127.0.0.1 included. With Tailscale stopped, every method
    /// is `Unavailable` until it runs again.
    #[tokio::test]
    async fn whois_resolves_by_ip_and_a_stopped_tailscale_answers_nothing() {
        let machines = machines();
        let fake = FakeTailscale::new(loopback("me@example.com", PORT, &machines).remove(0));
        let dialed_from = SocketAddr::from((machines[1].ip, 50123));
        assert_eq!(
            fake.whois(dialed_from).await.unwrap().stable_id,
            machines[1].stable_id()
        );
        for stranger in [
            SocketAddr::from((Ipv4Addr::new(127, 0, 0, 9), PORT)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 50123)),
        ] {
            assert_eq!(
                fake.whois(stranger).await,
                Err(TailscaleError::Unknown(stranger))
            );
        }

        fake.update(|config| config.running = false);
        let down = TailscaleError::Unavailable("not running".to_owned());
        assert_eq!(fake.self_info().await, Err(down.clone()));
        assert_eq!(fake.peers().await, Err(down.clone()));
        assert_eq!(fake.whois(machines[1].addr(PORT)).await, Err(down));

        fake.update(|config| config.running = true);
        assert_eq!(fake.peers().await.unwrap().len(), 2);
        assert!(fake.config().running);
    }

    /// A change to the config is in every answer begun after it.
    #[tokio::test]
    async fn an_update_changes_later_answers() {
        let machines = machines();
        let fake = FakeTailscale::new(loopback("me@example.com", PORT, &machines).remove(0));
        let laptop = machines[1].stable_id();
        let relayed = PathInfo {
            direct: None,
            relay: Some("fra".to_owned()),
        };
        fake.update(|config| {
            for peer in &mut config.peers {
                if peer.stable_id == laptop {
                    peer.path = Some(relayed.clone());
                }
            }
        });
        let peers = fake.peers().await.unwrap();
        assert_eq!(Tiers::of(&peers).get(&laptop), Tier::Relay);
        assert_eq!(Tiers::of(&peers).get(&machines[2].stable_id()), Tier::Lan);
    }
}
