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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use multiview_events::AuthzScope;

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
    ///
    /// Plain entries authorize [`AuthzScope::Output`]. Entries prefixed with
    /// `program:` authorize only [`AuthzScope::Program`] and are deliberately
    /// inert for plain output authorization (ADR-W026), preventing namespace
    /// punning between a program and an output that share an id.
    pub scoped_output_ids: Option<Vec<String>>,
    /// Discovery-domain allowlist (ADR-W026), or `None` for all domains,
    /// including unlabelled rows. `Some([])` sees no discovery inventory;
    /// `Some(labels)` sees only rows carrying one of those labels. A scoped
    /// principal is denied an unlabelled (`domain: None`) row — fail-closed so a
    /// stripped/missing label can never widen visibility.
    pub scoped_discovery_domains: Option<Vec<String>>,
}

/// Allocation-free borrowed view of a principal's three authorization axes.
///
/// Each `None` axis is unrestricted; `Some([])` denies every resource on that
/// axis; `Some(entries)` is an exact allowlist. The one fail-closed policy lives
/// in [`scope_permits`], shared by REST and realtime (ADR-W026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthzScopes<'a> {
    objects: Option<&'a [String]>,
    outputs: Option<&'a [String]>,
    discovery_domains: Option<&'a [String]>,
}

impl<'a> AuthzScopes<'a> {
    /// Borrow three owned optional allowlists as one authorization view.
    ///
    /// This constructor is used by realtime's session-owned scope state; it
    /// borrows only and allocates nothing on the event delivery path.
    #[must_use]
    pub fn new(
        objects: Option<&'a [String]>,
        outputs: Option<&'a [String]>,
        discovery_domains: Option<&'a [String]>,
    ) -> Self {
        Self {
            objects,
            outputs,
            discovery_domains,
        }
    }

    /// Whether this view imposes **no** restriction on any axis — the principal
    /// can see and act on the whole system.
    ///
    /// True iff every axis is `None` (unrestricted). Any `Some(_)` axis — even
    /// `Some([])`, which denies everything on that axis — makes the principal
    /// restricted and therefore **not** global. This is the single definition of
    /// "unrestricted across all axes"; whole-system-artifact gating keys off it
    /// (via [`Principal::is_global`]) so a newly-added axis is covered here in one
    /// place rather than in a frozen per-axis check (ADR-W026).
    ///
    /// The exhaustive `let Self { .. }` destructure below is deliberate and
    /// load-bearing: it carries **no** `..` rest pattern, so adding a fourth
    /// scope axis to [`AuthzScopes`] fails to compile *right here* until that
    /// axis is folded into the predicate. That makes the "covered in one place"
    /// promise fail-closed **by construction** — a newly-added axis can never
    /// silently read as global (which would let a principal scoped only on the
    /// new axis reach whole-system artifacts). Do not "simplify" it back to
    /// `self.objects.is_none() && …` field access, which would reopen that hole.
    #[must_use]
    pub fn is_global(&self) -> bool {
        // Exhaustive destructure (no `..`): a new axis field breaks the build here.
        let Self {
            objects,
            outputs,
            discovery_domains,
        } = self;
        objects.is_none() && outputs.is_none() && discovery_domains.is_none()
    }
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

    /// Whether this principal is confined to a discovery-domain allowlist.
    #[must_use]
    pub fn is_discovery_scoped(&self) -> bool {
        self.scoped_discovery_domains.is_some()
    }

    /// Borrow all three authorization axes as the unified view consumed by
    /// [`scope_permits`]. No allowlist is cloned or allocated.
    #[must_use]
    pub fn scopes(&self) -> AuthzScopes<'_> {
        AuthzScopes::new(
            self.scoped_object_ids.as_deref(),
            self.scoped_output_ids.as_deref(),
            self.scoped_discovery_domains.as_deref(),
        )
    }

    /// Whether this principal is unrestricted on **every** authorization axis
    /// (object, output, discovery-domain) — i.e. it can see the whole system.
    ///
    /// The inverse (restricted on at least one axis) gates whole-system
    /// artifacts like the config export, support bundle, and the whole-document
    /// config mutations (revert-to-start / promote): any scoped principal is
    /// denied outright, because such a document embeds every object / output /
    /// domain and cannot be meaningfully redacted per-field. Delegates to the
    /// unified [`AuthzScopes::is_global`] so the axis set lives in one place
    /// (ADR-W026).
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.scopes().is_global()
    }

    /// The full-access principal used when authentication is **disabled** (an
    /// explicit, opt-in deployment mode for trusted/local networks). It carries
    /// [`Role::Admin`] and no object/output/discovery scoping, so every request is
    /// treated as a local administrator. Reachable only when the operator turned
    /// auth off — the default build still requires a verified API key.
    #[must_use]
    pub fn local_admin() -> Self {
        Self {
            key_id: "local-admin".to_owned(),
            role: Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }
}

