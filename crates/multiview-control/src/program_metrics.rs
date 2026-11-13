//! `GET /api/v1/metrics/program` — Program tick lateness and repeats
//! (ADR-0108 §10, the Program protection bound).
//!
//! The engine keeps cumulative wait-free counters; the binary installs a
//! [`ProgramMetricsSource`] reading them. A 1 s control-plane sampler keeps a
//! bounded ring of 11 cumulative samples, so the window statistics cover the
//! last 10 s. Nothing here touches the clock thread.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_engine::lateness::{LatenessSnapshot, FINE_LATENESS_EDGES_NS, LATENESS_EDGES_NS};
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;

/// A cumulative reading of the Program's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgramCounters {
    /// The lateness histogram.
    pub lateness: LatenessSnapshot,
    /// Last-good repeats emitted (ticks behind).
    pub frames_repeated: u64,
}

/// Reads the running Program's counters (wait-free loads).
pub trait ProgramMetricsSource: Send + Sync {
    /// The cumulative counters now, `None` before a run started.
    fn read(&self) -> Option<ProgramCounters>;
}

/// The sample interval and ring length: 11 samples span 10 s.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Samples kept (bounded).
pub const SAMPLES: usize = 11;

/// The installed source plus its bounded sample ring.
pub struct ProgramMetrics {
    source: Arc<dyn ProgramMetricsSource>,
    ring: Mutex<VecDeque<ProgramCounters>>,
}

impl std::fmt::Debug for ProgramMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramMetrics").finish_non_exhaustive()
    }
}

impl ProgramMetrics {
    /// Wrap a source.
    #[must_use]
    pub fn new(source: Arc<dyn ProgramMetricsSource>) -> Self {
        Self {
            source,
            ring: Mutex::new(VecDeque::with_capacity(SAMPLES)),
        }
    }

    /// Take one sample into the ring (oldest dropped beyond [`SAMPLES`]).
    pub fn sample(&self) {
        let Some(counters) = self.source.read() else {
            return;
        };
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        ring.push_back(counters);
        while ring.len() > SAMPLES {
            ring.pop_front();
        }
    }

    /// The report: window = newest reading minus the oldest kept sample.
    #[must_use]
    pub fn report(&self) -> Option<ProgramMetricsReport> {
        let now = self.source.read()?;
        let ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        let oldest = ring.front().copied().unwrap_or_default();
        let window = now.lateness.since(&oldest.lateness);
        let ms = |ns: Option<u64>| ns.filter(|ns| *ns != u64::MAX).map(ns_to_ms);
        Some(ProgramMetricsReport {
            window_ticks: window.count,
            lateness_p50_ms: ms(window.quantile_upper_ns(500)),
            lateness_p99_ms: ms(window.quantile_upper_ns(990)),
            lateness_over_max_bucket: window.buckets.last().copied().unwrap_or(0),
            lateness_max_ms: ns_to_ms(now.lateness.max_ns),
            ticks_total: now.lateness.count,
            frames_repeated_total: now.frames_repeated,
            frames_repeated_window: now.frames_repeated.saturating_sub(oldest.frames_repeated),
            bucket_edges_ms: LATENESS_EDGES_NS.iter().map(|ns| ns_to_ms(*ns)).collect(),
            buckets_total: now.lateness.buckets.to_vec(),
            lateness_mean_ms: window.mean_ns().map(ns_to_ms),
            lateness_p50_fine_ms: ms(window.fine_quantile_upper_ns(500)),
            lateness_p99_fine_ms: ms(window.fine_quantile_upper_ns(990)),
            lateness_mean_total_ms: now.lateness.mean_ns().map(ns_to_ms),
            fine_bucket_edges_ms: FINE_LATENESS_EDGES_NS
                .iter()
                .map(|ns| ns_to_ms(*ns))
                .collect(),
            fine_buckets_total: now.lateness.fine_buckets.to_vec(),
            fine_buckets_window: window.fine_buckets.to_vec(),
        })
    }
}

fn ns_to_ms(ns: u64) -> f64 {
    // Microsecond resolution: lateness values fit u32 microseconds (~71 min)
    // and saturate beyond; `f64::from(u32)` is exact.
    let micros = u32::try_from(ns / 1_000).unwrap_or(u32::MAX);
    f64::from(micros) / 1_000.0
}

