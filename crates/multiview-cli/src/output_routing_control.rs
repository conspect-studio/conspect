//! The control-plane adapter over the run's [`OutputRoutingHandle`]
//! (ADR-0108 slice 1): implements [`multiview_control::output_routing::OutputRouting`]
//! by mapping each output's **config id** to the router's output index from
//! the run's own output map (`RunOutput::id`, the binding id of each runner),
//! so a renamed or reordered output can never address another output's index.
//!
//! The adapter reads the run's slot on every call (a wait-free load), so it
//! follows the handle of whichever run is live and answers `Busy` between
//! runs. Nothing here blocks: `try_take` is a bounded `try_send`.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use multiview_control::output_routing::{
    OutputLive, OutputOutcome, OutputRouting, OutputTakeRequest, PlanCost, PlanRefusal, RoutedFeed,
    RoutingLive, TakeSubmitError,
};

use crate::output_routing::{
    FallbackReason, OutputRoutingHandle, OutputRoutingSlot, OutputTake, RefuseReason, TakeOutcome,
    TakeResult, DEFAULT_MAX_ENCODERS,
};
use crate::routing_aux::PictureBus;

/// The wire string of a take outcome (`route.output`).
#[must_use]
pub fn outcome_str(result: TakeResult) -> &'static str {
    match result {
        TakeResult::Applied => "applied",
        TakeResult::Superseded => "superseded",
        TakeResult::Refused(RefuseReason::FeedNotFresh) => "refused:feed-not-fresh",
        TakeResult::Refused(RefuseReason::Capacity) => "refused:capacity",
        TakeResult::Refused(RefuseReason::EncoderNotReady) => "refused:encoder-not-ready",
        TakeResult::Refused(RefuseReason::Unroutable) => "refused:unroutable",
        TakeResult::Refused(RefuseReason::FormatMismatch) => "refused:format-mismatch",
        TakeResult::Fallback(FallbackReason::AuxFailed) => "fallback:aux-failed",
        TakeResult::Fallback(FallbackReason::EncoderFailed) => "fallback:encoder-failed",
    }
}

fn routed_feed(bus: &PictureBus) -> RoutedFeed {
    match bus {
        PictureBus::Program => RoutedFeed::Program,
        PictureBus::Preview => RoutedFeed::Preview,
        PictureBus::Clean => RoutedFeed::Clean,
        PictureBus::Source(id) => RoutedFeed::Source(id.clone()),
    }
}

fn picture_bus(feed: &RoutedFeed) -> PictureBus {
    match feed {
        RoutedFeed::Program => PictureBus::Program,
        RoutedFeed::Preview => PictureBus::Preview,
        RoutedFeed::Clean => PictureBus::Clean,
        RoutedFeed::Source(id) => PictureBus::Source(id.clone()),
    }
}

/// Outcomes buffered between the engine and the control relay.
const RELAY_CAPACITY: usize = 64;

/// The [`OutputRouting`] implementation over a run's routing slot.
pub struct OutputRoutingAdapter {
    slot: OutputRoutingSlot,
    max_encoders: usize,
    outcome_tx: SyncSender<OutputOutcome>,
    outcome_rx: Mutex<Option<Receiver<OutputOutcome>>>,
    /// The handle whose outcome stream the pump already took.
    pumped: Mutex<Option<Arc<OutputRoutingHandle>>>,
}

impl std::fmt::Debug for OutputRoutingAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputRoutingAdapter")
            .finish_non_exhaustive()
    }
}

impl OutputRoutingAdapter {
    /// An adapter over `slot`.
    #[must_use]
    pub fn new(slot: OutputRoutingSlot) -> Self {
        let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(RELAY_CAPACITY);
        Self {
            slot,
            max_encoders: DEFAULT_MAX_ENCODERS,
            outcome_tx,
            outcome_rx: Mutex::new(Some(outcome_rx)),
            pumped: Mutex::new(None),
        }
    }

    fn handle(&self) -> Option<Arc<OutputRoutingHandle>> {
        let handle = self.slot.load()?;
        self.ensure_pump(&handle);
        Some(handle)
    }

    /// Start (once per run handle) a thread translating the handle's outcome
    /// indices to config ids. Full relay channel = drop (the engine side is
    /// already drop-when-full; outcomes are best-effort, the readback is truth).
    fn ensure_pump(&self, handle: &Arc<OutputRoutingHandle>) {
        let mut pumped = self.pumped.lock().unwrap_or_else(PoisonError::into_inner);
        if pumped.as_ref().is_some_and(|h| Arc::ptr_eq(h, handle)) {
            return;
        }
        *pumped = Some(Arc::clone(handle));
        let Some(rx) = handle.outcomes() else {
            return;
        };
        let ids: Vec<Option<String>> = handle.outputs().iter().map(|o| o.id.clone()).collect();
        let tx = self.outcome_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("route-outcome-map".to_owned())
            .spawn(move || pump(&rx, &ids, &tx));
        if let Err(error) = spawned {
            tracing::warn!(%error, "output routing: outcome pump did not start");
        }
    }

    /// Install a pump for the current handle now (call after a run started).
    pub fn poll(&self) {
        let _ = self.handle();
    }
}

