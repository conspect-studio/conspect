//! Live format reset of one output (#342 B3, design DESIGN-B.md "B3"):
//! Class-2 for that output only.
//!
//! A [`Reformat`] opens a new encoder at the output's new size / fit / bit
//! rate with the SAME codec, off the clock (the `routing-open` thread),
//! fed the picture the output carries now (Program or Clean through a
//! `FormatConvert` at a non-canvas format; a routed source/PVW re-keyed at
//! the new feed format). Once its first window is staged the output is
//! FORCE-moved onto it at an IDR — make-before-break: the old key feeds the
//! output until the move lands. The move changes the decoder configuration,
//! which the dispatcher signals (`mark_discontinuity`): HLS writes
//! `EXT-X-DISCONTINUITY` at the segment the IDR starts and the segment muxer
//! is rebuilt with the next TS `tables_version`. The new key becomes that
//! output's base key (fallback and later takes); the old key retires once no
//! output uses it. Program and every other output are untouched.
//!
//! Bounded: at most one reformat in flight per output (a newer one
//! supersedes it), it counts against the global encoder cap, and it is
//! refused at [`REFORMAT_DEADLINE_TICKS`]. Only HLS outputs with TS
//! segments reset live (review M-1: their segment muxer is rebuilt at the
//! discontinuity); SRT/RIST and every other kind are `restart-required`
//! (follow-up #380).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{TryRecvError, TrySendError};
use std::sync::Arc;

use super::{
    DynamicKey, GapFill, OpenRequest, OutputRouter, PendingOpen, ReformatPhase, ReformatRefusal,
    ReformatState, TakeResult, NO_FORCE, OPEN_DEADLINE_TICKS, REFORMAT_DEADLINE_TICKS,
};
use crate::rendition_dispatch::{DtsTimeBase, RenditionDispatcher};
use crate::routing_aux::PictureBus;

