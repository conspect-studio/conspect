//! The **LOCAL** support **context-pack composer** (Conspect, ADR-0053 §3 / brief
//! §7.2 / §10, spec §7/§11): the previewable diagnostics bundle an operator
//! deliberately assembles to attach to a support ticket.
//!
//! # What a bundle is
//!
//! A bundle is a **redacted, media-free** snapshot of the machine's own
//! diagnostics over a time `window`, drawn only from the sections the operator
//! asks for (`include[]`). It is composed entirely from **control-plane state**:
//!
//! * `diagnostics` / `metrics` — utilisation percentiles + shed-load + reconnect
//!   history from the consent-independent local retention store (CONSPECT S5,
//!   [`multiview_telemetry::RetentionStore`]); no live engine call.
//! * `incidents` — the incident markers (input flap / encoder saturation / clock
//!   holdover) from the same retention store.
//! * `config` — the working config-as-code resources (sources/outputs/overlays/
//!   probes/devices), **redacted** through [`redact_config`]: every reference-only
//!   secret is removed and every transport URL is masked, and every removal is
//!   listed in the preview so the operator sees exactly what was masked.
//!
//! # Three guarantees this module pins (and a test holds)
//!
//! 1. **Never media (invariant: media-free).** No frame / thumbnail / snapshot /
//!    NV12 / RGBA byte ever enters a bundle — the composer has no path that reads
//!    the preview tap or the framestore, and the preview shape carries no such
//!    field. A bundle is *diagnostics*, never *pictures*.
//! 2. **Secrets out, URLs masked (with a removal list).** The config redactor
//!    drops `secret_ref`/secret-bearing keys entirely and masks URL-bearing values
//!    to an opaque token, recording each as a [`Redaction`] the preview surfaces.
//! 3. **Consent-independent.** Composing a bundle performs **no** telemetry-consent
//!    check (§7.2): consent governs the *daily outbound analytics pipe*, not a
//!    deliberate operator attachment. The retention store this draws from is itself
//!    consent-independent. (Egress of the pack is gated elsewhere — a data
//!    request + a local yes — not here.)
//!
//! # Isolation (invariant #10)
//!
//! The store holds control-plane state behind a short-held `Mutex`; the composer
//! reads other control-plane stores (retention, resources) and never holds an
//! engine handle, spawns onto the data plane, or is `.await`ed by the engine. A
//! wedged client of this surface cannot back-pressure the engine.
use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use multiview_core::time::MediaTime;
use multiview_telemetry::{RetentionStore, RetentionWindow};

use crate::resource_store::ResourceRepository;

/// The default cap on retained composed bundles (oldest evicted past this).
/// Bounded so a long-running deployment cannot grow control-plane memory without
/// bound (invariant #10 / brief §16).
pub const DEFAULT_BUNDLE_CAPACITY: usize = 64;

/// The masking token a redacted secret/URL value is replaced with in a bundle.
/// Opaque + uniform so the operator can see *that* a value was masked without the
/// value itself ever appearing in the pack.
pub const REDACTED_TOKEN: &str = "[redacted]";

// ── The compose request ─────────────────────────────────────────────────────

/// Which diagnostic section a bundle includes. Serialised `lowercase` so the slug
/// is stable across the machine and the (later) portal sync. `#[non_exhaustive]`
/// so a future section is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BundleInclude {
    /// Whole-system diagnostics: utilisation percentiles + shed-load + reconnects.
    Diagnostics,
    /// The metrics summary over the window (utilisation percentiles + counts).
    Metrics,
    /// The working config-as-code, **redacted** (secrets out, URLs masked).
    Config,
    /// The incident markers (input flap / encoder saturation / clock holdover).
    Incidents,
}

/// The time window a bundle reports over (spec §7.2). Serialised as the operator-
/// facing token (`1h` / `24h` / `7d`). `#[non_exhaustive]` so a future window is
/// additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub enum BundleWindow {
    /// The last hour.
    #[serde(rename = "1h")]
    LastHour,
    /// The last 24 hours.
    #[serde(rename = "24h")]
    LastDay,
    /// The last 7 days (the full retained span).
    #[serde(rename = "7d")]
    LastWeek,
}