const PROGRAM_SCOPE_PREFIX: &str = "program:";

fn allowlist_permits(allowlist: Option<&[String]>, id: Option<&str>) -> bool {
    match (allowlist, id) {
        (None, _) => true,
        (Some(allowed), Some(id)) => allowed.iter().any(|entry| entry == id),
        // A scoped principal never receives an unlabelled row: omission cannot
        // widen visibility (ADR-W026's load-bearing fail-closed rule).
        (Some(_), None) => false,
    }
}

fn output_scope_permits(allowlist: Option<&[String]>, output_id: &str) -> bool {
    match allowlist {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|entry| !entry.starts_with(PROGRAM_SCOPE_PREFIX) && entry == output_id),
    }
}

fn program_scope_permits(allowlist: Option<&[String]>, program_id: &str) -> bool {
    match allowlist {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|entry| entry.strip_prefix(PROGRAM_SCOPE_PREFIX) == Some(program_id)),
    }
}

/// Whether `scopes` permits delivery/access to `scope` (ADR-W026).
///
/// This is the one fail-closed, exhaustive rule shared by realtime delivery and
/// REST authorization. The match has no wildcard and [`AuthzScope`] is
/// deliberately not `#[non_exhaustive]`, so adding an authorization axis fails
/// compilation here until its policy is explicit.
#[must_use]
pub fn scope_permits(scopes: &AuthzScopes<'_>, scope: AuthzScope<'_>) -> bool {
    match scope {
        AuthzScope::Public => true,
        AuthzScope::Object(id) => allowlist_permits(scopes.objects, Some(id)),
        AuthzScope::Output(id) => output_scope_permits(scopes.outputs, id),
        AuthzScope::Program(id) => program_scope_permits(scopes.outputs, id),
        AuthzScope::DiscoveryDomain(domain) => allowlist_permits(scopes.discovery_domains, domain),
        AuthzScope::ObjectAndOutput { object, output } => {
            allowlist_permits(scopes.objects, Some(object))
                && output_scope_permits(scopes.outputs, output)
        }
    }
}

/// Enforce the unified authorization scope, producing a 403 twin of
/// [`scope_permits`] for REST handlers.
///
/// # Errors
///
/// [`ControlError::Forbidden`] if any required axis denies `scope`.
pub fn authorize_scope(principal: &Principal, scope: AuthzScope<'_>) -> Result<(), ControlError> {
    if scope_permits(&principal.scopes(), scope) {
        Ok(())
    } else {
        Err(ControlError::Forbidden(format!(
            "principal {:?} is not authorized for {scope:?}",
            principal.key_id
        )))
    }
}

/// Per-object authorization (BOLA defense, ADR-W005 / OWASP API1).
///
/// Thin REST wrapper over the same [`scope_permits`] rule realtime consumes.
/// Checked on **every** resource id a request addresses — not just the role.
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal's object axis denies `object_id`.
pub fn authorize_object(principal: &Principal, object_id: &str) -> Result<(), ControlError> {
    authorize_scope(principal, AuthzScope::Object(object_id))
}