impl OutputRouter {
    /// Drain the reformat mailbox: refuse what cannot reset live, supersede
    /// an older reformat of the same output, and start the open.
    pub(super) fn drain_reformats(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        loop {
            let req = match self.reformats.try_recv() {
                Ok(req) => req,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            };
            let index = req.output_index;
            if index >= self.outputs.len()
                || !self.can_reformat
                || !self.cfg.live_reset.get(index).copied().unwrap_or(false)
                || !dispatcher.inband_ok(index)
            {
                self.emit(
                    req.operation_id,
                    index,
                    tick,
                    TakeResult::ResetRefused(ReformatRefusal::RestartRequired),
                );
                continue;
            }
            // At most one reformat per output: a newer one supersedes it.
            self.abandon_reformat(index, tick, ReformatRefusal::Superseded, dispatcher);
            // Admission: the global cap (a reformat's key is one more encoder
            // until the old key retires), after closing 0-subscriber lingers.
            while self.dynamic.len() + self.opens.len() >= self.cfg.max_encoders {
                if !self.close_oldest_linger(dispatcher, None) {
                    break;
                }
            }
            if self.dynamic.len() + self.opens.len() >= self.cfg.max_encoders {
                self.emit(
                    req.operation_id,
                    index,
                    tick,
                    TakeResult::ResetRefused(ReformatRefusal::Capacity),
                );
                continue;
            }
            let bus = self
                .outputs
                .get(index)
                .map_or(PictureBus::Program, |o| o.bus.clone());
            let id = self.next_open;
            self.next_open = self.next_open.wrapping_add(1);
            let settings = self.cfg.base_keys.get(index).copied().unwrap_or(0);
            let format = if super::aux_composed(&bus) {
                req.feed_format
            } else {
                None
            };
            match self.open_tx.try_send(OpenRequest {
                id,
                settings,
                bus: bus.clone(),
                reformat: Some(req.settings.clone()),
            }) {
                Ok(()) => {
                    self.opens.insert(
                        id,
                        PendingOpen {
                            bus,
                            format,
                            settings,
                            reformat: true,
                            principal: req.principal.clone(),
                            deadline: tick.saturating_add(OPEN_DEADLINE_TICKS),
                            // A reformat's open is waited for by the
                            // reformat, not a take.
                            waiter: None,
                        },
                    );
                    if let Some(out) = self.outputs.get_mut(index) {
                        out.reformat = Some(ReformatState {
                            req,
                            started: tick,
                            phase: ReformatPhase::Opening(id),
                        });
                    }
                    self.dirty = true;
                }
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    self.emit(
                        req.operation_id,
                        index,
                        tick,
                        TakeResult::ResetRefused(ReformatRefusal::Capacity),
                    );
                }
            }
        }
    }

    /// The open of a reformat arrived: register its key (never shared with a
    /// take), start its worker, and wait for its first window. Returns
    /// `false` when `id` is not a reformat's open (the caller handles it).
    pub(super) fn reformat_opened(
        &mut self,
        id: u64,
        open: &PendingOpen,
        encoder: Result<Box<dyn super::LiveEncoder>, String>,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        if !open.reformat {
            return false;
        }
        let index = self.outputs.iter().position(|o| {
            o.reformat
                .as_ref()
                .is_some_and(|r| r.phase == ReformatPhase::Opening(id))
        });
        let Some(index) = index.filter(|_| tick < open.deadline) else {
            // Superseded, or too late: never takes a dispatcher slot.
            if let Ok(encoder) = encoder {
                self.reap(super::Reap::Encoder(encoder));
            }
            if let Some(index) = index {
                self.refuse_reformat(index, tick, ReformatRefusal::EncoderNotReady);
            }
            return true;
        };
        let Ok(mut encoder) = encoder else {
            self.refuse_reformat(index, tick, ReformatRefusal::EncoderNotReady);
            return true;
        };
        let tb = encoder.time_base();
        // The baked Program picture is published AFTER the router's tick
        // (`publish_program`, once baked), so a key on Program is first fed
        // at the next tick; every other bus is fed this tick (`feed`).
        let first = if open.bus == PictureBus::Program {
            tick.saturating_add(1)
        } else {
            tick
        };
        encoder.start_at(first);
        let gate = encoder.move_gate();
        let Ok(key) = dispatcher.add_video_key(
            DtsTimeBase {
                num: tb.num,
                den: tb.den,
            },
            gate,
        ) else {
            self.reap(super::Reap::Encoder(encoder));
            self.refuse_reformat(index, tick, ReformatRefusal::Capacity);
            return true;
        };
        if let Some(params) = encoder.parameter_sets() {
            let _ = dispatcher.set_parameter_sets(key, params);
        }
        let force_at = Arc::new(AtomicU64::new(NO_FORCE));
        let worker = GapFill {
            inner: encoder,
            force_at: Arc::clone(&force_at),
            next: None,
            last: None,
        };
        let lost = Arc::new(AtomicBool::new(false));
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let Ok(worker) =
            self.spawn_live_worker(key, generation, tb, Box::new(worker), Arc::clone(&lost))
        else {
            let _ = dispatcher.retire_video_key(key);
            self.refuse_reformat(index, tick, ReformatRefusal::EncoderNotReady);
            return true;
        };
        self.last_forced.insert(key, tick);
        self.dynamic.push(DynamicKey {
            bus: open.bus.clone(),
            format: open.format,
            settings: open.settings,
            reformat: true,
            pinned_for: None,
            key,
            worker: Some(worker),
            force_at,
            principal: open.principal.clone(),
            linger_deadline: None,
            lingering_since: tick,
            last_fed: None,
            last_aux_tick: None,
            seen_event: false,
            lost,
            generation,
            silent: 0,
            starved: 0,
            refused_this_tick: false,
            reported_degraded: false,
        });
        if let Some(r) = self
            .outputs
            .get_mut(index)
            .and_then(|o| o.reformat.as_mut())
        {
            r.phase = ReformatPhase::Warming(key);
        }
        self.dirty = true;
        true
    }

    /// Advance every reformat: refuse at the deadline; once the new key has
    /// dispatched a window, request the FORCED move at its next IDR.
    pub(super) fn advance_reformats(&mut self, tick: u64, dispatcher: &mut RenditionDispatcher) {
        for index in 0..self.outputs.len() {
            let Some((started, phase)) = self
                .outputs
                .get(index)
                .and_then(|o| o.reformat.as_ref())
                .map(|r| (r.started, r.phase))
            else {
                continue;
            };
            if tick.saturating_sub(started) >= REFORMAT_DEADLINE_TICKS {
                self.abandon_reformat(index, tick, ReformatRefusal::EncoderNotReady, dispatcher);
                continue;
            }
            match phase {
                ReformatPhase::Opening(id) => {
                    if !self.opens.contains_key(&id) {
                        // Expired or lost: the result (if any) is reaped.
                        self.abandon_reformat(
                            index,
                            tick,
                            ReformatRefusal::EncoderNotReady,
                            dispatcher,
                        );
                    }
                }
                ReformatPhase::Warming(key) => {
                    let seen = self
                        .dynamic
                        .iter()
                        .find(|d| d.key == key)
                        .map(|d| d.seen_event);
                    if seen.is_none() || dispatcher.is_key_failed(key) {
                        self.abandon_reformat(
                            index,
                            tick,
                            ReformatRefusal::EncoderNotReady,
                            dispatcher,
                        );
                    } else if seen == Some(true) {
                        let at = tick.saturating_add(1);
                        if let Some(d) = self.dynamic.iter().find(|d| d.key == key) {
                            d.force_at.store(at, Ordering::Release);
                        }
                        self.last_forced.insert(key, at);
                        let windows = self.cfg.gop.saturating_add(3);
                        // FORCED: the decoder configuration changes on
                        // purpose; the in-band output splices with a
                        // discontinuity (never a format-mismatch refusal).
                        if dispatcher
                            .rebind_video_forced(index, key, at, windows)
                            .is_ok()
                        {
                            if let Some(r) = self
                                .outputs
                                .get_mut(index)
                                .and_then(|o| o.reformat.as_mut())
                            {
                                r.phase = ReformatPhase::Moving(key);
                            }
                        } else {
                            self.abandon_reformat(
                                index,
                                tick,
                                ReformatRefusal::EncoderNotReady,
                                dispatcher,
                            );
                        }
                    }
                }
                ReformatPhase::Moving(_) => {}
            }
        }
    }

    /// The forced move of a reformat landed on `key` at `at`: the key becomes
    /// the output's base key (fallback + later takes); the old base key is
    /// released. Returns `false` when no reformat of `index` moves to `key`.
    pub(super) fn reformat_applied(
        &mut self,
        index: usize,
        key: usize,
        at: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let moving = self
            .outputs
            .get(index)
            .and_then(|o| o.reformat.as_ref())
            .is_some_and(|r| r.phase == ReformatPhase::Moving(key));
        if !moving {
            return false;
        }
        let Some(req) = self
            .outputs
            .get_mut(index)
            .and_then(|o| o.reformat.take())
            .map(|r| r.req)
        else {
            return false;
        };
        let old_base = self.cfg.base_keys.get(index).copied();
        let _ = dispatcher.set_fallback_key(index, key);
        if let Some(base) = self.cfg.base_keys.get_mut(index) {
            *base = key;
        }
        if let Some(formats) = self.cfg.formats.get_mut(index) {
            *formats = req.feed_format;
        } else if req.feed_format.is_some() {
            self.cfg.formats.resize(index + 1, None);
            if let Some(f) = self.cfg.formats.get_mut(index) {
                *f = req.feed_format;
            }
        }
        if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == key) {
            d.pinned_for = Some(index);
            d.linger_deadline = None;
        }
        if let Some(out) = self.outputs.get_mut(index) {
            out.key = key;
            out.since = at;
        }
        // The old base: a previous reformat's dynamic key nobody else uses
        // is unpinned and lingers out like any other live key; a run base
        // key keeps its worker (other outputs may share it).
        if let Some(old) = old_base.filter(|&o| o != key) {
            if let Some(d) = self.dynamic.iter_mut().find(|d| d.key == old) {
                if d.pinned_for == Some(index) {
                    d.pinned_for = None;
                }
            }
        }
        self.emit(req.operation_id, index, at, TakeResult::ResetApplied);
        true
    }

    /// The forced move of a reformat was refused (or superseded by the
    /// dispatcher): the reformat is refused, its key closes.
    pub(super) fn reformat_move_refused(
        &mut self,
        index: usize,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let moving = self
            .outputs
            .get(index)
            .and_then(|o| o.reformat.as_ref())
            .is_some_and(|r| matches!(r.phase, ReformatPhase::Moving(_)));
        if moving {
            self.abandon_reformat(index, tick, ReformatRefusal::EncoderNotReady, dispatcher);
        }
        moving
    }

    /// End `index`'s reformat (if any) with `reason`: its open is orphaned
    /// (released at its deadline), a started key closes, a requested move is
    /// cancelled (the output keeps its key).
    pub(super) fn abandon_reformat(
        &mut self,
        index: usize,
        tick: u64,
        reason: ReformatRefusal,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(state) = self.outputs.get_mut(index).and_then(|o| o.reformat.take()) else {
            return;
        };
        match state.phase {
            ReformatPhase::Opening(id) => {
                // The open's result goes to the reaper (`drain_opens`).
                self.opens.remove(&id);
            }
            ReformatPhase::Warming(key) => self.close(key, dispatcher),
            ReformatPhase::Moving(key) => {
                self.cancel_dispatcher_move(index, dispatcher, tick);
                self.close(key, dispatcher);
            }
        }
        self.dirty = true;
        self.emit(
            state.req.operation_id,
            index,
            tick,
            TakeResult::ResetRefused(reason),
        );
    }

    /// A reformat refused before it started a key.
    fn refuse_reformat(&mut self, index: usize, tick: u64, reason: ReformatRefusal) {
        if let Some(state) = self.outputs.get_mut(index).and_then(|o| o.reformat.take()) {
            self.dirty = true;
            self.emit(
                state.req.operation_id,
                index,
                tick,
                TakeResult::ResetRefused(reason),
            );
        }
    }

    /// Reformats in flight (at most one per output).
    #[must_use]
    pub fn reformats_in_flight(&self) -> usize {
        self.outputs.iter().filter(|o| o.reformat.is_some()).count()
    }

    /// Whether `key` is a reformat key still pinned as some output's base,
    /// or warming/moving for a reformat in flight: it never lingers.
    pub(super) fn reformat_holds(&self, key: usize) -> bool {
        self.dynamic
            .iter()
            .any(|d| d.key == key && d.reformat && d.pinned_for.is_some())
            || self.outputs.iter().any(|o| {
                o.reformat.as_ref().is_some_and(|r| {
                    matches!(r.phase, ReformatPhase::Warming(k) | ReformatPhase::Moving(k) if k == key)
                })
            })
    }
}
