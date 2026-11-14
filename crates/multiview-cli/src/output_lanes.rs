//! **Rate lanes**: outputs at a frame rate other than Program's (slice C,
//! #343, `docs/architecture/output-routing.md` §12.6, ADR-0109).
//!
//! An output whose `[outputs.format] fps` differs from the canvas leaves the
//! Program egress entirely. Per distinct rate one **output clock**
//! ([`multiview_engine::spawn_output_lane`], its own named clock thread on the
//! run's shared [`TimeSource`], seeded with Program's seed) latches the newest
//! baked Program frame not after its tick's PTS from the
//! [`ProgramLatch`] and offers it, non-blocking, to each of its outputs'
//! encode workers. Per output:
//!
//! * one [`RenditionWorker`] whose encoder runs at the output's rate and
//!   format ([`LaneVideo`]: format convert + gap fill, PTS = DTS = the output
//!   tick, never a float);
//! * one lane consumer thread with its own [`RenditionDispatcher`] (cadence =
//!   the output rate) and [`BindingDelivery`], receiving its worker's video
//!   events and the **Program AAC packets** fanned out from Program's audio
//!   encode worker ([`ProgramTaps::fan_audio`], a bounded `try_send` per lane:
//!   the same samples on the same global 1024 grid, zero extra encode);
//! * the ordinary sink thread (`run_one_output`).
//!
//! Isolation: Program does one wait-free [`ProgramLatch::publish`] per baked
//! tick and one `try_send` per lane per audio tick, both on the bake consumer /
//! audio worker (never the Program clock), and only when a lane exists. A
//! wedged or failed lane clock, worker, consumer or sink ends only its outputs;
//! nothing on the Program side ever waits on a lane. Run end: after Program's
//! egress joined, each lane runs on until it passed the last Program frame
//! (bounded), then stops; every join is bounded and detaches beyond its grace.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{
    spawn_output_lane, LanePacing, OutputLaneClock, OutputLaneHandle, ProgramLatch, StopSignal,
    Tick, TimeSource, LATCH_DELAY,
};
use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters};
use multiview_output::batch::RecoveryGate;
use multiview_output::sink::{EncodeConfig, VideoRenditionEncoder};

use super::{
    codec_for_encoder, recovery_gate, run_one_output, OutputClockMetrics, PipelineError,
    PipelineVideoWorker, RunnableOutput, SinkRunOutcome, SinkRunner, SINK_WEDGE_GRACE,
};
use crate::binding_delivery::{
    BindingDelivery, BindingPolicy, BindingStatus, DeliveryLimits, WindowDelivery,
};
use crate::format_convert::{black_at, FormatConvert};
use crate::rendition_dispatch::{
    BindingConfig, DispatchConfig, DispatchError, DtsTimeBase, RenditionDispatcher,
};
use crate::rendition_workers::{BakedTick, DispatchEvent, RenditionWorker, VideoEncodeWorker};
use crate::routing_aux::{AuxCanvas, AuxFormat};

/// Program audio events buffered per lane (about 2 s at 30 fps). Full =
/// dropped and counted on that lane; Program never waits.
const LANE_AUDIO_QUEUE: usize = 64;
/// Video events between one lane worker and its consumer (lane-internal).
const LANE_VIDEO_QUEUE: usize = 64;
/// The largest tick gap a lane worker fills, in seconds of its own ticks; a
/// larger gap fails that output alone.
const LANE_GAP_SECONDS: u64 = 2;
/// At run end, how long (wall time) a lane may take to pass the last Program
/// frame before it is stopped anyway: the latch delay plus a wide margin.
const LANE_DRAIN_GRACE: Duration = Duration::from_secs(2);
/// The lane consumer's receive step (it also runs the staging watchdog).
const CONSUMER_STEP: Duration = Duration::from_millis(5);

/// A rate output as the build planned it (everything but the run).
pub(super) struct LaneOutputSpec {
    /// The output's config id.
    pub(super) id: String,
    /// The runnable sink.
    pub(super) output: RunnableOutput,
    /// The encoder settings at the output's rate (cadence, GOP, size).
    pub(super) encode: EncodeConfig,
    /// The output's picture format (`None` = the canvas size).
    pub(super) format: Option<AuxFormat>,
    /// The audio streams the output carries (program = 0, track i = i + 1).
    pub(super) audio_tracks: Vec<usize>,
    /// Its muxer reads decoder configuration in band.
    pub(super) inband: bool,
}

