//! Tailscale (DESIGN.md §6.1): everything delocal asks of it, behind one
//! trait, so that the rest of the daemon never sees how it is asked.
//!
//! [`Tailscale`] has three methods: this machine, its peers, and who is at
//! an address. They are async because the preferred implementation is an
//! HTTP client on the LocalAPI's socket. v1 polls the peer list every 30 s
//! (§6.2, §6.4); the LocalAPI's event stream is a v1.1 item, so the trait
//! has no stream type.
//!
//! **Addresses carry a port.** A machine's addresses, its own and its
//! peers', are socket addresses. The real implementation will pair each
//! Tailscale IP with delocal's port (§6.3); the fake may give every machine
//! a port of its own on 127.0.0.1, which is how two daemons share one host
//! without `sudo` (§14.2). `whois` takes the socket address a connection
//! came from.
//!
//! So far:
//!
//! - [`tier`]: connection tiers from path info (§6.4)
//!
//! The real implementations, the LocalAPI and the CLI fallback with their
//! recorded fixtures, are Phase 3's (§15).

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

pub mod tier;

/// A Tailscale node's stable ID (§4), what a peer's node ID is bound to
/// (§5, `machines` in §11).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableId(pub String);

impl fmt::Display for StableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// This machine, as Tailscale knows it (§5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfInfo {
    /// Its Tailscale IPs, each with the port delocal listens on (§6.3).
    pub addrs: Vec<SocketAddr>,
    pub stable_id: StableId,
    /// The login name of the user who owns it (§4).
    pub user: String,
    pub hostname: String,
}

/// Another node of the tailnet (§6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub hostname: String,
    /// Its Tailscale IPs, each with delocal's port.
    pub addrs: Vec<SocketAddr>,
    pub stable_id: StableId,
    /// The login name of the user who owns it.
    pub user: String,
    pub online: bool,
    /// A tagged node is owned by the tailnet rather than a user (§5).
    pub tags: Vec<String>,
    /// How traffic to it goes now; `None` if Tailscale did not say.
    pub path: Option<PathInfo>,
}

/// How traffic to a peer goes now (§6.1), what its tier is computed from
/// (§6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
    /// The peer's address on a direct path, if traffic goes straight to it.
    pub direct: Option<SocketAddr>,
    /// The DERP region or peer relay in use, if any.
    pub relay: Option<String>,
}

/// Who is at an address (§5): what the trust rule checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhoIs {
    pub stable_id: StableId,
    /// The login name of the user who owns the node.
    pub user: String,
    pub tags: Vec<String>,
}

/// Why Tailscale did not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailscaleError {
    /// It could not be asked: not running, logged out, or not answering
    /// (§13).
    Unavailable(String),
    /// No node of the tailnet is at this address.
    Unknown(SocketAddr),
}

impl fmt::Display for TailscaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "tailscale is unavailable: {why}"),
            Self::Unknown(addr) => write!(f, "no tailscale node is at {addr}"),
        }
    }
}

impl std::error::Error for TailscaleError {}

/// What a [`Tailscale`] method returns: a future of its answer.
pub type Answer<'a, T> = Pin<Box<dyn Future<Output = Result<T, TailscaleError>> + Send + 'a>>;

/// Everything delocal asks of Tailscale (§6.1). See the module docs.
///
/// Each method is async, written as a method that returns a boxed future
/// ([`Answer`]) rather than as an `async fn`. An `async fn` in a trait
/// cannot be called through `dyn`, and this trait is used as `dyn
/// Tailscale`: the daemon holds whichever implementation it was given, the
/// LocalAPI or the CLI fallback chosen at run time (§6.1) or the fake in
/// tests, as one type. The futures are `Send`, so they may run on any of
/// the runtime's threads.
pub trait Tailscale: Send + Sync {
    /// This machine: its addresses, stable ID, user and hostname.
    fn self_info(&self) -> Answer<'_, SelfInfo>;
    /// Every peer on the tailnet, online or not, with its path info.
    fn peers(&self) -> Answer<'_, Vec<Peer>>;
    /// The node at `addr`, the socket address a connection came from.
    fn whois(&self, addr: SocketAddr) -> Answer<'_, WhoIs>;
}
