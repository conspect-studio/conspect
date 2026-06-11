//! The engine→control health-warning ingest: a read-only, lossy, lagged-skip
//! subscriber that mirrors engine health-warning transitions into the
//! [`WarningRepository`] (ADR-0035 SA-0).
//!
//! ## Isolation (invariant #10) is the load-bearing property
//!
//! Ingest subscribes to the engine's drop-oldest event broadcast
//! ([`EventSubscription`]) and **only ever reads**. It never sends on a path the
//! engine awaits and never blocks the engine's publish. A slow store or a burst
//! of warnings cannot back-pressure the engine: when this subscriber falls
//! behind, the broadcast reports [`RecvError::Lagged`] and ingest **resubscribes
//! at the head** (lagged-skip), dropping the events it missed rather than ever
//! applying back-pressure. Missing an intermediate transition is safe — health
//! warnings coalesce on their stable [`WarningCode`](multiview_events::WarningCode)
//! and the next transition carries the current value, so the mirror re-converges.
//!
//! This is a deliberate copy of [`crate::alarm_ingest`] (swallow-and-skip on a
//! store error, `Lagged` resubscribes at head). It is structured as a pure
//! classifier ([`warning_transition`]) plus a thin drive loop
//! ([`run_warning_ingest`]) so the classification is exhaustively unit-testable
//! with no async, no sockets, and no sleeps.
use std::sync::Arc;

use multiview_engine::{EnginePublisher, EventSubscription, RecvError};
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

use crate::warning_store::WarningRepository;

/// A control-owned, dependency-light view of the SA-0 composite MISMATCH.
///
/// The build site (the CLI) computes the both-halves cross-check with
/// `multiview_hal::composite_mismatch` and maps the resulting
/// `multiview_hal::CompositeMismatch` into this small view, so this crate does not
/// need a `multiview-hal` dependency to know how to *phrase* the warning. `None`
/// (no mismatch) means a clean / GPU-free / software-only host — nothing is
/// emitted (the no-false-positive rule holds at the emit seam too).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompositeMismatchView {
    /// The discovered GPU's human-readable name, if a `DeviceLoad` named one.
    pub gpu_name: Option<String>,
    /// `true` when the compositor obtained **no** adapter at all (the canonical
    /// no-Vulkan-adapter case); `false` when it resolved a *software* adapter
    /// (llvmpipe / `device_type == Cpu`). Only changes the wording, not the code.
    pub no_adapter: bool,
}

/// Emit the SA-0 capability warnings for a build-time composite cross-check.
///
/// This is the thin **emit seam** the CLI build path calls (off the output-clock
/// thread — the clock is not yet constructed, so inv #1 is preserved). On a
/// `Some(mismatch)` it builds the latched `gpu-present-no-vulkan-adapter`
/// [`HealthWarning`] (the canonical message + the `graphics` / `libvulkan1`
/// remediation) and publishes it through the engine's **drop-oldest**
/// [`EnginePublisher::publish_event`] (the identical non-blocking path as
/// `SystemMetrics`; inv #10 — it can never back-pressure the engine). On `None`
/// (a clean / GPU-free / software-only host) it publishes nothing.
///
/// `since_nanos` is the engine monotonic timestamp to stamp the warning's `since`
/// with. Returns the number of warnings published (`0` or `1` in SA-0).
pub fn emit_capability_warnings<S>(
    publisher: &EnginePublisher<S, Event>,
    mismatch: Option<CompositeMismatchView>,
    since_nanos: i64,
) -> usize {
    let Some(mismatch) = mismatch else {
        return 0;
    };
    let warning = gpu_present_no_vulkan_adapter_warning(&mismatch, since_nanos);
    // The warn-level log (not info) is the ADR-0035 requirement: the silent
    // info-only fallback becomes loud and reported.
    tracing::warn!(
        code = warning.code.as_str(),
        gpu = mismatch.gpu_name.as_deref().unwrap_or("(unnamed)"),
        "GPU present but GPU compositing is unavailable; compositing fell back to CPU"
    );
    publisher.publish_event(Event::HealthWarningRaised(warning));
    1
}

