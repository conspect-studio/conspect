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
use crate::rendition_dispatch::GroupPreflight;
use crate::routing_audio::{AudioPrep, AudioTakeStart};

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
    /// A grouped dispatcher move was registered for the proposal.
    grouped: bool,
}

/// A take or set that arrived for an output a set owned by the clock still
/// holds (review A-1): run once that set resolved.
pub(super) enum DeferredOp {
    Take(OutputTake),
    Set(TakeSet),
}

impl DeferredOp {
    /// The outputs this op names (one per take, one per set member).
    fn outputs(&self) -> Vec<usize> {
        match self {
            Self::Take(t) => vec![t.output_index],
            Self::Set(s) => s.members.iter().map(|m| m.output_index).collect(),
        }
    }
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
    /// Ops deferred behind a clock-owned set: at most one per output (no two
    /// share an output; a newer op on an output supersedes the older), so
    /// bounded by the outputs.
    pub(super) deferred: Vec<DeferredOp>,
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
        SetRefusal::FormatMismatch => TakeResult::Refused(RefuseReason::FormatMismatch),
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
        TakeResult::Refused(RefuseReason::FormatMismatch) => SetRefusal::FormatMismatch,
        TakeResult::Refused(RefuseReason::EncoderNotReady)
        | TakeResult::Applied
        | TakeResult::Fallback(_)
        | TakeResult::ResetApplied
        | TakeResult::ResetRefused(_) => SetRefusal::EncoderNotReady,
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

    /// The published per-principal holdings: the cached list, rebuilt
    /// (one bounded scan) only when [`OutputRouter::holdings_dirty`] says a
    /// holding changed. An unchanged tick is one `Arc` clone: no scan, no
    /// allocation on the bake consumer (invariant #1). Test builds verify
    /// the cache against a fresh scan on every call.
    pub(super) fn held_by_principal(&mut self) -> Arc<Vec<(String, usize)>> {
        if self.holdings_dirty {
            let next = self.principals_held();
            if *self.principals_held != next {
                self.principals_held = Arc::new(next);
            }
            self.holdings_dirty = false;
            #[cfg(test)]
            {
                self.holdings_rebuilds += 1;
            }
        }
        #[cfg(test)]
        assert_eq!(
            *self.principals_held,
            self.principals_held(),
            "a holdings change did not mark holdings_dirty"
        );
        Arc::clone(&self.principals_held)
    }

    /// Every principal holding encoders, with [`Self::principal_held`]
    /// (sorted; bounded by the keys, opens and sets the router holds).
    pub(super) fn principals_held(&self) -> Vec<(String, usize)> {
        let mut principals: Vec<&str> = self
            .dynamic
            .iter()
            .map(|d| d.principal.as_str())
            .chain(self.opens.values().map(|o| o.principal.as_str()))
            .chain(
                self.sets
                    .active
                    .iter()
                    .filter(|s| s.reserved > 0)
                    .map(|s| s.principal.as_str()),
            )
            .filter(|p| !p.is_empty())
            .collect();
        principals.sort_unstable();
        principals.dedup();
        principals
            .into_iter()
            .map(|p| (p.to_owned(), self.principal_held(p)))
            .filter(|(_, held)| *held > 0)
            .collect()
    }

    /// Encoders charged to `principal`: live keys, opens in flight and its
    /// sets' unissued reservations.
    fn principal_held(&self, principal: &str) -> usize {
        self.dynamic
            .iter()
            .filter(|d| d.principal == principal)
            .count()
            + self
                .opens
                .values()
                .filter(|o| o.principal == principal)
                .count()
            + self.set_reservations(principal).1
    }

    /// Whether output `index` is held by a set the clock owns: its mixer
    /// half is applying or committed at `T`, or (no mixer half) its grouped
    /// dispatcher move is decided or partly applied. Such a set is never
    /// superseded (review A-1); a later op on its outputs is deferred.
    pub(super) fn output_owned(&self, index: usize, dispatcher: &RenditionDispatcher) -> bool {
        self.group_of(index)
            .and_then(|g| self.sets.get(g))
            .is_some_and(|set| match set.arm.as_ref() {
                Some(arm) => matches!(arm.state(), ArmState::Applying | ArmState::Committed),
                None => {
                    set.proposed.is_some() && set.grouped && !dispatcher.group_undecided(set.id)
                }
            })
    }

