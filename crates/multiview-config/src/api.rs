//! The `[api]` configuration section: config-declared API keys (ADR-W026).
//!
//! Bootstrap admin authentication stays environment-only
//! (`MULTIVIEW_CONTROL_TOKEN`, always unscoped). This section makes **scoped,
//! non-admin** API keys mintable declaratively, so the object / output /
//! discovery-domain authorization axes have live principals — without it the
//! axes are "a lock with no keyhole" (no production path can mint a scoped key).
//!
//! Each key's secret is referenced by an **environment-variable name**
//! (`secret_env`) — never inlined (rule 34: secrets never touch git). The
//! `multiview-cli` startup wiring resolves the env var and registers the key with
//! the control-plane `ApiKeyStore`.
//!
//! ```toml
//! [[api.keys]]
//! key_id = "site-a-operator"
//! secret_env = "MULTIVIEW_KEY_SITE_A"
//! role = "operator"
//! scoped_object_ids = ["cam-3"]
//! scoped_output_ids = ["out-1", "program:main"]
//! scoped_discovery_domains = ["site-a"]
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The reserved program-scope prefix in [`ApiKeyConfig::scoped_output_ids`]: an
/// entry `program:<id>` grants the timing epoch for program `<id>` (ADR-W026),
/// distinct from a plain output grant of the same id — this keeps a timing grant
/// from punning the plain output-id namespace.
pub const PROGRAM_SCOPE_PREFIX: &str = "program:";

/// The coarse role a config-declared API key authenticates as.
///
/// A **non-admin subset** of the control-plane `Role` (which this crate cannot
/// depend on — the dependency runs control→config, never the reverse);
/// `multiview-control` maps this to `Role` at registration. There is
/// deliberately **no** `admin` variant: admin authentication is environment-only
/// (the bootstrap `MULTIVIEW_CONTROL_TOKEN`, always unscoped), so config-as-code
/// can never mint an administrator — a `[[api.keys]]` declaring `role = "admin"`
/// is structurally unrepresentable and fails to parse (the mint invariant,
/// ADR-W026). Deliberately **not** `#[non_exhaustive]`: the role set is small and
/// closed, and the control-plane mapping must stay an exhaustive match so adding
/// a role compile-forces its authorization handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyRole {
    /// Strictly read-only floor (below [`ApiKeyRole::Viewer`]).
    ReadOnly,
    /// Read-only observer.
    Viewer,
    /// Day-to-day operations (start/stop/swap, edit layouts).
    Operator,
}

/// One config-declared API key (ADR-W026).
///
/// `#[non_exhaustive]`: construct via [`ApiKeyConfig::new`] plus the
/// `with_scoped_*` builders so a future scope axis never breaks a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApiKeyConfig {
    /// Stable, non-secret key id (audit/logging; the presented token is
    /// `<key_id>.<secret>`).
    pub key_id: String,
    /// The **name** of the environment variable the key's secret is read from at
    /// startup — never the secret itself (rule 34).
    pub secret_env: String,
    /// The role this key authenticates as.
    pub role: ApiKeyRole,
    /// Object-id allowlist (BOLA); absent ⇒ unrestricted on the object axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_object_ids: Option<Vec<String>>,
    /// Output-id allowlist: a plain id authorizes that output; a
    /// `program:<id>` entry authorizes only the timing epoch for program `<id>`
    /// (ADR-W026). Absent ⇒ unrestricted on the output axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_output_ids: Option<Vec<String>>,
    /// Discovery-domain allowlist (ADR-W026): absent ⇒ sees all rows including
    /// unlabelled; `Some([])` ⇒ sees no discovery inventory; `Some(list)` ⇒ only
    /// rows labelled with a listed domain (unlabelled DENIED — fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_discovery_domains: Option<Vec<String>>,
}

