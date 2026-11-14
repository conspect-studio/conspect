//! Atomic multi-crosspoint sets on the router (ADR-0110 decision 8,
//! output-routing.md §12.8, #308).
//!
//! A [`TakeSet`] is 1..=[`MAX_SET_MEMBERS`] output members (one per output)
//! plus an optional mixer half whose handshake is the shared [`SetArm`] (the
//! caller sends the mixer half to the switcher as
//! `SwitcherVerb::PatchArmed` with the same arm). Members prime, open and
//! warm through the router's existing take machinery but never move alone:
//! each parks in `Phase::Ready`. When every member is ready and the mixer
//! half is `SwitcherReady`, the router picks ONE adoption tick `T`, forces an
//! IDR at `T` on every member key, registers one grouped dispatcher move
//! (decided once for every member at window `T`) and proposes `T` to the
//! switcher. Either everything lands at `T` or every member gets the same
//! refusal. Runs on the bake consumer only; nothing here touches the clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::Arc;

use multiview_engine::patch_set::{ArmState, SetArm, SetRefusal, SET_ADOPT_DEADLINE_TICKS};

use super::{
    AudioChoice, LayoutChange, OpenRequest, OutputRouter, OutputTake, Pending, PendingOpen, Phase,
    PictureBus, RefuseReason, RenditionDispatcher, TakeResult, BOOT_PRINCIPAL, FORCE_FLOOR_TICKS,
    OPEN_DEADLINE_TICKS, OPEN_QUEUE,
};

/// Output members of one set at most (the control plane's 64-crosspoint
/// bound, §12.8).
pub const MAX_SET_MEMBERS: usize = 64;

/// The earliest adoption tick is this many ticks after the router's tick
/// (the proposal must reach the dispatcher before window `T` closes) …
const LEAD_TICKS: u64 = 2;
/// … and this many after the clock's last composed tick (the switcher
/// drains the proposal before composing `T`).
const CLOCK_LEAD_TICKS: u64 = 3;
/// A proposal whose dispatcher decision never arrives is refused this many
/// ticks after `T` (a backstop; the dispatcher decides within its
/// `MAX_PENDING_WINDOWS` windows).
const DECISION_BACKSTOP_TICKS: u64 = 32;

/// One atomic set (already authorized by the control plane). The members'
/// own `operation_id`/`principal` are replaced by the set's.
#[derive(Debug, Clone)]
pub struct TakeSet {
    /// The set's id, echoed in every member's outcome.
    pub operation_id: String,
    /// The principal every reservation is charged to.
    pub principal: String,
    /// One take per distinct output, each with a video bus.
    pub members: Vec<OutputTake>,
    /// The mixer half's handshake (`None`: no mixer member).
    pub arm: Option<Arc<SetArm>>,
}

/// One set in flight.
pub(super) struct ActiveSet {
    id: u64,
    operation_id: String,
    principal: String,
    members: Vec<usize>,
    arm: Option<Arc<SetArm>>,
    admitted: u64,
    /// Encoder opens reserved at admission and not yet issued.
    reserved: usize,
    /// The proposed adoption tick.
    proposed: Option<u64>,
}

/// The router's set state (bounded: a set holds at least one output and a
/// later set or take on an output supersedes it, so at most one set per
/// output is in flight).
#[derive(Default)]
pub(super) struct SetState {
    pub(super) active: Vec<ActiveSet>,
    next_id: u64,
    /// Sets whose mixer half committed but whose outputs were refused.
    pub(super) partial: u64,
    /// The clock's last composed tick (the switcher's applied tick).
    clock: Option<Arc<AtomicU64>>,
}

impl SetState {
    fn get(&self, id: u64) -> Option<&ActiveSet> {
        self.active.iter().find(|s| s.id == id)
    }
}

/// The member outcome of a set refused through its arm.
const fn arm_result(reason: SetRefusal) -> TakeResult {
    match reason {
        SetRefusal::FeedNotFresh | SetRefusal::Deadline => {
            TakeResult::Refused(RefuseReason::FeedNotFresh)
        }
        SetRefusal::EncoderNotReady => TakeResult::Refused(RefuseReason::EncoderNotReady),
        SetRefusal::Capacity | SetRefusal::Busy => TakeResult::Refused(RefuseReason::Capacity),
        SetRefusal::OnAirProtected => TakeResult::Refused(RefuseReason::OnAirProtected),
        SetRefusal::Superseded => TakeResult::Superseded,
        SetRefusal::UnknownSlot | SetRefusal::UnknownSource | SetRefusal::Invalid => {
            TakeResult::Refused(RefuseReason::Unroutable)
        }
    }
}

