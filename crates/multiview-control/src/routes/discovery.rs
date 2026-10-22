//! The mDNS device-**discovery** surface under `/api/v1/discovery/devices`
//! (DEV-A5, ADR-M008 §6, ADR-0041 untrusted-inventory doctrine).
//!
//! Discovery produces an **untrusted inventory** of services found on the LAN —
//! Cast (`_googlecast._tcp`), NDI sources incl. `ZowieBox` (`_ndi._tcp`), and an
//! operator-configured (unverified) zowietek-control type. The inventory is a
//! list of **hints requiring explicit confirm-adopt**: discovery NEVER creates a
//! device. An operator confirms adoption by `POST`ing to the existing
//! [`create_device`](super::devices::create_device) (`POST /devices/{id}`)
//! referencing a discovered address — this module only *informs* that choice.
//!
//! * `POST /api/v1/discovery/devices/scan` — kick a time-bounded browse
//!   (role: write; `202` + operation id). The browse runs on a bounded
//!   control-plane task that populates the untrusted inventory and publishes
//!   `device.discovered` events on [`Topic::Devices`](multiview_events::Topic::Devices)
//!   via the conflating broadcaster (drop-oldest — invariant #10), each
//!   correlated to the scan's operation id via the envelope `corr`
//!   (ADR-RT007). Scans are **single-flight**: a concurrent request attaches
//!   to the running scan (same operation id) instead of starting a second,
//!   mutually-destructive browse; a replayed `Idempotency-Key` answers with
//!   the original operation id **without** re-executing the browse.
//! * `GET /api/v1/discovery/devices` — the current untrusted inventory snapshot
//!   (role: read; AAAA-first, IPv4 labelled legacy, stale rows purged on read).
//!
//! ## Isolation (invariant #10)
//!
//! The scan task touches only control-plane state: it browses (the browser owns
//! its own socket/thread), writes the **bounded drop-oldest** inventory, and
//! publishes events through the engine's **non-blocking** broadcast. It never
//! awaits a client and never sends on a channel a slow consumer can fill, so it
//! cannot back-pressure the engine — the same proof shape as the other
//! control-plane producers.
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::command::OperationId;
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::devices::broadcaster::DeviceBroadcaster;
use crate::devices::discovery::{
    scan_service_types, DiscoveredService, DiscoveryBrowser, DiscoveryInventory, ScanAdmission,
    DEFAULT_ENTRY_TTL, DEFAULT_SCAN_BUDGET,
};
use crate::error::ControlResult;
use crate::realtime::CorrKey;
use crate::state::AppState;

/// The `202 Accepted` body for a scan: the operation id correlating the scan and
/// the human-readable scope of what is browsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ScanAccepted {
    /// The operation id of the scan that actually runs — freshly started,
    /// attached-to (single-flight), or replayed (`Idempotency-Key`). The
    /// `device.discovered` rows it produces stream on the realtime `devices`
    /// topic while it runs, each echoing this id as the envelope `corr`.
    pub operation_id: String,
    /// The service types being browsed (Cast + NDI, plus any configured
    /// zowietek-control type).
    pub service_types: Vec<String>,
    /// The scan time budget in milliseconds (the browse is always time-bounded).
    pub budget_ms: u64,
    /// A reminder that discovery is **untrusted**: rows require explicit
    /// confirm-adopt and discovery never creates a device (ADR-0041).
    pub note: String,
}