impl BundleWindow {
    /// The retention-store window this maps to.
    #[must_use]
    pub const fn retention(self) -> RetentionWindow {
        match self {
            BundleWindow::LastHour => RetentionWindow::LastHour,
            BundleWindow::LastDay => RetentionWindow::LastDay,
            BundleWindow::LastWeek => RetentionWindow::LastWeek,
        }
    }

    /// The operator-facing token (for the preview's echoed `window`).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            BundleWindow::LastHour => "1h",
            BundleWindow::LastDay => "24h",
            BundleWindow::LastWeek => "7d",
        }
    }
}

/// The `POST /api/v1/support/bundle` request body: the reporting `window` and the
/// sections to `include`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BundleRequest {
    /// The reporting window (`1h` / `24h` / `7d`).
    pub window: BundleWindow,
    /// The sections to include (deduplicated by the composer).
    pub include: Vec<BundleInclude>,
}

/// The `202` body of a successful compose: the id to read the preview back under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BundleAccepted {
    /// The composed bundle's id (read the preview at `GET .../bundle/{bundle_id}`).
    pub bundle_id: String,
}

// ── The redaction record ────────────────────────────────────────────────────

/// What a single redaction removed/masked, surfaced in the preview so the
/// operator sees exactly what was taken out (never the value itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Redaction {
    /// The dotted path of the redacted field (e.g. `cam-1.auth.secret_ref`). A
    /// location, never the value.
    pub path: String,
    /// Why it was redacted: `secret` (a reference-only secret, removed) or `url`
    /// (a transport URL, masked).
    pub reason: RedactionReason,
}

/// Why a field was redacted from a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RedactionReason {
    /// A reference-only secret (e.g. a `secret_ref`) — removed entirely.
    Secret,
    /// A transport URL — masked to the opaque token.
    Url,
}

// ── The composed bundle preview ─────────────────────────────────────────────

/// One composed support bundle: the echoed request, the redacted/diagnostic
/// sections, and the list of every redaction the config pass made. **Carries no
/// media** by construction — there is no field that could hold a frame/picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Bundle {
    /// The bundle id (the read-back key).
    pub bundle_id: String,
    /// The reporting window token the bundle was composed over.
    pub window: String,
    /// The media-timeline instant (nanoseconds) the bundle was composed.
    pub composed_at_nanos: i64,
    /// The redacted config sections, present iff `config` was requested. A JSON
    /// object keyed by resource collection (`sources`/`outputs`/…); every secret
    /// has been removed and every URL masked.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<serde_json::Value>))]
    pub config: Option<serde_json::Value>,
    /// The utilisation/metrics summary, present iff `metrics` or `diagnostics`
    /// was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Diagnostics>,
    /// The incident markers in the window, present iff `incidents` (or
    /// `diagnostics`) was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incidents: Option<Vec<Incident>>,
    /// Every redaction the config pass made (the masking the operator sees).
    /// Empty when nothing was masked; never omitted (the surface is uniform).
    pub redactions: Vec<Redaction>,
}

/// The utilisation/diagnostics summary a bundle carries over its window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Diagnostics {
    /// Total utilisation samples folded across the window.
    pub samples: u64,
    /// CPU busy-fraction floor (p0) over the window.
    pub cpu_p0: f64,
    /// CPU busy-fraction median (p50) over the window.
    pub cpu_p50: f64,
    /// CPU busy-fraction 95th percentile over the window.
    pub cpu_p95: f64,
    /// CPU busy-fraction ceiling (p100) over the window.
    pub cpu_p100: f64,
    /// The number of shed-load events recorded in the window.
    pub shed_events: usize,
    /// The number of per-input reconnects recorded in the window.
    pub reconnects: usize,
}

/// One incident marker in a bundle (a diagnostic event — never media).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Incident {
    /// The Unix second the incident was recorded.
    pub at_unix_seconds: u64,
    /// The incident class label (`input_flap` / `encoder_saturation` /
    /// `clock_holdover`).
    pub kind: String,
    /// What the incident applied to (input id, `program`, `system`, …).
    pub subject: String,
}

// ── The config redactor ─────────────────────────────────────────────────────

