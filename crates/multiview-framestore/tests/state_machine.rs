#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Property and stateful tests for the per-tile failure-ladder state machine.
//!
//! Two layers of testing:
//!  * `classify_*` — direct property tests of the pure transition function
//!    (`classify`) against a hand-written reference oracle and its boundary
//!    semantics.
//!  * the `proptest_state_machine!` block — drives a real `TileStore` with
//!    randomized, shrinkable `Publish`/`Tick` command sequences and asserts the
//!    SUT's observed state and `read` outcome match an independent reference
//!    model after every transition.

use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_framestore::{classify, NoSignalPolicy, TileRead, TileStore, TileThresholds};

use proptest::prelude::*;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

/// Fixed thresholds used across the stateful test (hold 100, stale 500,
/// nosignal 2000 ns) — small integers keep the search space dense around the
/// boundaries.
const HOLD_NS: i64 = 100;
const STALE_NS: i64 = 500;
const NOSIGNAL_NS: i64 = 2_000;

fn fixed_thresholds() -> TileThresholds {
    TileThresholds::new(
        MediaTime::from_nanos(HOLD_NS),
        MediaTime::from_nanos(STALE_NS),
        MediaTime::from_nanos(NOSIGNAL_NS),
    )
    .unwrap()
}

/// Independent reference oracle for `classify`, written in the simplest
/// possible form so a divergence flags a real bug.
fn oracle(elapsed_ns: i64, hold: i64, stale: i64, nosignal: i64) -> SourceState {
    let e = elapsed_ns.max(0);
    if e < hold {
        SourceState::Live
    } else if e < stale {
        SourceState::Stale
    } else if e < nosignal {
        SourceState::Reconnecting
    } else {
        SourceState::NoSignal
    }
}

proptest! {
    /// `classify` matches the independent oracle for any elapsed time and any
    /// valid threshold triple.
    #[test]
    fn classify_matches_oracle(
        elapsed in -1_000i64..1_000_000,
        hold in 1i64..1_000,
        stale_gap in 1i64..1_000,
        nosignal_gap in 1i64..1_000,
    ) {
        let stale = hold + stale_gap;
        let nosignal = stale + nosignal_gap;
        let t = TileThresholds::new(
            MediaTime::from_nanos(hold),
            MediaTime::from_nanos(stale),
            MediaTime::from_nanos(nosignal),
        ).unwrap();
        let got = classify(MediaTime::from_nanos(elapsed), t);
        prop_assert_eq!(got, oracle(elapsed, hold, stale, nosignal));
    }

    /// The ladder is monotone in elapsed time: as time only ever increases, the
    /// state can only degrade (Live -> Stale -> Reconnecting -> NoSignal) and
    /// never recover without a fresh frame.
    #[test]
    fn classify_is_monotone_in_elapsed(
        a in 0i64..5_000,
        b in 0i64..5_000,
    ) {
        let t = fixed_thresholds();
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let rank = |s: SourceState| match s {
            SourceState::Live => 0,
            SourceState::Stale => 1,
            SourceState::Reconnecting => 2,
            SourceState::NoSignal => 3,
            _ => 4,
        };
        let s_lo = classify(MediaTime::from_nanos(lo), t);
        let s_hi = classify(MediaTime::from_nanos(hi), t);
        prop_assert!(rank(s_lo) <= rank(s_hi),
            "more elapsed time must not improve the state: {:?}@{} vs {:?}@{}",
            s_lo, lo, s_hi, hi);
    }
}

/// Exact boundary behaviour (inclusive lower edges of each degraded state).
#[test]
fn classify_boundaries_are_inclusive_at_lower_edge() {
    let t = fixed_thresholds();
    assert_eq!(
        classify(MediaTime::from_nanos(HOLD_NS - 1), t),
        SourceState::Live
    );
    assert_eq!(
        classify(MediaTime::from_nanos(HOLD_NS), t),
        SourceState::Stale
    );
    assert_eq!(
        classify(MediaTime::from_nanos(STALE_NS - 1), t),
        SourceState::Stale
    );
    assert_eq!(
        classify(MediaTime::from_nanos(STALE_NS), t),
        SourceState::Reconnecting
    );
    assert_eq!(
        classify(MediaTime::from_nanos(NOSIGNAL_NS - 1), t),
        SourceState::Reconnecting
    );
    assert_eq!(
        classify(MediaTime::from_nanos(NOSIGNAL_NS), t),
        SourceState::NoSignal
    );
}

// ---------------------------------------------------------------------------
// Stateful model: drive a real TileStore with random Publish/Tick sequences.
// ---------------------------------------------------------------------------