/// Test seams (all `None`/empty in production).
#[derive(Default)]
pub(crate) struct LaneHooks {
    /// Lane pacing override ([`LanePacing::Stepped`] on a manual clock).
    pub(crate) pacing: Option<LanePacing>,
    /// Run first in every lane tick (a chaos test wedges it).
    pub(crate) on_tick: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// Run first in every lane encode (a chaos test panics in it).
    pub(crate) on_encode: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// Observes each lane tick's latch: (output tick, latched Program tick).
    pub(crate) on_latch: Option<Arc<dyn Fn(u64, Option<u64>) + Send + Sync>>,
    /// Latch delay override (tests on a loaded runner widen the margin).
    pub(crate) latch_delay: Option<Duration>,
    /// Replacement sink runners, one per rate output in config order.
    pub(crate) runners: Vec<SinkRunner>,
}

/// One lane's Program audio tap.
struct AudioTap {
    tx: SyncSender<DispatchEvent>,
    dropped: Arc<AtomicU64>,
}

/// What Program's bake consumer and audio worker feed the lanes. Empty (no
/// latch, no tap: zero work) for a run without a rate output.
#[derive(Clone, Default)]
pub(super) struct ProgramTaps {
    latch: Option<Arc<ProgramLatch<Nv12Image>>>,
    audio: Arc<Vec<AudioTap>>,
}

impl ProgramTaps {
    /// Publish one baked Program frame (wait-free; a no-op without a lane).
    pub(super) fn publish(&self, tick: u64, pts: MediaTime, frame: &Arc<Nv12Image>) {
        if let Some(latch) = self.latch.as_ref() {
            latch.publish(tick, pts, Arc::clone(frame));
        }
    }

