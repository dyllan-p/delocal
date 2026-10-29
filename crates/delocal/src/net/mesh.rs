//! The mesh (DESIGN.md §6.3, §5): one connection to each of this user's
//! machines that runs delocal, on the addresses Tailscale reports.
//!
//! **Listening.** [`Transport::start`] listens on every address
//! Tailscale gives this machine (§6.1), each a Tailscale IP with delocal's
//! port, never on all interfaces (§5).
//!
//! **The trust rule** (§5). A connection that arrives is accepted only if
//! Tailscale's `whois` of the address it came from names a node of this
//! machine's own user that is not tagged, or a tagged node that `machines
//! trust` names. Anything else is closed before a byte is said, without
//! `Hello`.
//!
//! **Dialing** ([`Transport::dial`]). A dialer keeps one Tailscale node
//! connected. The **dial rule** (§6.3): of two machines, the one with the
//! smaller node ID dials, so a dialer whose node's ID it knows, from a
//! handshake or a binding (§5), dials only a larger one, and only while no
//! connection to it is up. A node it has not met it dials, since it cannot
//! tell; if that node dials too, the duplicate rule below keeps one. After
//! a connection ends, or an attempt fails, it waits ([`Backoff`]): a
//! second at first, doubling with each attempt that fails, up to five
//! minutes, and back to a second once a handshake succeeds.
//!
//! **Duplicates** (§6.3). If two connections to one machine are up at
//! once, the one begun by the larger node ID is dropped with
//! `Goodbye { Duplicate }`. Both ends judge alike, so both keep the same
//! one. Two begun by the same end mean that end took the older for gone
//! and dialed again, so the newer is kept.
//!
//! **Reports.** What happens comes to the owner in order on one channel
//! ([`Report`]): a machine connected, a message from it, it disconnected,
//! or it runs another protocol version. A **hub** task hears from every
//! connection and decides which is live; only the live connection to a
//! machine is reported, so a replaced or dropped one never says anything
//! after the one that replaced it. A replacement is reported as a
//! disconnection and a connection, so that the owner starts the
//! connection's exchange (§7.4) afresh on the connection that stays.
//!
//! **Queues.** Messages to send wait in each connection's own queue, which
//! has no bound, since the daemon's sends must not block (its `Outbox`).
//! Messages received wait for the owner in short queues: a slow owner
//! holds up the connections, and TCP holds up the peers, rather than this
//! machine's memory filling with file data.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use delocal_engine::{HostName, NodeId};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::conn::{self, End, Out, Refused, SILENCE};
use super::frame::Reader;
use super::wire::{Hello, Message, PROTOCOL, Reason};
use crate::scan;
use crate::tailscale::{Peer, StableId, Tailscale, TailscaleError, WhoIs};

/// How long a dial, or a `whois`, may take.
pub const CONNECT: Duration = Duration::from_secs(10);

/// How many reports, and how many messages on their way to the hub, may
/// wait. Each may be a `FileData` of 1 MiB.
pub const QUEUE: usize = 64;

/// How long a listener waits after an accept fails, most often for want of
/// file descriptors, before it accepts again.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// How long a dialer waits between attempts; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after a connection ends, and after the first failure.
    pub first: Duration,
    /// The longest wait.
    pub cap: Duration,
}

impl Backoff {
    /// A second, doubling to five minutes, the cap of discovery's own
    /// backoff (§6.2).
    pub const DEFAULT: Self = Self {
        first: Duration::from_secs(1),
        cap: Duration::from_secs(5 * 60),
    };
}

/// A dialer's place in its [`Backoff`].
#[derive(Debug)]
struct Retry {
    backoff: Backoff,
    next: Duration,
}

impl Retry {
    fn new(backoff: Backoff) -> Self {
        Self {
            backoff,
            next: backoff.first,
        }
    }

    /// The wait before the next attempt; the one after is twice as long,
    /// up to the cap.
    fn wait(&mut self) -> Duration {
        let wait = self.next;
        self.next = self.next.saturating_mul(2).min(self.backoff.cap);
        wait
    }

    /// A handshake succeeded: the next wait is the first again.
    fn reset(&mut self) {
        self.next = self.backoff.first;
    }
}

/// Everything [`Transport::start`] needs.
pub struct Setup {
    pub node: NodeId,
    pub hostname: HostName,
    pub tailscale: Arc<dyn Tailscale>,
    /// The clock `Hello` carries (§12), for the other end's skew warning.
    pub clock: Arc<dyn scan::Clock + Send + Sync>,
    /// Tailscale nodes `machines trust` admits though they are tagged (§5).
    pub trusted: BTreeSet<StableId>,
    /// The node ID each Tailscale node is bound to, as far as is known
    /// (§5), for the dial rule.
    pub known: BTreeMap<StableId, NodeId>,
    /// [`Backoff::DEFAULT`]; shorter in tests.
    pub backoff: Backoff,
}

