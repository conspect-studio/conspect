//! The `[network]` section (ADR-0107, design note
//! `docs/architecture/network-front-door.md` section 10): the engine's single
//! network front door. Slice 1 ships the keys that select and size the shared
//! UDP socket; later slices add the TCP listener, TLS and TURN keys.
//!
//! Absent `[network]` keeps the legacy behaviour unchanged: WebRTC on
//! `webrtc.udp_port`, `MoQ` on `MULTIVIEW_MOQ_BIND`. Present, the front door owns
//! one dual-stack `[::]:https_port` UDP socket for both.

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Default `network.https_port`.
pub const DEFAULT_HTTPS_PORT: u16 = 443;
/// Default `network.workers`: OS threads of the front door's own runtime.
pub const DEFAULT_WORKERS: u16 = 2;
/// Most `network.workers` accepted.
pub const MAX_WORKERS: u16 = 64;

const fn default_https_port() -> u16 {
    DEFAULT_HTTPS_PORT
}

const fn default_workers() -> u16 {
    DEFAULT_WORKERS
}

/// `[network]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NetworkConfig {
    /// The UDP port of the shared front-door socket (QUIC/MoQ and WebRTC),
    /// bound dual-stack on `[::]`. Default 443. A port below 1024 needs
    /// `CAP_NET_BIND_SERVICE` (or systemd socket activation, a later slice).
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    /// Public addresses for ICE host candidates (IP literals, IPv6 first). When
    /// non-empty it replaces `webrtc.advertised_addresses`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advertise: Vec<String>,
    /// OS threads of the front door's own Tokio runtime. It never shares the
    /// runtime that drives the output clock (invariant #1). Default 2.
    #[serde(default = "default_workers")]
    pub workers: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            https_port: DEFAULT_HTTPS_PORT,
            advertise: Vec::new(),
            workers: DEFAULT_WORKERS,
        }
    }
}

impl NetworkConfig {
    /// Validate the section.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] for port 0, a worker count outside
    /// `1..=MAX_WORKERS`, or an `advertise` entry that is not an IP literal.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.https_port == 0 {
            return Err(ConfigError::Validation(
                "network.https_port must not be 0: the front door needs a stable port".to_owned(),
            ));
        }
        if self.workers == 0 || self.workers > MAX_WORKERS {
            return Err(ConfigError::Validation(format!(
                "network.workers must be between 1 and {MAX_WORKERS}"
            )));
        }
        for address in &self.advertise {
            let ip: std::net::IpAddr = address.parse().map_err(|_| {
                ConfigError::Validation(format!(
                    "network.advertise entry {address:?} is not an IP literal"
                ))
            })?;
            if ip.is_unspecified() {
                return Err(ConfigError::Validation(format!(
                    "network.advertise entry {address:?} is unspecified"
                )));
            }
        }
        Ok(())
    }

    /// The advertised IP literals, IPv6 first (ADR-0042).
    #[must_use]
    pub fn advertised_ips(&self) -> Vec<std::net::IpAddr> {
        let mut ips: Vec<std::net::IpAddr> = self
            .advertise
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect();
        ips.sort_by_key(std::net::IpAddr::is_ipv4);
        ips
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_round_trip() {
        let n: NetworkConfig = toml::from_str("").unwrap();
        assert_eq!(n, NetworkConfig::default());
        assert_eq!(n.https_port, 443);
        assert_eq!(n.workers, 2);
        let n: NetworkConfig = toml::from_str(
            "https_port = 8443\nadvertise = [\"203.0.113.10\", \"2001:db8::10\"]\nworkers = 3",
        )
        .unwrap();
        n.validate().unwrap();
        assert_eq!(
            n.advertised_ips(),
            vec![
                "2001:db8::10".parse::<std::net::IpAddr>().unwrap(),
                "203.0.113.10".parse().unwrap()
            ]
        );
    }

    #[test]
    fn rejects_bad_values_and_unknown_keys() {
        for bad in [
            "https_port = 0",
            "workers = 0",
            "workers = 65",
            "advertise = [\"host.example\"]",
            "advertise = [\"::\"]",
        ] {
            let n: NetworkConfig = toml::from_str(bad).unwrap();
            assert!(n.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<NetworkConfig>("bogus = 1").is_err());
    }
}