/// `POST /api/v1/discovery/devices/scan` — kick a time-bounded mDNS browse
/// (role: write; `202` + operation id).
///
/// The browse runs on a bounded control-plane task: it asks the injected browser
/// for services within a time budget, classifies each into the **untrusted
/// inventory** (AAAA-first, TTL-stamped), and publishes a `device.discovered`
/// event per service, correlated to this operation id via the envelope `corr`
/// (ADR-RT007). It never creates a device (ADR-0041).
///
/// Scans are **single-flight** (one in-flight browse — concurrent `mdns-sd`
/// browses corrupt each other's listeners/queriers, and one browse at a time is
/// the ADR-M008 rate limit): a request that arrives while a scan runs
/// **attaches** to it and is answered with the *running* scan's operation id.
/// A retried `Idempotency-Key` returns the original operation id without
/// re-executing the browse (the canonical replay semantics).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/discovery/devices/scan",
        tag = "discovery",
        responses(
            (status = 202, description = "Scan accepted (or attached to the single-flight running scan, or replayed by Idempotency-Key — the operation id names the scan that actually runs); discovered rows stream as device.discovered events correlated via corr and land in the untrusted inventory.", body = ScanAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to scan.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn scan_devices(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;

    let zowietek = state.discovery_config.zowietek_service_type.clone();
    let domain = state.discovery_config.domain.clone();
    // Discovery-domain gate (ADR-W026): a principal that could not SEE this
    // node's discovery inventory (its domain is outside the principal's
    // allowlist, or the node is unlabelled and the principal is discovery-scoped)
    // may not spend the single-flight scan budget or correlate scan activity via
    // the 202 window. The REST twin of the `device.discovered` stream filter.
    crate::auth::authorize_scope(
        &principal,
        multiview_events::AuthzScope::DiscoveryDomain(domain.as_deref()),
    )?;
    let service_types = scan_service_types(
        zowietek.as_deref(),
        &state.discovery_config.extra_service_types,
    );
    let budget = DEFAULT_SCAN_BUDGET;

    let fresh = match state.idempotency.reserve(idem.0.as_deref()) {
        // A retried request with the same key: answer with the original
        // operation id WITHOUT re-executing the browse (the canonical
        // routes/mod.rs replay semantics — the original scan already ran or is
        // running).
        Reservation::Replay(op) => return Ok(scan_accepted(&op, service_types, budget)),
        Reservation::Fresh(op) => op,
    };

    match state.discovery_scan_gate.begin(fresh.clone()) {
        ScanAdmission::Attached(running) => {
            // Single-flight: a scan is already running. Attach — answer with
            // the RUNNING scan's operation id (its device.discovered rows are
            // the rows this caller will see) and never start a second,
            // mutually-destructive browse. The fresh reservation is re-pointed
            // at the running op so a replay of this key also answers with the
            // operation that actually executed.
            state
                .idempotency
                .rebind(idem.0.as_deref(), &fresh, running.clone());
            Ok(scan_accepted(&running, service_types, budget))
        }
        ScanAdmission::Started(guard) => {
            // Window-correlate every device.discovered row this scan publishes
            // (engine seq after the fence) to this operation id (ADR-RT007).
            // Recorded BEFORE the scan task spawns, so no row can be published
            // ahead of its correlation.
            let from_seq = state.engine.events.sequence();
            state
                .corr
                .record_window(CorrKey::Discovery, fresh.clone(), from_seq);

            // The browse runs off the request path on a bounded control-plane
            // task. It publishes via the engine's non-blocking drop-oldest
            // broadcast and writes the bounded inventory — it never awaits a
            // client (invariant #10).
            let inventory = Arc::clone(&state.discovery);
            let browser = Arc::clone(&state.discovery_browser);
            let broadcaster =
                DeviceBroadcaster::new(Arc::clone(&state.engine), Arc::clone(&state.device_status));
            let scan_types = service_types.clone();
            tokio::spawn(async move {
                run_scan(
                    inventory,
                    browser,
                    broadcaster,
                    scan_types,
                    budget,
                    zowietek,
                    domain,
                )
                .await;
                // The guard clears the single-flight slot when this task ends
                // (drop runs even if the task is cancelled), so the next POST
                // can start a fresh browse.
                drop(guard);
            });

            Ok(scan_accepted(&fresh, service_types, budget))
        }
    }
}

/// Build the `202 Accepted` scan response for `op` (the operation id of the
/// scan that actually runs — fresh, attached, or replayed).
fn scan_accepted(op: &OperationId, service_types: Vec<String>, budget: Duration) -> Response {
    let body = ScanAccepted {
        operation_id: op.to_string(),
        service_types,
        budget_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
        note: "discovered services are an untrusted inventory; confirm-adopt via \
               POST /devices/{id} — discovery never creates a device"
            .to_owned(),
    };
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// Run one bounded browse: classify each found service into the untrusted
/// inventory and publish a `device.discovered` event for it.
///
/// `configured_zowietek` is the operator-configured zowietek-control service
/// type (the `[discovery]` config section) — the only way a service is ever
/// classified `zowietek-control`.
///
/// `domain` is the observing node's operator-declared discovery domain
/// (ADR-W026). This one local-config value stamps BOTH the realtime
/// `device.discovered` event and its REST inventory row; it is never read from
/// responder-controlled `raw`, so the two surfaces cannot disagree and a
/// discovered device cannot assert its own authorization scope.
///
/// The browser's `browse` is potentially blocking (the `mdns-sd` daemon delivers
/// over channels the interleaved drain consumes for the budget), so it runs on a
/// blocking thread — this keeps the async runtime free and, crucially, off the
/// engine path. Every write here is to bounded control-plane state or the
/// non-blocking broadcast (invariant #10).
async fn run_scan(
    inventory: Arc<DiscoveryInventory>,
    browser: Arc<dyn DiscoveryBrowser>,
    broadcaster: DeviceBroadcaster,
    service_types: Vec<String>,
    budget: Duration,
    configured_zowietek: Option<String>,
    domain: Option<String>,
) {
    let found = tokio::task::spawn_blocking(move || browser.browse(&service_types, budget))
        .await
        .unwrap_or_default();
    let expires_at = Instant::now() + DEFAULT_ENTRY_TTL;
    for raw in &found {
        let service = DiscoveredService::from_raw(
            raw,
            configured_zowietek.as_deref(),
            expires_at,
            domain.clone(),
        );
        // Publish the untrusted row first (so a realtime client sees it), then
        // record it in the inventory the GET reads. The event and the row are
        // stamped with the SAME `domain` value (once from local config), so the
        // realtime and REST surfaces can never disagree on scope.
        let primary = service.primary();
        broadcaster.discovered(
            service.driver_kind.as_str(),
            &primary.address,
            primary.family,
            Some(service.name.clone()),
            service.domain.clone(),
        );
        inventory.upsert(service);
    }
}

/// `GET /api/v1/discovery/devices` — the current untrusted inventory snapshot
/// (role: read).
///
/// Returns the discovered services found by the most recent scans, AAAA-first
/// with IPv4 labelled legacy, stale (TTL-expired) rows purged on read. These are
/// **hints**, not devices: adopting one is the separate `POST /devices/{id}`
/// confirm-adopt referencing a discovered address (ADR-0041).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/discovery/devices",
        tag = "discovery",
        responses(
            (status = 200, description = "The untrusted discovery inventory (hints requiring explicit confirm-adopt; never devices).", body = [DiscoveredService]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read discovery.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_discovered(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<DiscoveredService>>> {
    principal.role.require(Action::Read)?;
    // Discovery-domain visibility (ADR-W026): a discovery-scoped principal sees
    // only rows in its domain and NEVER an unlabelled row (fail-closed) — the
    // REST twin of the realtime `device.discovered` filter, so a scoped client
    // cannot enumerate out-of-domain inventory it could not receive on the
    // stream. List-filtering (not 403) matches the #211 convention: no existence
    // oracle (ADR-W005).
    Ok(Json(visible_discovery_rows(
        &principal,
        state.discovery.snapshot(),
    )))
}

/// Filter a discovery inventory snapshot to the rows `principal` may see
/// (ADR-W026): each row is gated on the discovery-domain axis via the shared
/// [`scope_permits`](crate::auth::scope_permits) predicate, so REST and the
/// realtime `device.discovered` filter cannot fork. An unscoped principal keeps
/// every row; a discovery-scoped one keeps only its labelled domains.
pub(crate) fn visible_discovery_rows(
    principal: &Principal,
    rows: Vec<DiscoveredService>,
) -> Vec<DiscoveredService> {
    let scopes = principal.scopes();
    rows.into_iter()
        .filter(|row| {
            crate::auth::scope_permits(
                &scopes,
                multiview_events::AuthzScope::DiscoveryDomain(row.domain.as_deref()),
            )
        })
        .collect()
}

#[cfg(test)]
mod discovery_scope_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::visible_discovery_rows;
    use crate::auth::{Principal, Role};
    use crate::devices::discovery::{DiscoveredService, RawDiscoveredService};
    use std::time::{Duration, Instant};

    fn principal(domains: Option<Vec<&str>>) -> Principal {
        Principal {
            key_id: "k".to_owned(),
            role: Role::Operator,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: domains.map(|d| d.into_iter().map(str::to_owned).collect()),
        }
    }

    fn row(domain: Option<&str>) -> DiscoveredService {
        let raw = RawDiscoveredService::new(
            "_ndi._tcp.local.".to_owned(),
            format!("row-{}", domain.unwrap_or("unlabelled")),
            "host.local.".to_owned(),
            5961,
            vec![],
            vec![],
        );
        DiscoveredService::from_raw(
            &raw,
            None,
            Instant::now() + Duration::from_secs(60),
            domain.map(str::to_owned),
        )
    }

    #[test]
    fn unscoped_principal_sees_every_row_including_unlabelled() {
        let rows = vec![row(Some("site-a")), row(Some("site-b")), row(None)];
        assert_eq!(visible_discovery_rows(&principal(None), rows).len(), 3);
    }

    #[test]
    fn discovery_scoped_principal_sees_only_its_domain_never_unlabelled() {
        let rows = vec![row(Some("site-a")), row(Some("site-b")), row(None)];
        let visible = visible_discovery_rows(&principal(Some(vec!["site-a"])), rows);
        let domains: Vec<Option<String>> = visible.iter().map(|r| r.domain.clone()).collect();
        assert_eq!(
            domains,
            vec![Some("site-a".to_owned())],
            "a discovery-scoped principal sees only its own domain — never another's, \
             never an unlabelled row (fail-closed)"
        );
    }

    #[test]
    fn empty_domain_allowlist_sees_nothing() {
        let rows = vec![row(Some("site-a")), row(None)];
        assert!(visible_discovery_rows(&principal(Some(vec![])), rows).is_empty());
    }
}
