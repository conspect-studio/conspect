//! The tally **arbiter**: aggregate many tally sources (PGM/PVW/ME/aux/router/
//! GPI/IS-07) into one resolved per-tile [`TallyState`] under a defined
//! conflict-resolution policy (broadcast-multiviewer brief §2, ADR-MV001).
//!
//! A facility tallies the *same* element from several independent buses at once:
//! a production switcher's PGM and PVW, a router crosspoint, a GPI closure, an
//! IS-07 event. They disagree — PGM says red, an ISO bus says amber. The arbiter
//! is the pure value machine that, given the set of [`TallyFact`]s asserted for a
//! tile this tick, picks the **one** winning [`TallyState`] under a configured
//! [`ConflictPolicy`], optionally **latching** the last lit state so a momentary
//! glitch does not blink the lamp.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Like the rest of the M11 control surface, the arbiter is a pure
//! classify-style machine over an injected [`MediaTime`] (mirroring
//! [`multiview_framestore::state`] and the alarm engine). [`TallyArbiter::resolve`]
//! **returns** the resolved per-tile states; it never reaches into the engine,
//! never sends on a channel and never blocks. The engine samples it on its slow
//! control tick and renders the result via `multiview-overlay` at a frame boundary —
//! it can therefore never stall the output clock or back-pressure anything.
use std::collections::BTreeMap;

use multiview_core::tally::{TallyColor, TallyState};
use multiview_core::time::MediaTime;

use super::profile::TallyProfile;

/// One tally assertion from a single source for a single multiview tile.
///
/// The arbiter consumes a flat list of these per tick; each carries the resolved
/// [`state`](TallyFact::state) (already mapped through a [`TallyProfile`] or
/// supplied directly by a bus driver), the destination [`tile`](TallyFact::tile),
/// and a [`priority`](TallyFact::priority) used by the priority conflict policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TallyFact {
    /// The multiview tile this assertion targets.
    pub tile: u32,
    /// The tally state this source asserts for the tile.
    pub state: TallyState,
    /// Source priority for the [`ConflictPolicy::Priority`] policy (higher wins).
    pub priority: u8,
}

impl TallyFact {
    /// Construct a fact asserting `state` for `tile` at the given `priority`.
    #[must_use]
    pub const fn new(tile: u32, state: TallyState, priority: u8) -> Self {
        Self {
            tile,
            state,
            priority,
        }
    }

    /// Build a fact by resolving `set_bits` through `profile` for the external
    /// element `index`, mapping the index to a tile and the bits to a colour.
    ///
    /// Returns [`None`] when the profile maps the index to no tile, or when no
    /// mapped bit is lit (nothing to assert).
    #[must_use]
    pub fn from_bits<I: IntoIterator<Item = u8>>(
        profile: &TallyProfile,
        index: u32,
        set_bits: I,
        priority: u8,
    ) -> Option<Self> {
        let tile = profile.tile_for(index)?;
        let state = profile.resolve_bits(set_bits)?;
        Some(Self::new(tile, state, priority))
    }
}

/// How the arbiter resolves several simultaneous [`TallyFact`]s for one tile.
///
/// Serialised **tagged** by variant name (repo convention — never `untagged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ConflictPolicy {
    /// The fact with the highest [`priority`](TallyFact::priority) wins; ties are
    /// broken by colour urgency (red > amber > green > off) so the resolution is
    /// deterministic. The default.
    #[default]
    Priority,
    /// The most urgent **colour** wins regardless of source priority
    /// (red > amber > green > off). Models "any bus that says on-air lights red".
    ColorUrgency,
}

/// Colour urgency rank used to break ties / drive the colour-urgency policy.
///
/// Red (on-air) is the most urgent, then amber (third state), then green
/// (preview), then off. Higher rank = more urgent.
const fn color_rank(color: TallyColor) -> u8 {
    match color {
        TallyColor::Off => 0,
        TallyColor::Green => 1,
        TallyColor::Amber => 2,
        TallyColor::Red => 3,
        // `TallyColor` is `#[non_exhaustive]`; a future operator-custom colour has
        // no defined urgency yet, so it deliberately shares green's
        // least-urgent-*lit* rank (1) — it can never spuriously override a known
        // red/amber until it is given an explicit rank here.
        // reason: the shared body with `Green` is intentional, not a copy-paste bug.
        #[allow(clippy::match_same_arms)]
        _ => 1,
    }
}

/// How long a resolved lit state is **held** after its asserting facts vanish,
/// to ride out a momentary bus glitch.
///
/// [`LatchPolicy::None`] holds nothing (the lamp follows the facts exactly);
/// [`LatchPolicy::Hold`] keeps the last lit [`TallyState`] for `hold` after the
/// last fact for that tile disappears, then falls back to off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LatchPolicy {
    /// No latch: the resolved state follows the facts exactly.
    #[default]
    None,
    /// Hold the last lit state for this long after its facts vanish.
    Hold {
        /// The hold window on the media timeline.
        hold: MediaTime,
    },
}

/// The held state for one tile under [`LatchPolicy::Hold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    state: TallyState,
    /// When the held state was last reasserted by a live fact.
    last_seen: MediaTime,
}

