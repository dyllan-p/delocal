//! The mesh over real TCP on one host (DESIGN.md §6.3, §5, §14.2).
//!
//! **Whois here** is the fake's (§6.1). Every machine is at its own
//! loopback address, and whois names the machine at the IP a connection
//! came from, so a machine played by hand dials from its own address too.
//! Machines 1, 2 and 3 are at 127.0.0.4, 127.0.0.5 and 127.0.0.6, on ports
//! the system picks. 127.0.0.2 and 127.0.0.3 are left to the one test that
//! puts two machines on one port, as two daemons are (§14.2), so that no
//! other test holds a port there. On macOS, `scripts/loopback.sh` adds
//! these addresses.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use delocal_engine::{HostName, NodeId};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;
use crate::net::frame::{self, FrameError, Reader};
use crate::net::wire::{Message, PROTOCOL, Reason, RequestId};
use crate::store::sample;
use crate::tailscale::fake::{self, FakeTailscale};
use crate::tailscale::{StableId, WhoIs};

const USER: &str = "me@example.com";

/// Short enough that a test can wait out several attempts.
const BACKOFF: Backoff = Backoff {
    first: Duration::from_millis(100),
    cap: Duration::from_millis(800),
};

/// Machines 1, 2 and 3; see the module docs.
const MACHINES: [fake::Machine<'static>; 3] = [
    fake::Machine {
        hostname: "m1",
        ip: Ipv4Addr::new(127, 0, 0, 4),
    },
    fake::Machine {
        hostname: "m2",
        ip: Ipv4Addr::new(127, 0, 0, 5),
    },
    fake::Machine {
        hostname: "m3",
        ip: Ipv4Addr::new(127, 0, 0, 6),
    },
];

fn machine(n: u8) -> fake::Machine<'static> {
    MACHINES[usize::from(n - 1)]
}

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(machine(n).ip)
}

fn node(n: u8) -> NodeId {
    sample::node(n)
}

fn stable(n: u8) -> StableId {
    machine(n).stable_id()
}

/// Machine `n`, untagged, of this user.
fn who(n: u8) -> WhoIs {
    WhoIs {
        stable_id: stable(n),
        user: USER.to_owned(),
        tags: Vec::new(),
    }
}

fn hello(n: u8) -> Hello {
    Hello {
        proto: PROTOCOL,
        node: node(n),
        hostname: HostName::new(format!("m{n}")).unwrap(),
        version: "0.1.0".to_owned(),
        now: sample::at(0),
    }
}

/// Machine `n`'s Tailscale: the fake's, which knows machines 1, 2 and 3.
fn fake(n: u8) -> Arc<FakeTailscale> {
    let config = fake::loopback(USER, 0, &MACHINES).remove(usize::from(n - 1));
    Arc::new(FakeTailscale::new(config))
}

/// Machine `n`, with [`fake`]'s Tailscale.
fn setup(n: u8) -> Setup {
    Setup {
        node: node(n),
        hostname: HostName::new(format!("m{n}")).unwrap(),
        tailscale: fake(n),
        clock: Arc::new(|| sample::at(0)),
        trusted: BTreeSet::new(),
        known: BTreeMap::new(),
        backoff: BACKOFF,
    }
}

struct Machine {
    n: u8,
    transport: Transport,
    reports: mpsc::Receiver<Report>,
}

impl Machine {
    async fn start(n: u8, setup: Setup) -> Self {
        let (transport, reports) = Transport::start(setup)
            .await
            .expect("listening; on macOS, run scripts/loopback.sh first");
        Self {
            n,
            transport,
            reports,
        }
    }

    /// This machine, for another to dial.
    fn target(&self) -> Target {
        Target {
            stable_id: stable(self.n),
            addrs: self.transport.addrs().to_vec(),
        }
    }

    /// The next report, within 10 s.
    async fn next(&mut self) -> Report {
        tokio::time::timeout(Duration::from_secs(10), self.reports.recv())
            .await
            .expect("no report within 10 s")
            .unwrap()
    }

    /// Every report waiting now.
    fn waiting(&mut self) -> Vec<Report> {
        std::iter::from_fn(|| self.reports.try_recv().ok()).collect()
    }
}