/// Whether a JSON object **key** names a reference-only secret that must be
/// **removed** entirely from a bundle (the value never appears, masked or not).
fn key_is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("secret")
        || k.contains("password")
        || k.contains("passphrase")
        || k.contains("token")
        || k.contains("credential")
        || k.contains("api_key")
        || k.contains("apikey")
        || k == "key"
        || k == "auth"
}

/// Whether a JSON object **key** names a value that should be **masked** because
/// it may carry a transport URL/endpoint (host/credentials in a URI).
fn key_is_url(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "url" || k.ends_with("_url") || k == "uri" || k.ends_with("_uri") || k == "endpoint"
}

/// Whether a string **value** looks like a URL with a scheme (so a value under a
/// non-obvious key is masked too — bad-inputs-are-the-purpose).
fn value_looks_like_url(value: &str) -> bool {
    // A scheme like `rtsp://`, `srt://`, `https://`, `op://`, `udp://`, … —
    // anything `scheme://…` is treated as a maskable endpoint.
    if let Some(pos) = value.find("://") {
        let scheme = &value[..pos];
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    } else {
        false
    }
}

/// Redact a config JSON document for inclusion in a bundle: every secret-bearing
/// key is **removed**, every URL-bearing value is **masked** to [`REDACTED_TOKEN`],
/// and every change is recorded as a [`Redaction`] (a path + a reason, never the
/// value). Returns the redacted document; pushes redactions onto `out`.
///
/// The walk is path-aware (so the preview lists *where* a value was masked) and
/// total over arbitrary nesting (objects + arrays), so a deeply-nested or
/// adversarial config is still fully scrubbed.
#[must_use]
pub fn redact_config(value: &serde_json::Value, out: &mut Vec<Redaction>) -> serde_json::Value {
    redact_at("", value, out)
}

/// The path-tracking recursion behind [`redact_config`].
fn redact_at(path: &str, value: &serde_json::Value, out: &mut Vec<Redaction>) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut redacted = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if key_is_secret(key) {
                    // Drop the whole subtree — the value (or any nested secret)
                    // never appears in the bundle, masked or otherwise.
                    out.push(Redaction {
                        path: child_path,
                        reason: RedactionReason::Secret,
                    });
                    continue;
                }
                if key_is_url(key) || child.as_str().is_some_and(value_looks_like_url) {
                    out.push(Redaction {
                        path: child_path,
                        reason: RedactionReason::Url,
                    });
                    redacted.insert(
                        key.clone(),
                        serde_json::Value::String(REDACTED_TOKEN.to_owned()),
                    );
                    continue;
                }
                redacted.insert(key.clone(), redact_at(&child_path, child, out));
            }
            serde_json::Value::Object(redacted)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let item_path = format!("{path}[{i}]");
                    // A bare URL string in an array is masked too.
                    if item.as_str().is_some_and(value_looks_like_url) {
                        out.push(Redaction {
                            path: item_path,
                            reason: RedactionReason::Url,
                        });
                        serde_json::Value::String(REDACTED_TOKEN.to_owned())
                    } else {
                        redact_at(&item_path, item, out)
                    }
                })
                .collect(),
        ),
        // A scalar at the top level (or under a non-url key) is kept verbatim.
        other => other.clone(),
    }
}

// ── The config-export redactor ───────────────────────────────────────────────

/// The sentinel a secret value is replaced with in an **exported** config-as-code
/// document (`GET /api/v1/config/export`).
///
/// Distinct from the support-bundle [`REDACTED_TOKEN`] by design. A support bundle
/// *drops* secret-bearing keys (the diagnostic pack never needs them); an exported
/// config must stay a **structurally valid** [`multiview_config::MultiviewConfig`]
/// so it round-trips — a TURN server still needs a non-empty credential field — so
/// the export *replaces the value in place* with this clearly-marked placeholder
/// instead of removing the key. The operator restores the real secret (or re-points
/// a `secret_ref`) before the exported file is used to run; the placeholder never
/// silently authenticates and never carries a real secret.
pub const EXPORT_REDACTED_SENTINEL: &str = "<redacted>";

