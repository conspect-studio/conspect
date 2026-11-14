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
    AudioPlanCost, LayoutBody, OutputLive, OutputOutcome, OutputReformatRequest, OutputRouting,
    OutputTakeRequest, PlanCost, PlanPath, PlanRefusal, ReformatSubmitError, RoutedAudio,
    RoutedFeed, RoutingLive, SlotPlan, SubtitleTakeError, SubtitleTakeRequest, TakeSubmitError,
};

use crate::output_routing::{
    FallbackReason, OutputRoutingHandle, OutputRoutingSlot, OutputTake, ReformatRefusal,
    RefuseReason, SubtitleRepoint, TakeOutcome, TakeResult, DEFAULT_MAX_ENCODERS,
};
use crate::routing_audio::AudioChoice;
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
        TakeResult::ResetApplied => "reset:applied",
        TakeResult::ResetRefused(ReformatRefusal::RestartRequired) => {
            "reset:refused:restart-required"
        }
        TakeResult::ResetRefused(ReformatRefusal::Capacity) => "reset:refused:capacity",
        TakeResult::ResetRefused(ReformatRefusal::EncoderNotReady) => {
            "reset:refused:encoder-not-ready"
        }
        TakeResult::ResetRefused(ReformatRefusal::Superseded) => "reset:refused:superseded",
    }
}

fn routed_feed(bus: &PictureBus) -> RoutedFeed {
    match bus {
        PictureBus::Program => RoutedFeed::Program,
        PictureBus::Preview => RoutedFeed::Preview,
        PictureBus::Clean => RoutedFeed::Clean,
        PictureBus::Source(id) => RoutedFeed::Source(id.clone()),
        PictureBus::Layout(layout) => RoutedFeed::Layout(layout.id.clone()),
    }
}

/// The engine bus of a take: a layout needs its resolved body (`None` for a
/// layout take without one, refused as unroutable).
fn picture_bus(
    feed: &RoutedFeed,
    layout: Option<&multiview_control::output_routing::LayoutBody>,
) -> Option<PictureBus> {
    Some(match feed {
        RoutedFeed::Program => PictureBus::Program,
        RoutedFeed::Preview => PictureBus::Preview,
        RoutedFeed::Clean => PictureBus::Clean,
        RoutedFeed::Source(id) => PictureBus::Source(id.clone()),
        RoutedFeed::Layout(id) => {
            let body = layout.filter(|b| b.0.id == *id)?;
            PictureBus::Layout(crate::routing_aux::LayoutRef::new(Arc::clone(&body.0)))
        }
    })
}

/// Whether `feed` reaches the output's encoder as the SAME picture `Arc`
/// (§12.4 P2, #342 review M1/M2): Program, Clean and a routed layout (all
/// canvas-sized) when the output's base format is the canvas (no convert); a source or the PVW source when its
/// newest tile passes the aux thread's own pass-through predicate
/// ([`crate::routing_aux::passes_through`]) at the output's feed format —
/// the very check the aux compose runs, so plan and engine cannot drift.
/// Wait-free.
fn is_identity(
    handle: &OutputRoutingHandle,
    run_output: &crate::output_routing::RunOutput,
    feed: &RoutedFeed,
) -> bool {
    let tile_passes = |id: &str| {
        handle.pictures().is_some_and(|pictures| {
            (pictures.stores)(id)
                .and_then(|store| store.slot().load())
                .is_some_and(|tile| {
                    crate::routing_aux::passes_through(
                        &tile,
                        pictures.canvas,
                        run_output.feed_format,
                    )
                })
        })
    };
    match feed {
        // A routed layout is composed at the CANVAS size like Program (#368:
        // never per output format); its key's worker converts to a
        // non-canvas base format. So it is the same picture exactly when the
        // output's base format is the canvas.
        RoutedFeed::Program | RoutedFeed::Clean | RoutedFeed::Layout(_) => {
            run_output.base_format.is_none()
        }
        RoutedFeed::Source(id) => tile_passes(id),
        RoutedFeed::Preview => handle
            .pictures()
            .and_then(|p| p.preview_selection.as_ref())
            .and_then(|selection| selection.load())
            .is_some_and(|id| tile_passes(&id)),
    }
}

