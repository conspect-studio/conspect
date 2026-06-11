//! Tally arbiter + profile + GPIO tests (ADR-MV001): conflict resolution and the
//! bit↔colour / index↔tile mapping are deterministic and correct, the anti-glitch
//! latch holds and expires, and the GPI/GPO model detects edges — all pure value
//! machines over an injected `MediaTime`, never blocking.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use multiview_core::time::MediaTime;
use multiview_engine::tally::arbiter::{ConflictPolicy, LatchPolicy, TallyArbiter, TallyFact};
use multiview_engine::tally::gpio::{Edge, GpiPoint, GpoPoint, Polarity};
use multiview_engine::tally::profile::{BitMapping, TallyProfile};
use proptest::prelude::*;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

fn red() -> TallyState {
    TallyState {
        color: TallyColor::Red,
        brightness: Brightness::FULL,
        source: BusSource::Program,
    }
}

fn green() -> TallyState {
    TallyState {
        color: TallyColor::Green,
        brightness: Brightness::FULL,
        source: BusSource::Preview,
    }
}

fn amber() -> TallyState {
    TallyState {
        color: TallyColor::Amber,
        brightness: Brightness::FULL,
        source: BusSource::Aux { index: 0 },
    }
}

// ---------- profile: bit↔colour + index↔tile ----------

#[test]
fn profile_resolves_highest_priority_lit_bit() {
    let profile = TallyProfile::new()
        .with_bit(BitMapping::new(0, TallyColor::Red, BusSource::Program, 10))
        .with_bit(BitMapping::new(1, TallyColor::Green, BusSource::Preview, 5));

    // Both lit: priority 10 (red/PGM) wins over priority 5 (green/PVW).
    let resolved = profile.resolve_bits([0, 1]).unwrap();
    assert_eq!(resolved.color, TallyColor::Red);
    assert_eq!(resolved.source, BusSource::Program);

    // Only the green bit set.
    let resolved = profile.resolve_bits([1]).unwrap();
    assert_eq!(resolved.color, TallyColor::Green);

    // No mapped bit set.
    assert!(profile.resolve_bits([7]).is_none());
}

#[test]
fn profile_off_color_bit_is_not_lit() {
    let profile =
        TallyProfile::new().with_bit(BitMapping::new(0, TallyColor::Off, BusSource::Program, 99));
    // An Off mapping never lights, even at max priority.
    assert!(profile.resolve_bits([0]).is_none());
}

#[test]
fn profile_index_mapping_identity_and_remap_and_strict() {
    // Default: identity for unmapped indices.
    let profile = TallyProfile::new().with_index(2, 7);
    assert_eq!(profile.tile_for(2), Some(7));
    assert_eq!(profile.tile_for(5), Some(5)); // identity

    // Strict: unmapped indices resolve to None.
    let strict = TallyProfile::new().with_index(2, 7).strict_index();
    assert_eq!(strict.tile_for(2), Some(7));
    assert_eq!(strict.tile_for(5), None);
}

#[test]
fn profile_brightness_is_stamped() {
    let profile = TallyProfile::new()
        .with_brightness(Brightness::DIM)
        .with_bit(BitMapping::new(0, TallyColor::Red, BusSource::Program, 1));
    let resolved = profile.resolve_bits([0]).unwrap();
    assert_eq!(resolved.brightness, Brightness::DIM);
}

#[test]
fn fact_from_bits_maps_index_and_bits() {
    let profile = TallyProfile::new()
        .with_index(3, 9)
        .with_bit(BitMapping::new(0, TallyColor::Red, BusSource::Program, 1));
    let fact = TallyFact::from_bits(&profile, 3, [0], 5).unwrap();
    assert_eq!(fact.tile, 9);
    assert_eq!(fact.state.color, TallyColor::Red);
    assert_eq!(fact.priority, 5);

    // Strict profile with an unmapped index yields no fact.
    let strict = profile.clone().strict_index();
    assert!(TallyFact::from_bits(&strict, 42, [0], 5).is_none());
}

// ---------- arbiter: conflict resolution ----------

#[test]
fn arbiter_priority_policy_higher_priority_wins() {
    let mut arb = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
    let out = arb.resolve(
        [
            TallyFact::new(0, green(), 1),
            TallyFact::new(0, red(), 9), // higher priority
        ],
        ms(0),
    );
    assert_eq!(out[&0].color, TallyColor::Red);
}

