//! The engine→control tally ingest: a read-only, lossy, lagged-skip subscriber
//! that mirrors engine [`TallyEvent`](multiview_events::TallyEvent)s into the
//! [`TallyMirror`].
//!
//! ## Isolation (invariant #10) is the load-bearing property
//!
//! Ingest subscribes to the engine's drop-oldest event broadcast
//! ([`EventSubscription`]) and **only ever reads**. It never sends on a path the
//! engine awaits and never blocks the engine's publish. A slow mirror or a burst
//! of tally changes cannot back-pressure the engine: when this subscriber falls
//! behind, the broadcast reports [`RecvError::Lagged`] and ingest **resubscribes
//! at the head** (lagged-skip), dropping the intermediate states it missed.
//! Missing an intermediate tally change is safe — the next
//! [`TallyEvent`](multiview_events::TallyEvent) for a target carries the current
//! resolved state, so the mirror re-converges.
//!
//! The ingest is a pure classifier ([`crate::tally_state::tally_observation`])
//! plus a thin drive loop, so the "which events are tally observations" decision
//! is exhaustively unit-testable with no async, sockets, or sleeps.
use std::sync::Arc;

use multiview_engine::{EventSubscription, RecvError};
use multiview_events::Event;

use crate::tally_state::{tally_observation, TallyMirror};

/// The outcome of pumping one step of the tally ingest loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TallyIngestStep {
    /// A tally observation was applied to the mirror.
    Applied,
    /// A non-tally event (or a duplicate) was skipped.
    Skipped,
    /// This subscriber lagged; it resubscribed at the head (lagged-skip). The
    /// engine was never back-pressured.
    Lagged,
    /// The engine is gone (every publish handle dropped); the loop should stop.
    Closed,
}

/// Receive one event and apply it to the mirror, returning the step outcome.
///
/// On [`RecvError::Lagged`] this **resubscribes at the head and returns
/// [`TallyIngestStep::Lagged`]** — it never propagates back-pressure
/// (invariant #10). On a non-tally event it returns [`TallyIngestStep::Skipped`].
/// On a tally observation it records the latest state and returns
/// [`TallyIngestStep::Applied`].
pub async fn tally_ingest_step(
    sub: &mut EventSubscription<Event>,
    mirror: &TallyMirror,
) -> TallyIngestStep {
    match sub.recv().await {
        Ok(seq_event) => match tally_observation(&seq_event.event) {
            Some(tally) => {
                mirror.apply(tally.clone());
                TallyIngestStep::Applied
            }
            None => TallyIngestStep::Skipped,
        },
        Err(RecvError::Lagged(missed)) => {
            tracing::debug!(missed, "tally ingest lagged; resubscribing at head");
            *sub = sub.resubscribe();
            TallyIngestStep::Lagged
        }
        Err(RecvError::Closed) => TallyIngestStep::Closed,
    }
}

/// Run the tally ingest loop to completion.
///
/// Drains engine tally observations into `mirror` until the engine is gone. This
/// is the long-lived task the control plane spawns at startup; it owns one
/// engine subscription and the shared mirror. It can never block the engine (it
/// only reads the drop-oldest broadcast and lagged-skips).
pub async fn run_tally_ingest(mut sub: EventSubscription<Event>, mirror: Arc<TallyMirror>) {
    loop {
        match tally_ingest_step(&mut sub, mirror.as_ref()).await {
            TallyIngestStep::Closed => break,
            TallyIngestStep::Applied | TallyIngestStep::Skipped | TallyIngestStep::Lagged => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::tally::{TallyColor, TallyState};
    use multiview_engine::EnginePublisher;
    use multiview_events::{Event, TallyEvent, TallyTarget};

    use super::{tally_ingest_step, TallyIngestStep};
    use crate::tally_state::TallyMirror;

    type Publisher = EnginePublisher<serde_json::Value, Event>;

    fn tally(index: u32, state: TallyState) -> Event {
        Event::TallyState(TallyEvent {
            target: TallyTarget::Tile { index },
            state,
        })
    }

    #[tokio::test]
    async fn applies_tally_observation_to_the_mirror() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let mirror = TallyMirror::new();

        engine.publish_event(tally(7, TallyState::program()));
        let step = tally_ingest_step(&mut sub, &mirror).await;
        assert_eq!(step, TallyIngestStep::Applied);

        let got = mirror.get(&TallyTarget::Tile { index: 7 }).unwrap();
        assert_eq!(got.state.color, TallyColor::Red);
    }

    #[tokio::test]
    async fn skips_non_tally_events() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let mirror = TallyMirror::new();
        engine.publish_event(Event::Ping);
        assert_eq!(
            tally_ingest_step(&mut sub, &mirror).await,
            TallyIngestStep::Skipped
        );
        assert!(mirror.is_empty());
    }

    #[tokio::test]
    async fn closed_when_engine_is_gone() {
        let engine: Publisher = EnginePublisher::new(8);
        let mut sub = engine.subscribe();
        let mirror = TallyMirror::new();
        drop(engine);
        assert_eq!(
            tally_ingest_step(&mut sub, &mirror).await,
            TallyIngestStep::Closed
        );
    }

    #[tokio::test]
    async fn lagged_skip_never_back_pressures_the_engine() {
        // A tiny ring; the engine publishes far more than capacity while ingest
        // never drains. Each publish must return promptly; ingest recovers via
        // resubscribe (lagged-skip), never forcing the engine to wait.
        let engine: Publisher = EnginePublisher::new(4);
        let mut sub = engine.subscribe();
        let mirror = TallyMirror::new();

        for i in 0..1000u32 {
            let seq = engine.publish_event(tally(i, TallyState::preview()));
            assert_eq!(seq, u64::from(i) + 1);
        }

        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tally_ingest_step(&mut sub, &mirror),
        )
        .await
        .expect("lagged recovery must not block");
        assert_eq!(step, TallyIngestStep::Lagged);

        // After recovery the next published tally is applied cleanly.
        engine.publish_event(tally(4242, TallyState::program()));
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tally_ingest_step(&mut sub, &mirror),
        )
        .await
        .expect("post-recovery delivery must not block");
        assert_eq!(step, TallyIngestStep::Applied);
        assert!(mirror.get(&TallyTarget::Tile { index: 4242 }).is_some());
    }
}