/// The engine audio choice of a control-plane audio feed.
fn audio_choice(audio: &RoutedAudio) -> AudioChoice {
    match audio {
        RoutedAudio::Follow => AudioChoice::Follow,
        RoutedAudio::Program => AudioChoice::Program,
        RoutedAudio::None => AudioChoice::Silence,
        RoutedAudio::Source(id) => AudioChoice::Source(id.clone()),
        RoutedAudio::Track { name, input } => AudioChoice::Track {
            name: name.clone(),
            input: input.clone(),
        },
    }
}

/// The control-plane audio feed of an engine audio choice.
fn routed_audio(choice: &AudioChoice) -> RoutedAudio {
    match choice {
        AudioChoice::Follow => RoutedAudio::Follow,
        AudioChoice::Program => RoutedAudio::Program,
        AudioChoice::Silence => RoutedAudio::None,
        AudioChoice::Source(id) => RoutedAudio::Source(id.clone()),
        AudioChoice::Track { name, input } => RoutedAudio::Track {
            name: name.clone(),
            input: input.clone(),
        },
    }
}

/// The carried-audio label (`program`, `preview`, `none`, `src:<id>`,
/// `track:<name>`) of choice label `choice` on picture bus `bus` — the
/// engine's own resolution (§5.1), over snapshot labels.
fn carried_label(choice: &str, bus: &str) -> String {
    if choice != "follow" {
        return choice.to_owned();
    }
    match bus {
        "preview" => "preview".to_owned(),
        b if b.starts_with("src:") => b.to_owned(),
        // Program and Clean follow the program mix.
        _ => "program".to_owned(),
    }
}

/// The engine audio of a take: `None` keeps the output's choice. `follow`
/// on an output without a program-audio slot is that output's only (and
/// current) audio, so it is not sent (the engine would refuse an explicit
/// audio there): a slice-1 body `audio: "follow"` keeps working.
fn take_audio(audio: Option<&RoutedAudio>, program_audio: bool) -> Option<AudioChoice> {
    match audio {
        Some(RoutedAudio::Follow) if !program_audio => None,
        other => other.map(audio_choice),
    }
}