#[test]
fn arbiter_priority_ties_break_by_color_urgency() {
    let mut arb = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
    // Equal priority: red (more urgent) beats green regardless of fact order.
    let out = arb.resolve(
        [TallyFact::new(0, green(), 5), TallyFact::new(0, red(), 5)],
        ms(0),
    );
    assert_eq!(out[&0].color, TallyColor::Red);

    let out = arb.resolve(
        [TallyFact::new(0, red(), 5), TallyFact::new(0, green(), 5)],
        ms(0),
    );
    assert_eq!(out[&0].color, TallyColor::Red);
}

#[test]
fn arbiter_color_urgency_policy_ignores_priority() {
    let mut arb = TallyArbiter::new(ConflictPolicy::ColorUrgency, LatchPolicy::None);
    // Green at priority 99 still loses to red at priority 0 (colour wins).
    let out = arb.resolve(
        [TallyFact::new(0, green(), 99), TallyFact::new(0, red(), 0)],
        ms(0),
    );
    assert_eq!(out[&0].color, TallyColor::Red);
}

#[test]
fn arbiter_resolves_each_tile_independently() {
    let mut arb = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
    let out = arb.resolve(
        [
            TallyFact::new(0, red(), 1),
            TallyFact::new(1, green(), 1),
            TallyFact::new(2, amber(), 1),
        ],
        ms(0),
    );
    assert_eq!(out[&0].color, TallyColor::Red);
    assert_eq!(out[&1].color, TallyColor::Green);
    assert_eq!(out[&2].color, TallyColor::Amber);
}

// ---------- arbiter: latch (anti-glitch) ----------

#[test]
fn arbiter_latch_holds_then_expires() {
    let mut arb = TallyArbiter::new(
        ConflictPolicy::Priority,
        LatchPolicy::Hold { hold: ms(100) },
    );

    // t=0: red asserted for tile 0.
    let out = arb.resolve([TallyFact::new(0, red(), 1)], ms(0));
    assert_eq!(out[&0].color, TallyColor::Red);

    // t=50: facts vanish — within the hold window, red is held.
    let out = arb.resolve([], ms(50));
    assert_eq!(out[&0].color, TallyColor::Red);

    // t=150: hold window (100) elapsed since last seen (t=0) -> dropped.
    let out = arb.resolve([], ms(150));
    assert!(!out.contains_key(&0));
}

#[test]
fn arbiter_latch_refreshes_on_reassert() {
    let mut arb = TallyArbiter::new(
        ConflictPolicy::Priority,
        LatchPolicy::Hold { hold: ms(100) },
    );
    arb.resolve([TallyFact::new(0, red(), 1)], ms(0));
    // Reassert at t=90: refreshes last_seen.
    arb.resolve([TallyFact::new(0, red(), 1)], ms(90));
    // t=150: 60ns since refresh < 100 -> still held.
    let out = arb.resolve([], ms(150));
    assert_eq!(out[&0].color, TallyColor::Red);
}

#[test]
fn arbiter_no_latch_drops_immediately() {
    let mut arb = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
    arb.resolve([TallyFact::new(0, red(), 1)], ms(0));
    let out = arb.resolve([], ms(1));
    assert!(out.is_empty());
}

// ---------- GPIO ----------

#[test]
fn gpi_detects_edges_with_polarity() {
    let mut p = GpiPoint::new(Polarity::ActiveHigh);
    assert_eq!(p.sample(false), Edge::None); // steady low
    assert_eq!(p.sample(true), Edge::Rising); // low->high
    assert_eq!(p.sample(true), Edge::None); // steady high
    assert_eq!(p.sample(false), Edge::Falling); // high->low
    assert!(!p.is_active());

    // Active-low inverts: a raw low is logically active.
    let mut q = GpiPoint::new(Polarity::ActiveLow);
    assert_eq!(q.sample(true), Edge::None); // raw high = inactive
    assert_eq!(q.sample(false), Edge::Rising); // raw low = active
    assert!(q.is_active());
}

#[test]
fn gpo_request_honours_polarity() {
    let mut p = GpoPoint::new(Polarity::ActiveHigh);
    assert!(!p.raw_high());
    assert!(p.request(true)); // active-high: raw high
    assert!(p.is_active());

    let mut q = GpoPoint::new(Polarity::ActiveLow);
    // active-low: logical inactive => raw high
    assert!(q.raw_high());
    assert!(!q.request(true)); // logical active => raw low
    assert!(q.is_active());
}