    /// Win the refusal of set `group` before anything of it is cancelled
    /// (review A-1). `true`: the set may be superseded now (its arm is
    /// refused, or it has none and its group is undecided). `false`: the
    /// clock owns it (the refusal lost to `begin_apply`/`commit`, or its
    /// dispatcher group is decided): it finishes its `T` decision.
    pub(super) fn try_supersede_set(&self, group: u64, dispatcher: &RenditionDispatcher) -> bool {
        let Some(set) = self.sets.get(group) else {
            return true;
        };
        match set.arm.as_ref() {
            Some(arm) => {
                arm.refuse(SetRefusal::Superseded)
                    || !matches!(arm.state(), ArmState::Applying | ArmState::Committed)
            }
            None => !(set.proposed.is_some() && set.grouped && !dispatcher.group_undecided(group)),
        }
    }

    /// Park `op` until the set holding its outputs resolved. Newest wins:
    /// an older deferred op sharing an output was already superseded by the
    /// caller ([`Self::supersede_deferred`]).
    pub(super) fn defer(&mut self, op: DeferredOp) {
        self.sets.deferred.push(op);
        self.dirty = true;
    }

    /// Supersede every deferred op naming one of `outputs` (newest wins):
    /// each of its members reports `Superseded`, a set's arm is refused.
    pub(super) fn supersede_deferred(&mut self, outputs: &[usize], tick: u64) {
        if self.sets.deferred.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.sets.deferred.len() {
            let hit = self
                .sets
                .deferred
                .get(i)
                .is_some_and(|op| op.outputs().iter().any(|o| outputs.contains(o)));
            if !hit {
                i += 1;
                continue;
            }
            match self.sets.deferred.swap_remove(i) {
                DeferredOp::Take(t) => {
                    self.emit(t.operation_id, t.output_index, tick, TakeResult::Superseded);
                }
                DeferredOp::Set(set) => {
                    if let Some(arm) = set.arm.as_ref() {
                        let _ = arm.refuse(SetRefusal::Superseded);
                    }
                    for m in &set.members {
                        self.emit(
                            set.operation_id.clone(),
                            m.output_index,
                            tick,
                            TakeResult::Superseded,
                        );
                    }
                }
            }
        }
    }

