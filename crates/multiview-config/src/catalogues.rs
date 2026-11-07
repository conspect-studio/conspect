//! `[catalogues.iptv_org]` — the IPTV-ORG source-browser catalogue (ADR-M015).
//!
//! ```toml
//! [catalogues.iptv_org]
//! enabled = true            # kill switch for sync + browse + probe (default true)
//! interval = "6h"           # sync cadence (min 10m); jittered
//! probe_linked_every = "1h" # optional: re-probe linked sources (off when absent)
//! ```

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Minimum accepted `interval` (10 minutes).
pub const MIN_SYNC_INTERVAL_MS: u64 = 10 * 60 * 1000;
/// Minimum accepted `probe_linked_every` (5 minutes).
pub const MIN_PROBE_LINKED_MS: u64 = 5 * 60 * 1000;

/// The `[catalogues]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CataloguesConfig {
    /// The IPTV-ORG catalogue. Absent ⇒ built-in defaults (enabled, 6 h sync).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iptv_org: Option<IptvOrgConfig>,
}

fn default_true() -> bool {
    true
}

/// `[catalogues.iptv_org]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IptvOrgConfig {
    /// Master switch (default `true`). `false` disables sync, browse and probe.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Sync interval (explicit-unit string, default `6h` ⇒ `360m`; min 10 min).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<crate::webrtc::DurationString>,
    /// Re-probe linked sources on this cadence (min 5 min). Absent ⇒ off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_linked_every: Option<crate::webrtc::DurationString>,
}

impl Default for IptvOrgConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: None,
            probe_linked_every: None,
        }
    }
}

impl IptvOrgConfig {
    /// The effective sync interval (default 6 h).
    #[must_use]
    pub fn sync_interval(&self) -> std::time::Duration {
        self.interval.map_or(
            std::time::Duration::from_secs(6 * 3600),
            crate::webrtc::DurationString::as_duration,
        )
    }

    /// Validate bounds.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the offending key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(i) = self.interval {
            if i.millis() < MIN_SYNC_INTERVAL_MS {
                return Err(ConfigError::Validation(
                    "catalogues.iptv_org.interval must be at least 10m".to_owned(),
                ));
            }
        }
        if let Some(p) = self.probe_linked_every {
            if p.millis() < MIN_PROBE_LINKED_MS {
                return Err(ConfigError::Validation(
                    "catalogues.iptv_org.probe_linked_every must be at least 5m".to_owned(),
                ));
            }
        }
        Ok(())
    }
}