/// Whether an object **key** names a value that, for config-as-code export, is a
/// **reference-only** secret pointer rather than an inline cleartext secret — a
/// `secret_ref` (e.g. `op://Servers/cam/credentials`, [`multiview_config`]'s
/// documented "never plaintext" indirection). The reference is *not* a secret and
/// is exactly what a reimport needs, so the export **keeps** it: redacting it would
/// break config-as-code (and would not improve security — the pointer is not the
/// secret it resolves to).
fn key_is_secret_reference(key: &str) -> bool {
    key.eq_ignore_ascii_case("secret_ref")
}

/// Redact every **inline cleartext** secret in a config-as-code document **for
/// export**, replacing the scalar value in place with [`EXPORT_REDACTED_SENTINEL`]
/// so the document stays structurally valid (and re-importable) while no plaintext
/// secret ever leaves the process.
///
/// A redacted field is one whose object key [`key_is_secret`] classifies (the same
/// policy the support-bundle redactor uses — `password`, `static_auth_secret`,
/// `token`, `secret`, `auth`, `api_key`/`apikey`, `credential`, `passphrase`, bare
/// `key`) **and** whose value is a scalar (string/number/bool) — i.e. an inline
/// cleartext secret. Unlike the support-bundle pass this:
///
/// * **keeps the key + the value's place** (replacing only the scalar), so a TURN
///   server still has a non-empty `password`/`static_auth_secret` and passes
///   validation, round-tripping as a clearly-marked placeholder;
/// * **recurses into structured holders** — a secret-named *object*/*array* (e.g. a
///   source's `auth = { secret_ref = "op://…" }`) keeps its shape so the document
///   re-imports, and its scalar leaves are still scrubbed by the same rule; and
/// * **preserves a `secret_ref` pointer** ([`key_is_secret_reference`]) — the
///   `op://…` reference is the intended config-as-code secret mechanism, not a
///   leak; and
/// * **does not mask transport URLs** — a config-as-code document needs its
///   `url`/`endpoint` hosts to be reimportable (a stream key embedded in an `rtmp`/
///   `srt` URL is a documented residue of the inline-secret posture, masked only in
///   the diagnostic support bundle, never here).
///
/// The walk is total over arbitrary nesting (objects + arrays), so a deeply-nested
/// or adversarial document is fully scrubbed. Returns the redacted document.
#[must_use]
pub fn redact_config_for_export(value: &serde_json::Value) -> serde_json::Value {
    // The export signature borrows the stored document and must return an owned
    // copy, so exactly one clone is inherent here; redaction is then the single
    // shared in-place pass — no second full-document rebuild.
    let mut owned = value.clone();
    redact_inline_secrets_in_place(&mut owned);
    owned
}

/// The shared in-place core behind [`redact_config_for_export`] and
/// [`redact_inline_secrets_for_read`]: walk `value` and replace every **inline
/// cleartext** secret scalar with [`EXPORT_REDACTED_SENTINEL`], mutating the tree
/// in place so a caller that already owns a response copy pays no second
/// allocation of the whole document.
///
/// An inline secret is a scalar (string/number/bool) under a key [`key_is_secret`]
/// classifies that is **not** a [`key_is_secret_reference`] pointer. A secret-named
/// *object*/*array*/`null` is not itself the secret, so the walk recurses to keep
/// its shape and still scrub any nested inline secret; transport URLs and
/// `secret_ref` pointers are preserved (config-as-code needs them). The walk is
/// total over arbitrary nesting, so a deeply-nested or adversarial document is
/// fully scrubbed.
fn redact_inline_secrets_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let is_inline_secret = key_is_secret(key)
                    && !key_is_secret_reference(key)
                    && !matches!(
                        child,
                        serde_json::Value::Object(_)
                            | serde_json::Value::Array(_)
                            | serde_json::Value::Null
                    );
                if is_inline_secret {
                    // An inline cleartext secret (a scalar under a secret-named
                    // key): replace the value in place, keep the key.
                    *child = serde_json::Value::String(EXPORT_REDACTED_SENTINEL.to_owned());
                } else {
                    // A non-secret key, a `secret_ref` pointer, or a structured
                    // holder under a secret-named key: recurse so the shape survives
                    // and any nested inline secret is still scrubbed.
                    redact_inline_secrets_in_place(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_inline_secrets_in_place(item);
            }
        }
        // A scalar under a non-secret key (or at the top level) is kept verbatim.
        _ => {}
    }
}