    /// Run every deferred op whose outputs no clock-owned set holds any
    /// more (bounded: at most one op per output).
    pub(super) fn run_deferred(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        if self.sets.deferred.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.sets.deferred.len() {
            let free = self.sets.deferred.get(i).is_some_and(|op| {
                op.outputs()
                    .iter()
                    .all(|&o| !self.output_owned(o, dispatcher))
            });
            if !free {
                i += 1;
                continue;
            }
            match self.sets.deferred.swap_remove(i) {
                DeferredOp::Take(take) => self.process_take(take, tick, dispatcher),
                DeferredOp::Set(set) => self.admit_set(set, tick, dispatcher),
            }
            // Processing may defer again or supersede others: restart.
            i = 0;
        }
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
        // Newest wins over older deferred ops on these outputs; while a set
        // the clock owns holds one of them, this set waits for it (A-1).
        let outputs: Vec<usize> = set.members.iter().map(|m| m.output_index).collect();
        self.supersede_deferred(&outputs, tick);
        if outputs.iter().any(|&o| self.output_owned(o, dispatcher)) {
            self.defer(DeferredOp::Set(set));
            return;
        }
        let mut groups: Vec<u64> = outputs.iter().filter_map(|&o| self.group_of(o)).collect();
        groups.sort_unstable();
        groups.dedup();
        for group in groups {
            if !self.try_supersede_set(group, dispatcher) {
                self.defer(DeferredOp::Set(set));
                return;
            }
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
        let mut opens: Vec<(
            PictureBus,
            Option<super::AuxFormat>,
            usize,
            Option<super::ReformatSettings>,
        )> = Vec::new();
        // Ownership transfers (review B-1): an unheld lingering key another
        // principal owns is re-attributed to this set's principal when a
        // member adopts it, so it counts against that principal's cap
        // (deduplicated). Held and reformat keys keep their attribution.
        let mut adopts: Vec<usize> = Vec::new();
        for m in &set.members {
            let index = m.output_index;
            let Some(bus) = m.bus.clone() else { continue };
            // The output's encoder settings (review M-2): its rendition
            // settings plus any live reset's ad-hoc settings.
            let Some(out) = self.outputs.get(index) else {
                continue;
            };
            if bus == PictureBus::Program || out.bus == bus {
                continue;
            }
            let (settings, reset) = (out.settings, out.reset.clone());
            let format = self.feed_key(index, &bus).1;
            let live = self
                .dynamic
                .iter()
                .find(|d| {
                    d.bus == bus
                        && d.format == format
                        && d.settings == settings
                        && d.reset == reset
                        && !d.reformat
                        && !dispatcher.is_key_failed(d.key)
                })
                .map(|d| (d.key, d.principal != set.principal));
            if let Some((key, foreign)) = live {
                if foreign
                    && set.principal != BOOT_PRINCIPAL
                    && self.subscribers(key) == 0
                    && !adopts.contains(&key)
                {
                    adopts.push(key);
                }
            }
            let live = live.is_some();
            let in_flight = self
                .opens
                .values()
                .any(|o| o.bus == bus && o.settings == settings && o.reset == reset && !o.reformat);
            let entry = (bus, format, settings, reset);
            if !live && !in_flight && !opens.contains(&entry) {
                opens.push(entry);
            }
        }
        let needed = opens.len();
        let adopted = adopts.len();
        let fits = |router: &Self| {
            let (all, _) = router.set_reservations(&set.principal);
            let total = router.dynamic.len() + router.opens.len() + all;
            let held = router.principal_held(&set.principal);
            total + needed <= router.cfg.max_encoders
                && held + needed + adopted <= router.cfg.max_per_principal
                && router.opens.len() + all + needed <= OPEN_QUEUE
        };
        // Closing lingers only makes room for new opens (an adopted key is
        // itself a linger: closing it would not make room for adopting it).
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
        self.holdings_dirty = true;
        self.sets.active.push(ActiveSet {
            id,
            operation_id: set.operation_id,
            principal: set.principal,
            members,
            arm: set.arm,
            admitted: tick,
            reserved: needed,
            proposed: None,
            grouped: false,
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
        let (Some(&base), Some(out)) = (self.cfg.base_keys.get(index), self.outputs.get(index))
        else {
            return;
        };
        // The output's encoder settings (review M-2), as `start_move`.
        let (settings, reset) = (out.settings, out.reset.clone());
        let Some(group) = self.group_of(index) else {
            return;
        };
        let (_, format) = self.feed_key(index, bus);
        if let Some(audio) = self.audio.as_mut() {
            audio.admit(index, bus, choice, tick, dispatcher);
        }
        let existing = if *bus == PictureBus::Program {
            Some(base)
        } else {
            self.dynamic
                .iter()
                .find(|d| {
                    d.bus == *bus
                        && d.format == format
                        && d.settings == settings
                        && d.reset == reset
                        && !d.reformat
                        && !dispatcher.is_key_failed(d.key)
                })
                .map(|d| d.key)
        };
        if let Some(key) = existing {
            // A lingering key nobody subscribes is adopted (re-attributed);
            // a held key stays with its holder (+0 for the set). Adopting
            // another principal's key moves it onto this principal's cap:
            // re-validated here (review B-1), the admission counted it.
            let unheld = self.subscribers(key) == 0;
            let transfer = unheld
                && principal != BOOT_PRINCIPAL
                && self
                    .dynamic
                    .iter()
                    .any(|d| d.key == key && d.principal != principal && !d.reformat);
            if transfer && self.principal_held(principal) >= self.cfg.max_per_principal {
                self.refuse_pending(index, tick, RefuseReason::Capacity, dispatcher);
                return;
            }
            self.adopt_key(
                key,
                (unheld && principal != BOOT_PRINCIPAL).then_some(principal),
            );
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
            reformat: reset.clone(),
        }) {
            Ok(()) => {
                if let Some(s) = self.sets.active.iter_mut().find(|s| s.id == group) {
                    s.reserved = s.reserved.saturating_sub(1);
                }
                self.insert_open(
                    id,
                    PendingOpen {
                        bus: bus.clone(),
                        format,
                        settings,
                        reset,
                        reformat: false,
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
        // The refusal is tried BEFORE anything is cancelled (review A-1): a
        // supersede never reaches here for a set the clock owns (callers win
        // `try_supersede_set` first), so a lost refusal below is a genuine
        // fault after the commit (an encoder died), never a supersede.
        let set = self.sets.active.swap_remove(pos);
        self.holdings_dirty = true;
        let mut result = result;
        let refused = set
            .arm
            .as_ref()
            .is_none_or(|arm| arm.refuse(arm_reason(result)));
        if set.proposed.is_some() {
            dispatcher.cancel_group(group);
        }
        if let (false, Some(arm)) = (refused, set.arm.as_ref()) {
            {
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
                    self.holdings_dirty = true;
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

    /// A no-op member (already on its bus, carrying no audio change the
    /// set had to switch) lands at the set's tick `at`. Its audio choice, if
    /// any, is recorded: the output already carries that audio (an audio
    /// change is a grouped member, review A-3), so nothing fades.
    fn finish_noop_member(
        &mut self,
        index: usize,
        at: u64,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(p) = self.outputs.get_mut(index).and_then(|o| o.pending.take()) else {
            return;
        };
        let bus = self
            .outputs
            .get(index)
            .map_or(PictureBus::Program, |o| o.bus.clone());
        if let (Some(choice), Some(audio)) = (p.take.audio, self.audio.as_mut()) {
            match audio.take_audio(index, choice, &bus, tick, dispatcher) {
                AudioTakeStart::Unchanged | AudioTakeStart::Refused => {}
                AudioTakeStart::Started => {
                    // The carried audio changed after the preflight: never
                    // a step; the change lands as soon as it can.
                    audio.finish_take(index, true, &bus, tick, dispatcher);
                }
            }
        }
        self.emit(p.take.operation_id, index, at, TakeResult::Applied);
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
        // Preflight before anything is armed (review A-2): an out-of-band
        // output whose destination carries another decoder configuration
        // refuses the whole set; the arm is refused, the mixer never moves.
        match dispatcher.preflight_group(&moves) {
            GroupPreflight::Ready => {}
            GroupPreflight::Pending => return,
            GroupPreflight::FormatMismatch => {
                self.end_set(
                    id,
                    TakeResult::Refused(RefuseReason::FormatMismatch),
                    tick,
                    dispatcher,
                );
                return;
            }
        }
        // Members staying on their video that change audio (review A-3):
        // part of the shared decision, their audio prepared before `T`.
        let mut stays: Vec<(usize, usize)> = Vec::new();
        for &(i, p) in pending {
            let Phase::Ready { key } = p else { continue };
            let Some((bus, Some(choice))) = self
                .outputs
                .get(i)
                .filter(|o| o.key == key)
                .and_then(|o| o.pending.as_ref())
                .map(|p| (p.take.bus.clone(), p.take.audio.clone()))
            else {
                continue;
            };
            let bus = bus.unwrap_or(PictureBus::Program);
            let prep = self.audio.as_mut().map_or(AudioPrep::Failed, |a| {
                a.prepare(i, &bus, &choice, tick, dispatcher)
            });
            match prep {
                AudioPrep::Unchanged => {}
                AudioPrep::Ready => stays.push((i, key)),
                AudioPrep::Pending => return,
                AudioPrep::Failed => {
                    self.end_set(
                        id,
                        TakeResult::Refused(RefuseReason::EncoderNotReady),
                        tick,
                        dispatcher,
                    );
                    return;
                }
            }
        }
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
        let mut group: Vec<(usize, usize)> = moves.clone();
        group.extend_from_slice(&stays);
        if !group.is_empty()
            && dispatcher
                .rebind_group(id, at, &group, arm.clone())
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
            s.grouped = !group.is_empty();
        }
        for &(index, key) in &group {
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
        set.grouped = false;
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