    /// Fan one Program audio event to every lane: a bounded `try_send` each,
    /// full or gone = dropped and counted on that lane. Never blocks.
    pub(super) fn fan_audio(&self, event: &DispatchEvent) {
        for tap in self.audio.iter() {
            if tap.tx.try_send(event.clone()).is_err() {
                tap.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Split the runnable outputs into Program's (its egress, unchanged) and the
/// rate outputs (`[outputs.format] fps` other than the canvas, #343).
pub(super) fn split_rate_outputs(
    outputs: Vec<RunnableOutput>,
    config: &multiview_config::MultiviewConfig,
) -> (Vec<RunnableOutput>, Vec<RunnableOutput>) {
    outputs.into_iter().partition(|runnable| {
        config
            .outputs
            .iter()
            .find(|o| o.id() == runnable.id())
            .and_then(|o| multiview_config::output_rate(o, &config.canvas))
            .is_none()
    })
}

/// Plan each rate output: its encoder at its own rate (GOP = one second of
/// its ticks), size and bit rate; its audio selection; its muxer's in-band
/// rule. An explicit audio declaration joins `declared_audio_outputs` (a run
/// without program audio then fails, exactly like a Program-rate output).
///
/// # Errors
/// An unknown codec or an out-of-range bit rate (as for Program's outputs).
pub(super) fn rate_output_specs(
    runnables: Vec<RunnableOutput>,
    config: &multiview_config::MultiviewConfig,
    base: &EncodeConfig,
    canvas: AuxCanvas,
    declared_audio_outputs: &mut Vec<(String, Vec<usize>)>,
) -> Result<Vec<LaneOutputSpec>, PipelineError> {
    let mut specs = Vec::with_capacity(runnables.len());
    for output in runnables {
        let id = output.id().to_owned();
        let Some(declared) = config.outputs.iter().find(|o| o.id() == id) else {
            continue;
        };
        let Some(rate) = multiview_config::output_rate(declared, &config.canvas) else {
            continue;
        };
        let rate = rate.rational().reduce();
        let requested = declared
            .video_slot0()
            .and_then(|v| v.codec.as_deref())
            .or_else(|| super::output_codec(declared))
            .unwrap_or("mpeg2video");
        let concrete = super::resolve_encoder(requested)?;
        let format = AuxFormat::base_of_output(declared, canvas);
        let (width, height) = format.map_or((base.width, base.height), |f| (f.width, f.height));
        let bit_rate = declared
            .video_slot0()
            .and_then(|v| v.bitrate_kbps)
            .map_or(Ok(base.bit_rate), |kbps| {
                usize::try_from(kbps)
                    .ok()
                    .and_then(|kbps| kbps.checked_mul(1000))
                    .filter(|_| kbps <= multiview_config::MAX_SLOT_BITRATE_KBPS)
                    .ok_or_else(|| PipelineError::Codec {
                        codec: requested.to_owned(),
                        reason: format!("video bit rate {kbps} kbit/s is out of range"),
                    })
            })?;
        let mut encode = base.clone();
        encode.codec_name = concrete.name;
        encode.format = concrete.format;
        encode.width = width;
        encode.height = height;
        encode.bit_rate = bit_rate;
        encode.cadence = rate;
        encode.gop = super::ticks_per_second(rate).max(1);
        encode.audio = None;
        let ll_hls = matches!(declared, multiview_config::Output::LlHls { .. });
        let inband = !ll_hls && super::inband_parameter_changes_ok(&output);
        let audio_tracks = match &output {
            RunnableOutput::File { sink, .. }
            | RunnableOutput::Hls { sink, .. }
            | RunnableOutput::Push { sink, .. } => {
                let selection = sink.audio_selection();
                let mut indexes = Vec::with_capacity(selection.tracks.len() + 1);
                if selection.program {
                    indexes.push(0);
                }
                indexes.extend(
                    selection
                        .tracks
                        .iter()
                        .map(|track| usize::from(track.index) + 1),
                );
                indexes
            }
            #[allow(unreachable_patterns)]
            _ => Vec::new(),
        };
        if declared.effective_audio().is_some() && !audio_tracks.is_empty() {
            declared_audio_outputs.push((id.clone(), audio_tracks.clone()));
        }
        specs.push(LaneOutputSpec {
            id,
            output,
            encode,
            format,
            audio_tracks,
            inband,
        });
    }
    Ok(specs)
}

/// A rate output's entry in the run's output map: addressable by id, never
/// routable (a take answers `unroutable`; takes onto rate outputs are a
/// follow-up of #343).
pub(super) fn unroutable_run_output(spec: &LaneOutputSpec) -> crate::output_routing::RunOutput {
    crate::output_routing::RunOutput {
        id: Some(spec.id.clone()),
        settings: usize::MAX,
        feed_format: spec.format,
        base_format: spec.format,
        routable: false,
        program_audio: false,
        codec_name: spec.encode.codec_name.clone(),
    }
}

/// Program's audio streams, as the lanes mux them.
pub(super) struct LaneAudio {
    /// The program AAC stream (`None`: the run carries no program audio).
    pub(super) program: Option<(StreamCodecParameters, Rational)>,
    /// The discrete tracks.
    pub(super) tracks: Vec<(StreamCodecParameters, Rational)>,
}

impl LaneAudio {
    fn bases(&self) -> Vec<DtsTimeBase> {
        self.program.as_ref().map_or_else(Vec::new, |(_, tb)| {
            std::iter::repeat_n(
                DtsTimeBase {
                    num: tb.num,
                    den: tb.den,
                },
                self.tracks.len() + 1,
            )
            .collect()
        })
    }
}

/// One rate output with its encoder opened (before the run starts).
struct PreparedOutput {
    id: String,
    rate: Rational,
    encoder: VideoRenditionEncoder,
    format: Option<AuxFormat>,
    black: Arc<Nv12Image>,
    runner: SinkRunner,
    policy: BindingPolicy,
    gate: Option<RecoveryGate>,
    audio_tracks: Vec<usize>,
    inband: bool,
    audio_rx: Option<Receiver<DispatchEvent>>,
    dropped_audio: Arc<AtomicU64>,
}

/// The run's rate lanes, opened and ready to start once Program is seeded.
pub(super) struct PreparedLanes {
    outputs: Vec<PreparedOutput>,
    latch: Arc<ProgramLatch<Nv12Image>>,
    program_cadence: Rational,
    audio: LaneAudio,
    hooks: LaneHooks,
    /// Outputs that could not even open (reported, never fatal to Program).
    lines: Vec<String>,
    push_links: Vec<(String, Arc<multiview_output::push_link::PushLinkStatus>)>,
}

/// Open every rate output's encoder (off the clock, before the run). An
/// output whose encoder cannot open is reported and skipped: a lane failure
/// never fails the Program run. Returns `None` (no taps, zero Program work)
/// when the run has no rate output.
pub(super) fn prepare(
    specs: Vec<LaneOutputSpec>,
    program_cadence: Rational,
    canvas: AuxCanvas,
    audio: LaneAudio,
    mut hooks: LaneHooks,
) -> Option<(PreparedLanes, ProgramTaps)> {
    if specs.is_empty() {
        return None;
    }
    let latch_delay = hooks.latch_delay.unwrap_or(LATCH_DELAY);
    let latch = Arc::new(ProgramLatch::for_program(program_cadence, latch_delay));
    let mut runners = std::mem::take(&mut hooks.runners).into_iter();
    let mut outputs = Vec::with_capacity(specs.len());
    let mut taps = Vec::with_capacity(specs.len());
    let mut lines = Vec::new();
    let mut push_links = Vec::new();
    for spec in specs {
        let test_runner = runners.next();
        let encoder = match VideoRenditionEncoder::new(&spec.encode) {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::error!(output = %spec.id, %error, "rate output: encoder did not open; output skipped");
                lines.push(format!(
                    "{}: rate output FAILED (encoder did not open: {error})",
                    spec.id
                ));
                continue;
            }
        };
        let picture = spec
            .format
            .unwrap_or_else(|| AuxFormat::new(canvas.width, canvas.height, Default::default()));
        let Some(black) = black_at(picture, canvas.color) else {
            lines.push(format!(
                "{}: rate output FAILED (cannot allocate a {}x{} picture)",
                spec.id, picture.width, picture.height
            ));
            continue;
        };
        let policy = match &spec.output {
            RunnableOutput::Push { id, link, .. } => {
                push_links.push((id.clone(), Arc::clone(link)));
                BindingPolicy::Push(Arc::clone(link))
            }
            _ => BindingPolicy::Bounded,
        };
        let runner: SinkRunner = match test_runner {
            Some(runner) => runner,
            None => {
                let output = spec.output;
                Box::new(move |rx, params, time_base, audio, tracks| {
                    run_one_output(output, rx, &params, time_base, audio.as_ref(), &tracks)
                })
            }
        };
        let dropped_audio = Arc::new(AtomicU64::new(0));
        let audio_rx = audio.program.is_some().then(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel(LANE_AUDIO_QUEUE);
            taps.push(AudioTap {
                tx,
                dropped: Arc::clone(&dropped_audio),
            });
            rx
        });
        let audio_tracks = if audio.program.is_some() {
            spec.audio_tracks
        } else {
            Vec::new()
        };
        outputs.push(PreparedOutput {
            id: spec.id,
            rate: spec.encode.cadence,
            gate: recovery_gate(codec_for_encoder(&spec.encode.codec_name)),
            encoder,
            format: spec.format,
            black,
            runner,
            policy,
            audio_tracks,
            inband: spec.inband,
            audio_rx,
            dropped_audio,
        });
    }
    let program_taps = ProgramTaps {
        latch: Some(Arc::clone(&latch)),
        audio: Arc::new(taps),
    };
    Some((
        PreparedLanes {
            outputs,
            latch,
            program_cadence,
            audio,
            hooks,
            lines,
            push_links,
        },
        program_taps,
    ))
}

impl PreparedLanes {
    /// The push links of the rate outputs (for the status registry).
    pub(super) fn push_links(
        &self,
    ) -> Vec<(String, Arc<multiview_output::push_link::PushLinkStatus>)> {
        self.push_links.clone()
    }

    /// Start every lane: per output its worker, consumer and sink; per
    /// distinct rate one output clock seeded with Program's `seed_nanos` on
    /// `time`. Called once Program's seed is known, before its first tick
    /// completes. A lane that cannot start is reported, never fatal.
    pub(super) fn start(self, time: &Arc<dyn TimeSource>, seed_nanos: i64) -> RunningLanes {
        let PreparedLanes {
            outputs,
            latch,
            program_cadence,
            audio,
            hooks,
            mut lines,
            push_links: _,
        } = self;
        let stop = StopSignal::new();
        let end = Arc::new(RunEnd::default());
        let audio_bases = audio.bases();
        let mut running_outputs = Vec::new();
        // Per distinct rate: (rate, ids, offer targets).
        let mut groups: Vec<(Rational, Vec<String>, Vec<OfferTarget>)> = Vec::new();
        for output in outputs {
            let rate = output.rate;
            let id = output.id.clone();
            match start_output(
                output,
                &audio,
                &audio_bases,
                program_cadence,
                hooks.on_encode.clone(),
            ) {
                Ok((running, target)) => {
                    running_outputs.push(running);
                    if let Some(group) = groups.iter_mut().find(|(r, _, _)| *r == rate) {
                        group.1.push(id);
                        group.2.push(target);
                    } else {
                        groups.push((rate, vec![id], vec![target]));
                    }
                }
                Err(error) => {
                    tracing::error!(output = %id, %error, "rate output: did not start; output skipped");
                    lines.push(format!("{id}: rate output FAILED (did not start: {error})"));
                }
            }
        }
        let mut clocks = Vec::with_capacity(groups.len());
        let mut metrics = Vec::with_capacity(groups.len());
        for (rate, ids, targets) in groups {
            let dropped_offers = Arc::new(AtomicU64::new(0));
            let late_latches = Arc::new(AtomicU64::new(0));
            let dropped_audio: Vec<Arc<AtomicU64>> =
                targets.iter().map(|t| Arc::clone(&t.dropped_audio)).collect();
            let passed_end = Arc::new(AtomicBool::new(false));
            let body = LaneTick {
                latch: Arc::clone(&latch),
                program_period_ns: program_cadence.period_nanos(),
                targets,
                dropped_offers: Arc::clone(&dropped_offers),
                late_latches: Arc::clone(&late_latches),
                end: Arc::clone(&end),
                passed_end: Arc::clone(&passed_end),
                on_tick: hooks.on_tick.clone(),
                on_latch: hooks.on_latch.clone(),
            };
            let mut cfg = OutputLaneClock::new(lane_name(rate), rate, seed_nanos);
            if let Some(delay) = hooks.latch_delay {
                cfg.latch_delay = delay;
            }
            if let Some(pacing) = hooks.pacing {
                cfg.pacing = pacing;
            }
            match spawn_output_lane(cfg, Arc::clone(time), stop.clone(), move |tick| {
                body.tick(tick);
            }) {
                Ok(handle) => {
                    metrics.push(OutputClockMetrics {
                        cadence: rate,
                        outputs: ids.clone(),
                        stats: handle.stats(),
                        dropped_offers,
                        late_latches,
                        dropped_audio,
                    });
                    clocks.push(RunningClock {
                        rate,
                        ids,
                        handle,
                        passed_end,
                    });
                }
                Err(error) => {
                    // The closure (and its workers) dropped: each worker's
                    // input closes, so its outputs end on their own.
                    tracing::error!(rate = %rate_label(rate), %error, "output clock did not start");
                    lines.push(format!(
                        "output clock {}: FAILED to start ({error}); outputs {} carry no video",
                        rate_label(rate),
                        ids.join(", ")
                    ));
                }
            }
        }
        RunningLanes {
            stop,
            end,
            latch,
            program_cadence,
            clocks,
            outputs: running_outputs,
            metrics,
            lines,
        }
    }
}

/// The clock thread name suffix (`clock-out-25`, `clock-out-30000-1001`).
fn lane_name(rate: Rational) -> String {
    if rate.den == 1 {
        rate.num.to_string()
    } else {
        format!("{}-{}", rate.num, rate.den)
    }
}

/// `num/den` for reports and metrics.
pub(super) fn rate_label(rate: Rational) -> String {
    format!("{}/{}", rate.num, rate.den)
}

/// The run-end marker the lane clocks read: once Program's egress joined, the
/// PTS of the last Program frame. A lane tick after it offers nothing.
#[derive(Default)]
struct RunEnd {
    ended: AtomicBool,
    final_pts_ns: AtomicI64,
}

impl RunEnd {
    fn mark(&self, final_pts_ns: i64) {
        self.final_pts_ns.store(final_pts_ns, Ordering::Release);
        self.ended.store(true, Ordering::Release);
    }

    fn passed(&self, pts: MediaTime) -> bool {
        self.ended.load(Ordering::Acquire)
            && pts.as_nanos() > self.final_pts_ns.load(Ordering::Acquire)
    }
}

/// One output's offer target on its lane clock.
struct OfferTarget {
    worker: RenditionWorker,
    black: Arc<Nv12Image>,
    dropped_audio: Arc<AtomicU64>,
}

/// The per-tick body of one output clock (runs on the clock thread).
struct LaneTick {
    latch: Arc<ProgramLatch<Nv12Image>>,
    program_period_ns: i64,
    targets: Vec<OfferTarget>,
    dropped_offers: Arc<AtomicU64>,
    late_latches: Arc<AtomicU64>,
    end: Arc<RunEnd>,
    passed_end: Arc<AtomicBool>,
    on_tick: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    on_latch: Option<Arc<dyn Fn(u64, Option<u64>) + Send + Sync>>,
}

impl LaneTick {
    /// Latch, then offer to every output: wait-free loads and one `try_send`
    /// per output. Never takes a lock Program holds, never waits.
    fn tick(&self, tick: Tick) {
        if let Some(hook) = self.on_tick.as_ref() {
            hook(tick.index);
        }
        if self.end.passed(tick.pts) {
            self.passed_end.store(true, Ordering::Release);
            return;
        }
        let latched = self.latch.latch(tick.pts);
        if let Some(l) = latched.as_ref() {
            if l.expired
                || l.pts.as_nanos().saturating_add(self.program_period_ns) <= tick.pts.as_nanos()
            {
                self.late_latches.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(observe) = self.on_latch.as_ref() {
            observe(tick.index, latched.as_ref().map(|l| l.program_tick));
        }
        for target in &self.targets {
            let canvas = latched
                .as_ref()
                .map_or_else(|| Arc::clone(&target.black), |l| Arc::clone(&l.frame));
            if target
                .worker
                .offer(BakedTick {
                    tick: tick.index,
                    canvas,
                })
                .is_err()
            {
                self.dropped_offers.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A rate output's encoder wrapper: converts the latched canvas to the
/// output's format and keeps the encoder's ticks consecutive (a dropped or
/// skipped tick is filled with the previous picture, up to
/// [`LANE_GAP_SECONDS`] of ticks; beyond that this output fails alone).
pub(super) struct LaneVideo {
    inner: Box<dyn VideoEncodeWorker>,
    convert: Option<FormatConvert>,
    black: Arc<Nv12Image>,
    last: Option<Arc<Nv12Image>>,
    next: u64,
    max_gap: u64,
    on_encode: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

impl LaneVideo {
    /// Wrap `inner` (an encoder at `rate` and `format`); `black` is the
    /// picture at the format shown before any Program frame.
    pub(super) fn new(
        inner: Box<dyn VideoEncodeWorker>,
        rate: Rational,
        format: Option<AuxFormat>,
        black: Arc<Nv12Image>,
    ) -> Self {
        Self {
            inner,
            convert: format.map(FormatConvert::new),
            black,
            last: None,
            next: 0,
            max_gap: u64::from(super::ticks_per_second(rate)).saturating_mul(LANE_GAP_SECONDS),
            on_encode: None,
        }
    }

    fn picture(&mut self, src: &Arc<Nv12Image>) -> Arc<Nv12Image> {
        match self.convert.as_mut() {
            Some(convert) => convert
                .convert(src)
                .unwrap_or_else(|| Arc::clone(&self.black)),
            None if src.width() == self.black.width() && src.height() == self.black.height() => {
                Arc::clone(src)
            }
            None => Arc::clone(&self.black),
        }
    }
}

impl VideoEncodeWorker for LaneVideo {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        if let Some(hook) = self.on_encode.as_ref() {
            hook(item.tick);
        }
        if item.tick < self.next || item.tick - self.next > self.max_gap {
            return Err(PipelineError::Output {
                kind: "rate-output",
                reason: format!("tick gap {}..{}", self.next, item.tick),
            });
        }
        let picture = self.picture(&item.canvas);
        let fill = self.last.clone().unwrap_or_else(|| Arc::clone(&self.black));
        let mut out = Vec::new();
        for tick in self.next..item.tick {
            out.append(&mut self.inner.encode(BakedTick {
                tick,
                canvas: Arc::clone(&fill),
            })?);
        }
        out.append(&mut self.inner.encode(BakedTick {
            tick: item.tick,
            canvas: Arc::clone(&picture),
        })?);
        self.last = Some(picture);
        self.next = item.tick.saturating_add(1);
        Ok(out)
    }
    fn reorder_bound(&self) -> crate::rendition_workers::ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

/// One running rate output.
struct RunningOutput {
    id: String,
    consumer: JoinHandle<Result<Vec<BindingStatus>, PipelineError>>,
    sink: JoinHandle<Result<SinkRunOutcome, PipelineError>>,
}

/// Spawn one output's sink, consumer and worker.
fn start_output(
    output: PreparedOutput,
    audio: &LaneAudio,
    audio_bases: &[DtsTimeBase],
    program_cadence: Rational,
    on_encode: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<(RunningOutput, OfferTarget), PipelineError> {
    let PreparedOutput {
        id,
        rate,
        encoder,
        format,
        black,
        runner,
        policy,
        gate,
        audio_tracks,
        inband,
        audio_rx,
        dropped_audio,
    } = output;
    let params = encoder.codec_params().clone();
    let time_base = encoder.time_base();
    let dispatcher = RenditionDispatcher::new(DispatchConfig {
        video_count: 1,
        audio_count: audio_bases.len(),
        video_time_base: vec![DtsTimeBase {
            num: time_base.num,
            den: time_base.den,
        }],
        audio_time_base: audio_bases.to_vec(),
        bindings: vec![BindingConfig {
            output_index: 0,
            video_key: Some(0),
            audio_tracks,
            inband_parameter_changes_ok: inband,
        }],
        first_tick: 0,
        cadence: rate,
        video_move_gates: Vec::new(),
    })
    .map_err(|e| PipelineError::Output {
        kind: "rate-output",
        reason: format!("dispatch: {e:?}"),
    })?;
    let (delivery, receivers) =
        BindingDelivery::new(vec![policy], DeliveryLimits::default(), vec![gate]);
    let rx = receivers
        .into_iter()
        .next()
        .ok_or_else(|| PipelineError::Output {
            kind: "rate-output",
            reason: "delivery receiver missing".to_owned(),
        })?;
    let sink_audio = audio.program.clone();
    let sink_tracks = audio.tracks.clone();
    let sink = std::thread::Builder::new()
        .name("multiview-rate-sink".to_owned())
        .spawn(move || runner(rx, params, time_base, sink_audio, sink_tracks))
        .map_err(|e| PipelineError::thread_spawn("rate output sink", e))?;
    let (video_tx, video_rx) = std::sync::mpsc::sync_channel::<DispatchEvent>(LANE_VIDEO_QUEUE);
    let gate = AudioGate {
        held: std::collections::VecDeque::new(),
        program_cadence,
        rate,
        dropped: Arc::clone(&dropped_audio),
    };
    let consumer = std::thread::Builder::new()
        .name("multiview-rate-consumer".to_owned())
        .spawn(move || lane_consumer(dispatcher, delivery, &video_rx, audio_rx, gate))
        .map_err(|e| PipelineError::thread_spawn("rate output consumer", e))?;
    let mut video = LaneVideo::new(
        Box::new(PipelineVideoWorker(encoder)),
        rate,
        format,
        Arc::clone(&black),
    );
    video.on_encode = on_encode;
    let worker = RenditionWorker::spawn(0, rate, time_base, Box::new(video), move |event| {
        video_tx.send(event).map_err(|e| PipelineError::Output {
            kind: "rate-output",
            reason: e.to_string(),
        })
    })?;
    Ok((
        RunningOutput { id, consumer, sink },
        OfferTarget {
            worker,
            black,
            dropped_audio,
        },
    ))
}

/// Program audio events a lane consumer holds back (about 8 s at 30 fps).
/// Full = the oldest is dropped and counted on that lane.
const LANE_AUDIO_HOLD: usize = 256;
/// How far (in output ticks) audio is released ahead of the lane's video.
const AUDIO_LEAD_TICKS: i64 = 4;

/// Releases fanned-out Program audio to the lane dispatcher no further than
/// a few output ticks past the lane's newest video tick, so a late or wedged
/// lane clock never fills its staging with audio windows waiting for video.
struct AudioGate {
    held: std::collections::VecDeque<DispatchEvent>,
    program_cadence: Rational,
    rate: Rational,
    dropped: Arc<AtomicU64>,
}

impl AudioGate {
    fn hold(&mut self, event: DispatchEvent) {
        if self.held.len() >= LANE_AUDIO_HOLD
            && matches!(self.held.front(), Some(DispatchEvent::Audio { .. }))
        {
            self.held.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.held.push_back(event);
    }

    /// The events releasable now: Program audio stamped before the horizon
    /// (all of it once `video_done`); the terminal event only last.
    fn release(&mut self, video_tick: Option<u64>, video_done: bool) -> Vec<DispatchEvent> {
        let horizon = video_tick.map(|tick| {
            let next = i64::try_from(tick).unwrap_or(i64::MAX).saturating_add(1 + AUDIO_LEAD_TICKS);
            MediaTime::from_tick(next, self.rate).as_nanos()
        });
        let mut out = Vec::new();
        while let Some(front) = self.held.front() {
            let ready = video_done
                || match front {
                    DispatchEvent::Audio { tick, .. } => horizon.is_some_and(|h| {
                        MediaTime::from_tick(
                            i64::try_from(*tick).unwrap_or(i64::MAX),
                            self.program_cadence,
                        )
                        .as_nanos()
                            < h
                    }),
                    // Every earlier event was released; the tail closes the
                    // audio stages only once video passed the last audio.
                    _ => horizon.is_some(),
                };
            if !ready {
                break;
            }
            if let Some(event) = self.held.pop_front() {
                out.push(event);
            }
        }
        out
    }
}

/// One rate output's consumer: dispatch its worker's video events and the
/// fanned-out Program audio events into atomic windows for its one binding.
/// Ends when both inputs ended; never blocks anything outside its lane.
fn lane_consumer(
    mut dispatcher: RenditionDispatcher,
    mut delivery: BindingDelivery,
    video_rx: &Receiver<DispatchEvent>,
    audio_rx: Option<Receiver<DispatchEvent>>,
    mut gate: AudioGate,
) -> Result<Vec<BindingStatus>, PipelineError> {
    let mut video_done = false;
    let mut audio_done = audio_rx.is_none();
    let mut video_tick: Option<u64> = None;
    let result = loop {
        if video_done && audio_done && gate.held.is_empty() {
            break Ok(());
        }
        let mut events = Vec::new();
        if video_done {
            if audio_done {
                // Only held audio remains: released below.
            } else if let Some(rx) = audio_rx.as_ref() {
                match rx.recv_timeout(CONSUMER_STEP) {
                    Ok(event) => gate.hold(event),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => audio_done = true,
                }
            }
        } else {
            match video_rx.recv_timeout(CONSUMER_STEP) {
                Ok(event) => events.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => video_done = true,
            }
            while let Ok(event) = video_rx.try_recv() {
                events.push(event);
            }
        }
        if let Some(rx) = audio_rx.as_ref().filter(|_| !audio_done) {
            loop {
                match rx.try_recv() {
                    Ok(event) => gate.hold(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        audio_done = true;
                        break;
                    }
                }
            }
        }
        let mut failed = None;
        for event in events {
            if let DispatchEvent::Video { tick, .. } = &event {
                video_tick = Some(video_tick.map_or(*tick, |t| t.max(*tick)));
            }
            if let Err(e) = dispatcher.dispatch(event, Instant::now(), &mut delivery) {
                if !matches!(e, DispatchError::Admission { .. }) {
                    failed = Some(e);
                    break;
                }
            }
        }
        if failed.is_none() {
            for event in gate.release(video_tick, video_done) {
                if let Err(e) = dispatcher.dispatch(event, Instant::now(), &mut delivery) {
                    if !matches!(e, DispatchError::Admission { .. }) {
                        failed = Some(e);
                        break;
                    }
                }
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
        if let Err(e) = dispatcher.watchdog(Instant::now(), &mut delivery) {
            break Err(e);
        }
        delivery.pump(Instant::now());
    };
    let finished = dispatcher.is_finished();
    delivery.finish();
    match result {
        Err(e) => Err(PipelineError::Output {
            kind: "rate-output",
            reason: format!("dispatch: {e:?}"),
        }),
        Ok(()) if !finished => Err(PipelineError::Output {
            kind: "rate-output",
            reason: "incomplete final window (an encoder or audio tail did not arrive)".to_owned(),
        }),
        Ok(()) => Ok(delivery.statuses()),
    }
}

/// One running output clock.
struct RunningClock {
    rate: Rational,
    ids: Vec<String>,
    handle: OutputLaneHandle,
    passed_end: Arc<AtomicBool>,
}

/// The run's started rate lanes.
pub(super) struct RunningLanes {
    stop: StopSignal,
    end: Arc<RunEnd>,
    latch: Arc<ProgramLatch<Nv12Image>>,
    program_cadence: Rational,
    clocks: Vec<RunningClock>,
    outputs: Vec<RunningOutput>,
    metrics: Vec<OutputClockMetrics>,
    lines: Vec<String>,
}

impl RunningLanes {
    /// The per-clock metric handles (for `GET /api/v1/metrics/program`).
    pub(super) fn metrics(&self) -> Vec<OutputClockMetrics> {
        self.metrics.clone()
    }

    /// Run end, after Program's egress joined (every Program frame is
    /// published): let each lane pass the last Program frame (at most
    /// [`LANE_DRAIN_GRACE`]), stop the clocks, then join every consumer and
    /// sink with a bounded grace each (detached beyond it). Returns one report
    /// line per clock and per output; a lane failure is a line, never an error.
    pub(super) fn shutdown(self) -> Vec<String> {
        let RunningLanes {
            stop,
            end,
            latch,
            program_cadence,
            clocks,
            outputs,
            metrics: _,
            mut lines,
        } = self;
        let final_pts = latch.newest_tick().map_or(-1, |tick| {
            MediaTime::from_tick(i64::try_from(tick).unwrap_or(i64::MAX), program_cadence)
                .as_nanos()
        });
        end.mark(final_pts);
        let drain_until = Instant::now() + LANE_DRAIN_GRACE;
        while Instant::now() < drain_until
            && !clocks
                .iter()
                .all(|c| c.passed_end.load(Ordering::Acquire) || c.handle.is_finished())
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        stop.stop();
        for clock in clocks {
            let stats = clock.handle.stats();
            let label = rate_label(clock.rate);
            let outcome = clock.handle.join_within(SINK_WEDGE_GRACE);
            lines.push(format!(
                "output clock {label} ({}): {} ticks, {} skipped{}",
                clock.ids.join(", "),
                stats.ticks(),
                stats.skipped_ticks(),
                match outcome {
                    Ok(()) => String::new(),
                    Err(error) => format!(", {error}"),
                }
            ));
        }
        for output in outputs {
            lines.extend(join_output(output));
        }
        lines
    }
}

/// Join one rate output's consumer then sink, each within
/// [`SINK_WEDGE_GRACE`]; never an unbounded join.
fn join_output(output: RunningOutput) -> Vec<String> {
    let RunningOutput { id, consumer, sink } = output;
    let mut lines = Vec::new();
    let wait = |handle: &dyn Fn() -> bool| {
        let deadline = Instant::now() + SINK_WEDGE_GRACE;
        while !handle() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        handle()
    };
    if wait(&|| consumer.is_finished()) {
        match consumer.join() {
            Ok(Ok(bindings)) => {
                for line in super::binding_report_lines(&bindings) {
                    lines.push(format!("{id}: {line}"));
                }
            }
            Ok(Err(error)) => {
                tracing::error!(output = %id, %error, "rate output ended with an error");
                lines.push(format!("{id}: rate output FAILED ({error})"));
            }
            Err(_) => lines.push(format!("{id}: rate output FAILED (consumer panicked)")),
        }
    } else {
        lines.push(format!("{id}: rate output consumer WEDGED (detached)"));
    }
    if wait(&|| sink.is_finished()) {
        match sink.join() {
            Ok(Ok(outcome)) => {
                if let Some((path, text)) = outcome.playlist {
                    if let Err(e) = std::fs::write(&path, text.as_bytes()) {
                        lines.push(format!(
                            "{id}: writing playlist {} FAILED: {e}",
                            path.display()
                        ));
                    }
                }
                lines.push(outcome.line);
            }
            Ok(Err(error)) => lines.push(format!("{id}: rate output sink FAILED ({error})")),
            Err(_) => lines.push(format!("{id}: rate output sink FAILED (panicked)")),
        }
    } else {
        lines.push(format!("{id}: rate output sink WEDGED (detached)"));
    }
    lines
}