/// A Tailscale node to keep connected: what [`Transport::dial`] takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub stable_id: StableId,
    /// Tried in order.
    pub addrs: Vec<SocketAddr>,
}

impl From<&Peer> for Target {
    fn from(peer: &Peer) -> Self {
        Self {
            stable_id: peer.stable_id.clone(),
            addrs: peer.addrs.clone(),
        }
    }
}

/// What the transport tells its owner, in order. See the module docs.
#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    /// A machine is connected, its `Hello` exchanged.
    Connected {
        peer: NodeId,
        hello: Hello,
        stable_id: StableId,
    },
    /// A message from a connected machine. `Hello`, `Goodbye`, `Ping` and
    /// `Pong` are the transport's own, and never come here.
    Received { peer: NodeId, message: Message },
    /// A connected machine is no longer.
    Disconnected { peer: NodeId },
    /// A machine runs another protocol version: neither syncs with the
    /// other until one is updated, and `status` says which (§6.3, §13).
    Incompatible { stable_id: StableId, hello: Hello },
}

/// A live connection, as seen from this end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    /// The node that dialed it.
    pub initiator: NodeId,
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

/// Why the transport did not start.
#[derive(Debug)]
pub enum StartError {
    /// Tailscale did not say what this machine's addresses are.
    Tailscale(TailscaleError),
    /// One of them could not be listened on.
    Listen { addr: SocketAddr, error: io::Error },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tailscale(e) => write!(f, "{e}"),
            Self::Listen { addr, error } => write!(f, "cannot listen on {addr}: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

/// The transport; see the module docs. Dropping it stops everything it
/// started and closes every connection.
pub struct Transport {
    shared: Arc<Shared>,
    addrs: Vec<SocketAddr>,
    tasks: Mutex<Tasks>,
}

#[derive(Default)]
struct Tasks {
    set: JoinSet<()>,
    dialing: BTreeSet<StableId>,
}

impl Transport {
    /// Listen on this machine's Tailscale addresses, and start the hub.
    /// Reports arrive on the receiver returned.
    pub async fn start(setup: Setup) -> Result<(Self, mpsc::Receiver<Report>), StartError> {
        let me = setup
            .tailscale
            .self_info()
            .await
            .map_err(StartError::Tailscale)?;
        let mut listeners = Vec::new();
        for &addr in &me.addrs {
            let listener = TcpListener::bind(addr)
                .await
                .map_err(|error| StartError::Listen { addr, error })?;
            let bound = listener
                .local_addr()
                .map_err(|error| StartError::Listen { addr, error })?;
            listeners.push((bound, listener));
        }
        let (hub, events) = mpsc::channel(QUEUE);
        let (reports, received) = mpsc::channel(QUEUE);
        let (state, _) = watch::channel(State {
            live: BTreeMap::new(),
            known: setup.known,
        });
        let shared = Arc::new(Shared {
            node: setup.node,
            hostname: setup.hostname,
            user: me.user,
            tailscale: setup.tailscale,
            clock: setup.clock,
            trusted: setup.trusted,
            backoff: setup.backoff,
            hub,
            state,
            next_conn: AtomicU64::new(0),
        });
        let mut tasks = Tasks::default();
        tasks
            .set
            .spawn(run_hub(Arc::clone(&shared), events, reports));
        let addrs = listeners.iter().map(|(addr, _)| *addr).collect();
        for (addr, listener) in listeners {
            info!(%addr, "listening");
            tasks.set.spawn(listen(Arc::clone(&shared), listener));
        }
        let transport = Self {
            shared,
            addrs,
            tasks: Mutex::new(tasks),
        };
        Ok((transport, received))
    }

    /// The addresses it listens on, each port as bound.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Keep `target` connected, under the dial rule, from now until the
    /// transport is dropped. A node already being dialed is left as it is.
    pub fn dial(&self, target: Target) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        // Reap what has finished, so the set does not grow.
        while tasks.set.try_join_next().is_some() {}
        if tasks.dialing.insert(target.stable_id.clone()) {
            tasks.set.spawn(dialer(Arc::clone(&self.shared), target));
        }
    }

    /// Queue `message` for `to`'s live connection. False if there is none.
    pub fn send(&self, to: NodeId, message: Message) -> bool {
        let state = self.shared.state.borrow();
        state
            .live
            .get(&to)
            .is_some_and(|live| live.out.send(Out::Message(message)).is_ok())
    }

    /// The live connection to `peer`, if there is one.
    pub fn link(&self, peer: NodeId) -> Option<Link> {
        self.shared
            .state
            .borrow()
            .live
            .get(&peer)
            .map(|live| live.link)
    }
}

/// What every task shares.
struct Shared {
    node: NodeId,
    hostname: HostName,
    /// This machine's Tailscale user, whom the trust rule compares with.
    user: String,
    tailscale: Arc<dyn Tailscale>,
    clock: Arc<dyn scan::Clock + Send + Sync>,
    trusted: BTreeSet<StableId>,
    backoff: Backoff,
    hub: mpsc::Sender<Event>,
    /// Written by the hub alone; read by dialers, which wait on it, and by
    /// [`Transport::send`].
    state: watch::Sender<State>,
    next_conn: AtomicU64,
}

impl Shared {
    fn hello(&self) -> Hello {
        Hello {
            proto: PROTOCOL,
            node: self.node,
            hostname: self.hostname.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            now: self.clock.now(),
        }
    }

