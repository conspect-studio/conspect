//! The engine side of a cast stream's life: the cast take onto a cast
//! output (operation id `cast:<uuid>`, never persisted), the bounded wait for
//! its outcome, and the end of an output's last stream (deactivate after the
//! linger, take back to Program when the cast API moved it).
//!
//! No lock is held across an `.await`: the wait polls the routing snapshot
//! (a wait-free load) and a non-blocking event subscription.

use std::time::Duration;

use crate::output_routing::{OutputTakeRequest, RoutedFeed, TakeSubmitError, CAST_TAKE_PREFIX};
use crate::state::AppState;

/// The longest a stream creation waits for its take's outcome.
pub const TAKE_WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(25);

/// The output's current picture feed from the engine readback (`None` when
/// the engine has published none for it).
#[must_use]
pub fn live_feed(state: &AppState, config_id: &str) -> Option<RoutedFeed> {
    let control = state.output_routing.as_ref()?;
    let snapshot = control.routing.snapshot()?;
    let live = snapshot.outputs.get(config_id)?;
    if live.pending.is_some() {
        return None;
    }
    RoutedFeed::from_bus_name(&live.bus)
}

/// How a cast take ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeResult {
    /// The output shows the feed.
    Applied,
    /// The engine refused or superseded it: its outcome text.
    Refused(String),
    /// No outcome within [`TAKE_WAIT`].
    TimedOut,
    /// The take could not be enqueued.
    NotEnqueued(TakeSubmitError),
}

/// A fresh cast operation id.
#[must_use]
pub fn cast_operation_id() -> String {
    format!("{CAST_TAKE_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Enqueue a cast take of `config_id` to `feed` (no wait). `principal` is
/// the key id the take is charged to.
///
/// # Errors
/// [`TakeSubmitError`] when it cannot be enqueued (or no routing exists).
pub fn submit_take(
    state: &AppState,
    operation_id: &str,
    config_id: &str,
    feed: &RoutedFeed,
    layout: Option<crate::output_routing::LayoutBody>,
    principal: &str,
) -> Result<(), TakeSubmitError> {
    let Some(control) = state.output_routing.as_ref() else {
        return Err(TakeSubmitError::Unroutable);
    };
    // ABUSE-1: the cast API's own feed moves (a take for a new stream, the
    // take back to Program) re-check every live token on the output first.
    super::reauth::before_change(
        state,
        config_id,
        Some(operation_id),
        Some(feed),
        layout.as_ref().map(|body| body.0.as_ref()),
        None,
        principal,
    );
    control.routing.try_take(OutputTakeRequest {
        operation_id: operation_id.to_owned(),
        output: config_id.to_owned(),
        video: Some(feed.clone()),
        audio: None,
        layout,
        principal: principal.to_owned(),
    })
}

/// Take `config_id` to `feed` and wait (bounded by [`TAKE_WAIT`]) for the
/// outcome: the `route.output` event of this operation, or the readback
/// showing the feed with no take pending.
pub async fn take_and_wait(
    state: &AppState,
    config_id: &str,
    feed: &RoutedFeed,
    layout: Option<crate::output_routing::LayoutBody>,
    principal: &str,
) -> TakeResult {
    let op = cast_operation_id();
    // Subscribe before the enqueue, so the outcome cannot be missed.
    let mut events = state.engine.subscribe();
    if let Err(error) = submit_take(state, &op, config_id, feed, layout, principal) {
        return TakeResult::NotEnqueued(error);
    }
    let deadline = tokio::time::Instant::now() + TAKE_WAIT;
    loop {
        while let Ok(event) = events.try_recv() {
            if let multiview_events::Event::RouteOutput(outcome) = &*event.event {
                if outcome.operation_id == op {
                    return if outcome.outcome == "applied" {
                        TakeResult::Applied
                    } else {
                        TakeResult::Refused(outcome.outcome.clone())
                    };
                }
            }
        }
        if live_feed(state, config_id).as_ref() == Some(feed) {
            return TakeResult::Applied;
        }
        if tokio::time::Instant::now() >= deadline {
            return TakeResult::TimedOut;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Finish the outputs whose last stream ended: deactivate each and, when the
/// cast API moved its feed, take it back to Program (no wait).
pub fn finish(state: &AppState, ended: &[super::Ended]) {
    for end in ended {
        if let Some(engine) = state.cast_engine.as_ref() {
            if let Err(error) = engine.deactivate(&end.config_id) {
                tracing::debug!(%error, output = %end.output, "cast: deactivate failed");
            }
        }
        if end.moved {
            let op = cast_operation_id();
            if let Err(error) = submit_take(
                state,
                &op,
                &end.config_id,
                &RoutedFeed::Program,
                None,
                "cast",
            ) {
                tracing::warn!(?error, output = %end.output, "cast: take back to Program not enqueued");
            }
        }
    }
}

/// Run a sweep now and finish what it ended (the lazy sweep of every API
/// call and the 5 s timer).
pub fn sweep_now(state: &AppState) {
    let ended = state.cast_streams.sweep();
    finish(state, &ended);
}

/// The periodic re-check of every live token under its owner's CURRENT
/// grants (the sweep's backstop: a re-scoped or revoked key, a feed change
/// no take path saw), audited to `cast`.
pub fn recheck_now(state: &AppState) {
    super::reauth::recheck_all(state, "cast");
}

/// Start the one 5 s sweep task (idempotent per registry).
pub(crate) fn spawn_sweeper(state: &AppState) {
    if !state.cast_streams.claim_sweeper() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = std::sync::Arc::downgrade(&state.cast_streams);
    let state = state.clone();
    runtime.spawn(async move {
        let mut tick = tokio::time::interval(super::SWEEP_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if weak.strong_count() <= 1 {
                // Only this task's own state clone holds the registry.
                return;
            }
            sweep_now(&state);
            // Backstop (ABUSE-1): what an output carries now, under each
            // owner's current grants (a re-scoped or revoked key, a feed
            // change no take path saw).
            recheck_now(&state);
            crate::routes::cast_sessions::reap_streamless_sessions(&state);
        }
    });
}