/// A transition applied to the tile over time.
#[derive(Clone, Debug)]
enum Cmd {
    /// A fresh frame arrives; `dt` ns elapse first, then the frame is published
    /// at the new `now`. The frame's payload is `value`.
    Publish { dt: u32, value: u32 },
    /// `dt` ns elapse with no frame; the tile is then read/queried at `now`.
    Tick { dt: u32 },
}

/// The reference model: monotonic `now`, the instant of the last fresh frame
/// (and its payload), and whether a frame has ever been published.
#[derive(Clone, Debug)]
struct Model {
    now: i64,
    last_frame_at: Option<i64>,
    last_value: Option<u32>,
}

struct TileRef;

impl ReferenceStateMachine for TileRef {
    type State = Model;
    type Transition = Cmd;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(Model {
            now: 0,
            last_frame_at: None,
            last_value: None,
        })
        .boxed()
    }

    fn transitions(_state: &Self::State) -> BoxedStrategy<Self::Transition> {
        // `dt` is biased toward small values so the search lands repeatedly on
        // the threshold boundaries (100/500/2000 ns).
        prop_oneof![
            (0u32..3_000, any::<u32>()).prop_map(|(dt, value)| Cmd::Publish { dt, value }),
            (0u32..3_000).prop_map(|dt| Cmd::Tick { dt }),
        ]
        .boxed()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match *transition {
            Cmd::Publish { dt, value } => {
                state.now = state.now.saturating_add(i64::from(dt));
                state.last_frame_at = Some(state.now);
                state.last_value = Some(value);
            }
            Cmd::Tick { dt } => {
                state.now = state.now.saturating_add(i64::from(dt));
            }
        }
        state
    }
}

/// Local newtype wrapper so we can implement the foreign `StateMachineTest`
/// trait for the foreign `TileStore` (orphan rule).
struct TileSut;

impl StateMachineTest for TileSut {
    type SystemUnderTest = TileStore<u32>;
    type Reference = TileRef;

    fn init_test(_ref_state: &Model) -> Self::SystemUnderTest {
        TileStore::new("tile-under-test", fixed_thresholds(), NoSignalPolicy::Slate)
    }

    fn apply(
        state: Self::SystemUnderTest,
        ref_state: &Model,
        transition: Cmd,
    ) -> Self::SystemUnderTest {
        match transition {
            Cmd::Publish { value, .. } => {
                // `ref_state` is already advanced to the post-transition `now`,
                // which equals the publish instant.
                state.publish(value, MediaTime::from_nanos(ref_state.now));
            }
            Cmd::Tick { .. } => { /* time passes; nothing published */ }
        }
        state
    }

    fn check_invariants(state: &Self::SystemUnderTest, ref_state: &Model) {
        let now = MediaTime::from_nanos(ref_state.now);

        // 1. The SUT's classified state equals the reference's expectation.
        let expected_state = match ref_state.last_frame_at {
            None => SourceState::NoSignal,
            Some(at) => oracle(ref_state.now - at, HOLD_NS, STALE_NS, NOSIGNAL_NS),
        };
        assert_eq!(
            state.state(now),
            expected_state,
            "state mismatch at now={}, last_frame_at={:?}",
            ref_state.now,
            ref_state.last_frame_at
        );

        // 2. The read outcome is consistent with the state and the policy
        //    (Slate): NoSignal both when nothing was published and when the
        //    no-signal threshold elapsed.
        let read = state.read(now);
        match (ref_state.last_frame_at, expected_state) {
            (None, _) | (Some(_), SourceState::NoSignal) => {
                assert!(
                    matches!(read, TileRead::NoSignal),
                    "expected NoSignal read at now={}, got {:?}",
                    ref_state.now,
                    read.state()
                );
            }
            (Some(_), SourceState::Live) => {
                assert!(matches!(read, TileRead::Fresh { .. }));
                // The held frame is exactly the last published value.
                assert_eq!(read.frame().map(|f| **f), ref_state.last_value);
            }
            (Some(_), SourceState::Stale | SourceState::Reconnecting) => {
                assert!(matches!(read, TileRead::Held { .. }));
                assert_eq!(read.state(), expected_state);
                // The held frame is still the last-good frame.
                assert_eq!(read.frame().map(|f| **f), ref_state.last_value);
            }
            (Some(_), _) => panic!("non-exhaustive SourceState in oracle"),
        }

        // 3. The read's reported state always agrees with `state()` except in
        //    the Slate-NoSignal case (where `read` reports NoSignal because it
        //    stops holding).
        if !matches!(read, TileRead::NoSignal) {
            assert_eq!(read.state(), state.state(now));
        }
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// Randomized Publish/Tick sequences: the real `TileStore` tracks the
    /// reference failure ladder exactly, and reads never disagree with state.
    #[test]
    fn tile_store_follows_reference_ladder(sequential 1..60 => TileSut);
}
