//! The mesh over real TCP on 127.0.0.1 (DESIGN.md §6.3, §5, §14.2).
//!
//! **Whois here.** Each machine's Tailscale is a [`Stub`] whose `whois`
//! names one node for every address. The fake's `whois` looks addresses up
//! exactly (§6.1), but a connection on one host comes from an ephemeral
//! port that nobody knows before it is dialed, so the fake cannot name the
//! machine behind it. How two daemons on one host pass the trust rule is
//! an open question in this PR; these tests give the transport the answer
//! a real tailnet would.

use std::collections::{BTreeMap, BTreeSet};
use std::future::ready;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use delocal_engine::{HostName, NodeId};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;
use crate::net::frame::{self, FrameError, Reader};
use crate::net::wire::{Message, PROTOCOL, Reason, RequestId};
use crate::store::sample;
use crate::tailscale::{Answer, Peer, SelfInfo, StableId, Tailscale, TailscaleError, WhoIs};

const USER: &str = "me@example.com";

/// Short enough that a test can wait out several attempts.
const BACKOFF: Backoff = Backoff {
    first: Duration::from_millis(100),
    cap: Duration::from_millis(800),
};

/// A Tailscale for one machine of these tests: it is at 127.0.0.1 on a
/// port the system picks, and `whois` answers `from` for every address.
struct Stub {
    me: SelfInfo,
    from: Mutex<Result<WhoIs, TailscaleError>>,
}

impl Tailscale for Stub {
    fn self_info(&self) -> Answer<'_, SelfInfo> {
        Box::pin(ready(Ok(self.me.clone())))
    }

    fn peers(&self) -> Answer<'_, Vec<Peer>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn whois(&self, _addr: SocketAddr) -> Answer<'_, WhoIs> {
        Box::pin(ready(self.from.lock().unwrap().clone()))
    }
}

fn node(n: u8) -> NodeId {
    sample::node(n)
}

fn stable(n: u8) -> StableId {
    StableId(format!("fake-m{n}"))
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

/// Machine `n`, whose `whois` says every connection is `from`.
fn setup(n: u8, from: Result<WhoIs, TailscaleError>) -> Setup {
    let me = SelfInfo {
        addrs: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 0))],
        stable_id: stable(n),
        user: USER.to_owned(),
        hostname: format!("m{n}"),
    };
    Setup {
        node: node(n),
        hostname: HostName::new(format!("m{n}")).unwrap(),
        tailscale: Arc::new(Stub {
            me,
            from: Mutex::new(from),
        }),
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
        let (transport, reports) = Transport::start(setup).await.unwrap();
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    let mut b = Machine::start(2, setup(2, Ok(who(1)))).await;
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

/// The dial rule (§6.3): a machine that knows the other's node ID is the
/// smaller does not dial it, however long it waits; once the smaller
/// dials, they connect.
#[tokio::test]
async fn a_machine_dials_only_a_larger_one() {
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    let mut knows = setup(2, Ok(who(1)));
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

/// §5: a connection is answered with `Hello` only if `whois` names this
/// user's untagged node, or a tagged node `machines trust` names. Anything
/// else is closed before a byte.
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
    let unknown = Err(TailscaleError::Unknown(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        1,
    ))));
    let cases: [(&str, Result<WhoIs, TailscaleError>, bool, bool); 6] = [
        ("this user's machine", Ok(who(2)), false, true),
        ("another user's machine", Ok(stranger.clone()), false, false),
        ("another user's machine, trusted", Ok(stranger), true, false),
        ("a tagged machine", Ok(tagged.clone()), false, false),
        ("a tagged machine, trusted", Ok(tagged), true, true),
        ("an address tailscale does not know", unknown, false, false),
    ];
    for (name, from, trusted, answered) in cases {
        let mut setup = setup(1, from);
        if trusted {
            setup.trusted.insert(stable(2));
        }
        let a = Machine::start(1, setup).await;
        let stream = TcpStream::connect(a.transport.addrs()[0]).await.unwrap();
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;

    // It arrives.
    let stream = TcpStream::connect(a.transport.addrs()[0]).await.unwrap();
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
            reason: Reason::Protocol
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
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
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
            reason: Reason::Protocol
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
    greet(TcpStream::connect(a.transport.addrs()[0]).await.unwrap(), n).await
}

/// A listener on 127.0.0.1 for a machine played by hand, and a target
/// that names it as machine 2.
async fn by_hand() -> (TcpListener, Target) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
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
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    a.transport.dial(Target {
        stable_id: stable(2),
        addrs: vec![listener.local_addr().unwrap()],
    });
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
    let mut c = Machine::start(3, setup(3, Ok(who(2)))).await;
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    a.transport.dial(target);
    let (stream, _) = accept(&listener).await;
    let _kept = greet(stream, 2).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));

    let (mut reader, write) = arrive(&a, 2).await;
    assert_eq!(
        reader.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::Duplicate
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
    let mut c = Machine::start(3, setup(3, Ok(who(2)))).await;
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
            reason: Reason::Duplicate
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    let (mut old_read, mut old_write) = arrive(&a, 2).await;
    assert!(matches!(a.next().await, Report::Connected { .. }));
    let (_new_read, mut new_write) = arrive(&a, 2).await;
    assert_eq!(a.next().await, Report::Disconnected { peer: node(2) });
    assert!(matches!(a.next().await, Report::Connected { peer, .. } if peer == node(2)));
    assert_eq!(
        old_read.read().await.unwrap(),
        Message::Goodbye {
            reason: Reason::Duplicate
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
    let mut a = Machine::start(1, setup(1, Ok(who(2)))).await;
    let b = Machine::start(2, setup(2, Ok(who(1)))).await;
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