/// Mask inline cleartext secrets in a REST response view for a non-admin
/// principal, using the same structurally-preserving policy as config export.
///
/// The caller passes an owned response copy; this mutates that view **in place**
/// (no second document clone) and never the stored document. An
/// [`crate::auth::Role::Admin`] principal keeps the original value for operational
/// access. Every less-privileged role receives [`EXPORT_REDACTED_SENTINEL`] in each
/// inline-secret field.
pub(crate) fn redact_inline_secrets_for_read(
    principal: &crate::auth::Principal,
    value: &mut serde_json::Value,
) {
    if principal.role != crate::auth::Role::Admin {
        redact_inline_secrets_in_place(value);
    }
}

// ── The bundle store ────────────────────────────────────────────────────────

/// Mint a fresh bundle id (an uppercased short hex of a v4 UUID, `SB-` prefixed
/// for "support bundle").
#[must_use]
fn mint_bundle_id() -> String {
    let id = Uuid::new_v4().simple().to_string();
    let short: String = id.chars().take(12).collect();
    format!("SB-{}", short.to_uppercase())
}

/// Append + read access to the composed-bundle store. Bundles are immutable once
/// composed; there is no edit/delete — only `compose` (which the route drives via
/// [`compose_bundle`]) appends, and `get` reads.
pub trait BundleStore: Send + Sync + 'static {
    /// Store a composed bundle, evicting the oldest past capacity.
    fn put(&self, bundle: Bundle);

    /// Fetch one composed bundle by id, or `None` if unknown/evicted.
    fn get(&self, bundle_id: &str) -> Option<Bundle>;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use BundleStore as BundleRepository;

/// An in-memory, bounded composed-bundle store.
#[derive(Debug)]
pub struct InMemoryBundles {
    capacity: usize,
    bundles: Mutex<VecDeque<Bundle>>,
}

impl Default for InMemoryBundles {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_BUNDLE_CAPACITY)
    }
}

impl InMemoryBundles {
    /// A fresh, empty store at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty store retaining at most `capacity` newest bundles.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            bundles: Mutex::new(VecDeque::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Bundle>> {
        self.bundles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl BundleStore for InMemoryBundles {
    fn put(&self, bundle: Bundle) {
        let mut guard = self.lock();
        guard.push_back(bundle);
        while guard.len() > self.capacity {
            guard.pop_front();
        }
    }

    fn get(&self, bundle_id: &str) -> Option<Bundle> {
        self.lock()
            .iter()
            .find(|b| b.bundle_id == bundle_id)
            .cloned()
    }
}

/// The control-plane sources the composer draws config from (one
/// [`ResourceRepository`] per collection). Read-mostly, off the engine hot loop.
pub struct ConfigSources<'a> {
    /// The `sources` store.
    pub sources: &'a dyn ResourceRepository,
    /// The `outputs` store.
    pub outputs: &'a dyn ResourceRepository,
    /// The `overlays` store.
    pub overlays: &'a dyn ResourceRepository,
    /// The `probes` store.
    pub probes: &'a dyn ResourceRepository,
    /// The `devices` store.
    pub devices: &'a dyn ResourceRepository,
}