/// Wait up to 10 s for `done`.
async fn until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{what}: not within 10 s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `a`'s link to `b` and `b`'s to `a` are one connection, dialed by
/// `initiator`.
fn one_connection(a: &Machine, b: &Machine, initiator: NodeId) -> bool {
    match (a.transport.link(node(b.n)), b.transport.link(node(a.n))) {
        (Some(ab), Some(ba)) => {
            ab.local == ba.remote
                && ab.remote == ba.local
                && ab.initiator == initiator
                && ba.initiator == initiator
        }
        _ => false,
    }
}

/// Connected less disconnected, for each peer named, and the last report.
fn tally(reports: &[Report]) -> (i32, Option<&Report>) {
    let net = reports
        .iter()
        .map(|report| match report {
            Report::Connected { .. } => 1,
            Report::Disconnected { .. } => -1,
            _ => 0,
        })
        .sum();
    (net, reports.last())
}

/// §6.3: two machines that have not met dial each other at once, so two
/// connections come up; both ends drop the one the larger node ID began,
/// and keep the same one. Afterwards the larger, which now knows the
/// smaller's ID, does not dial again: several backoffs pass and nothing
/// changes. The connection that stays carries messages both ways.
#[tokio::test]
async fn dialling_from_both_sides_at_once_leaves_one_connection() {
    let mut a = Machine::start(1, setup(1)).await;
    let mut b = Machine::start(2, setup(2)).await;
    a.transport.dial(b.target());
    b.transport.dial(a.target());
    until("one connection, the smaller's", || {
        one_connection(&a, &b, node(1))
    })
    .await;
    let link = a.transport.link(node(2)).unwrap();

    tokio::time::sleep(BACKOFF.cap * 2).await;
    assert_eq!(a.transport.link(node(2)), Some(link));
    assert!(one_connection(&a, &b, node(1)));
    for (machine, peer) in [(&mut a, 2), (&mut b, 1)] {
        let reports = machine.waiting();
        let (net, last) = tally(&reports);
        assert_eq!(net, 1, "{reports:?}");
        assert!(
            matches!(last, Some(Report::Connected { peer: p, stable_id, .. }) if *p == node(peer) && *stable_id == stable(peer)),
            "{reports:?}"
        );
    }

    let there = Message::FileDone { req: RequestId(1) };
    assert!(a.transport.send(node(2), there.clone()));
    assert_eq!(
        b.next().await,
        Report::Received {
            peer: node(1),
            message: there
        }
    );
    let back = Message::NotAvailable { req: RequestId(1) };
    assert!(b.transport.send(node(1), back.clone()));
    assert_eq!(
        a.next().await,
        Report::Received {
            peer: node(2),
            message: back
        }
    );
}

/// §6.1, §14.2, with nothing standing in for Tailscale but the fake: two
/// machines of its loopback tailnet, at 127.0.0.2 and 127.0.0.3 on one
/// port, as two daemons on one host are. One dials the other, found in
/// its peer list, from its own address; the other's `whois` of that
/// address names the machine that called. Each then knows the other by
/// its stable ID and node ID. Each way round, on fresh transports.
#[tokio::test]
async fn two_transports_at_their_own_loopback_addresses_know_each_other() {
    let pair = [
        fake::Machine {
            hostname: "desktop",
            ip: Ipv4Addr::new(127, 0, 0, 2),
        },
        fake::Machine {
            hostname: "laptop",
            ip: Ipv4Addr::new(127, 0, 0, 3),
        },
    ];
    for (dialer, other) in [(0, 1), (1, 0)] {
        let port = free_port(&pair);
        let mut fakes = Vec::new();
        let mut machines = Vec::new();
        for (i, config) in fake::loopback(USER, port, &pair).into_iter().enumerate() {
            let tailscale = Arc::new(FakeTailscale::new(config));
            fakes.push(Arc::clone(&tailscale));
            let n = u8::try_from(i + 1).unwrap();
            machines.push(
                Machine::start(
                    n,
                    Setup {
                        tailscale,
                        ..setup(n)
                    },
                )
                .await,
            );
            assert_eq!(machines[i].transport.addrs(), [pair[i].addr(port)]);
        }
        let peers = fakes[dialer].peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        machines[dialer].transport.dial(Target::from(&peers[0]));

        for (me, them) in [(dialer, other), (other, dialer)] {
            let report = machines[me].next().await;
            assert!(
                matches!(&report, Report::Connected { peer, stable_id, .. }
                    if *peer == node(machines[them].n) && *stable_id == pair[them].stable_id()),
                "{}: {report:?}",
                pair[me].hostname
            );
        }
        let link = machines[dialer]
            .transport
            .link(node(machines[other].n))
            .unwrap();
        assert_eq!(link.local.ip(), pair[dialer].ip);
        assert_eq!(link.remote, pair[other].addr(port));
        let theirs = machines[other]
            .transport
            .link(node(machines[dialer].n))
            .unwrap();
        assert_eq!((theirs.local, theirs.remote), (link.remote, link.local));
    }
}