impl ApiKeyConfig {
    /// Build an unscoped key of `role` whose secret is read from `secret_env`.
    #[must_use]
    pub fn new(key_id: impl Into<String>, secret_env: impl Into<String>, role: ApiKeyRole) -> Self {
        Self {
            key_id: key_id.into(),
            secret_env: secret_env.into(),
            role,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }

    /// Confine the key to an object-id allowlist (BOLA).
    #[must_use]
    pub fn with_scoped_object_ids(mut self, ids: Vec<String>) -> Self {
        self.scoped_object_ids = Some(ids);
        self
    }

    /// Confine the key to an output-id allowlist (plain outputs + `program:`
    /// timing grants).
    #[must_use]
    pub fn with_scoped_output_ids(mut self, ids: Vec<String>) -> Self {
        self.scoped_output_ids = Some(ids);
        self
    }

    /// Confine the key to a discovery-domain allowlist.
    #[must_use]
    pub fn with_scoped_discovery_domains(mut self, domains: Vec<String>) -> Self {
        self.scoped_discovery_domains = Some(domains);
        self
    }
}

/// The `[api]` section (ADR-W026): config-declared API keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApiConfig {
    /// The config-declared API keys. Absent/empty ⇒ only the bootstrap admin
    /// (env token) exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ApiKeyConfig>,
}