/// Build the latched `gpu-present-no-vulkan-adapter` warning (the catalog entry).
///
/// The single source of the operator copy + remediation for SA-0 (ADR-0035 §5.1).
fn gpu_present_no_vulkan_adapter_warning(
    mismatch: &CompositeMismatchView,
    since_nanos: i64,
) -> HealthWarning {
    let gpu = mismatch.gpu_name.as_deref().unwrap_or("a GPU");
    let cause = if mismatch.no_adapter {
        "no Vulkan adapter was available"
    } else {
        "the wgpu compositor resolved a software (CPU) adapter"
    };
    HealthWarning {
        code: WarningCode::GpuPresentNoVulkanAdapter,
        severity: WarningSeverity::Warning,
        subsystem: "compositor".to_owned(),
        message: format!(
            "{gpu} detected, but GPU compositing is UNAVAILABLE ({cause}); \
             compositing fell back to the CPU reference (high CPU, GPU idle)."
        ),
        remediation: "Grant the container the `graphics` driver capability \
                      (set NVIDIA_DRIVER_CAPABILITIES to include `graphics`, or `all`) \
                      and install the Vulkan loader (`libvulkan1`) + the GPU's ICD \
                      (e.g. `nvidia_icd.json`); then restart so the wgpu compositor \
                      can acquire a real GPU adapter."
            .to_owned(),
        since: since_nanos,
        active: true,
    }
}

/// Classify an engine [`Event`] as a health-warning transition, if it is one.
///
/// Returns a reference to the carried [`HealthWarning`] for the two warning
/// events (`health.warning.raised` carrying `active = true`,
/// `health.warning.cleared` carrying `active = false`), and [`None`] for every
/// other event. Pure and total — the unit of behaviour the ingest loop is built
/// on. (Both variants are upserted identically: the carried `active` flag drives
/// raise-vs-clear coalescing in the store, mirroring `Alert.active`.)
#[must_use]
pub fn warning_transition(event: &Event) -> Option<&HealthWarning> {
    match event {
        Event::HealthWarningRaised(w) | Event::HealthWarningCleared(w) => Some(w),
        _ => None,
    }
}

/// The outcome of pumping one step of the warning ingest loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningIngestStep {
    /// A health-warning transition was applied to the store.
    Applied,
    /// A non-warning event was skipped.
    Skipped,
    /// This subscriber lagged; it resubscribed at the head (lagged-skip). The
    /// engine was never back-pressured.
    Lagged,
    /// The engine is gone (every publish handle dropped); the loop should stop.
    Closed,
}

/// Receive one event and apply it to the store, returning the step outcome.
///
/// On [`RecvError::Lagged`] this **resubscribes at the head and returns
/// [`WarningIngestStep::Lagged`]** — it never propagates back-pressure (invariant
/// #10). On a non-warning event it returns [`WarningIngestStep::Skipped`]. On a
/// warning transition it [`upsert`](WarningRepository::upsert)s the warning
/// (coalescing on its code) and returns [`WarningIngestStep::Applied`]. A store
/// error is logged and treated as a skip — a flaky control-plane store must never
/// wedge ingest or the engine.
pub async fn warning_ingest_step(
    sub: &mut EventSubscription<Event>,
    store: &dyn WarningRepository,
) -> WarningIngestStep {
    match sub.recv().await {
        Ok(seq_event) => match warning_transition(&seq_event.event) {
            Some(warning) => {
                if let Err(err) = store.upsert(warning.clone()) {
                    tracing::warn!(error = %err, "health-warning ingest: store upsert failed; dropping");
                    WarningIngestStep::Skipped
                } else {
                    WarningIngestStep::Applied
                }
            }
            None => WarningIngestStep::Skipped,
        },
        Err(RecvError::Lagged(missed)) => {
            // Drop-oldest overflow for THIS slow subscriber only: resubscribe at
            // the head. The engine never saw back-pressure (invariant #10). The
            // mirror re-converges on the next transition per code.
            tracing::debug!(
                missed,
                "health-warning ingest lagged; resubscribing at head"
            );
            *sub = sub.resubscribe();
            WarningIngestStep::Lagged
        }
        Err(RecvError::Closed) => WarningIngestStep::Closed,
    }
}

