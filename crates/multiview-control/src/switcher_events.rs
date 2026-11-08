//! Off-clock relay from the bounded switcher outcome ring to the existing
//! management WS/SSE engine event stream. It cannot back-pressure the clock.

use crate::state::EngineStateSnapshot;
use multiview_engine::isolation::{EnginePublisher, EventSubscription, RecvError};
use multiview_engine::switcher_control::SwitcherOutcome;
use multiview_events::{Event, SwitcherOperation};
use std::sync::Arc;

/// Consume correlated terminal outcomes until the switcher driver is dropped.
/// The publisher's event broadcast is bounded drop-oldest; a stalled WS/SSE
/// subscriber is never awaited by this adapter or by the output tick.
pub async fn forward_switcher_outcomes(
    mut outcomes: EventSubscription<SwitcherOutcome>,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    me: String,
    program: String,
) {
    loop {
        match outcomes.recv().await {
            Ok(item) => {
                let outcome = &item.event;
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