/// The tally arbiter: resolve a per-tick set of [`TallyFact`]s into one
/// [`TallyState`] per tile under a [`ConflictPolicy`] and [`LatchPolicy`].
///
/// Construct with [`TallyArbiter::new`], then call [`TallyArbiter::resolve`] once
/// per control tick with the facts asserted this tick and the current
/// [`MediaTime`]. The arbiter carries only the latch memory between ticks; it is
/// `Clone` for lock-free snapshotting.
#[derive(Debug, Clone, Default)]
pub struct TallyArbiter {
    conflict: ConflictPolicy,
    latch: LatchPolicy,
    /// Per-tile held state for the latch policy.
    held: BTreeMap<u32, Held>,
}

impl TallyArbiter {
    /// Construct an arbiter with the given conflict and latch policies.
    #[must_use]
    pub fn new(conflict: ConflictPolicy, latch: LatchPolicy) -> Self {
        Self {
            conflict,
            latch,
            held: BTreeMap::new(),
        }
    }

    /// The active conflict policy.
    #[must_use]
    pub const fn conflict_policy(&self) -> ConflictPolicy {
        self.conflict
    }

    /// The active latch policy.
    #[must_use]
    pub const fn latch_policy(&self) -> LatchPolicy {
        self.latch
    }

    /// Resolve every tile's tally from this tick's `facts` at media time `now`.
    ///
    /// For each distinct tile referenced by a fact, the winning [`TallyState`] is
    /// chosen under the [`ConflictPolicy`]. The [`LatchPolicy`] then folds in any
    /// still-valid held state: a tile with a live lit winner refreshes its hold;
    /// a tile with no live facts keeps its held state until the hold window
    /// expires. The result is a deterministic, sorted `tile → state` map.
    ///
    /// Pure and total: a non-monotonic `now` cannot prematurely expire a hold
    /// (elapsed is clamped to zero).
    pub fn resolve<I>(&mut self, facts: I, now: MediaTime) -> BTreeMap<u32, TallyState>
    where
        I: IntoIterator<Item = TallyFact>,
    {
        // Group facts by tile, folding the per-tile winner as we go. The running
        // winner is kept as the whole `TallyFact` so its source priority survives
        // the fold (a `TallyState` alone does not carry priority).
        let mut winners: BTreeMap<u32, TallyFact> = BTreeMap::new();
        for fact in facts {
            let winner = match winners.get(&fact.tile).copied() {
                None => fact,
                Some(current) => self.pick(current, fact),
            };
            winners.insert(fact.tile, winner);
        }
        let winners: BTreeMap<u32, TallyState> =
            winners.into_iter().map(|(t, f)| (t, f.state)).collect();

        match self.latch {
            LatchPolicy::None => {
                self.held.clear();
                winners
            }
            LatchPolicy::Hold { hold } => self.apply_hold(winners, hold, now),
        }
    }

    /// Resolve a per-tile winner between the running best fact and a new fact.
    ///
    /// Under [`ConflictPolicy::Priority`] the higher source priority wins, ties
    /// broken by colour urgency (red > amber > green > off). Under
    /// [`ConflictPolicy::ColorUrgency`] the more urgent colour wins regardless of
    /// priority, ties broken by the higher priority. Both are total and
    /// deterministic (a tie keeps the running winner, so fold order is stable for
    /// genuinely equal facts).
    fn pick(&self, current: TallyFact, fact: TallyFact) -> TallyFact {
        let cur_rank = color_rank(current.state.color);
        let new_rank = color_rank(fact.state.color);
        let new_wins = match self.conflict {
            ConflictPolicy::Priority => {
                fact.priority > current.priority
                    || (fact.priority == current.priority && new_rank > cur_rank)
            }
            ConflictPolicy::ColorUrgency => {
                new_rank > cur_rank || (new_rank == cur_rank && fact.priority > current.priority)
            }
        };
        if new_wins {
            fact
        } else {
            current
        }
    }

    /// Apply the hold latch: refresh held entries from live lit winners, keep
    /// unexpired held states for tiles with no live winner, and drop expired ones.
    fn apply_hold(
        &mut self,
        winners: BTreeMap<u32, TallyState>,
        hold: MediaTime,
        now: MediaTime,
    ) -> BTreeMap<u32, TallyState> {
        // Refresh / install held state from this tick's lit winners.
        for (&tile, &state) in &winners {
            if state.color.is_lit() {
                self.held.insert(
                    tile,
                    Held {
                        state,
                        last_seen: now,
                    },
                );
            }
        }

        // Expire held entries whose hold window has elapsed.
        let hold_ns = hold.as_nanos().max(0);
        self.held.retain(|_, h| {
            let elapsed = now.saturating_sub(h.last_seen).as_nanos().max(0);
            elapsed < hold_ns
        });

        // Build the result: every live winner, plus any held tile not already
        // present with a live lit winner.
        let mut out = winners;
        for (&tile, held) in &self.held {
            out.entry(tile).or_insert(held.state);
        }
        out
    }
}