/// Compose a bundle from control-plane state: the retention store for
/// diagnostics/metrics/incidents, the config stores (redacted) for config. Pure
/// assembly — no engine call, no consent check (§7.2). `now_unix_seconds` is the
/// wall second the caller sampled once; `at` is the media-timeline compose
/// instant; `mint` lets a test inject a deterministic id.
#[must_use]
pub fn compose_bundle(
    request: &BundleRequest,
    retention: &RetentionStore,
    config: &ConfigSources<'_>,
    now_unix_seconds: u64,
    at: MediaTime,
    mint: impl FnOnce() -> String,
) -> Bundle {
    let window = request.window;
    let want = |k: BundleInclude| request.include.contains(&k);
    let want_diag = want(BundleInclude::Diagnostics);

    let mut redactions = Vec::new();

    let config_section = if want(BundleInclude::Config) {
        Some(redacted_config(config, &mut redactions))
    } else {
        None
    };

    let diagnostics = if want_diag || want(BundleInclude::Metrics) {
        let rw = window.retention();
        let summary = retention.utilisation_summary(now_unix_seconds, rw);
        let sheds = retention.shed_window(now_unix_seconds, rw).len();
        let reconnects = retention.reconnect_window(now_unix_seconds, rw).len();
        Some(Diagnostics {
            samples: summary.map_or(0, |s| s.samples),
            cpu_p0: summary.map_or(0.0, |s| s.cpu_p0),
            cpu_p50: summary.map_or(0.0, |s| s.cpu_p50),
            cpu_p95: summary.map_or(0.0, |s| s.cpu_p95),
            cpu_p100: summary.map_or(0.0, |s| s.cpu_p100),
            shed_events: sheds,
            reconnects,
        })
    } else {
        None
    };

    let incidents = if want(BundleInclude::Incidents) || want_diag {
        let markers = retention.incident_window(now_unix_seconds, window.retention());
        Some(
            markers
                .into_iter()
                .map(|m| Incident {
                    at_unix_seconds: m.at_unix_seconds,
                    kind: m.kind.label().to_owned(),
                    subject: m.subject,
                })
                .collect(),
        )
    } else {
        None
    };

    Bundle {
        bundle_id: mint(),
        window: window.token().to_owned(),
        composed_at_nanos: at.as_nanos(),
        config: config_section,
        diagnostics,
        incidents,
        redactions,
    }
}

/// Build the redacted `config` section: each collection's resource bodies run
/// through [`redact_config`], keyed by `id` under the collection name.
fn redacted_config(config: &ConfigSources<'_>, out: &mut Vec<Redaction>) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    for (name, store) in [
        ("sources", config.sources),
        ("outputs", config.outputs),
        ("overlays", config.overlays),
        ("probes", config.probes),
        ("devices", config.devices),
    ] {
        let Ok(items) = store.list() else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        let mut collection = serde_json::Map::new();
        for versioned in items {
            let id = versioned.resource.id.clone();
            let path = format!("{name}.{id}");
            let redacted = redact_at(&path, &versioned.resource.body, out);
            collection.insert(id, redacted);
        }
        root.insert(name.to_owned(), serde_json::Value::Object(collection));
    }
    serde_json::Value::Object(root)
}