/// A port free at each of `pair`'s addresses just now. No other test
/// listens or dials at them, so it stays free for the test to bind.
fn free_port(pair: &[fake::Machine<'_>; 2]) -> u16 {
    let hint = "127.0.0.2 and 127.0.0.3; on macOS, run scripts/loopback.sh first";
    for _ in 0..100 {
        let first = std::net::TcpListener::bind((pair[0].ip, 0)).expect(hint);
        let port = first.local_addr().unwrap().port();
        if std::net::TcpListener::bind((pair[1].ip, port)).is_ok() {
            return port;
        }
    }
    panic!("no port free at both {} and {}", pair[0].ip, pair[1].ip);
}

/// The dial rule (§6.3): a machine that knows the other's node ID is the
/// smaller does not dial it, however long it waits; once the smaller
/// dials, they connect.
#[tokio::test]
async fn a_machine_dials_only_a_larger_one() {
    let mut a = Machine::start(1, setup(1)).await;
    let mut knows = setup(2);
    knows.known.insert(stable(1), node(1));
    let b = Machine::start(2, knows).await;
    b.transport.dial(a.target());
    tokio::time::sleep(BACKOFF.cap * 2).await;
    assert_eq!(a.transport.link(node(2)), None);
    assert_eq!(a.waiting(), []);

    a.transport.dial(b.target());
    until("connected, by the smaller", || {
        one_connection(&a, &b, node(1))
    })
    .await;
}

/// §5: a connection is answered with `Hello` only if `whois` of the
/// address it came from names this user's untagged node, or a tagged node
/// `machines trust` names. Anything else is closed before a byte. Each
/// case changes who the fake says is at machine 2's address, and machine
/// 2 dials.
#[tokio::test]
async fn only_this_users_machines_are_answered() {
    let tagged = WhoIs {
        stable_id: stable(2),
        user: "tagged-devices".to_owned(),
        tags: vec!["tag:server".to_owned()],
    };
    let stranger = WhoIs {
        user: "someone@example.org".to_owned(),
        ..who(2)
    };
    let cases: [(&str, Option<WhoIs>, bool, bool); 6] = [
        ("this user's machine", Some(who(2)), false, true),
        (
            "another user's machine",
            Some(stranger.clone()),
            false,
            false,
        ),
        (
            "another user's machine, trusted",
            Some(stranger),
            true,
            false,
        ),
        ("a tagged machine", Some(tagged.clone()), false, false),
        ("a tagged machine, trusted", Some(tagged), true, true),
        ("an address tailscale does not know", None, false, false),
    ];
    for (name, at_2, trusted, answered) in cases {
        let tailscale = fake(1);
        tailscale.update(|config| match at_2 {
            Some(who) => {
                config.whois.insert(ip(2), who);
            }
            None => {
                config.whois.remove(&ip(2));
            }
        });
        let mut setup = Setup {
            tailscale,
            ..setup(1)
        };
        if trusted {
            setup.trusted.insert(stable(2));
        }
        let a = Machine::start(1, setup).await;
        let stream = connect_from(2, a.transport.addrs()[0]).await;
        let mut reader = Reader::new(stream);
        let read = tokio::time::timeout(Duration::from_secs(10), reader.read())
            .await
            .unwrap_or_else(|_| panic!("{name}: neither answered nor closed"));
        if answered {
            assert!(
                matches!(&read, Ok(Message::Hello(h)) if h.node == node(1) && h.proto == PROTOCOL),
                "{name}: {read:?}"
            );
        } else {
            assert!(matches!(read, Err(FrameError::Closed)), "{name}: {read:?}");
        }
    }
}

/// §6.3: another protocol version is refused with `Goodbye`, on a
/// connection that arrives and on one this machine dialed, and the owner
/// is told who runs what.
#[tokio::test]
async fn a_version_mismatch_gives_goodbye() {
    let newer = Hello {
        proto: PROTOCOL + 1,
        version: "9.0.0".to_owned(),
        ..hello(2)
    };
    let mut a = Machine::start(1, setup(1)).await;

    // It arrives.
    let stream = connect_from(2, a.transport.addrs()[0]).await;
    let (read, mut write) = stream.into_split();
    let mut reader = Reader::new(read);
    frame::write(&mut write, &Message::Hello(newer.clone()))
        .await
        .unwrap();
    let theirs = reader.read().await.unwrap();
    assert!(
        matches!(&theirs, Message::Hello(h) if h.proto == PROTOCOL && h.node == node(1)),
        "{theirs:?}"
    );
    assert_eq!(
        reader.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::protocol()
        }
    );
    assert!(matches!(reader.read().await, Err(FrameError::Closed)));
    // Closing at once, as a machine told `Goodbye` does, spares the other
    // end its linger.
    drop(write);
    assert_eq!(
        a.next().await,
        Report::Incompatible {
            stable_id: stable(2),
            hello: newer.clone()
        }
    );

    // It is dialed.
    let listener = TcpListener::bind((ip(2), 0)).await.unwrap();
    a.transport.dial(Target {
        stable_id: stable(2),
        addrs: vec![listener.local_addr().unwrap()],
    });
    let (stream, _) = accept(&listener).await;
    let (read, mut write) = stream.into_split();
    let mut reader = Reader::new(read);
    frame::write(&mut write, &Message::Hello(newer.clone()))
        .await
        .unwrap();
    assert!(matches!(reader.read().await.unwrap(), Message::Hello(_)));
    assert_eq!(
        reader.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::protocol()
        }
    );
    drop(write);
    assert_eq!(
        a.next().await,
        Report::Incompatible {
            stable_id: stable(2),
            hello: newer
        }
    );
    assert_eq!(a.transport.link(node(2)), None);
}

