//! `GET /api/v1/metrics/frontdoor` — the network front door's live counters
//! (ADR-0107 slice 3). Same auth as `/metrics/program`: Read + unscoped
//! whole-system principals only. 404 when no front door runs this process.
//!
//! The CLI installs a [`FrontDoorMetricsSource`] over the running
//! `multiview-frontdoor::Stats` (and the nft-floor re-reader, slice 3); this
//! module only shapes the wire report and enforces auth. No engine channel,
//! no lock held across an await (invariant #10): every counter is a wait-free
//! atomic load.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;
use crate::system::{NftFloorStatus, Udp443Steering};

/// Reads the running front door's live counters and gauges.
pub trait FrontDoorMetricsSource: Send + Sync {
    /// The current report, or `None` before/without a running front door.
    fn read(&self) -> Option<FrontDoorMetricsReport>;
}

/// The `GET /api/v1/metrics/frontdoor` body: the live
/// `multiview_frontdoor::StatsSnapshot` counters, plus the steering,
/// kernel-drop and nft-floor gauges and the QUIC retry counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FrontDoorMetricsReport {
    /// Datagrams read from the socket(s).
    pub received: u64,
    /// Delivered to the QUIC lane.
    pub to_quic: u64,
    /// Delivered to the WebRTC lane.
    pub to_rtc: u64,
    /// Dropped by the classifier.
    pub dropped_classify: u64,
    /// Dropped by the per-source new-flow bucket.
    pub dropped_per_source: u64,
    /// Dropped by the global new-flow budget.
    pub dropped_global: u64,
    /// Dropped by the global Version Negotiation budget.
    pub dropped_vn: u64,
    /// Dropped by the global handshake (Initial) budget.
    pub dropped_initial: u64,
    /// Dropped because a lane queue was full.
    pub dropped_queue_full: u64,
    /// Dropped because the lane had no consumer.
    pub dropped_no_consumer: u64,
    /// Socket receive errors (other than `WouldBlock`).
    pub recv_errors: u64,
    /// Source keys currently tracked by the new-flow limiter.
    pub tracked_sources: u64,
    /// Datagrams read from the established socket (steered peers).
    pub received_established_socket: u64,
    /// Datagrams read from the listener socket.
    pub received_listener_socket: u64,
    /// Datagrams read from the established socket whose source was in no
    /// established set.
    pub steered_unverified: u64,
    /// Established peers not steered because of the program's peer cap.
    pub steering_overflow: u64,
    /// Steering programs attached.
    pub steering_updates: u64,
    /// Steering attaches that failed.
    pub steering_update_errors: u64,
    /// Peers the current steering program steers.
    pub steered_peers: u64,
    /// Kernel receive drops on the listener socket.
    pub kernel_drops_listener: u64,
    /// Kernel receive drops on the established socket.
    pub kernel_drops_established: u64,
    /// Dropped by the per-source STUN bucket.
    pub dropped_stun_per_source: u64,
    /// Dropped by the global STUN budget.
    pub dropped_stun_global: u64,
    /// Dropped by the per-established-peer bucket.
    pub dropped_established_peer: u64,
    /// QUIC Retries sent (RFC 9000 section 8.1.2).
    pub quic_retries: u64,
    /// How the front door currently receives.
    pub udp443_steering: Udp443Steering,
    /// The current nft UDP-443 floor status (re-read periodically; see
    /// `GET /api/v1/system/capabilities` for the startup snapshot).
    pub nft_floor: NftFloorStatus,
}

/// `GET /api/v1/metrics/frontdoor` — the network front door's live counters
/// (role: read; whole-system principals only).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/metrics/frontdoor",
        tag = "system",
        responses(
            (status = 200, description = "Front-door counters and gauges.", body = FrontDoorMetricsReport),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Scoped principal (whole-system metric).", body = crate::problem::Problem),
            (status = 404, description = "No running front door reports metrics.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_frontdoor_metrics(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::routes::require_unscoped_for_whole_system(&principal)?;
    let Some(report) = state.frontdoor_metrics.as_ref().and_then(|m| m.read()) else {
        return Err(crate::ControlError::NotFound {
            kind: "metrics",
            id: "frontdoor".to_owned(),
        });
    };
    Ok((StatusCode::OK, Json(report)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Src(Option<FrontDoorMetricsReport>);
    impl FrontDoorMetricsSource for Src {
        fn read(&self) -> Option<FrontDoorMetricsReport> {
            self.0
        }
    }

    const SAMPLE: FrontDoorMetricsReport = FrontDoorMetricsReport {
        received: 1,
        to_quic: 1,
        to_rtc: 0,
        dropped_classify: 0,
        dropped_per_source: 0,
        dropped_global: 0,
        dropped_vn: 0,
        dropped_initial: 0,
        dropped_queue_full: 0,
        dropped_no_consumer: 0,
        recv_errors: 0,
        tracked_sources: 0,
        received_established_socket: 0,
        received_listener_socket: 1,
        steered_unverified: 0,
        steering_overflow: 0,
        steering_updates: 1,
        steering_update_errors: 0,
        steered_peers: 0,
        kernel_drops_listener: 0,
        kernel_drops_established: 0,
        dropped_stun_per_source: 0,
        dropped_stun_global: 0,
        dropped_established_peer: 0,
        quic_retries: 0,
        udp443_steering: Udp443Steering::ReuseportCbpf,
        nft_floor: NftFloorStatus::Present,
    };

    #[test]
    fn a_source_with_no_front_door_reports_none() {
        let src = Src(None);
        assert!(src.read().is_none());
    }

    #[test]
    fn a_running_front_door_reports_its_snapshot() {
        let src = Src(Some(SAMPLE));
        assert_eq!(src.read(), Some(SAMPLE));
    }
}