/// A bundle id minted with the production scheme (exposed so the route's compose
/// path uses the same generator the store + tests expect).
#[must_use]
pub fn new_bundle_id() -> String {
    mint_bundle_id()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )]

    use super::{
        compose_bundle, key_is_secret, key_is_url, redact_config, redact_config_for_export,
        value_looks_like_url, Bundle, BundleInclude, BundleRequest, BundleStore, BundleWindow,
        ConfigSources, InMemoryBundles, RedactionReason, EXPORT_REDACTED_SENTINEL, REDACTED_TOKEN,
    };
    use crate::resource_store::{
        InMemoryDeviceStore, InMemoryOutputStore, InMemoryOverlayStore, InMemoryProbeStore,
        InMemorySourceStore, ResourceInput, ResourceRepository,
    };
    use multiview_core::time::MediaTime;
    use multiview_telemetry::RetentionStore;

    fn config_sources<'a>(
        sources: &'a dyn ResourceRepository,
        outputs: &'a dyn ResourceRepository,
        overlays: &'a dyn ResourceRepository,
        probes: &'a dyn ResourceRepository,
        devices: &'a dyn ResourceRepository,
    ) -> ConfigSources<'a> {
        ConfigSources {
            sources,
            outputs,
            overlays,
            probes,
            devices,
        }
    }

    #[test]
    fn redactor_removes_secrets_and_masks_urls_with_a_path_list() {
        let body = serde_json::json!({
            "id": "cam-1",
            "url": "rtsp://camera.example/stream",
            "auth": { "secret_ref": "op://Servers/cam/credentials" },
            "nested": { "endpoint": "srt://host:9000", "label": "ok" }
        });
        let mut redactions = Vec::new();
        let out = redact_config(&body, &mut redactions);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(!serialized.contains("rtsp://camera.example/stream"));
        assert!(!serialized.contains("op://Servers/cam/credentials"));
        assert!(!serialized.contains("srt://host:9000"));
        // The plain label survives.
        assert!(serialized.contains("ok"));
        // The masked url is the opaque token.
        assert_eq!(out["url"], serde_json::json!(REDACTED_TOKEN));
        // The whole `auth` subtree (a secret-bearing key) is gone.
        assert!(out.get("auth").is_none());
        // Every removal is listed, and none of the redaction PATHS leak a value.
        assert!(!redactions.is_empty());
        assert!(redactions
            .iter()
            .any(|r| r.reason == RedactionReason::Secret));
        assert!(redactions.iter().any(|r| r.reason == RedactionReason::Url));
        for r in &redactions {
            assert!(!r.path.contains("rtsp://"));
            assert!(!r.path.contains("op://"));
        }
    }

    #[test]
    fn key_and_value_classifiers_are_exact() {
        assert!(key_is_secret("secret_ref"));
        assert!(key_is_secret("auth"));
        assert!(key_is_secret("API_KEY"));
        assert!(!key_is_secret("display_name"));
        assert!(key_is_url("url"));
        assert!(key_is_url("ingest_url"));
        assert!(key_is_url("endpoint"));
        assert!(!key_is_url("title"));
        assert!(value_looks_like_url("rtsp://x/y"));
        assert!(value_looks_like_url("op://vault/item"));
        assert!(!value_looks_like_url("not a url"));
        assert!(!value_looks_like_url("a/b/c"));
    }

    #[test]
    fn a_composed_bundle_carries_no_media_token_under_any_include() {
        let sources = InMemorySourceStore::new();
        let outputs = InMemoryOutputStore::new();
        let overlays = InMemoryOverlayStore::new();
        let probes = InMemoryProbeStore::new();
        let devices = InMemoryDeviceStore::new();
        let retention = RetentionStore::new();
        let cfg = config_sources(&sources, &outputs, &overlays, &probes, &devices);
        let request = BundleRequest {
            window: BundleWindow::LastWeek,
            include: vec![
                BundleInclude::Diagnostics,
                BundleInclude::Metrics,
                BundleInclude::Config,
                BundleInclude::Incidents,
            ],
        };
        let bundle = compose_bundle(
            &request,
            &retention,
            &cfg,
            1_000_000,
            MediaTime::from_nanos(0),
            || "SB-TEST".to_owned(),
        );
        let serialized = serde_json::to_string(&bundle).unwrap().to_lowercase();
        for forbidden in [
            "frame",
            "thumbnail",
            "jpeg",
            "snapshot",
            "nv12",
            "rgba",
            "media",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "no media token may appear; found {forbidden:?}"
            );
        }
        assert_eq!(bundle.bundle_id, "SB-TEST");
        assert_eq!(bundle.window, "7d");
    }

    #[test]
    fn config_section_redacts_a_seeded_source() {
        let sources = InMemorySourceStore::new();
        sources
            .create(
                "cam-1",
                ResourceInput {
                    name: "Camera 1".to_owned(),
                    body: serde_json::json!({
                        "id": "cam-1",
                        "url": "rtsp://camera.example/stream",
                        "auth": { "secret_ref": "op://Servers/cam/credentials" }
                    }),
                },
            )
            .unwrap();
        let outputs = InMemoryOutputStore::new();
        let overlays = InMemoryOverlayStore::new();
        let probes = InMemoryProbeStore::new();
        let devices = InMemoryDeviceStore::new();
        let retention = RetentionStore::new();
        let cfg = config_sources(&sources, &outputs, &overlays, &probes, &devices);
        let request = BundleRequest {
            window: BundleWindow::LastDay,
            include: vec![BundleInclude::Config],
        };
        let bundle = compose_bundle(
            &request,
            &retention,
            &cfg,
            1_000_000,
            MediaTime::from_nanos(0),
            || "SB-CFG".to_owned(),
        );
        let serialized = serde_json::to_string(&bundle).unwrap();
        assert!(!serialized.contains("rtsp://camera.example/stream"));
        assert!(!serialized.contains("op://Servers/cam/credentials"));
        assert!(!bundle.redactions.is_empty(), "the masking is surfaced");
    }

    #[test]
    fn export_redactor_masks_inline_secrets_keeps_structure_and_refs() {
        let doc = serde_json::json!({
            "webrtc": {
                "ice_servers": [
                    { "kind": "turn", "url": "turn:[2001:db8::55]:3478",
                      "username": "pub", "password": "PLAINTEXT-TURN-PW" },
                    { "kind": "turn", "url": "turns:[2001:db8::56]:5349",
                      "username": "eph", "static_auth_secret": "PLAINTEXT-REST-SECRET" }
                ]
            },
            "sources": [
                { "id": "whip-cam", "kind": "webrtc", "token": "PLAINTEXT-BEARER" },
                { "id": "cam-2", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam2",
                  "auth": { "secret_ref": "op://Servers/cam/credentials" } }
            ]
        });
        let out = redact_config_for_export(&doc);
        let serialized = serde_json::to_string(&out).unwrap();

        // No inline cleartext secret survives.
        assert!(!serialized.contains("PLAINTEXT-TURN-PW"));
        assert!(!serialized.contains("PLAINTEXT-REST-SECRET"));
        assert!(!serialized.contains("PLAINTEXT-BEARER"));
        // The sentinel marks each redaction in place.
        assert_eq!(
            out["webrtc"]["ice_servers"][0]["password"],
            serde_json::json!(EXPORT_REDACTED_SENTINEL)
        );
        assert_eq!(
            out["webrtc"]["ice_servers"][1]["static_auth_secret"],
            serde_json::json!(EXPORT_REDACTED_SENTINEL)
        );
        assert_eq!(
            out["sources"][0]["token"],
            serde_json::json!(EXPORT_REDACTED_SENTINEL)
        );
        // Structure is preserved: the TURN server keeps a non-empty url/username.
        assert_eq!(
            out["webrtc"]["ice_servers"][0]["url"],
            serde_json::json!("turn:[2001:db8::55]:3478")
        );
        // A transport URL is NOT masked (config-as-code needs the host).
        assert_eq!(
            out["sources"][1]["url"],
            serde_json::json!("rtsp://[2001:db8::1]/cam2")
        );
        // A `secret_ref` pointer is preserved (a reference, not a leak), and its
        // holding `auth` object keeps its shape so the config re-imports.
        assert_eq!(
            out["sources"][1]["auth"]["secret_ref"],
            serde_json::json!("op://Servers/cam/credentials")
        );
    }

    #[test]
    fn export_redactor_scrubs_nested_inline_secrets_under_a_structured_holder() {
        // A secret-named *object* (`auth`) carrying an inline cleartext `password`
        // keeps its shape but loses the plaintext — no nesting hides a leak.
        let doc = serde_json::json!({
            "auth": { "username": "u", "password": "DEEP-PLAINTEXT" },
            "list": [ { "token": "ALSO-PLAINTEXT" } ]
        });
        let out = redact_config_for_export(&doc);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(!serialized.contains("DEEP-PLAINTEXT"));
        assert!(!serialized.contains("ALSO-PLAINTEXT"));
        assert_eq!(out["auth"]["username"], serde_json::json!("u"));
        assert_eq!(
            out["auth"]["password"],
            serde_json::json!(EXPORT_REDACTED_SENTINEL)
        );
        assert_eq!(
            out["list"][0]["token"],
            serde_json::json!(EXPORT_REDACTED_SENTINEL)
        );
    }

    #[test]
    fn bundle_store_is_bounded_and_round_trips() {
        let store = InMemoryBundles::with_capacity(2);
        for i in 0..3 {
            store.put(Bundle {
                bundle_id: format!("SB-{i}"),
                window: "1h".to_owned(),
                composed_at_nanos: 0,
                config: None,
                diagnostics: None,
                incidents: None,
                redactions: Vec::new(),
            });
        }
        assert!(store.get("SB-0").is_none(), "oldest evicted past capacity");
        assert!(store.get("SB-2").is_some(), "newest retained");
    }
}
