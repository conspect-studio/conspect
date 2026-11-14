//! The atomic multi-crosspoint set handshake (ADR-0110 decision 8,
//! output-routing.md §12.8 "Atomic at one tick", #308).
//!
//! A set is 0..1 mixer patch (applied by the switcher tick driver on the
//! output clock) plus 0..N output moves (applied by the rendition dispatcher
//! on the bake consumer). The two halves never wait for each other: they
//! meet in one [`SetArm`], a cell of atomics written only by
//! `compare_exchange`, so the clock thread never locks, allocates or awaits
//! to take part (invariants #1 and #10).
//!
//! State machine (every arrow is one CAS; the first terminal write wins):
//!
//! ```text
//! Pending ─switcher_ready─▶ SwitcherReady ─propose(T)─▶ Proposed(T) ─begin_apply(T)─▶ Applying(T)
//!    │                         ▲   ▲                       │                            │   │
//!    │                         │   └──────unpropose────────┘                            │   │
//!    │                         └─────────────────abort_apply (rejected frame)───────────┘   │
//!    │                                                                         commit(T)─┘
//!    │                                                                            ▼
//!    └──────refuse (Pending | SwitcherReady | Proposed) ──▶ Refused        Committed
//! ```
//!
//! Either half may refuse until the switcher claims the proposal at `T`
//! (`begin_apply`, on the clock, before it touches the input table). From
//! `Applying` only the switcher moves the cell (commit when the frame
//! composed, back to `SwitcherReady` when it was rejected), so a mixer patch
//! composed at `T` is never contradicted by a late refusal.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// A set not adopted within this many ticks of its admission is refused with
/// its first cause, and nothing changes (output-routing.md §12.8 "Bounds").
/// One constant shared by the switcher and the router.
pub const SET_ADOPT_DEADLINE_TICKS: u64 = 150;

/// Where a set's handshake stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ArmState {
    /// Admitted; the mixer half is not validated/primed yet.
    Pending = 0,
    /// The mixer half validated and its new Program source (if any) is fresh.
    SwitcherReady = 1,
    /// The router proposed adoption tick [`SetArm::tick`].
    Proposed = 2,
    /// The switcher applied the mixer half at [`SetArm::tick`]; the frame's
    /// commit or rejection is outstanding.
    Applying = 3,
    /// The mixer half was composed and committed at [`SetArm::tick`].
    Committed = 4,
    /// Refused with [`SetArm::reason`]; nothing of the set changed.
    Refused = 5,
}

impl ArmState {
    const fn bits(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::SwitcherReady => 1,
            Self::Proposed => 2,
            Self::Applying => 3,
            Self::Committed => 4,
            Self::Refused => 5,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value & STATE_MASK {
            0 => Self::Pending,
            1 => Self::SwitcherReady,
            2 => Self::Proposed,
            3 => Self::Applying,
            4 => Self::Committed,
            _ => Self::Refused,
        }
    }

    /// `Committed` or `Refused`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Refused)
    }
}

/// The typed refusal every member of a refused set reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SetRefusal {
    /// A member's source had no fresh frame in time.
    FeedNotFresh = 1,
    /// A destination encoder did not open or key in time.
    EncoderNotReady = 2,
    /// The set's resource delta exceeds a cap.
    Capacity = 3,
    /// A mixer member re-patches the Program input under `protect`, or the
    /// Program/Preview input during an AUTO.
    OnAirProtected = 4,
    /// A newer take or set replaced this set.
    Superseded = 5,
    /// A mixer member names an undeclared input.
    UnknownSlot = 6,
    /// A mixer member names a source without a frame store.
    UnknownSource = 7,
    /// The set was not adopted within [`SET_ADOPT_DEADLINE_TICKS`].
    Deadline = 8,
    /// Any other validation failure (malformed set, no provider).
    Invalid = 9,
    /// Another set is already armed on the M/E (`engine-busy`).
    Busy = 10,
    /// An output member's destination decoder configuration differs from
    /// what its out-of-band container carries (`format-mismatch`); found
    /// before the adoption tick was proposed.
    FormatMismatch = 11,
}