/// The arm refusal recorded for a router-side member outcome.
const fn arm_reason(result: TakeResult) -> SetRefusal {
    match result {
        TakeResult::Refused(RefuseReason::FeedNotFresh) => SetRefusal::FeedNotFresh,
        TakeResult::Refused(RefuseReason::Capacity) => SetRefusal::Capacity,
        TakeResult::Refused(RefuseReason::OnAirProtected) => SetRefusal::OnAirProtected,
        TakeResult::Refused(RefuseReason::Unroutable) => SetRefusal::Invalid,
        TakeResult::Superseded => SetRefusal::Superseded,
        TakeResult::Refused(RefuseReason::EncoderNotReady | RefuseReason::FormatMismatch)
        | TakeResult::Applied
        | TakeResult::Fallback(_) => SetRefusal::EncoderNotReady,
    }
}

impl OutputRouter {
    /// Hand the router the clock's applied-tick cell (the switcher's
    /// `SwitcherControl::applied_tick_cell`), so a set with a mixer half
    /// picks an adoption tick the clock has not composed yet. Without it the
    /// router uses its own tick only.
    pub(crate) fn set_switcher_clock(&mut self, applied_tick: Arc<AtomicU64>) {
        self.sets.clock = Some(applied_tick);
    }

    /// The set output `index`'s pending take belongs to.
    pub(super) fn group_of(&self, index: usize) -> Option<u64> {
        self.outputs
            .get(index)
            .and_then(|o| o.pending.as_ref())
            .and_then(|p| p.group)
    }

    /// Unissued open reservations of every set, and of `principal`'s.
    pub(super) fn set_reservations(&self, principal: &str) -> (usize, usize) {
        self.sets.active.iter().fold((0, 0), |(all, mine), s| {
            (
                all + s.reserved,
                mine + if s.principal == principal {
                    s.reserved
                } else {
                    0
                },
            )
        })
    }

    /// Refuse a set that never entered the take state machine: one outcome
    /// per member, its arm refused (first terminal write wins).
    fn refuse_unadmitted(&mut self, set: &TakeSet, reason: RefuseReason, tick: u64) {
        if let Some(arm) = set.arm.as_ref() {
            let _ = arm.refuse(arm_reason(TakeResult::Refused(reason)));
        }
        for m in &set.members {
            self.emit(
                set.operation_id.clone(),
                m.output_index,
                tick,
                TakeResult::Refused(reason),
            );
        }
    }

