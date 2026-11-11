//! Off-clock relay from the bounded switcher outcome ring to the existing
//! management WS/SSE engine event stream. It cannot back-pressure the clock.

use crate::source_demand::{SwitcherLeaseSweep, SwitcherPreviewLeases};
use crate::state::EngineStateSnapshot;
use multiview_engine::isolation::{EnginePublisher, EventSubscription, RecvError};
use multiview_engine::switcher_control::{SwitcherControl, SwitcherOutcome};
use multiview_engine::StopSignal;
use multiview_events::{Event, SwitcherOperation};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the off-clock lease sweep reconciles lost selections. A control
/// cadence, never the output tick.
pub const SWEEPER_INTERVAL: Duration = Duration::from_millis(250);

/// Consume correlated terminal outcomes until the switcher driver is dropped.
/// The publisher's event broadcast is bounded drop-oldest; a stalled WS/SSE
/// subscriber is never awaited by this adapter or by the output tick.
pub async fn forward_switcher_outcomes(
    mut outcomes: EventSubscription<SwitcherOutcome>,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    me: String,
    program: String,
    leases: Arc<crate::source_demand::SwitcherPreviewLeases>,
) {
    loop {
        match outcomes.recv().await {
            Ok(item) => {
                let outcome = &item.event;
                leases.resolve(&outcome.operation_id, outcome.completed);
                publisher.publish_event(Event::SwitcherOperation(SwitcherOperation {
                    me: me.clone(),
                    program: program.clone(),
                    tick: outcome.tick,
                    operation_id: outcome.operation_id.clone(),
                    completed: outcome.completed,
                    error: outcome.error.clone(),
                }));
            }
            Err(RecvError::Lagged(_)) => {
                // An operator that falls behind must re-fetch the authoritative
                // M/E state and operation status; never block the output tick.
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Reconcile accepted-but-unresolved PVW selections against the engine's own
/// applied snapshot, on a bounded control cadence and **off** the output clock.
///
/// A dead relay cannot retain pending demand forever. This reconciles resource
/// ownership only: it does not infer operation success from source equality.
/// At most 64 reservations are inspected per pass, off the output clock.
pub async fn sweep_switcher_leases(
    control: SwitcherControl,
    leases: Arc<SwitcherPreviewLeases>,
    stop: StopSignal,
    interval: Duration,
) {
    while !stop.is_stopped() {
        tokio::time::sleep(interval).await;
        if stop.is_stopped() {
            break;
        }
        // Read the commit count first: the following snapshot is at least as new.
        let composed_count = control.composed_count();
        let preview = control
            .snapshot()
            .map(|snapshot| snapshot.preview.as_str().to_owned());
        let report = leases.sweep(
            preview.as_deref(),
            composed_count,
            Instant::now(),
            SwitcherPreviewLeases::MAX_PENDING,
        );
        if report != SwitcherLeaseSweep::default() {
            tracing::debug!(
                adopted = report.adopted,
                dropped = report.dropped,
                retained = report.retained,
                cancelled = report.cancelled,
                pending = leases.pending_count(),
                "switcher lease sweep reconciled pending preview selections"
            );
        }
    }
}