/// Dial `addr` from machine `n`'s address, as the transport would.
async fn connect_from(n: u8, addr: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(ip(n), 0)).unwrap();
    socket.connect(addr).await.unwrap()
}

/// Accept the next connection on `listener`, and say when it arrived.
async fn accept(listener: &TcpListener) -> (TcpStream, Instant) {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("not dialed within 10 s")
        .unwrap();
    (stream, Instant::now())
}

/// Complete the handshake on `stream` as machine `n`, by hand, and keep
/// the connection.
async fn greet(stream: TcpStream, n: u8) -> (Reader<OwnedReadHalf>, OwnedWriteHalf) {
    let (read, mut write) = stream.into_split();
    let mut reader = Reader::new(read);
    frame::write(&mut write, &Message::Hello(hello(n)))
        .await
        .unwrap();
    assert!(matches!(reader.read().await.unwrap(), Message::Hello(_)));
    (reader, write)
}

/// Complete the handshake on `stream` as machine 2, then close it.
async fn greet_and_close(stream: TcpStream) {
    greet(stream, 2).await;
}

/// Dial `a` and complete the handshake as machine `n`, by hand.
async fn arrive(a: &Machine, n: u8) -> (Reader<OwnedReadHalf>, OwnedWriteHalf) {
    greet(connect_from(n, a.transport.addrs()[0]).await, n).await
}

/// A listener for machine 2 played by hand, and a target that names it.
async fn by_hand() -> (TcpListener, Target) {
    let listener = TcpListener::bind((ip(2), 0)).await.unwrap();
    let target = Target {
        stable_id: stable(2),
        addrs: vec![listener.local_addr().unwrap()],
    };
    (listener, target)
}