/// The `GET /api/v1/metrics/program` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProgramMetricsReport {
    /// Ticks in the window (about the last 10 s).
    pub window_ticks: u64,
    /// p50 lateness over the window: the upper edge of its bucket, ms
    /// (`null` when empty or beyond the last edge).
    pub lateness_p50_ms: Option<f64>,
    /// p99 lateness over the window: the upper edge of its bucket, ms.
    pub lateness_p99_ms: Option<f64>,
    /// Window ticks later than the last bucket edge (66.8 ms).
    pub lateness_over_max_bucket: u64,
    /// Largest lateness since the run started, ms.
    pub lateness_max_ms: f64,
    /// Ticks emitted since the run started.
    pub ticks_total: u64,
    /// Last-good repeats since the run started (`frames_repeated`).
    pub frames_repeated_total: u64,
    /// Last-good repeats in the window.
    pub frames_repeated_window: u64,
    /// The bucket upper edges, ms (a final overflow bucket follows).
    pub bucket_edges_ms: Vec<f64>,
    /// Cumulative per-bucket counts (edges, then overflow).
    pub buckets_total: Vec<u64>,
    /// Mean lateness over the window, ms (`null` when the window is empty).
    pub lateness_mean_ms: Option<f64>,
    /// p50 lateness over the window on the fine edges, ms.
    pub lateness_p50_fine_ms: Option<f64>,
    /// p99 lateness over the window on the fine edges, ms.
    pub lateness_p99_fine_ms: Option<f64>,
    /// Mean lateness since the run started, ms.
    pub lateness_mean_total_ms: Option<f64>,
    /// The fine bucket upper edges, ms (0.05, 0.1, 0.25, then the coarse
    /// edges; a final overflow bucket follows).
    pub fine_bucket_edges_ms: Vec<f64>,
    /// Cumulative per-fine-bucket counts (fine edges, then overflow).
    pub fine_buckets_total: Vec<u64>,
    /// Per-fine-bucket counts in the window.
    pub fine_buckets_window: Vec<u64>,
}

/// Start the 1 s sampler (control-plane task; ends with the runtime).
pub(crate) fn spawn_sampler(metrics: &Arc<ProgramMetrics>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let metrics = Arc::clone(metrics);
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            interval.tick().await;
            metrics.sample();
        }
    });
}

/// `GET /api/v1/metrics/program` — Program tick lateness p50/p99/max over a
/// 10 s window plus totals (role: read; whole-system principals only).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/metrics/program",
        tag = "system",
        responses(
            (status = 200, description = "Program tick lateness and repeats.", body = ProgramMetricsReport),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Scoped principal (whole-system metric).", body = crate::problem::Problem),
            (status = 404, description = "No running Program reports metrics.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_program_metrics(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::routes::require_unscoped_for_whole_system(&principal)?;
    let Some(report) = state.program_metrics.as_ref().and_then(|m| m.report()) else {
        return Err(crate::ControlError::NotFound {
            kind: "metrics",
            id: "program".to_owned(),
        });
    };
    Ok((StatusCode::OK, Json(report)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_engine::lateness::TickLateness;

    struct Src(Arc<TickLateness>, Arc<std::sync::atomic::AtomicU64>);
    impl ProgramMetricsSource for Src {
        fn read(&self) -> Option<ProgramCounters> {
            Some(ProgramCounters {
                lateness: self.0.snapshot(),
                frames_repeated: self.1.load(std::sync::atomic::Ordering::Relaxed),
            })
        }
    }

    #[test]
    fn the_window_covers_only_the_kept_samples_and_the_ring_is_bounded() {
        let h = TickLateness::shared();
        let rep = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let m = ProgramMetrics::new(Arc::new(Src(Arc::clone(&h), Arc::clone(&rep))));
        // An old burst of very late ticks, then 11 samples of on-time ticks.
        for _ in 0..100 {
            h.record(50_000_000, 0);
        }
        for _ in 0..(SAMPLES + 5) {
            m.sample();
            for _ in 0..30 {
                h.record(0, 0);
            }
        }
        assert_eq!(m.ring.lock().unwrap().len(), SAMPLES);
        let r = m.report().unwrap();
        assert_eq!(
            r.lateness_p99_ms,
            Some(0.5),
            "the old burst left the window"
        );
        assert_eq!(r.lateness_max_ms, 50.0, "max is cumulative");
        assert_eq!(r.ticks_total, 100 + 30 * (11 + 5));
        // The finer view: on-time ticks land in the 0.05 ms bucket, the mean
        // over the window is 0, and the coarse fields are unchanged.
        assert_eq!(r.lateness_p99_fine_ms, Some(0.05));
        assert_eq!(r.lateness_mean_ms, Some(0.0));
        assert_eq!(r.fine_bucket_edges_ms.first(), Some(&0.05));
        assert_eq!(r.fine_buckets_total.len(), r.fine_bucket_edges_ms.len() + 1);
        assert_eq!(
            r.fine_buckets_total.iter().sum::<u64>(),
            r.buckets_total.iter().sum::<u64>()
        );
        assert_eq!(r.fine_buckets_window.iter().sum::<u64>(), r.window_ticks);
    }

    #[test]
    fn the_mean_and_fine_quantiles_resolve_sub_millisecond_lateness() {
        let h = TickLateness::shared();
        let rep = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let m = ProgramMetrics::new(Arc::new(Src(Arc::clone(&h), Arc::clone(&rep))));
        m.sample();
        // 99 ticks 80 µs late and one 3 ms late.
        for _ in 0..99 {
            h.record(80_000, 0);
        }
        h.record(3_000_000, 0);
        let r = m.report().unwrap();
        assert_eq!(r.window_ticks, 100);
        assert_eq!(r.lateness_p50_fine_ms, Some(0.1));
        assert_eq!(r.lateness_p99_fine_ms, Some(0.1));
        assert_eq!(r.lateness_p50_ms, Some(0.5), "coarse view unchanged");
        // (99 * 80 µs + 3 ms) / 100 = 109.2 µs.
        assert_eq!(r.lateness_mean_ms, Some(0.109));
        assert_eq!(r.lateness_mean_total_ms, Some(0.109));
    }
}