    /// §5's trust rule. A tagged node belongs to the tailnet, not to a
    /// user, so `machines trust` is the only way in for one; it does not
    /// let in another user's node, since v1 syncs only one user's machines
    /// (§3.2).
    fn trusts(&self, who: &WhoIs) -> bool {
        if who.tags.is_empty() {
            who.user == self.user
        } else {
            self.trusted.contains(&who.stable_id)
        }
    }
}

/// The connections that are live, and what is known of who is who.
struct State {
    /// The one live connection to each machine.
    live: BTreeMap<NodeId, Live>,
    /// The node ID of each Tailscale node, as far as is known.
    known: BTreeMap<StableId, NodeId>,
}

struct Live {
    conn: u64,
    link: Link,
    out: mpsc::UnboundedSender<Out>,
}

impl State {
    /// The dial rule (§6.3): whether `me` should dial `target` now.
    fn should_dial(&self, me: NodeId, target: &StableId) -> bool {
        match self.known.get(target) {
            None => true,
            Some(&peer) => me < peer && !self.live.contains_key(&peer),
        }
    }
}

/// Of a live connection to `peer` begun by `old` and a new one begun by
/// `new`, whether the new one is kept (§6.3); see the module docs.
fn keeps_new(me: NodeId, peer: NodeId, old: NodeId, new: NodeId) -> bool {
    old == new || new == me.min(peer)
}

/// What a connection tells the hub.
enum Event {
    /// Its handshake is done.
    Up {
        conn: u64,
        hello: Hello,
        stable_id: StableId,
        link: Link,
        out: mpsc::UnboundedSender<Out>,
    },
    Received {
        conn: u64,
        peer: NodeId,
        message: Message,
    },
    Down {
        conn: u64,
        peer: NodeId,
    },
    Incompatible {
        stable_id: StableId,
        hello: Hello,
    },
}

/// The hub; see the module docs.
async fn run_hub(
    shared: Arc<Shared>,
    mut events: mpsc::Receiver<Event>,
    reports: mpsc::Sender<Report>,
) {
    while let Some(event) = events.recv().await {
        let mut out = Vec::new();
        match event {
            Event::Received {
                conn,
                peer,
                message,
            } => {
                let live = shared
                    .state
                    .borrow()
                    .live
                    .get(&peer)
                    .is_some_and(|live| live.conn == conn);
                if live {
                    out.push(Report::Received { peer, message });
                }
            }
            Event::Up {
                conn,
                hello,
                stable_id,
                link,
                out: queue,
            } => {
                let peer = hello.node;
                shared.state.send_if_modified(|state| {
                    let known = state.known.insert(stable_id.clone(), peer) != Some(peer);
                    if let Some(old) = state.live.get(&peer)
                        && !keeps_new(shared.node, peer, old.link.initiator, link.initiator)
                    {
                        debug!(peer = %peer.short(), "dropped a duplicate connection");
                        let _ = queue.send(Out::Goodbye(Reason::Duplicate));
                        return known;
                    }
                    let live = Live {
                        conn,
                        link,
                        out: queue,
                    };
                    if let Some(old) = state.live.insert(peer, live) {
                        debug!(peer = %peer.short(), "replaced a connection");
                        let _ = old.out.send(Out::Goodbye(Reason::Duplicate));
                        out.push(Report::Disconnected { peer });
                    }
                    info!(peer = %peer.short(), host = hello.hostname.as_str(), %link.remote, "connected");
                    out.push(Report::Connected {
                        peer,
                        hello,
                        stable_id,
                    });
                    true
                });
            }
            Event::Down { conn, peer } => {
                shared.state.send_if_modified(|state| {
                    if state.live.get(&peer).is_some_and(|live| live.conn == conn) {
                        state.live.remove(&peer);
                        info!(peer = %peer.short(), "disconnected");
                        out.push(Report::Disconnected { peer });
                        true
                    } else {
                        false
                    }
                });
            }
            Event::Incompatible { stable_id, hello } => {
                out.push(Report::Incompatible { stable_id, hello });
            }
        }
        for report in out {
            if reports.send(report).await.is_err() {
                return;
            }
        }
    }
}

/// Accept connections on `listener` until the transport is dropped.
async fn listen(shared: Arc<Shared>, listener: TcpListener) {
    let mut conns = JoinSet::new();
    loop {
        match listener.accept().await {
            Ok((stream, remote)) => {
                while conns.try_join_next().is_some() {}
                conns.spawn(accepted(Arc::clone(&shared), stream, remote));
            }
            Err(error) => {
                warn!(%error, "accept failed");
                tokio::time::sleep(ACCEPT_PAUSE).await;
            }
        }
    }
}

/// A connection that arrived from `remote`: §5's trust rule, then its
/// handshake and its life.
async fn accepted(shared: Arc<Shared>, stream: TcpStream, remote: SocketAddr) {
    let who = match tokio::time::timeout(CONNECT, shared.tailscale.whois(remote)).await {
        Ok(Ok(who)) => who,
        Ok(Err(error)) => {
            info!(%remote, %error, "refused a connection: whois failed");
            return;
        }
        Err(_) => {
            info!(%remote, "refused a connection: whois did not answer");
            return;
        }
    };
    if !shared.trusts(&who) {
        info!(%remote, user = %who.user, tags = ?who.tags, "refused a connection: not this user's");
        return;
    }
    connection(&shared, stream, who.stable_id, false).await;
}

/// Keep `target` connected; see the module docs.
async fn dialer(shared: Arc<Shared>, target: Target) {
    let mut state = shared.state.subscribe();
    let mut retry = Retry::new(shared.backoff);
    loop {
        let rule = |state: &State| state.should_dial(shared.node, &target.stable_id);
        if state.wait_for(rule).await.is_err() {
            return;
        }
        if dial(&shared, &target).await {
            retry.reset();
        }
        tokio::time::sleep(retry.wait()).await;
    }
}

/// One attempt to connect to `target`, at each of its addresses in turn
/// until one answers, and the connection's life if it does. True if a
/// handshake succeeded.
async fn dial(shared: &Arc<Shared>, target: &Target) -> bool {
    for &addr in &target.addrs {
        match tokio::time::timeout(CONNECT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                return connection(shared, stream, target.stable_id.clone(), true).await;
            }
            Ok(Err(error)) => debug!(%addr, %error, "dial failed"),
            Err(_) => debug!(%addr, "dial timed out"),
        }
    }
    false
}

