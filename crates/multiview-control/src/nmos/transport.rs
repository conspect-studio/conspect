//! Live NMOS network transport (off-by-default `nmos` feature).
//!
//! The NMOS **models + Node API** ([`super::is04`]..[`super::is10`],
//! [`super::nmos_router`]) are pure and always compiled. This module is the thin,
//! feature-gated seam for the parts that need a real network:
//!
//! * **registry discovery** via mDNS/DNS-SD (`_nmos-register._tcp`), and
//! * **registration** by `POST`ing an [`is04::Registration`] to a discovered
//!   Registry, plus the periodic heartbeat,
//! * the **ST 2110 receiver bind** (joining the multicast group from the
//!   activated IS-05 [`is05::TransportParams`] on a real NIC).
//!
//! It is **compile-only** in this environment — there is no PTP NIC, no registry,
//! and no 2110 network — so it holds the typed contracts and the pure
//! request/SDP builders that a live client would use, not the socket itself.
//! Keeping the network here preserves the CI-green, pure-Rust default build.
use super::is04::{self, Registration};
use super::is05;

/// A discovered NMOS Registry's registration API base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEndpoint {
    /// The registration API base, e.g. `http://registry.local/x-nmos/registration/v1.3`.
    pub base_url: String,
    /// The advertised registry priority (lower wins; DNS-SD `pri=`).
    pub priority: u16,
}

/// The DNS-SD service type a Multiview node browses to find a Registry.
pub const REGISTER_SERVICE: &str = "_nmos-register._tcp";

/// The URL an [`is04::Registration`] is `POST`ed to at a registry endpoint.
#[must_use]
pub fn resource_url(registry: &RegistryEndpoint) -> String {
    format!("{}/resource", registry.base_url.trim_end_matches('/'))
}

/// The URL a node heartbeats at (`/health/nodes/{node_id}`).
#[must_use]
pub fn heartbeat_url(registry: &RegistryEndpoint, node_id: &str) -> String {
    format!(
        "{}/health/nodes/{node_id}",
        registry.base_url.trim_end_matches('/')
    )
}

/// Build the registration body a node would `POST` to advertise itself.
#[must_use]
pub fn node_registration(node: &is04::Node) -> Registration {
    Registration::node(node)
}

/// The multicast group + port a live receiver bind would join, taken from an
/// activated IS-05 transport-param leg.
///
/// Returns [`None`] if the leg carries no destination — there is nothing to bind.
#[must_use]
pub fn bind_target(params: &is05::TransportParams) -> Option<(String, u16)> {
    match (&params.destination_ip, params.destination_port) {
        (Some(ip), Some(port)) => Some((ip.clone(), port)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::super::is04::{Node, ResourceCore};
    use super::super::is05::TransportParams;
    use super::{bind_target, heartbeat_url, node_registration, resource_url, RegistryEndpoint};

    fn endpoint() -> RegistryEndpoint {
        RegistryEndpoint {
            base_url: "http://registry.local/x-nmos/registration/v1.3/".to_owned(),
            priority: 100,
        }
    }

    #[test]
    fn resource_and_heartbeat_urls_are_well_formed() {
        let ep = endpoint();
        assert_eq!(
            resource_url(&ep),
            "http://registry.local/x-nmos/registration/v1.3/resource"
        );
        assert_eq!(
            heartbeat_url(&ep, "node-1"),
            "http://registry.local/x-nmos/registration/v1.3/health/nodes/node-1"
        );
    }

    #[test]
    fn node_registration_wraps_the_node() {
        let node = Node {
            core: ResourceCore::new("node-1", "1700000000:0", "Multiview"),
            href: "http://multiview.local/".to_owned(),
            hostname: None,
        };
        let reg = node_registration(&node);
        assert_eq!(reg.data["id"], "node-1");
    }

    #[test]
    fn bind_target_needs_both_ip_and_port() {
        let full = TransportParams {
            destination_ip: Some("239.0.0.1".to_owned()),
            destination_port: Some(5004),
            source_ip: None,
            rtp_enabled: Some(true),
        };
        assert_eq!(bind_target(&full), Some(("239.0.0.1".to_owned(), 5004)));

        let no_port = TransportParams {
            destination_ip: Some("239.0.0.1".to_owned()),
            destination_port: None,
            source_ip: None,
            rtp_enabled: None,
        };
        assert_eq!(bind_target(&no_port), None);
    }
}