/// Run the warning ingest loop to completion.
///
/// Drains engine health-warning transitions into `store` until the engine is
/// gone. This is the long-lived task the control plane spawns at startup; it owns
/// one engine subscription and the shared warning store. It can never block the
/// engine (it only reads the drop-oldest broadcast and lagged-skips).
pub async fn run_warning_ingest(
    mut sub: EventSubscription<Event>,
    store: Arc<dyn WarningRepository>,
) {
    loop {
        match warning_ingest_step(&mut sub, store.as_ref()).await {
            WarningIngestStep::Closed => break,
            WarningIngestStep::Applied | WarningIngestStep::Skipped | WarningIngestStep::Lagged => {
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_engine::EnginePublisher;
    use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

    use super::{warning_ingest_step, warning_transition, WarningIngestStep};
    use crate::warning_store::{InMemoryWarningStore, WarningFilter, WarningRepository};

    type Publisher = EnginePublisher<serde_json::Value, Event>;

    fn warning(active: bool) -> HealthWarning {
        HealthWarning {
            code: WarningCode::GpuPresentNoVulkanAdapter,
            severity: WarningSeverity::Warning,
            subsystem: "compositor".to_owned(),
            message: "msg".to_owned(),
            remediation: "fix".to_owned(),
            since: 1,
            active,
        }
    }

    #[test]
    fn classifier_maps_both_warning_events_and_ignores_others() {
        assert!(warning_transition(&Event::HealthWarningRaised(warning(true))).is_some());
        assert!(warning_transition(&Event::HealthWarningCleared(warning(false))).is_some());
        assert!(warning_transition(&Event::Ping).is_none());
    }

    #[tokio::test]
    async fn ingest_step_applies_a_warning_to_the_store() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let store = InMemoryWarningStore::new();

        engine.publish_event(Event::HealthWarningRaised(warning(true)));
        let step = warning_ingest_step(&mut sub, &store).await;
        assert_eq!(step, WarningIngestStep::Applied);
        assert_eq!(store.list(&WarningFilter::active_only()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ingest_step_skips_non_warning_events() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let store = InMemoryWarningStore::new();

        engine.publish_event(Event::Ping);
        assert_eq!(
            warning_ingest_step(&mut sub, &store).await,
            WarningIngestStep::Skipped
        );
        assert!(store.list(&WarningFilter::default()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn ingest_lagged_skip_never_back_pressures_the_engine() {
        let engine: Publisher = EnginePublisher::new(4);
        let mut sub = engine.subscribe();
        let store = InMemoryWarningStore::new();

        for i in 0..1000 {
            let seq = engine.publish_event(Event::HealthWarningRaised(warning(true)));
            assert_eq!(seq, u64::try_from(i + 1).unwrap());
        }

        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            warning_ingest_step(&mut sub, &store),
        )
        .await
        .expect("lagged recovery must not block");
        assert_eq!(step, WarningIngestStep::Lagged);

        engine.publish_event(Event::HealthWarningRaised(warning(true)));
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            warning_ingest_step(&mut sub, &store),
        )
        .await
        .expect("post-recovery delivery must not block");
        assert_eq!(step, WarningIngestStep::Applied);
    }

    #[tokio::test]
    async fn ingest_step_reports_closed_when_engine_is_gone() {
        let engine: Publisher = EnginePublisher::new(8);
        let mut sub = engine.subscribe();
        let store = InMemoryWarningStore::new();
        drop(engine);
        assert_eq!(
            warning_ingest_step(&mut sub, &store).await,
            WarningIngestStep::Closed
        );
    }
}
