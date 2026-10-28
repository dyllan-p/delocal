//! The tier poll in the daemon (DESIGN.md §6.4): a tier change in the fake
//! Tailscale reaches the engine, through the binding of the peer's node ID
//! to its Tailscale node (§5).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use delocal_engine::{
    ContentHash, Entry, Event, Kind, NodeId, Outbound, Tier, Timestamp, WantState,
};

use super::super::core::Input;
use super::super::{Daemon, Fetch, Outbox, STORE};
use super::{Fixture, batch, from_peer, p};
use crate::fs::{Folder, RealFolder};
use crate::store::{Group, HostWrite, Machine, Store};
use crate::tailscale::fake::{Config, FakeTailscale};
use crate::tailscale::{PathInfo, Peer, SelfInfo, StableId, Tailscale};

/// How often these daemons poll: often enough to wait for.
const POLL: Duration = Duration::from_millis(50);

/// Fetches, kept for the test to read; messages are dropped.
#[derive(Clone, Default)]
struct Fetches(Arc<Mutex<Vec<Fetch>>>);

impl Outbox for Fetches {
    fn send(&mut self, _: NodeId, _: Outbound) {}

    fn fetch(&mut self, fetch: Fetch) {
        self.0.lock().unwrap().push(fetch);
    }
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// The fixture's peer's Tailscale node.
fn stable() -> StableId {
    StableId("nPeerCNTRL".to_owned())
}

fn relayed() -> Option<PathInfo> {
    Some(PathInfo {
        direct: None,
        relay: Some("fra".to_owned()),
    })
}

fn on_the_lan() -> Option<PathInfo> {
    Some(PathInfo {
        direct: Some(addr("192.168.1.20:41641")),
        relay: Some("fra".to_owned()),
    })
}

fn direct() -> Option<PathInfo> {
    Some(PathInfo {
        direct: Some(addr("203.0.113.7:41641")),
        relay: Some("fra".to_owned()),
    })
}

/// A fake Tailscale with one peer, the node [`stable`], reached by `path`.
fn tailscale(path: Option<PathInfo>) -> Arc<FakeTailscale> {
    Arc::new(FakeTailscale::new(Config {
        self_info: SelfInfo {
            addrs: vec![addr("127.0.0.1:41831")],
            stable_id: StableId("nHereCNTRL".to_owned()),
            user: "me@example.com".to_owned(),
            hostname: "here".to_owned(),
        },
        peers: vec![Peer {
            hostname: "peer".to_owned(),
            addrs: vec![addr("127.0.0.1:41832")],
            stable_id: stable(),
            user: "me@example.com".to_owned(),
            online: true,
            tags: Vec::new(),
            path,
        }],
        whois: BTreeMap::new(),
        running: true,
    }))
}

/// The `machines` row binding `node` to [`stable`] (§5).
fn binding(node: NodeId) -> Machine {
    Machine {
        node,
        hostname: "peer".to_owned(),
        ts_stable_id: stable().0,
        ts_user: "me@example.com".to_owned(),
        trusted: true,
        last_seen: Timestamp::from_unix_nanos(1),
        delocal_version: "0.1.0".to_owned(),
    }
}

/// A daemon on the fixture's folder that polls `tailscale` every [`POLL`],
/// once its startup scan is done.
fn start(fixture: &Fixture, tailscale: &Arc<FakeTailscale>, outbox: Box<dyn Outbox>) -> Daemon {
    let folder: Arc<dyn Folder> = Arc::new(RealFolder::new(fixture.root()));
    let mut setup = fixture.setup(folder, outbox);
    setup.tailscale = Some(Arc::clone(tailscale) as Arc<dyn Tailscale>);
    setup.tier_poll = POLL;
    let daemon = Daemon::start(setup).unwrap();
    daemon.settle();
    daemon
}

impl Daemon {
    /// Hand the engine task host writes, as a transport's would come.
    fn write(&self, write: HostWrite) {
        self.inputs
            .blocking_send(Input::Writes(vec![write]))
            .unwrap();
    }
}

/// The engine's tier for `peer`, `None` if it is not connected.
fn tier(daemon: &Daemon, peer: NodeId) -> Option<Tier> {
    daemon.engine(move |engine| engine.peers().find(|(n, _)| *n == peer).map(|(_, t)| t))
}

/// Wait until the engine has `peer` at `want`, after `what`.
fn wait_for(daemon: &Daemon, peer: NodeId, want: Tier, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let tier = tier(daemon, peer);
        if tier == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: never {want:?}, still {tier:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The peer's add of a 200 MiB file: over the relay limit, within the
/// direct one (§6.5).
fn big(peer: NodeId) -> Entry {
    Entry {
        kind: Kind::File,
        size: 200 << 20,
        mtime_ns: 1_000_000_000,
        stamp: 1_000_000_000,
        hash: ContentHash::from_bytes([7; 32]),
        ..from_peer(peer, "big", None)
    }
}

/// §6.4: a tier change in the fake reaches the engine. The peer is bound
/// to its Tailscale node in the store before the daemon starts, connects
/// on relay, and sends a 200 MiB file, which waits for a better tier
/// (§6.5) through several polls that find nothing new. When the fake moves
/// the peer onto the LAN, the next poll tells the engine, and the file is
/// fetched.
#[test]
fn a_tier_change_in_the_fake_reaches_the_engine() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.state().join(STORE)).unwrap();
    let mut group = Group::new(Timestamp::from_unix_nanos(1));
    group.host(HostWrite::PutMachine(binding(fixture.peer)));
    store.commit(&[group]).unwrap();
    drop(store);
    let tailscale = tailscale(relayed());
    let fetches = Fetches::default();
    let daemon = start(&fixture, &tailscale, Box::new(fetches.clone()));

    daemon.inject(Event::PeerConnected {
        peer: fixture.peer,
        tier: Tier::Relay,
    });
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(fixture.peer, fixture.folder, 0, vec![big(fixture.peer)]),
    });
    daemon.settle();
    std::thread::sleep(POLL * 5);
    daemon.settle();
    let folder = fixture.folder;
    let state = daemon.engine(move |engine| {
        engine
            .folder(folder)
            .and_then(|f| f.wants().get(&p("big")))
            .map(|want| want.state.clone())
    });
    assert_eq!(state, Some(WantState::Deferred { need: Tier::Direct }));
    assert_eq!(tier(&daemon, fixture.peer), Some(Tier::Relay));
    assert!(fetches.0.lock().unwrap().is_empty());

    tailscale.update(|config| config.peers[0].path = on_the_lan());
    wait_for(&daemon, fixture.peer, Tier::Lan, "on the lan");
    daemon.settle();
    let fetched = fetches.0.lock().unwrap().clone();
    assert_eq!(fetched.len(), 1, "{fetched:?}");
    assert_eq!(fetched[0].path, p("big"));
    assert_eq!(fetched[0].from, fixture.peer);
    assert_eq!(fetched[0].size, 200 << 20);
    daemon.stop().unwrap();
}

