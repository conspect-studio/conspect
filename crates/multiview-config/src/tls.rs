//! Control-plane TLS termination schema (TLS-0, [ADR-W029](../../../docs/decisions/ADR-W029.md)) —
//! the static-cert floor of the management-plane HTTPS ladder, as operator
//! configuration.
//!
//! [`TlsConfig`] is the `[control.tls]` section: when present, the
//! `multiview-control` serve path terminates TLS (rustls) on the control
//! listener instead of plain HTTP. Absent ⇒ plain HTTP (today's behaviour — the
//! default build pulls no native TLS dependency). The runtime rustls
//! termination lives in `multiview-control`; this crate only **models +
//! validates** the knobs (mirroring the [`crate::limits`] split), so a config
//! carrying `[control.tls]` validates on any host even in a build without the
//! TLS serving feature.
//!
//! The union is **internally tagged by `mode`** (`#[serde(tag = "mode")]`, never
//! `untagged`) — the operator writes `mode = "static"` inline with the
//! `cert_file`/`key_file` paths, and future automated modes (ACME, later TLS
//! phases) slot in as new `mode` values without breaking the existing wire shape.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The `[control.tls]` section: how the control plane terminates TLS.
///
/// Internally tagged by `mode`. The only TLS-0 mode is `mode = "static"`
/// ([`TlsConfig::Static`]): an operator-managed PEM certificate + private key (the
/// TLS-0 floor; renewal is a drop-in file replacement + restart, no automation).
///
/// Absent from `[control]` ⇒ plain HTTP. `#[non_exhaustive]` because later TLS
/// phases add modes (e.g. ACME DNS-01) as new variants; the existing wire shape
/// is preserved.
///
/// # Examples
///
/// ```toml
/// [control.tls]
/// mode = "static"
/// cert_file = "/etc/multiview/tls/fullchain.pem"
/// key_file  = "/etc/multiview/tls/privkey.pem"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TlsConfig {
    /// Static, operator-managed certificate: terminate TLS with the PEM
    /// certificate chain at [`cert_file`](TlsConfig::Static::cert_file) and the
    /// private key at [`key_file`](TlsConfig::Static::key_file). Both are
    /// required. Renewal is the operator's responsibility (replace the files +
    /// restart); there is no ACME/automation in this mode.
    Static {
        /// Path to the PEM certificate chain (leaf certificate first, then any
        /// intermediates). Loaded at serve time by `multiview-control`.
        cert_file: PathBuf,
        /// Path to the PEM private key (PKCS#8, PKCS#1, or SEC1). Loaded at
        /// serve time by `multiview-control`.
        key_file: PathBuf,
    },
}

impl TlsConfig {
    /// Validate the TLS configuration at config load (fail-closed).
    ///
    /// Only the **deployment-independent shape** is validated here: a `static`
    /// mode's `cert_file` or `key_file` path must be non-empty (an empty path can
    /// never load a certificate, so it must fail at config load rather than at
    /// first bind). Existence/readability/parse of the PEM files is checked at
    /// serve time by `multiview-control` — a config can legitimately be authored
    /// on a host that is not the deployment target (mirroring how
    /// [`crate::ControlConfig::cast_media_base`] validates shape here and host
    /// reachability at startup).
    ///
    /// # Errors
    /// [`ConfigError::Validation`] if a `static` mode's `cert_file` or `key_file`
    /// path is empty.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            TlsConfig::Static {
                cert_file,
                key_file,
            } => {
                if cert_file.as_os_str().is_empty() {
                    return Err(ConfigError::Validation(
                        "control.tls.cert_file is empty (static mode needs a PEM certificate path)"
                            .to_owned(),
                    ));
                }
                if key_file.as_os_str().is_empty() {
                    return Err(ConfigError::Validation(
                        "control.tls.key_file is empty (static mode needs a PEM private-key path)"
                            .to_owned(),
                    ));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::TlsConfig;

    #[test]
    fn static_round_trips_through_json() {
        let tls = TlsConfig::Static {
            cert_file: std::path::PathBuf::from("/c.pem"),
            key_file: std::path::PathBuf::from("/k.pem"),
        };
        let json = serde_json::to_string(&tls).expect("serialize");
        // Internally tagged: the discriminant rides an inline `mode` field.
        assert!(json.contains(r#""mode":"static""#), "got {json}");
        let back: TlsConfig = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(tls, back);
    }

    #[test]
    fn an_empty_cert_or_key_is_rejected() {
        assert!(TlsConfig::Static {
            cert_file: std::path::PathBuf::new(),
            key_file: std::path::PathBuf::from("/k.pem"),
        }
        .validate()
        .is_err());
        assert!(TlsConfig::Static {
            cert_file: std::path::PathBuf::from("/c.pem"),
            key_file: std::path::PathBuf::new(),
        }
        .validate()
        .is_err());
    }
}
