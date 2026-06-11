//! Authentication and authorization scaffolding (ADR-W005).
//!
//! Two concerns live here:
//!
//! * **Authentication.** Machine/API access uses a long, high-entropy random
//!   **API key** presented as a `Bearer` token. Keys are stored only as
//!   **HMAC-SHA256** digests (the brief: high-entropy keys do not need a slow
//!   KDF). [`ApiKeyStore::verify`] recomputes the HMAC over the presented key
//!   and compares in **constant time** ([`subtle`]) to defeat timing oracles.
//! * **Authorization.** A coarse [`Role`] (admin/operator/viewer) gates *what
//!   actions* a principal may take, and a per-object [`authorize_object`] check
//!   defends against **BOLA** (OWASP API1) — the broken-object-level-authz risk
//!   the brief flags as #1: role gating alone is not enough, every resource id
//!   must be checked against what the principal is allowed to touch.
//!
//! This is scaffolding: the cookie-session + CSRF path for the browser UI is
//! out of scope here; API-key + RBAC + per-object authz is the tested surface.
use std::collections::HashMap;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::ControlError;

type HmacSha256 = Hmac<Sha256>;

/// A coarse role governing which actions a principal may perform.
///
/// Ordered by privilege: `ReadOnly < Viewer < Operator < Admin`. [`Role::can`]
/// expresses the action gate.
///
/// `ReadOnly` and `Viewer` are both read-only at the action gate; they differ in
/// intent and ordering: `ReadOnly` is the floor (e.g. an audit/compliance role
/// that should never appear in a write-capable context), while `Viewer` is the
/// conventional observer. Keeping them distinct lets deployments and the
/// claims→role mapping express least privilege precisely without changing the
/// write gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Strictly read-only floor: may observe state and stream events, never
    /// mutate, and is the lowest privilege (below [`Role::Viewer`]).
    ReadOnly,
    /// Read-only: may observe state and stream events, never mutate.
    Viewer,
    /// May perform day-to-day operations (start/stop/swap, edit layouts).
    Operator,
    /// Full control, including managing API keys and destructive actions.
    Admin,
}

/// A management action subject to role gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Action {
    /// Read a resource or subscribe to events.
    Read,
    /// Create or modify a resource, or submit an operational command.
    Write,
    /// Delete a resource or perform an administrative action.
    Administer,
}

impl Role {
    /// Whether a principal with this role may perform `action`.
    ///
    /// * `ReadOnly` and `Viewer` may only [`Action::Read`].
    /// * `Operator` may [`Action::Read`] and [`Action::Write`].
    /// * `Admin` may do anything.
    #[must_use]
    pub fn can(self, action: Action) -> bool {
        matches!(
            (self, action),
            (Self::Admin, _)
                | (Self::Operator, Action::Read | Action::Write)
                | (Self::Viewer | Self::ReadOnly, Action::Read)
        )
    }

    /// Enforce the action gate, producing a [`ControlError::Forbidden`] on deny.
    ///
    /// # Errors
    ///
    /// [`ControlError::Forbidden`] if this role may not perform `action`.
    pub fn require(self, action: Action) -> Result<(), ControlError> {
        if self.can(action) {
            Ok(())
        } else {
            Err(ControlError::Forbidden(format!(
                "role {self:?} may not perform {action:?}"
            )))
        }
    }
}

/// An authenticated principal: the identity behind a verified API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// A stable, non-secret identifier for the key (for audit/logging).
    pub key_id: String,
    /// The principal's role.
    pub role: Role,
    /// The set of object ids this principal may access, or `None` for "all
    /// objects of any kind it has the role for" (e.g. an admin or an
    /// unrestricted operator). Used by [`authorize_object`] for BOLA defense.
    pub scoped_object_ids: Option<Vec<String>>,
    /// The set of **output** ids this principal may act on (the output-scoped
    /// operator role), or `None` for "any output". Checked independently of
    /// [`Principal::scoped_object_ids`] by [`authorize_output`] so a role
    /// confined to a subset of program outputs cannot touch another's
    /// renditions even when its action role would otherwise permit it (per-
    /// output BOLA).
    pub scoped_output_ids: Option<Vec<String>>,
}

impl Principal {
    /// Whether this principal is scoped to a specific object-id allowlist.
    #[must_use]
    pub fn is_scoped(&self) -> bool {
        self.scoped_object_ids.is_some()
    }

    /// Whether this principal is confined to a specific output-id allowlist.
    #[must_use]
    pub fn is_output_scoped(&self) -> bool {
        self.scoped_output_ids.is_some()
    }

