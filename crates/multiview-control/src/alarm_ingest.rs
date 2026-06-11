//! The engine→control alarm ingest: a read-only, lossy, lagged-skip subscriber
//! that mirrors engine alarm transitions into the [`AlarmRepository`].
//!
//! ## Isolation (invariant #10) is the load-bearing property
//!
//! Ingest subscribes to the engine's drop-oldest event broadcast
//! ([`EventSubscription`]) and **only ever reads**. It never sends on a path the
//! engine awaits and never blocks the engine's publish. A slow store or a burst
//! of alarms cannot back-pressure the engine: when this subscriber falls behind,
//! the broadcast reports [`RecvError::Lagged`] and ingest **resubscribes at the
//! head** (lagged-skip), dropping the events it missed rather than ever applying
//! back-pressure. Missing an intermediate transition is safe — the next
//! transition for that alarm carries the current [`AlarmRecord`], so the mirror
//! re-converges.
//!
//! The ingest is structured as a pure classifier ([`alarm_transition`]) plus a
//! thin drive loop ([`run_alarm_ingest`]) so the classification — "which engine
//! events are alarm transitions, and which transition kind" — is exhaustively
//! unit-testable with no async, no sockets, and no sleeps.
use std::sync::Arc;

use multiview_core::alarm::AlarmRecord;
use multiview_engine::{EventSubscription, RecvError};
use multiview_events::Event;

use crate::alarm_store::AlarmRepository;
use crate::notify::AlarmTransitionKind;

/// Classify an engine [`Event`] as an alarm transition, if it is one.
///
/// Returns the [`AlarmTransitionKind`] and a reference to the carried
/// [`AlarmRecord`] for the four alarm events (`alarm.raised` / `alarm.updated` /
/// `alarm.cleared` / `alarm.acked`), and [`None`] for every other event. Pure
/// and total — the unit of behaviour the ingest loop is built on.
#[must_use]
pub fn alarm_transition(event: &Event) -> Option<(AlarmTransitionKind, &AlarmRecord)> {
    match event {
        Event::AlarmRaised(t) => Some((AlarmTransitionKind::Raised, &t.record)),
        Event::AlarmUpdated(t) => Some((AlarmTransitionKind::Updated, &t.record)),
        Event::AlarmCleared(t) => Some((AlarmTransitionKind::Cleared, &t.record)),
        Event::AlarmAcked(t) => Some((AlarmTransitionKind::Acked, &t.record)),
        _ => None,
    }
}

/// The outcome of pumping one step of the alarm ingest loop.
///
/// Returned by [`ingest_step`] so the drive loop's control flow is itself
/// testable without a live broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestStep {
    /// An alarm transition was applied to the store.
    Applied(AlarmTransitionKind),
    /// A non-alarm event (or a resumed/duplicate) was skipped.
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
/// [`IngestStep::Lagged`]** — it never propagates back-pressure (invariant #10).
/// On a non-alarm event it returns [`IngestStep::Skipped`]. On an alarm
/// transition it [`upsert`](AlarmRepository::upsert)s the record and returns
/// [`IngestStep::Applied`]. A store error is logged and treated as a skip — a
/// flaky control-plane store must never wedge ingest or the engine.
pub async fn ingest_step(
    sub: &mut EventSubscription<Event>,
    store: &dyn AlarmRepository,
) -> IngestStep {
    match sub.recv().await {
        Ok(seq_event) => match alarm_transition(&seq_event.event) {
            Some((kind, record)) => {
                if let Err(err) = store.upsert(record.clone()) {
                    tracing::warn!(error = %err, "alarm ingest: store upsert failed; dropping");
                    IngestStep::Skipped
                } else {
                    IngestStep::Applied(kind)
                }
            }
            None => IngestStep::Skipped,
        },
        Err(RecvError::Lagged(missed)) => {
            // Drop-oldest overflow for THIS slow subscriber only: resubscribe at
            // the head. The engine never saw back-pressure (invariant #10). The
            // mirror re-converges on the next transition per alarm.
            tracing::debug!(missed, "alarm ingest lagged; resubscribing at head");
            *sub = sub.resubscribe();
            IngestStep::Lagged
        }
        Err(RecvError::Closed) => IngestStep::Closed,
    }
}

