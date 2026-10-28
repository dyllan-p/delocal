//! Connection tiers (DESIGN.md §6.4): how each connected peer is reached,
//! from Tailscale's path info.
//!
//! **The rule** ([`classify`]). A peer reached directly at a private
//! address is `lan`; one reached directly at any other address is `direct`;
//! any other is `relay`, through a DERP server or a peer relay. Private
//! means RFC 1918 or link-local. A peer with no path info is `relay`, the
//! most conservative, and so is every peer after a poll that Tailscale did
//! not answer, since then no path info is available for any of them.
//!
//! §6.4's third kind of private address, one on the same subnet as one of
//! this machine's non-Tailscale interfaces, needs those interfaces and is
//! Phase 3's. Until then such a peer is `direct`: the size rules (§6.5)
//! defer more for it than they will, never less.
//!
//! **Changes** ([`Tiers::changes`]). A connected peer is matched to its
//! Tailscale node by its binding (§5, `machines` in §11), and named only if
//! its tier differs from the one the engine holds.

use std::collections::BTreeMap;
use std::net::IpAddr;

use delocal_engine::{NodeId, Tier};

use super::{PathInfo, Peer, StableId};

/// A peer's tier from its path info; see the module docs.
pub fn classify(path: Option<&PathInfo>) -> Tier {
    match path.and_then(|path| path.direct) {
        Some(addr) if private(addr.ip()) => Tier::Lan,
        Some(_) => Tier::Direct,
        None => Tier::Relay,
    }
}

/// RFC 1918 or link-local. An IPv4 address in IPv6 form (`::ffff:a.b.c.d`)
/// is judged as the IPv4 address it is.
fn private(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unicast_link_local(),
    }
}

/// What one poll found: the tier of each Tailscale node, by stable ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tiers(BTreeMap<StableId, Tier>);

impl Tiers {
    /// The tiers of `peers`, from their path info.
    pub fn of(peers: &[Peer]) -> Self {
        Self(
            peers
                .iter()
                .map(|peer| (peer.stable_id.clone(), classify(peer.path.as_ref())))
                .collect(),
        )
    }

    /// The tier of the node `id`. A node the poll did not find has no path
    /// info, and is `relay`.
    pub fn get(&self, id: &StableId) -> Tier {
        self.0.get(id).copied().unwrap_or(Tier::Relay)
    }