    /// The full-access principal used when authentication is **disabled** (an
    /// explicit, opt-in deployment mode for trusted/local networks). It carries
    /// [`Role::Admin`] and no object/output scoping, so every request is treated
    /// as a local administrator. Reachable only when the operator turned auth off
    /// — the default build still requires a verified API key.
    #[must_use]
    pub fn local_admin() -> Self {
        Self {
            key_id: "local-admin".to_owned(),
            role: Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
        }
    }
}

/// Per-object authorization (BOLA defense, ADR-W005 / OWASP API1).
///
/// Checked on **every** resource id a request addresses — not just the role. A
/// principal scoped to an explicit object allowlist is denied any id outside it,
/// even when its role would otherwise permit the action.
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal is scoped and `object_id` is not
/// in its allowlist.
pub fn authorize_object(principal: &Principal, object_id: &str) -> Result<(), ControlError> {
    match &principal.scoped_object_ids {
        None => Ok(()),
        Some(allowed) => {
            if allowed.iter().any(|id| id == object_id) {
                Ok(())
            } else {
                Err(ControlError::Forbidden(format!(
                    "principal {:?} is not authorized for object {object_id:?}",
                    principal.key_id
                )))
            }
        }
    }
}

/// Per-**output** authorization (output-scoped role; per-output BOLA defense).
///
/// The output-scoped operator role is confined to a subset of program outputs
/// (renditions/heads). This is checked on **every** output id a request
/// addresses, independently of [`authorize_object`]: a principal confined to an
/// output allowlist is denied any output id outside it, even when its role would
/// otherwise permit the action.
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal is output-scoped and `output_id`
/// is not in its allowlist.
pub fn authorize_output(principal: &Principal, output_id: &str) -> Result<(), ControlError> {
    match &principal.scoped_output_ids {
        None => Ok(()),
        Some(allowed) => {
            if allowed.iter().any(|id| id == output_id) {
                Ok(())
            } else {
                Err(ControlError::Forbidden(format!(
                    "principal {:?} is not authorized for output {output_id:?}",
                    principal.key_id
                )))
            }
        }
    }
}

/// A registered API key: its non-secret id, the HMAC digest of the secret, and
/// the principal it authenticates as.
#[derive(Debug, Clone)]
struct KeyRecord {
    digest: Vec<u8>,
    principal: Principal,
}

/// A store of registered API keys, verifying presented `Bearer` tokens against
/// stored HMAC-SHA256 digests in constant time.
///
/// The store is keyed by the non-secret `key_id` so verification is O(1) after
/// the client identifies which key it holds — but the comparison of the secret
/// itself is constant-time regardless. The HMAC key (a server pepper) binds the
/// digests to this deployment.
#[derive(Debug, Clone)]
pub struct ApiKeyStore {
    pepper: Vec<u8>,
    keys: HashMap<String, KeyRecord>,
}

impl ApiKeyStore {
    /// Create a store with the given server `pepper` (the HMAC key).
    #[must_use]
    pub fn new(pepper: impl Into<Vec<u8>>) -> Self {
        Self {
            pepper: pepper.into(),
            keys: HashMap::new(),
        }
    }