// ---------- property tests ----------

prop_compose! {
    fn arb_color()(c in 0u8..4) -> TallyColor {
        TallyColor::from_tsl_code(c).unwrap_or(TallyColor::Off)
    }
}

prop_compose! {
    fn arb_fact(tile: u32)(color in arb_color(), priority in 0u8..20) -> TallyFact {
        let state = TallyState { color, brightness: Brightness::FULL, source: BusSource::Program };
        TallyFact::new(tile, state, priority)
    }
}

fn color_rank(c: TallyColor) -> u8 {
    match c {
        TallyColor::Off => 0,
        TallyColor::Amber => 2,
        TallyColor::Red => 3,
        // Green and any future custom colour rank as least-urgent-lit, matching
        // the arbiter's `color_rank`.
        _ => 1,
    }
}

proptest! {
    /// Under the priority policy the winner is the fact that is maximal under the
    /// lexicographic `(priority, colour-rank)` order: the winning state must be the
    /// state of a fact whose key equals the global maximum key. (A fact's priority
    /// is not carried in `TallyState`, so we check that *some* fact with the
    /// winning state attains the max key — this correctly handles duplicate states
    /// at different priorities.)
    #[test]
    fn prop_priority_winner_is_maximal(facts in prop::collection::vec(arb_fact(0), 1..12)) {
        let mut arb = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
        let out = arb.resolve(facts.clone(), ms(0));
        let winner = out[&0];

        let max_key = facts.iter()
            .map(|f| (f.priority, color_rank(f.state.color)))
            .max()
            .unwrap();
        // Some fact carrying the winning state must attain the global max key.
        let winner_attains_max = facts.iter().any(|f| {
            f.state == winner && (f.priority, color_rank(f.state.color)) == max_key
        });
        prop_assert!(
            winner_attains_max,
            "winner {:?} does not attain max key {max_key:?}",
            winner.color
        );
    }

    /// Under the colour-urgency policy, the winner has the maximal colour rank.
    #[test]
    fn prop_color_urgency_winner_is_most_urgent(facts in prop::collection::vec(arb_fact(0), 1..12)) {
        let mut arb = TallyArbiter::new(ConflictPolicy::ColorUrgency, LatchPolicy::None);
        let out = arb.resolve(facts.clone(), ms(0));
        let winner = out[&0];
        let winner_rank = color_rank(winner.color);
        let max_rank = facts.iter().map(|f| color_rank(f.state.color)).max().unwrap();
        prop_assert_eq!(winner_rank, max_rank);
    }

    /// Resolution's winning **colour** is order-independent for a single tile:
    /// rotating the fact list yields the same winning colour (the `(priority,
    /// colour-rank)` maximum is order-invariant; the source tiebreak between two
    /// otherwise-identical-rank facts is not pinned, so we assert colour only).
    #[test]
    fn prop_resolution_color_is_order_independent(
        mut facts in prop::collection::vec(arb_fact(0), 1..10),
        seed in any::<u64>(),
    ) {
        let mut arb_a = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
        let out_a = arb_a.resolve(facts.clone(), ms(0));

        // Deterministic rotation by seed.
        let n = u64::try_from(facts.len()).unwrap_or(1).max(1);
        let rot = usize::try_from(seed % n).unwrap_or(0);
        facts.rotate_left(rot);

        let mut arb_b = TallyArbiter::new(ConflictPolicy::Priority, LatchPolicy::None);
        let out_b = arb_b.resolve(facts, ms(0));

        prop_assert_eq!(out_a[&0].color, out_b[&0].color);
    }

    /// A GPI point's stored active level always equals the polarity applied to the
    /// last raw sample, and an edge is reported iff the active level changed.
    #[test]
    fn prop_gpi_edge_matches_level_change(samples in prop::collection::vec(any::<bool>(), 1..40)) {
        for pol in [Polarity::ActiveHigh, Polarity::ActiveLow] {
            let mut p = GpiPoint::new(pol);
            let mut prev = false;
            for &raw in &samples {
                let expected_active = pol.apply(raw);
                let edge = p.sample(raw);
                prop_assert_eq!(p.is_active(), expected_active);
                let changed = expected_active != prev;
                prop_assert_eq!(edge.is_transition(), changed);
                if changed {
                    prop_assert_eq!(edge.is_rising(), expected_active);
                }
                prev = expected_active;
            }
        }
    }
}