/// Run the alarm ingest loop to completion.
///
/// Drains engine alarm transitions into `store` until the engine is gone. This
/// is the long-lived task the control plane spawns at startup; it owns one
/// engine subscription and the shared alarm store. It can never block the engine
/// (it only reads the drop-oldest broadcast and lagged-skips).
pub async fn run_alarm_ingest(mut sub: EventSubscription<Event>, store: Arc<dyn AlarmRepository>) {
    loop {
        match ingest_step(&mut sub, store.as_ref()).await {
            IngestStep::Closed => break,
            IngestStep::Applied(_) | IngestStep::Skipped | IngestStep::Lagged => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
    use multiview_core::time::MediaTime;
    use multiview_engine::EnginePublisher;
    use multiview_events::{AlarmTransition, Alert, AlertSeverity, Event};

    use super::{alarm_transition, ingest_step, IngestStep};
    use crate::alarm_store::{AlarmRepository, InMemoryAlarmStore};
    use crate::notify::AlarmTransitionKind;

    type Publisher = EnginePublisher<serde_json::Value, Event>;

    fn record(id: &str, severity: PerceivedSeverity) -> AlarmRecord {
        AlarmRecord::new(
            AlarmId::new(id),
            AlarmKind::Black,
            severity,
            AlarmScope::Tile { index: 0 },
            MediaTime::from_nanos(1),
        )
    }

    #[test]
    fn classifier_maps_each_alarm_event_and_ignores_others() {
        let r = record("x", PerceivedSeverity::Major);
        assert!(matches!(
            alarm_transition(&Event::AlarmRaised(AlarmTransition::new(r.clone()))),
            Some((AlarmTransitionKind::Raised, _))
        ));
        assert!(matches!(
            alarm_transition(&Event::AlarmUpdated(AlarmTransition::new(r.clone()))),
            Some((AlarmTransitionKind::Updated, _))
        ));
        assert!(matches!(
            alarm_transition(&Event::AlarmCleared(AlarmTransition::new(r.clone()))),
            Some((AlarmTransitionKind::Cleared, _))
        ));
        assert!(matches!(
            alarm_transition(&Event::AlarmAcked(AlarmTransition::new(r))),
            Some((AlarmTransitionKind::Acked, _))
        ));
        // A non-alarm event is not a transition.
        let alert = Event::AlertRaised(Alert {
            key: "k".to_owned(),
            severity: AlertSeverity::Warning,
            title: "t".to_owned(),
            detail: None,
            active: true,
        });
        assert!(alarm_transition(&alert).is_none());
    }

    #[tokio::test]
    async fn ingest_step_applies_alarm_transitions_to_the_store() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let store = InMemoryAlarmStore::new();

        engine.publish_event(Event::AlarmRaised(AlarmTransition::new(record(
            "a1",
            PerceivedSeverity::Major,
        ))));

        let step = ingest_step(&mut sub, &store).await;
        assert_eq!(step, IngestStep::Applied(AlarmTransitionKind::Raised));
        let stored = store.get(&AlarmId::new("a1")).unwrap();
        assert_eq!(stored.record.severity, PerceivedSeverity::Major);
    }

    #[tokio::test]
    async fn ingest_step_skips_non_alarm_events() {
        let engine: Publisher = EnginePublisher::new(64);
        let mut sub = engine.subscribe();
        let store = InMemoryAlarmStore::new();

        engine.publish_event(Event::AlertRaised(Alert {
            key: "k".to_owned(),
            severity: AlertSeverity::Info,
            title: "t".to_owned(),
            detail: None,
            active: true,
        }));

        assert_eq!(ingest_step(&mut sub, &store).await, IngestStep::Skipped);
        assert!(store
            .list(&crate::alarm_store::AlarmFilter::default())
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn ingest_lagged_skip_never_back_pressures_the_engine() {
        // A tiny ring; the engine publishes far more than capacity while ingest
        // never drains. Each publish must return promptly; ingest recovers via
        // resubscribe (lagged-skip), never forcing the engine to wait
        // (invariant #10).
        let engine: Publisher = EnginePublisher::new(4);
        let mut sub = engine.subscribe();
        let store = InMemoryAlarmStore::new();

        for i in 0..1000 {
            let seq = engine.publish_event(Event::AlarmRaised(AlarmTransition::new(record(
                &format!("a{i}"),
                PerceivedSeverity::Major,
            ))));
            assert_eq!(seq, u64::try_from(i + 1).unwrap());
        }

        // The far-behind ingest observes the overflow and resubscribes rather
        // than erroring or hanging. A timeout guards against a regression that
        // would block here.
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ingest_step(&mut sub, &store),
        )
        .await
        .expect("lagged recovery must not block");
        assert_eq!(step, IngestStep::Lagged);

        // After recovery the next published alarm is applied cleanly.
        engine.publish_event(Event::AlarmRaised(AlarmTransition::new(record(
            "after",
            PerceivedSeverity::Minor,
        ))));
        let step = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ingest_step(&mut sub, &store),
        )
        .await
        .expect("post-recovery delivery must not block");
        assert_eq!(step, IngestStep::Applied(AlarmTransitionKind::Raised));
        assert!(store.get(&AlarmId::new("after")).is_ok());
    }

    #[tokio::test]
    async fn ingest_step_reports_closed_when_engine_is_gone() {
        let engine: Publisher = EnginePublisher::new(8);
        let mut sub = engine.subscribe();
        let store = InMemoryAlarmStore::new();
        drop(engine);
        assert_eq!(ingest_step(&mut sub, &store).await, IngestStep::Closed);
    }
}