fn pump(rx: &Receiver<TakeOutcome>, ids: &[Option<String>], tx: &SyncSender<OutputOutcome>) {
    while let Ok(outcome) = rx.recv() {
        let Some(Some(output)) = ids.get(outcome.output_index) else {
            continue;
        };
        let mapped = OutputOutcome {
            operation_id: outcome.operation_id,
            output: output.clone(),
            tick: outcome.tick,
            outcome: outcome_str(outcome.outcome).to_owned(),
            feed: Some(routed_feed(&outcome.bus)),
        };
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = tx.try_send(mapped) {
            return;
        }
    }
}

impl OutputRouting for OutputRoutingAdapter {
    fn plan(&self, output: &str, feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal> {
        let Some(handle) = self.handle() else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        let Some(index) = handle.index_of(output) else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        let Some(run_output) = handle.outputs().get(index) else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        if !run_output.routable {
            return Err(PlanRefusal::OutputKindUnroutable);
        }
        if matches!(feed, RoutedFeed::Preview | RoutedFeed::Clean) && !handle.has_preview() {
            return Err(PlanRefusal::SwitcherRequired);
        }
        let snapshot = handle.snapshot();
        let bus = feed.bus_name();
        // +0 when the feed is Program (the base encoder), another output
        // with the same settings already carries this bus, or a live encoder
        // for (bus, settings) still runs (a lingering one is reused by the
        // take): encode once per (bus, settings), §4.3.
        let shared = *feed == RoutedFeed::Program
            || handle.outputs().iter().enumerate().any(|(i, o)| {
                o.settings == run_output.settings
                    && snapshot.outputs.get(i).is_some_and(|v| v.bus == bus)
            })
            || snapshot
                .live_keys
                .iter()
                .any(|(b, settings)| *b == bus && *settings == run_output.settings);
        Ok(PlanCost {
            needs_encoder: !shared,
            sessions_in_use: snapshot
                .dynamic_encoders_live
                .saturating_add(snapshot.opens_in_flight),
            sessions_max: self.max_encoders,
        })
    }

    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        let handle = self.handle().ok_or(TakeSubmitError::Busy)?;
        let index = handle
            .index_of(&take.output)
            .ok_or(TakeSubmitError::Unroutable)?;
        handle
            .try_take(OutputTake {
                operation_id: take.operation_id,
                output_index: index,
                bus: picture_bus(&take.feed),
                principal: take.principal,
            })
            .map_err(|_| TakeSubmitError::Busy)
    }

    fn snapshot(&self) -> Option<RoutingLive> {
        let handle = self.handle()?;
        let snapshot = handle.snapshot();
        let mut outputs = HashMap::new();
        for (run_output, view) in handle.outputs().iter().zip(&snapshot.outputs) {
            if let Some(id) = &run_output.id {
                outputs.insert(
                    id.clone(),
                    OutputLive {
                        bus: view.bus.clone(),
                        since_tick: view.since_tick,
                        pending: view.pending.as_ref().map(|p| (p.bus.clone(), p.held_ticks)),
                        degraded: view.degraded,
                    },
                );
            }
        }
        Some(RoutingLive {
            outputs,
            encoders: snapshot.video_keys,
            sessions_in_use: snapshot
                .dynamic_encoders_live
                .saturating_add(snapshot.opens_in_flight),
            sessions_max: self.max_encoders,
            dropped_ticks: snapshot.dropped_aux_ticks,
        })
    }

    fn subscribe_outcomes(&self) -> Option<Receiver<OutputOutcome>> {
        self.outcome_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

fn installed() -> &'static Mutex<Option<OutputRoutingSlot>> {
    static SLOT: OnceLock<Mutex<Option<OutputRoutingSlot>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or with `None` clear) the run's routing slot for the control
/// plane (mirrors `source_demand::install_control_demand`).
pub fn install_output_routing(slot: Option<OutputRoutingSlot>) {
    *installed().lock().unwrap_or_else(PoisonError::into_inner) = slot;
}

/// The installed routing slot, if any.
#[must_use]
pub fn installed_output_routing() -> Option<OutputRoutingSlot> {
    installed()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The control plane's Program metrics source over a run's metrics slot.
pub struct ProgramMetricsAdapter(pub crate::pipeline::ProgramMetricsSlot);

impl multiview_control::program_metrics::ProgramMetricsSource for ProgramMetricsAdapter {
    fn read(&self) -> Option<multiview_control::program_metrics::ProgramCounters> {
        let handles = self.0.load()?;
        Some(multiview_control::program_metrics::ProgramCounters {
            lateness: handles.lateness.snapshot(),
            frames_repeated: handles
                .frames_repeated
                .load(std::sync::atomic::Ordering::Acquire),
        })
    }
}

fn installed_metrics() -> &'static Mutex<Option<crate::pipeline::ProgramMetricsSlot>> {
    static SLOT: OnceLock<Mutex<Option<crate::pipeline::ProgramMetricsSlot>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or clear) the run's Program metrics slot for the control plane.
pub fn install_program_metrics(slot: Option<crate::pipeline::ProgramMetricsSlot>) {
    *installed_metrics()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = slot;
}

/// The installed Program metrics slot, if any.
#[must_use]
pub fn installed_program_metrics() -> Option<crate::pipeline::ProgramMetricsSlot> {
    installed_metrics()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_outcome_has_its_wire_string() {
        assert_eq!(outcome_str(TakeResult::Applied), "applied");
        assert_eq!(
            outcome_str(TakeResult::Refused(RefuseReason::FeedNotFresh)),
            "refused:feed-not-fresh"
        );
        assert_eq!(
            outcome_str(TakeResult::Fallback(FallbackReason::AuxFailed)),
            "fallback:aux-failed"
        );
    }
}