    /// Admit a set at drain: refuse it whole when any member is unroutable
    /// or its new encoders exceed a cap, supersede whatever is pending on
    /// its outputs, reserve its opens and enter every member as a grouped
    /// take.
    #[allow(clippy::too_many_lines)]
    // reason: one linear admission (route checks, supersede, the capacity
    // delta, then install); splitting it scatters the all-or-nothing rule.
    pub(super) fn admit_set(
        &mut self,
        set: TakeSet,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        // Route checks (pure), every member before any change.
        let mut routable = !set.members.is_empty() && set.members.len() <= MAX_SET_MEMBERS;
        for (i, m) in set.members.iter().enumerate() {
            let index = m.output_index;
            let needs_switcher = matches!(m.bus, Some(PictureBus::Preview | PictureBus::Clean));
            let audio_ok =
                m.audio.is_none() || (self.audio.is_some() && dispatcher.has_program_audio(index));
            let stale =
                matches!(&m.bus, Some(PictureBus::Layout(l)) if self.layout_take_is_stale(l));
            routable &= index < self.outputs.len()
                && m.bus.is_some()
                && (!needs_switcher || self.cfg.has_preview)
                && audio_ok
                && !stale
                && !set.members.iter().take(i).any(|o| o.output_index == index);
        }
        if !routable {
            self.refuse_unadmitted(&set, RefuseReason::Unroutable, tick);
            return;
        }
        // A later set supersedes what is pending on its outputs (a whole
        // set included), releasing those reservations before it reserves.
        for m in &set.members {
            self.supersede(m.output_index, tick, dispatcher);
        }
        // Layout members: the §7 canvas admission, as a single take.
        for m in &set.members {
            if let Some(PictureBus::Layout(layout)) = &m.bus {
                self.remember_state(LayoutChange::Edited(layout.clone()));
                let _ = self.refresh_layout_body(layout, tick);
                if !self.admit_aux_canvas(layout, m.output_index, dispatcher) {
                    self.refuse_unadmitted(&set, RefuseReason::Capacity, tick);
                    return;
                }
            }
        }
        // The resource delta: members whose (bus, format, settings) has no
        // live key, no open in flight and no earlier member sharing it.
        let mut opens: Vec<(PictureBus, Option<super::AuxFormat>, usize)> = Vec::new();
        for m in &set.members {
            let index = m.output_index;
            let Some(bus) = m.bus.clone() else { continue };
            let Some(&settings) = self.cfg.base_keys.get(index) else {
                continue;
            };
            if bus == PictureBus::Program || self.outputs.get(index).is_some_and(|o| o.bus == bus) {
                continue;
            }
            let format = self.feed_key(index, &bus).1;
            let live = self.dynamic.iter().any(|d| {
                d.bus == bus
                    && d.format == format
                    && d.settings == settings
                    && !dispatcher.is_key_failed(d.key)
            });
            let in_flight = self
                .opens
                .values()
                .any(|o| o.bus == bus && o.settings == settings);
            let entry = (bus, format, settings);
            if !live && !in_flight && !opens.contains(&entry) {
                opens.push(entry);
            }
        }
        let needed = opens.len();
        let fits = |router: &Self| {
            let (all, mine) = router.set_reservations(&set.principal);
            let total = router.dynamic.len() + router.opens.len() + all;
            let held = router
                .dynamic
                .iter()
                .filter(|d| d.principal == set.principal)
                .count()
                + router
                    .opens
                    .values()
                    .filter(|o| o.principal == set.principal)
                    .count()
                + mine;
            total + needed <= router.cfg.max_encoders
                && held + needed <= router.cfg.max_per_principal
                && router.opens.len() + all + needed <= OPEN_QUEUE
        };
        while needed > 0 && !fits(self) {
            if !self.close_oldest_linger(dispatcher, None) {
                break;
            }
        }
        if !fits(self) {
            self.refuse_unadmitted(&set, RefuseReason::Capacity, tick);
            return;
        }
        let id = self.sets.next_id;
        self.sets.next_id = self.sets.next_id.wrapping_add(1);
        if self.boot.slots.len() < self.outputs.len() {
            self.boot.slots.resize_with(self.outputs.len(), || None);
        }
        let mut members = Vec::with_capacity(set.members.len());
        for mut m in set.members {
            let index = m.output_index;
            // An operator set replaces a boot intent on its outputs.
            self.boot.audio_after.retain(|b| b.output_index != index);
            if let Some(slot) = self.boot.slots.get_mut(index) {
                *slot = None;
            }
            set.operation_id.clone_into(&mut m.operation_id);
            set.principal.clone_into(&mut m.principal);
            if let Some(out) = self.outputs.get_mut(index) {
                out.pending = Some(Pending {
                    take: m,
                    started: tick,
                    phase: Phase::Priming,
                    group: Some(id),
                });
            }
            members.push(index);
        }
        self.sets.active.push(ActiveSet {
            id,
            operation_id: set.operation_id,
            principal: set.principal,
            members,
            arm: set.arm,
            admitted: tick,
            reserved: needed,
            proposed: None,
        });
        self.dirty = true;
    }