impl SetRefusal {
    const fn bits(self) -> u8 {
        match self {
            Self::FeedNotFresh => 1,
            Self::EncoderNotReady => 2,
            Self::Capacity => 3,
            Self::OnAirProtected => 4,
            Self::Superseded => 5,
            Self::UnknownSlot => 6,
            Self::UnknownSource => 7,
            Self::Deadline => 8,
            Self::Invalid => 9,
            Self::Busy => 10,
            Self::FormatMismatch => 11,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::FeedNotFresh),
            2 => Some(Self::EncoderNotReady),
            3 => Some(Self::Capacity),
            4 => Some(Self::OnAirProtected),
            5 => Some(Self::Superseded),
            6 => Some(Self::UnknownSlot),
            7 => Some(Self::UnknownSource),
            8 => Some(Self::Deadline),
            9 => Some(Self::Invalid),
            10 => Some(Self::Busy),
            11 => Some(Self::FormatMismatch),
            _ => None,
        }
    }

    /// The wire code (`refused:<code>`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::FeedNotFresh => "feed-not-fresh",
            Self::EncoderNotReady => "encoder-not-ready",
            Self::Capacity => "capacity",
            Self::OnAirProtected => "on-air-protected",
            Self::Superseded => "superseded",
            Self::UnknownSlot => "unknown-slot",
            Self::UnknownSource => "unknown-source",
            Self::Deadline => "deadline",
            Self::Invalid => "invalid",
            Self::Busy => "engine-busy",
            Self::FormatMismatch => "format-mismatch",
        }
    }

    /// Classify a switcher validation error by its typed prefix.
    #[must_use]
    pub fn from_switcher_error(error: &str) -> Self {
        if error.starts_with("on-air-protected") {
            Self::OnAirProtected
        } else if error.starts_with("unknown-slot") {
            Self::UnknownSlot
        } else if error.starts_with("unknown-source") {
            Self::UnknownSource
        } else {
            Self::Invalid
        }
    }
}

/// The shared handshake cell of one set (atomics only, never locked).
///
/// Equality is identity: two handles are equal only when they name the same
/// cell (a set's halves share one `Arc<SetArm>`).
pub struct SetArm {
    /// Low 3 bits: [`ArmState`]; high bits: the [`SetRefusal`] of a
    /// `Refused` cell, so a refusal and its reason land in ONE CAS.
    state: AtomicU8,
    tick: AtomicU64,
}

const STATE_MASK: u8 = 0b111;
const REASON_SHIFT: u8 = 3;

impl std::fmt::Debug for SetArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetArm")
            .field("state", &self.state())
            .field("tick", &self.tick())
            .field("reason", &self.reason())
            .finish()
    }
}