/// §6.3: a connection that ends is dialed again after a backoff. The
/// backoff doubles with each attempt that fails, up to its cap, and is
/// back to the first once a handshake succeeds.
#[tokio::test]
async fn a_lost_connection_is_dialed_again_after_a_backoff() {
    let (listener, target) = by_hand().await;
    let mut a = Machine::start(1, setup(1)).await;
    a.transport.dial(target);
    let (stream, mut last) = accept(&listener).await;
    greet_and_close(stream).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));
    assert_eq!(a.next().await, Report::Disconnected { peer: node(2) });

    // After a handshake, the first wait; then each refusal doubles it, up
    // to the cap; and a handshake makes it the first again.
    let waits = [
        (BACKOFF.first, false),
        (BACKOFF.first * 2, false),
        (BACKOFF.first * 4, false),
        (BACKOFF.cap, true),
        (BACKOFF.first, false),
    ];
    for (i, (wait, greet)) in waits.into_iter().enumerate() {
        let (stream, at) = accept(&listener).await;
        let gap = at - last;
        last = at;
        assert!(gap >= wait, "attempt {}: {gap:?} < {wait:?}", i + 2);
        // The waits that should be the first are well short of the cap:
        // neither stuck at the cap nor left doubled by an earlier failure.
        // The others are bounded below only, so a slow machine does not
        // fail the test.
        if wait == BACKOFF.first {
            assert!(gap < BACKOFF.cap, "attempt {}: {gap:?}", i + 2);
        }
        if greet {
            greet_and_close(stream).await;
        }
    }
}

/// The dial rule (§6.3), learned: a machine dialed before its ID was
/// known, and found smaller at the handshake, is not dialed again once the
/// connection ends. It is the one to dial.
#[tokio::test]
async fn a_machine_found_smaller_is_not_dialed_again() {
    let (listener, target) = by_hand().await;
    let mut c = Machine::start(3, setup(3)).await;
    c.transport.dial(target);
    let (stream, _) = accept(&listener).await;
    greet_and_close(stream).await;
    assert!(matches!(c.next().await, Report::Connected { .. }));
    assert_eq!(c.next().await, Report::Disconnected { peer: node(2) });
    let again = tokio::time::timeout(BACKOFF.cap * 2, listener.accept()).await;
    assert!(again.is_err(), "dialed again");
}

/// The dial rule (§6.3): a machine that a larger one has dialed first, as
/// can happen before they have met, does not dial it as well while that
/// connection is up.
#[tokio::test]
async fn a_machine_does_not_dial_one_already_connected() {
    let (listener, target) = by_hand().await;
    let mut a = Machine::start(1, setup(1)).await;
    let _theirs = arrive(&a, 2).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));
    a.transport.dial(target);
    let dialed = tokio::time::timeout(BACKOFF.cap * 2, listener.accept()).await;
    assert!(dialed.is_err(), "dialed a machine already connected");
    assert_eq!(a.transport.link(node(2)).unwrap().initiator, node(2));
}

/// §6.3: a connection the larger node begins to a machine the smaller
/// has already connected to is told `Goodbye { Duplicate }` and closed,
/// and the owner hears nothing of it.
#[tokio::test]
async fn the_connection_the_larger_begins_is_told_goodbye() {
    let (listener, target) = by_hand().await;
    let mut a = Machine::start(1, setup(1)).await;
    a.transport.dial(target);
    let (stream, _) = accept(&listener).await;
    let _kept = greet(stream, 2).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));

    let (mut reader, write) = arrive(&a, 2).await;
    assert_eq!(
        reader.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::duplicate()
        }
    );
    assert!(matches!(reader.read().await, Err(FrameError::Closed)));
    drop(write);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.transport.link(node(2)).unwrap().initiator, node(1));
    assert_eq!(a.waiting(), []);
}

/// §6.3, the other way round: a machine whose own dial reached a smaller
/// one first gives way when the smaller's dial arrives, as two machines
/// that dial each other at once can find. The smaller's connection is
/// kept, its own told `Goodbye { Duplicate }`.
#[tokio::test]
async fn the_smaller_ones_connection_replaces_the_larger_ones() {
    let (listener, target) = by_hand().await;
    let mut c = Machine::start(3, setup(3)).await;
    c.transport.dial(target);
    let (stream, _) = accept(&listener).await;
    let (mut own_read, _own_write) = greet(stream, 2).await;
    assert!(matches!(c.next().await, Report::Connected { .. }));
    assert_eq!(c.transport.link(node(2)).unwrap().initiator, node(3));

    let _theirs = arrive(&c, 2).await;
    assert_eq!(c.next().await, Report::Disconnected { peer: node(2) });
    assert!(matches!(c.next().await, Report::Connected { peer, .. } if peer == node(2)));
    assert_eq!(c.transport.link(node(2)).unwrap().initiator, node(2));
    assert_eq!(
        own_read.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::duplicate()
        }
    );
}