    /// A grouped member's feed is fresh: open or reuse its encoder (its
    /// capacity was reserved at admission), never move.
    pub(super) fn member_start(
        &mut self,
        index: usize,
        bus: &PictureBus,
        choice: Option<&AudioChoice>,
        principal: &str,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(&settings) = self.cfg.base_keys.get(index) else {
            return;
        };
        let Some(group) = self.group_of(index) else {
            return;
        };
        let (_, format) = self.feed_key(index, bus);
        if let Some(audio) = self.audio.as_mut() {
            audio.admit(index, bus, choice, tick, dispatcher);
        }
        let existing = if *bus == PictureBus::Program {
            Some(settings)
        } else {
            self.dynamic
                .iter()
                .find(|d| {
                    d.bus == *bus
                        && d.format == format
                        && d.settings == settings
                        && !dispatcher.is_key_failed(d.key)
                })
                .map(|d| d.key)
        };
        if let Some(key) = existing {
            // A lingering key nobody subscribes is adopted (re-attributed);
            // a held key stays with its holder (+0 for the set).
            let unheld = self.subscribers(key) == 0;
            if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == key) {
                if unheld && principal != BOOT_PRINCIPAL {
                    principal.clone_into(&mut d.principal);
                }
                d.linger_deadline = None;
            }
            self.member_ready(index, key);
            return;
        }
        let reserved = self.sets.get(group).is_some_and(|s| s.reserved > 0);
        if !reserved {
            // The admission found this key live; it closed meanwhile.
            while !self.has_capacity(principal) {
                if !self.close_oldest_linger(dispatcher, None) {
                    break;
                }
            }
            if !self.has_capacity(principal) {
                self.refuse_pending(index, tick, RefuseReason::Capacity, dispatcher);
                return;
            }
        }
        let id = self.next_open;
        self.next_open = self.next_open.wrapping_add(1);
        match self.open_tx.try_send(OpenRequest {
            id,
            settings,
            bus: bus.clone(),
        }) {
            Ok(()) => {
                if let Some(s) = self.sets.active.iter_mut().find(|s| s.id == group) {
                    s.reserved = s.reserved.saturating_sub(1);
                }
                self.opens.insert(
                    id,
                    PendingOpen {
                        bus: bus.clone(),
                        format,
                        settings,
                        principal: principal.to_owned(),
                        deadline: tick.saturating_add(OPEN_DEADLINE_TICKS),
                        waiter: Some(index),
                    },
                );
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
                    p.phase = Phase::Opening(id);
                }
                self.dirty = true;
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.refuse_pending(index, tick, RefuseReason::Capacity, dispatcher);
            }
        }
    }

    /// Park a grouped member on its warm (or current) key.
    pub(super) fn member_ready(&mut self, index: usize, key: usize) {
        if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
            p.phase = Phase::Ready { key };
            self.dirty = true;
        }
    }

    /// End set `group` with `result` for every member still pending in it:
    /// its dispatcher moves cancelled, its arm refused, its reservations
    /// released, its audio restored. A refusal recorded on the arm first
    /// (by the switcher) is the outcome every member reports.
    pub(super) fn end_set(
        &mut self,
        group: u64,
        result: TakeResult,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(pos) = self.sets.active.iter().position(|s| s.id == group) else {
            return;
        };
        let set = self.sets.active.swap_remove(pos);
        if set.proposed.is_some() {
            dispatcher.cancel_group(group);
        }
        let mut result = result;
        if let Some(arm) = set.arm.as_ref() {
            if !arm.refuse(arm_reason(result)) {
                match arm.state() {
                    ArmState::Refused => {
                        if let (TakeResult::Refused(_), Some(reason)) = (result, arm.reason()) {
                            result = arm_result(reason);
                        }
                    }
                    ArmState::Committed | ArmState::Applying => {
                        // The residual post-commit fault (§12.8): the mixer
                        // half landed, the outputs did not.
                        self.sets.partial = self.sets.partial.saturating_add(1);
                        tracing::warn!(
                            operation = %set.operation_id,
                            at = arm.tick(),
                            outcome = ?result,
                            "routing: set-partial: the mixer half committed but an output \
                             member was refused (an encoder failed after the commit)"
                        );
                    }
                    _ => {}
                }
            }
        }
        for index in set.members {
            let mine = self
                .outputs
                .get(index)
                .and_then(|o| o.pending.as_ref())
                .is_some_and(|p| p.group == Some(group));
            if !mine {
                continue;
            }
            let Some(old) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) else {
                continue;
            };
            self.abandon_audio(index, old.phase, tick, dispatcher);
            if let Phase::Opening(id) = old.phase {
                if let Some(open) = self.opens.get_mut(&id) {
                    open.waiter = None;
                }
            }
            self.emit(old.take.operation_id, index, tick, result);
        }
        self.dirty = true;
    }

    /// The first cause of a set that missed its deadline (§12.8 "Bounds").
    fn first_cause(&self, set: &ActiveSet) -> RefuseReason {
        let phases: Vec<Phase> = set
            .members
            .iter()
            .filter_map(|&i| self.outputs.get(i).and_then(|o| o.pending.as_ref()))
            .map(|p| p.phase)
            .collect();
        if phases.iter().any(|p| matches!(p, Phase::Priming))
            || set
                .arm
                .as_ref()
                .is_some_and(|a| a.state() == ArmState::Pending)
        {
            RefuseReason::FeedNotFresh
        } else {
            RefuseReason::EncoderNotReady
        }
    }

    /// Advance every set once per tick: settle, expire, (re-)propose.
    /// O(sets × members), off the clock.
    pub(super) fn advance_sets(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let ids: Vec<u64> = self.sets.active.iter().map(|s| s.id).collect();
        for id in ids {
            self.advance_set(id, tick, dispatcher);
        }
    }

    fn advance_set(&mut self, id: u64, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(set) = self.sets.get(id) else {
            return;
        };
        let arm = set.arm.clone();
        let proposed = set.proposed;
        let admitted = set.admitted;
        let members = set.members.clone();
        let state = arm.as_ref().map(|a| a.state());
        if state == Some(ArmState::Refused) {
            let reason = arm
                .as_ref()
                .and_then(|a| a.reason())
                .unwrap_or(SetRefusal::Invalid);
            self.end_set(id, arm_result(reason), tick, dispatcher);
            return;
        }
        let pending: Vec<(usize, Phase)> = members
            .iter()
            .filter_map(|&i| {
                self.outputs
                    .get(i)
                    .and_then(|o| o.pending.as_ref())
                    .filter(|p| p.group == Some(id))
                    .map(|p| (i, p.phase))
            })
            .collect();
        if let Some(at) = proposed {
            let moving = pending.iter().any(|(_, p)| matches!(p, Phase::Moving(_)));
            let mixer_done = state.is_none_or(|s| s == ArmState::Committed);
            if !moving && mixer_done {
                // Every dispatcher member applied at `at` (drain_moves);
                // the no-op members land with them.
                if let Some(pos) = self.sets.active.iter().position(|s| s.id == id) {
                    self.sets.active.swap_remove(pos);
                }
                for (index, _) in pending {
                    self.finish_noop_member(index, at, tick, dispatcher);
                }
                self.dirty = true;
                return;
            }
            if state == Some(ArmState::SwitcherReady) {
                // The switcher missed or rejected `at`: re-propose.
                self.unpropose_set(id, tick, dispatcher);
                return;
            }
            if tick > at.saturating_add(DECISION_BACKSTOP_TICKS) {
                self.end_set(
                    id,
                    TakeResult::Refused(RefuseReason::EncoderNotReady),
                    tick,
                    dispatcher,
                );
            }
            return;
        }
        let deadline = admitted.saturating_add(SET_ADOPT_DEADLINE_TICKS);
        if tick >= deadline {
            let cause = self
                .sets
                .get(id)
                .map_or(RefuseReason::FeedNotFresh, |set| self.first_cause(set));
            self.end_set(id, TakeResult::Refused(cause), tick, dispatcher);
            return;
        }
        let all_ready = !pending.is_empty()
            && pending.len() == members.len()
            && pending
                .iter()
                .all(|(_, p)| matches!(p, Phase::Ready { .. }));
        if !all_ready || state.is_some_and(|s| s != ArmState::SwitcherReady) {
            return;
        }
        self.propose_set(id, tick, deadline, &pending, dispatcher);
    }

    /// A no-op member (already on its bus) lands at the set's tick `at`; an
    /// audio choice it carries then starts as an audio-only take.
    fn finish_noop_member(
        &mut self,
        index: usize,
        at: u64,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let has_audio = self
            .outputs
            .get_mut(index)
            .and_then(|o| o.pending.as_mut())
            .map(|p| {
                p.group = None;
                p.take.audio.is_some()
            });
        match has_audio {
            Some(true) => self.start_audio_take(index, tick, dispatcher),
            Some(false) => {
                if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) {
                    self.emit(p.take.operation_id, index, at, TakeResult::Applied);
                }
            }
            None => {}
        }
    }

    /// Pick `T`, force an IDR at `T` on every member key, register the
    /// grouped dispatcher move and propose `T` to the switcher.
    #[allow(clippy::too_many_lines)]
    // reason: one linear proposal (T, keying floor, forces, group, arm,
    // phases); splitting it hides the order the handshake depends on.
    fn propose_set(
        &mut self,
        id: u64,
        tick: u64,
        deadline: u64,
        pending: &[(usize, Phase)],
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(set) = self.sets.get(id) else {
            return;
        };
        let arm = set.arm.clone();
        let members = set.members.clone();
        let mut at = tick.saturating_add(LEAD_TICKS);
        if arm.is_some() {
            if let Some(clock) = self.sets.clock.as_ref() {
                at = at.max(
                    clock
                        .load(Ordering::Acquire)
                        .saturating_add(CLOCK_LEAD_TICKS),
                );
            }
        }
        // Members that move (a no-op member already reads its key).
        let moves: Vec<(usize, usize)> = pending
            .iter()
            .filter_map(|&(i, p)| match p {
                Phase::Ready { key } if self.outputs.get(i).is_some_and(|o| o.key != key) => {
                    Some((i, key))
                }
                _ => None,
            })
            .collect();
        // Keying at T: a key other outputs already watch counts the force
        // against its 2 s floor; the set waits for the floor (≤ 60 ticks).
        for &(_, key) in &moves {
            let watched = self
                .outputs
                .iter()
                .enumerate()
                .any(|(i, o)| !members.contains(&i) && o.key == key);
            if let Some(&last) = self.last_forced.get(&key) {
                if watched {
                    at = at.max(last.saturating_add(FORCE_FLOOR_TICKS));
                }
            }
        }
        if at >= deadline {
            // Not adoptable in time: the deadline refuses it.
            return;
        }
        for &(_, key) in &moves {
            let cell = self.cfg.base_force.get(key).or_else(|| {
                self.dynamic
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| &d.force_at)
            });
            if let Some(cell) = cell {
                cell.store(at, Ordering::Release);
            }
            self.last_forced.insert(key, at);
        }
        if !moves.is_empty()
            && dispatcher
                .rebind_group(id, at, &moves, arm.clone())
                .is_err()
        {
            self.end_set(
                id,
                TakeResult::Refused(RefuseReason::EncoderNotReady),
                tick,
                dispatcher,
            );
            return;
        }
        if let Some(arm) = arm.as_ref() {
            if !arm.propose(at) {
                // Refused meanwhile: settled next tick through the arm.
                dispatcher.cancel_group(id);
                return;
            }
        }
        if let Some(s) = self.sets.active.iter_mut().find(|s| s.id == id) {
            s.proposed = Some(at);
        }
        for &(index, key) in &moves {
            let (bus, choice) = self
                .outputs
                .get(index)
                .and_then(|o| o.pending.as_ref())
                .map_or((None, None), |p| (p.take.bus.clone(), p.take.audio.clone()));
            if let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.as_mut()) {
                p.phase = Phase::Moving(key);
            }
            if let (Some(audio), Some(bus)) = (self.audio.as_mut(), bus) {
                audio.follow(index, &bus, choice, at, Some(key), dispatcher);
            }
        }
        self.dirty = true;
    }

    /// The switcher handed the proposal back (missed, repeated or rejected
    /// tick): cancel the grouped move and park the members again.
    fn unpropose_set(&mut self, id: u64, tick: u64, dispatcher: &mut RenditionDispatcher) {
        let Some(set) = self.sets.active.iter_mut().find(|s| s.id == id) else {
            return;
        };
        set.proposed = None;
        let members = set.members.clone();
        for index in members {
            let phase = self
                .outputs
                .get(index)
                .and_then(|o| o.pending.as_ref())
                .filter(|p| p.group == Some(id))
                .map(|p| p.phase);
            if let Some(Phase::Moving(key)) = phase {
                // Parked before the cancel, so its Superseded outcome never
                // strands it; the audio scheduled with the move is restored.
                self.member_ready(index, key);
                self.abandon_audio(index, Phase::Ready { key }, tick, dispatcher);
            }
        }
        dispatcher.cancel_group(id);
        self.dirty = true;
    }
}