impl PartialEq for SetArm {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for SetArm {}

impl Default for SetArm {
    fn default() -> Self {
        Self::new()
    }
}

impl SetArm {
    /// A fresh `Pending` cell.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(ArmState::Pending.bits()),
            tick: AtomicU64::new(0),
        }
    }

    /// The current state (Acquire: a `Proposed`/`Committed` read sees its tick).
    #[must_use]
    pub fn state(&self) -> ArmState {
        ArmState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// The proposed or committed adoption tick (meaningful in `Proposed` and
    /// `Committed` only).
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick.load(Ordering::Acquire)
    }

    /// The refusal, once `Refused`.
    #[must_use]
    pub fn reason(&self) -> Option<SetRefusal> {
        let raw = self.state.load(Ordering::Acquire);
        if ArmState::from_u8(raw) == ArmState::Refused {
            SetRefusal::from_u8(raw >> REASON_SHIFT)
        } else {
            None
        }
    }

    fn cas(&self, from: ArmState, to: ArmState) -> bool {
        self.state
            .compare_exchange(from.bits(), to.bits(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Switcher: the mixer half validated and is primed (`Pending` →
    /// `SwitcherReady`).
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn switcher_ready(&self) -> bool {
        self.cas(ArmState::Pending, ArmState::SwitcherReady)
    }

    /// Router: propose adoption tick `at` (`SwitcherReady` → `Proposed`).
    /// The tick is written before the state, so a reader that sees
    /// `Proposed` sees `at`.
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn propose(&self, at: u64) -> bool {
        if self.state() != ArmState::SwitcherReady {
            return false;
        }
        self.tick.store(at, Ordering::Release);
        self.cas(ArmState::SwitcherReady, ArmState::Proposed)
    }

    /// Switcher: the proposal cannot be met (missed, repeated or rejected
    /// tick, or the source lost its fresh frame): `Proposed` →
    /// `SwitcherReady`, re-proposable.
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn unpropose(&self) -> bool {
        self.cas(ArmState::Proposed, ArmState::SwitcherReady)
    }

    /// Switcher: claim the proposal on its adoption tick, before applying
    /// the mixer half (`Proposed(at)` → `Applying`). Fails for any other
    /// tick, or when a refusal won first.
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn begin_apply(&self, at: u64) -> bool {
        self.state() == ArmState::Proposed
            && self.tick() == at
            && self.cas(ArmState::Proposed, ArmState::Applying)
    }

    /// Switcher: the frame of `at` composed (`Applying(at)` → `Committed`).
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn commit(&self, at: u64) -> bool {
        self.state() == ArmState::Applying
            && self.tick() == at
            && self.cas(ArmState::Applying, ArmState::Committed)
    }

    /// Switcher: the frame of the adoption tick was rejected and the table
    /// restored (`Applying` → `SwitcherReady`, re-proposable).
    #[must_use = "a lost transition means another writer moved the set first"]
    pub fn abort_apply(&self) -> bool {
        self.cas(ArmState::Applying, ArmState::SwitcherReady)
    }

    /// Refuse from `Pending`, `SwitcherReady` or `Proposed` (either half).
    /// The first terminal write wins: `false` when the set already ended or
    /// the switcher is applying it.
    #[must_use = "a lost refusal means the set already ended or is being applied"]
    pub fn refuse(&self, reason: SetRefusal) -> bool {
        [
            ArmState::Pending,
            ArmState::SwitcherReady,
            ArmState::Proposed,
        ]
        .into_iter()
        .any(|from| self.refuse_from(from, reason))
    }

    fn refuse_from(&self, from: ArmState, reason: SetRefusal) -> bool {
        let to = ArmState::Refused.bits() | (reason.bits() << REASON_SHIFT);
        self.state
            .compare_exchange(from.bits(), to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn the_happy_path_commits_only_at_the_proposed_tick() {
        let arm = SetArm::new();
        assert!(!arm.propose(5), "no proposal before the switcher is ready");
        assert!(arm.switcher_ready());
        assert!(!arm.switcher_ready(), "once");
        assert!(arm.propose(7));
        assert_eq!((arm.state(), arm.tick()), (ArmState::Proposed, 7));
        assert!(!arm.commit(7), "only once applying");
        assert!(!arm.begin_apply(6) && !arm.begin_apply(8), "only at T");
        assert!(arm.begin_apply(7));
        assert!(!arm.refuse(SetRefusal::Superseded), "never while applying");
        assert!(!arm.commit(6));
        assert!(arm.commit(7));
        assert_eq!(arm.state(), ArmState::Committed);
        assert!(!arm.refuse(SetRefusal::Deadline), "terminal");
        assert!(!arm.unpropose());
        assert_eq!(arm.reason(), None);
    }

    #[test]
    fn a_missed_or_rejected_tick_returns_to_ready_and_a_refusal_wins_once() {
        let arm = SetArm::new();
        assert!(arm.switcher_ready() && arm.propose(3));
        assert!(arm.unpropose());
        assert_eq!(arm.state(), ArmState::SwitcherReady);
        assert!(arm.propose(4) && arm.begin_apply(4) && arm.abort_apply());
        assert_eq!(arm.state(), ArmState::SwitcherReady);
        assert_eq!(arm.reason(), None);
        assert!(arm.refuse(SetRefusal::Superseded));
        assert_eq!(arm.reason(), Some(SetRefusal::Superseded));
        assert!(
            !arm.refuse(SetRefusal::Deadline),
            "first terminal write wins"
        );
        assert_eq!(arm.reason(), Some(SetRefusal::Superseded));
    }

    /// Exhaustive over every sequence of up to five operations: the state
    /// only ever moves along the documented arrows, a terminal state never
    /// changes, `Committed` happens only at the proposed tick, and a
    /// `Refused` cell always carries a reason.
    #[test]
    fn every_short_operation_sequence_respects_the_machine() {
        #[derive(Clone, Copy)]
        enum Op {
            Ready,
            Propose(u64),
            Unpropose,
            Begin(u64),
            Commit(u64),
            Abort,
            Refuse,
        }
        let ops = [
            Op::Ready,
            Op::Propose(1),
            Op::Propose(2),
            Op::Unpropose,
            Op::Begin(1),
            Op::Begin(2),
            Op::Commit(1),
            Op::Commit(2),
            Op::Abort,
            Op::Refuse,
        ];
        let n = ops.len();
        let mut checked = 0_u32;
        for len in 1..=5_u32 {
            for mut code in 0..n.pow(len) {
                let arm = SetArm::new();
                for _ in 0..len {
                    let op = ops[code % n];
                    code /= n;
                    let before = arm.state();
                    let before_tick = arm.tick();
                    let ok = match op {
                        Op::Ready => arm.switcher_ready(),
                        Op::Propose(t) => arm.propose(t),
                        Op::Unpropose => arm.unpropose(),
                        Op::Begin(t) => arm.begin_apply(t),
                        Op::Commit(t) => arm.commit(t),
                        Op::Abort => arm.abort_apply(),
                        Op::Refuse => arm.refuse(SetRefusal::FeedNotFresh),
                    };
                    let after = arm.state();
                    if before.is_terminal() {
                        assert!(!ok && after == before, "terminal states never change");
                        continue;
                    }
                    let to_ready = after == ArmState::SwitcherReady
                        && matches!(
                            before,
                            ArmState::Pending | ArmState::Proposed | ArmState::Applying
                        );
                    let to_proposed =
                        (before, after) == (ArmState::SwitcherReady, ArmState::Proposed);
                    let to_refused = after == ArmState::Refused && before != ArmState::Applying;
                    let plain = to_ready || to_proposed || to_refused;
                    let legal = if before == after {
                        !ok
                    } else if plain {
                        ok
                    } else if (before, after) == (ArmState::Proposed, ArmState::Applying) {
                        ok && matches!(op, Op::Begin(t) if t == before_tick)
                    } else if (before, after) == (ArmState::Applying, ArmState::Committed) {
                        ok && matches!(op, Op::Commit(t) if t == before_tick)
                    } else {
                        false
                    };
                    assert!(legal, "{before:?} -> {after:?}");
                    if after == ArmState::Refused {
                        assert!(arm.reason().is_some());
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 30_000);
    }
}
