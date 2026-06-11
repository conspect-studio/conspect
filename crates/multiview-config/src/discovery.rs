//! The `[discovery]` configuration section: mDNS/DNS-SD device-discovery
//! browse types (ADR-M008 §6; managed-devices brief §6).
//!
//! Discovery always browses the built-in Cast (`_googlecast._tcp`) and NDI
//! (`_ndi._tcp`) service types. This section adds the **operator-configured**
//! types: the zowietek control-API service type — which the vendor does not
//! document, so it is only ever recognised when an operator configures it
//! here, never fabricated from a built-in constant — and any extra DNS-SD
//! types to browse (reported `unknown` unless a driver family claims them).

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The `[discovery]` section: operator-configured mDNS/DNS-SD browse types.
///
/// ```toml
/// [discovery]
/// zowietek_service_type = "_zowietek-ctl._tcp.local."
/// extra_service_types = ["_extra._udp"]
/// ```
///
/// Absent ⇒ only the built-in Cast + NDI types are browsed and no service is
/// ever classified `zowietek-control` (the vendor's type is unverified —
/// best-effort, clearly labelled, never guessed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DiscoveryConfig {
    /// The zowietek control-API DNS-SD service type, e.g.
    /// `"_zowietek-ctl._tcp.local."`. Browsed in addition to the built-in
    /// types; a service of exactly this type is classified `zowietek-control`.
    /// Absent ⇒ no zowietek-control browse or classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zowietek_service_type: Option<String>,
    /// Extra DNS-SD service types to browse (e.g. `"_extra._udp"`). Services
    /// found under them are reported with their honest inferred driver kind
    /// (usually `unknown`) — extra browse scope, never extra trust.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_service_types: Vec<String>,
}

impl DiscoveryConfig {
    /// Build a discovery section from its parts (the struct is
    /// `#[non_exhaustive]`, which forbids struct-expression construction
    /// outside this crate).
    #[must_use]
    pub fn new(zowietek_service_type: Option<String>, extra_service_types: Vec<String>) -> Self {
        Self {
            zowietek_service_type,
            extra_service_types,
        }
    }

    /// Validate every configured service type: each must be a well-formed
    /// DNS-SD type (`_name._tcp` / `_name._udp`, optionally `.local.`-suffixed).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the malformed entry.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ty) = &self.zowietek_service_type {
            validate_service_type(ty, "discovery.zowietek_service_type")?;
        }
        for ty in &self.extra_service_types {
            validate_service_type(ty, "discovery.extra_service_types")?;
        }
        Ok(())
    }
}

/// Check that `ty` is a plausible DNS-SD service type: it must start with `_`
/// and (after tolerating a trailing `.local.` / trailing dot) end with `._tcp`
/// or `._udp` — the two transport labels DNS-SD defines (RFC 6763 §7).
fn validate_service_type(ty: &str, field: &str) -> Result<(), ConfigError> {
    if ty.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field}: a DNS-SD service type must not be empty (omit the field instead)"
        )));
    }
    let trimmed = ty.trim_end_matches('.');
    let without_local = trimmed.strip_suffix(".local").unwrap_or(trimmed);
    let normalized = without_local.trim_end_matches('.');
    if !normalized.starts_with('_')
        || !(normalized.ends_with("._tcp") || normalized.ends_with("._udp"))
    {
        return Err(ConfigError::Validation(format!(
            "{field}: {ty:?} is not a DNS-SD service type (expected \
             \"_name._tcp\" or \"_name._udp\", optionally \".local.\"-suffixed)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::DiscoveryConfig;

    #[test]
    fn default_is_empty_and_valid() {
        let cfg = DiscoveryConfig::default();
        assert!(cfg.zowietek_service_type.is_none());
        assert!(cfg.extra_service_types.is_empty());
        cfg.validate().expect("the empty section validates");
    }

    #[test]
    fn well_formed_types_validate() {
        let cfg = DiscoveryConfig::new(
            Some("_zowietek-ctl._tcp.local.".to_owned()),
            vec!["_x._tcp".to_owned(), "_y._udp.local.".to_owned()],
        );
        cfg.validate().expect("well-formed DNS-SD types validate");
    }

    #[test]
    fn malformed_types_are_rejected() {
        for bad in ["", "zowietek.local", "_x._http", "x._tcp"] {
            let cfg = DiscoveryConfig::new(Some(bad.to_owned()), Vec::new());
            assert!(
                cfg.validate().is_err(),
                "{bad:?} must be rejected as a service type"
            );
        }
    }
}
