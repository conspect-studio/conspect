//! `[network_diagnostics]` — the network-path diagnostics panel (docs/io/iptv-org.md §3a).
//!
//! ```toml
//! [network_diagnostics]
//! egress_lookup = true   # ask https://www.cloudflare.com/cdn-cgi/trace how the internet sees this server
//! ```

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// The `[network_diagnostics]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NetworkDiagnosticsConfig {
    /// Look up the server's public egress identity (fixed host, cached 10 min).
    /// `false` makes no outbound request to that host. Default `true`.
    #[serde(default = "default_true")]
    pub egress_lookup: bool,
}

impl Default for NetworkDiagnosticsConfig {
    fn default() -> Self {
        Self {
            egress_lookup: true,
        }
    }
}
