//! Operator test-injection types (ADR-0183 item 5): a narrow, clearly-labelled
//! synthetic-evidence surface so an operator can see the overlay and armed
//! automation work end-to-end without waiting for a live ad break. These types
//! can only submit through [`crate::source_event_service::SourceEventRegistry::inject_test_observation`],
//! which requires an already-active real source generation — this module
//! cannot allocate a new source identity or epoch, and it can never claim a
//! reliable production boundary: every injected observation is tagged with the
//! `EventOrigin::External` origin and the `TEST` event-id prefix so it is
//! unmistakably distinguishable from real evidence in the API/UI.
use multiview_core::source_event::{
    EventBoundary, EventOrigin, SourceEventKey, SourceEventObservation,
};

/// The caller-selected synthetic lifecycle boundary to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestBoundary {
    /// Announce/confirm a synthetic break start, optionally with a duration.
    Start {
        /// Declared duration in nanoseconds, if the operator supplied one.
        duration_ns: Option<i64>,
    },
    /// Confirm a synthetic break end for the most recent injected occurrence.
    End,
}
impl TestBoundary {
    /// Build the observation. The occurrence identity is fixed per source
    /// (`test:break`) so a `Start` then `End` call targets the SAME tracked
    /// occurrence (matching the real tracker's per-key boundary model) rather
    /// than leaking a new identity per call.
    #[must_use]
    pub fn into_observation(self, source_id: String, epoch: u64) -> SourceEventObservation {
        let boundary = match self {
            Self::Start { duration_ns } => EventBoundary::Start {
                pts_ns: None,
                duration_ns,
            },
            Self::End => EventBoundary::End { pts_ns: None },
        };
        SourceEventObservation {
            key: SourceEventKey {
                source_id,
                epoch,
                origin: EventOrigin::External,
                event_id: "test:break".to_owned(),
            },
            // Each boundary kind (`Start`/`End`) dedups on its own revision
            // slot, so a fresh `1` is valid for either — a repeated call with
            // the same boundary kind is accepted as a dedup-equal duplicate
            // rather than a stale/conflicting revision.
            revision: 1,
            boundary,
            correlation_id: None,
            asset_id: None,
            descriptor: None,
        }
    }
}
