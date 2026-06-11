//! The Conspect **config-lock interceptor** (Hook 2, the S2 backend; ADR-0050
//! §5/§6, the brief §6.2/§13.7).
//!
//! A single additive axum middleware — one place, not per-route copy-paste —
//! that consults the entitlement [`LeaseStore`](multiview_licence::LeaseStore)
//! and, when the ladder is at a `config_locked()` rung, refuses control-plane
//! **configuration** mutations with an RFC-9457 problem naming the ladder reason
//! and linking `/settings/licence`. Reads and **operational continuity**
//! (start/stop/swap, the lease install that *recovers* the lock) pass through
//! untouched.
//!
//! # Never off air (invariant #1 / #10)
//!
//! This guard reads a store and returns a problem document. It holds **no**
//! engine handle, sends on **no** engine channel, and never blocks the engine —
//! a denied reconfiguration is a *convenience* refusal; the running scene keeps
//! playing (ADR-0050 §6.3). It is the courtesy backend twin of the SPA's
//! read-only config-lock interceptor (brief §13.7); the API returning the lock is
//! the authoritative gate, the UI is the courtesy.
//!
//! # What is locked (configuration), what is not (operational continuity)
//!
//! The guard locks the **mutating verbs** (`POST`/`PUT`/`DELETE`/`PATCH`) on the
//! *resource* + *config-apply* surfaces — sources, outputs, overlays, probes,
//! layouts, devices, sync-groups, audio-routing, salvos, tally profiles, and the
//! `config/*` versioning + `commands/apply-layout` surfaces. It deliberately does
//! **not** lock:
//!
//! * any read (`GET`/`HEAD`/`OPTIONS`) — operational visibility is never lost;
//! * the operational commands `commands/start` / `commands/stop` /
//!   `commands/swap` and the operational `routing/*/take`, `salvos/*/take`,
//!   `tally/override`, `sync-groups/*/measure`, device bare-verb actions
//!   (`probe`/`reboot`/`identify`/`test-pattern`/`set-mode`) and alarm ack —
//!   these are **operational continuity**, not reconfiguration;
//! * the **licence** surface (`/api/v1/licence*`) — installing a fresh lease is
//!   how the operator *recovers* from the lock, so it must always be reachable.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::problem::Problem;
use crate::state::AppState;

/// The link the config-lock problem points the operator at to remediate
/// (re-claim / install a fresh lease). A relative SPA route, matching the
/// brief's `/settings/licence` chrome interceptor.
const LICENCE_SETTINGS_LINK: &str = "/settings/licence";

/// Whether `method` is a configuration **mutation** (a write verb). Reads
/// (`GET`/`HEAD`/`OPTIONS`/`TRACE`/`CONNECT`) are never locked.
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// Whether `path` is an **operational continuity** surface that stays reachable
/// under config-lock (start/stop/swap, the recovery licence install, operational
/// takes/actions). These are exempt from the lock even when they use a mutating
/// verb — they are not *reconfiguration*.
///
/// `path` is the request path as the router sees it (under the `/api/v1` nest the
/// middleware is applied to, the prefix is already stripped, so we match both the
/// bare and the prefixed form to be robust to where the layer is attached).
fn is_operational_or_recovery(path: &str) -> bool {
    // Normalise: compare against the suffix after the optional `/api/v1` nest.
    let p = path.strip_prefix("/api/v1").unwrap_or(path);
    // The licence surface is the RECOVERY path — installing a fresh lease unlocks
    // the machine, so it can never be locked.
    if p == "/licence" || p.starts_with("/licence/") {
        return true;
    }
    // Operational commands (program start/stop/swap) are continuity, not config.
    if matches!(p, "/commands/start" | "/commands/stop" | "/commands/swap") {
        return true;
    }
    // Operational takes/actions: a salvo/routing take, a tally override, a
    // sync-group skew measure, an alarm ack, and the device bare-verb actions are
    // operational continuity (they drive the running show), not reconfiguration.
    if p.ends_with("/take")
        || p.ends_with("/arm")
        || p.ends_with("/cancel")
        || p.ends_with("/measure")
        || p.ends_with("/ack")
        || p.ends_with("/probe")
        || p.ends_with("/reboot")
        || p.ends_with("/identify")
        || p.ends_with("/test-pattern")
        || p.ends_with("/set-mode")
        || p == "/tally/override"
        || p.starts_with("/routing/")
    {
        return true;
    }
    false
}