impl ApiConfig {
    /// Validate the API-key declarations (ADR-W026): non-empty unique key ids, a
    /// non-empty `secret_env` per key, non-empty output grants with well-formed
    /// `program:` prefixes, and DNS-label-like discovery domains.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the first violation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for key in &self.keys {
            if key.key_id.trim().is_empty() {
                return Err(ConfigError::Validation(
                    "api.keys: a key_id must not be empty".to_owned(),
                ));
            }
            if !seen.insert(key.key_id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "api.keys: duplicate key_id {:?}",
                    key.key_id
                )));
            }
            if key.secret_env.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "api.keys[{}]: secret_env must name the environment variable holding \
                     the secret (never inline a secret)",
                    key.key_id
                )));
            }
            if let Some(objects) = &key.scoped_object_ids {
                for entry in objects {
                    if entry.is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "api.keys[{}].scoped_object_ids: an entry must not be empty",
                            key.key_id
                        )));
                    }
                }
            }
            if let Some(outputs) = &key.scoped_output_ids {
                for entry in outputs {
                    if entry.is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "api.keys[{}].scoped_output_ids: an entry must not be empty",
                            key.key_id
                        )));
                    }
                    // A `program:` grant must name a program after the reserved
                    // prefix — `"program:"` alone is meaningless.
                    if let Some(program) = entry.strip_prefix(PROGRAM_SCOPE_PREFIX) {
                        if program.is_empty() {
                            return Err(ConfigError::Validation(format!(
                                "api.keys[{}].scoped_output_ids: {entry:?} names no program \
                                 after the reserved \"program:\" prefix",
                                key.key_id
                            )));
                        }
                    }
                }
            }
            if let Some(domains) = &key.scoped_discovery_domains {
                for domain in domains {
                    crate::discovery::validate_discovery_domain(
                        domain,
                        &format!("api.keys[{}].scoped_discovery_domains", key.key_id),
                    )?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{ApiConfig, ApiKeyConfig, ApiKeyRole};

    fn key(key_id: &str, secret_env: &str) -> ApiKeyConfig {
        ApiKeyConfig::new(key_id, secret_env, ApiKeyRole::Operator)
    }

    #[test]
    fn default_api_section_is_empty_and_valid() {
        let cfg = ApiConfig::default();
        assert!(cfg.keys.is_empty());
        cfg.validate().expect("the empty section validates");
    }

    #[test]
    fn old_toml_without_api_parses_and_a_full_key_round_trips() {
        // A fully-specified key parses from the documented `[[keys]]` shape and
        // round-trips losslessly.
        let toml = r#"
            [[keys]]
            key_id = "site-a-operator"
            secret_env = "MULTIVIEW_KEY_SITE_A"
            role = "operator"
            scoped_object_ids = ["cam-3"]
            scoped_output_ids = ["out-1", "program:main"]
            scoped_discovery_domains = ["site-a"]
        "#;
        let cfg: ApiConfig = toml::from_str(toml).expect("api keys parse");
        cfg.validate().expect("a well-formed key validates");
        assert_eq!(cfg.keys.len(), 1);
        assert_eq!(cfg.keys[0].role, ApiKeyRole::Operator);
        assert_eq!(
            cfg.keys[0].scoped_output_ids.as_deref(),
            Some(&["out-1".to_owned(), "program:main".to_owned()][..])
        );
        let wire = toml::to_string(&cfg).expect("serializes");
        let back: ApiConfig = toml::from_str(&wire).expect("round-trips");
        assert_eq!(back, cfg);
    }

    #[test]
    fn duplicate_key_ids_are_rejected() {
        let cfg = ApiConfig {
            keys: vec![key("dup", "ENV_A"), key("dup", "ENV_B")],
        };
        assert!(
            cfg.validate().is_err(),
            "a duplicate key_id must be rejected"
        );
    }

    #[test]
    fn empty_secret_env_is_rejected() {
        // A secret must be referenced by env-var name, never omitted (and never
        // inlined — there is no field to inline it into).
        let cfg = ApiConfig {
            keys: vec![key("k", "   ")],
        };
        assert!(
            cfg.validate().is_err(),
            "a blank secret_env must be rejected (rule 34)"
        );
    }

    #[test]
    fn empty_key_id_is_rejected() {
        let cfg = ApiConfig {
            keys: vec![key("", "ENV")],
        };
        assert!(cfg.validate().is_err(), "an empty key_id must be rejected");
    }

    #[test]
    fn malformed_program_grant_is_rejected() {
        let cfg = ApiConfig {
            keys: vec![key("k", "ENV").with_scoped_output_ids(vec!["program:".to_owned()])],
        };
        assert!(
            cfg.validate().is_err(),
            "a bare \"program:\" grant names no program and must be rejected"
        );
    }

    #[test]
    fn empty_output_grant_is_rejected() {
        let cfg = ApiConfig {
            keys: vec![key("k", "ENV").with_scoped_output_ids(vec![String::new()])],
        };
        assert!(cfg.validate().is_err(), "an empty output grant is rejected");
    }

    #[test]
    fn malformed_discovery_domain_is_rejected() {
        let cfg = ApiConfig {
            keys: vec![key("k", "ENV").with_scoped_discovery_domains(vec!["Site A".to_owned()])],
        };
        assert!(
            cfg.validate().is_err(),
            "a non-DNS-label discovery domain must be rejected"
        );
    }

    #[test]
    fn well_formed_scoped_key_validates() {
        let cfg = ApiConfig {
            keys: vec![key("site-a-op", "MULTIVIEW_KEY_SITE_A")
                .with_scoped_object_ids(vec!["cam-3".to_owned()])
                .with_scoped_output_ids(vec!["out-1".to_owned(), "program:main".to_owned()])
                .with_scoped_discovery_domains(vec!["site-a".to_owned()])],
        };
        cfg.validate()
            .expect("a fully-scoped, well-formed key validates");
    }

    #[test]
    fn config_cannot_mint_an_admin_key() {
        // MINT INVARIANT (ADR-W026, auth-panel F2): admin authentication is
        // environment-only (the bootstrap `MULTIVIEW_CONTROL_TOKEN`, always
        // unscoped). Config-as-code mints only NON-admin keys, so `ApiKeyRole`
        // carries no `admin` variant at all — a `[[keys]]` declaring
        // `role = "admin"` is structurally unrepresentable and fails to parse
        // (fail-closed), so config can never silently mint a full-or-scoped
        // administrator.
        let toml = r#"
            [[keys]]
            key_id = "sneaky-admin"
            secret_env = "ENV_SNEAKY"
            role = "admin"
        "#;
        let parsed: Result<ApiConfig, _> = toml::from_str(toml);
        assert!(
            parsed.is_err(),
            "a config-declared key must not be able to authenticate as admin"
        );

        // The non-admin roles remain config-mintable (the guard does not
        // over-restrict): every role the control plane maps still parses.
        for role in ["read_only", "viewer", "operator"] {
            let toml =
                format!("[[keys]]\nkey_id = \"k\"\nsecret_env = \"ENV_K\"\nrole = \"{role}\"\n");
            let parsed: Result<ApiConfig, _> = toml::from_str(&toml);
            assert!(
                parsed.is_ok(),
                "the non-admin role {role:?} must remain config-mintable"
            );
        }
    }

    #[test]
    fn empty_object_grant_is_rejected() {
        // Auth-panel F5: an empty object-id entry is as meaningless as an empty
        // output grant, and — unchecked — would silently widen the object axis.
        // It must be rejected, at parity with `scoped_output_ids`.
        let cfg = ApiConfig {
            keys: vec![key("k", "ENV").with_scoped_object_ids(vec![String::new()])],
        };
        assert!(
            cfg.validate().is_err(),
            "an empty scoped_object_ids entry must be rejected"
        );
    }
}