/// What a step does to the fake, and the tier the engine then has.
type Step = (&'static str, fn(&mut Config), Tier);

/// The engine follows every tier the fake gives the peer, starting from a
/// connection whose tier the first poll corrects: the LAN, a public
/// address, no path info, the LAN again, Tailscale not answering, and
/// Tailscale no longer listing the peer.
#[test]
fn the_engine_follows_every_tier_the_fake_gives() {
    let fixture = Fixture::new();
    let tailscale = tailscale(on_the_lan());
    let daemon = start(&fixture, &tailscale, Box::new(Fetches::default()));
    daemon.write(HostWrite::PutMachine(binding(fixture.peer)));
    daemon.inject(Event::PeerConnected {
        peer: fixture.peer,
        tier: Tier::Relay,
    });
    wait_for(&daemon, fixture.peer, Tier::Lan, "connected");

    let steps: [Step; 6] = [
        ("direct", |c| c.peers[0].path = direct(), Tier::Direct),
        ("no path info", |c| c.peers[0].path = None, Tier::Relay),
        ("lan again", |c| c.peers[0].path = on_the_lan(), Tier::Lan),
        ("not running", |c| c.running = false, Tier::Relay),
        ("running again", |c| c.running = true, Tier::Lan),
        ("not listed", |c| c.peers.clear(), Tier::Relay),
    ];
    for (what, change, want) in steps {
        tailscale.update(change);
        wait_for(&daemon, fixture.peer, want, what);
    }
    daemon.stop().unwrap();
}

/// The binding is the `machines` table as the daemon writes it: a peer
/// with none is relay, one written while running counts at the next poll,
/// and one removed counts no more. And a peer that has disconnected is
/// never named by a poll, since the engine takes a tier for a peer that is
/// not connected as a connection.
#[test]
fn bindings_count_as_written_and_a_disconnected_peer_stays_gone() {
    let fixture = Fixture::new();
    let tailscale = tailscale(on_the_lan());
    let daemon = start(&fixture, &tailscale, Box::new(Fetches::default()));
    daemon.inject(Event::PeerConnected {
        peer: fixture.peer,
        tier: Tier::Direct,
    });
    wait_for(&daemon, fixture.peer, Tier::Relay, "no binding");

    daemon.write(HostWrite::PutMachine(binding(fixture.peer)));
    wait_for(&daemon, fixture.peer, Tier::Lan, "bound");
    daemon.write(HostWrite::RemoveMachine(fixture.peer));
    wait_for(&daemon, fixture.peer, Tier::Relay, "unbound");
    daemon.write(HostWrite::PutMachine(binding(fixture.peer)));
    wait_for(&daemon, fixture.peer, Tier::Lan, "bound again");

    daemon.inject(Event::PeerDisconnected { peer: fixture.peer });
    tailscale.update(|config| config.peers[0].path = direct());
    std::thread::sleep(POLL * 5);
    daemon.settle();
    assert_eq!(daemon.engine(|engine| engine.peers().count()), 0);
    daemon.stop().unwrap();
}