    /// Compute the HMAC-SHA256 digest of a secret under this store's pepper.
    #[must_use]
    pub fn digest(&self, secret: &str) -> Vec<u8> {
        // HMAC accepts a key of any length: `KeyInit::new_from_slice` is
        // documented infallible for `Hmac<_>` (it pads/hashes the key as
        // needed), so the `let-else` fallback below is defensive and never
        // taken — but it keeps this method total without `unwrap`/`expect`.
        let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(&self.pepper) else {
            return Vec::new();
        };
        mac.update(secret.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Register an API key for `principal`, computing and storing its digest.
    pub fn register(&mut self, key_id: impl Into<String>, secret: &str, principal: Principal) {
        let key_id = key_id.into();
        let digest = self.digest(secret);
        self.keys.insert(key_id, KeyRecord { digest, principal });
    }

    /// Verify a presented `Bearer` token of the form `<key_id>.<secret>`.
    ///
    /// The token carries the non-secret key id and the secret separated by a
    /// `.`; the secret is HMAC'd and compared to the stored digest in constant
    /// time. Returns the authenticated [`Principal`] on success.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unauthenticated`] if the token is malformed, the key id
    /// is unknown, or the secret's digest does not match.
    pub fn verify(&self, token: &str) -> Result<Principal, ControlError> {
        let (key_id, secret) = token.split_once('.').ok_or(ControlError::Unauthenticated)?;
        let record = self.keys.get(key_id).ok_or(ControlError::Unauthenticated)?;
        let presented = self.digest(secret);
        // Constant-time comparison: `ct_eq` returns a `Choice`; only accept on a
        // true bit. Lengths match (both are SHA-256 outputs) but `ct_eq` is
        // length-safe regardless.
        if presented.ct_eq(&record.digest).into() {
            Ok(record.principal.clone())
        } else {
            Err(ControlError::Unauthenticated)
        }
    }

    /// Extract and verify a principal from an HTTP `Authorization` header value.
    ///
    /// Expects `Bearer <key_id>.<secret>`.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unauthenticated`] if the scheme is missing/not `Bearer`
    /// or the token does not verify.
    pub fn verify_authorization(
        &self,
        header_value: Option<&str>,
    ) -> Result<Principal, ControlError> {
        let value = header_value.ok_or(ControlError::Unauthenticated)?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .ok_or(ControlError::Unauthenticated)?;
        self.verify(token.trim())
    }
}

/// Build the control plane's API-key store with a bootstrap **admin** key.
///
/// Secure-by-default access for the management API/UI without shipping a secret
/// in config (CLAUDE.md secret hygiene — credentials come from the environment,
/// not the repo):
///
/// * `admin_secret = Some(secret)` — the operator supplied the admin secret
///   (e.g. via the `MULTIVIEW_CONTROL_TOKEN` environment variable). It is
///   registered as the `admin` key and is **stable across restarts**, so a saved
///   browser token keeps working. Returns `None` (nothing to surface — the
///   operator already holds the token).
/// * `admin_secret = None` — no secret was provided. A random admin secret is
///   generated and the full bearer token `admin.<secret>` is **returned** so the
///   caller can log it **once** for first access (the Grafana/Jenkins bootstrap
///   pattern). Regenerated each start until a stable secret is configured.
///
/// The presented bearer token is always `admin.<secret>`. The HMAC pepper is a
/// fresh random per process: digests are recomputed from the registered secret
/// on each start, so the pepper need not be persisted, and it never leaves the
/// process. Additional non-admin keys/roles are a follow-up (config-declared).
#[must_use]
pub fn provision_admin_keys(admin_secret: Option<String>) -> (ApiKeyStore, Option<String>) {
    // A per-process random pepper binds the digests to this run; it is never
    // persisted or logged. `uuid::Uuid::new_v4()` is CSPRNG-backed (getrandom).
    let pepper = uuid::Uuid::new_v4().into_bytes().to_vec();
    let mut store = ApiKeyStore::new(pepper);

    let (secret, bootstrap_token) = if let Some(secret) = admin_secret {
        (secret, None)
    } else {
        let secret = uuid::Uuid::new_v4().to_string();
        let token = format!("admin.{secret}");
        (secret, Some(token))
    };

    store.register(
        "admin",
        &secret,
        Principal {
            key_id: "admin".to_owned(),
            role: Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
        },
    );

    (store, bootstrap_token)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn operator_supplied_secret_authenticates_and_is_not_surfaced() {
        let (store, bootstrap) = provision_admin_keys(Some("s3cret".to_owned()));
        // Nothing to log back — the operator already holds the secret.
        assert!(bootstrap.is_none());
        // The presented bearer token is `admin.<secret>` and authenticates as Admin.
        let principal = store.verify("admin.s3cret").expect("admin token verifies");
        assert_eq!(principal.role, Role::Admin);
        assert_eq!(principal.key_id, "admin");
        // A wrong secret is rejected.
        assert!(store.verify("admin.wrong").is_err());
    }

    #[test]
    fn generated_bootstrap_token_is_returned_and_authenticates() {
        let (store, bootstrap) = provision_admin_keys(None);
        // A full bearer token is surfaced for first access.
        let token = bootstrap.expect("a bootstrap token is generated when none supplied");
        assert!(
            token.starts_with("admin."),
            "the bootstrap token is the full `admin.<secret>` bearer value"
        );
        // The verbatim surfaced token authenticates as Admin.
        let principal = store
            .verify(token.strip_prefix("Bearer ").unwrap_or(&token))
            .expect("the surfaced bootstrap token verifies");
        assert_eq!(principal.role, Role::Admin);
    }

    #[test]
    fn two_generations_differ_and_are_not_cross_valid() {
        let (store_a, token_a) = provision_admin_keys(None);
        let (_store_b, token_b) = provision_admin_keys(None);
        let token_a = token_a.unwrap();
        let token_b = token_b.unwrap();
        assert_ne!(token_a, token_b, "each generated secret is random");
        // store_a must not accept store_b's token (distinct secrets + peppers).
        assert!(store_a.verify(&token_b).is_err());
    }
}