    /// Every connected peer whose tier this poll changes, with its new
    /// tier. `connected` is the engine's peers with the tiers it holds;
    /// `bindings` maps a node ID to its Tailscale node. A peer with no
    /// binding has no path info, and is `relay`.
    pub fn changes(
        &self,
        connected: impl IntoIterator<Item = (NodeId, Tier)>,
        bindings: &BTreeMap<NodeId, StableId>,
    ) -> Vec<(NodeId, Tier)> {
        connected
            .into_iter()
            .filter_map(|(peer, was)| {
                let tier = bindings.get(&peer).map_or(Tier::Relay, |id| self.get(id));
                (tier != was).then_some((peer, tier))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn path(direct: Option<&str>, relay: Option<&str>) -> Option<PathInfo> {
        Some(PathInfo {
            direct: direct.map(|addr| addr.parse::<SocketAddr>().unwrap()),
            relay: relay.map(str::to_owned),
        })
    }

    /// §6.4, case by case: reached directly at a private address is `lan`,
    /// at any other `direct`, and anything else `relay`, missing path info
    /// included.
    #[test]
    fn the_tier_of_each_kind_of_path() {
        let cases: &[(&str, Option<PathInfo>, Tier)] = &[
            // RFC 1918, each range, at its edges.
            ("10/8", path(Some("10.0.0.1:41641"), None), Tier::Lan),
            (
                "10/8 top",
                path(Some("10.255.255.254:41641"), None),
                Tier::Lan,
            ),
            ("172.16/12", path(Some("172.16.0.1:41641"), None), Tier::Lan),
            (
                "172.16/12 top",
                path(Some("172.31.255.254:41641"), None),
                Tier::Lan,
            ),
            (
                "below 172.16/12",
                path(Some("172.15.255.254:41641"), None),
                Tier::Direct,
            ),
            (
                "above 172.16/12",
                path(Some("172.32.0.1:41641"), None),
                Tier::Direct,
            ),
            (
                "192.168/16",
                path(Some("192.168.1.20:41641"), None),
                Tier::Lan,
            ),
            (
                "192.169",
                path(Some("192.169.0.1:41641"), None),
                Tier::Direct,
            ),
            // Link-local, both families.
            (
                "169.254/16",
                path(Some("169.254.10.1:41641"), None),
                Tier::Lan,
            ),
            ("fe80::/10", path(Some("[fe80::1]:41641"), None), Tier::Lan),
            // An IPv4 address in IPv6 form is judged as IPv4.
            (
                "::ffff:192.168",
                path(Some("[::ffff:192.168.1.20]:41641"), None),
                Tier::Lan,
            ),
            (
                "::ffff:public",
                path(Some("[::ffff:203.0.113.7]:41641"), None),
                Tier::Direct,
            ),
            // Public addresses, both families.
            (
                "public v4",
                path(Some("203.0.113.7:41641"), None),
                Tier::Direct,
            ),
            (
                "public v6",
                path(Some("[2001:db8::7]:41641"), None),
                Tier::Direct,
            ),
            // Not RFC 1918 or link-local, so not `lan` before Phase 3's
            // same-subnet rule: IPv6 unique local, loopback, and CGNAT.
            (
                "fc00::/7",
                path(Some("[fd12:3456::1]:41641"), None),
                Tier::Direct,
            ),
            (
                "loopback",
                path(Some("127.0.0.1:41641"), None),
                Tier::Direct,
            ),
            (
                "100.64/10",
                path(Some("100.64.0.1:41641"), None),
                Tier::Direct,
            ),
            // A direct address wins over the DERP region Tailscale also
            // names: it is the peer's home, not the path in use.
            (
                "direct and relay",
                path(Some("192.168.1.20:41641"), Some("fra")),
                Tier::Lan,
            ),
            ("relay", path(None, Some("fra")), Tier::Relay),
            ("no path", path(None, None), Tier::Relay),
            ("no path info", None, Tier::Relay),
        ];
        for (name, path, tier) in cases {
            assert_eq!(classify(path.as_ref()), *tier, "{name}");
        }
    }

    fn id(s: &str) -> StableId {
        StableId(s.to_owned())
    }

    fn node(n: u8) -> NodeId {
        NodeId::from_bytes([n; 16])
    }

    fn peer(stable: &str, path: Option<PathInfo>) -> Peer {
        Peer {
            hostname: stable.to_owned(),
            addrs: Vec::new(),
            stable_id: id(stable),
            user: "me@example.com".to_owned(),
            online: true,
            tags: Vec::new(),
            path,
        }
    }

    #[test]
    fn a_node_the_poll_did_not_find_is_relay() {
        let tiers = Tiers::of(&[peer("a", path(Some("10.0.0.1:1"), None))]);
        assert_eq!(tiers.get(&id("a")), Tier::Lan);
        assert_eq!(tiers.get(&id("b")), Tier::Relay);
        assert_eq!(Tiers::default().get(&id("a")), Tier::Relay);
    }

    /// Only connected peers whose tier differs are named; a peer with no
    /// binding, or one the poll did not find, is `relay`.
    #[test]
    fn changes_name_each_connected_peer_whose_tier_differs() {
        let tiers = Tiers::of(&[
            peer("a", path(Some("10.0.0.1:1"), None)),
            peer("b", path(Some("203.0.113.7:1"), None)),
            peer("c", path(None, Some("fra"))),
            peer("unconnected", path(Some("10.0.0.2:1"), None)),
        ]);
        let bindings: BTreeMap<NodeId, StableId> = [
            (node(1), id("a")),
            (node(2), id("b")),
            (node(3), id("c")),
            (node(4), id("gone from tailscale")),
            (node(9), id("unconnected")),
        ]
        .into_iter()
        .collect();
        let connected = [
            (node(1), Tier::Relay),  // now lan
            (node(2), Tier::Direct), // unchanged
            (node(3), Tier::Lan),    // now relay
            (node(4), Tier::Lan),    // not listed: relay
            (node(5), Tier::Direct), // no binding: relay
            (node(6), Tier::Relay),  // no binding, already relay
        ];
        assert_eq!(
            tiers.changes(connected, &bindings),
            [
                (node(1), Tier::Lan),
                (node(3), Tier::Relay),
                (node(4), Tier::Relay),
                (node(5), Tier::Relay),
            ]
        );
    }
}