/// The axum middleware: refuse a configuration mutation with a `409
/// config_locked` RFC-9457 problem when the ladder is locked, else pass through.
///
/// Applied once to the `/api/v1` router (see [`crate::router`]). It is wait-free:
/// a single lock-free read of the entitlement store's computed status, no `.await`
/// on the engine, no channel send.
pub async fn config_lock_guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Reads + operational continuity + the licence recovery path are never locked.
    if !is_mutating(request.method()) || is_operational_or_recovery(request.uri().path()) {
        return next.run(request).await;
    }

    // Consult the entitlement plane. The lock fires ONLY on positive evidence of a
    // lapsed lease (`config_locked()` true); an unlicensed (no-lease) or compliant
    // machine is never locked (fail-toward-leniency, ADR-0050 §6.3).
    let locked = state
        .licence
        .store
        .status()
        .is_some_and(|status| status.config_locked);
    if !locked {
        return next.run(request).await;
    }

    // The ladder reason, for the operator-facing detail (the same machine-readable
    // reasons the licence resource renders — there is no second opinion).
    let reasons = state
        .licence
        .store
        .status()
        .map(|s| s.reasons.join(", "))
        .unwrap_or_default();
    let detail = format!(
        "configuration is locked because the entitlement lease has lapsed ({reasons}); \
         the running program is unaffected (it keeps playing) — install a fresh lease at \
         {LICENCE_SETTINGS_LINK} to unlock reconfiguration"
    );
    Problem::new(
        StatusCode::CONFLICT.as_u16(),
        "config_locked",
        "Configuration locked (entitlement lapsed)",
    )
    .with_detail(detail)
    .with_instance(LICENCE_SETTINGS_LINK)
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_write_verbs_are_mutations() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PUT));
        assert!(is_mutating(&Method::DELETE));
        assert!(is_mutating(&Method::PATCH));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
        assert!(!is_mutating(&Method::OPTIONS));
    }

    #[test]
    fn licence_and_operational_paths_are_exempt() {
        // Recovery: the whole licence surface.
        assert!(is_operational_or_recovery("/api/v1/licence/lease"));
        assert!(is_operational_or_recovery("/licence/lease"));
        assert!(is_operational_or_recovery("/api/v1/licence"));
        // Operational continuity.
        assert!(is_operational_or_recovery("/api/v1/commands/start"));
        assert!(is_operational_or_recovery("/api/v1/commands/stop"));
        assert!(is_operational_or_recovery("/api/v1/commands/swap"));
        assert!(is_operational_or_recovery("/api/v1/salvos/s1/take"));
        assert!(is_operational_or_recovery("/api/v1/routing/video/take"));
        assert!(is_operational_or_recovery("/api/v1/tally/override"));
        assert!(is_operational_or_recovery("/api/v1/alarms/a1/ack"));
        assert!(is_operational_or_recovery("/api/v1/devices/d1/reboot"));
    }

    #[test]
    fn configuration_paths_are_locked() {
        // Resource CRUD + config-apply are configuration, not operational.
        assert!(!is_operational_or_recovery("/api/v1/layouts/x"));
        assert!(!is_operational_or_recovery("/api/v1/sources/x"));
        assert!(!is_operational_or_recovery("/api/v1/outputs/x"));
        assert!(!is_operational_or_recovery("/api/v1/overlays/x"));
        assert!(!is_operational_or_recovery("/api/v1/commands/apply-layout"));
        assert!(!is_operational_or_recovery("/api/v1/config/working"));
        assert!(!is_operational_or_recovery("/api/v1/audio-routing"));
    }
}