/// Whether an encoded bus with key (picture, format, settings) — the key
/// `run_output` would use for `feed` — is carried by a live key (§4.3,
/// §12.4 P1): an output with the same settings carries it at the same feed
/// name, or a live (lingering) encoder for (feed name, settings) runs. The one predicate behind the plan's +0
/// and the readback's P1 (#342 review M5).
fn carried_by_live_key(
    handle: &OutputRoutingHandle,
    snapshot: &crate::output_routing::RoutingSnapshot,
    run_output: &crate::output_routing::RunOutput,
    feed: &RoutedFeed,
) -> bool {
    let bus = feed.bus_name();
    // #342: a routed source/PVW is encoded at the output's feed format, so
    // its key is (bus@format, settings); Program/Clean stay the bus.
    let routed = matches!(feed, RoutedFeed::Source(_) | RoutedFeed::Preview);
    let key_name = |format: Option<crate::routing_aux::AuxFormat>| -> String {
        if routed {
            // Only a source/PVW feed is routed here; it never needs a body.
            picture_bus(feed, None).map_or_else(
                || bus.clone(),
                |b| crate::routing_aux::feed_name(&b, format),
            )
        } else {
            bus.clone()
        }
    };
    let wanted = key_name(run_output.feed_format);
    handle.outputs().iter().enumerate().any(|(i, o)| {
        o.settings == run_output.settings
            && key_name(o.feed_format) == wanted
            && snapshot.outputs.get(i).is_some_and(|v| v.bus == bus)
    }) || snapshot
        .live_keys
        .iter()
        .any(|(b, settings)| *b == wanted && *settings == run_output.settings)
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
            audio: outcome.audio.as_ref().map(routed_audio),
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
        // §7 rule 1: a layout not composing for another output needs a free
        // aux canvas; this output's own layout is replaced by the take.
        let own = snapshot.outputs.get(index).map(|v| v.bus.as_str());
        let held_by_others: Vec<&str> = snapshot
            .outputs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .flat_map(|(_, v)| {
                std::iter::once(v.bus.as_str()).chain(v.pending.as_ref().map(|p| p.bus.as_str()))
            })
            .filter(|b| b.starts_with("aux:"))
            .collect();
        let mut distinct = held_by_others.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let needs_aux_canvas = matches!(feed, RoutedFeed::Layout(_))
            && own != Some(bus.as_str())
            && !distinct.contains(&bus.as_str());
        if matches!(feed, RoutedFeed::Layout(_))
            && !distinct.contains(&bus.as_str())
            && distinct.len() >= handle.max_aux_canvases()
        {
            return Err(PlanRefusal::AuxCapacity {
                active: distinct.len(),
                max: handle.max_aux_canvases(),
            });
        }
        let shared = *feed == RoutedFeed::Program
            || carried_by_live_key(&handle, &snapshot, run_output, feed);
        Ok(PlanCost {
            needs_encoder: !shared,
            sessions_in_use: snapshot
                .dynamic_encoders_live
                .saturating_add(snapshot.opens_in_flight),
            sessions_max: self.max_encoders,
            needs_aux_canvas,
        })
    }

    /// §12.4 path of a crosspoint onto the output's video slot 0 (#342 B2):
    /// P1 when an encoded bus with key (picture, format, settings) is live
    /// (Program at the output's format is always its base key), P2 when the
    /// picture is identity at the output's format, else P3 (+1 convert unless
    /// that (picture, format) feed is already live). Wait-free: one snapshot
    /// load and, for a source, one newest-tile load.
    fn plan_slot(&self, output: &str, feed: &RoutedFeed) -> Result<SlotPlan, PlanRefusal> {
        let cost = self.plan(output, feed)?;
        let handle = self.handle().ok_or(PlanRefusal::OutputKindUnroutable)?;
        let run_output = handle
            .index_of(output)
            .and_then(|index| handle.outputs().get(index).cloned())
            .ok_or(PlanRefusal::OutputKindUnroutable)?;
        if !cost.needs_encoder {
            return Ok(SlotPlan {
                path: PlanPath::P1,
                cost,
                converts: 0,
            });
        }
        let identity = is_identity(&handle, &run_output, feed);
        if identity {
            return Ok(SlotPlan {
                path: PlanPath::P2,
                cost,
                converts: 0,
            });
        }
        let feed_live = matches!(feed, RoutedFeed::Source(_) | RoutedFeed::Preview) && {
            picture_bus(feed, None).is_some_and(|b| {
                let name = crate::routing_aux::feed_name(&b, run_output.feed_format);
                handle.snapshot().live_keys.iter().any(|(n, _)| *n == name)
            })
        };
        Ok(SlotPlan {
            path: PlanPath::P3,
            cost,
            converts: u8::from(!feed_live),
        })
    }

    fn current_path(&self, output: &str) -> Option<PlanPath> {
        let handle = self.handle()?;
        let index = handle.index_of(output)?;
        let run_output = handle.outputs().get(index)?.clone();
        let snapshot = handle.snapshot();
        let feed = RoutedFeed::from_bus_name(&snapshot.outputs.get(index)?.bus)?;
        // P1 exactly when the plan would say +0 for this feed (#342 review
        // M5): Program (its base key) or a live key of its own (feed name,
        // settings) carries the output's current bus.
        Some(
            if feed == RoutedFeed::Program
                || carried_by_live_key(&handle, &snapshot, &run_output, &feed)
            {
                PlanPath::P1
            } else if is_identity(&handle, &run_output, &feed) {
                PlanPath::P2
            } else {
                PlanPath::P3
            },
        )
    }

    fn plan_audio(
        &self,
        output: &str,
        video: Option<&RoutedFeed>,
        audio: Option<&RoutedAudio>,
    ) -> Result<AudioPlanCost, PlanRefusal> {
        let Some(handle) = self.handle() else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        let Some(index) = handle.index_of(output) else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        let Some(run_output) = handle.outputs().get(index) else {
            return Err(PlanRefusal::OutputKindUnroutable);
        };
        // The same kind gate as a video take: an output the engine does not
        // route (VAAPI/VideoToolbox, §4.2) takes no audio either.
        if !run_output.routable {
            return Err(PlanRefusal::OutputKindUnroutable);
        }
        let snapshot = handle.snapshot();
        audio_cost(
            run_output.program_audio,
            snapshot.outputs.get(index),
            video,
            audio,
        )
    }

    /// A live format reset (#342 B3): the new config's encoded size, fit and
    /// video bitrate for the SAME encoder; a codec change, an output the run
    /// does not route or a kind whose container carries parameter sets out
    /// of band (refused by the router) is restart-required.
    fn try_reformat(&self, request: OutputReformatRequest) -> Result<(), ReformatSubmitError> {
        let handle = self.handle().ok_or(ReformatSubmitError::RestartRequired)?;
        let index = handle
            .index_of(&request.output)
            .ok_or(ReformatSubmitError::RestartRequired)?;
        let run_output = handle
            .outputs()
            .get(index)
            .ok_or(ReformatSubmitError::RestartRequired)?;
        let pictures = handle
            .pictures()
            .ok_or(ReformatSubmitError::RestartRequired)?;
        if !run_output.routable {
            return Err(ReformatSubmitError::RestartRequired);
        }
        // The SAME codec: the new config's slot-0 codec (absent = the output
        // codec) must resolve to the running encoder's family.
        let requested = request
            .config
            .video_slot0()
            .and_then(|v| v.codec.as_deref())
            .or_else(|| crate::pipeline::configured_codec(&request.config));
        let same = requested.is_some_and(|token| {
            multiview_config::CodecFamily::of(token)
                == multiview_config::CodecFamily::of(&run_output.codec_name)
        });
        if !same {
            return Err(ReformatSubmitError::RestartRequired);
        }
        let canvas = pictures.canvas;
        let base_format = crate::routing_aux::AuxFormat::base_of_output(&request.config, canvas);
        let feed_format = crate::routing_aux::AuxFormat::feed_of_output(&request.config, canvas);
        let (width, height) =
            base_format.map_or((canvas.width, canvas.height), |f| (f.width, f.height));
        let kbps = request
            .config
            .video_slot0()
            .and_then(|v| v.bitrate_kbps)
            .unwrap_or(crate::pipeline::DEFAULT_VIDEO_KBPS);
        let bit_rate = usize::try_from(kbps)
            .ok()
            .and_then(|k| k.checked_mul(1000))
            .ok_or(ReformatSubmitError::RestartRequired)?;
        handle
            .try_reformat(crate::output_routing::Reformat {
                operation_id: request.operation_id,
                output_index: index,
                settings: crate::output_routing::ReformatSettings {
                    codec_name: run_output.codec_name.clone(),
                    width,
                    height,
                    bit_rate,
                    base_format,
                },
                feed_format,
                principal: request.principal,
            })
            .map_err(|_| ReformatSubmitError::Busy)
    }

    fn plan_layout_audio(
        &self,
        output: &str,
        video: &RoutedFeed,
        layout_audio: &multiview_config::LayoutAudio,
        audio: Option<&RoutedAudio>,
    ) -> Result<AudioPlanCost, PlanRefusal> {
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
        let snapshot = handle.snapshot();
        let label = layout_follow_label(layout_audio);
        audio_cost_following(
            run_output.program_audio,
            snapshot.outputs.get(index),
            Some(video),
            audio,
            Some(&label),
        )
    }

    fn layout_changed(&self, id: &str, body: Option<LayoutBody>) -> bool {
        // No run: nothing composes the layout, nothing to tell.
        let Some(handle) = self.handle() else {
            return true;
        };
        let change = match body {
            Some(body) if body.0.id == id => crate::output_routing::LayoutChange::Edited(
                crate::routing_aux::LayoutRef::new(Arc::clone(&body.0)),
            ),
            Some(_) => return false,
            None => crate::output_routing::LayoutChange::Removed {
                id: id.to_owned(),
                revision: crate::output_routing::next_layout_revision(),
            },
        };
        handle.try_layout_change(change).is_ok()
    }

    fn routed_layouts(&self) -> Vec<String> {
        self.handle().map_or_else(Vec::new, |handle| {
            handle
                .routed_layouts()
                .into_iter()
                .filter_map(|bus| bus.strip_prefix("aux:").map(str::to_owned))
                .collect()
        })
    }

    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError> {
        let handle = self.handle().ok_or(TakeSubmitError::Busy)?;
        let index = handle
            .index_of(&take.output)
            .ok_or(TakeSubmitError::Unroutable)?;
        let run_output = handle
            .outputs()
            .get(index)
            .ok_or(TakeSubmitError::Unroutable)?;
        // Every take — video, combined or audio-only — honours the kind gate.
        if !run_output.routable {
            return Err(TakeSubmitError::Unroutable);
        }
        let program_audio = run_output.program_audio;
        let audio = take_audio(take.audio.as_ref(), program_audio);
        if audio.is_some() && !program_audio {
            // The engine would refuse it `unroutable`; the control plane
            // already answered `audio-unsupported` from the plan.
            return Err(TakeSubmitError::Unroutable);
        }
        let bus = match take.video.as_ref() {
            Some(feed) => {
                Some(picture_bus(feed, take.layout.as_ref()).ok_or(TakeSubmitError::Unroutable)?)
            }
            None => None,
        };
        if bus.is_none() && audio.is_none() {
            return Err(TakeSubmitError::Unroutable);
        }
        handle
            .try_take(OutputTake {
                operation_id: take.operation_id,
                output_index: index,
                bus,
                audio,
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
                        audio: run_output.program_audio.then(|| view.audio.clone()),
                        pending_audio: view.pending.as_ref().and_then(|p| p.audio.clone()),
                        private_audio: view.private_audio,
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

    /// Slice F part F2: re-point the output's subtitle route table (RCU,
    /// wait-free for the off-clock sampler, no channel), under the run's
    /// subtitle gate: a run that ended, is tearing down, whose consumer
    /// exited, or was replaced refuses `Busy` and is never mutated (review
    /// INV-1, INV2-1). The answer is a completed-tick lower bound observed
    /// before the swap (`0`: possibly none), not a landing tick; see
    /// [`OutputRoutingHandle::repoint_subtitles`] (review COR2-1/INV2-2).
    fn try_take_subtitles(&self, take: SubtitleTakeRequest) -> Result<u64, SubtitleTakeError> {
        let handle = self.slot.load().ok_or(SubtitleTakeError::Busy)?;
        handle
            .repoint_subtitles(&take.output, &take.slots)
            .map_err(|refusal| match refusal {
                SubtitleRepoint::Ended => SubtitleTakeError::Busy,
                SubtitleRepoint::NotInRun => SubtitleTakeError::NotInRun,
            })
    }

    fn subtitle_routes(&self, output: &str) -> Option<Vec<Option<String>>> {
        self.slot.load()?.subtitle_table(output)
    }

    fn subscribe_outcomes(&self) -> Option<Receiver<OutputOutcome>> {
        self.outcome_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The audio cost of a take on one output (§5.2, §7): +1 transient CPU AAC
/// encode when the take changes what the output carries and it has no
/// private audio bus yet; 0 when the target is what it carries or a private
/// bus already runs. An explicit audio choice on an output without a
/// program-audio slot is refused.
fn audio_cost(
    program_audio: bool,
    view: Option<&crate::output_routing::OutputRouteView>,
    video: Option<&RoutedFeed>,
    audio: Option<&RoutedAudio>,
) -> Result<AudioPlanCost, PlanRefusal> {
    audio_cost_following(program_audio, view, video, audio, None)
}

/// The carried label `follow` resolves to on a routed layout whose `audio`
/// attribute is `audio` (the engine's `AudioSource::follow`, §5.1).
fn layout_follow_label(audio: &multiview_config::LayoutAudio) -> String {
    match audio {
        multiview_config::LayoutAudio::None => "none".to_owned(),
        multiview_config::LayoutAudio::Source(id) => format!("src:{id}"),
        _ => "program".to_owned(),
    }
}

/// [`audio_cost`] where `follow` on the new video resolves to
/// `follow_label` when given (a routed layout's attribute).
fn audio_cost_following(
    program_audio: bool,
    view: Option<&crate::output_routing::OutputRouteView>,
    video: Option<&RoutedFeed>,
    audio: Option<&RoutedAudio>,
    follow_label: Option<&str>,
) -> Result<AudioPlanCost, PlanRefusal> {
    if take_audio(audio, program_audio).is_some() && !program_audio {
        return Err(PlanRefusal::AudioUnsupported);
    }
    let Some(view) = view.filter(|_| program_audio) else {
        // No program audio on this output: nothing to encode.
        return Ok(AudioPlanCost::default());
    };
    let choice = audio.map_or_else(|| view.audio_choice.clone(), RoutedAudio::label);
    let bus = video.map_or_else(|| view.bus.clone(), RoutedFeed::bus_name);
    let carried = match follow_label {
        Some(label) if choice == "follow" => label.to_owned(),
        _ => carried_label(&choice, &bus),
    };
    let changes = carried != view.audio;
    Ok(AudioPlanCost {
        needs_encoder: changes && !view.private_audio,
    })
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
            phases: handles.phases.snapshot(),
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

    fn view(
        bus: &str,
        audio: &str,
        choice: &str,
        private: bool,
    ) -> crate::output_routing::OutputRouteView {
        crate::output_routing::OutputRouteView {
            bus: bus.to_owned(),
            key: String::new(),
            since_tick: 0,
            pending: None,
            degraded: false,
            audio: audio.to_owned(),
            audio_choice: choice.to_owned(),
            private_audio: private,
        }
    }

    #[test]
    fn a_track_maps_to_the_engine_track_choice_with_its_route_input() {
        let track = RoutedAudio::Track {
            name: "eng".into(),
            input: "cam1".into(),
        };
        assert_eq!(
            take_audio(Some(&track), true),
            Some(AudioChoice::Track {
                name: "eng".into(),
                input: "cam1".into(),
            })
        );
        assert_eq!(
            take_audio(Some(&RoutedAudio::None), true),
            Some(AudioChoice::Silence)
        );
        assert_eq!(routed_audio(&audio_choice(&track)), track);
        // A slice-1 `follow` on an output without the slot changes nothing.
        assert_eq!(take_audio(Some(&RoutedAudio::Follow), false), None);
        assert_eq!(
            take_audio(Some(&RoutedAudio::Follow), true),
            Some(AudioChoice::Follow)
        );
    }

    #[test]
    fn program_audio_drives_the_audio_unsupported_refusal() {
        let v = view("program", "program", "follow", false);
        let src = RoutedAudio::Source("x".into());
        assert_eq!(
            audio_cost(false, Some(&v), None, Some(&src)),
            Err(PlanRefusal::AudioUnsupported)
        );
        assert_eq!(
            audio_cost(false, Some(&v), None, Some(&RoutedAudio::Follow)),
            Ok(AudioPlanCost::default()),
            "follow is what a slotless output carries"
        );
        assert_eq!(
            audio_cost(true, Some(&v), None, Some(&src)),
            Ok(AudioPlanCost {
                needs_encoder: true
            })
        );
    }

    #[test]
    fn audio_cost_is_zero_for_the_carried_audio_or_a_running_private_bus() {
        let src = RoutedAudio::Source("x".into());
        let on_x = view("program", "src:x", "src:x", true);
        assert_eq!(
            audio_cost(true, Some(&on_x), None, Some(&src)),
            Ok(AudioPlanCost::default())
        );
        // A running private bus absorbs a change.
        assert_eq!(
            audio_cost(true, Some(&on_x), None, Some(&RoutedAudio::None)),
            Ok(AudioPlanCost::default())
        );
        // Follow of a video take to a source changes the audio: +1.
        let plain = view("program", "program", "follow", false);
        assert_eq!(
            audio_cost(
                true,
                Some(&plain),
                Some(&RoutedFeed::Source("x".into())),
                None
            ),
            Ok(AudioPlanCost {
                needs_encoder: true
            })
        );
        assert_eq!(
            audio_cost(true, Some(&plain), Some(&RoutedFeed::Clean), None),
            Ok(AudioPlanCost::default()),
            "Clean follows the program mix it already carries"
        );
    }

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
