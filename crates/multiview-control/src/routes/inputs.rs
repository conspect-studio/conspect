//! The read-only input stream-inventory surface under `/api/v1/inputs`.
//!
//! `GET /api/v1/inputs/{id}/streams` returns the input's
//! [`StreamInventory`](multiview_core::stream::StreamInventory) — every
//! elementary stream (video / audio tracks / subtitles / SCTE-35 / KLV /
//! timecode) the input offers, each with a stable kind-scoped id (RT-3,
//! ADR-0034 §9). This is **read-only discovery** (no routing/switching here):
//! the operator's "all streams must be visible" surface.
//!
//! **Inv #10 (isolation).** The handler reads the inventory from the **cached
//! off-engine** [`EngineStateSnapshot`](crate::state::EngineStateSnapshot) — the
//! wait-free `LatestState` slot the engine publishes into — and **never** touches
//! the output-clock thread, awaits the engine, or sends on any channel the
//! engine could fill. The inventory is built off the engine by the ingest at
//! `open()` and folded into the conflated snapshot blob; this handler only
//! projects it back out. BOLA via [`authorize_object`](crate::auth::authorize_object)
//! mirrors the sources routes; a `404` is returned when the input is unknown /
//! not yet probed; errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::stream::StreamInventory;

use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::state::{AppState, EngineStateSnapshot};

/// The resource kind label used in `404` problem documents for this surface.
const INPUT_KIND: &str = "input";

/// Extract one input's [`StreamInventory`] from the conflated engine-state
/// snapshot blob.
///
/// The inventory is folded into the snapshot under `inputs.<id>.streams` (RT-3:
/// folded into the conflated `EngineStateSnapshot`, not a new typed snapshot
/// stream). Returns:
///
/// * `Ok(inventory)` when the input is present and its inventory parses;
/// * `Err(NotFound)` when the snapshot is absent (engine not yet publishing),
///   has no `inputs` map, or has no entry for `id` (the input is unknown / not
///   yet probed);
/// * `Err(Repository)` when the entry exists but does not deserialise into a
///   [`StreamInventory`] (an engine-side shape drift — surfaced loudly rather
///   than masked as a `404`).
///
/// Pure projection over an immutable snapshot value: no engine, no I/O, no lock
/// the engine holds (inv #10).
fn inventory_from_snapshot(
    snapshot: &EngineStateSnapshot,
    id: &str,
) -> Result<StreamInventory, ControlError> {
    let entry = snapshot
        .get("inputs")
        .and_then(|inputs| inputs.get(id))
        .and_then(|input| input.get("streams"))
        .ok_or_else(|| ControlError::NotFound {
            kind: INPUT_KIND,
            id: id.to_owned(),
        })?;
    serde_json::from_value::<StreamInventory>(entry.clone()).map_err(|e| {
        ControlError::Repository(format!(
            "input {id:?} stream inventory in the engine snapshot is malformed: {e}"
        ))
    })
}

/// `GET /api/v1/inputs/{id}/streams` — the input's elementary-stream inventory
/// (role: read; per-object authz).
///
/// Returns the cached [`StreamInventory`] for input `id` from the off-engine
/// snapshot (inv #10 — the output-clock thread is never touched). `404` when the
/// input is unknown or has not been probed yet.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/inputs/{id}/streams",
        tag = "inputs",
        params(("id" = String, Path, description = "Input/source id.")),
        responses(
            (status = 200, description = "The input's elementary-stream inventory.", body = crate::openapi_schemas::StreamInventoryDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this input.", body = crate::problem::Problem),
            (status = 404, description = "No such input, or it has not been probed yet.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_input_streams(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Read the latest cached snapshot the engine published (wait-free, off the
    // output-clock thread). An absent snapshot means the engine has not begun
    // publishing — the input is simply not-yet-probed, a `404`, never a panic.
    let snapshot = state
        .engine
        .state
        .latest()
        .ok_or_else(|| ControlError::NotFound {
            kind: INPUT_KIND,
            id: id.clone(),
        })?;
    let inventory = inventory_from_snapshot(&snapshot, &id)?;
    Ok((StatusCode::OK, Json(inventory)).into_response())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::{inventory_from_snapshot, INPUT_KIND};
    use crate::error::ControlError;
    use serde_json::json;

    fn snapshot_with_cam1() -> serde_json::Value {
        json!({
            "v": 1,
            "tick": 7,
            "canvas": { "width": 1920, "height": 1080 },
            "inputs": {
                "cam1": {
                    "streams": {
                        "input_id": "cam1",
                        "streams": [
                            {
                                "id": { "kind_scope": "v", "key": "pid:256", "tier": "hard" },
                                "kind": "video",
                                "language": null,
                                "codec": "h264",
                                "title": null,
                                "default": false,
                                "detail": { "detail": "video", "params": { "width": 1920, "height": 1080, "frame_rate": null } }
                            }
                        ]
                    }
                }
            }
        })
    }

    #[test]
    fn known_input_yields_its_inventory() {
        let snap = snapshot_with_cam1();
        let inv = inventory_from_snapshot(&snap, "cam1").unwrap();
        assert_eq!(inv.input_id.as_deref(), Some("cam1"));
        assert_eq!(inv.streams.len(), 1);
        assert_eq!(inv.video().count(), 1);
    }

    #[test]
    fn unknown_input_is_not_found() {
        let snap = snapshot_with_cam1();
        match inventory_from_snapshot(&snap, "ghost") {
            Err(ControlError::NotFound { kind, id }) => {
                assert_eq!(kind, INPUT_KIND);
                assert_eq!(id, "ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_without_inputs_map_is_not_found() {
        // A snapshot that predates any inventory fold-in (just the base blob)
        // must read as not-yet-probed, never a panic.
        let snap = json!({ "v": 1, "tick": 0, "canvas": { "width": 1, "height": 1 } });
        assert!(matches!(
            inventory_from_snapshot(&snap, "cam1"),
            Err(ControlError::NotFound { .. })
        ));
    }

    #[test]
    fn malformed_inventory_is_a_loud_repository_error_not_404() {
        // An entry exists but is the wrong shape (engine-side drift): surface it
        // as a 500 Repository error rather than masking it as a 404.
        let snap = json!({
            "inputs": { "cam1": { "streams": { "streams": "not-an-array" } } }
        });
        assert!(matches!(
            inventory_from_snapshot(&snap, "cam1"),
            Err(ControlError::Repository(_))
        ));
    }
}