/// A connection to the Tailscale node `stable_id`, which this end dialed
/// or accepted: its handshake, then its life, with the hub told of each.
/// True if the handshake succeeded.
async fn connection(
    shared: &Arc<Shared>,
    stream: TcpStream,
    stable_id: StableId,
    dialed: bool,
) -> bool {
    let (local, remote) = match (stream.local_addr(), stream.peer_addr()) {
        (Ok(local), Ok(remote)) => (local, remote),
        (Err(error), _) | (_, Err(error)) => {
            debug!(%error, "a connection closed before it began");
            return false;
        }
    };
    // Pings and decisions are small and should not wait to be coalesced.
    if let Err(error) = stream.set_nodelay(true) {
        debug!(%remote, %error, "could not turn off coalescing");
    }
    let (read, mut write) = stream.into_split();
    let mut reader = Reader::with_silence(read, SILENCE);
    let hello = match conn::handshake(&mut reader, &mut write, &shared.hello()).await {
        Ok(hello) => hello,
        Err(Refused::Protocol(hello)) => {
            warn!(%remote, host = hello.hostname.as_str(), proto = hello.proto, version = %hello.version, "another protocol version");
            let _ = shared
                .hub
                .send(Event::Incompatible { stable_id, hello })
                .await;
            return false;
        }
        Err(refused) => {
            debug!(%remote, %refused, "handshake failed");
            return false;
        }
    };
    let peer = hello.node;
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let initiator = if dialed { shared.node } else { peer };
    let (outs, out) = mpsc::unbounded_channel();
    let up = Event::Up {
        conn,
        hello,
        stable_id,
        link: Link {
            initiator,
            local,
            remote,
        },
        out: outs.clone(),
    };
    if shared.hub.send(up).await.is_err() {
        return true;
    }
    let wrap = |message| Event::Received {
        conn,
        peer,
        message,
    };
    let end: End = conn::run(reader, write, outs, out, &shared.hub, wrap).await;
    debug!(peer = %peer.short(), %remote, ?end, "connection ended");
    let _ = shared.hub.send(Event::Down { conn, peer }).await;
    true
}

#[cfg(test)]
mod tests;