/// Per-**output** authorization (output-scoped role; per-output BOLA defense).
///
/// Thin REST wrapper over the same [`scope_permits`] rule realtime consumes.
/// `program:*` grants are inert here: they authorize [`AuthzScope::Program`], not
/// a plain output with the same id (ADR-W026 namespace separation).
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal's output axis denies `output_id`.
pub fn authorize_output(principal: &Principal, output_id: &str) -> Result<(), ControlError> {
    authorize_scope(principal, AuthzScope::Output(output_id))
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
///
/// The key map is **interior-mutable** (`RwLock`) so a key's authorization can be
/// revoked or re-scoped at runtime (ADR-RT010): [`revoke`](Self::revoke) and
/// [`set_principal`](Self::set_principal) mutate it behind the write lock and bump
/// a wait-free [`generation`](Self::generation) counter, which established realtime
/// sessions sample to re-resolve their authorization mid-session
/// ([`principal_for_key`](Self::principal_for_key)). The store is control-plane
/// only — the engine never touches it, so the lock and counter cannot affect the
/// output path (invariant #10). It is not [`Clone`]; share it via [`std::sync::Arc`].
#[derive(Debug)]
pub struct ApiKeyStore {
    pepper: Vec<u8>,
    keys: RwLock<HashMap<String, KeyRecord>>,
    /// Bumped on every authorization mutation (revoke / re-scope / role change);
    /// a realtime session that observes a higher value re-resolves its principal.
    generation: AtomicU64,
}

impl ApiKeyStore {
    /// Create a store with the given server `pepper` (the HMAC key).
    #[must_use]
    pub fn new(pepper: impl Into<Vec<u8>>) -> Self {
        Self {
            pepper: pepper.into(),
            keys: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Read-lock the key map, recovering the guard if a writer poisoned it. The
    /// writers only insert/remove/reassign whole `KeyRecord` values under the write
    /// lock, so a recovered guard exposes a well-formed (if possibly mid-edit) map —
    /// never a torn value — which is safe to read for verification / re-resolution
    /// and keeps the read path panic-free (safety rule 3, hot-path no-`unwrap`).
    fn read_keys(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, KeyRecord>> {
        self.keys.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write-lock the key map, recovering a poisoned guard for the same reason as
    /// [`read_keys`](Self::read_keys).
    fn write_keys(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, KeyRecord>> {
        self.keys.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Compute the HMAC-SHA256 digest of a secret under this store's pepper.
    #[must_use]
    pub fn digest(&self, secret: &str) -> Vec<u8> {
        // HMAC accepts a key of any length: `KeyInit::new_from_slice` is
        // documented infallible for `Hmac<_>` (it pads/hashes the key as
        // needed), so the `let-else` fallback below is defensive and never
        // taken — but it keeps this method total without `unwrap`/`expect`.
        let Ok(mut mac) = <HmacSha256 as KeyInit>::new_from_slice(&self.pepper) else {
            return Vec::new();
        };
        mac.update(secret.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Register an API key for `principal`, computing and storing its digest.
    ///
    /// Construction-time API: `&mut self` gives exclusive access, so it uses
    /// `RwLock::get_mut` (no lock taken) and does **not** bump the generation —
    /// keys are registered before the store is shared and any session exists. The
    /// runtime authorization mutators are [`revoke`](Self::revoke) /
    /// [`set_principal`](Self::set_principal).
    pub fn register(&mut self, key_id: impl Into<String>, secret: &str, principal: Principal) {
        let key_id = key_id.into();
        let digest = self.digest(secret);
        // Recover a poisoned lock rather than silently dropping the insert (rule 37,
        // no swallowed errors). The writers only insert/remove/reassign whole
        // `KeyRecord` values, so a guard poisoned by an unrelated prior panic still
        // exposes a well-formed map — recovering keeps registration total and
        // panic-free (safety rule 3), consistent with `read_keys`/`write_keys`.
        // `&mut self` means no other guard is live.
        self.keys
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key_id, KeyRecord { digest, principal });
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
        let keys = self.read_keys();
        let record = keys.get(key_id).ok_or(ControlError::Unauthenticated)?;
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

    /// The current authorization **generation**.
    ///
    /// A wait-free `Acquire` load, bumped by every runtime authorization mutation
    /// ([`revoke`](Self::revoke) / [`set_principal`](Self::set_principal)). An
    /// established realtime session captures this at connect and re-resolves its
    /// principal whenever it observes a higher value (ADR-RT010). No lock, never
    /// touches the engine (invariant #10).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Re-resolve the **current** [`Principal`] for a previously-issued `key_id`,
    /// or [`None`] if the key has been revoked.
    ///
    /// This is the realtime re-authorization primitive (ADR-RT010): a session that
    /// authenticated at connect looks up its own `key_id` to pick up a mid-session
    /// role/scope change, or to learn it was revoked. Takes only the read lock
    /// (control-plane only; the engine never holds it).
    #[must_use]
    pub fn principal_for_key(&self, key_id: &str) -> Option<Principal> {
        self.read_keys().get(key_id).map(|r| r.principal.clone())
    }

    /// Revoke an API key: it stops authenticating and every live realtime session
    /// bound to it re-resolves to "revoked" and disconnects (ADR-RT010).
    ///
    /// Bumps the [`generation`](Self::generation) **while holding the write lock**,
    /// so any session that observes the new generation is guaranteed to read the
    /// removed entry. Returns whether a key was actually removed; revoking an
    /// unknown key is a no-op and does **not** bump the generation.
    pub fn revoke(&self, key_id: &str) -> bool {
        let mut keys = self.write_keys();
        let removed = keys.remove(key_id).is_some();
        if removed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        removed
    }

    /// Replace an existing key's authorization (role and/or object/output scope),
    /// keeping its secret digest so the same bearer token keeps authenticating with
    /// the new authorization (ADR-RT010) — models an admin re-scoping or
    /// downgrading a key.
    ///
    /// Bumps the [`generation`](Self::generation) **while holding the write lock**.
    /// Returns whether the key existed; setting an unknown key is a no-op and does
    /// **not** bump the generation (register the key first).
    pub fn set_principal(&self, key_id: &str, principal: Principal) -> bool {
        let mut keys = self.write_keys();
        let Some(record) = keys.get_mut(key_id) else {
            return false;
        };
        record.principal = principal;
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
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

/// The reserved key id of the bootstrap **admin** (environment-provisioned,
/// always unscoped). A config-declared key may never reuse it — see
/// [`register_config_api_keys`], which rejects the collision rather than let a
/// declared key silently overwrite the administrator (ADR-W026).
pub(crate) const BOOTSTRAP_ADMIN_KEY_ID: &str = "admin";

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
        BOOTSTRAP_ADMIN_KEY_ID,
        &secret,
        Principal {
            key_id: BOOTSTRAP_ADMIN_KEY_ID.to_owned(),
            role: Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        },
    );

    (store, bootstrap_token)
}

/// Map a config-declared [`multiview_config::ApiKeyRole`] to the control-plane
/// [`Role`] (ADR-W026). The two enums are kept separate because the dependency
/// runs control→config, never the reverse.
///
/// `ApiKeyRole` carries no `Admin` variant by design — config can never mint an
/// administrator (admin auth is environment-only), so there is no admin arm to
/// map. The match stays exhaustive so a new config role compile-forces its
/// mapping here.
fn role_from_config(role: multiview_config::ApiKeyRole) -> Role {
    match role {
        multiview_config::ApiKeyRole::ReadOnly => Role::ReadOnly,
        multiview_config::ApiKeyRole::Viewer => Role::Viewer,
        multiview_config::ApiKeyRole::Operator => Role::Operator,
    }
}

/// Build the [`Principal`] a config-declared API key authenticates as (ADR-W026):
/// its role plus the three scope allowlists, carried verbatim from config.
#[must_use]
pub fn principal_from_config(key: &multiview_config::ApiKeyConfig) -> Principal {
    Principal {
        key_id: key.key_id.clone(),
        role: role_from_config(key.role),
        scoped_object_ids: key.scoped_object_ids.clone(),
        scoped_output_ids: key.scoped_output_ids.clone(),
        scoped_discovery_domains: key.scoped_discovery_domains.clone(),
    }
}

/// Register every config-declared API key (ADR-W026) into `store`, resolving each
/// key's secret via `resolve_secret` (the CLI passes an environment reader; tests
/// inject a map). This is what makes a scoped principal MINTABLE in production —
/// without it the object/output/discovery axes are a lock with no keyhole.
///
/// A key whose `secret_env` is unset or empty is a **hard startup error**, never
/// a silent skip: an un-authenticatable scoped key is a latent misconfiguration
/// (the operator declared a confined principal that can never be used), so the
/// caller surfaces it and refuses to start.
///
/// # Errors
///
/// Returns the offending key's diagnostic (naming its `secret_env`) when the
/// secret is unset or empty.
pub fn register_config_api_keys<R>(
    store: &mut ApiKeyStore,
    keys: &[multiview_config::ApiKeyConfig],
    mut resolve_secret: R,
) -> Result<(), String>
where
    R: FnMut(&str) -> Option<String>,
{
    for key in keys {
        // Fail-closed on a reserved or already-registered id: a config key must
        // never silently overwrite another key's secret + principal (auth-panel
        // F3 — a `key_id = "admin"` would otherwise clobber the bootstrap
        // administrator, a lockout / takeover). The bootstrap admin is
        // provisioned before this runs, so the already-registered check below
        // also catches it; the explicit reserved check is order-independent
        // defense with a precise diagnostic, and also rejects an in-batch
        // duplicate the moment the first of the pair is registered.
        if key.key_id == BOOTSTRAP_ADMIN_KEY_ID {
            return Err(format!(
                "api.keys[{}]: key_id is reserved for the bootstrap administrator and \
                 cannot be declared in config",
                key.key_id
            ));
        }
        if store.principal_for_key(&key.key_id).is_some() {
            return Err(format!(
                "api.keys[{}]: key_id is already registered; refusing to overwrite it",
                key.key_id
            ));
        }
        let secret = resolve_secret(&key.secret_env)
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| {
                format!(
                    "api.keys[{}]: secret environment variable {:?} is unset or empty",
                    key.key_id, key.secret_env
                )
            })?;
        store.register(&key.key_id, &secret, principal_from_config(key));
    }
    Ok(())
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

    // ---- ADR-W026: config-declared scoped API keys ----

    #[test]
    fn config_key_maps_role_and_all_three_scope_axes() {
        use multiview_config::{ApiKeyConfig, ApiKeyRole};
        let key = ApiKeyConfig::new("site-a-op", "ENV_A", ApiKeyRole::Operator)
            .with_scoped_object_ids(vec!["cam-3".to_owned()])
            .with_scoped_output_ids(vec!["out-1".to_owned(), "program:main".to_owned()])
            .with_scoped_discovery_domains(vec!["site-a".to_owned()]);
        let principal = principal_from_config(&key);
        assert_eq!(principal.key_id, "site-a-op");
        assert_eq!(principal.role, Role::Operator);
        assert_eq!(
            principal.scoped_object_ids.as_deref(),
            Some(&["cam-3".to_owned()][..])
        );
        assert_eq!(
            principal.scoped_output_ids.as_deref(),
            Some(&["out-1".to_owned(), "program:main".to_owned()][..])
        );
        assert_eq!(
            principal.scoped_discovery_domains.as_deref(),
            Some(&["site-a".to_owned()][..])
        );
    }

    #[test]
    fn is_global_is_true_only_when_every_axis_is_unrestricted() {
        // The single source of truth for whole-system gating (ADR-W026): a view
        // is global iff EVERY axis is `None`. Any restricted axis — including an
        // empty `Some([])` allowlist that denies everything on that axis — makes
        // it non-global. These per-axis assertions lock the predicate's behaviour
        // so the compile-forced exhaustive destructure in `AuthzScopes::is_global`
        // is proven behaviour-preserving (the compile-force itself guards the
        // "new axis" case, which no runtime test can exercise).
        let obj = vec!["cam-1".to_owned()];
        let out = vec!["out-1".to_owned()];
        let dom = vec!["site-a".to_owned()];
        let empty: Vec<String> = Vec::new();

        // Unrestricted on all three axes -> global.
        assert!(AuthzScopes::new(None, None, None).is_global());

        // Restricted on exactly ONE axis -> NOT global (each axis independently).
        assert!(!AuthzScopes::new(Some(obj.as_slice()), None, None).is_global());
        assert!(!AuthzScopes::new(None, Some(out.as_slice()), None).is_global());
        assert!(!AuthzScopes::new(None, None, Some(dom.as_slice())).is_global());

        // An empty allowlist is a restriction (denies everything on that axis),
        // not the absence of one — still NOT global, on every axis.
        assert!(!AuthzScopes::new(Some(empty.as_slice()), None, None).is_global());
        assert!(!AuthzScopes::new(None, Some(empty.as_slice()), None).is_global());
        assert!(!AuthzScopes::new(None, None, Some(empty.as_slice())).is_global());
    }

    #[test]
    fn principal_is_global_delegates_to_the_unified_scope_view() {
        // `Principal::is_global` forwards to `AuthzScopes::is_global`; confirm the
        // delegation so whole-system gating (config export / revert / promote)
        // measures a principal against the one shared definition, not a private
        // per-axis copy that could drift.
        let unscoped = Principal {
            key_id: "op".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        };
        assert!(unscoped.is_global());

        // Scoped on a single axis (discovery-domain only) -> not global, so it is
        // denied every whole-system artifact.
        let discovery_only = Principal {
            key_id: "disco".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: Some(vec!["site-a".to_owned()]),
        };
        assert!(!discovery_only.is_global());
    }

    #[test]
    fn register_config_api_keys_registers_and_authenticates() {
        use multiview_config::{ApiKeyConfig, ApiKeyRole};
        let mut store = ApiKeyStore::new(b"pepper".to_vec());
        let keys = vec![ApiKeyConfig::new("k1", "ENV_K1", ApiKeyRole::Viewer)];
        register_config_api_keys(&mut store, &keys, |name| {
            (name == "ENV_K1").then(|| "the-secret".to_owned())
        })
        .expect("registration succeeds when the secret env is present");
        let principal = store
            .verify("k1.the-secret")
            .expect("the registered key authenticates");
        assert_eq!(principal.role, Role::Viewer);
    }

    #[test]
    fn register_config_api_keys_errors_on_missing_secret() {
        use multiview_config::{ApiKeyConfig, ApiKeyRole};
        let mut store = ApiKeyStore::new(b"pepper".to_vec());
        let keys = vec![ApiKeyConfig::new("k1", "ENV_MISSING", ApiKeyRole::Operator)];
        // A scoped key whose secret env is unset is a HARD startup error, never a
        // silent no-op (an un-authenticatable key is a latent misconfiguration).
        let err = register_config_api_keys(&mut store, &keys, |_| None)
            .expect_err("a missing secret env is a startup error");
        assert!(
            err.contains("ENV_MISSING"),
            "the error names the missing env var: {err}"
        );
    }

    #[test]
    fn register_config_api_keys_refuses_to_overwrite_the_bootstrap_admin() {
        use multiview_config::{ApiKeyConfig, ApiKeyRole};
        // The bootstrap admin (key_id "admin") is provisioned first; a config
        // `[[api.keys]]` reusing that reserved id must be a HARD startup error,
        // never a silent HashMap clobber that swaps the admin's secret +
        // principal (lockout / privilege takeover — auth-panel F3).
        let (mut store, _) = provision_admin_keys(Some("admin-secret".to_owned()));
        let keys = vec![ApiKeyConfig::new(
            "admin",
            "ENV_ADMIN",
            ApiKeyRole::Operator,
        )];
        let err = register_config_api_keys(&mut store, &keys, |_| Some("attacker".to_owned()))
            .expect_err("a config key colliding with the reserved admin id must be rejected");
        assert!(
            err.contains("admin"),
            "the error names the reserved/colliding key id: {err}"
        );
        // The bootstrap admin is untouched: its original secret still
        // authenticates as Admin, and the attacker's secret does NOT.
        let principal = store
            .verify("admin.admin-secret")
            .expect("the bootstrap admin secret still authenticates");
        assert_eq!(principal.role, Role::Admin);
        assert!(
            store.verify("admin.attacker").is_err(),
            "the config key's secret must NOT have overwritten the admin"
        );
    }

    #[test]
    fn register_config_api_keys_refuses_a_duplicate_of_an_already_registered_id() {
        use multiview_config::{ApiKeyConfig, ApiKeyRole};
        // Fail-closed on ANY collision with an already-registered id, not just
        // the reserved admin: the first registration wins and a second silent
        // clobber is refused (register_config_api_keys is independently
        // fail-closed, not reliant on the caller having validated).
        let mut store = ApiKeyStore::new(b"pepper".to_vec());
        let keys = vec![
            ApiKeyConfig::new("dup", "ENV_A", ApiKeyRole::Viewer),
            ApiKeyConfig::new("dup", "ENV_B", ApiKeyRole::Operator),
        ];
        let err = register_config_api_keys(&mut store, &keys, |name| match name {
            "ENV_A" => Some("secret-a".to_owned()),
            "ENV_B" => Some("secret-b".to_owned()),
            _ => None,
        })
        .expect_err("a second key reusing an already-registered id must be rejected");
        assert!(
            err.contains("dup"),
            "the error names the colliding id: {err}"
        );
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