/// A machine that dials again while its old connection still looks up at
/// this end has taken the old one for gone: the newer is kept and the
/// older told `Goodbye`. The owner hears a disconnection and a connection,
/// and from then on only what the newer one says, though the older is
/// still read while it lingers.
#[tokio::test]
async fn a_replaced_connection_says_nothing_more() {
    let mut a = Machine::start(1, setup(1)).await;
    let (mut old_read, mut old_write) = arrive(&a, 2).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));
    let (_new_read, mut new_write) = arrive(&a, 2).await;
    assert_eq!(a.next().await, Report::Disconnected { peer: node(2) });
    assert!(matches!(a.next().await, Report::Connected { peer, .. } if peer == node(2)));
    assert_eq!(
        old_read.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::duplicate()
        }
    );

    let late = Message::FileDone { req: RequestId(1) };
    frame::write(&mut old_write, &late).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let current = Message::FileDone { req: RequestId(2) };
    frame::write(&mut new_write, &current).await.unwrap();
    assert_eq!(
        a.next().await,
        Report::Received {
            peer: node(2),
            message: current
        }
    );
}

/// A machine whose transport stops is disconnected, and nothing more can
/// be sent to it; nor to a machine never connected.
#[tokio::test]
async fn a_machine_that_goes_is_disconnected() {
    let mut a = Machine::start(1, setup(1)).await;
    let b = Machine::start(2, setup(2)).await;
    a.transport.dial(b.target());
    assert!(matches!(a.next().await, Report::Connected { peer, .. } if peer == node(2)));
    assert!(!a.transport.send(node(9), Message::Ping));

    drop(b);
    assert_eq!(a.next().await, Report::Disconnected { peer: node(2) });
    assert_eq!(a.transport.link(node(2)), None);
    assert!(!a.transport.send(node(2), Message::Ping));
}

/// The rule for two connections to one machine, from both ends' view.
#[test]
fn of_two_connections_the_smaller_ids_is_kept() {
    let (small, large) = (node(1), node(2));
    // Begun from different ends: the smaller's, whichever came first, and
    // seen alike from either end.
    for (me, peer) in [(small, large), (large, small)] {
        assert!(keeps_new(me, peer, large, small));
        assert!(!keeps_new(me, peer, small, large));
    }
    // Begun from the same end: the newer.
    for (me, peer) in [(small, large), (large, small)] {
        assert!(keeps_new(me, peer, small, small));
        assert!(keeps_new(me, peer, large, large));
    }
}

/// The dial rule, case by case.
#[test]
fn the_dial_rule() {
    let link = Link {
        initiator: node(1),
        local: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)),
        remote: SocketAddr::from((Ipv4Addr::LOCALHOST, 2)),
    };
    let (out, _) = mpsc::unbounded_channel();
    let mut state = State {
        live: BTreeMap::new(),
        known: BTreeMap::new(),
    };
    // A node not met is dialed, from either side.
    assert!(state.should_dial(node(1), &stable(2)));
    assert!(state.should_dial(node(3), &stable(2)));
    state.known.insert(stable(2), node(2));
    // A known larger node is dialed; a known smaller one, or this one, not.
    assert!(state.should_dial(node(1), &stable(2)));
    assert!(!state.should_dial(node(3), &stable(2)));
    assert!(!state.should_dial(node(2), &stable(2)));
    // Not while a connection to it is up.
    state.live.insert(node(2), Live { conn: 0, link, out });
    assert!(!state.should_dial(node(1), &stable(2)));
}

/// The backoff doubles to its cap and starts again after a reset.
#[test]
fn the_backoff_doubles_to_its_cap() {
    let mut retry = Retry::new(Backoff::DEFAULT);
    let waits: Vec<u64> = (0..11).map(|_| retry.wait().as_secs()).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    retry.reset();
    assert_eq!(retry.wait(), Duration::from_secs(1));
}
