//! The full libav\* end-to-end `multiview run` pipeline (the `ffmpeg` feature).
//!
//! Where [`crate::run`] is the FFmpeg-free software smoke of invariant #1, this
//! module adds the libav decoders: it ingests video from the configured sources,
//! composites it on the CPU reference compositor driven by the engine's protected
//! output core,
//! encodes the canvas **once**, and fans the encoded program out to the file and
//! HLS output sinks declared in the config.
//!
//! ```text
//! each source ─▶ own decode thread ─▶ scale to its cell ─▶ publish NV12
//!   (live or file/VOD, PTS-paced to wall-clock — invariant #4)        │
//!                                                                     ▼
//!                                                          per-tile TileStore
//!                                                          (last-good, lock-free)
//!                                                                     │  (sampled,
//!                                                                     │   never pacing)
//!  OutputClock ─▶ EngineRuntime drive (CPU compositor) ◀─────────────┘
//!       │
//!       └▶ one composited canvas per tick (out_pts = f(tick))
//!                                                              ├▶ encode ONCE (mpeg2video
//!                                                              │   by default; libx264 under
//!                                                              │   gpl-codecs)
//!                                                              └▶ fan out: FileSink + SegmentSink
//! ```
//!
//! ## Ingest is streamed, never buffered (BUG-2 fix)
//!
//! Each declared source decodes on its **own dedicated thread** and publishes
//! frames into its per-tile [`TileStore`](multiview_framestore::TileStore) as they
//! arrive — the output clock starts immediately and **samples** the stores per
//! tick. A live stream (RTSP/HLS/SRT/RTMP/TS) never emits EOF, so the previous
//! "decode the whole source into a `Vec` before starting the clock" approach
//! hung forever and never honoured `--duration`/`--ticks`. Streaming makes a
//! live input a *sampled* producer that can neither pace nor stall the output
//! clock (invariant #1), and a bounded run (`run_for`) stops the clock after `N`
//! ticks and tears the ingest threads down (they cannot back-pressure the
//! engine — invariant #10).
//!
//! Per [invariant #4](../docs/research/streaming-gotchas.md) the ingest threads
//! pace each frame to wall-clock **by its PTS** (a custom pacer; `-re` is never
//! used) so a live source plays in real time and a file/VOD source plays at its
//! natural rate rather than being slurped as fast as the disk allows.
//!
//! ## Invariants upheld
//!
//! * **#1 output-clock.** The engine's [`EngineRuntime`](multiview_engine::EngineRuntime)
//!   emits exactly one composited frame per tick; a source that has produced no
//!   frame yet, or has run out of frames, simply holds its last-good frame (or
//!   shows the slate) — it never stalls the loop. Ingest runs on separate
//!   threads the engine never waits on.
//! * **#3 timing.** PTS is re-stamped from the tick counter by the output sinks
//!   (`out_pts = tick`); raw input PTS is never forwarded to an encoder/muxer.
//! * **#5 NV12-throughout.** Frames stay NV12 from decode through composite; the
//!   one NV12 → encoder-format (`yuv420p`) conversion happens inside the sink,
//!   immediately before `send_frame`.
//! * **#7 encode-once-mux-many.** The canvas is composited once per tick and
//!   encoded once per rendition; the file and HLS sinks each consume the *same*
//!   composited frames.
//! * **Licensing.** The default encode codec is LGPL `mpeg2video`; the GPL
//!   `libx264` path is reachable only when the crate is built with
//!   `gpl-codecs`. No FFI lives here — the crate stays `unsafe_code = forbid`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::{MultiviewConfig, Output, Source, SourceKind};
use multiview_control::EngineStateSnapshot;
use multiview_core::frame::FrameMeta;
use multiview_core::layout::{Cell, Layout};
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_core::traits::SourceState;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, MonotonicTimeSource, MultiviewProgram,
    OutputClock, Pacer, RealtimePacer, StopSignal, TimeSource,
};
use multiview_events::Event;
use multiview_ffmpeg::{
    DecodedVideoFrame, EncodedPacket, ScaleSpec, Scaler, StreamCodecParameters, StreamVideoDecoder,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use multiview_output::sink::{
    EncodeConfig, PacketMuxOutcome, PacketMuxSink, PacketSource, ProgramEncoder, PushProtocol,
};

/// The per-subscriber drop-oldest depth of the engine's outbound event stream.
/// The pipeline has no realtime consumers wired here, but the publisher still
/// needs a positive ring (invariant #10).
const EVENT_CAPACITY: usize = 64;

/// Bounded capacity of the hot-loop → bake-consumer streaming queue under the
/// **offline** ([`SendPolicy::BlockForExact`]) policy. A full queue
/// back-pressures the *renderer* (legitimate — offline is not a live clock), so
/// every tick is encoded exactly once (ADR-0025). Sized to keep a few NV12
/// canvases in flight (memory is `O(cap)`, not `O(ticks)`).
const OFFLINE_QUEUE_CAP: usize = 8;

/// Bounded capacity of the hot-loop → bake-consumer streaming queue under the
/// **live** ([`SendPolicy::DropOnOverload`]) policy. Kept small so a wedged
/// encoder is detected and shed promptly; the hot loop `try_send`s and drops on
/// `Full` so the output clock can never stall (inv #1/#10).
const LIVE_QUEUE_CAP: usize = 4;

/// Bounded capacity of each consumer → sink fan-out queue. The consumer drives
/// these with a bounded `send_timeout` (it is off the hot path, so blocking there
/// is allowed — it can only back-pressure the *consumer*, never the engine), so a
/// slow sink paces the consumer rather than dropping a baked frame.
const SINK_QUEUE_CAP: usize = 4;

/// The shed-event debounce window (output ticks) for the live drop-on-overload
/// shed (invariant #9 emission). A sustained encode/egress overload sheds frames
/// continuously; we coalesce a burst of drops into at most one `shed.load`
/// event per this many ticks (≈1 s at 50–60 fps) so the §7.2 retention store and
/// the web UI see the shed *condition* + its cumulative count without a
/// per-dropped-frame flood on the drop-oldest broadcast (inv #10). The cumulative
/// `dropped` rides every emitted event, so the trend is recoverable across
/// coalesced windows.
const SHED_EVENT_EVERY_TICKS: u64 = 60;

/// The rolling HLS live-playlist segment window (HLS-0/1, ADR-0032): how many
/// most-recent segments the live `.m3u8` lists and the sink keeps on disk. Six
/// 1-GOP segments is a small DVR depth that keeps reload-at-live-edge cheap while
/// bounding disk; older segments are pruned as they age out (a one-refresh-behind
/// client may 404 the just-evicted segment — acceptable until the HLS-2 grace
/// reaper). The proper grace-period / configurable DVR is a later slice.
const HLS_LIVE_WINDOW: usize = 6;

/// The grace a wedged sink is given before it is detached so teardown stays
/// bounded (ENG-1, invariant #1 — `stop` always completes). It bounds BOTH the
/// consumer's per-packet fan-out `send_timeout` (so a sink that never drains
/// cannot stall the consumer) AND the [`StreamEgress::join`] wait for each sink
/// thread to finish (so a sink wedged finalising — e.g. a push muxer blocked
/// writing its trailer to a dead peer — cannot hang the join forever). A healthy
/// sink drains/finalises in milliseconds, far inside this window; only a genuine
/// wedge trips it, and it is then reported (never silently dropped) and detached
/// (reaped at process exit). Matches the ingest [`INGEST_JOIN_GRACE`] posture.
const SINK_WEDGE_GRACE: Duration = Duration::from_secs(2);

/// How the hot-loop projection hands each composited tick to the bake consumer
/// over the bounded streaming queue (ADR-0025). The choice is the single
/// load-bearing split between the offline render and the live daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPolicy {
    /// **Offline** (`run_for`, `--ticks N`): a blocking `send`. A full queue
    /// back-pressures the renderer until the consumer drains, so every tick is
    /// baked + encoded exactly once and a file is never silently truncated.
    /// Pacing the *offline* renderer to encode speed is legitimate (it is not a
    /// live output clock), so this does not risk invariant #1/#10.
    BlockForExact,
    /// **Live** (`run_until`, Ctrl-C daemon): a non-blocking `try_send`. On a
    /// full queue the incoming frame is dropped and counted — the hot loop never
    /// blocks, so the output clock keeps ticking (inv #1) and a slow consumer can
    /// never back-pressure the engine (inv #10). Overload is visible (`dropped`),
    /// never hidden.
    DropOnOverload,
}

impl SendPolicy {
    /// The bounded streaming-queue capacity this policy runs with.
    const fn queue_cap(self) -> usize {
        match self {
            Self::BlockForExact => OFFLINE_QUEUE_CAP,
            Self::DropOnOverload => LIVE_QUEUE_CAP,
        }
    }
}

/// The pure, change-driven, rate-limited shed-load emission decision (invariant
/// #9 emission) for the live encode/egress drop-on-overload shed.
///
/// `dropped_now` is the current cumulative drop counter; `tick` is the current
/// output tick index; `last_dropped`/`last_tick` carry the emitter's debounce
/// state across ticks and are advanced **only** when an event is emitted. Emits
/// a [`multiview_events::Event::ShedLoad`] (reason `EncoderOverload`, program
/// scope) iff the drop counter advanced since the last emit AND at least
/// [`SHED_EVENT_EVERY_TICKS`] ticks have elapsed — so a sustained-overload drop
/// storm coalesces into at most one event per window (inv #10), each carrying the
/// cumulative `dropped` so the trend stays recoverable. Pure + side-effect-free
/// apart from advancing the two state cells, so it is exhaustively unit-testable.
fn shed_load_event(
    dropped_now: u64,
    tick: u64,
    last_dropped: &mut u64,
    last_tick: &mut u64,
) -> Option<Event> {
    if dropped_now <= *last_dropped {
        return None;
    }
    if tick.saturating_sub(*last_tick) < SHED_EVENT_EVERY_TICKS {
        return None;
    }
    *last_dropped = dropped_now;
    *last_tick = tick;
    Some(Event::ShedLoad(multiview_events::ShedLoad {
        reason: multiview_events::ShedReason::EncoderOverload,
        scope: multiview_events::ShedScope::Program,
        // The program-egress shed is the cheapest-impact rung that touches
        // program output; report it as ladder level 1 (a single active shed
        // action), distinct from full quality (0).
        level: 1,
        dropped: dropped_now,
    }))
}

/// The egress plan for one [`Pipeline::drive_streaming`] call: the send
/// policy, the per-sink runners, and an optional hot-loop tick observer (test
/// only). Bundled so the streaming core keeps a small argument list.
struct StreamPlan {
    /// How the hot loop hands ticks to the consumer (block vs drop-on-overload).
    policy: SendPolicy,
    /// One runner per output sink, each driving its sink to completion off-hot.
    runners: Vec<SinkRunner>,
    /// Incremented once per emitted tick on the hot loop when present, so a test
    /// can prove a frame was encoded while the engine was still ticking. `None`
    /// on the production paths.
    hot_tick_observer: Option<Arc<AtomicU64>>,
}

/// Errors building or running the libav\* pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PipelineError {
    /// The configuration failed to solve into a layout.
    #[error("invalid configuration: {0}")]
    Config(#[from] multiview_config::ConfigError),
    /// An ingest source could not be opened/decoded.
    #[error("ingest source {id:?}: {reason}")]
    Ingest {
        /// The source id that failed.
        id: String,
        /// The underlying reason.
        reason: String,
    },
    /// The output clock rejected the canvas cadence.
    #[error("output clock: {0}")]
    Clock(String),
    /// A program could not be assembled (ADR-0030 MP-0): the engine rejected the
    /// per-program clock + compositor drive while building its
    /// [`MultiviewProgram`](multiview_engine::MultiviewProgram) — e.g. a
    /// clock/spec cadence mismatch. Carries the offending
    /// [`ProgramId`](multiview_config::ProgramId) so the failure is attributable
    /// per program once the engine runs several.
    #[error("program {program}: {reason}")]
    Program {
        /// The id of the program that failed to assemble.
        program: multiview_config::ProgramId,
        /// The underlying reason (the engine error string).
        reason: String,
    },
    /// The engine drive/compositor rejected the canvas.
    #[error("engine: {0}")]
    Engine(String),
    /// A composited canvas could not be bridged to a libav frame.
    #[error("frame bridge: {0}")]
    Bridge(String),
    /// An output sink failed.
    #[error("output {kind}: {reason}")]
    Output {
        /// The output kind that failed.
        kind: &'static str,
        /// The underlying reason.
        reason: String,
    },
    /// No runnable output sink was declared (or supported) in the config.
    #[error("config declares no runnable file/HLS output (only {0} are wired today)")]
    NoOutput(&'static str),
    /// The requested codec is not encodable in this build (e.g. H.264 without
    /// the `gpl-codecs` feature).
    #[error("codec {codec:?} cannot be encoded by this build: {reason}")]
    Codec {
        /// The requested logical codec.
        codec: String,
        /// Why it is unavailable.
        reason: String,
    },
}

/// A summary of one pipeline run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PipelineReport {
    /// Composited canvas frames driven by the output clock (== ticks).
    pub frames: u64,
    /// The fixed output cadence (exact rational).
    pub cadence: Rational,
    /// Canvas width in pixels.
    pub canvas_width: u32,
    /// Canvas height in pixels.
    pub canvas_height: u32,
    /// The concrete libav encoder used (e.g. `"mpeg2video"`, `"libx264"`).
    pub encoder: String,
    /// Per-output artifacts written, one line each (path + encoded packet count).
    pub outputs: Vec<String>,
    /// How many composited frames were **dropped** before reaching the
    /// bake/encode consumer because the bounded streaming queue was full under
    /// the live (`run_until`) drop-on-overload policy (ADR-0025). Offline runs
    /// (`run_for`, block-for-exact) back-pressure the renderer instead of
    /// dropping, so this is `0` for them — an offline file is never silently
    /// truncated.
    pub dropped: u64,
    /// Whether the output ever faltered. Defined honestly as `dropped > 0`
    /// (ADR-0025): a live run that shed frames under encoder overload faltered;
    /// an offline render (which cannot drop) only ever reports `false`.
    pub faltered: bool,
}

impl PipelineReport {
    /// Render the report as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        let verdict = if self.faltered {
            "FALTERED"
        } else {
            "never faltered"
        };
        let dropped = if self.dropped > 0 {
            format!("; dropped {} under encoder overload", self.dropped)
        } else {
            String::new()
        };
        let mut lines = vec![format!(
            "run (ffmpeg): {} frame(s) at {}/{} fps on {}x{}; encoder {}; output {}{}",
            self.frames,
            self.cadence.num,
            self.cadence.den,
            self.canvas_width,
            self.canvas_height,
            self.encoder,
            verdict,
            dropped,
        )];
        for out in &self.outputs {
            lines.push(format!("  - {out}"));
        }
        lines.join("\n")
    }
}

/// The logical video codec the configured outputs request, resolved to a
/// concrete libav encoder this build is allowed to use.
struct ResolvedEncoder {
    /// The concrete libav encoder short name (`"mpeg2video"`, `"libx264"`, …).
    name: String,
    /// The pixel format the encoder is fed (`yuv420p` for the software codecs).
    format: Pixel,
}

/// Map a config output `codec` token to a logical [`multiview_ffmpeg::VideoCodec`].
/// Unknown / unsupported tokens fall back to MPEG-2 (the LGPL-clean default).
fn logical_codec(token: &str) -> multiview_ffmpeg::VideoCodec {
    use multiview_ffmpeg::VideoCodec;
    match token.to_ascii_lowercase().as_str() {
        "h264" | "avc" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "ffv1" => VideoCodec::Ffv1,
        "mjpeg" => VideoCodec::Mjpeg,
        _ => VideoCodec::Mpeg2Video,
    }
}

/// Resolve the encoder to use for `codec_token`, honouring the compiled features
/// (LGPL `mpeg2video` by default; `libx264`/`libx265` under `gpl-codecs`).
///
/// If the requested codec cannot be encoded by this build (e.g. H.264 with no
/// `gpl-codecs`), this falls back to MPEG-2 so a config that names `h264` still
/// produces a real, playable file in an LGPL-clean build — and logs the
/// substitution. Returns [`PipelineError::Codec`] only if even the fallback is
/// somehow unavailable in the linked `FFmpeg`.
fn resolve_encoder(codec_token: &str) -> Result<ResolvedEncoder, PipelineError> {
    use multiview_ffmpeg::{select_encoder, VideoCodec};
    let requested = logical_codec(codec_token);
    if let Some(name) = select_encoder(requested) {
        return Ok(ResolvedEncoder {
            name: name.to_owned(),
            format: encoder_input_format(name),
        });
    }
    // The requested codec is not encodable in this build; fall back to MPEG-2.
    tracing::warn!(
        requested = ?requested,
        "requested output codec is not encodable in this build (need gpl-codecs/cuda); \
         falling back to LGPL mpeg2video"
    );
    let fallback = select_encoder(VideoCodec::Mpeg2Video).ok_or_else(|| PipelineError::Codec {
        codec: codec_token.to_owned(),
        reason: "neither the requested codec nor the mpeg2video fallback is present in FFmpeg"
            .to_owned(),
    })?;
    Ok(ResolvedEncoder {
        name: fallback.to_owned(),
        format: encoder_input_format(fallback),
    })
}

/// The pixel format to feed a resolved encoder, by its concrete libav name.
///
/// **NVENC ingests NV12 natively** (invariant #5: the composited canvas is
/// already NV12). When the resolved encoder is a `*_nvenc` hardware encoder we
/// feed it NV12, so the per-tick full-canvas `FrameConverter` swscale
/// (`sink.rs`'s NV12→YUV420P conversion) collapses to its passthrough branch —
/// the single biggest avoidable CPU cost on the output path (a full ~3 MP@1080p /
/// ~8 MP@4K libswscale every tick). The software codecs (`mpeg2video`, `libx264`,
/// `libx265`, `ffv1`, `mjpeg`, …) take planar `YUV420P` as before — required, as
/// they do not accept NV12 directly.
fn encoder_input_format(name: &str) -> Pixel {
    if name.ends_with("_nvenc") {
        Pixel::NV12
    } else {
        Pixel::YUV420P
    }
}

/// Build a wgpu [`GpuTarget`](multiview_compositor::backend::GpuTarget) from the
/// hardware-addressing handles the load source resolved for the chosen device.
///
/// Pure (no I/O): it just projects the hal [`GpuTargetInfo`](multiview_hal::GpuTargetInfo)
/// onto the compositor's wgpu-free target shape, so the wgpu adapter pick can
/// match the chosen device by PCI bus id / `(vendor, device)` pair / name. Lives
/// here (the cli is the only crate that depends on both hal and the compositor).
#[cfg(feature = "gpu")]
fn gpu_target_from_info(info: &multiview_hal::GpuTargetInfo) -> multiview_compositor::GpuTarget {
    multiview_compositor::GpuTarget {
        pci_bus_id: info.pci_bus_id.clone(),
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        name: info.name.clone(),
    }
}

/// The load-aware admission pick for **one** GPU island: the single chosen
/// device's wgpu compositor target and its CUDA decode/encode ordinal, both
/// resolved from the SAME [`Selection`](multiview_hal::Selection).
///
/// This is the affinity-carrying value (ADR-0035 Tier-1 / the GPU-placement
/// principle: *load informs placement, never fragments a pipeline*). Because
/// `wgpu_target` (PCI bus id / `(vendor, device)` / name — what the compositor
/// adapter pick matches on) and `cuda_ordinal` (the selector NVDEC `*_cuvid` /
/// NVENC `*_nvenc` address a GPU by) both come off the one chosen device's
/// [`GpuTargetInfo`](multiview_hal::GpuTargetInfo), decode, composite, and encode
/// physically cannot land on different GPUs — there is exactly one device per
/// island. When admission names no device, BOTH fields are absent in lockstep, so
/// every stage keeps its default (the compositor's `HighPerformance` adapter,
/// NVDEC's default CUDA device).
#[cfg(feature = "gpu")]
#[derive(Debug, Clone, Default)]
struct AdmissionPick {
    /// The wgpu compositor target pinning the chosen device, or `None` when
    /// admission named no device (keep the default adapter).
    wgpu_target: Option<multiview_compositor::GpuTarget>,
    /// The chosen device's CUDA enumeration ordinal (e.g. `Some("1")`) for the
    /// NVDEC decode + NVENC encode pin, or `None` in lockstep with `wgpu_target`.
    cuda_ordinal: Option<String>,
}

/// Choose the GPU to host the whole pipeline island at **admission** — the
/// load-aware, decide-once pick (ADR-0035 Tier-1, ADR-0018; the GPU-placement
/// principle: *load informs placement, never fragments a pipeline — affinity is
/// the hard constraint*).
///
/// This is **synchronous and runs before the output clock starts** (called from
/// the top of [`Pipeline::drive_streaming`]); it polls the existing NVML load
/// source ONCE, assembles one candidate per visible GPU, builds a
/// [`PipelineDemand`](multiview_hal::PipelineDemand) from the canvas geometry +
/// cadence + tile count + the NVENC-session flag, and asks
/// [`multiview_hal::select_device`] for the least-contended GPU that can host the
/// whole `decode → composite → encode` island. It **never blocks or `.await`s**
/// on the data plane (inv #1).
///
/// Returns an [`AdmissionPick`] carrying the chosen device's wgpu compositor
/// target AND its CUDA decode/encode ordinal — both off the SAME selection, so
/// decode + composite + encode follow one GPU (affinity). When the pick succeeds
/// AND the chosen device's hardware handles resolve, both fields are populated;
/// when there is no NVML / no `cuda` feature / no visible GPU / the scorer
/// rejected every candidate / the chosen device's handles could not be resolved,
/// **both fields are `None` in lockstep** (the default value) — every failure
/// mode degrades gracefully to today's behaviour, logged once, never a panic and
/// never a stalled clock (inv #1/#2). The wgpu target and the CUDA ordinal are
/// never split: either one chosen device pins both stages, or neither.
///
/// # NVDEC pinned here; NVENC device-bind is the remaining Tier-2 item
///
/// The chosen device's `cuda_ordinal` is plumbed into the NVDEC (`*_cuvid`)
/// decode open ([`StreamVideoDecoder::new_preferring_hw`] →
/// `HwDeviceContext::create(Cuda, Some(ordinal))`), so decode co-locates with the
/// compositor. NVENC (`*_nvenc`) encode currently opens via libav's encoder
/// context without an explicit `hw_device_ctx` bind, so it uses the default CUDA
/// device (ordinal 0); binding the encoder context to this same ordinal is the
/// remaining Tier-2 item (it needs an encoder-side `hw_device_ctx` seam in
/// `multiview-ffmpeg`'s `VideoEncoder`). Decode + composite — the bulk of the
/// pipeline — ARE co-located on the one chosen GPU.
#[cfg(feature = "gpu")]
fn select_admission_pick(
    load_source: &dyn multiview_hal::LoadSource,
    canvas_w: u32,
    canvas_h: u32,
    cadence: Rational,
    tile_count: usize,
    opens_encode_session: bool,
) -> AdmissionPick {
    use multiview_core::pixel::PixelFormat;
    use multiview_core::traits::BackendKind;
    use multiview_hal::{
        select_device, Capability, CostBudget, GpuCandidate, Pins, PipelineDemand, PlacementPolicy,
        Resolution, Stage, StageCaps, TileLoad,
    };

    let loads = load_source.poll();
    if loads.is_empty() {
        // No NVML / no visible GPU (the dev container, CI, a non-NVIDIA host):
        // keep today's default-adapter behaviour. Not even logged at info — this
        // is the overwhelmingly common, entirely-expected path. Both the wgpu
        // target and the CUDA ordinal stay `None` in lockstep.
        return AdmissionPick::default();
    }

    let canvas_res = Resolution::new(canvas_w.max(1), canvas_h.max(1));
    // A conservative per-engine budget large enough not to gate a typical
    // multiview on a real GPU; the LIVE 4060-vs-P2000 routing on the contended
    // box rests on the VRAM headroom gate (the 95%-full 4060 exceeds the 0.85
    // ceiling) + VRAM-dominant scoring, which need no perf-class table. A real
    // per-GPU perf-class `CostBudget` table is the documented Tier-2 refinement
    // (ADR-0035 §5) — until then every candidate shares this permissive budget,
    // so the budget gate is effectively inert and the headroom/score do the work.
    let budget = CostBudget::new(100_000.0, 100_000.0, 100_000.0);

    // One candidate per visible GPU. We treat each as capable of the whole island
    // at the canvas resolution in NV12 (the compositor canvas + NVENC are NV12,
    // inv #5); the headroom + VRAM gates are what actually steer the pick. A
    // candidate carries its STABLE id from the load snapshot so `select_device`
    // matches loads to candidates and a pin (future) binds correctly.
    let cap = |stage: Stage| {
        Capability::new(
            BackendKind::Cuda,
            stage,
            canvas_res,
            vec![PixelFormat::Nv12],
        )
    };
    let candidates: Vec<GpuCandidate> = loads
        .iter()
        .map(|load| GpuCandidate {
            device_id: load.device_id.clone(),
            stage_caps: StageCaps::new(
                cap(Stage::Decode),
                cap(Stage::Composite),
                cap(Stage::Encode),
            ),
            budget,
        })
        .collect();

    // Demand: the canvas at the output cadence, NV12, a small per-tile decode +
    // the composite + the single encode rendition. `predicted_pool_bytes = 0`
    // leaves the VRAM gate to the headroom ceiling (we do not yet model the exact
    // pool footprint here — a conservative-but-safe choice that never over-rejects
    // a roomy GPU; the headroom ceiling still rejects a near-full one).
    let mut tile_loads: Vec<TileLoad> = Vec::with_capacity(tile_count + 2);
    for _ in 0..tile_count.max(1) {
        tile_loads.push(TileLoad::new(Stage::Decode, canvas_res));
    }
    tile_loads.push(TileLoad::new(Stage::Composite, canvas_res));
    tile_loads.push(TileLoad::new(Stage::Encode, canvas_res));
    let demand = PipelineDemand::new(
        cadence,
        tile_loads,
        canvas_res,
        PixelFormat::Nv12,
        0,
        opens_encode_session,
    );

    // Ask the scorer for the single least-contended GPU that can host the whole
    // island. A reject (no fit / all over the headroom ceiling) → fall back to
    // the default adapter / CPU, logged, never a stall (inv #1).
    let selection = match select_device(
        &candidates,
        &demand,
        &loads,
        &Pins::none(),
        PlacementPolicy::default(),
    ) {
        Ok(selection) => selection,
        Err(reason) => {
            tracing::warn!(
                ?reason,
                "load-aware admission found no single GPU to host the pipeline; \
                 falling back to the default adapter / CPU (inv #1: never stalls)"
            );
            return AdmissionPick::default();
        }
    };

    // Resolve the chosen device's hardware-addressing handles so wgpu can pin it
    // AND NVDEC/NVENC can share its CUDA ordinal — both off the SAME selection
    // (affinity). If they cannot be resolved, fall back to the default adapter +
    // default CUDA device rather than pin blindly (both `None`, in lockstep).
    let Some(info) = load_source.device_target(&selection.device) else {
        tracing::warn!(
            device_index = selection.device.index(),
            "load-aware admission chose a GPU but could not resolve its hardware \
             handles; falling back to the default adapter + default CUDA device"
        );
        return AdmissionPick::default();
    };
    let target = gpu_target_from_info(&info);
    if !target.is_some() {
        tracing::warn!(
            device_index = selection.device.index(),
            "load-aware admission chose a GPU but its hardware handles were empty; \
             falling back to the default adapter + default CUDA device"
        );
        return AdmissionPick::default();
    }
    // The ONE chosen device feeds both stages: its PCI handles pin the wgpu
    // compositor, its CUDA ordinal pins NVDEC decode (and is the NVENC pin once
    // the encoder device-bind seam lands). Decode + composite + encode therefore
    // resolve to one physical GPU — they cannot diverge.
    tracing::info!(
        device_index = selection.device.index(),
        stable_id = selection.device.stable_id(),
        pci_bus_id = ?info.pci_bus_id,
        cuda_ordinal = ?info.cuda_ordinal,
        name = ?info.name,
        score = selection.score,
        "load-aware admission: pinning the whole pipeline (decode + composite + \
         encode) to the least-contended GPU (ADR-0035 Tier-1; NVENC device-bind \
         is the remaining Tier-2 item)"
    );
    AdmissionPick {
        wgpu_target: Some(target),
        cuda_ordinal: info.cuda_ordinal,
    }
}

#[cfg(all(test, feature = "gpu"))]
mod admission_target_tests {
    use super::{gpu_target_from_info, select_admission_pick};
    use multiview_core::time::Rational;
    use multiview_hal::{DeviceId, DeviceLoad, GpuTargetInfo, LoadSource, NullLoadPoller, Vendor};

    /// A fake two-GPU [`LoadSource`] modelling a contended dual-GPU host: a
    /// 95%-VRAM RTX 4060 at ordinal 0 (over the 0.85 headroom ceiling, so the
    /// scorer rejects it) and an idle Quadro P2000 at ordinal 1. Each device
    /// resolves a DISTINCT [`GpuTargetInfo`] (distinct PCI bus id + ordinal), so a
    /// test can prove the chosen device's bus-id (→ wgpu) and ordinal (→ NVDEC)
    /// both come from the SAME device — no cross-GPU divergence.
    struct FakeTwoGpu;

    impl FakeTwoGpu {
        const GPU0_UUID: &'static str = "GPU-4060";
        const GPU1_UUID: &'static str = "GPU-p2000";
        const GPU0_BUS: &'static str = "00000000:01:00.0";
        const GPU1_BUS: &'static str = "00000000:02:00.0";
    }

    impl LoadSource for FakeTwoGpu {
        fn poll(&self) -> Vec<DeviceLoad> {
            // GPU0 (4060): VRAM 7796/8188 ≈ 0.952, over the 0.85 ceiling → rejected.
            let mut gpu0 = DeviceLoad::unknown(DeviceId::new(Vendor::Nvidia, Self::GPU0_UUID, 0));
            gpu0.vram_used_bytes = Some(7_796 * 1024 * 1024);
            gpu0.vram_total_bytes = Some(8_188 * 1024 * 1024);
            // GPU1 (P2000): VRAM 1781/5120 ≈ 0.348, under the ceiling → admissible.
            let mut gpu1 = DeviceLoad::unknown(DeviceId::new(Vendor::Nvidia, Self::GPU1_UUID, 1));
            gpu1.vram_used_bytes = Some(1_781 * 1024 * 1024);
            gpu1.vram_total_bytes = Some(5_120 * 1024 * 1024);
            vec![gpu0, gpu1]
        }

        fn device_target(&self, device: &DeviceId) -> Option<GpuTargetInfo> {
            // Resolve each device's handles by its STABLE id, the same way NVML
            // does — so the test asserts the bus-id and ordinal that come back
            // belong to the one chosen device.
            match device.stable_id() {
                id if id == Self::GPU0_UUID => Some(GpuTargetInfo {
                    pci_bus_id: Some(Self::GPU0_BUS.to_owned()),
                    vendor_id: Some(0x10de),
                    device_id: Some(0x2882),
                    name: Some("NVIDIA GeForce RTX 4060".to_owned()),
                    cuda_ordinal: Some("0".to_owned()),
                }),
                id if id == Self::GPU1_UUID => Some(GpuTargetInfo {
                    pci_bus_id: Some(Self::GPU1_BUS.to_owned()),
                    vendor_id: Some(0x10de),
                    device_id: Some(0x1c30),
                    name: Some("Quadro P2000".to_owned()),
                    cuda_ordinal: Some("1".to_owned()),
                }),
                _ => None,
            }
        }
    }

    #[test]
    fn no_visible_gpu_falls_back_to_none_in_lockstep() {
        // The NullLoadPoller (the GPU-free / no-NVML host — the dev container, CI)
        // polls zero devices, so admission must return an empty pick: BOTH the
        // wgpu target and the CUDA ordinal `None`, IN LOCKSTEP, so neither the
        // compositor nor NVDEC pins a GPU and the caller keeps today's default
        // behaviour. This is the graceful-fallback proof for the GPU-free path
        // (inv #1: never a panic, never a stall).
        let source = NullLoadPoller::new();
        let pick = select_admission_pick(
            &source,
            1920,
            1080,
            Rational::new(30, 1),
            4,
            true, // opens an NVENC session
        );
        assert!(
            pick.wgpu_target.is_none(),
            "no visible GPU → the compositor keeps its default adapter (None)"
        );
        assert!(
            pick.cuda_ordinal.is_none(),
            "no visible GPU → NVDEC keeps its default CUDA device (None)"
        );
    }

    #[test]
    fn chosen_device_feeds_decode_and_composite_with_one_identity() {
        // AFFINITY PROOF: with two GPUs visible, the load-aware pick routes the
        // pipeline off the 95%-VRAM 4060 (over the headroom ceiling) onto the idle
        // P2000 — and the SINGLE chosen device's PCI bus id (→ wgpu compositor) and
        // CUDA ordinal (→ NVDEC decode, the NVENC pin once wired) must both come
        // from THAT one device. They cannot diverge: the wgpu target's bus-id and
        // the cuda_ordinal are read off one `GpuTargetInfo`. This is the core of
        // the GPU-placement principle — load informs placement, never fragments a
        // pipeline across GPUs.
        let source = FakeTwoGpu;
        let pick = select_admission_pick(
            &source,
            1920,
            1080,
            Rational::new(30, 1),
            4,
            true, // opens an NVENC session
        );

        let target = pick
            .wgpu_target
            .expect("a visible admissible GPU must yield a wgpu target");
        // The chosen device is the P2000 (the 4060 is over the 0.85 VRAM ceiling).
        assert_eq!(
            target.pci_bus_id.as_deref(),
            Some(FakeTwoGpu::GPU1_BUS),
            "the compositor must be pinned to the P2000's PCI bus id, not the 4060's"
        );
        assert_eq!(
            pick.cuda_ordinal.as_deref(),
            Some("1"),
            "NVDEC must be pinned to the P2000's CUDA ordinal (1), not the 4060's (0)"
        );
        // The load-bearing assertion: the wgpu bus-id and the CUDA ordinal both
        // identify the SAME physical device (no decode-on-GPU0 / composite-on-GPU1
        // split). GPU1_BUS ↔ ordinal "1" is the P2000 in the fake topology.
        assert_eq!(
            target.pci_bus_id.as_deref(),
            Some(FakeTwoGpu::GPU1_BUS),
            "bus id and ordinal must resolve to one device — the P2000"
        );
        assert_eq!(
            target.name.as_deref(),
            Some("Quadro P2000"),
            "the chosen device's name confirms the single identity"
        );
    }

    #[test]
    fn target_info_projects_onto_the_wgpu_target() {
        // The hal `GpuTargetInfo` (PCI bus id / pair / name / ordinal) projects
        // onto the compositor's wgpu-free `GpuTarget` (the cuda_ordinal is carried
        // on the hal side for Tier-2, not on the wgpu target). A populated info is
        // a usable (`is_some`) target.
        let info = GpuTargetInfo {
            pci_bus_id: Some("00000000:01:00.0".to_owned()),
            vendor_id: Some(0x10de),
            device_id: Some(0x1c30),
            name: Some("Quadro P2000".to_owned()),
            cuda_ordinal: Some("1".to_owned()),
        };
        let target = gpu_target_from_info(&info);
        assert!(target.is_some());
        assert_eq!(target.pci_bus_id.as_deref(), Some("00000000:01:00.0"));
        assert_eq!(target.vendor_id, Some(0x10de));
        assert_eq!(target.device_id, Some(0x1c30));
        assert_eq!(target.name.as_deref(), Some("Quadro P2000"));
    }

    #[test]
    fn empty_info_projects_to_an_inert_target() {
        // An all-`None` info (NVML resolved nothing) projects to an inert target
        // that pins nothing — so the caller keeps the default adapter.
        let target = gpu_target_from_info(&GpuTargetInfo::default());
        assert!(!target.is_some());
    }
}

#[cfg(test)]
mod encoder_format_tests {
    use super::encoder_input_format;
    use ffmpeg_next::format::Pixel;

    #[test]
    fn nvenc_encoders_are_fed_nv12_to_skip_the_per_tick_swscale() {
        // The composited canvas is already NV12 (inv #5); NVENC ingests NV12
        // natively, so the encoder input format must be NV12 — that is what makes
        // the sink's FrameConverter hit its passthrough branch (no full-canvas
        // libswscale per tick).
        assert_eq!(encoder_input_format("h264_nvenc"), Pixel::NV12);
        assert_eq!(encoder_input_format("hevc_nvenc"), Pixel::NV12);
    }

    #[test]
    fn software_encoders_keep_yuv420p() {
        // The software codecs do not accept NV12 directly — they MUST be fed
        // planar YUV420P, exactly as before this change.
        assert_eq!(encoder_input_format("mpeg2video"), Pixel::YUV420P);
        assert_eq!(encoder_input_format("libx264"), Pixel::YUV420P);
        assert_eq!(encoder_input_format("libx265"), Pixel::YUV420P);
        assert_eq!(encoder_input_format("ffv1"), Pixel::YUV420P);
        assert_eq!(encoder_input_format("mjpeg"), Pixel::YUV420P);
        // A name that merely CONTAINS "nvenc" but does not end with the suffix is
        // not treated as NVENC (defensive: the suffix is the discriminator).
        assert_eq!(encoder_input_format("nvenc_fake_sw"), Pixel::YUV420P);
    }
}

/// A runnable **mux-only** output sink resolved from a config `[[outputs]]`
/// entry. None of these holds an encoder: the canvas is encoded ONCE by the bake
/// consumer's single [`ProgramEncoder`] and the *same* coded packets are fanned,
/// as owned copies, to each of these muxers (invariant #7, encode-once-mux-many).
enum RunnableOutput {
    /// A single container file (container inferred from the path extension).
    File {
        /// The packet-fed file muxer.
        sink: PacketMuxSink,
        /// Where the container is written (for the run report).
        path: PathBuf,
    },
    /// An HLS segmenter writing `seg*.ts` + a **rolling live** media playlist into
    /// a directory (HLS-0/1, ADR-0032). The sink publishes the windowed `.m3u8`
    /// on disk on every closed segment and prunes the evicted `.ts`, so an
    /// infinite live run writes (and bounds) the playlist + segment set instead of
    /// rendering once at a finalize that never arrives.
    Hls {
        /// The packet-fed GOP-segment muxer (live flavour).
        sink: PacketMuxSink,
        /// Where the `.m3u8` playlist is published (owned + written by the live
        /// sink itself — recorded here only for the run report).
        playlist_path: PathBuf,
    },
    /// A live push transport (RTMP / SRT) muxing the same packets to a remote
    /// peer over the matching protocol — the egress twin of
    /// [`RunnableOutput::File`], differing only in that the muxer targets a
    /// network URL. A push whose peer is unreachable is reported and dropped,
    /// never allowed to fail the program (invariants #1/#10).
    Push {
        /// The packet-fed push muxer.
        sink: PacketMuxSink,
        /// A short transport label (`rtmp`/`srt`) for the run report + logs.
        label: &'static str,
        /// The destination URL (for the run report + logs).
        url: String,
    },
}

/// A built, ready-to-run pipeline.
pub struct Pipeline {
    /// The solved layout (canvas + normalized cells).
    layout: Arc<Layout>,
    /// The fixed output cadence (exact rational).
    cadence: Rational,
    /// The single program this pipeline runs (ADR-0030 MP-0). The legacy
    /// top-level `canvas`/`layout`/`cells`/`overlays`/`outputs` block desugars
    /// here into exactly one implicit `Multiview` program (`id = "main"`), and
    /// [`Pipeline::drive_streaming`] constructs **one**
    /// [`MultiviewProgram`](multiview_engine::MultiviewProgram) from it — the run
    /// path flows through one `Program`. Its [`ProgramId`] also scopes the
    /// per-program context [`PipelineError`] carries. The `programs: Vec<…>`
    /// schema root + multi-program supervisor arrive in MP-1/MP-5.
    program_spec: multiview_config::ProgramSpec,
    /// Per-source native caption cue stores, keyed by source id. Each store is
    /// written by an isolated caption reader thread (HLS `WebVTT` rendition demux)
    /// and **sampled** at each output tick by the overlay baker, which burns the
    /// active cue into *that source's tile* (per-tile burn-in). A source with no
    /// caption selector (or whose rendition could not be resolved) is absent here
    /// and simply shows no caption. Native caption burn-in needs the `overlay`
    /// feature to render, so the stores are only built/sampled under it.
    #[cfg(feature = "overlay")]
    caption_stores: std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    /// RT-10b: the live subtitle re-point handle, published when a `drive` begins
    /// (the hot-loop-owned [`SubtitleRouter`](crate::captions::SubtitleRouter) builds
    /// its [`SubtitleRouteHandle`](crate::captions::SubtitleRouteHandle) into this
    /// shared slot). The control plane reads it via
    /// [`subtitle_route_handle`](Self::subtitle_route_handle) to drive a subtitle
    /// breakaway (`RouteSubtitle`, RT-11) into the running pipeline. `None` before a
    /// run starts (or on a run with no caption stores). A lock-free `ArcSwapOption`
    /// so publishing/reading the handle never blocks the engine (inv #1/#10).
    #[cfg(feature = "overlay")]
    subtitle_route: Arc<arc_swap::ArcSwapOption<crate::captions::SubtitleRouteHandle>>,
    /// Per-source caption reader plans (resolved at build time): the rendition
    /// `m3u8` to demux + the store to publish into. Consumed by the ingest
    /// supervisor (one reader thread each) on the first `drive`, like the video
    /// ingest plans. Empty ⇒ no native caption readers.
    #[cfg(feature = "overlay")]
    caption_plans: Vec<crate::captions::CaptionPlan>,
    /// An optional legacy `--subtitles` sidecar track, routed through the SAME
    /// per-tile burn-in path as native captions: its active cue is sampled per
    /// tick and burned into [`Self::sidecar_target`]'s tile. `None` ⇒ no sidecar.
    #[cfg(feature = "overlay")]
    subtitles: Option<multiview_overlay::subtitle::CueTrack>,
    /// The source id the legacy `--subtitles` sidecar burns into (the first
    /// source-bound cell). `None` ⇒ no bound target (the sidecar shows nowhere).
    #[cfg(feature = "overlay")]
    sidecar_target: Option<String>,
    /// Per-source per-tick audio-loudness timelines (dBFS), keyed by source id,
    /// one inner entry per output tick, derived off the hot path from each
    /// source's own decoded audio (the per-input meter). A source absent from the
    /// map (a live URL, NDI, or an audio-free clip) rides its meter floor rather
    /// than showing a fabricated constant.
    #[cfg(feature = "overlay")]
    meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
    /// Per-source human label (`display_name`, or the id when unnamed), keyed by
    /// source id — the text drawn bottom-left of each tile.
    #[cfg(feature = "overlay")]
    tile_labels: std::collections::HashMap<String, String>,
    /// Operator-declared content-fault probes, resolved from `config.probes`
    /// (M10). Each probe `watches` a cell; this resolves that cell to its bound
    /// **source** so the per-tick [`FaultDetector`] can build that source's fault
    /// machine from the operator's *declared* threshold / zone / dwell / severity:
    /// the analyser config via the engine's
    /// [`black_config_from_kind`](multiview_engine::black_config_from_kind) /
    /// [`freeze_config_from_kind`](multiview_engine::freeze_config_from_kind)
    /// mappers, and the X.733 lifecycle via
    /// [`AlarmStateMachine::from_probe`](multiview_engine::AlarmStateMachine::from_probe),
    /// instead of the hardcoded defaults. A source with no declared probe keeps the
    /// default behaviour. Multiple probes of the same kind on one source: the first
    /// wins (config validation already rejects duplicate probe ids).
    #[cfg(feature = "overlay")]
    declared_probes: Vec<multiview_config::probe::Probe>,
    /// An optional **analog** clock face requested by a `[[overlays]]` entry with
    /// `kind = "clock"` + `face = "analog"`. `None` ⇒ only the default digital
    /// clock label is drawn.
    #[cfg(feature = "overlay")]
    analog_clock: Option<crate::overlays::AnalogClockSpec>,
    /// Per-source last-good-frame stores, keyed by source id. Shared (`Arc`)
    /// between the engine's drive loop (reader) and the ingest threads (writers).
    stores: HashMapStores,
    /// The per-source producer stop flags (ADR-W018): every startup ingest
    /// thread registers its flag here, and the live-source hub shares the same
    /// registry, so a live `RemoveSource` can tear down exactly one producer.
    stop_registry: crate::live_sources::StopRegistry,
    /// Per-source streaming ingest plans: how to open + decode each source, and
    /// the tile size its frames are scaled to. The drive starts one decode
    /// thread per plan; the threads publish into [`Self::stores`] as frames
    /// arrive (never buffered ahead of the clock — the BUG-2 fix).
    ingest_plans: Vec<IngestPlan>,
    /// Per-source last-good **audio** stores (AUD-2), keyed by source id. Shared
    /// (`Arc`) between each source's audio decode thread (writer) and the
    /// [`ProgramBus`](multiview_audio::program::ProgramBus) the bake consumer
    /// samples per tick (reader). Built for every file/URL source AND for the
    /// `bars` synthetic source (its 1 kHz line-up tone, AUD-5) — a `solid`/`clock`
    /// synthetic / NDI / audio-free source simply rides silence (the store
    /// silence-fills past what was published). Empty when this run did not opt into
    /// program audio.
    audio_stores: std::collections::HashMap<String, Arc<multiview_audio::store::AudioStore>>,
    /// Per-source **audio** ingest plans (AUD-2): how to open + decode each
    /// source's audio. The drive starts one audio decode thread per plan
    /// (alongside the video decode thread) writing into [`Self::audio_stores`].
    /// Built only for libav-openable (file/URL) sources; empty when this run did
    /// not opt into program audio.
    audio_ingest_plans: Vec<crate::audio::AudioIngestPlan>,
    /// Per-source synthetic **tone** plans (AUD-5): the `bars` line-up tone
    /// companion. The drive starts one tone publish thread per plan, writing the
    /// 1 kHz reference sine into that source's [`Self::audio_stores`] entry (which
    /// is routed onto the program bus exactly like a decoded source's audio).
    /// Built only for `bars` synthetic sources; empty when this run did not opt
    /// into program audio.
    tone_ingest_plans: Vec<crate::audio::ToneIngestPlan>,
    /// The fixed canvas color (ADR-C001 SDR BT.709 limited).
    canvas_color: CanvasColor,
    /// The "no signal" slate composited for tiles with no usable frame.
    nosignal_card: Nv12Image,
    /// The canvas background shown where no tile covers.
    background: LinearRgba,
    /// The resolved concrete encoder (name + fed pixel format).
    encoder: ResolvedEncoder,
    /// The single encode configuration the bake consumer builds its one
    /// [`ProgramEncoder`] from (invariant #7): the canvas is encoded ONCE per
    /// run and the same packets are fanned to every mux-only sink.
    encode_cfg: EncodeConfig,
    /// The runnable outputs declared in the config.
    outputs: Vec<RunnableOutput>,
    /// The configured DRM/KMS display heads (DEV-B1 / ADR-0044, feature
    /// `display-kms`): **raw-frame** sinks fed the pre-encode NV12 canvas
    /// through wait-free mailboxes — never part of the packet fan-out. Taken
    /// (and started) once at stream start; the sinks live for the run.
    #[cfg(feature = "display-kms")]
    display_plans: Vec<DisplayOutputPlan>,
    /// Per-input elementary-stream inventories, keyed (and id-sorted) by source
    /// id (RT-3, ADR-0034 §9). Probed **once at build time** — off the
    /// output-clock thread — from each path-backed source's demuxer (the
    /// inventory is an open-time snapshot, `param_probe.rs`). Threaded into the
    /// published `EngineStateSnapshot` (so the control plane's read-only
    /// `GET /inputs/{id}/streams` surface can show every stream an input offers)
    /// and emitted once as `input.streams` events at run start. Empty on the
    /// synthetic / live-URL / no-`ffmpeg` paths (no container to probe); folding
    /// an empty map leaves the snapshot unchanged (inv #10 — never on the hot
    /// loop).
    inventories: std::collections::BTreeMap<String, multiview_core::stream::StreamInventory>,
    /// The Conspect tile-watermark signal (S3, ADR-0050 §5): the wait-free
    /// published [`EnforcementLevel`](multiview_licence::EnforcementLevel) the
    /// off-hot-path overlay bake samples each frame to decide whether to stamp the
    /// corner watermark on the composited multiview canvas. `None` (the default)
    /// never watermarks; the binary wires the shared signal so the engine, the
    /// API, and the chrome read the same ladder state. Sampled with one wait-free
    /// load off the hot loop — it can never pace or stall the output clock
    /// (invariant #1). Only consumed under `overlay` (the bake renders it).
    #[cfg(feature = "overlay")]
    watermark_signal: Option<crate::licence::WatermarkSignal>,
}

type HashMapStores = std::collections::HashMap<String, Arc<TileStore<Nv12Image>>>;

/// Everything one source needs to be ingested on its own decode thread: where
/// its media lives, the store to publish into, the tile size to scale to, the
/// canvas color tag, and whether it is a live (never-ending) source.
struct IngestPlan {
    /// The source id (for diagnostics / store keying).
    id: String,
    /// Where the media lives.
    location: SourceLocation,
    /// The destination tile size (even pixels) the frames are scaled to.
    tile_w: u32,
    /// The destination tile height.
    tile_h: u32,
    /// The store to publish decoded frames into (shared with the drive loop).
    store: Arc<TileStore<Nv12Image>>,
    /// Whether this is a live (continuous, never-EOF) source. Live sources are
    /// reopened on EOF/error (a transient HLS/RTSP drop reconnects); a finite
    /// file/VOD source plays once and then holds its last frame.
    live: bool,
    /// An optional **in-container subtitle route**: the muxed subtitle stream's
    /// index + time-base + the decoder kind (DVB-sub bitmap, or `ass`/`subrip`/
    /// `mov_text` text) + the per-source cue store. When present, the video
    /// ingest loop decodes that stream's packets as a sibling of the video
    /// packets and publishes its cues (bitmap **or** text) into the store (#36
    /// Phase 2 + SUR-3c). `None` ⇒ this source carries no in-container subtitle
    /// decode. Only built under `overlay` (the burn-in renderer consumes cues).
    #[cfg(feature = "overlay")]
    incontainer_sub: Option<InContainerSubRoute>,
    /// An optional **embedded CEA-608 route**: the `cc_dec` field/channel + the
    /// per-source cue store. When present, the video ingest loop pulls the
    /// `AV_FRAME_DATA_A53_CC` side data off each decoded video frame and feeds it
    /// to `cc_dec`, publishing the recovered TEXT cues into the store (captions.md
    /// §2/§4, SUR-3c). `None` ⇒ this source decodes no embedded captions. Only
    /// built under `overlay` (the burn-in renderer consumes the cues).
    #[cfg(feature = "overlay")]
    embedded_cc: Option<EmbeddedCcRoute>,
    /// The canvas colour the source's frames are tagged in. Carried so an
    /// in-process synthetic generator renders into the canvas output space.
    canvas_color: CanvasColor,
    /// The output cadence a synthetic generator paces its publishes to.
    cadence: Rational,
    /// The CUDA enumeration **ordinal** (e.g. `Some("1")`) the NVDEC `*_cuvid`
    /// decoder for this source is pinned to — the SAME GPU the load-aware
    /// admission pick pinned the compositor to (ADR-0035 Tier-1 / the
    /// GPU-placement principle: decode + composite + encode follow one chosen
    /// device, never split across GPUs). Stamped from the admission selection in
    /// [`Pipeline::drive_streaming`] before the ingest threads spawn; `None` when
    /// admission named no specific device (no NVML / GPU-free / scorer rejection)
    /// — in **lockstep** with the compositor's `None`, so neither stage pins a GPU
    /// and decode opens libav's default CUDA device (today's behaviour). Consumed
    /// by [`open_and_stream`] → [`StreamVideoDecoder::new_preferring_hw`].
    cuda_ordinal: Option<String>,
}

/// The in-container subtitle decode route stashed on an [`IngestPlan`]: which
/// muxed subtitle stream to decode (index + its time-base), the decoder kind
/// (DVB-sub bitmap or `ass`/`subrip`/`mov_text` text), and the per-source cue
/// store the decoded cues are published into (shared with the baker).
#[cfg(feature = "overlay")]
struct InContainerSubRoute {
    /// The subtitle stream index within the source container.
    stream_index: usize,
    /// The subtitle stream time-base (for the caption decoder's PTS rebase).
    time_base: Rational,
    /// The decoder this subtitle stream needs (`DvbSubtitle` for bitmap, or
    /// `Ass`/`SubRip`/`MovText` for in-container text).
    source: multiview_ffmpeg::CaptionSource,
    /// The lock-free store the decoded cues (bitmap or text) are published into.
    store: Arc<crate::captions::CueStore>,
}

/// The embedded CEA-608 (A53 side-data) decode route stashed on an [`IngestPlan`]:
/// the `cc_dec` channel/field and the per-source cue store the recovered TEXT cues
/// are published into. The A53 bytes are pulled off each decoded video frame in
/// the same ingest loop (no separate stream — captions.md §2/§4).
#[cfg(feature = "overlay")]
struct EmbeddedCcRoute {
    /// The 608 field/channel the `cc_dec` decoder surfaces (CC1–CC4).
    channel: multiview_ffmpeg::CcChannel,
    /// The lock-free store the decoded text cues are published into.
    store: Arc<crate::captions::CueStore>,
}

impl Pipeline {
    /// Build the pipeline from an already-validated configuration.
    ///
    /// Solves the layout, resolves each declared source to a streaming
    /// [`IngestPlan`] (it does **not** decode anything here — decoding happens on
    /// per-source threads started by [`Pipeline::run_for`]/`run_until`, so a
    /// never-ending live source can never stall the build), resolves the output
    /// encoder (LGPL by default), and builds the runnable file/HLS sinks.
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the layout cannot be solved, a `test`
    /// source's clip cannot be generated, an NDI/unsupported source kind is
    /// declared, no runnable output is declared, or the encoder cannot be
    /// resolved.
    #[allow(clippy::too_many_lines)]
    // reason: a sequential constructor that solves the layout, builds one store +
    // ingest plan per source, wires native captions, and assembles the outputs —
    // each step is in-scope and splitting it would only scatter the wiring.
    pub fn build(config: &MultiviewConfig) -> Result<Self, PipelineError> {
        let layout = Arc::new(config.solve_layout()?);
        let cadence = config.canvas.fps.rational();
        let canvas_color = CanvasColor::default();
        let tag = canvas_color.output_tag();

        // Map each source id to the pixel size of the cell that binds it, so the
        // decoded frames are scaled to tile the canvas (the reference compositor
        // places tiles 1:1 at the cell origin).
        let mut stores: HashMapStores = std::collections::HashMap::new();
        let mut ingest_plans: Vec<IngestPlan> = Vec::with_capacity(config.sources.len());
        // Per-source audio stores + decode plans (AUD-2). Built for every
        // libav-openable (file/URL) source so program audio — when this run opts
        // in via `enable_program_audio` — has a per-source store to route onto the
        // bus and a plan to spawn a decode thread from. A synthetic / NDI /
        // audio-free source contributes none and simply rides silence on the bus.
        let mut audio_stores: std::collections::HashMap<
            String,
            Arc<multiview_audio::store::AudioStore>,
        > = std::collections::HashMap::new();
        let mut audio_ingest_plans: Vec<crate::audio::AudioIngestPlan> = Vec::new();
        // AUD-5: synthetic line-up tone plans (the `bars` source's 1 kHz companion).
        let mut tone_ingest_plans: Vec<crate::audio::ToneIngestPlan> = Vec::new();

        // Per-source native caption stores + reader plans. Built best-effort: a
        // source whose selector resolves to an HLS WebVTT rendition gets a store
        // (sampled per tile) and a reader plan (one isolated demux thread); a
        // resolve failure logs and yields no store (the tile shows no caption) —
        // it must never fail the build of a live source (invariant #1/#10). Only
        // built under `overlay` (native burn-in needs the renderer).
        #[cfg(feature = "overlay")]
        let mut caption_stores: std::collections::HashMap<
            String,
            Arc<crate::captions::CueStore>,
        > = std::collections::HashMap::new();
        #[cfg(feature = "overlay")]
        let mut caption_plans: Vec<crate::captions::CaptionPlan> = Vec::new();

        // Resolve every source's HLS WebVTT caption plan CONCURRENTLY, off the
        // serial build path (#48), indexed by id for the loop's cheap lookup.
        #[cfg(feature = "overlay")]
        let mut prefetched_captions = prefetch_caption_plans(config);

        for source in &config.sources {
            let (tile_w, tile_h) = cell_pixel_size(&layout, &source.id)
                .unwrap_or((config.canvas.width, config.canvas.height));
            let store = Arc::new(TileStore::new(
                source.id.clone(),
                TileThresholds::default(),
                NoSignalPolicy::HoldForever,
            ));
            #[cfg_attr(not(feature = "overlay"), allow(unused_mut))]
            let mut plan = ingest_plan_for(
                source,
                tile_w,
                tile_h,
                Arc::clone(&store),
                canvas_color,
                cadence,
            )?;

            // Wire this source's native captions (HLS WebVTT rendition thread +/or
            // in-container DVB-sub route), registering any cue store + reader plan
            // and stashing the dvbsub route on `plan`. Only under `overlay`.
            #[cfg(feature = "overlay")]
            wire_source_captions(
                source,
                &mut plan,
                &mut caption_stores,
                &mut caption_plans,
                &mut prefetched_captions,
            );

            stores.insert(source.id.clone(), store);
            ingest_plans.push(plan);

            // AUD-2: resolve this source's audio decode plan (a libav-openable
            // file/URL location + its live flag). A synthetic/NDI/unsupported
            // source yields `None` (no audio thread; it rides silence on the bus).
            // The store is built per audio-bearing source so the bus can route it;
            // it stays empty (silence-filling) until the decode thread fills it.
            if let Some(audio_plan) = audio_ingest_plan_for(source) {
                audio_stores.insert(source.id.clone(), crate::audio::new_store());
                audio_ingest_plans.push(audio_plan);
            }
            // AUD-5: the `bars` synthetic source emits a 1 kHz line-up tone (its
            // colour-bars companion), so it contributes a real audio store on the
            // bus rather than silence. Build the store the same way an audio-bearing
            // source does (so the existing bus-routing below picks it up) and queue
            // a tone publish plan; a tone thread fills the store at cadence. `solid`
            // / `clock` synthetic sources carry no audio and stay silent.
            if let Some(tone_plan) = tone_ingest_plan_for(source, cadence) {
                audio_stores
                    .entry(source.id.clone())
                    .or_insert_with(crate::audio::new_store);
                tone_ingest_plans.push(tone_plan);
            }
        }

        let nosignal_card =
            Nv12Image::solid(config.canvas.width, config.canvas.height, 16, 128, 128, tag)
                .map_err(|e| PipelineError::Engine(e.to_string()))?;

        // Resolve the encoder from the first output that names a codec (file/HLS
        // share one encode — invariant #7). Default to MPEG-2 if none names one.
        let codec_token = config
            .outputs
            .iter()
            .find_map(output_codec)
            .unwrap_or("mpeg2video");
        let encoder = resolve_encoder(codec_token)?;

        // One-second GOP (rounded), also the per-segment frame count for HLS.
        let gop = ticks_per_second(cadence).max(1);

        let cfg = EncodeConfig {
            codec_name: encoder.name.clone(),
            width: config.canvas.width,
            height: config.canvas.height,
            format: encoder.format,
            cadence,
            gop,
            bit_rate: 4_000_000,
            // AUD-4: video-only until the program-audio path is wired through
            // drive_streaming (subsequent slice); `None` keeps the muxer
            // single-stream so the existing run output is unchanged.
            audio: None,
            // NVENC device-affinity pin (Tier-2 P1a): `None` until the admission
            // pick threads the chosen ordinal in (separate integration); behaviour
            // is unchanged from before the seam existed.
            cuda_ordinal: None,
        };

        let built = build_outputs(&config.outputs)?;
        #[cfg(feature = "display-kms")]
        let has_display = !built.display.is_empty();
        #[cfg(not(feature = "display-kms"))]
        let has_display = false;
        let outputs = built.packet;
        if outputs.is_empty() && !has_display {
            return Err(PipelineError::NoOutput("file/HLS"));
        }

        // Derive the PER-SOURCE per-tick audio-loudness timelines off the
        // build path: decode each file-backed source's OWN audio with
        // multiview-audio's ballistics DSP and snapshot the meter at each tick, so a
        // tile's vertical meter reflects that input's own audio. A live URL /
        // NDI / audio-free source contributes no timeline (its tile meter then
        // rides its floor — no fabricated constant). This never touches the hot
        // path: it is computed once at build time.
        #[cfg(feature = "overlay")]
        let meter_db_timelines = build_meter_timelines(config, cadence);
        #[cfg(feature = "overlay")]
        let tile_labels = config
            .sources
            .iter()
            .map(|s| {
                let label = s.display_name.clone().unwrap_or_else(|| s.id.clone());
                (s.id.clone(), label)
            })
            .collect();
        // Read an optional analog clock face from a `[[overlays]]` clock entry.
        #[cfg(feature = "overlay")]
        let analog_clock =
            analog_clock_from_config(&config.overlays, config.canvas.width, config.canvas.height);

        // The legacy `--subtitles` sidecar (if attached later) burns into the
        // first source-bound cell. Pre-resolve that target id once here.
        #[cfg(feature = "overlay")]
        let sidecar_target = layout.cells.iter().find_map(|c| c.source.clone());

        // Probe each path-backed source's full elementary-stream inventory ONCE,
        // here at build time — off the output-clock thread (the clock has not
        // started). The inventory is an open-time snapshot; threading it into the
        // published snapshot is the RT-3 read-only discovery surface. Synthetic /
        // live-URL / NDI sources (and the no-`ffmpeg` build) contribute nothing
        // and simply ride an empty map (inv #10 — never on the hot loop).
        let inventories = build_input_inventories(config);

        // Desugar the legacy single-program config into one implicit `"main"`
        // `Multiview` program (ADR-0030 §6 / MP-0). The run path is driven through
        // one `MultiviewProgram` built from this spec; carrying the raw config
        // block (not the solved layout) keeps the spec the authentic config-derived
        // program identity the engine consumes. No `programs:` schema root yet
        // (MP-5) — this is the single-program desugaring, used by the run path.
        let program_spec = multiview_config::ProgramSpec::main_multiview(
            config.canvas.clone(),
            config.layout.clone(),
            config.cells.clone(),
            config.overlays.clone(),
            config.outputs.clone(),
        );

        Ok(Self {
            layout,
            cadence,
            program_spec,
            stores,
            stop_registry: crate::live_sources::stop_registry(),
            ingest_plans,
            audio_stores,
            audio_ingest_plans,
            tone_ingest_plans,
            inventories,
            #[cfg(feature = "overlay")]
            caption_stores,
            #[cfg(feature = "overlay")]
            subtitle_route: Arc::new(arc_swap::ArcSwapOption::empty()),
            #[cfg(feature = "overlay")]
            caption_plans,
            canvas_color,
            nosignal_card,
            background: LinearRgba::opaque(0.02, 0.02, 0.05),
            encoder,
            encode_cfg: cfg,
            outputs,
            #[cfg(feature = "display-kms")]
            display_plans: built.display,
            #[cfg(feature = "overlay")]
            subtitles: None,
            #[cfg(feature = "overlay")]
            sidecar_target,
            #[cfg(feature = "overlay")]
            meter_db_timelines,
            #[cfg(feature = "overlay")]
            tile_labels,
            #[cfg(feature = "overlay")]
            declared_probes: config.probes.clone(),
            #[cfg(feature = "overlay")]
            analog_clock,
            #[cfg(feature = "overlay")]
            watermark_signal: None,
        })
    }

    /// Attach the Conspect tile-watermark signal (S3, ADR-0050 §5): the wait-free
    /// published ladder level the overlay bake samples each frame to decide
    /// whether to stamp the corner watermark on the composited multiview canvas.
    /// The binary wires the shared signal (same store as the control plane), so
    /// every surface reads the same ladder state. Sampled off the hot loop with a
    /// single wait-free load — it can never stall the output clock (invariant #1).
    /// Without the `overlay` feature there is no bake to render it, so this is a
    /// no-op identity.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn with_watermark_signal(mut self, signal: crate::licence::WatermarkSignal) -> Self {
        self.watermark_signal = Some(signal);
        self
    }

    /// Tile-watermark attachment is a no-op when the `overlay` feature is disabled
    /// (there is no overlay bake to render it).
    #[cfg(not(feature = "overlay"))]
    #[must_use]
    pub fn with_watermark_signal(self, _signal: crate::licence::WatermarkSignal) -> Self {
        self
    }

    /// Attach a parsed subtitle track whose active cue is burned into the
    /// program by the overlay baker (GAP-3). The track's cues are looked up by
    /// each output frame's media time, so the cue burns in exactly while it is
    /// active. Without the `overlay` feature this is a no-op identity.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn with_subtitles(mut self, track: multiview_overlay::subtitle::CueTrack) -> Self {
        self.subtitles = Some(track);
        self
    }

    /// Subtitle attachment is a no-op when the `overlay` feature is disabled
    /// (there is no overlay baker to burn the cue into); the track is dropped.
    #[cfg(not(feature = "overlay"))]
    #[must_use]
    pub fn with_subtitles(self, _track: multiview_overlay::subtitle::CueTrack) -> Self {
        self
    }

    /// Attach a pre-populated **native caption cue store** for source `source_id`
    /// (RT-10b). The run builds one re-pointable subtitle layer per attached store
    /// and samples it each output tick (`active_at(pts)`), burning the active cue
    /// into that source's tile — exactly the path the HLS `WebVTT` reader feeds via
    /// [`caption_plans`](Self::caption_plans), but with the store supplied directly.
    /// Replaces any existing store for the same source. Returns the shared
    /// [`Arc`](std::sync::Arc) so the caller (a native caption reader, or a test)
    /// can keep publishing cues into it.
    #[cfg(feature = "overlay")]
    pub fn attach_caption_store(
        &mut self,
        source_id: impl Into<String>,
        store: Arc<crate::captions::CueStore>,
    ) -> Arc<crate::captions::CueStore> {
        let id = source_id.into();
        self.caption_stores.insert(id, Arc::clone(&store));
        store
    }

    /// The native caption cue store wired for source `source_id`, if any.
    ///
    /// Returns the shared [`Arc`](std::sync::Arc) the source's caption reader
    /// (HLS `WebVTT` rendition thread, in-container DVB-sub/text route, or the
    /// embedded CEA-608 route) publishes into and the baker samples each tick.
    /// `None` when the source declared no resolvable caption source. Exposed so a
    /// caller (or a test) can observe the cues a source actually produced through
    /// the whole wiring — not merely that a decoder exists.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn caption_store_for(&self, source_id: &str) -> Option<Arc<crate::captions::CueStore>> {
        self.caption_stores.get(source_id).map(Arc::clone)
    }

    /// The fixed output cadence (exact rational).
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// The number of ingest sources wired into this pipeline.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.stores.len()
    }

    /// The shared per-source producer stop registry (ADR-W018): hand this to
    /// the [`LiveSourceHub`](crate::live_sources::LiveSourceHub) so a live
    /// remove can tear down a startup producer (ingest thread or generator).
    #[must_use]
    pub fn stop_registry(&self) -> crate::live_sources::StopRegistry {
        Arc::clone(&self.stop_registry)
    }

    /// The resolved concrete encoder name.
    #[must_use]
    pub fn encoder_name(&self) -> &str {
        &self.encoder.name
    }

    /// Opt this run into **program audio** (AUD-4): the single bake consumer
    /// builds a [`multiview_audio::program::ProgramBus`] from the same cadence and
    /// mixes one block per tick, the [`ProgramEncoder`] gains a second (AAC)
    /// elementary stream, and every mux sink registers a video **and** audio
    /// stream. Default OFF — with this never called, `encode_cfg.audio` stays
    /// `None` and the run is byte-identical to the video-only path (no bus, no
    /// audio packets, `run_av` delegates to the old single-stream `run`).
    ///
    /// The program bus mixes the **real decoded audio** of every audio-bearing
    /// source (AUD-2): a per-source decode thread resamples each source's audio to
    /// the canonical 48 kHz stereo and publishes it into a lock-free `AudioStore`
    /// the bus samples per tick. The `bars` synthetic source contributes a 1 kHz
    /// **line-up tone** the same way (AUD-5) — its colour-bars companion. A source
    /// with no audio (the `solid`/`clock` synthetic kinds, NDI, an audio-free clip)
    /// contributes silence. The AAC encoder runs at 48 kHz stereo / 128 kbps (the
    /// canonical program format).
    pub fn enable_program_audio(&mut self) {
        self.encode_cfg.audio = Some(multiview_output::AudioEncodeConfig::aac(48_000, 2, 128_000));
    }

    /// Run the engine for exactly `max_ticks` ticks under the realtime pacer,
    /// then encode the composited program once and fan it out to every
    /// configured sink.
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the clock/engine reject the canvas, or any
    /// sink fails. Input exhaustion is **not** an error: a source past its last
    /// frame holds its last-good frame, so the output keeps emitting on cadence.
    pub async fn run_for(&mut self, max_ticks: u64) -> Result<PipelineReport, PipelineError> {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let stop = StopSignal::new();
        // Offline render: block-for-exact so every tick is encoded (exact N). The
        // finite run also writes the bounded self-contained `program.ts` anchor.
        let plan = StreamPlan {
            policy: SendPolicy::BlockForExact,
            runners: self.build_sink_runners(false),
            hot_tick_observer: None,
        };
        // Offline render serves no UI: a throwaway publisher/preview + no-op
        // control hook.
        let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(EVENT_CAPACITY);
        let preview = crate::preview::program_slot();
        let out = self
            .drive_streaming(
                time,
                RealtimePacer,
                &stop,
                Some(max_ticks),
                plan,
                &publisher,
                &preview,
                |_: &mut CompositorDrive<Nv12Image>| {},
            )
            .await?;
        Ok(out.report)
    }

    /// Run the engine **until `stop`** under the realtime pacer (the binary wires
    /// `stop` to Ctrl-C), then encode + fan out.
    ///
    /// # Errors
    ///
    /// See [`Pipeline::run_for`].
    pub async fn run_until(&mut self, stop: &StopSignal) -> Result<PipelineReport, PipelineError> {
        let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(EVENT_CAPACITY);
        let preview = crate::preview::program_slot();
        self.run_until_serving(stop, &publisher, &preview, |_d| {})
            .await
    }

    /// Like [`Pipeline::run_until`], but the engine's outbound `publisher`,
    /// the live-preview `preview` slot, and a per-frame-boundary `control` hook
    /// are supplied by the caller — so the **same ingest/composite/encode
    /// pipeline also serves the control plane, the web UI, and the live previews**
    /// (the binary shares these with `multiview_control`). `control` runs on the
    /// output-clock loop and must be non-blocking (invariants #1 + #10).
    ///
    /// # Errors
    ///
    /// See [`Pipeline::run_for`].
    pub async fn run_until_serving<FC>(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
        preview: &crate::preview::ProgramSlot,
        control: FC,
    ) -> Result<PipelineReport, PipelineError>
    where
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.run_until_serving_observed(stop, publisher, preview, control, None)
            .await
    }

    /// Like [`Pipeline::run_until_serving`], but mirrors this program's **live
    /// per-tick count** into `tick_observer` (incremented once per emitted output
    /// tick on the hot loop, a single wait-free `fetch_add`).
    ///
    /// MP-1 (ADR-0030 §2.2): the daemon run path drives this one program through an
    /// engine [`ProgramSet`](multiview_engine::ProgramSet) (a set of exactly one
    /// program, id `"main"`, for the legacy single-program config — behaviour-
    /// identical to today). The `ProgramSet` samples each program's
    /// `ticks_emitted` to observe progress without touching its hot loop; the same
    /// `Arc<AtomicU64>` is handed here so the program's tick count is genuinely live
    /// on the supervisor side (not fabricated). `None` ⇒ no observer (the plain
    /// [`Pipeline::run_until_serving`] behaviour).
    ///
    /// # Errors
    ///
    /// See [`Pipeline::run_for`].
    pub async fn run_until_serving_observed<FC>(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
        preview: &crate::preview::ProgramSlot,
        control: FC,
        tick_observer: Option<Arc<AtomicU64>>,
    ) -> Result<PipelineReport, PipelineError>
    where
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        // Live daemon: drop-on-overload so a wedged encoder can never stall the
        // output clock (inv #1) or back-pressure the engine (inv #10). A live run
        // suppresses the `program.ts` anchor (HLS-2): the rolling HLS window is the
        // bounded on-disk artifact — an ever-growing `program.ts` is not written.
        let plan = StreamPlan {
            policy: SendPolicy::DropOnOverload,
            runners: self.build_sink_runners(true),
            hot_tick_observer: tick_observer,
        };
        let out = self
            .drive_streaming(
                time,
                RealtimePacer,
                stop,
                None,
                plan,
                publisher,
                preview,
                control,
            )
            .await?;
        Ok(out.report)
    }

    /// The per-source frame stores, shared with the control plane's preview
    /// provider for the live per-input thumbnails.
    #[must_use]
    pub fn preview_stores(&self) -> HashMapStores {
        self.stores.clone()
    }

    /// RT-10b: the live subtitle re-point handle for the running pipeline, or
    /// [`None`] before a `drive` has started (or on a run with no caption stores).
    ///
    /// The control plane drives a **subtitle breakaway** by calling
    /// [`SubtitleRouteHandle::request_repoint`](crate::captions::SubtitleRouteHandle::request_repoint)
    /// on it — re-pointing a layer to another source's cues takes effect on the
    /// next output tick (the run's [`SubtitleRouter`](crate::captions::SubtitleRouter)
    /// drains it at the sample boundary). Reading the handle is a lock-free
    /// `ArcSwapOption` load, so it can never pace or stall the engine (inv #1/#10).
    /// This is the run-side seam `Command::RouteSubtitle` (RT-11) drives.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn subtitle_route_handle(&self) -> Option<Arc<crate::captions::SubtitleRouteHandle>> {
        self.subtitle_route.load_full()
    }

    /// RT-10b: the **shared** subtitle re-point slot (the `Arc` behind
    /// [`subtitle_route_handle`](Self::subtitle_route_handle)).
    ///
    /// Cloning this slot lets a caller observe the live handle **concurrently with
    /// a running `drive`** (which borrows the pipeline mutably): the run publishes
    /// the handle into this slot at drive start, and a holder of a slot clone reads
    /// it with a lock-free [`ArcSwapOption`](arc_swap::ArcSwapOption) load. This is
    /// how the control plane wires `RouteSubtitle` to the running pipeline (and how
    /// the run-path re-point is exercised in tests) without aliasing `self`.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn subtitle_route_slot(
        &self,
    ) -> Arc<arc_swap::ArcSwapOption<crate::captions::SubtitleRouteHandle>> {
        Arc::clone(&self.subtitle_route)
    }

    /// Build one [`SinkRunner`] per configured runnable output. Each runner
    /// drives the **existing, tested** `multiview_output` `PacketMuxSink::run`
    /// over a [`StreamingPacketSource`] that pulls coded packets off that sink's
    /// bounded fan-out channel — so a sink muxes the program as the single encoder
    /// produces it (invariant #7) rather than after the whole run. Taking the
    /// outputs by value here moves them out of `self`, so a second
    /// `drive_streaming` call simply has no sinks (the run produces no further
    /// artifacts) rather than double-running a sink.
    ///
    /// `live` selects whether the self-contained `program.ts` anchor is added: a
    /// finite/offline render (`false`) prepends it (bounded, a single playable
    /// container), a live run (`true`) suppresses it so the run never writes an
    /// ever-growing `program.ts` (HLS-2, ADR-0032) — the rolling HLS window is the
    /// bounded on-disk artifact instead.
    fn build_sink_runners(&mut self, live: bool) -> Vec<SinkRunner> {
        let outputs = maybe_prepend_program_ts(std::mem::take(&mut self.outputs), live);
        outputs
            .into_iter()
            .map(|output| -> SinkRunner {
                Box::new(move |rx, params, time_base, audio| {
                    run_one_output(output, rx, &params, time_base, audio.as_ref())
                })
            })
            .collect()
    }

    /// **Test seam** (ADR-0025): drive the streaming bake→encode→fan-out path
    /// with an injected time source + pacer and **fake** sink runners, returning
    /// the report plus the streaming observability (peak queue occupancy, cap).
    ///
    /// This exposes the exact same machinery `run_for`/`run_until` use — the
    /// bounded hot-loop → consumer queue under `policy`, the single off-hot-path
    /// bake consumer, and the per-sink fan-out — but lets a test inject a slow /
    /// blocked / counting sink and a [`ManualTimeSource`](multiview_engine::ManualTimeSource)
    /// so the concurrency contract (bounded memory, no stall, exact-N offline,
    /// streaming-not-batch) is asserted without a real encoder or `ffprobe`.
    ///
    /// `hot_tick_observer`, if supplied, is incremented once per emitted tick on
    /// the hot loop, so a fake sink can read it to prove a frame was encoded
    /// while the engine was still ticking (streaming, not batch).
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the clock/engine reject the canvas, the
    /// bake fails, or a sink runner returns an error.
    pub async fn drive_streaming_for_test<P: Pacer>(
        &mut self,
        params: StreamTestParams<P>,
        stop: &StopSignal,
    ) -> Result<StreamTestResult, PipelineError> {
        let StreamTestParams {
            time,
            pacer,
            max_ticks,
            policy,
            runners,
            hot_tick_observer,
        } = params;
        let runners: Vec<SinkRunner> = runners
            .into_iter()
            .map(|r| -> SinkRunner {
                // The fake sink consumes the coded-packet fan-out channel; it does
                // not mux, so the seeded codec params + time-base (and the audio
                // params + time-base) are unused.
                Box::new(move |rx, _params, _time_base, _audio| {
                    let outcome = r(rx);
                    Ok(SinkRunOutcome {
                        line: format!("test sink: {} packet(s)", outcome.frames),
                        playlist: None,
                        frames: outcome.frames,
                    })
                })
            })
            .collect();
        let capacity = policy.queue_cap();
        let plan = StreamPlan {
            policy,
            runners,
            hot_tick_observer,
        };
        // The test seam serves no UI: throwaway publisher/preview + no-op control.
        let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(EVENT_CAPACITY);
        let preview = crate::preview::program_slot();
        let out = self
            .drive_streaming(
                time,
                pacer,
                stop,
                max_ticks,
                plan,
                &publisher,
                &preview,
                |_: &mut CompositorDrive<Nv12Image>| {},
            )
            .await?;
        Ok(StreamTestResult {
            report: out.report,
            peak_occupancy: out.peak_occupancy,
            capacity,
            sink_frames: out.sink_frames,
        })
    }

    /// The streaming core shared by `run_for`/`run_until`/the test seam: spawn the
    /// per-sink fan-out threads + the single bake consumer, drive the engine's
    /// protected output core (one composited canvas per tick), stream each tick
    /// to the consumer over a bounded queue (per `policy`), and on teardown drain
    /// + finalise every sink. Memory is `O(queue)` for any run length (ADR-0025).
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the clock/engine reject the canvas, the
    /// bake consumer fails, or a sink runner returns an error.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    // reason: the streaming core threads the engine's outbound publisher, the
    // live-preview slot, and the control-plane command hook through to the hot
    // loop so the SAME pipeline both ingests/encodes AND serves the UI; the
    // arguments are each distinct and dictated by the engine's run signature.
    async fn drive_streaming<P, FC>(
        &mut self,
        time: Arc<dyn TimeSource>,
        pacer: P,
        stop: &StopSignal,
        max_ticks: Option<u64>,
        plan: StreamPlan,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
        preview: &crate::preview::ProgramSlot,
        control: FC,
    ) -> Result<DriveStreamOutcome, PipelineError>
    where
        P: Pacer,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let StreamPlan {
            policy,
            runners,
            hot_tick_observer,
        } = plan;
        let clock =
            OutputClock::new(self.cadence).map_err(|e| PipelineError::Clock(e.to_string()))?;
        let drive = CompositorDrive::new(
            Arc::clone(&self.layout),
            self.stores.clone(),
            self.nosignal_card.clone(),
            self.canvas_color,
            self.background,
        )
        .map_err(|e| PipelineError::Engine(e.to_string()))?;
        // Under the opt-in `gpu` feature the run PREFERS the wgpu GPU
        // compositor. The device is chosen LOAD-AWARE at admission (ADR-0035
        // Tier-1, decide-once): poll NVML ONCE, score every visible GPU, and pin
        // the WHOLE pipeline — decode + composite + encode — to the least-contended
        // one that can host the island (the GPU-placement principle — affinity is
        // the hard constraint). This is synchronous and runs BEFORE the output
        // clock starts; it never blocks or `.await`s on the data plane (inv #1).
        //
        // The single chosen device feeds BOTH stages from one `AdmissionPick`: its
        // PCI handles pin the wgpu compositor adapter, and its CUDA ordinal is
        // stamped onto every ingest plan so NVDEC `*_cuvid` decode opens on the
        // SAME GPU (no cross-GPU split). On a GPU-free host / no NVML / a scorer
        // rejection, both the wgpu target and the ordinal are `None` IN LOCKSTEP:
        // we fall back to the default `HighPerformance` adapter AND NVDEC's default
        // CUDA device, exactly as before. `RunBackend::select` then uses the GPU
        // only if that adapter initializes and otherwise transparently falls back
        // to the CPU reference (inv #1: a missing/failed GPU never stalls or
        // crashes the run). Without the feature the drive keeps its default CPU
        // backend and no ordinal is stamped, so the default build path is
        // byte-for-byte unchanged.
        #[cfg(feature = "gpu")]
        let drive = {
            use multiview_compositor::backend::{GpuTarget, RunBackend};
            let load_source = crate::system_metrics::default_load_source();
            let opens_encode_session = self.encoder.name.ends_with("_nvenc");
            let pick = select_admission_pick(
                load_source.as_ref(),
                self.layout.canvas.width,
                self.layout.canvas.height,
                self.cadence,
                self.layout.cells.len(),
                opens_encode_session,
            );
            // Stamp the chosen device's CUDA ordinal onto every ingest plan BEFORE
            // the threads spawn, so decode co-locates with the compositor on the
            // one chosen GPU (affinity). `None` leaves the plans on the default
            // CUDA device — in lockstep with `wgpu_target` being `None`, so neither
            // stage pins a GPU.
            if let Some(ordinal) = pick.cuda_ordinal.as_deref() {
                for plan in &mut self.ingest_plans {
                    plan.cuda_ordinal = Some(ordinal.to_owned());
                }
            }
            // The pinned chosen device, or `GpuTarget::none()` (prefer the GPU at
            // the default adapter) when admission did not name a specific GPU — so
            // the run still prefers the GPU on a single-GPU / no-NVML host.
            let target = pick.wgpu_target.unwrap_or_else(GpuTarget::none);
            drive.with_backend(RunBackend::select(Some(target)))
        };

        let ts: Arc<dyn TimeSource> = time;
        // The engine's outbound publisher is supplied by the caller so the
        // control plane can share it (the live UI reads engine state + events);
        // the same goes for the live-preview slot the projection fills.
        let preview = Arc::clone(preview);

        // Start streaming ingest BEFORE the clock loop: one decode thread per
        // source, each publishing into its `TileStore` as frames arrive. The
        // supervisor owns the threads + a stop flag; it never blocks the engine
        // (the engine only ever *samples* the lock-free stores — invariant #10)
        // and it is torn down (stop + join) when this scope ends, so a bounded
        // run reliably stops the clock AND the ingest (invariant #1). Taking the
        // plans by value means a second call simply ingests nothing (the stores
        // hold their last frames / slate) rather than double-spawning.
        let plans = std::mem::take(&mut self.ingest_plans);
        // Native caption readers exist only under `overlay` (they feed the
        // per-tile burn-in renderer); without it there are none.
        #[cfg(feature = "overlay")]
        let caption_plans = std::mem::take(&mut self.caption_plans);
        #[cfg(not(feature = "overlay"))]
        let caption_plans: Vec<crate::captions::CaptionPlan> = Vec::new();
        // AUD-2: spawn a per-source audio decode thread (the peer of the video
        // ingest) ONLY when this run opted into program audio — otherwise there is
        // no `ProgramBus` to consume the stores, so a decode thread would be pure
        // waste. Each pairs the source's plan with its `AudioStore` (already routed
        // onto the bus below); the thread fills the store, the bus samples it. When
        // audio is off, the plans are left in place (untouched) and no thread runs.
        let audio_plans: Vec<(
            crate::audio::AudioIngestPlan,
            Arc<multiview_audio::store::AudioStore>,
        )> = if self.encode_cfg.audio.is_some() {
            std::mem::take(&mut self.audio_ingest_plans)
                .into_iter()
                .filter_map(|plan| {
                    self.audio_stores
                        .get(&plan.id)
                        .map(|store| (plan, Arc::clone(store)))
                })
                .collect()
        } else {
            Vec::new()
        };
        // AUD-5: synthetic tone publish plans (the `bars` line-up tone). Spawned
        // ONLY when this run opted into program audio (otherwise there is no
        // `ProgramBus` consuming the stores). Each pairs the `bars` source's tone
        // plan with its `AudioStore` (already routed onto the bus below); the
        // thread fills the store with the 1 kHz tone, the bus samples it.
        let tone_plans: Vec<(
            crate::audio::ToneIngestPlan,
            Arc<multiview_audio::store::AudioStore>,
        )> = if self.encode_cfg.audio.is_some() {
            std::mem::take(&mut self.tone_ingest_plans)
                .into_iter()
                .filter_map(|plan| {
                    self.audio_stores
                        .get(&plan.id)
                        .map(|store| (plan, Arc::clone(store)))
                })
                .collect()
        } else {
            Vec::new()
        };
        // The supervisor registers every producer's per-thread stop flag in the
        // run's shared registry (ADR-W018) — the video decode thread under `{id}`,
        // its audio/tone/caption companions under `{id}/<role>` — so a live
        // `RemoveSource` tears down exactly that source's producers.
        let supervisor = IngestSupervisor::start(
            plans,
            audio_plans,
            tone_plans,
            caption_plans,
            &self.stop_registry,
        );

        // Prime the first frame per tile BEFORE constructing the runtime (whose
        // `new` seeds tick 0 to "now") and therefore before the output clock's
        // first tick (#40 startup-hold fix). See [`Self::prime_bound_tiles`] for
        // the bounded-wait rationale and the invariant-#1/#2 guarantees.
        self.prime_bound_tiles(ts.as_ref());

        // Build the SINGLE program encoder (invariant #7) the consumer owns: the
        // canvas is encoded once and the same packets fan to every mux-only sink.
        let encoder = ProgramEncoder::new(&self.encode_cfg).map_err(|e| PipelineError::Output {
            kind: "encode",
            reason: e.to_string(),
        })?;

        // The program-audio bus (AUD-4): built ONLY when this run opted into audio
        // (`encode_cfg.audio` is `Some`). It mixes one block per output tick at the
        // audio config's sample rate + channel layout, paced by the pipeline
        // cadence. AUD-2: every per-source `AudioStore` is routed onto the bus at
        // unity gain, so the bus mixes the REAL decoded audio the per-source decode
        // threads (spawned above) publish — not silence. AUD-5: the `bars`
        // synthetic source's store carries its 1 kHz line-up tone (published by its
        // tone thread) and is routed here identically. A source with no audio (the
        // `solid`/`clock` synthetic kinds, NDI, an audio-free clip) has no store
        // here and simply does not contribute (its absence reads as silence on the
        // mix). The bus moves into
        // the bake consumer (it is `Send`); it is ticked off the hot path, never on
        // the output-clock loop. `None` (audio off) means the consumer encodes no
        // audio at all, so the run is byte-identical to the video-only path.
        let audio_bus = self.encode_cfg.audio.as_ref().map(|cfg| {
            let mut bus = program_audio_bus(cfg, self.cadence);
            // Route every per-source store onto the program bus at unity gain. The
            // route key is the source id; per-source gains / breakaways are a
            // control-plane concern (AUD-7) that re-points/re-gains these routes at
            // runtime. Sorted for a deterministic route order across runs.
            let mut ids: Vec<&String> = self.audio_stores.keys().collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(store) = self.audio_stores.get(id) {
                    let _ = bus.add_source(id.clone(), Arc::clone(store), 1.0);
                }
            }
            bus
        });
        // The program-bus loudness normaliser (AUD-6): pair one with the audio bus
        // so the mixed program is normalised toward the target LUFS with a
        // true-peak ceiling BEFORE encode, while discrete tracks stay unaltered
        // (ADR-R005/R006). Built from the same audio config (so its format matches
        // the bus); `None` when audio is off. Default target: the -16 LUFS
        // streaming/web level (this output is a live streaming multiview).
        let audio_loudnorm = self.encode_cfg.audio.as_ref().and_then(program_loudnorm);

        // Spawn the per-sink fan-out threads + the single bake consumer thread
        // BEFORE the engine loop, so a frame produced this tick is baked +
        // encoded WHILE the engine keeps ticking (streaming, not batch — ADR-0025).
        // The consumer owns a Send `BakeContext` and builds its own (non-Send)
        // overlay baker from it, plus the single `ProgramEncoder`; the bake +
        // encode math moved off the hot loop, never onto it.
        let bake_ctx = self.bake_context();
        let (egress, hot_tx) = StreamEgress::spawn(
            bake_ctx,
            runners,
            policy,
            encoder,
            audio_bus,
            audio_loudnorm,
        );

        // Build the single program now (post-prime, ADR-0030 MP-0): the run path
        // flows through ONE `MultiviewProgram` that owns this program's clock +
        // compositor drive + (internally) `EngineRuntime` + its own stop handle.
        // `MultiviewProgram::new` reads tick 0's seed from `ts` here (via the
        // wrapped `EngineRuntime`), so tick 0 is due at this instant — the prime
        // delay sits before the epoch and is never paid back as a burst, exactly
        // as the previous inline `EngineRuntime::new` did. The program clones the
        // caller's `stop` (Arc-backed) as its own handle, so the existing Ctrl-C /
        // control-plane stop still ends the run unchanged. A spec/cadence mismatch
        // is a build-assembly bug, surfaced as a typed error (never a panic).
        let mut program =
            MultiviewProgram::new(&self.program_spec, clock, drive, ts, pacer, stop.clone())
                .map_err(|e| PipelineError::Program {
                    program: self.program_spec.id.clone(),
                    reason: e.to_string(),
                })?;

        // The hot-loop drop counter (live drop-on-overload) and the queue
        // high-watermark probe. Both are wait-free atomics shared with the
        // consumer; neither can back-pressure the engine (inv #10).
        let dropped = Arc::new(AtomicU64::new(0));
        let in_flight = Arc::clone(&egress.in_flight);
        let peak_occupancy = Arc::clone(&egress.peak_occupancy);

        let hot_dropped = Arc::clone(&dropped);
        // A read-only clone for the per-tick event projection: it samples this
        // wait-free counter to emit a change-driven, rate-limited `shed.load`
        // event when the encode/egress drop-on-overload shed actually fires
        // (invariants #1/#10 — the publish rides the drop-oldest broadcast and
        // the engine never blocks on it). Only the producer (`hot_dropped`) ever
        // writes it; this side only reads.
        let shed_dropped = Arc::clone(&dropped);
        // The per-tile content-fault detector: shares (by `Arc`) the SAME
        // lock-free per-source last-good stores the engine samples, plus the
        // build-time per-source meter timeline (for silence). Sampling-only and
        // non-blocking. Only built under `overlay` (the badge renderer).
        #[cfg(feature = "overlay")]
        let caption_stores = self.caption_stores.clone();
        // RT-10b: the per-layer subtitle crosspoint. One re-pointable
        // `SubtitleLayer` per source-bound caption store (identity routing by
        // default), sampled per output tick via `active_at(now)` so subtitle
        // rendering goes through the re-pointable layer. A run with no caption
        // stores builds an empty router (no layers): sampling it yields an empty
        // map, exactly like the old `sample_caption_stores`. The router is sampled
        // (`&mut`) inside the hot-loop projection; its `SubtitleRouteHandle` is
        // published into the shared slot so the control plane (`RouteSubtitle`,
        // RT-11) can drive a breakaway into the running pipeline (inv #1/#10).
        #[cfg(feature = "overlay")]
        let mut subtitle_router = crate::captions::SubtitleRouter::from_stores(
            caption_stores
                .iter()
                .map(|(id, store)| (id.clone(), Arc::clone(store))),
        );
        #[cfg(feature = "overlay")]
        self.subtitle_route
            .store(Some(Arc::new(subtitle_router.handle())));
        // Resolve each operator-declared probe's watched CELL to its bound SOURCE
        // via the solved layout, so the per-tick fault detector can build that
        // source's fault machine from the declared threshold/zone/dwell/severity
        // (M10 — the config→analyser → X.733 driver). A probe whose cell is unbound
        // is skipped (no source to sample). This runs once at run start, off the
        // hot loop.
        #[cfg(feature = "overlay")]
        let source_probes = resolve_source_probes(&self.layout, &self.declared_probes);
        #[cfg(feature = "overlay")]
        let mut fault_detector = FaultDetector::new(
            self.stores.clone(),
            self.meter_db_timelines.clone(),
            self.cadence,
            source_probes,
        );
        // RT-3 read-only stream-inventory discovery (off the hot loop, inv #10):
        // the per-input inventories were probed ONCE at build time. Emit one
        // `input.streams` event per input here — at run start, BEFORE the clock
        // loop — so a connected client sees the inventory delta exactly once
        // (the inventory is static after open; a re-probe would replace it and
        // re-emit). The publish rides the wait-free drop-oldest broadcast, never
        // a channel a client can fill. Then pre-build the `inputs` snapshot
        // fragment ONCE so the per-tick projection only clones + inserts it
        // (no inventory re-serialisation on the hot loop).
        for event in crate::control::input_streams_events(&self.inventories) {
            publisher.publish_event(event);
        }
        let input_fragment = crate::control::input_inventories_fragment(&self.inventories);
        // DEV-B1 / ADR-0044: start the configured DRM/KMS display heads NOW —
        // startup, before the output clock runs — and keep their handles alive
        // for the whole run (drop = stop + join, off the hot path). Each sink
        // owns its device on a dedicated thread; the engine side holds only
        // wait-free mailbox publishers, fed in `state_of` exactly where the
        // live-preview slot is filled. A startup failure (no such connector,
        // no usable mode, modeset rejected) fails the run like any other
        // misconfigured output; after startup the sinks can never fail the
        // engine (invariants #1 + #10).
        #[cfg(feature = "display-kms")]
        let (_display_handles, display_publishers) =
            start_display_sinks(std::mem::take(&mut self.display_plans), self.cadence)?;
        // The hot-loop projection runs once per tick. It SAMPLES the caption/fault
        // state (kept here on the hot loop — the bounded cue store holds only a
        // small live window, so it must be sampled now, not after the run), clones
        // the canvas into one `Arc` (no more than today), builds a `StreamItem`,
        // and hands it to the bake consumer over the bounded queue per `policy`:
        // blocking send offline (exact-N back-pressure on the renderer), wait-free
        // `try_send` + drop-and-count live (the engine never blocks — inv #1/#10).
        let state_of = move |frame: &CompositedFrame| -> EngineStateSnapshot {
            // RT-10b: sample the per-layer subtitle router (drains any pending
            // re-point at this sample boundary, then `active_at(pts)` per layer) —
            // identical text lines to the old per-source `sample_caption_stores`
            // when no layer was re-pointed. The bitmap (DVB-sub) cues stay on the
            // per-source path below (the text-layer primitive RT-10a built is
            // text-only).
            #[cfg(feature = "overlay")]
            let captions = subtitle_router.sample(frame.pts());
            #[cfg(feature = "overlay")]
            let caption_bitmaps = sample_caption_bitmaps(&caption_stores, frame.pts());
            #[cfg(feature = "overlay")]
            let faults = fault_detector.sample(frame.pts(), frame.tick.index, &frame.source_states);
            if let Some(obs) = hot_tick_observer.as_ref() {
                obs.fetch_add(1, Ordering::Release);
            }
            // The composited canvas, cloned once into an `Arc` reused for BOTH
            // the bake/encode fan-out AND the live-preview slot (a single wait-
            // free swap; the control plane serves the latest still off it).
            let canvas = Arc::new(frame.canvas.clone());
            preview.store(Some(Arc::clone(&canvas)));
            // The display heads ride the SAME pre-encode canvas `Arc` through
            // their wait-free mailboxes (one atomic bump + one lock-free swap
            // each — the engine never awaits a sink; ADR-0044, inv #1/#10).
            #[cfg(feature = "display-kms")]
            for display in &display_publishers {
                display.publish(CanvasFrame(Arc::clone(&canvas)));
            }
            let item = StreamItem {
                canvas: Arc::clone(&canvas),
                tick_index: frame.tick.index,
                #[cfg(feature = "overlay")]
                source_states: frame.source_states.clone(),
                #[cfg(feature = "overlay")]
                captions,
                #[cfg(feature = "overlay")]
                caption_bitmaps,
                #[cfg(feature = "overlay")]
                faults,
            };
            match policy {
                SendPolicy::BlockForExact => {
                    // A blocking send back-pressures the OFFLINE renderer (not a
                    // live clock) until the consumer drains — exact-N, never drops.
                    // Disconnect only happens if the consumer died; nothing to do.
                    if hot_tx.send(item).is_ok() {
                        bump_occupancy(&in_flight, &peak_occupancy);
                    }
                }
                SendPolicy::DropOnOverload => match hot_tx.try_send(item) {
                    Ok(()) => bump_occupancy(&in_flight, &peak_occupancy),
                    Err(TrySendError::Full(_)) => {
                        // Shed and COUNT — never block (inv #1/#10).
                        hot_dropped.fetch_add(1, Ordering::Release);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        // The consumer ended (it cannot, mid-run, in normal
                        // operation); count as a drop and keep ticking.
                        hot_dropped.fetch_add(1, Ordering::Release);
                    }
                },
            }
            let mut snapshot = crate::control::state_snapshot(
                frame.tick.index,
                frame.pts().as_nanos(),
                frame.canvas.width(),
                frame.canvas.height(),
            );
            // Thread the per-input stream inventories into the conflated blob so
            // the control plane's read-only `GET /inputs/{id}/streams` can show
            // every stream (RT-3). The fragment is pre-built (a tiny static clone
            // per tick); a `None` fragment (no inputs probed) is a no-op.
            crate::control::insert_input_fragment(&mut snapshot, input_fragment.as_ref());
            // And the per-tile lifecycle states, so a connecting client is
            // seeded with the CURRENT tile states (the `tiles` `$snapshot`)
            // instead of waiting for the next sparse `tile.state` delta.
            crate::control::fold_tile_states(&mut snapshot, &frame.source_states);
            snapshot
        };
        // Sparse tile-state events: emit at most one `tile.state` change per tick
        // (seed each tile once, then on transitions), keyed by the source id, so
        // the monitoring UI shows live per-tile lifecycle without a per-tick flood.
        let mut last_states: std::collections::HashMap<String, SourceState> =
            std::collections::HashMap::new();
        // Change-driven, rate-limited shed-load emission state. A sustained
        // overload sheds many frames; we emit at most one `shed.load` per
        // `SHED_EVENT_EVERY_TICKS` window AND only when the cumulative drop
        // counter advanced — so the §7.2 retention store + the WebUI see the shed
        // condition without a per-dropped-frame flood (inv #10). Carries the
        // cumulative `dropped` so the trend is recoverable even across coalesced
        // windows.
        let mut last_shed_dropped: u64 = 0;
        let mut last_shed_tick: u64 = 0;
        let event_of = move |frame: &CompositedFrame| -> Option<Event> {
            for (source, &state) in &frame.source_states {
                if last_states.get(source) != Some(&state) {
                    let from = last_states.get(source).copied().unwrap_or(state);
                    last_states.insert(source.clone(), state);
                    return Some(Event::TileState(multiview_events::TileState {
                        from: from.into(),
                        to: state.into(),
                        input: Some(source.clone()),
                        trigger: "state_change".to_owned(),
                    }));
                }
            }
            // The live shed: under DropOnOverload the encode/egress consumer fell
            // behind and the hot loop shed-and-counted a composited frame. Emit a
            // change-driven, rate-limited `shed.load` (the pure decision lives in
            // `shed_load_event` so it is exhaustively unit-testable).
            let dropped_now = shed_dropped.load(Ordering::Acquire);
            shed_load_event(
                dropped_now,
                frame.tick.index,
                &mut last_shed_dropped,
                &mut last_shed_tick,
            )
        };

        // Drive the protected per-tick loop through the program. The program owns
        // its stop handle (a clone of the caller's `stop`), so — unlike the inline
        // `EngineRuntime` path — the run methods no longer take `stop`; the same
        // bounded (`run_for`) vs forever (`run`) split + the same publisher /
        // state/event projections / per-frame control hook are threaded verbatim.
        let outcome = match max_ticks {
            Some(max) => {
                program
                    .run_for_with_control(publisher, max, state_of, event_of, control)
                    .await
            }
            None => {
                program
                    .run_with_control(publisher, state_of, event_of, control)
                    .await
            }
        }
        .map_err(|e| PipelineError::Engine(e.to_string()));

        // The clock has stopped (bounded budget reached, or `stop` raised): tear
        // ingest down deterministically (signal + join), then close the egress and
        // finalise. The hot queue is already closed by this point — see below.
        supervisor.shutdown();
        // The engine loop consumed the `state_of` closure (the ONLY owner of the
        // hot sender) when `run_for`/`run` returned above, so the hot queue is
        // already closed → the bake consumer's `recv()` has seen end-of-input,
        // drains the queue, fans the remainder, and drops the per-sink senders →
        // each sink sees its channel close and finalises (writes its trailer).
        // Dropping the program here only releases the engine's own resources
        // (its wrapped `EngineRuntime` = clock + drive + time source + pacer).
        drop(program);

        // Join the bake consumer + sink threads FIRST — folding their outcome and
        // writing the trailers (the sinks' own `run()` finalisation) — BEFORE
        // propagating any engine error, so a mid-run engine error still finalises
        // the partial output and never detaches the egress threads with a trailer
        // unwritten. The engine error (the root cause) is surfaced first; an
        // egress/sink error second.
        let egress_result = egress.join();
        let outcome = outcome?;
        let egress_out = egress_result?;

        let dropped_count = dropped.load(Ordering::Acquire);
        let report = PipelineReport {
            frames: outcome.ticks,
            cadence: self.cadence,
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
            encoder: self.encoder.name.clone(),
            outputs: egress_out.lines,
            // Honest falter (ADR-0025): a live run that shed frames faltered;
            // offline never drops, so it never falters here.
            dropped: dropped_count,
            faltered: dropped_count > 0,
        };
        Ok(DriveStreamOutcome {
            report,
            peak_occupancy: egress_out.peak_occupancy,
            sink_frames: egress_out.sink_frames,
        })
    }

    /// Bounded **prime-wait** before the output clock's first tick (#40): hold
    /// the very first tick until every cell-bound source's [`TileStore`] has
    /// published one frame — so tick 0 samples real content, not the cold
    /// last-good / slate placeholder (the ~0.75 s "held first frame" startup
    /// transient). Called after the ingest threads are spawned and *before*
    /// [`EngineRuntime::new`] seeds tick 0, so the prime delay sits before the
    /// epoch and is never paid back as a catch-up burst.
    ///
    /// CRITICAL invariant #1/#2: the wait is hard-capped at [`PRIME_WAIT_BUDGET`]
    /// measured by `clock` (the same monotonic source the engine uses), so a
    /// source that never produces — the deliberately-missing source, a
    /// dead/wedged live input — can NOT block startup. Once the budget elapses
    /// the caller proceeds anyway and that tile rides its `NO_SIGNAL` / last-good
    /// placeholder (already produced by [`TileStore::read_at`]). It only delays
    /// the first tick; the cadence and per-tick logic are unchanged and no input
    /// ever paces the output. Bound sources only: an unbound source (no cell) is
    /// never sampled, so it must not be waited on.
    fn prime_bound_tiles(&self, clock: &dyn TimeSource) {
        let bound_ids: std::collections::HashSet<&str> = self
            .layout
            .cells
            .iter()
            .filter_map(|c| c.source.as_deref())
            .collect();
        let prime_stores: Vec<&Arc<TileStore<Nv12Image>>> = self
            .stores
            .iter()
            .filter(|(id, _)| bound_ids.contains(id.as_str()))
            .map(|(_, store)| store)
            .collect();
        let prime = wait_for_prime(
            &prime_stores,
            PRIME_WAIT_BUDGET,
            PRIME_WAIT_POLL,
            clock,
            std::thread::sleep,
        );
        if prime.all_primed {
            tracing::debug!(
                primed = prime.primed,
                total = prime.total,
                waited_ms = prime.waited.as_millis(),
                "all tiles primed before first output tick"
            );
        } else {
            tracing::warn!(
                primed = prime.primed,
                total = prime.total,
                waited_ms = prime.waited.as_millis(),
                "prime-wait budget elapsed with unprimed tile(s); starting clock anyway \
                 (they ride NO_SIGNAL/last-good — invariant #1)"
            );
        }
    }

    /// Build the per-tile [`TileSpec`](crate::overlays::TileSpec) list from the
    /// solved layout's cells: one entry per source-bound cell, carrying the cell's
    /// pixel rectangle and the source's display label.
    #[cfg(feature = "overlay")]
    fn tile_specs(&self) -> Vec<crate::overlays::TileSpec> {
        use multiview_overlay::geometry::PixelRect;
        let (cw, ch) = (self.layout.canvas.width, self.layout.canvas.height);
        let mut specs = Vec::new();
        for cell in &self.layout.cells {
            let Some(id) = cell.source.as_deref() else {
                continue;
            };
            let label = self
                .tile_labels
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_owned());
            let rect = PixelRect {
                x: norm_to_px_f32(cell.x, cw),
                y: norm_to_px_f32(cell.y, ch),
                width: norm_to_px_f32(cell.w, cw),
                height: norm_to_px_f32(cell.h, ch),
            };
            specs.push(crate::overlays::TileSpec::new(id, label, rect));
        }
        specs
    }

    /// Extract the **`Send`** bake context the off-hot-path consumer thread owns
    /// (ADR-0025). The consumer builds its own (non-`Send`) [`OverlayBaker`] from
    /// this on its thread; only the plain owned data (tile specs, per-source meter
    /// timelines, sidecar track + target, analog clock, canvas color, cadence)
    /// crosses the thread boundary — the bake math is unchanged, only its call
    /// site moves off the hot loop. With the `overlay` feature off the context is
    /// just the canvas color + cadence (the bake is an identity pass-through).
    #[cfg(feature = "overlay")]
    fn bake_context(&self) -> BakeContext {
        BakeContext {
            tile_specs: self.tile_specs(),
            meter_db_timelines: self.meter_db_timelines.clone(),
            subtitles: self.subtitles.clone(),
            sidecar_target: self.sidecar_target.clone(),
            analog_clock: self.analog_clock,
            canvas_color: self.canvas_color,
            cadence: self.cadence,
            watermark_signal: self.watermark_signal.clone(),
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
        }
    }

    /// The `Send` bake context with the `overlay` feature off: only the data the
    /// identity bake needs to pass the canvas through and stamp its media time.
    #[cfg(not(feature = "overlay"))]
    #[allow(clippy::unused_self)]
    fn bake_context(&self) -> BakeContext {
        BakeContext {}
    }
}

/// The `Send` per-frame bake context the streaming consumer thread owns
/// (ADR-0025). It carries exactly the owned data the off-hot-path bake reads;
/// the consumer builds its own non-`Send` [`OverlayBaker`] from it once and
/// rasterises one frame per received [`StreamItem`], keeping the bake math
/// identical to the old post-loop batch path.
struct BakeContext {
    /// The per-tile overlay placements (cell rect + label per source-bound cell).
    #[cfg(feature = "overlay")]
    tile_specs: Vec<crate::overlays::TileSpec>,
    /// Build-time per-source per-tick loudness timelines (dBFS) for the meters.
    #[cfg(feature = "overlay")]
    meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
    /// The optional legacy `--subtitles` sidecar track (burned into its target).
    #[cfg(feature = "overlay")]
    subtitles: Option<multiview_overlay::subtitle::CueTrack>,
    /// The source id the sidecar burns into (the first source-bound cell).
    #[cfg(feature = "overlay")]
    sidecar_target: Option<String>,
    /// An optional analog clock face placement.
    #[cfg(feature = "overlay")]
    analog_clock: Option<crate::overlays::AnalogClockSpec>,
    /// The fixed canvas color (for the overlay blend + output tag).
    #[cfg(feature = "overlay")]
    canvas_color: CanvasColor,
    /// The fixed output cadence (for the per-frame media time).
    #[cfg(feature = "overlay")]
    cadence: Rational,
    /// The Conspect tile-watermark signal (S3): sampled each baked frame to decide
    /// whether to stamp the corner watermark. `None` never watermarks.
    #[cfg(feature = "overlay")]
    watermark_signal: Option<crate::licence::WatermarkSignal>,
    /// The canvas width the corner watermark anchors to.
    #[cfg(feature = "overlay")]
    canvas_width: u32,
    /// The canvas height the corner watermark anchors to.
    #[cfg(feature = "overlay")]
    canvas_height: u32,
}

impl BakeContext {
    /// The loudness (dBFS) to show for source `id` at output tick `i` (mirrors
    /// the old `Pipeline::meter_db_for`): that source's own per-tick build-time
    /// timeline, falling back to the meter floor for an audio-free source.
    #[cfg(feature = "overlay")]
    fn meter_db_for(&self, id: &str, i: usize) -> f64 {
        match self.meter_db_timelines.get(id) {
            Some(timeline) => timeline
                .get(i)
                .copied()
                .or_else(|| timeline.last().copied())
                .unwrap_or(multiview_audio::Ballistics::FLOOR_DB),
            None => multiview_audio::Ballistics::FLOOR_DB,
        }
    }

    /// The legacy `--subtitles` sidecar lines active for source `id` at `pts` if
    /// `id` is the sidecar target (mirrors the old `sidecar_caption_lines`).
    #[cfg(feature = "overlay")]
    fn sidecar_caption_lines(&self, id: &str, pts: MediaTime) -> Option<Vec<String>> {
        if self.sidecar_target.as_deref() != Some(id) {
            return None;
        }
        let cue = self.subtitles.as_ref().and_then(|t| t.active_cue(pts))?;
        if cue.lines.is_empty() {
            None
        } else {
            Some(cue.lines.clone())
        }
    }
}

/// The non-`Send` overlay rasteriser the consumer thread builds once from a
/// [`BakeContext`] and reuses to bake each frame. With the `overlay` feature off
/// this is a zero-field identity baker (the canvas passes through unchanged).
#[cfg(feature = "overlay")]
struct StreamBaker {
    baker: crate::overlays::OverlayBaker,
    canvas_color: CanvasColor,
    cadence: Rational,
    meter: BakeContext,
}

#[cfg(not(feature = "overlay"))]
struct StreamBaker;

#[cfg(feature = "overlay")]
impl StreamBaker {
    /// Build the rasteriser from the owned context, on the consumer thread.
    ///
    /// # Errors
    /// Returns [`PipelineError::Engine`] if the bundled fonts fail to load.
    fn new(ctx: BakeContext) -> Result<Self, PipelineError> {
        // Drive the on-screen clock from the REAL OS clock (CLOCK_REALTIME via
        // std; the host disciplines it via NTP). The displayed time-of-day is
        // sampled live at each bake (anti-drift) — a pure display concern wholly
        // independent of the engine's output cadence (invariant #1).
        let mut baker = crate::overlays::OverlayBaker::new(
            ctx.tile_specs.clone(),
            crate::wallclock::WallClockSource::system(),
        )
        .map_err(|e| PipelineError::Engine(format!("overlay baker: {e}")))?;
        if let Some(spec) = ctx.analog_clock {
            baker = baker.with_analog_clock(spec);
        }
        // The Conspect tile-watermark seam (S3, ADR-0050 §5): when a signal is
        // wired, the bake stamps the corner watermark on the composited multiview
        // canvas whenever the published ladder level is at a watermark rung. The
        // per-frame decision is a single wait-free `arc_swap` load off the hot loop.
        if let Some(signal) = ctx.watermark_signal.clone() {
            baker = baker.with_watermark(signal, ctx.canvas_width, ctx.canvas_height);
        }
        Ok(Self {
            baker,
            canvas_color: ctx.canvas_color,
            cadence: ctx.cadence,
            meter: ctx,
        })
    }

    /// Bake one streamed tick's overlays into its canvas, returning the overlaid
    /// `Arc<Nv12Image>` — the SAME math as the old post-loop `bake_overlays`, run
    /// once per received item off the hot loop.
    ///
    /// # Errors
    /// Returns [`PipelineError::Engine`] if the baker/sub-pass rejects the canvas.
    fn bake(&mut self, item: &StreamItem) -> Result<Arc<Nv12Image>, PipelineError> {
        use multiview_compositor::overlay::apply_overlays_to_nv12;

        let i = usize::try_from(item.tick_index).unwrap_or(usize::MAX);
        let pts = MediaTime::from_tick(i64::try_from(i).unwrap_or(i64::MAX), self.cadence);
        let mut dynamics = std::collections::HashMap::new();
        // Native cues were sampled per-tick into `item.captions` (the cue store is
        // a small live window); the in-memory sidecar track has no eviction, so it
        // is sampled here at `pts`. Native wins on overlap.
        let mut captions: std::collections::HashMap<String, Vec<String>> = item.captions.clone();
        for spec in self.baker.tiles() {
            let state = item
                .source_states
                .get(&spec.source_id)
                .copied()
                .unwrap_or(multiview_core::traits::SourceState::NoSignal);
            let fault = item
                .faults
                .get(&spec.source_id)
                .copied()
                .unwrap_or(crate::overlays::TileFault::None);
            dynamics.insert(
                spec.source_id.clone(),
                crate::overlays::TileDynamics {
                    meter_db: self.meter.meter_db_for(&spec.source_id, i),
                    state,
                    fault,
                },
            );
            if !captions.contains_key(&spec.source_id) {
                if let Some(lines) = self.meter.sidecar_caption_lines(&spec.source_id, pts) {
                    captions.insert(spec.source_id.clone(), lines);
                }
            }
        }
        let list = self
            .baker
            .draw_list(pts, &dynamics, &captions, &item.caption_bitmaps)
            .map_err(|e| PipelineError::Engine(format!("overlay draw: {e}")))?;
        let overlaid = apply_overlays_to_nv12(&item.canvas, &list, self.canvas_color)
            .map_err(|e| PipelineError::Engine(format!("overlay blend: {e}")))?;
        Ok(Arc::new(overlaid))
    }
}

#[cfg(not(feature = "overlay"))]
impl StreamBaker {
    /// Overlays disabled at compile time: a zero-cost identity baker.
    #[allow(clippy::unnecessary_wraps)]
    fn new(_ctx: BakeContext) -> Result<Self, PipelineError> {
        Ok(Self)
    }

    /// Hand back the bare canvas unchanged (no overlay sub-pass compiled in).
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn bake(&mut self, item: &StreamItem) -> Result<Arc<Nv12Image>, PipelineError> {
        Ok(Arc::clone(&item.canvas))
    }
}

/// One streamed output tick handed off the hot loop to the bake consumer over
/// the bounded queue (ADR-0025): the composited canvas (one cheap `Arc` clone —
/// no more than the old collector) plus the per-source state SAMPLED on the hot
/// loop this tick (captions/faults must be sampled now, before the small live
/// cue window evicts them). The bake *render* runs on the consumer thread.
struct StreamItem {
    /// The composited canvas the protected output core emitted this tick.
    canvas: Arc<Nv12Image>,
    /// The engine's tick index for this frame (drives the per-frame media time
    /// and the per-source meter timeline lookup in the consumer's bake).
    // reason: only the `overlay`-on baker reads this (per-frame pts + meter index);
    // with overlays compiled out the bake is an identity pass-through that needs
    // only the canvas, so the field is legitimately unread in that build. It is
    // still set unconditionally to keep one `StreamItem` shape across features.
    #[cfg_attr(not(feature = "overlay"), allow(dead_code))]
    tick_index: u64,
    /// Per-source lifecycle state sampled this tick (`source_id -> state`); a
    /// source absent here is treated as `NO_SIGNAL` by the baker.
    #[cfg(feature = "overlay")]
    source_states: std::collections::HashMap<String, multiview_core::traits::SourceState>,
    /// Per-source active caption lines sampled from the cue stores at THIS tick's
    /// pts (`source_id -> on-screen lines`). Sampled on the hot loop because the
    /// bounded drop-oldest cue store only holds a small live window — sampling it
    /// later would miss cues evicted meanwhile. A source with no active cue is
    /// absent.
    #[cfg(feature = "overlay")]
    captions: std::collections::HashMap<String, Vec<String>>,
    /// Per-source active **bitmap** caption cue sampled at THIS tick's pts. Same
    /// hot-loop sampling rationale as `captions`. A source with no active bitmap
    /// cue is absent.
    #[cfg(feature = "overlay")]
    caption_bitmaps: std::collections::HashMap<String, multiview_ffmpeg::caption::CueBitmap>,
    /// Per-source content fault sampled this tick (`source_id -> fault`), folded
    /// through dwell/hysteresis by the [`FaultDetector`] on the hot loop (freeze
    /// needs the previous sampled frame + the dwell needs every tick in order). A
    /// healthy source maps to [`crate::overlays::TileFault::None`] (or is absent).
    #[cfg(feature = "overlay")]
    faults: std::collections::HashMap<String, crate::overlays::TileFault>,
}

/// The human report line + optional playlist text one sink runner produced, plus
/// the number of frames it consumed (for the test seam's observability).
struct SinkRunOutcome {
    /// The human-facing report line for this output (path + packet/segment counts).
    line: String,
    /// For an HLS sink, the rendered media playlist text + its on-disk path to
    /// write after the encode completes. `None` for a file/push sink.
    playlist: Option<(PathBuf, String)>,
    /// How many baked frames this sink consumed (for tests).
    frames: usize,
}

/// A boxed closure that drives ONE mux-only output sink to completion over its
/// bounded fan-out channel of coded packets, run on its own thread by the
/// [`StreamEgress`]. Production builds one per [`RunnableOutput`] (calling
/// `PacketMuxSink::run` over a [`StreamingPacketSource`], seeded by the single
/// encoder's [`StreamCodecParameters`] + time-base); the test seam injects fakes.
type SinkRunner = Box<
    dyn FnOnce(
            Receiver<EncodedPacket>,
            StreamCodecParameters,
            Rational,
            Option<(StreamCodecParameters, Rational)>,
        ) -> Result<SinkRunOutcome, PipelineError>
        + Send,
>;

/// What one **test** fake sink reports after consuming its fan-out channel:
/// the number of baked frames it received (ADR-0025 test seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestSinkOutcome {
    /// How many baked frames the fake sink received before end-of-program.
    pub frames: usize,
}

/// A boxed fake-sink closure the [`Pipeline::drive_streaming_for_test`] seam
/// injects: it consumes the coded-packet fan-out [`Receiver`] (each
/// [`EncodedPacket`] the single encoder produced) and returns a
/// [`TestSinkOutcome`]. It runs on its own off-hot-path thread, exactly like a
/// production mux sink, so a test can block/slow/count it to assert the streaming
/// concurrency contract.
pub type TestSinkRunner = Box<dyn FnOnce(Receiver<EncodedPacket>) -> TestSinkOutcome + Send>;

/// The outcome of [`Pipeline::drive_streaming`]: the report plus streaming
/// observability folded from the egress threads.
struct DriveStreamOutcome {
    report: PipelineReport,
    peak_occupancy: usize,
    sink_frames: Vec<usize>,
}

/// The injected parameters for the ADR-0025 streaming test seam
/// ([`Pipeline::drive_streaming_for_test`]): the time source + pacer, the
/// tick budget, the send policy, the fake sink runners, and an optional hot-loop
/// tick observer. Bundled into one struct so the seam keeps a small argument
/// list and the test reads as a record of named knobs.
pub struct StreamTestParams<P> {
    /// The injected time source (typically a
    /// [`ManualTimeSource`](multiview_engine::ManualTimeSource)).
    pub time: Arc<dyn TimeSource>,
    /// The injected pacer (typically a
    /// [`CooperativePacer`](multiview_engine::CooperativePacer)).
    pub pacer: P,
    /// The tick budget (`Some(N)` for a bounded run, `None` to run until `stop`).
    pub max_ticks: Option<u64>,
    /// The send policy (offline block-for-exact vs live drop-on-overload).
    pub policy: SendPolicy,
    /// One fake sink runner per output, run on its own off-hot-path thread.
    pub runners: Vec<TestSinkRunner>,
    /// Optional hot-loop tick observer (incremented once per emitted tick).
    pub hot_tick_observer: Option<Arc<AtomicU64>>,
}

/// The observable result of the ADR-0025 streaming test seam.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamTestResult {
    /// The pipeline report (frames, dropped, faltered, …).
    pub report: PipelineReport,
    /// The peak number of frames ever in flight in the hot-loop → consumer queue
    /// (the high-watermark), proving memory is bounded by the cap.
    pub peak_occupancy: usize,
    /// The bounded streaming-queue capacity the run used (per [`SendPolicy`]).
    pub capacity: usize,
    /// Per-sink frame counts (in runner order) the fake sinks reported.
    pub sink_frames: Vec<usize>,
}

/// Increment the in-flight occupancy and update the high-watermark. Wait-free
/// atomics shared with the consumer; this is the only bookkeeping the hot loop
/// does on a successful send and it can never back-pressure the engine (inv #10).
fn bump_occupancy(in_flight: &AtomicI64, peak: &AtomicUsize) {
    let now = in_flight.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    if now > 0 {
        let now_usize = usize::try_from(now).unwrap_or(usize::MAX);
        // Monotonic max via compare-and-set; a benign race only ever raises it.
        let mut observed = peak.load(Ordering::Acquire);
        while now_usize > observed {
            match peak.compare_exchange_weak(
                observed,
                now_usize,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(prev) => observed = prev,
            }
        }
    }
}

/// The single off-hot-path egress: one bake-consumer thread (owns the
/// [`StreamBaker`] built from the [`BakeContext`]) + one thread per sink. The
/// consumer receives [`StreamItem`]s over the bounded hot queue, bakes each into
/// an `Arc<Nv12Image>`, and fans it (blocking send — off the hot path, so it can
/// only pace the consumer, never the engine) to each sink's bounded channel.
struct StreamEgress {
    consumer: JoinHandle<Result<(), PipelineError>>,
    sinks: Vec<JoinHandle<Result<SinkRunOutcome, PipelineError>>>,
    /// Frames currently queued in the hot-loop → consumer channel (bumped by the
    /// hot loop on send, decremented by the consumer on recv).
    in_flight: Arc<AtomicI64>,
    /// The high-watermark of `in_flight` — the evidence of bounded memory
    /// (O(cap), independent of run length). It is a gauge: with the single engine
    /// sender incrementing after a send and the single consumer decrementing after
    /// `recv`, its ceiling is `cap + 1` (one in-transit frame can be double-counted
    /// at the full boundary), while the channel itself never buffers more than
    /// `cap`. So a reader must bound it by `cap + 1`, not `cap`.
    peak_occupancy: Arc<AtomicUsize>,
}

/// The folded outcome of joining the egress threads: the report lines (in sink
/// order), the peak queue occupancy, and the per-sink frame counts.
struct EgressOutcome {
    lines: Vec<String>,
    peak_occupancy: usize,
    sink_frames: Vec<usize>,
}

impl StreamEgress {
    /// Spawn the per-sink threads + the single bake consumer, returning the egress
    /// handle and the **hot** sender the engine's projection sends `StreamItem`s
    /// on. Dropping that sender (when the engine loop ends) closes the hot queue
    /// → the consumer drains, fans the remainder, drops the sink senders → each
    /// sink sees end-of-program and finalises → [`StreamEgress::join`] folds stats.
    fn spawn(
        ctx: BakeContext,
        runners: Vec<SinkRunner>,
        policy: SendPolicy,
        encoder: ProgramEncoder,
        audio_bus: Option<multiview_audio::program::ProgramBus>,
        audio_loudnorm: Option<multiview_audio::LoudnormProcessor>,
    ) -> (Self, SyncSender<StreamItem>) {
        let in_flight = Arc::new(AtomicI64::new(0));
        let peak_occupancy = Arc::new(AtomicUsize::new(0));

        // The codec parameters + time-base every mux sink seeds its stream from
        // are snapshotted ONCE from the single encoder (invariant #7); the encoder
        // itself moves into the bake consumer below to do the one encode.
        let params = encoder.codec_params().clone();
        let time_base = encoder.time_base();
        // The program-audio stream's params + time-base, snapshotted the same way
        // when (and only when) the encoder carries an audio encoder (AUD-4). Paired
        // SAFELY (no unwrap) by `zip`: `Some` only when BOTH the params and the
        // time-base are present, which the encoder guarantees together. `None` for
        // a video-only run, so every sink registers a single video stream exactly
        // as before. Cloned per runner below, like `params`.
        let audio: Option<(StreamCodecParameters, Rational)> = encoder
            .audio_codec_params()
            .cloned()
            .zip(encoder.audio_time_base());

        // One bounded fan-out channel of coded packets + thread per sink. The sink
        // thread drives `PacketMuxSink::run` (or a test fake) over a
        // `StreamingPacketSource` — muxing the SAME packets the single encoder
        // produced, never re-encoding (encode-once-mux-many).
        let mut sink_txs: Vec<SyncSender<EncodedPacket>> = Vec::with_capacity(runners.len());
        let mut sinks = Vec::with_capacity(runners.len());
        for (i, runner) in runners.into_iter().enumerate() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(SINK_QUEUE_CAP);
            sink_txs.push(tx);
            let builder = std::thread::Builder::new().name(format!("multiview-sink-{i}"));
            let params = params.clone();
            let audio = audio.clone();
            match builder.spawn(move || runner(rx, params, time_base, audio)) {
                Ok(handle) => sinks.push(handle),
                Err(e) => {
                    // A sink thread that cannot spawn is a real failure (we cannot
                    // produce its artifact); drop its sender so nothing wedges, and
                    // record a thread that immediately returns the spawn error.
                    sink_txs.pop();
                    let reason = e.to_string();
                    sinks.push(spawn_failed_sink(reason));
                }
            }
        }

        let (hot_tx, hot_rx) = std::sync::mpsc::sync_channel::<StreamItem>(policy.queue_cap());
        let consumer_in_flight = Arc::clone(&in_flight);
        let builder = std::thread::Builder::new().name("multiview-bake-consumer".to_owned());
        let consumer = builder
            .spawn(move || {
                consumer_main(
                    ctx,
                    &hot_rx,
                    sink_txs,
                    encoder,
                    audio_bus,
                    audio_loudnorm,
                    &consumer_in_flight,
                )
            })
            .unwrap_or_else(|_| {
                // Spawning the consumer failed: fall back to a thread that does
                // nothing. The `hot_rx`/`sink_txs` moved into the failed closure
                // drop, closing the channels so the hot loop sees Disconnected and
                // each sink sees end-of-program — nothing wedges.
                std::thread::spawn(|| Ok(()))
            });

        (
            Self {
                consumer,
                sinks,
                in_flight,
                peak_occupancy,
            },
            hot_tx,
        )
    }

    /// Join the bake consumer + every sink thread, folding their outcomes. Joins
    /// the consumer FIRST (it drops the sink senders on the way out, so each sink
    /// then sees end-of-program and finalises), then each sink with a **bounded**
    /// wait. A panicked thread surfaces as an engine error rather than being
    /// swallowed.
    ///
    /// **Bounded teardown (ENG-1, invariant #1):** a sink that drained then wedged
    /// in its finalise (e.g. a push muxer blocked writing its trailer to a dead
    /// peer) must never hang `stop`. Each sink is given [`SINK_WEDGE_GRACE`] to
    /// finish (polled via `is_finished()`, never a `join()` that cannot return);
    /// a sink still running past the grace is REPORTED and DETACHED — its handle
    /// dropped without joining, its own muxer state freed in `Drop`, reaped at
    /// process exit — so teardown always completes. Joining the consumer first is
    /// itself bounded because the consumer's fan-out uses a bounded `send_timeout`,
    /// so a wedged sink can no longer block it either.
    ///
    /// # Errors
    /// Returns a [`PipelineError`] if the consumer panicked, or a sink that DID
    /// finish errored/panicked (a detached wedged sink is reported, not errored).
    fn join(self) -> Result<EgressOutcome, PipelineError> {
        self.consumer
            .join()
            .map_err(|_| PipelineError::Engine("bake consumer thread panicked".to_owned()))??;
        let mut lines = Vec::with_capacity(self.sinks.len());
        let mut frames = Vec::with_capacity(self.sinks.len());
        // A shared teardown budget: healthy sinks finish in milliseconds; only a
        // genuinely wedged sink consumes the grace before being detached.
        let deadline = Instant::now() + SINK_WEDGE_GRACE;
        for (i, handle) in self.sinks.into_iter().enumerate() {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                // Wedged: detach (drop the handle without joining) so `stop`
                // completes, and report it (never silently dropped).
                tracing::warn!(
                    sink = i,
                    "sink wedged finalising (teardown grace exceeded); detaching so stop \
                     completes (invariant #1)"
                );
                lines.push(format!(
                    "sink {i}: WEDGED (teardown grace exceeded, detached)"
                ));
                frames.push(0);
                continue;
            }
            let outcome = handle.join().map_err(|_| PipelineError::Output {
                kind: "sink",
                reason: "sink thread panicked".to_owned(),
            })??;
            // Write the HLS playlist (if any) once the segment encode completed.
            if let Some((path, text)) = outcome.playlist {
                std::fs::write(&path, text.as_bytes()).map_err(|e| PipelineError::Output {
                    kind: "hls",
                    reason: format!("writing playlist {}: {e}", path.display()),
                })?;
            }
            lines.push(outcome.line);
            frames.push(outcome.frames);
        }
        Ok(EgressOutcome {
            lines,
            peak_occupancy: self.peak_occupancy.load(Ordering::Acquire),
            sink_frames: frames,
        })
    }
}

/// Spawn a thread that immediately returns the given sink-spawn failure, so a
/// failed spawn surfaces as a real error at join rather than being silent.
fn spawn_failed_sink(reason: String) -> JoinHandle<Result<SinkRunOutcome, PipelineError>> {
    std::thread::spawn(move || {
        Err(PipelineError::Output {
            kind: "sink",
            reason: format!("could not spawn sink thread: {reason}"),
        })
    })
}

/// Build the program-audio bus (AUD-4) from the run's [`AudioEncodeConfig`] and
/// the pipeline cadence: an [`AudioFormat`](multiview_audio::AudioFormat) at the
/// config's sample rate + channel layout, paced by `cadence`. The channel count
/// selects mono/stereo, falling back to stereo for any other count (the program
/// bus mixes mono or stereo today). No sources are routed: the bus is silence
/// until per-source decode is wired (a later slice).
fn program_audio_bus(
    cfg: &multiview_output::AudioEncodeConfig,
    cadence: Rational,
) -> multiview_audio::program::ProgramBus {
    let layout = match cfg.channels {
        1 => multiview_audio::ChannelLayout::Mono,
        _ => multiview_audio::ChannelLayout::Stereo,
    };
    let format = multiview_audio::AudioFormat::new(cfg.sample_rate, layout);
    multiview_audio::program::ProgramBus::new(format, cadence)
}

/// Build the program-bus loudness normaliser (AUD-6) for the run's audio config.
///
/// EBU R128 / ITU-R BS.1770 normalisation applies to the **program bus only**
/// (discrete tracks stay unaltered — the ADR-R005/R006 authenticity guarantee).
/// The format mirrors [`program_audio_bus`] (so the normaliser's meter matches the
/// bus it processes). The default target is the `-16` LUFS streaming/web level
/// with the default `-1.5` dBTP true-peak ceiling (resilience-and-av §4.1). Returns
/// `None` only if the audio format is unusable (zero rate/channels — already
/// validated upstream), in which case the bus is emitted un-normalised rather than
/// failing the run.
fn program_loudnorm(
    cfg: &multiview_output::AudioEncodeConfig,
) -> Option<multiview_audio::LoudnormProcessor> {
    let layout = match cfg.channels {
        1 => multiview_audio::ChannelLayout::Mono,
        _ => multiview_audio::ChannelLayout::Stereo,
    };
    let format = multiview_audio::AudioFormat::new(cfg.sample_rate, layout);
    multiview_audio::LoudnormProcessor::new(format, multiview_audio::LoudnessTarget::Streaming).ok()
}

/// Drive the program-audio bus to the output **tick index** of one surviving
/// [`StreamItem`] and return that frame's audio block (RT-8b, the lip-sync fix).
///
/// The bake consumer only receives the frames that SURVIVED the `DropOnOverload`
/// shed, so it must drive the bus by the absolute output tick index — not once per
/// surviving frame — or the audio `SampleClock` would fall behind video by exactly
/// the dropped ticks' samples (a per-surviving-frame `bus.tick()` is the drift bug
/// RT-8a fixed in the audio crate and RT-8b closes here). The output clock stamps
/// `out_pts = f(tick)` and the engine carries that 0-based index on every
/// `StreamItem`, so advancing the bus to `tick_index + 1` emits exactly the samples
/// owed for output ticks `0..=tick_index` — catching up across any gap. A
/// duplicated or out-of-order index is a monotonic no-op in
/// [`SampleClock`](multiview_audio::cadence::SampleClock) (it never rewinds), so a
/// stale item can never make the bus emit rewound audio (invariant #3).
fn drive_audio_for_item(
    bus: &mut multiview_audio::program::ProgramBus,
    tick_index: u64,
) -> multiview_audio::format::AudioBlock {
    // `tick_index` is 0-based; advancing the clock to `tick_index + 1` yields the
    // samples for output ticks `0..=tick_index`. `saturating_add` guards the
    // (unreachable) `u64::MAX` tick so the catch-up arithmetic never wraps.
    bus.tick_to(tick_index.saturating_add(1))
}

/// The single bake-consumer thread body (encode-once-mux-many, invariant #7):
/// build the [`StreamBaker`] from the owned [`BakeContext`] and own the single
/// [`ProgramEncoder`], then loop receiving [`StreamItem`]s — bake each into an
/// `Arc<Nv12Image>`, encode it ONCE, and fan each produced [`EncodedPacket`] (an
/// independently-owned copy) to every sink's bounded channel (blocking send —
/// off the hot path). On end-of-program (the hot sender drops) the loop ends, the
/// encoder is flushed (its trailing packets fanned), and the sink senders drop —
/// so each sink sees its channel close and finalises (writes its trailer).
///
/// A bake or encode error stops the consumer (the sink senders still drop on
/// return, so the sinks finalise what they have); a sink whose receiver has hung
/// up is simply skipped for the rest of the run. The encoder runs on THIS
/// off-hot-path thread: it can neither stall the output clock (the engine already
/// handed the frame off — invariant #1) nor be back-pressured by a slow sink
/// (invariant #10).
///
/// # Errors
/// Returns [`PipelineError`] if building the baker, baking a frame, or the single
/// encode fails.
fn consumer_main(
    ctx: BakeContext,
    hot_rx: &Receiver<StreamItem>,
    sink_txs: Vec<SyncSender<EncodedPacket>>,
    mut encoder: ProgramEncoder,
    mut audio_bus: Option<multiview_audio::program::ProgramBus>,
    mut audio_loudnorm: Option<multiview_audio::LoudnormProcessor>,
    in_flight: &AtomicI64,
) -> Result<(), PipelineError> {
    let mut baker = StreamBaker::new(ctx)?;
    // Track which sinks are still live (their receiver has not hung up) so a sink
    // that ended early does not wedge or repeatedly error the consumer.
    let mut live: Vec<bool> = vec![true; sink_txs.len()];
    // `recv()` returns `Err` only when the hot sender drops = end-of-program
    // (clean stop / EOF), which ends the loop.
    while let Ok(item) = hot_rx.recv() {
        // One frame left the queue.
        in_flight.fetch_sub(1, Ordering::AcqRel);
        let overlaid = baker.bake(&item)?;
        // Bridge once + encode once (invariant #7): the canvas is encoded a SINGLE
        // time here, never once per sink.
        let frame = nv12_to_decoded(&overlaid)?;
        let packets = encoder
            .encode_frame(frame)
            .map_err(|e| PipelineError::Output {
                kind: "encode",
                reason: e.to_string(),
            })?;
        fan_packets(&sink_txs, &mut live, packets);
        // Program audio (AUD-4 + RT-8b), OFF the engine hot loop: when the bus is
        // present, mix program audio up to THIS frame's output tick index — catching
        // up the samples for any ticks `DropOnOverload` shed since the last surviving
        // frame — and encode it into AAC packets, fanned to the SAME sinks as the
        // video. Driving by the tick index (not once per surviving frame) keeps the
        // audio `SampleClock` a pure function of the tick counter, so audio stays
        // lip-synced to video even under sustained encoder overload (invariant #3).
        // The pull does no I/O and cannot block (inv #1/#10); the encode runs here on
        // the bake consumer, never on the output-clock loop. `None` (audio off) skips
        // this entirely — the run is video-only as before.
        if let Some(bus) = audio_bus.as_mut() {
            let block = drive_audio_for_item(bus, item.tick_index);
            // EBU R128 loudness normalisation of the PROGRAM bus (AUD-6), still off
            // the engine hot loop (this bake-consumer thread): a smoothed makeup
            // gain toward the target LUFS with a true-peak limiter clamping to the
            // -1.5 dBTP ceiling so normalisation never clips. The block shape is
            // preserved exactly, so the AAC encode sees the same frame count it
            // would have. `None` (no normaliser) emits the mixed bus unaltered.
            // Discrete tracks never pass through here (program-bus-only — ADR-R006).
            let block = match audio_loudnorm.as_mut() {
                Some(norm) => norm.process(block),
                None => block,
            };
            let audio_packets = encoder
                .encode_audio_interleaved(block.interleaved(), block.frame_count())
                .map_err(|e| PipelineError::Output {
                    kind: "encode-audio",
                    reason: e.to_string(),
                })?;
            fan_packets(&sink_txs, &mut live, audio_packets);
        }
    }
    // End-of-program: flush the encoder and fan its trailing packets, then drop
    // the sink senders so each sink sees end-of-program and finalises.
    let tail = encoder.finish().map_err(|e| PipelineError::Output {
        kind: "encode",
        reason: e.to_string(),
    })?;
    fan_packets(&sink_txs, &mut live, tail);
    drop(sink_txs);
    Ok(())
}

/// Fan each coded packet to every still-live sink as an independently-owned copy
/// (so each muxer's in-place stream-index set + rescale is sound even though the
/// same packet feeds many muxers — invariant #7). The copy **preserves the
/// packet's [`StreamKind`]** (cloning the [`EncodedPacket`] wrapper, not
/// re-wrapping the raw packet) so an audio packet stays tagged audio and routes
/// to the muxer's audio stream — AUD-4; a video-default re-wrap mis-routed audio
/// onto the video stream, corrupting its DTS. A blocking send paces the CONSUMER
/// to the slowest sink, never the engine (the engine already handed the frame
/// off); a hung-up receiver (the sink ended) marks that sink dead for the rest
/// of the run.
fn fan_packets(
    sink_txs: &[SyncSender<EncodedPacket>],
    live: &mut [bool],
    packets: Vec<EncodedPacket>,
) {
    for packet in packets {
        for (i, tx) in sink_txs.iter().enumerate() {
            if !live.get(i).copied().unwrap_or(false) {
                continue;
            }
            // Bounded send: a healthy sink drains within the grace, so this
            // succeeds (near-)instantly; a sink that has hung up OR has wedged and
            // stopped draining for `SINK_WEDGE_GRACE` marks the sink dead for the
            // rest of the run, so a wedged sink can never stall the consumer
            // forever (ENG-1, inv #1).
            // `packet.clone()` is a ref-counted, independently-writable copy that
            // KEEPS the kind tag (unlike `from_packet`, which forces video).
            if !send_bounded(tx, packet.clone()) {
                if let Some(flag) = live.get_mut(i) {
                    *flag = false;
                }
            }
        }
    }
}

/// Send `packet` on `tx`, waiting at most [`SINK_WEDGE_GRACE`] for a slot. Returns
/// `true` once enqueued; `false` if the receiver hung up (`Disconnected`) or the
/// sink stopped draining for the whole grace (wedged) — the caller then marks the
/// sink dead. `std`'s `SyncSender` has no stable `send_timeout`, so this polls a
/// non-blocking `try_send` with a short sleep (off the hot path; a healthy sink
/// takes the first `try_send`, never the sleep).
fn send_bounded(tx: &SyncSender<EncodedPacket>, packet: EncodedPacket) -> bool {
    let deadline = Instant::now() + SINK_WEDGE_GRACE;
    let mut packet = packet;
    loop {
        match tx.try_send(packet) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                packet = returned;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Drive one runnable output to completion over its bounded fan-out channel,
/// reusing the existing, tested `multiview_output` sink `run()` VERBATIM over a
/// [`StreamingFrameSource`] (ADR-0025). The HLS playlist text is returned for the
/// egress to write once the encode completes (so the on-disk playlist is written
/// off the sink thread, keeping the sink path identical to the batch one).
///
/// # Errors
/// Returns [`PipelineError::Output`] if a **file or HLS** sink's encode/mux fails
/// (those write a local artifact a failed run must surface). A **push** sink whose
/// peer is unreachable, by contrast, never fails the run: it is reported and
/// dropped so the program's local outputs still complete (invariants #1/#10 — a
/// dead remote consumer must not back-pressure or fail the program).
fn run_one_output(
    output: RunnableOutput,
    rx: Receiver<EncodedPacket>,
    params: &StreamCodecParameters,
    time_base: Rational,
    audio: Option<&(StreamCodecParameters, Rational)>,
) -> Result<SinkRunOutcome, PipelineError> {
    match output {
        RunnableOutput::File { sink, path } => {
            let mut source = StreamingPacketSource::new(rx);
            let audio_mux = audio.map(|(p, tb)| multiview_output::MuxStream::new(p, *tb));
            let outcome = sink
                .run_av(
                    &mut source,
                    multiview_output::MuxStream::new(params, time_base),
                    audio_mux,
                )
                .map_err(|e| PipelineError::Output {
                    kind: "file",
                    reason: e.to_string(),
                })?;
            let (packets, keyframes) = match &outcome {
                PacketMuxOutcome::Single(s) => (s.packets, s.keyframes),
                PacketMuxOutcome::Segment(s) => (s.stats.packets, s.stats.keyframes),
            };
            Ok(SinkRunOutcome {
                line: format!(
                    "file {}: {packets} packet(s), {keyframes} keyframe(s)",
                    path.display()
                ),
                playlist: None,
                frames: source.delivered,
            })
        }
        RunnableOutput::Hls {
            sink,
            playlist_path,
        } => {
            let mut source = StreamingPacketSource::new(rx);
            let audio_mux = audio.map(|(p, tb)| multiview_output::MuxStream::new(p, *tb));
            let outcome = sink
                .run_av(
                    &mut source,
                    multiview_output::MuxStream::new(params, time_base),
                    audio_mux,
                )
                .map_err(|e| PipelineError::Output {
                    kind: "hls",
                    reason: e.to_string(),
                })?;
            match outcome {
                PacketMuxOutcome::Segment(result) => {
                    // The live sink already published (and finalized, with
                    // EXT-X-ENDLIST) the on-disk `.m3u8` atomically on each closed
                    // segment — the `LivePlaylist` owns that file. Do NOT write it
                    // again here (`playlist: None`), or we would race/clobber the
                    // sink's atomic publish with a stale second write (HLS-0/1).
                    Ok(SinkRunOutcome {
                        line: format!(
                            "hls {} + {} segment(s) ({} packet(s))",
                            playlist_path.display(),
                            result.segments.len(),
                            result.stats.packets
                        ),
                        playlist: None,
                        frames: source.delivered,
                    })
                }
                // A segment sink always yields `Segment`; tolerate the otherwise
                // unreachable single-container shape without a playlist rather
                // than panicking on the hot teardown path.
                PacketMuxOutcome::Single(stats) => Ok(SinkRunOutcome {
                    line: format!(
                        "hls {}: {} packet(s) (no segments)",
                        playlist_path.display(),
                        stats.packets
                    ),
                    playlist: None,
                    frames: source.delivered,
                }),
            }
        }
        RunnableOutput::Push { sink, label, url } => Ok(run_push_output(
            &sink, label, &url, rx, params, time_base, audio,
        )),
    }
}

/// Run a live push sink (RTMP / SRT) over its fan-out channel, tolerating an
/// unreachable peer. **Infallible** by design (returns a [`SinkRunOutcome`], never
/// an error): a push that cannot be delivered must not fail the program.
///
/// [`PushSink::run`] connects when it opens the muxer; with a reachable peer it
/// streams the encoded program (the same encode-once-mux-many drive loop the file
/// sink uses) and reports the packet/keyframe counts. A connect failure (no peer)
/// is **logged and the sink dropped** — never an error that fails the run: the
/// caller drops the [`Receiver`] when this returns, so the bake consumer's next
/// fan-out send to this sink fails and it marks the sink dead for the rest of the
/// run, while the program's file/HLS outputs keep producing (invariants #1/#10). A
/// push that connected and then errored mid-stream is reported the same tolerant
/// way.
fn run_push_output(
    sink: &PacketMuxSink,
    label: &'static str,
    url: &str,
    rx: Receiver<EncodedPacket>,
    params: &StreamCodecParameters,
    time_base: Rational,
    audio: Option<&(StreamCodecParameters, Rational)>,
) -> SinkRunOutcome {
    let mut source = StreamingPacketSource::new(rx);
    let audio_mux = audio.map(|(p, tb)| multiview_output::MuxStream::new(p, *tb));
    match sink.run_av(
        &mut source,
        multiview_output::MuxStream::new(params, time_base),
        audio_mux,
    ) {
        Ok(outcome) => {
            let (packets, keyframes) = match &outcome {
                PacketMuxOutcome::Single(s) => (s.packets, s.keyframes),
                PacketMuxOutcome::Segment(s) => (s.stats.packets, s.stats.keyframes),
            };
            SinkRunOutcome {
                line: format!("{label} push {url}: {packets} packet(s), {keyframes} keyframe(s)"),
                playlist: None,
                frames: source.delivered,
            }
        }
        Err(e) => {
            // A push peer that is unreachable / drops must never fail the program.
            tracing::warn!(
                transport = label,
                url,
                error = %e,
                "live push could not be delivered (peer unreachable or dropped); \
                 the program's other outputs are unaffected"
            );
            SinkRunOutcome {
                line: format!("{label} push {url}: not delivered ({e})"),
                playlist: None,
                frames: source.delivered,
            }
        }
    }
}

/// Sample every per-source caption cue store at `pts`, returning the active
/// **bitmap** cue per source (`source_id -> CueBitmap`). The bitmap (DVB-sub)
/// sibling of the per-layer [`SubtitleRouter`](crate::captions::SubtitleRouter)
/// text sampling (RT-10b's `CueSource` primitive is text-only), for the
/// [`CaptionCue::Bitmap`] shape; a source with no active bitmap cue at `pts` is
/// omitted. Called per tick on the hot loop — a pure lock-free read that can
/// neither pace nor stall the engine (invariants #1/#10).
#[cfg(feature = "overlay")]
fn sample_caption_bitmaps(
    stores: &std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    pts: MediaTime,
) -> std::collections::HashMap<String, multiview_ffmpeg::caption::CueBitmap> {
    let mut out = std::collections::HashMap::new();
    for (id, store) in stores {
        if let Some(multiview_ffmpeg::caption::CaptionCue::Bitmap { bitmap, .. }) =
            store.active_at(pts)
        {
            out.insert(id.clone(), bitmap);
        }
    }
    out
}

/// An operator-declared probe override for one source, resolved from
/// `config.probes` (M10): the config [`Probe`](multiview_config::probe::Probe) the
/// engine builds a config-derived analyser + X.733 lifecycle from, per fault
/// class. A `None` entry means "no declared probe for this class — use the
/// hardcoded default".
///
/// The first declared probe of each kind for a source wins (duplicate probe ids
/// are already rejected by config validation; two probes of the same kind on one
/// source is an unusual config and deterministically resolves to the first).
#[cfg(feature = "overlay")]
#[derive(Default, Clone)]
struct SourceProbeOverride {
    /// The declared black probe (its `luma_threshold` + `zone` + dwell + severity).
    black: Option<multiview_config::probe::Probe>,
    /// The declared freeze probe (its `difference_threshold` + `zone` + dwell + …).
    freeze: Option<multiview_config::probe::Probe>,
    /// The declared silence probe (its dwell + severity, and its `level_dbfs`,
    /// which becomes this source's silence detection floor in place of the shared
    /// [`SILENCE_FLOOR_DB`]).
    silence: Option<multiview_config::probe::Probe>,
}

/// Resolve `config.probes` into per-**source** overrides by mapping each probe's
/// watched **cell** to its bound source via the solved `layout` (M10).
///
/// A probe whose cell is unbound (no `source`) or absent from the layout is
/// skipped — there is no picture/audio to analyse. Runs once at run start, off the
/// hot loop.
#[cfg(feature = "overlay")]
fn resolve_source_probes(
    layout: &Layout,
    probes: &[multiview_config::probe::Probe],
) -> std::collections::HashMap<String, SourceProbeOverride> {
    use multiview_config::probe::ProbeKind;
    let mut out: std::collections::HashMap<String, SourceProbeOverride> =
        std::collections::HashMap::new();
    for probe in probes {
        // The config carries cell ids only on the raw cells, not the solved
        // `Layout`; the layout cell binds a `source`, and a probe's `cell` field
        // names a config cell id. The desugared run binds cell id == source id for
        // the common single-source-per-cell case, so resolve by matching the
        // probe's cell against a layout cell whose bound source equals it, falling
        // back to treating the probe's `cell` as the source id directly when no
        // distinct cell id is carried. Either way an unbound name is skipped.
        let source = layout
            .cells
            .iter()
            .filter_map(|c| c.source.clone())
            .find(|s| s == &probe.cell)
            .unwrap_or_else(|| probe.cell.clone());
        let entry = out.entry(source).or_default();
        match probe.kind {
            ProbeKind::Black { .. } if entry.black.is_none() => {
                entry.black = Some(probe.clone());
            }
            ProbeKind::Freeze { .. } if entry.freeze.is_none() => {
                entry.freeze = Some(probe.clone());
            }
            ProbeKind::Silence { .. } if entry.silence.is_none() => {
                entry.silence = Some(probe.clone());
            }
            // A loudness probe (or a duplicate of an already-captured kind, or a
            // future kind) does not override one of the three content-fault badge
            // classes; the loudness alarm rides the meter path elsewhere.
            _ => {}
        }
    }
    out
}

/// The dBFS floor at/below which the per-input meter is treated as **silent**
/// for the audio-loss fault. Just above the meter's true floor so a genuinely
/// quiet-but-present programme does not trip it; sustained past the silence
/// dwell before the `NO AUDIO` badge raises (anti-flap). A source with no
/// build-time meter timeline rides [`multiview_audio::Ballistics::FLOOR_DB`], which
/// is below this floor, so an audio-free tile reads silent (intended).
#[cfg(feature = "overlay")]
const SILENCE_FLOOR_DB: f64 = -50.0;

/// Samples each tile's last-good frame + per-input loudness once per output tick
/// and classifies a per-tile **content fault** (black / frozen / silent),
/// distinct from the lifecycle [`SourceState`](multiview_core::traits::SourceState).
///
/// It shares the SAME lock-free per-source [`TileStore`]s the engine samples (by
/// `Arc`), so it never copies the picture and never blocks: a [`TileStore::read_at`]
/// is a wait-free atomic snapshot. Black/freeze come from the stateless engine
/// probes ([`multiview_engine::BlackProbe`]/[`multiview_engine::FreezeProbe`]) run over
/// a borrowed [`multiview_engine::LumaView`] of the sampled frame's tightly-packed
/// luma plane; freeze compares the current sample to the *previous* sampled frame
/// (held by `Arc`, no copy). Silence comes from the build-time per-source meter
/// timeline. Each instantaneous condition is folded through a per-source
/// [`multiview_engine::AlarmStateMachine`] so the badge dwells/hysteresis rather than
/// flapping. Any probe/geometry error logs and yields *no fault* (fail-safe):
/// fault detection must never break the output clock (inv #1) or the engine (#10).
///
/// ## Freeze on a REAL encoded source (not just byte-identical frames)
///
/// A genuinely frozen feed delivered as compressed video is **not** byte-identical
/// frame-to-frame: at every GOP boundary the codec re-quantizes the (unchanging)
/// picture, perturbing a small fraction of luma samples. Instrumenting the demo's
/// `frozen.ts` (mpeg2, ~1 s GOP) through this exact decode→scale→NV12 path showed
/// the per-frame changed-fraction sits at `0.0` for most frames but **spikes to
/// 1.2 %–4.6 % once per GOP** at the default tolerance (`diff_tolerance = 2`). The
/// engine [`FreezeConfig::default`] threshold is `0.1 %`, so each GOP spike reads
/// "not frozen" and, fed straight to the 2 s dwell machine, *resets the dwell*
/// every ~1 s — the freeze alarm never sustains long enough to raise.
///
/// We must not change the shared engine probe defaults (the alarm system depends
/// on them), so the CLI configures **its** freeze probe for real-world codec
/// noise (see [`Self::new`]): raising `diff_tolerance` to `6` collapses the GOP
/// spikes (the same source then sees max `1.27 %`, with `148 / 150` frames at or
/// below the engine-default `0.1 %` change threshold, which is therefore KEPT —
/// it cleanly separates a frozen feed from a moving-but-silent one, which stays
/// `> 0.1 %` on `146 / 150` frames). On top of that, the instantaneous freeze
/// condition is **debounced** over a short sliding window
/// ([`FREEZE_DEBOUNCE_WINDOW`]/[`FREEZE_DEBOUNCE_MIN_PRESENT`]) before it reaches
/// the dwell machine, so the *one or two* residual per-GOP noisy frames (and the
/// first decoded frame) cannot reset an otherwise-sustained freeze. Black and
/// silence are unaffected.
#[cfg(feature = "overlay")]
struct FaultDetector {
    /// The per-source last-good stores, shared with the engine drive loop.
    stores: HashMapStores,
    /// Build-time per-source per-tick loudness timelines (dBFS) for silence.
    meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
    /// Per-source dwell/hysteresis state machines for each fault class.
    machines: std::collections::HashMap<String, SourceFaultMachines>,
    /// The stateless black probe (default broadcast threshold).
    black: multiview_engine::BlackProbe,
    /// The stateless freeze probe, tuned for real encoded sources (see
    /// [`Self::new`]): wider per-sample tolerance + change threshold than the
    /// engine default so GOP re-quantization noise does not read as motion.
    freeze: multiview_engine::FreezeProbe,
    /// The previous sampled frame per source, for the freeze comparison (held by
    /// `Arc`, so caching it copies no pixels).
    previous: std::collections::HashMap<String, Arc<Nv12Image>>,
    /// Per-source sliding window of the most recent instantaneous freeze
    /// conditions (newest pushed back, oldest popped front), used to debounce a
    /// single noisy frame so it cannot reset the freeze dwell.
    freeze_window: std::collections::HashMap<String, std::collections::VecDeque<bool>>,
    /// Dwell-up/dwell-down windows (derived from the cadence) per fault class.
    hysteresis_black: multiview_engine::AlarmHysteresis,
    hysteresis_freeze: multiview_engine::AlarmHysteresis,
    hysteresis_silence: multiview_engine::AlarmHysteresis,
    /// Operator-declared probe overrides, keyed by source id (M10). When a source
    /// has a declared probe for a fault class, that class's analyser comes from the
    /// config probe via the engine's
    /// [`black_config_from_kind`](multiview_engine::black_config_from_kind) /
    /// [`freeze_config_from_kind`](multiview_engine::freeze_config_from_kind)
    /// mappers, and its X.733 lifecycle (dwell/severity/latch/scope) via
    /// [`AlarmStateMachine::from_probe`](multiview_engine::AlarmStateMachine::from_probe),
    /// instead of the hardcoded default — the config→analyser → alarm seam. Empty
    /// (the default) keeps the prior behaviour byte-for-byte.
    source_probes: std::collections::HashMap<String, SourceProbeOverride>,
    /// Per-source config-derived **black** analyser, built once from the declared
    /// probe's `luma_threshold` + `zone`. Present only for sources with a declared
    /// black probe; others fall back to the shared default [`Self::black`].
    declared_black: std::collections::HashMap<String, multiview_engine::BlackProbe>,
    /// Per-source config-derived **freeze** analyser (declared `difference_threshold`
    /// + `zone`). Present only for sources with a declared freeze probe.
    declared_freeze: std::collections::HashMap<String, multiview_engine::FreezeProbe>,
    /// Per-source config-derived **silence floor** in dBFS, built once from a
    /// declared silence probe's `level_dbfs`. Present only for sources with a
    /// declared silence probe; others fall back to the shared [`SILENCE_FLOOR_DB`].
    declared_silence_floor: std::collections::HashMap<String, f64>,
}

/// The number of recent sampled frames over which the instantaneous freeze
/// condition is debounced before it reaches the dwell machine (~0.5 s at 25 fps).
/// Sized so it spans more than one GOP boundary, so an isolated per-GOP noise
/// spike is outvoted by the surrounding frozen frames.
#[cfg(feature = "overlay")]
const FREEZE_DEBOUNCE_WINDOW: usize = 12;

/// How many of the [`FREEZE_DEBOUNCE_WINDOW`] most-recent frames must read frozen
/// for the debounced freeze condition to be "present". At `9 / 12` (75 %) a single
/// (or even a couple of) noisy GOP-boundary frame(s) inside the window cannot
/// flip the condition to absent and reset the dwell, while a genuinely moving
/// picture (mostly-changed frames) stays well below the bar.
#[cfg(feature = "overlay")]
const FREEZE_DEBOUNCE_MIN_PRESENT: usize = 9;

/// The three per-source dwell state machines (black / freeze / silence).
#[cfg(feature = "overlay")]
struct SourceFaultMachines {
    black: multiview_engine::AlarmStateMachine,
    freeze: multiview_engine::AlarmStateMachine,
    silence: multiview_engine::AlarmStateMachine,
}

#[cfg(feature = "overlay")]
impl FaultDetector {
    /// Build a detector over the shared `stores` + build-time meter `timelines`.
    /// The dwell windows are absolute media durations (cadence-agnostic), so the
    /// output `cadence` is taken only to make the timeline these dwells run on
    /// explicit at the call site.
    fn new(
        stores: HashMapStores,
        meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
        _cadence: Rational,
        source_probes: std::collections::HashMap<String, SourceProbeOverride>,
    ) -> Self {
        use multiview_engine::{
            black_config_from_kind, freeze_config_from_kind, AlarmHysteresis, BlackConfig,
            BlackProbe, FreezeConfig, FreezeProbe,
        };
        // Dwell windows on the media timeline. Black/silence raise after ~0.5 s of
        // the condition and clear after ~0.3 s of its absence; freeze needs a
        // longer ~2 s of identical frames so a brief genuine still does not trip
        // it. These give the anti-flap hysteresis without coupling to wall-clock.
        let dwell = |secs_num: i64, secs_den: i64| -> MediaTime {
            MediaTime::from_nanos(secs_num.saturating_mul(1_000_000_000) / secs_den.max(1))
        };
        let down = dwell(3, 10); // 0.3 s
                                 // The CLI freeze probe widens only the PER-SAMPLE tolerance vs the engine
                                 // default — the root cause of the missed badge was tolerance, not the
                                 // change threshold. Instrumented across the three real demo sources scaled
                                 // to a 636x356 tile, the changed-fraction with diff_tolerance=6 was:
                                 //   frozen:  median 0.000%, max 1.27%, 148/150 frames <= 0.1%
                                 //   silent:  median 0.229%, max 1.91%,   4/150 frames <= 0.1%  (moving)
                                 //   healthy: median 3.13%,  max 71%,      5/150 frames <= 0.1%
                                 // diff_tolerance=6 collapses GOP re-quantization noise (the frozen source's
                                 // per-GOP spike drops from 1.2-4.6% at the default tolerance 2 to <= 1.27%,
                                 // its steady frames to 0.000%); the engine-default change_threshold (0.1%)
                                 // is KEPT because at tolerance 6 it already separates a frozen feed (148/150
                                 // below) from a moving-but-silent feed (only 4/150 below). An earlier 0.5%
                                 // threshold over-loosened this and wrongly flagged the moving silent tile.
                                 // The two residual per-GOP spikes the frozen source still shows are absorbed
                                 // by the debounce window below, not by a looser threshold.
        let freeze_cfg = FreezeConfig::default().with_tolerance(6);
        // Build the per-source config-derived analysers ONCE from any declared
        // black/freeze probe (its operator-authored threshold + zone). A declared
        // freeze probe is honoured verbatim — the CLI's codec-noise tolerance/
        // debounce defaults still apply to it via the shared sample path, but its
        // change threshold + zone are the operator's. Sources without a declared
        // probe never appear here and fall back to the shared defaults.
        let mut declared_black = std::collections::HashMap::new();
        let mut declared_freeze = std::collections::HashMap::new();
        let mut declared_silence_floor = std::collections::HashMap::new();
        for (source, ov) in &source_probes {
            if let Some(cfg) = ov
                .black
                .as_ref()
                .and_then(|p| black_config_from_kind(&p.kind))
            {
                declared_black.insert(source.clone(), BlackProbe::new(cfg));
            }
            if let Some(cfg) = ov
                .freeze
                .as_ref()
                .and_then(|p| freeze_config_from_kind(&p.kind))
            {
                // Preserve the CLI's wider per-sample tolerance for real encoded
                // sources; only the operator-authored change threshold + zone come
                // from config (the engine `from_kind` keeps the default tolerance,
                // which we override to the CLI's 6 to match the default path).
                declared_freeze.insert(source.clone(), FreezeProbe::new(cfg.with_tolerance(6)));
            }
            // A declared silence probe carries the operator-authored level ceiling
            // (`level_dbfs`); thread it into this source's detection floor so the
            // instantaneous silence condition uses the operator's threshold rather
            // than the shared default. Widen `f32 -> f64` exactly (`f64::from`, no
            // `as` cast) to match the meter timeline scale.
            if let Some(multiview_config::probe::ProbeKind::Silence { level_dbfs }) =
                ov.silence.as_ref().map(|p| p.kind)
            {
                declared_silence_floor.insert(source.clone(), f64::from(level_dbfs));
            }
        }
        Self {
            stores,
            meter_db_timelines,
            machines: std::collections::HashMap::new(),
            black: BlackProbe::new(BlackConfig::default()),
            freeze: FreezeProbe::new(freeze_cfg),
            previous: std::collections::HashMap::new(),
            freeze_window: std::collections::HashMap::new(),
            hysteresis_black: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
            hysteresis_freeze: AlarmHysteresis::new(dwell(2, 1), down), // 2 s up
            hysteresis_silence: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
            source_probes,
            declared_black,
            declared_freeze,
            declared_silence_floor,
        }
    }

    /// Get-or-create the dwell machines for `id` (one per fault class).
    ///
    /// When the source has an operator-declared probe for a fault class (M10), that
    /// class's machine is built from the config probe via
    /// [`AlarmStateMachine::from_probe`](multiview_engine::AlarmStateMachine::from_probe)
    /// — honouring the declared dwell, severity, latch and scope. Classes with no
    /// declared probe use the hardcoded default machine, so an undeclared source is
    /// byte-for-byte the prior behaviour.
    fn machines_for(&mut self, id: &str) -> &mut SourceFaultMachines {
        use multiview_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
        use multiview_engine::{AlarmHysteresis, AlarmStateMachine};
        let hb = self.hysteresis_black;
        let hf = self.hysteresis_freeze;
        let hs = self.hysteresis_silence;
        let ov = self.source_probes.get(id).cloned().unwrap_or_default();
        self.machines.entry(id.to_owned()).or_insert_with(|| {
            let mk = |kind: AlarmKind, sev: PerceivedSeverity, hyst: AlarmHysteresis| {
                AlarmStateMachine::new(
                    AlarmId::new(format!("{id}:{kind:?}")),
                    kind,
                    AlarmScope::Probe { id: id.to_owned() },
                    sev,
                    hyst,
                )
            };
            // A declared probe's full X.733 lifecycle (id/scope/severity/dwell/
            // latch) via `from_probe`; otherwise the hardcoded default machine.
            let from_or_default = |probe: &Option<multiview_config::probe::Probe>,
                                   kind: AlarmKind,
                                   sev: PerceivedSeverity,
                                   hyst: AlarmHysteresis| {
                match probe {
                    Some(p) => AlarmStateMachine::from_probe(p),
                    None => mk(kind, sev, hyst),
                }
            };
            SourceFaultMachines {
                black: from_or_default(&ov.black, AlarmKind::Black, PerceivedSeverity::Major, hb),
                freeze: from_or_default(
                    &ov.freeze,
                    AlarmKind::Freeze,
                    PerceivedSeverity::Major,
                    hf,
                ),
                silence: from_or_default(
                    &ov.silence,
                    AlarmKind::Silence,
                    PerceivedSeverity::Minor,
                    hs,
                ),
            }
        })
    }

    /// Sample + classify every cell-bound source's content fault for the output
    /// instant `pts` (tick `index`), returning the active fault per source.
    ///
    /// `source_states` names the cell-bound sources this tick. For each, this
    /// reads its last-good frame (lock-free), runs the black + freeze probes over
    /// its luma, reads its silence condition from the build-time meter timeline,
    /// folds each through the per-source dwell machine, and returns the
    /// highest-precedence active fault (black > freeze > silence). A source with
    /// no usable frame (`NoSignal`) contributes no content fault (its lifecycle
    /// badge already conveys the loss). Errors are logged and treated as no-fault.
    fn sample(
        &mut self,
        pts: MediaTime,
        index: u64,
        source_states: &std::collections::HashMap<String, multiview_core::traits::SourceState>,
    ) -> std::collections::HashMap<String, crate::overlays::TileFault> {
        use crate::overlays::TileFault;
        let mut out = std::collections::HashMap::new();
        // Snapshot the source ids this tick (sorted for deterministic logging).
        let mut ids: Vec<String> = source_states.keys().cloned().collect();
        ids.sort_unstable();
        for id in ids {
            // Sample this tile's last-good frame (lock-free; never blocks).
            let frame = self
                .stores
                .get(&id)
                .and_then(|store| store.read_at(pts).frame().map(Arc::clone));

            // Instantaneous black / freeze conditions from the sampled luma.
            let (black_now, freeze_now) = match &frame {
                Some(img) => self.picture_conditions(&id, img),
                None => (false, false),
            };
            // Update the previous-frame cache for the next freeze comparison.
            // On a missing frame (NoSignal) drop both the previous frame AND the
            // debounce window so a recovered source restarts freeze cleanly rather
            // than inheriting stale frozen votes across the gap.
            if let Some(img) = &frame {
                self.previous.insert(id.clone(), Arc::clone(img));
            } else {
                self.previous.remove(&id);
                self.freeze_window.remove(&id);
            }
            // Instantaneous silence from the per-input meter timeline.
            let silence_now = self.silence_now(&id, index);

            // Debounce the freeze condition over a short window so a single noisy
            // GOP-boundary / warm-up frame cannot reset the 2 s freeze dwell.
            let freeze_debounced = self.debounce_freeze(&id, freeze_now);

            // Fold each condition through its per-source dwell machine.
            let machines = self.machines_for(&id);
            machines.black.observe(black_now, pts);
            machines.freeze.observe(freeze_debounced, pts);
            machines.silence.observe(silence_now, pts);

            // Precedence: black > freeze > silence (a black picture is the most
            // specific/severe content fault; a still or silent tile is lesser).
            let fault = if machines.black.is_active() {
                TileFault::Black
            } else if machines.freeze.is_active() {
                TileFault::Frozen
            } else if machines.silence.is_active() {
                TileFault::Silent
            } else {
                TileFault::None
            };
            if fault.is_present() {
                out.insert(id, fault);
            }
        }
        out
    }

    /// The instantaneous (black, frozen) conditions for `id`'s sampled `frame`,
    /// over a borrowed luma view of its tightly-packed Y plane. Any geometry
    /// error logs and yields `(false, false)` (fail-safe to no fault).
    fn picture_conditions(&self, id: &str, frame: &Nv12Image) -> (bool, bool) {
        use multiview_engine::LumaView;
        // The Y plane is tightly packed (`stride == width`) per `Nv12Image`.
        let current = match LumaView::packed(frame.y_plane(), frame.width(), frame.height()) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(source = %id, error = %e, "fault probe: luma view build failed");
                return (false, false);
            }
        };
        // Prefer this source's config-derived analyser (the operator-declared
        // threshold + zone, M10); fall back to the shared default when no probe was
        // declared for it.
        let black_probe = self.declared_black.get(id).unwrap_or(&self.black);
        let freeze_probe = self.declared_freeze.get(id).unwrap_or(&self.freeze);
        let black = black_probe.detect(&current).condition_present;
        // Freeze needs the previous sampled frame; if none yet (first frame or a
        // gap), it is not frozen this tick (fail-safe toward "live").
        let frozen = match self.previous.get(id) {
            Some(prev) => match LumaView::packed(prev.y_plane(), prev.width(), prev.height()) {
                Ok(prev_view) => freeze_probe.detect(&current, &prev_view).condition_present,
                Err(_) => false,
            },
            None => false,
        };
        (black, frozen)
    }

    /// Push this tick's instantaneous freeze condition into `id`'s sliding window
    /// and return the **debounced** condition: frozen-now iff at least
    /// [`FREEZE_DEBOUNCE_MIN_PRESENT`] of the last [`FREEZE_DEBOUNCE_WINDOW`]
    /// frames read frozen.
    ///
    /// This is the anti-flap layer between the per-frame probe and the dwell
    /// machine: the engine's [`AlarmStateMachine`](multiview_engine::AlarmStateMachine)
    /// resets its raise dwell on a *single* absent sample (correct for its general
    /// use, and not ours to change), but a real frozen feed emits an occasional
    /// noisy frame at each GOP boundary. Requiring a strong majority of the recent
    /// window to be frozen lets those isolated spikes pass without resetting the
    /// dwell, while a genuinely moving picture (mostly-changed frames) never
    /// reaches the majority and so never debounces to frozen. Until the window
    /// has filled it reports the simple majority of what it has seen so far.
    fn debounce_freeze(&mut self, id: &str, freeze_now: bool) -> bool {
        let window = self.freeze_window.entry(id.to_owned()).or_default();
        window.push_back(freeze_now);
        while window.len() > FREEZE_DEBOUNCE_WINDOW {
            window.pop_front();
        }
        let present = window.iter().filter(|&&f| f).count();
        if window.len() >= FREEZE_DEBOUNCE_WINDOW {
            // Full window: require the strong majority threshold.
            present >= FREEZE_DEBOUNCE_MIN_PRESENT
        } else {
            // Warming up: a simple majority of the frames seen so far. A genuinely
            // frozen source reads frozen from frame 2 onward; a moving one does not.
            present.saturating_mul(2) > window.len()
        }
    }

    /// The instantaneous silence condition for `id` at tick `index`: the source's
    /// build-time meter reading is at/below its silence floor. The floor is the
    /// operator-declared `level_dbfs` when this source has a declared silence probe
    /// (M10), otherwise the shared default [`SILENCE_FLOOR_DB`]. A source with no
    /// meter timeline rides the meter floor (which is below either silence floor),
    /// so an audio-free tile reads silent.
    fn silence_now(&self, id: &str, index: u64) -> bool {
        let db = match self.meter_db_timelines.get(id) {
            Some(timeline) => {
                let i = usize::try_from(index).unwrap_or(usize::MAX);
                timeline
                    .get(i)
                    .copied()
                    .or_else(|| timeline.last().copied())
                    .unwrap_or(multiview_audio::Ballistics::FLOOR_DB)
            }
            None => multiview_audio::Ballistics::FLOOR_DB,
        };
        let floor = self
            .declared_silence_floor
            .get(id)
            .copied()
            .unwrap_or(SILENCE_FLOOR_DB);
        db <= floor
    }
}

/// Owns the per-source streaming-ingest decode threads and a shared stop flag.
///
/// Each thread decodes one source and publishes frames into its [`TileStore`]
/// (shared lock-free with the engine's drive loop). The engine only ever
/// *samples* those stores, so a slow, fast, or never-ending ingest thread can
/// neither pace nor stall the output clock (invariant #1) and cannot
/// back-pressure the engine (invariant #10). [`IngestSupervisor::shutdown`]
/// raises the stop flag and joins every thread, so a bounded run tears ingest
/// down deterministically rather than leaking threads.
struct IngestSupervisor {
    /// One (stop flag, join handle) per spawned thread. Per-thread flags
    /// (ADR-W018) let a live `RemoveSource` tear down exactly one startup
    /// producer via the shared stop registry; shutdown raises every flag.
    producers: Vec<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl IngestSupervisor {
    /// Spawn one decode thread per video plan, one **audio** decode thread per
    /// audio plan (AUD-2), and one caption reader thread per caption plan, then
    /// return the running supervisor.
    ///
    /// Each producer thread carries its **own** stop flag, registered by source
    /// id in the run's shared `registry` (ADR-W018) so a live `RemoveSource` can
    /// stop exactly that producer. The video decode thread registers under the
    /// source id `{id}`; its companion producers register under derived
    /// `{id}/<role>` keys — the caption reader under `{id}/captions`, the audio
    /// decode thread under `{id}/audio` (AUD-2), the line-up tone thread under
    /// `{id}/tone` (AUD-5) — so a live remove/edit of the source stops every one
    /// of them (the hub raises the `{id}` flag AND every `{id}/`-prefixed
    /// companion flag).
    ///
    /// Each is just another best-effort writer of a lock-free store (the tile
    /// store, the per-source [`AudioStore`](multiview_audio::store::AudioStore),
    /// or the cue store) — all are joined the same way, so none can pace or stall
    /// the output clock (invariant #1) nor back-pressure the engine (invariant
    /// #10). The audio thread is the peer of the video decode thread: it decodes
    /// the SAME source's audio (its own libav context) into the source's
    /// `AudioStore`, which the program bus samples.
    fn start(
        plans: Vec<IngestPlan>,
        audio_plans: Vec<(
            crate::audio::AudioIngestPlan,
            Arc<multiview_audio::store::AudioStore>,
        )>,
        tone_plans: Vec<(
            crate::audio::ToneIngestPlan,
            Arc<multiview_audio::store::AudioStore>,
        )>,
        caption_plans: Vec<crate::captions::CaptionPlan>,
        registry: &crate::live_sources::StopRegistry,
    ) -> Self {
        let mut producers = Vec::with_capacity(
            plans
                .len()
                .saturating_add(audio_plans.len())
                .saturating_add(tone_plans.len())
                .saturating_add(caption_plans.len()),
        );
        for plan in plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            crate::live_sources::register_stop(registry, &id, &stop);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-ingest-{id}"));
            match builder.spawn(move || ingest_loop(&plan, &thread_stop)) {
                Ok(handle) => producers.push((stop, handle)),
                Err(e) => {
                    // A thread that cannot spawn is logged and skipped: its tile
                    // simply rides NO_SIGNAL (slate) rather than failing the run
                    // (invariant #1 — the output clock is independent of inputs).
                    tracing::error!(error = %e, source = %id, "could not spawn ingest thread");
                }
            }
        }
        for (plan, store) in audio_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            // Registered under the derived `{id}/audio` key (ADR-W018): a live
            // remove/edit of the source raises every `{id}`-rooted flag, so its
            // audio decode thread stops too — never left mixing a stale source's
            // audio onto the program bus under the replacement.
            crate::live_sources::register_stop(registry, &format!("{id}/audio"), &stop);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-audio-{id}"));
            match builder
                .spawn(move || crate::audio::audio_ingest_loop(&plan, &store, &thread_stop))
            {
                Ok(handle) => producers.push((stop, handle)),
                Err(e) => {
                    // An audio thread that cannot spawn is logged and skipped: its
                    // source rides silence on the program bus (the store
                    // silence-fills) rather than failing the run — audio is
                    // best-effort and never gates the output clock (invariant #1).
                    tracing::error!(error = %e, source = %id, "could not spawn audio decode thread");
                }
            }
        }
        for (plan, store) in tone_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            // Registered under the derived `{id}/tone` key (ADR-W018): a live
            // remove/edit of the source raises every `{id}`-rooted flag, so its
            // line-up tone thread stops too — never left feeding the program bus
            // a stale `bars` source's tone under the replacement.
            crate::live_sources::register_stop(registry, &format!("{id}/tone"), &stop);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-tone-{id}"));
            match builder
                .spawn(move || crate::audio::tone_publish_loop(&plan, &store, &thread_stop))
            {
                Ok(handle) => producers.push((stop, handle)),
                Err(e) => {
                    // A tone thread that cannot spawn is logged and skipped: the
                    // `bars` source rides silence on the program bus (the store
                    // silence-fills) rather than failing the run — the line-up tone
                    // is best-effort and never gates the output clock (invariant #1).
                    tracing::error!(error = %e, source = %id, "could not spawn tone publish thread");
                }
            }
        }
        for plan in caption_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            // Registered under the derived `{id}/captions` key (ADR-W018): a
            // live remove/edit of the source raises every `{id}`-rooted flag,
            // so its caption reader stops too — never left decoding a stale
            // URL's cues over the replacement picture.
            crate::live_sources::register_stop(registry, &format!("{id}/captions"), &stop);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-captions-{id}"));
            match builder.spawn(move || crate::captions::caption_loop(&plan, &thread_stop)) {
                Ok(handle) => producers.push((stop, handle)),
                Err(e) => {
                    // A caption reader that cannot spawn is logged and skipped:
                    // its tile simply shows no caption (best-effort — invariant
                    // #1; captions never gate the output clock).
                    tracing::error!(error = %e, source = %id, "could not spawn caption reader thread");
                }
            }
        }
        Self { producers }
    }

    /// Signal every ingest thread to stop and join them.
    fn shutdown(mut self) {
        self.join_all();
    }

    /// Raise the stop flag, then join every outstanding ingest thread within a
    /// bounded grace period.
    ///
    /// A thread blocked inside a libav **network** call (`ffmpeg::format::input`
    /// opening a stalled live URL, or a `packet.read` on a wedged socket) cannot
    /// observe the cooperative `stop` flag — libav offers no portable cancel
    /// from a safe wrapper. Blocking forever on `join` would defeat the whole
    /// BUG-2 fix (the bounded run would hang on teardown), so a thread that does
    /// not finish within [`INGEST_JOIN_GRACE`] is **detached**: it only ever
    /// *writes* a lock-free store it shares by `Arc`, owns its own libav state
    /// (freed in `Drop`), and is reaped at process exit — it can neither corrupt
    /// the produced output nor stall the caller. This keeps the output-clock
    /// guarantee (invariant #1) intact end-to-end, including teardown.
    fn join_all(&mut self) {
        for (stop, _) in &self.producers {
            stop.store(true, Ordering::Release);
        }
        let deadline = Instant::now() + INGEST_JOIN_GRACE;
        for (_, handle) in self.producers.drain(..) {
            let name = handle.thread().name().unwrap_or("ingest").to_owned();
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                tracing::warn!(
                    thread = %name,
                    "ingest thread wedged in a blocking libav call; detaching (reaped at exit)"
                );
                continue; // detach: drop the handle without joining.
            }
            if handle.join().is_err() {
                tracing::error!(thread = %name, "ingest thread panicked during join");
            }
        }
    }
}

impl Drop for IngestSupervisor {
    fn drop(&mut self) {
        // Defensive teardown if `shutdown` was not called (e.g. an early return
        // on the encode/error path): raise the flag and join (bounded) so no
        // thread blocks the caller. After `shutdown` the handle vec is already
        // drained, so this is a no-op on that path.
        self.join_all();
    }
}

/// The outcome of the startup prime-wait: did every store prime, and how long
/// the wait actually took. Returned for diagnostics/logging and asserted by the
/// deterministic prime tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrimeOutcome {
    /// How many of `total` stores were primed when the wait returned.
    primed: usize,
    /// The total number of bound stores waited on.
    total: usize,
    /// The wall-time the wait actually spent (clock-measured).
    waited: Duration,
    /// `true` if the wait returned because every store primed; `false` if it
    /// returned because [`PRIME_WAIT_BUDGET`] elapsed first (a dead/slow source).
    all_primed: bool,
}

/// Wait — **bounded** — for every store in `stores` to publish its first frame,
/// so the output clock's very first tick samples primed tiles instead of the
/// cold last-good placeholder (the #40 startup-hold fix).
///
/// The wait is the heart of why this is safe for invariant #1/#2: it polls
/// `is_primed` every `poll` and returns the instant all stores are primed, but
/// it is hard-capped at `budget` measured by the injected `clock`. A source that
/// never produces a frame (the deliberately-missing source, a dead/wedged live
/// input) therefore can NOT block startup — once `budget` elapses the wait
/// returns with `all_primed = false` and the caller starts the clock anyway; the
/// unprimed tiles ride their `NO_SIGNAL` / last-good placeholder, which
/// [`TileStore::read_at`] already produces. It never paces the engine and never
/// touches the cadence — it only delays the first tick by at most `budget`.
///
/// `clock` + `sleep` are injected so the whole wait is deterministically
/// testable with no real sleeping: a test passes a [`ManualTimeSource`] and a
/// `sleep` closure that advances it, so the budget/poll behaviour is exercised
/// without wall-clock flakiness. Production passes a real monotonic clock and
/// `std::thread::sleep`.
fn wait_for_prime<S>(
    stores: &[&Arc<TileStore<Nv12Image>>],
    budget: Duration,
    poll: Duration,
    clock: &dyn TimeSource,
    mut sleep: S,
) -> PrimeOutcome
where
    S: FnMut(Duration),
{
    let total = stores.len();
    let count_primed = |stores: &[&Arc<TileStore<Nv12Image>>]| -> usize {
        stores.iter().filter(|s| s.is_primed()).count()
    };

    let start = clock.now_nanos();
    let budget_ns = i64::try_from(budget.as_nanos()).unwrap_or(i64::MAX);

    // No bound stores ⇒ nothing to prime; do not delay the clock at all.
    if total == 0 {
        return PrimeOutcome {
            primed: 0,
            total: 0,
            waited: Duration::ZERO,
            all_primed: true,
        };
    }

    loop {
        let primed = count_primed(stores);
        let elapsed_ns = clock.now_nanos().saturating_sub(start).max(0);
        if primed == total {
            // Every input primed: start the clock immediately (typically well
            // inside the budget).
            return PrimeOutcome {
                primed,
                total,
                waited: duration_from_nanos(elapsed_ns),
                all_primed: true,
            };
        }
        if elapsed_ns >= budget_ns {
            // Budget spent: a source has not primed and we must NOT wait on it
            // (invariant #1). Start the clock; its tile rides its placeholder.
            return PrimeOutcome {
                primed,
                total,
                waited: duration_from_nanos(elapsed_ns),
                all_primed: false,
            };
        }
        // Sleep at most the remaining budget so the last poll lands on the cap.
        let remaining = budget_ns.saturating_sub(elapsed_ns);
        let nap = poll.min(duration_from_nanos(remaining));
        sleep(nap);
    }
}

/// Build a [`Duration`] from a non-negative nanosecond count, saturating a
/// negative input to zero (the guardrails deny `as` casts; this stays lossless).
fn duration_from_nanos(nanos: i64) -> Duration {
    Duration::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(u64::MAX))
}

/// The pixel size of the cell that binds `source_id`, if any.
fn cell_pixel_size(layout: &Layout, source_id: &str) -> Option<(u32, u32)> {
    let cell = layout
        .cells
        .iter()
        .find(|c| c.source.as_deref() == Some(source_id))?;
    Some(cell_dims(cell, layout.canvas.width, layout.canvas.height))
}

/// Convert a cell's normalized `w`/`h` into even pixel dimensions (NV12 needs
/// even extents), clamped to at least 2x2.
fn cell_dims(cell: &Cell, canvas_w: u32, canvas_h: u32) -> (u32, u32) {
    let to_even_px = |frac: f32, extent: u32| -> u32 {
        let raw = frac_to_px(frac, extent);
        let even = raw & !1; // round down to even
        even.max(2)
    };
    (to_even_px(cell.w, canvas_w), to_even_px(cell.h, canvas_h))
}

/// Convert a normalized fraction in `[0,1]` to a pixel count in `[0, extent]`,
/// `as`-cast-free (the guardrails deny `as_conversions`).
fn frac_to_px(frac: f32, extent: u32) -> u32 {
    if !frac.is_finite() || frac <= 0.0 {
        return 0;
    }
    let target = f64::from(frac) * f64::from(extent);
    if target >= f64::from(extent) {
        return extent;
    }
    // Largest u32 candidate whose lossless f64 widening is <= target.
    let mut lo: u32 = 0;
    let mut hi: u32 = extent;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Convert a normalized fraction in `[0,1]` to a pixel coordinate `f32` against
/// `extent`, `as`-cast-free, for placing the per-tile overlay surface. Reuses the
/// exact integer [`frac_to_px`] then widens losslessly (overlay sizes are well
/// under 2^24).
#[cfg(feature = "overlay")]
fn norm_to_px_f32(frac: f32, extent: u32) -> f32 {
    let px = frac_to_px(frac, extent);
    let high = u16::try_from(px >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(px & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Whole ticks in one second at `cadence` (`num/den`), rounded to nearest, used
/// as the GOP / segment length. Exact integer arithmetic (never float fps).
fn ticks_per_second(cadence: Rational) -> u32 {
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den).max(1);
    let rounded = (num + den / 2) / den;
    u32::try_from(rounded.max(1)).unwrap_or(u32::MAX)
}

/// Build the **per-source** per-tick audio-loudness timelines (dBFS) off the
/// build path — one entry per **file-backed** source that decodes to audio.
///
/// For each `file`/`test` source it runs that source's own decoded 48 kHz
/// samples through a sample-peak [`multiview_audio::Ballistics`] meter and snapshots
/// the meter at each output-tick boundary, producing one dBFS value per tick. The
/// meter is the program-loudness DSP, so a silent track reads its floor and a
/// loud track reads high — each tile's on-screen bar reflects *that input's* own
/// audio.
///
/// A source with no decodable audio is simply omitted from the map (live URLs are
/// not pre-decoded here — they never EOF; NDI/unknown carry no file; an
/// audio-free clip yields nothing); its tile then rides the meter floor rather
/// than fabricating a constant. This runs once at build time and never touches
/// the hot path (invariant #1/#10).
#[cfg(feature = "overlay")]
fn build_meter_timelines(
    config: &MultiviewConfig,
    cadence: Rational,
) -> std::collections::HashMap<String, Vec<f64>> {
    let mut timelines = std::collections::HashMap::new();
    for source in &config.sources {
        // Only a file-backed source has decodable audio to pre-measure here.
        // Synthetic (bars/solid/clock) carry no audio; live URLs never EOF and are
        // not pre-decoded; NDI/unknown carry no file. None get a build-time meter.
        let path = match &source.kind {
            SourceKind::File { path } => PathBuf::from(path),
            _ => continue,
        };
        match meter_timeline_for_file(&path, cadence) {
            Ok(timeline) if !timeline.is_empty() => {
                timelines.insert(source.id.clone(), timeline);
            }
            Ok(_) => {}
            Err(reason) => {
                tracing::debug!(source = %source.id, %reason, "no per-input meter timeline");
            }
        }
    }
    timelines
}

/// Decode `path`'s audio and snapshot a sample-peak meter at each output-tick
/// boundary, yielding one dBFS reading per tick.
#[cfg(feature = "overlay")]
fn meter_timeline_for_file(path: &Path, cadence: Rational) -> Result<Vec<f64>, String> {
    use multiview_audio::decode::AudioFileDecoder;
    use multiview_audio::{Ballistics, ChannelLayout, MeterScale, PeakMode};

    let mut decoder =
        AudioFileDecoder::open(path, ChannelLayout::Stereo).map_err(|e| e.to_string())?;
    let format = decoder.format();
    let rate = format.sample_rate();
    let channels = format.channel_count().max(1);
    let mut meter = Ballistics::new(rate, MeterScale::SamplePeak(PeakMode::Sample));

    // Samples per output tick = sample_rate * den / num (exact integer; never
    // float fps). At least one sample per tick so a tick always advances.
    let num = i128::from(cadence.num).max(1);
    let den = i128::from(cadence.den).max(1);
    let samples_per_tick = (i128::from(rate).saturating_mul(den) / num).max(1);

    let mut timeline = Vec::new();
    // Drive the meter sample-by-sample (downmix to mono by averaging the frame's
    // channels — the meter wants a single program-loudness reading), emitting the
    // reading each time we cross a tick boundary in input samples.
    let mut samples_since_tick: i128 = 0;
    while let Some(block) = decoder.next_block().map_err(|e| e.to_string())? {
        for frame in block.interleaved().chunks_exact(channels) {
            let sum: f32 = frame.iter().copied().sum();
            let mono = f64::from(sum) / f64::from(u32_from_usize_audio(channels));
            meter.push(mono);
            samples_since_tick = samples_since_tick.saturating_add(1);
            if samples_since_tick >= samples_per_tick {
                timeline.push(meter.reading_db());
                samples_since_tick = 0;
            }
        }
    }
    // Flush a final partial tick so a short clip still contributes a reading.
    if samples_since_tick > 0 {
        timeline.push(meter.reading_db());
    }
    Ok(timeline)
}

/// Saturating `usize` → `u32` for the audio-channel divisor (no `as`).
#[cfg(feature = "overlay")]
fn u32_from_usize_audio(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The codec token a config output names, if it carries one.
/// Read an optional analog clock face from the config `[[overlays]]` list: the
/// first entry whose `kind == "clock"` and whose `face` param is `"analog"`.
///
/// Placement comes from optional `x`/`y`/`radius` params (canvas pixels); a
/// missing placement defaults the face to the bottom-right corner sized to the
/// canvas. An optional `tz_minutes` param sets the timezone offset (default UTC).
/// Returns `None` when no analog clock is requested (the digital label still
/// renders). Without the `overlay` feature this is never called.
#[cfg(feature = "overlay")]
fn analog_clock_from_config(
    overlays: &[multiview_config::Overlay],
    canvas_w: u32,
    canvas_h: u32,
) -> Option<crate::overlays::AnalogClockSpec> {
    use multiview_overlay::clock::TimeZoneOffset;

    let entry = overlays.iter().find(|o| {
        o.kind == "clock"
            && o.params
                .get("face")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|f| f.eq_ignore_ascii_case("analog"))
    })?;

    let cw = u32_to_f32(canvas_w);
    let ch = u32_to_f32(canvas_h);
    // A face sized to ~22% of the shorter canvas side by default.
    let default_radius = cw.min(ch) * 0.11;
    // Config placement is whole-pixel / whole-minute; round each param to an i32
    // and widen it losslessly to f32 (no `as` cast), or fall back to the default.
    let param_f32 = |key: &str| -> Option<f32> {
        entry
            .params
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .map(|v| i32_to_f32(round_f64_to_i32(v)))
    };
    let radius = param_f32("radius").unwrap_or(default_radius).max(8.0);
    // Default placement: bottom-right corner, inset by the radius + a margin.
    let margin = radius * 0.25;
    let cx = param_f32("x").unwrap_or(cw - radius - margin);
    let cy = param_f32("y").unwrap_or(ch - radius - margin);
    let zone = entry
        .params
        .get("tz_minutes")
        .and_then(serde_json::Value::as_f64)
        .map_or(TimeZoneOffset::UTC, |m| {
            TimeZoneOffset::from_minutes(round_f64_to_i32(m))
        });

    Some(crate::overlays::AnalogClockSpec::new(zone, cx, cy, radius))
}

/// Exact small-`u32` → `f32` widening (canvas sizes are well under `2^24`), no
/// `as` cast — mirrors [`norm_to_px_f32`]'s widener.
#[cfg(feature = "overlay")]
fn u32_to_f32(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` → `f32` widening, no `as` cast.
#[cfg(feature = "overlay")]
fn i32_to_f32(value: i32) -> f32 {
    if value < 0 {
        -u32_to_f32(value.unsigned_abs())
    } else {
        u32_to_f32(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

/// Round a finite `f64` config measure to the nearest `i32` (saturating to the
/// `i32` range), no `as` cast: handle the sign, then a bounded binary search over
/// the unsigned magnitude (mirrors [`frac_to_px`]'s `u64`-bounded search).
#[cfg(feature = "overlay")]
fn round_f64_to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    let negative = r < 0.0;
    let magnitude = r.abs().min(f64::from(i32::MAX));
    // Largest u32 candidate whose widening is <= the magnitude (so the rounded
    // value maps back exactly for integral inputs within range).
    let mut lo = 0_u32;
    let mut hi = u32::try_from(i32::MAX).unwrap_or(u32::MAX);
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= magnitude {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    let value = i32::try_from(lo).unwrap_or(i32::MAX);
    if negative {
        value.saturating_neg()
    } else {
        value
    }
}

fn output_codec(output: &Output) -> Option<&str> {
    match output {
        Output::RtspServer { codec, .. }
        | Output::LlHls { codec, .. }
        | Output::Hls { codec, .. }
        | Output::Rtmp { codec, .. }
        | Output::Srt { codec, .. } => Some(codec.as_str()),
        // NDI carries no codec token; a future output kind names none here until
        // explicitly wired (`Output` is `#[non_exhaustive]`).
        Output::Ndi { .. } | _ => None,
    }
}

/// The outputs `build_outputs` assembles: the encode-once-mux-many **packet**
/// sinks, plus (feature `display-kms`) the **raw-frame** display-head plans —
/// two disjoint paths by design (ADR-0044: a display sink consumes the
/// pre-encode canvas and never joins the packet fan-out).
struct BuiltOutputs {
    /// File/HLS/push sinks fed the one encoded packet stream (invariant #7).
    packet: Vec<RunnableOutput>,
    /// DRM/KMS display heads, started as raw-frame sinks at stream start.
    #[cfg(feature = "display-kms")]
    display: Vec<DisplayOutputPlan>,
}

/// One configured display head (feature `display-kms`): everything
/// [`start_display_sinks`] needs to open the device and light the connector.
#[cfg(feature = "display-kms")]
#[derive(Debug, Clone)]
struct DisplayOutputPlan {
    /// The output's stable id (diagnostics).
    output_id: String,
    /// Which connector to drive.
    connector: multiview_output::display::ConnectorSelector,
    /// The mode request (auto / exact override).
    mode: multiview_output::display::ModeRequest,
    /// The CVT-RB forced mode for an EDID-less chain, if configured.
    forced_mode: Option<multiview_output::display::ForcedMode>,
}

/// Extract the display-head plan from one `Output::Display` (feature
/// `display-kms`).
#[cfg(feature = "display-kms")]
fn display_plan_of(output: &Output) -> Option<DisplayOutputPlan> {
    use multiview_output::display::{ConnectorSelector, ForcedMode, ModeRequest};
    let Output::Display {
        connector,
        mode,
        forced_mode,
        ..
    } = output
    else {
        return None;
    };
    let selector = if connector.trim().eq_ignore_ascii_case("auto") {
        ConnectorSelector::Auto
    } else {
        ConnectorSelector::Name(connector.clone())
    };
    let request = mode
        .as_ref()
        .map_or(ModeRequest::Auto, |spec| ModeRequest::Exact {
            width: spec.width,
            height: spec.height,
            refresh: spec.refresh.rational(),
        });
    let forced = forced_mode.as_ref().map(|spec| ForcedMode {
        width: spec.width,
        height: spec.height,
        refresh: spec.refresh.rational(),
    });
    Some(DisplayOutputPlan {
        output_id: output.id(),
        connector: selector,
        mode: request,
        forced_mode: forced,
    })
}

/// The display sinks' view of the composited program (feature `display-kms`):
/// an `Arc` clone of the **same** pre-encode NV12 canvas the preview slot and
/// the encode fan-out share — no extra pixel copy on the hot loop.
#[cfg(feature = "display-kms")]
#[derive(Debug)]
struct CanvasFrame(Arc<Nv12Image>);

#[cfg(feature = "display-kms")]
impl multiview_output::display::DisplayCanvas for CanvasFrame {
    fn width(&self) -> u32 {
        self.0.width()
    }
    fn height(&self) -> u32 {
        self.0.height()
    }
    fn y_plane(&self) -> &[u8] {
        self.0.y_plane()
    }
    fn uv_plane(&self) -> &[u8] {
        self.0.uv_plane()
    }
}

/// Open and light every configured display head (feature `display-kms`):
/// scan `/dev/dri` for the connector-owning card, probe + select the mode
/// (EDID preferred / exact-rational `cadence` match / CVT-RB forced), run the
/// `TEST_ONLY` validation and the one startup modeset, and spawn the
/// dedicated flip-loop thread per head (ADR-0044 §1). Startup-only; runs
/// before the output clock starts.
///
/// # Errors
///
/// [`PipelineError::Output`] when a head cannot be opened, has no usable
/// mode, or fails validation/modeset — a misconfigured output fails the run
/// (it is never silently skipped).
#[cfg(feature = "display-kms")]
fn start_display_sinks(
    plans: Vec<DisplayOutputPlan>,
    cadence: Rational,
) -> Result<
    (
        Vec<multiview_output::display::DisplaySinkHandle>,
        Vec<multiview_output::display::FramePublisher<CanvasFrame>>,
    ),
    PipelineError,
> {
    use multiview_output::display::kms::KmsDisplayDevice;
    use multiview_output::display::{DisplaySink, DisplaySinkConfig};
    let mut handles = Vec::with_capacity(plans.len());
    let mut publishers = Vec::with_capacity(plans.len());
    for plan in plans {
        let device = KmsDisplayDevice::open_for_connector(&plan.connector).map_err(|e| {
            PipelineError::Output {
                kind: "display",
                reason: format!("{}: {e}", plan.output_id),
            }
        })?;
        let (handle, publisher) = DisplaySink::start::<CanvasFrame, _>(
            device,
            DisplaySinkConfig {
                output_id: plan.output_id.clone(),
                connector: plan.connector,
                mode: plan.mode,
                forced_mode: plan.forced_mode,
                engine_cadence: Some(cadence),
                // A few ms bounds both the stop-flag latency and how quickly
                // an idle pipe notices a fresh mailbox frame; well under one
                // frame period at any broadcast cadence.
                poll_interval: Duration::from_millis(4),
            },
        )
        .map_err(|e| PipelineError::Output {
            kind: "display",
            reason: format!("{}: {e}", plan.output_id),
        })?;
        handles.push(handle);
        publishers.push(publisher);
    }
    Ok((handles, publishers))
}

/// Build the runnable sinks from the config outputs.
///
/// HLS/LL-HLS segment to disk; **RTMP and SRT push outputs are run** via the
/// [`PushSink`] (the same encode-once-mux-many drive loop the file/HLS sinks use —
/// invariant #7 — only the muxer targets a network URL). A **display** output
/// (DEV-B1 / ADR-0044) is built as a raw-frame DRM/KMS plan in a `display-kms`
/// build and is a hard error otherwise (never silently skipped — the gate in
/// [`crate::outputs`]). The RTSP *server* and NDI out are genuinely not
/// implemented (an RTSP server is its own RTP/RTSP protocol stack; NDI is the
/// proprietary runtime-loaded SDK), so they are honestly skipped with a log
/// line rather than pretended-runnable — a config mixing one with a supported
/// output still produces that supported output.
fn build_outputs(outputs: &[Output]) -> Result<BuiltOutputs, PipelineError> {
    // A display output in a non-display-kms build is a configuration the
    // binary cannot honour: fail the build clearly, never skip (DEV-B1).
    crate::outputs::ensure_display_outputs_supported(outputs).map_err(|reason| {
        PipelineError::Output {
            kind: "display",
            reason,
        }
    })?;
    let mut runnable = Vec::new();
    #[cfg(feature = "display-kms")]
    let mut display_plans = Vec::new();
    for output in outputs {
        match output {
            Output::Display { .. } => {
                // Runnable only under `display-kms` (the gate above already
                // rejected it otherwise): a raw-frame scanout plan, started at
                // stream start — deliberately NOT a packet sink (ADR-0044).
                #[cfg(feature = "display-kms")]
                if let Some(plan) = display_plan_of(output) {
                    display_plans.push(plan);
                }
            }
            Output::Hls { path, .. } | Output::LlHls { path, .. } => {
                let (dir, prefix, playlist_path) = hls_paths(Path::new(path));
                std::fs::create_dir_all(&dir).map_err(|e| PipelineError::Output {
                    kind: "hls",
                    reason: format!("creating {}: {e}", dir.display()),
                })?;
                // Live rolling playlist (HLS-0/1, ADR-0032): the sink publishes the
                // windowed `.m3u8` on every closed segment and prunes the evicted
                // `.ts` — so a live (infinite) run keeps `multiview.m3u8` current
                // and disk bounded, instead of 404ing until a finalize that never
                // comes. A 6-segment window is the rolling DVR depth.
                runnable.push(RunnableOutput::Hls {
                    sink: PacketMuxSink::segment_live(
                        dir,
                        prefix,
                        playlist_path.clone(),
                        HLS_LIVE_WINDOW,
                    ),
                    playlist_path,
                });
            }
            Output::Rtmp { url, .. } => {
                runnable.push(RunnableOutput::Push {
                    sink: PacketMuxSink::push(PushProtocol::Rtmp, url.clone()),
                    label: "rtmp",
                    url: url.clone(),
                });
            }
            Output::Srt { url, .. } => {
                runnable.push(RunnableOutput::Push {
                    sink: PacketMuxSink::push(PushProtocol::Srt, url.clone()),
                    label: "srt",
                    url: url.clone(),
                });
            }
            Output::RtspServer { .. } => {
                tracing::warn!(
                    "rtsp_server output is not implemented (an RTSP server is its own \
                     RTP/RTSP protocol stack); skipping"
                );
            }
            Output::Ndi { .. } => {
                tracing::warn!(
                    "ndi output is not implemented (the NDI SDK is runtime-loaded and \
                     not yet wired); skipping"
                );
            }
            // `Output` is `#[non_exhaustive]`; an unrecognized future kind is
            // skipped rather than silently mishandled.
            _ => {
                tracing::warn!("unrecognized output kind is not runnable from the CLI; skipping");
            }
        }
    }
    // The single-file `program.ts` anchor is NOT derived here: it depends on the
    // run mode (offline vs live), which is only known when the run starts. A LIVE
    // HLS run must not also write an ever-growing `program.ts` (HLS-2, ADR-0032),
    // while a finite/offline render still wants the self-contained container — so
    // the anchor is prepended at run time by `maybe_prepend_program_ts`, not here.
    Ok(BuiltOutputs {
        packet: runnable,
        #[cfg(feature = "display-kms")]
        display: display_plans,
    })
}

/// Optionally derive a single-file `program.ts` container sink from the first HLS
/// output (fed the same one encode, invariant #7) and prepend it to the runnable
/// outputs. The anchor is added **only for an offline/finite render** (`live ==
/// false`), where the file is bounded by the fixed tick count and a single
/// self-contained playable container is wanted. It is **suppressed for a live
/// run** (HLS-2, ADR-0032): an infinite live run must not also write an
/// ever-growing `program.ts` — the rolling segment window (the live `.m3u8` +
/// pruned `seg*.ts`) is the bounded on-disk artifact instead.
fn maybe_prepend_program_ts(mut runnable: Vec<RunnableOutput>, live: bool) -> Vec<RunnableOutput> {
    if live {
        return runnable;
    }
    let file_path = runnable.iter().find_map(|r| match r {
        RunnableOutput::Hls { playlist_path, .. } => {
            Some(playlist_path.with_file_name("program.ts"))
        }
        // A push has no on-disk directory to derive a program file from; only an
        // HLS output anchors the self-contained `program.ts`.
        RunnableOutput::File { .. } | RunnableOutput::Push { .. } => None,
    });
    if let Some(path) = file_path {
        runnable.insert(
            0,
            RunnableOutput::File {
                sink: PacketMuxSink::file(path.clone()),
                path,
            },
        );
    }
    runnable
}

/// Split an HLS output `path` into `(segment_dir, segment_prefix, playlist_path)`.
///
/// A `path` ending in `.m3u8` names the playlist; segments are written beside it
/// with prefix `seg`. A `path` naming a directory writes `index.m3u8` + `seg*.ts`
/// inside it.
fn hls_paths(path: &Path) -> (PathBuf, String, PathBuf) {
    if path.extension().and_then(std::ffi::OsStr::to_str) == Some("m3u8") {
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        (dir, "seg".to_owned(), path.to_path_buf())
    } else {
        (
            path.to_path_buf(),
            "seg".to_owned(),
            path.join("index.m3u8"),
        )
    }
}

/// A [`PacketSource`] over the bake consumer's per-sink fan-out channel of coded
/// packets (encode-once-mux-many): it pulls each [`EncodedPacket`] — an owned
/// copy the consumer fanned out from the single [`ProgramEncoder`] — off the
/// bounded receiver and hands it to the mux-only sink's `run()`. `recv()`
/// blocking on a packet is the sink's own pull (off the hot path — it can never
/// back-pressure the engine, only the off-hot-path consumer); a closed channel
/// (`recv() == Err`) is end-of-program → `Ok(None)`, so the sink writes its
/// trailer. The packets are already PTS-stamped in the encoder time-base (the
/// muxer rescales them) — nothing re-stamps here (invariant #3 was applied at
/// encode).
struct StreamingPacketSource {
    rx: Receiver<EncodedPacket>,
    /// How many packets this source delivered (for the per-sink report count).
    delivered: usize,
}

impl StreamingPacketSource {
    fn new(rx: Receiver<EncodedPacket>) -> Self {
        Self { rx, delivered: 0 }
    }
}

impl PacketSource for StreamingPacketSource {
    fn next_packet(&mut self) -> multiview_output::Result<Option<EncodedPacket>> {
        // Block until the consumer fans the next coded packet, or the channel
        // closes (end-of-program). This is the sink's pull, off the hot path.
        let Ok(packet) = self.rx.recv() else {
            return Ok(None);
        };
        self.delivered = self.delivered.saturating_add(1);
        Ok(Some(packet))
    }
}

/// Bridge a baked CPU-reference [`Nv12Image`] into the [`DecodedVideoFrame`] the
/// single [`ProgramEncoder`] consumes (NV12 → a libav frame). The encoder
/// re-stamps the PTS from its own tick counter, so the meta PTS here is an unused
/// placeholder. The consumer calls this once per baked frame, before the single
/// encode (invariant #7) — the bridge that used to run once *per sink* now runs
/// once *per frame*.
fn nv12_to_decoded(image: &Nv12Image) -> Result<DecodedVideoFrame, PipelineError> {
    let frame = nv12_to_video(image)?;
    let meta = FrameMeta {
        pts: MediaTime::ZERO,
        width: image.width(),
        height: image.height(),
        format: PixelFormat::Nv12,
        color: image.color(),
    };
    // The composited canvas is not an ingested stream — there is no source-tick
    // PTS to unwrap (the encoder re-stamps from its tick counter), so no raw PTS,
    // and no embedded (A53) caption side data.
    Ok(DecodedVideoFrame {
        frame,
        meta,
        raw_pts: None,
        a53_cc: None,
    })
}

/// Bridge a CPU-reference [`Nv12Image`] into a libav NV12 [`Video`] frame by
/// copying its planes row-by-row into the freshly-allocated frame's planes
/// (respecting libav's plane strides, which may be padded). No FFI: this uses
/// only `ffmpeg-next`'s safe `Video::new`/`data_mut`/`stride` value API.
fn nv12_to_video(image: &Nv12Image) -> Result<Video, PipelineError> {
    let w = image.width();
    let h = image.height();
    let mut frame = Video::new(Pixel::NV12, w, h);

    let wu = usize::try_from(w).map_err(|_| PipelineError::Bridge("width overflow".to_owned()))?;
    let hu = usize::try_from(h).map_err(|_| PipelineError::Bridge("height overflow".to_owned()))?;

    // Read strides before taking the mutable plane borrows (the immutable
    // `stride` borrow cannot overlap the `data_mut` borrow).
    let y_stride = frame_stride(&frame, 0)?;
    let uv_stride = frame_stride(&frame, 1)?;

    // Plane 0: Y, `w` bytes per row, `h` rows.
    copy_plane(frame.data_mut(0), y_stride, image.y_plane(), wu, hu)?;
    // Plane 1: interleaved Cb/Cr, `w` bytes per row, `h/2` rows.
    copy_plane(frame.data_mut(1), uv_stride, image.uv_plane(), wu, hu / 2)?;
    Ok(frame)
}

/// The libav stride (bytes per row) of `frame`'s `plane`.
fn frame_stride(frame: &Video, plane: usize) -> Result<usize, PipelineError> {
    let stride = frame.stride(plane);
    if stride == 0 {
        return Err(PipelineError::Bridge(format!(
            "plane {plane} has zero stride"
        )));
    }
    Ok(stride)
}

/// Copy `rows` rows of `row_bytes` bytes from a tightly-packed `src` into a
/// libav plane `dst` whose rows are `dst_stride` bytes apart (`dst_stride` >=
/// `row_bytes`; the trailing padding is left untouched).
fn copy_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    row_bytes: usize,
    rows: usize,
) -> Result<(), PipelineError> {
    if dst_stride < row_bytes {
        return Err(PipelineError::Bridge(
            "libav plane stride is narrower than the row".to_owned(),
        ));
    }
    for row in 0..rows {
        let src_start = row
            .checked_mul(row_bytes)
            .ok_or_else(|| PipelineError::Bridge("src offset overflow".to_owned()))?;
        let dst_start = row
            .checked_mul(dst_stride)
            .ok_or_else(|| PipelineError::Bridge("dst offset overflow".to_owned()))?;
        let src_row = src
            .get(src_start..src_start + row_bytes)
            .ok_or_else(|| PipelineError::Bridge("src row out of range".to_owned()))?;
        let dst_row = dst
            .get_mut(dst_start..dst_start + row_bytes)
            .ok_or_else(|| PipelineError::Bridge("dst row out of range".to_owned()))?;
        dst_row.copy_from_slice(src_row);
    }
    Ok(())
}

/// Probe each path-backed source's full elementary-stream [`StreamInventory`]
/// (RT-3, ADR-0034 §9), keyed (id-sorted) by source id.
///
/// Runs **once at build time**, off the output-clock thread: it opens each local
/// container's demuxer (an open-time snapshot, `param_probe.rs`) and reads its
/// [`StreamInventory`] — every elementary stream (video / audio tracks /
/// subtitles / SCTE-35 / KLV / timecode) the input offers, each with a stable
/// kind-scoped id. The result is threaded into the published
/// `EngineStateSnapshot` so the control plane's read-only
/// `GET /inputs/{id}/streams` surface can show every stream, and emitted once as
/// `input.streams` events at run start. **Off the hot loop** (inv #10): nothing
/// here touches the data plane or the output clock.
///
/// Only local **file** sources are probed here (the demuxer opens a path);
/// synthetic / live-URL / NDI sources contribute nothing and ride an empty map
/// until their ingest thread surfaces an inventory in a later slice. A probe
/// failure (unreadable container) is logged and skipped — it never fails the
/// build of an otherwise-runnable source (invariants #1/#10).
#[cfg(feature = "ffmpeg")]
fn build_input_inventories(
    config: &MultiviewConfig,
) -> std::collections::BTreeMap<String, multiview_core::stream::StreamInventory> {
    use multiview_ffmpeg::Demuxer;

    let mut inventories = std::collections::BTreeMap::new();
    for source in &config.sources {
        let SourceKind::File { path } = &source.kind else {
            continue;
        };
        match Demuxer::open(std::path::Path::new(path)) {
            Ok(demux) => {
                let inventory = demux.inventory().with_input_id(source.id.clone());
                inventories.insert(source.id.clone(), inventory);
            }
            Err(err) => {
                tracing::warn!(
                    source = %source.id,
                    error = %err,
                    "could not probe stream inventory for input (skipping discovery)"
                );
            }
        }
    }
    inventories
}

/// The no-`ffmpeg` build has no demuxer to probe, so no inventory is discovered
/// here (synthetic-only builds, and the GPU-free default). An empty map folds
/// into the snapshot as a no-op, so the run output is unchanged (RT-3).
#[cfg(not(feature = "ffmpeg"))]
fn build_input_inventories(
    _config: &MultiviewConfig,
) -> std::collections::BTreeMap<String, multiview_core::stream::StreamInventory> {
    std::collections::BTreeMap::new()
}

/// Resolve a config [`Source`] into a streaming [`IngestPlan`] (it does **not**
/// decode anything — the plan is consumed later by an ingest thread).
///
/// Synthetic sources (bars/solid/clock) record a [`SourceLocation::Synthetic`]
/// rendered in-process by [`crate::synth::generator_loop`] (no subprocess, no
/// media to open); file/rtsp/hls/ts/srt/rtmp sources record their path/URL to be
/// opened on the ingest thread. Live transports (rtsp/hls/ts/srt/rtmp) and the
/// continuously-rendered synthetic sources are flagged `live` so the ingest loop
/// runs forever; a `file` source is finite.
///
/// # Errors
///
/// Returns [`PipelineError::Ingest`] for an NDI/unsupported source kind, or a
/// synthetic source with invalid parameters. (Opening/decoding errors surface on
/// the ingest thread later — they must never fail the *build* of a never-ending
/// live source.)
fn ingest_plan_for(
    source: &Source,
    tile_w: u32,
    tile_h: u32,
    store: Arc<TileStore<Nv12Image>>,
    canvas_color: CanvasColor,
    cadence: Rational,
) -> Result<IngestPlan, PipelineError> {
    let (location, live) = match &source.kind {
        // Synthetic sources (bars/solid/clock/timer) are rendered in-process by a
        // generator thread (ADR-0027) — a peer of a decode thread, `live` because
        // it produces frames continuously. No ffmpeg subprocess, no media to open.
        SourceKind::Bars
        | SourceKind::Solid { .. }
        | SourceKind::Clock { .. }
        | SourceKind::Timer { .. } => {
            let kind =
                crate::synth::SyntheticKind::from_source_kind(&source.kind).ok_or_else(|| {
                    PipelineError::Ingest {
                        id: source.id.clone(),
                        reason: "invalid synthetic source parameters".to_owned(),
                    }
                })?;
            (SourceLocation::Synthetic(kind), true)
        }
        SourceKind::File { path } => (SourceLocation::Path(PathBuf::from(path)), false),
        SourceKind::Rtsp { url, .. }
        | SourceKind::Hls { url }
        | SourceKind::Ts { url }
        | SourceKind::Srt { url }
        | SourceKind::Rtmp { url } => (SourceLocation::Url(url.clone()), true),
        // A YouTube live source is a thin wrapper over HLS (ADR-0015): bound by its
        // watch URL, resolved to a fresh `*.googlevideo.com` HLS master on every
        // (re)connect by `yt-dlp`. It is `live` so the ingest loop reconnects (and
        // re-resolves) forever; a resolve failure never fails the build — it
        // degrades the tile on the ingest thread (invariants #1/#10). Only wired
        // under the off-by-default `youtube` feature.
        #[cfg(feature = "youtube")]
        SourceKind::Youtube { url } => (
            SourceLocation::Youtube {
                watch_url: url.clone(),
            },
            true,
        ),
        // An NDI source (ADR-0008 / IN-3) is bound by its NDI source NAME and
        // received from host memory via `multiview-input`'s runtime-loaded NDI seam
        // — it bypasses libav. It is `live` so the ingest loop reconnects forever; a
        // receive fault or an absent/unlicensed runtime degrades the tile on the
        // ingest thread, never failing the build (invariants #1/#10). Only wired
        // under the off-by-default `ndi` feature.
        #[cfg(feature = "ndi")]
        SourceKind::Ndi { name } => (SourceLocation::Ndi { name: name.clone() }, true),
        // With the `ndi` feature OFF the NDI runtime-load obligation is not built
        // in, so an NDI source is an honest typed refusal (not a silent skip).
        #[cfg(not(feature = "ndi"))]
        SourceKind::Ndi { .. } => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "NDI ingest requires the `ndi` feature (off by default)".to_owned(),
            })
        }
        // `SourceKind` is `#[non_exhaustive]`; a future kind is unsupported here
        // until explicitly wired (never silently mishandled).
        _ => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "unsupported source kind for the CLI pipeline".to_owned(),
            })
        }
    };

    Ok(IngestPlan {
        id: source.id.clone(),
        location,
        tile_w,
        tile_h,
        store,
        live,
        #[cfg(feature = "overlay")]
        incontainer_sub: None,
        #[cfg(feature = "overlay")]
        embedded_cc: None,
        canvas_color,
        cadence,
        // No GPU pinned yet: the load-aware admission pick (decide-once, in
        // `drive_streaming`) stamps the chosen device's CUDA ordinal onto every
        // plan before the ingest threads spawn. `None` is the default-device /
        // GPU-free path, in lockstep with the compositor's `None`.
        cuda_ordinal: None,
    })
}

/// Resolve a source's **audio** decode plan (AUD-2): the libav-openable location
/// (file path or network URL) its audio is decoded from, plus its live flag.
///
/// Returns `None` for sources with no libav audio path — a synthetic
/// (bars/solid/clock) source carries no audio, and an NDI source's audio is a
/// separate host-memory concern not wired here. Such a source rides silence on
/// the program bus (the store silence-fills). A network URL whose container has
/// no audio stream still yields a plan; its decode loop simply ends at open time
/// (no audio stream found) and the source rides silence — never an error.
///
/// The location mirrors the video [`ingest_plan_for`] mapping so a source's audio
/// is decoded from the SAME media as its video (the audio peer opens its own
/// libav context — the contexts are `!Send` and must not be shared).
fn audio_ingest_plan_for(source: &Source) -> Option<crate::audio::AudioIngestPlan> {
    let (location, live) = match &source.kind {
        // Synthetic sources render video in-process and carry no audio.
        SourceKind::Bars
        | SourceKind::Solid { .. }
        | SourceKind::Clock { .. }
        | SourceKind::Timer { .. } => return None,
        SourceKind::File { path } => (path.clone(), false),
        SourceKind::Rtsp { url, .. }
        | SourceKind::Hls { url }
        | SourceKind::Ts { url }
        | SourceKind::Srt { url }
        | SourceKind::Rtmp { url } => (url.clone(), true),
        // YouTube/NDI audio is not wired through the libav file decoder here:
        // YouTube needs the watch-URL resolve step (deferred to its own slice) and
        // NDI audio is a host-memory receive. Both ride silence on the bus for now.
        #[cfg(feature = "youtube")]
        SourceKind::Youtube { .. } => return None,
        #[cfg(feature = "ndi")]
        SourceKind::Ndi { .. } => return None,
        // Any other (incl. NDI without the feature, future kinds): no audio path.
        _ => return None,
    };
    Some(crate::audio::AudioIngestPlan {
        id: source.id.clone(),
        location,
        live,
    })
}

/// Resolve a `bars` synthetic source's **line-up tone** plan (AUD-5): the source
/// id + the output cadence the 1 kHz reference tone is paced to.
///
/// Returns `Some` **only** for [`SourceKind::Bars`] — the SMPTE/EBU colour-bars
/// card's audible companion is a 1 kHz tone. Every other source (the other
/// synthetic kinds `solid`/`clock`, and all decoded/NDI sources) returns `None`:
/// `solid`/`clock` carry no audio, and a decoded source's real audio is handled
/// by [`audio_ingest_plan_for`]. So this never double-routes a decoded source.
fn tone_ingest_plan_for(
    source: &Source,
    cadence: Rational,
) -> Option<crate::audio::ToneIngestPlan> {
    match &source.kind {
        SourceKind::Bars => Some(crate::audio::ToneIngestPlan {
            id: source.id.clone(),
            cadence,
        }),
        _ => None,
    }
}

/// Resolve every source's HLS `WebVTT` caption plan concurrently (#48) and index
/// them by source id for the build loop's cheap per-source lookup. The N
/// network-bound master-fetches overlap off the serial build path. Only under
/// `overlay` (the baker that consumes the cues is overlay-gated).
#[cfg(feature = "overlay")]
fn prefetch_caption_plans(
    config: &MultiviewConfig,
) -> std::collections::HashMap<String, crate::captions::CaptionPlan> {
    crate::captions::resolve_caption_plans(&config.sources)
        .into_iter()
        .map(|plan| (plan.id.clone(), plan))
        .collect()
}

/// Wire a source's native caption paths at build time: the HLS `WebVTT` rendition
/// (its concurrently-resolved plan, looked up from `prefetched`) and/or the
/// in-container DVB-sub route (#36 Phase 2, decoded on this source's own
/// video-ingest thread). Registers any cue store (for the baker to sample) +
/// reader plan, and stashes the dvbsub route on `plan`. Best-effort: a source
/// whose captions cannot be resolved simply shows none — this never fails the
/// build (invariants #1/#10).
#[cfg(feature = "overlay")]
fn wire_source_captions(
    source: &Source,
    plan: &mut IngestPlan,
    caption_stores: &mut std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    caption_plans: &mut Vec<crate::captions::CaptionPlan>,
    prefetched: &mut std::collections::HashMap<String, crate::captions::CaptionPlan>,
) {
    // HLS WebVTT rendition path: take this source's plan, already resolved
    // concurrently off the build path (#48). A cheap map lookup — no network here.
    if let Some(caption_plan) = prefetched.remove(&source.id) {
        caption_stores.insert(source.id.clone(), Arc::clone(&caption_plan.store));
        caption_plans.push(caption_plan);
    }

    let Some(selector) = source.captions.as_ref() else {
        return;
    };

    // Embedded CEA-608 (A53 side-data) path: the captions ride on the decoded
    // video frames (no separate stream), so this needs no container probe — the
    // video ingest loop pulls the A53 bytes off each frame and feeds `cc_dec`.
    // Wired when the `embedded_cc` selector resolves to a decodable 608 field; a
    // 708 service / unrecognised field declines honestly inside
    // `embedded_cc_channel` (logged, no silent cue-less decoder). Skipped if the
    // source already took the WebVTT path (a source has one caption store).
    if !caption_stores.contains_key(&source.id) {
        if let Some(channel) = crate::captions::embedded_cc_channel(&source.kind, selector) {
            let store = Arc::new(crate::captions::CueStore::new());
            caption_stores.insert(source.id.clone(), Arc::clone(&store));
            plan.embedded_cc = Some(EmbeddedCcRoute { channel, store });
            tracing::info!(source = %source.id, "native embedded CEA-608 caption route wired");
        }
    }

    // In-container subtitle path (DVB-sub bitmap, `ass`/`subrip`/`mov_text` text,
    // or teletext): the muxed subtitle stream is decoded on THIS source's
    // video-ingest thread (a sibling of the video packets), so the route is
    // stashed on the plan and its store registered for the baker. Only when the
    // selector takes the in-container path, the source has not already taken the
    // WebVTT/embedded-CC path, and the container actually carries a subtitle
    // stream this path decodes (else best-effort decline).
    if crate::captions::incontainer_selector_active(&source.kind, selector)
        && !caption_stores.contains_key(&source.id)
    {
        if let Some((route, cue_store)) =
            resolve_incontainer_sub_route(source, selector, &plan.location)
        {
            caption_stores.insert(source.id.clone(), cue_store);
            plan.incontainer_sub = Some(route);
        }
    }
}

/// Resolve the in-container **subtitle route** for a source whose selector takes
/// the native in-container path ([`crate::captions::incontainer_text_selector`]):
/// open the source container once, find its subtitle stream, map its codec to a
/// [`CaptionSource`](multiview_ffmpeg::CaptionSource) (DVB-sub bitmap, or
/// `ass`/`subrip`/`mov_text` text), and build the cue store. Returns `(route,
/// store)` so the caller can both stash the route on the ingest plan AND register
/// the store for the baker to sample. Best-effort: an open failure, no subtitle
/// stream, or an unsupported subtitle codec logs and returns `None` (the tile
/// simply shows no caption — it must never fail the pipeline build, #1/#10).
#[cfg(feature = "overlay")]
fn resolve_incontainer_sub_route(
    source: &Source,
    selector: &multiview_config::schema::CaptionSelector,
    location: &SourceLocation,
) -> Option<(InContainerSubRoute, Arc<crate::captions::CueStore>)> {
    use multiview_ffmpeg::convert::MediaKind;
    use multiview_ffmpeg::Demuxer;

    // Only a local-path container can be opened by `Demuxer` here; a `Ts` URL
    // source is decoded by the URL ingest path and is out of this MVP's scope.
    let path = match location {
        SourceLocation::Path(p) => p.as_path(),
        SourceLocation::Url(_) | SourceLocation::Synthetic(_) => return None,
        #[cfg(feature = "youtube")]
        SourceLocation::Youtube { .. } => return None,
        // NDI ingest carries no in-container subtitle stream (it is a raw
        // host-memory video receive); there is no container to open here.
        #[cfg(feature = "ndi")]
        SourceLocation::Ndi { .. } => return None,
    };
    let demux = match Demuxer::open(path) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(source = %source.id, error = %err, "could not open container for in-container captions");
            return None;
        }
    };
    let stream_index = demux.best_stream(MediaKind::Subtitle)?;
    let params = demux.streams();
    let stream = params.iter().find(|s| s.index == stream_index)?;
    // Choose the decoder from (selector, actual stream codec). A combination this
    // path does not decode (e.g. a `teletext_page` selector over a non-teletext
    // stream, or `hdmv_pgs_subtitle`) declines, so the tile shows no caption
    // rather than building a wrong/empty decoder.
    let Some(caption_source) =
        crate::captions::incontainer_caption_source(selector, &stream.codec_name)
    else {
        tracing::info!(
            source = %source.id,
            codec = %stream.codec_name,
            "in-container subtitle (selector, codec) not decoded by this path; no in-container captions"
        );
        return None;
    };
    let time_base = stream.time_base;
    let store = Arc::new(crate::captions::CueStore::new());
    tracing::info!(
        source = %source.id,
        stream_index,
        codec = %stream.codec_name,
        "native in-container subtitle caption route resolved"
    );
    Some((
        InContainerSubRoute {
            stream_index,
            time_base,
            source: caption_source,
            store: Arc::clone(&store),
        },
        store,
    ))
}

/// Where a source's media lives.
enum SourceLocation {
    /// A local filesystem path.
    Path(PathBuf),
    /// A libav-openable URL (rtsp/hls/ts/srt/rtmp).
    Url(String),
    /// An in-process synthetic source (bars/solid/clock) — no media to open;
    /// rendered by [`crate::synth::generator_loop`] on the ingest thread.
    Synthetic(crate::synth::SyntheticKind),
    /// A `YouTube` live source (ADR-0015): bound by its **watch** URL, resolved to
    /// a time-limited `*.googlevideo.com` HLS master by a runtime-discovered
    /// `yt-dlp` ([`multiview_input::youtube`]). The watch URL — never a hand-copied
    /// manifest — is carried so the ingest loop re-resolves a fresh master on every
    /// (re)connect: a manifest that ages out and starts 403-ing simply EOFs/errors
    /// the open, and the existing reconnect bracket re-enters and re-resolves, so a
    /// long run survives the ~6 h expiry. The resolution runs on the ingest thread
    /// (the control/IO plane), never on the output data plane (invariants #1/#10).
    #[cfg(feature = "youtube")]
    Youtube {
        /// The `YouTube` watch/live/channel URL bound by the config.
        watch_url: String,
    },
    /// An **NDI®** source (ADR-0008 / IN-3), bound by its NDI source **name** (the
    /// name other NDI tools discover, e.g. `STUDIO (CAM 1)`). NDI ingest is a
    /// host-memory receive that bypasses libav entirely: the ingest loop routes it
    /// to an NDI-specific drive ([`crate::pipeline::drive_ndi`]) over
    /// `multiview-input`'s runtime-loaded receive seam, not [`open_and_stream`].
    /// Only wired under the off-by-default `ndi` feature.
    #[cfg(feature = "ndi")]
    Ndi {
        /// The NDI source name bound by the config.
        name: String,
    },
}

/// The total budget the startup **prime-wait** ([`wait_for_prime`]) spends
/// waiting for every bound source's [`TileStore`] to publish its first frame
/// before the output clock's very first tick. Sized to cover normal decode +
/// scale latency (open container, decode the first GOP, scale to the tile) on
/// commodity hardware. CRITICAL (invariant #1/#2): this is a **hard upper
/// bound** — a source that never produces (a dead/missing/wedged input) can NOT
/// extend it; once it elapses the clock starts anyway and the unprimed tiles
/// ride their `NO_SIGNAL` / last-good placeholder ([`TileStore::read_at`] already
/// handles that). The prime-wait only ever *delays the first tick*; it never
/// changes the cadence, the per-tick logic, or makes a tile pace the output.
const PRIME_WAIT_BUDGET: Duration = Duration::from_millis(1_500);

/// How often [`wait_for_prime`] re-checks whether every store is primed. Short
/// so the clock starts promptly once the last input primes (typically well
/// before [`PRIME_WAIT_BUDGET`]); a few ms keeps the poll cheap.
const PRIME_WAIT_POLL: Duration = Duration::from_millis(5);

/// Generic libav I/O timeout (`rw_timeout`, **microseconds**) applied to network
/// (URL) ingest opens so a dead/stalled live source fails the open — or a
/// subsequent blocking read — instead of hanging a decode thread forever. Local
/// file opens get no timeout (a slow disk must not spuriously abort a finite
/// source mid-decode).
const INGEST_RW_TIMEOUT: Duration = Duration::from_secs(10);

/// Base reconnect backoff for a live source whose `open_and_stream` returned (EOF
/// or error). The first reconnect waits in `[BASE/2, BASE]` (equal jitter).
const INGEST_RECONNECT_BASE: Duration = Duration::from_millis(500);

/// Ceiling on the reconnect backoff: a source that keeps failing retries at most
/// this often (in `[CAP/2, CAP]`), so a hard-down source never hot-loops.
const INGEST_RECONNECT_CAP: Duration = Duration::from_secs(30);

/// Largest backoff attempt index used by [`reconnect_backoff`]. `BASE << this`
/// (500 ms × 64 = 32 s) already exceeds [`INGEST_RECONNECT_CAP`], so clamping the
/// attempt here keeps the `1u32 << attempt` shift from ever overflowing.
const INGEST_RECONNECT_MAX_ATTEMPT: u32 = 6;

/// A connection that streamed for at least this long is treated as "healthy": its
/// next drop reconnects from attempt 0 (a long-lived source that finally blips
/// recovers promptly), instead of carrying a stale escalated backoff.
const INGEST_RECONNECT_HEALTHY: Duration = Duration::from_secs(30);

/// How long [`IngestSupervisor::join_all`] waits for an ingest thread to observe
/// the stop flag and exit before detaching it. Generous enough that a thread in
/// a normal decode loop (which checks `stop` every packet) always joins cleanly,
/// short enough that a thread wedged in a blocking libav network call never
/// stalls the bounded run's teardown.
const INGEST_JOIN_GRACE: Duration = Duration::from_secs(2);

/// A small deterministic per-source PRNG for reconnect jitter. Seeded from the
/// source id so different sources de-correlate (no thundering herd) while the
/// sequence stays reproducible — which is what keeps [`reconnect_backoff`]
/// testable with no real randomness. A SplitMix64-style step; not cryptographic.
pub(crate) struct JitterRng(u64);

impl JitterRng {
    /// Seed from a stable hash of the source id (each source gets its own jitter
    /// phase). The seed is forced odd so the step never degenerates to a constant.
    pub(crate) fn seeded(id: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        Self(hasher.finish() | 1)
    }

    /// Advance the state and return the next jitter unit in `[0.0, 1.0]`.
    pub(crate) fn next_unit(&mut self) -> f64 {
        // SplitMix64 step (deterministic, well-distributed across the id space).
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // Map the top 32 bits into [0, 1] without a lossy `as` cast.
        let top = u32::try_from(self.0 >> 32).unwrap_or(u32::MAX);
        f64::from(top) / f64::from(u32::MAX)
    }
}

/// Capped-exponential reconnect backoff with **equal jitter**. The window doubles
/// per `attempt` (`BASE`, `2·BASE`, …), saturating at [`INGEST_RECONNECT_CAP`].
/// Equal jitter then splits the window into a fixed half plus a jittered half in
/// `[0, half]`, so the wait lands in `[window/2, window]` — never zero (no hot
/// reconnect loop) and de-correlated across sources/attempts (no thundering herd).
/// `jitter_fraction` comes from [`JitterRng::next_unit`]; a non-finite or
/// out-of-range value is clamped into `[0, 1]`.
pub(crate) fn reconnect_backoff(attempt: u32, jitter_fraction: f64) -> Duration {
    let shift = attempt.min(INGEST_RECONNECT_MAX_ATTEMPT);
    // `shift <= 6`, so the shift is always `Some`; `unwrap_or` is just belt-and-
    // braces against a future constant change.
    let mult = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    let window = INGEST_RECONNECT_BASE
        .checked_mul(mult)
        .unwrap_or(INGEST_RECONNECT_CAP)
        .min(INGEST_RECONNECT_CAP);
    let frac = if jitter_fraction.is_finite() {
        jitter_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let half = window / 2;
    half.saturating_add(half.mul_f64(frac))
}

/// The reconnect attempt index for the NEXT backoff, given the previous index and
/// how long the just-ended connection streamed. A connection that lasted at least
/// [`INGEST_RECONNECT_HEALTHY`] resets the escalation to 0 (it was healthy and
/// merely blipped); anything shorter is a fast failure that escalates the index by
/// one, capped at [`INGEST_RECONNECT_MAX_ATTEMPT`].
pub(crate) fn next_reconnect_attempt(prev: u32, ran_for: Duration) -> u32 {
    if ran_for >= INGEST_RECONNECT_HEALTHY {
        0
    } else {
        prev.saturating_add(1).min(INGEST_RECONNECT_MAX_ATTEMPT)
    }
}

/// Build the libav open options for a source. Network (URL) sources get
/// [`INGEST_RW_TIMEOUT`] as `rw_timeout` (microseconds) so a dead live source
/// cannot hang the decode thread on open or a blocking read; local files get an
/// empty dictionary (a slow disk must not abort a finite source mid-decode).
fn ingest_open_options(location: &SourceLocation) -> ffmpeg::Dictionary<'static> {
    let mut opts = ffmpeg::Dictionary::new();
    // Network sources (a plain URL or a resolved YouTube HLS master) get the
    // `rw_timeout` so a dead/stalled live source fails the open or a blocking read
    // instead of hanging the decode thread; local files and synthetic sources do
    // not. (A YouTube source's resolved master is a `*.googlevideo.com` URL — a
    // network source — so it is treated exactly like any other URL here.)
    let is_network = match location {
        SourceLocation::Url(_) => true,
        #[cfg(feature = "youtube")]
        SourceLocation::Youtube { .. } => true,
        // NDI bypasses libav entirely (a host-memory receive), so it never opens a
        // libav input and gets no libav options.
        #[cfg(feature = "ndi")]
        SourceLocation::Ndi { .. } => false,
        SourceLocation::Path(_) | SourceLocation::Synthetic(_) => false,
    };
    if is_network {
        // `rw_timeout` is expressed in microseconds; libav copies the strings.
        opts.set("rw_timeout", &INGEST_RW_TIMEOUT.as_micros().to_string());
    }
    // HLS open-time hardening (ADR-T011). The ABC-News-AU footgun is a master
    // playlist with a `TYPE=SUBTITLES` WebVTT rendition that libav folds into the
    // one shared context, so a corrupt/404/expired `.vtt` aborts the open or fails
    // the whole read. The PRIMARY guard is the variant-pin pre-open
    // ([`main_demuxer_open_url`]/[`resolve_hls_variant_url`]): the main demuxer is
    // pointed at one VIDEO variant media playlist (no SUBTITLES rendition), so
    // libav never fetches the `.vtt` on EITHER FFmpeg 7.x or 8.x. These knobs are
    // additional hardening (they also apply to the pinned variant playlist):
    //   * `seg_max_retry` — a transient SEGMENT fetch failure retries instead of
    //     failing the open.
    //   * `protocol_whitelist` — a sane set so an HTTPS playlist + AES (`crypto`)
    //     segments open, without admitting `file`/`concat`-style surprises beyond
    //     the standard HLS surface.
    //   * `strict=normal` — DEFENCE-IN-DEPTH ONLY. On FFmpeg 7.x `hls.c`
    //     `new_rendition` drops a SUBTITLES rendition pre-probe when
    //     `strict_std_compliance > experimental`; FFmpeg **8.x REMOVED that gate**
    //     (the "avformat/hls: add WebVTT subtitle support" patch), so on the 8.x
    //     deploy target this no longer stops the rendition fetch — the variant-pin
    //     is what makes the fix 8.x-robust. We never widen `allowed_extensions` to
    //     admit `.vtt`: the isolated `caption_loop` reader is the sole WebVTT path.
    if is_hls_location(location) {
        opts.set("strict", "normal");
        opts.set("seg_max_retry", "8");
        opts.set("protocol_whitelist", "file,http,https,tcp,tls,crypto,data");
    }
    opts
}

/// Whether `location` opens an HLS master playlist (a `.m3u8` URL, or a resolved
/// `YouTube` master — which libav opens as HLS too). Detected on the URL string so
/// the HLS open-time hardening in [`ingest_open_options`] applies (ADR-T011).
fn is_hls_location(location: &SourceLocation) -> bool {
    let url = match location {
        SourceLocation::Url(u) => u.as_str(),
        // A resolved YouTube watch URL is opened as a `*.googlevideo.com` HLS
        // master; treat it as HLS for the open-time hardening.
        #[cfg(feature = "youtube")]
        SourceLocation::Youtube { .. } => return true,
        // A local `.m3u8` file is also an HLS playlist.
        SourceLocation::Path(p) => p.to_str().unwrap_or(""),
        #[cfg(feature = "ndi")]
        SourceLocation::Ndi { .. } => return false,
        SourceLocation::Synthetic(_) => return false,
    };
    // An `.m3u8` (with or without a query string) is an HLS playlist.
    let base = url.split(['?', '#']).next().unwrap_or(url);
    base.to_ascii_lowercase().ends_with(".m3u8")
}

/// Resolve an HLS **master** playlist URL to the **video variant media-playlist**
/// URL the main demuxer should open instead (ADR-T011, the `FFmpeg`-8.x-robust
/// fix).
///
/// THE fix for the ABC-News-AU footgun on `FFmpeg` 8.x: opening the *master*
/// playlist lets libav's HLS demuxer surface the `TYPE=SUBTITLES` `WebVTT`
/// rendition into the one shared `AVFormatContext`. `FFmpeg` 7.x dropped that
/// rendition at parse when `strict_std_compliance > experimental` (the
/// `strict=normal` gate), but **`FFmpeg` 8.x removed that gate** (the "avformat/hls:
/// add `WebVTT` subtitle support" patch), so 8.x ALWAYS tries to load the
/// rendition's first
/// `.vtt` segment — which is broken/404/expired on this source — and
/// `avformat_open_input` ABORTS before any post-open discard can run. Pinning the
/// main demuxer to ONE video variant media playlist (which carries no SUBTITLES
/// rendition) stops libav from ever fetching the `.vtt`, on both 7.x and 8.x. The
/// isolated `caption_loop` reader remains the SOLE `WebVTT` path (it fetches the
/// `.vtt` rendition on its own context — a broken `.vtt` cannot abort the video).
///
/// Returns `Some(variant_url)` when `master_url` is a master playlist with at least
/// one `#EXT-X-STREAM-INF` variant; the variant is chosen for `target_height`
/// (decode-at-display-resolution, invariant #6). Returns `None` — leave the URL
/// unchanged — when the URL is already a media playlist (no variants), or the
/// master cannot be fetched/parsed (best-effort: the open then falls back to the
/// original URL, with the post-open discard + reconnect bracket as the remaining
/// guards; this never fails the build, invariants #1/#10).
fn resolve_hls_variant_url(master_url: &str, target_height: Option<u32>) -> Option<String> {
    resolve_hls_variant_url_with(master_url, target_height, &crate::captions::LibavFetcher)
}

/// [`resolve_hls_variant_url`] with an injectable [`PlaylistFetcher`](crate::captions::PlaylistFetcher)
/// — the fetch→parse→pick→resolve seam, exercised offline in tests with canned
/// master bytes (no network, no FFI).
fn resolve_hls_variant_url_with(
    master_url: &str,
    target_height: Option<u32>,
    fetcher: &dyn crate::captions::PlaylistFetcher,
) -> Option<String> {
    let playlist = match fetcher.fetch(master_url) {
        Ok(playlist) => playlist,
        Err(reason) => {
            tracing::warn!(%master_url, %reason, "could not fetch HLS master for variant pin; opening URL as-is");
            return None;
        }
    };
    let master = match multiview_input::hls::MasterPlaylist::parse(&playlist.body) {
        Ok(master) => master,
        Err(err) => {
            tracing::warn!(%master_url, error = %err, "HLS master parse failed for variant pin; opening URL as-is");
            return None;
        }
    };
    // A media playlist (no variants) has nothing to pin — open the URL as-is.
    let variant = master.pick_video_variant(target_height)?;
    // Resolve the (usually relative) variant URI against the master's EFFECTIVE
    // (post-redirect) URL — a redirecting/CDN-fronted master (c.mjh.nz -> a signed
    // Akamai master with relative variant URIs) serves children that only resolve
    // under the final base, not the requested one (RFC 3986 §5 / RFC 8216).
    // Resolving against the requested origin yields a 404 that aborts the ingest.
    // For a non-redirecting fetch the effective URL equals the requested URL.
    let variant_url = crate::captions::resolve_rendition_uri(&playlist.url, &variant.uri);
    tracing::info!(
        %master_url,
        effective_url = %playlist.url,
        %variant_url,
        target_height = ?target_height,
        variant_height = ?variant.resolution_height,
        "pinned HLS main demuxer to a video variant (WebVTT rendition isolation)"
    );
    Some(variant_url)
}

/// The URL the main video/audio demuxer should actually open for `url`.
///
/// For an HLS location ([`is_hls_location`]) this is the variant media-playlist URL
/// resolved by [`resolve_hls_variant_url`] (so the master's `WebVTT` rendition is
/// never folded into the main demuxer's shared context — ADR-T011); the
/// `target_height` is the displayed tile height (decode-at-display-resolution,
/// invariant #6). For any non-HLS URL, or when the variant resolve is a no-op (the
/// URL is already a media playlist, or the master could not be fetched/parsed), the
/// original `url` is returned unchanged. Best-effort: this never fails the open
/// (invariants #1/#10) — a fall-through to the original URL still has the post-open
/// discard + reconnect bracket as guards.
fn main_demuxer_open_url(url: &str, location: &SourceLocation, tile_h: u32) -> String {
    if !is_hls_location(location) {
        return url.to_owned();
    }
    resolve_hls_variant_url(url, Some(tile_h)).unwrap_or_else(|| url.to_owned())
}

/// The per-source streaming-ingest loop, run on a dedicated thread (BUG-2 fix).
///
/// Opens the source, decodes its best video stream to NV12 scaled to the tile
/// size, and **publishes each frame into the store as it is decoded** — paced to
/// wall-clock by the frame's PTS (invariant #4; `-re` is never used). Returns
/// when the `stop` flag is raised (a bounded/`stop`ped run tearing ingest down)
/// or — for a finite source — when the stream ends. A `live` source reconnects on
/// EOF/error after a [`reconnect_backoff`] wait (capped-exponential + per-source
/// jitter): consecutive fast failures escalate the wait up to
/// [`INGEST_RECONNECT_CAP`], while a connection that streamed for at least
/// [`INGEST_RECONNECT_HEALTHY`] resets the escalation, so a transient HLS/RTSP
/// drop recovers promptly and a hard-down source never hot-loops. The tile holds
/// its last-good frame meanwhile (invariant #2). The loop only ever *writes* the
/// lock-free store, so it can neither pace nor stall the output clock
/// (invariant #1) nor back-pressure the engine (invariant #10).
fn ingest_loop(plan: &IngestPlan, stop: &AtomicBool) {
    // Synthetic sources (bars/solid/clock) render in-process — no decode, no
    // reconnect; the generator publishes into the store at cadence until `stop`.
    if let SourceLocation::Synthetic(kind) = &plan.location {
        crate::synth::generator_loop(
            kind.clone(),
            &plan.store,
            plan.tile_w,
            plan.tile_h,
            plan.canvas_color,
            plan.cadence,
            stop,
        );
        return;
    }
    // NDI ingest is a host-memory receive that bypasses libav: route it to its own
    // supervised drive loop (over `multiview-input`'s runtime-loaded receive seam),
    // not `open_and_stream`. Like every ingest path it only writes the last-good
    // store and reconnects on fault — it never paces or stalls the output clock
    // (invariants #1/#2/#10). Only under the off-by-default `ndi` feature.
    #[cfg(feature = "ndi")]
    if let SourceLocation::Ndi { name } = &plan.location {
        drive_ndi(plan, name, stop);
        return;
    }
    let tag = CanvasColor::default().output_tag();
    let mut attempt: u32 = 0;
    let mut jitter = JitterRng::seeded(&plan.id);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        match open_and_stream(plan, tag, stop) {
            Ok(()) => {}
            Err(reason) => {
                tracing::warn!(source = %plan.id, %reason, "ingest stream ended/errored");
            }
        }
        // How long THIS connection streamed (before any backoff) — feeds the
        // healthy-reset decision below.
        let ran_for = started.elapsed();
        if !plan.live || stop.load(Ordering::Acquire) {
            // A finite source has played out (its tile now holds its last-good
            // frame forever); a stop was requested. Either way, this thread ends.
            return;
        }
        // Live source: update the escalation from THIS connection's health FIRST
        // — a connection that streamed for at least INGEST_RECONNECT_HEALTHY resets
        // to attempt 0 so it reconnects promptly even after an earlier bad patch; a
        // fast failure escalates the index — THEN wait the resulting
        // capped-exponential, jittered backoff (checking `stop` in slices so
        // teardown stays prompt) and reconnect.
        attempt = next_reconnect_attempt(attempt, ran_for);
        let nap = reconnect_backoff(attempt, jitter.next_unit());
        tracing::debug!(source = %plan.id, attempt, ?nap, "reconnecting live source after backoff");
        sleep_interruptible(nap, stop);
    }
}

/// Drive an NDI source: a host-memory receive that bypasses libav (ADR-0008 /
/// IN-3).
///
/// Probes the runtime-loaded NDI seam ([`multiview_input::ndi::NdiCapability`]).
/// When the runtime is **absent** (the default/CI case) or a receiver cannot be
/// connected, the tile is **degraded, never hung**: this returns promptly so the
/// tile rides its `NO_SIGNAL` placeholder via the store policy and the startup
/// prime-wait is never extended (invariant #1's hard upper bound at
/// [`PRIME_WAIT_BUDGET`]). When a receiver **is** available, each received frame is
/// converted to NV12 and published into the last-good store via the
/// [`NdiProducer`](multiview_input::ndi::NdiProducer); the supervised-reconnect
/// bracket re-enters on a receive fault. The loop only ever *writes* the lock-free
/// store, so it can neither pace nor stall the output clock (invariant #1) nor
/// back-pressure the engine (invariant #10).
///
/// HONEST SCOPE: binding a live SDK-backed
/// [`NdiReceiver`](multiview_input::ndi::NdiReceiver) onto the resolved
/// `multiview-ndi-sys` function table is a **live-only** concern (it needs the
/// proprietary SDK ABI + a running NDI network, neither in CI) and is the deferred
/// half — [`connect_ndi_receiver`] reports the runtime status and returns no live
/// receiver yet, so an NDI tile currently degrades to `NO_SIGNAL` rather than
/// streaming. The pure receive→NV12 conversion + the `NdiProducer` drive shape are
/// fully unit-tested in `multiview-input` over an injected fake receiver.
#[cfg(feature = "ndi")]
fn drive_ndi(plan: &IngestPlan, name: &str, stop: &AtomicBool) {
    let mut attempt: u32 = 0;
    let mut jitter = JitterRng::seeded(&plan.id);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        match connect_ndi_receiver(name) {
            Ok(receiver) => {
                let mut producer = multiview_input::ndi::NdiProducer::new(receiver);
                drive_ndi_producer(plan, &mut producer, stop);
            }
            Err(status) => {
                // Runtime absent / unusable / no live receiver yet: log once and let
                // the tile degrade. We do NOT spin — the reconnect backoff below
                // bounds retry frequency, and `stop`/prime-wait are never blocked.
                tracing::warn!(
                    source = %plan.id,
                    ndi_source = name,
                    ?status,
                    "ndi receive unavailable; tile will degrade (LIVE->...->NO_SIGNAL)"
                );
            }
        }
        let ran_for = started.elapsed();
        if !plan.live || stop.load(Ordering::Acquire) {
            return;
        }
        attempt = next_reconnect_attempt(attempt, ran_for);
        let nap = reconnect_backoff(attempt, jitter.next_unit());
        tracing::debug!(source = %plan.id, attempt, ?nap, "reconnecting ndi source after backoff");
        sleep_interruptible(nap, stop);
    }
}

/// Pump an [`NdiProducer`](multiview_input::ndi::NdiProducer) into `plan.store`
/// until the receiver faults, signals end-of-stream, or `stop` is raised.
///
/// Each received frame is sampled (non-blocking), converted to an [`Nv12Image`],
/// and published into the last-good store stamped with a wall-clock-relative
/// instant (NDI carries no output cadence; the engine's output clock paces
/// emission — inputs are sampled, never pacing, invariants #1/#2). A quiet sample
/// (`Ok(None)`: no frame this instant) yields briefly and re-polls rather than
/// spinning. A receive fault returns so the supervised-reconnect bracket re-enters.
#[cfg(feature = "ndi")]
fn drive_ndi_producer(
    plan: &IngestPlan,
    producer: &mut multiview_input::ndi::NdiProducer,
    stop: &AtomicBool,
) {
    use multiview_input::source::FrameProducer;

    let start = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match producer.next_frame() {
            Ok(Some(frame)) => {
                let Some(image) = ndi_host_to_image(&frame, plan) else {
                    // A malformed frame is dropped (never panics, never stalls); the
                    // tile holds its last-good frame.
                    continue;
                };
                // NDI frames arrive in real time; stamp publish with the
                // wall-clock-relative elapsed instant so the latch-on-tick read
                // advances. The output clock — not this stamp — paces emission.
                let at = MediaTime::from_nanos(
                    i64::try_from(start.elapsed().as_nanos()).unwrap_or(i64::MAX),
                );
                plan.store.publish(image, at);
            }
            // No frame ready this sample: re-poll after a short, `stop`-aware nap so
            // the loop never busy-spins on a quiet source.
            Ok(None) => {
                sleep_interruptible(Duration::from_millis(5), stop);
            }
            Err(err) => {
                tracing::warn!(source = %plan.id, error = %err, "ndi receive faulted");
                return;
            }
        }
    }
}

/// Bridge one NDI-received [`ProducedFrame`](multiview_input::source::ProducedFrame)
/// (NV12 host bytes: a `w*h` Y plane followed by a `w*h/2` interleaved Cb,Cr
/// plane) into the CLI's [`Nv12Image`] store payload, splitting the concatenated
/// bytes back into the two planes. Returns `None` (panic-free) if the geometry or
/// plane lengths are inconsistent — the caller drops the frame.
#[cfg(feature = "ndi")]
fn ndi_host_to_image(
    frame: &multiview_input::source::ProducedFrame,
    _plan: &IngestPlan,
) -> Option<Nv12Image> {
    let w = frame.meta.width;
    let h = frame.meta.height;
    let y_len = usize::try_from(w)
        .ok()?
        .checked_mul(usize::try_from(h).ok()?)?;
    let uv_len = y_len / 2;
    let total = y_len.checked_add(uv_len)?;
    if frame.pixels.len() != total {
        return None;
    }
    let (y_plane, uv_plane) = frame.pixels.split_at(y_len);
    Nv12Image::new(w, h, y_plane.to_vec(), uv_plane.to_vec(), frame.meta.color).ok()
}

/// Attempt to connect a live NDI receiver for source `name`.
///
/// HONEST SCOPE (live-only deferred half): binding a real
/// [`NdiReceiver`](multiview_input::ndi::NdiReceiver) onto the resolved
/// `multiview-ndi-sys` function table needs the proprietary SDK ABI + a running NDI
/// network — neither exists in CI — so this currently probes the runtime and
/// returns the typed status without a live receiver. With the runtime absent (the
/// default case) it reports the unavailable status so the tile degrades; even with
/// the runtime present, the live receiver binding is not yet wired (the converter +
/// `NdiProducer` drive shape that *consume* a receiver are complete and tested in
/// `multiview-input`). It never panics or blocks.
#[cfg(feature = "ndi")]
fn connect_ndi_receiver(
    _name: &str,
) -> Result<Box<dyn multiview_input::ndi::NdiReceiver + Send>, multiview_input::ndi::NdiLoadStatus>
{
    let status = multiview_input::ndi::NdiCapability::probe();
    // Whether available or not, the live SDK-backed receiver binding is the
    // deferred half. Surface the probe status (RuntimeNotFound when absent, or
    // Available when present) so the drive loop logs the honest reason and the tile
    // degrades rather than streaming a non-existent receiver.
    Err(status)
}

/// Map one decoded frame onto the timeline the store is stamped with.
///
/// Routes the frame's **raw** source-tick PTS through the per-input
/// [`PtsNormalizer`](multiview_input::normalize::PtsNormalizer): unwrap a 33-bit
/// MPEG-TS wrap, synthesize a genpts fallback from the declared cadence when the
/// raw PTS is absent, smooth a discontinuity, and enforce a strict monotonic
/// guard — yielding one clean nanosecond timeline (invariant #3). Anchoring at
/// `master_now = 0` places the first frame at the source-relative origin; the
/// wall-clock pacer handles real-time release and `publish_time` stamps
/// source-relative media time for latch-on-tick.
///
/// A degenerate input timebase (a zero denominator — impossible for a real
/// opened stream) makes [`PtsNormalizer::normalize`] error; that case falls back
/// to `fallback` (the decoder's own rescaled PTS) so ingest never drops a frame.
fn timeline_pts(
    normalizer: &mut multiview_input::normalize::PtsNormalizer,
    raw_pts: Option<i64>,
    fallback: MediaTime,
) -> MediaTime {
    normalizer.normalize(raw_pts, 0).unwrap_or(fallback)
}

/// Resolve a `YouTube` **watch** URL to a fresh live HLS master URL (ADR-0015),
/// for opening by the standard HLS ingest path.
///
/// This is the CLI's wiring seam onto [`multiview_input::youtube`]: it spawns the
/// runtime-discovered `yt-dlp` resolver under its hard timeout and returns the
/// resolved `*.googlevideo.com` manifest URL ([`ingest_url`](multiview_input::youtube::reresolve::ingest_url)).
/// It runs on the calling **ingest thread** (a `std::thread`, the control/IO
/// plane), so it spins up a small current-thread Tokio runtime to drive the async
/// resolver; the output data plane is never involved (invariants #1/#10). A
/// resolve failure (binary absent, timeout, extraction broke, or the stream is not
/// live) is returned as a `String` error — the caller's reconnect bracket backs
/// off and retries while the tile degrades; it is never a panic.
#[cfg(feature = "youtube")]
fn resolve_youtube_master(watch_url: &str) -> Result<String, String> {
    use multiview_input::youtube::reresolve::{ingest_url, ProcessResolver, Resolver};

    // A dedicated current-thread runtime for this one resolve. `yt-dlp`'s own hard
    // timeout (ResolverConfig::default) bounds the await, so this never blocks the
    // ingest thread indefinitely; the reconnect backoff bounds retry frequency.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("youtube resolver runtime: {e}"))?;
    let process = ProcessResolver::default();
    let master = runtime
        .block_on(process.resolve(watch_url))
        .map_err(|e| e.to_string())?;
    Ok(ingest_url(&master).to_owned())
}

/// Open `plan.location`, decode its best video stream to NV12 scaled to the tile
/// size, and publish each frame into `plan.store` paced to wall-clock by PTS.
///
/// Returns `Ok(())` at clean EOF (a finite source played out), or `Err` on an
/// open/decode error. Returns early (still `Ok`) the moment `stop` is observed.
///
/// Uses `ffmpeg-next`'s safe `Input`/`Parameters` value types only to bridge the
/// container's stream parameters into `multiview-ffmpeg`'s safe `StreamVideoDecoder`
/// (which `multiview-ffmpeg`'s `Demuxer` does not yet surface). No `unsafe`, no FFI.
fn open_and_stream(
    plan: &IngestPlan,
    tag: multiview_core::color::ColorInfo,
    stop: &AtomicBool,
) -> Result<(), String> {
    multiview_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    // A YouTube source resolves its watch URL to a FRESH `*.googlevideo.com` HLS
    // master here, on every (re)connect (ADR-0015): the resolved master is
    // time-limited and 403s at its `expire` deadline, so re-resolving on each open
    // — driven by the existing reconnect bracket in `ingest_loop` — is what keeps
    // the tile alive across the ~6 h expiry. Resolution runs on THIS ingest thread
    // (the control/IO plane) under a hard timeout (a hung `yt-dlp` is killed, never
    // awaited); it never touches the output data plane (invariants #1/#10). A
    // resolve failure returns `Err`, which the reconnect bracket backs off and
    // retries while the tile rides last-good → NO_SIGNAL.
    #[cfg(feature = "youtube")]
    let resolved_youtube_url = match &plan.location {
        SourceLocation::Youtube { watch_url } => Some(resolve_youtube_master(watch_url)?),
        _ => None,
    };

    // Network sources open with an `rw_timeout` so a dead live source fails fast
    // instead of hanging this decode thread on open or a blocking read (#45).
    let opts = ingest_open_options(&plan.location);
    let mut input = match &plan.location {
        SourceLocation::Path(p) => {
            ffmpeg::format::input_with_dictionary(p, opts).map_err(|e| e.to_string())?
        }
        SourceLocation::Url(u) => {
            // HLS WebVTT-rendition isolation (ADR-T011, FFmpeg-8.x-robust): if this
            // is an HLS master, pin the main demuxer to one VIDEO VARIANT media
            // playlist so libav never fetches the master's `TYPE=SUBTITLES` WebVTT
            // rendition (8.x dropped the `strict` gate, so a broken `.vtt` would
            // otherwise ABORT `avformat_open_input` before any post-open discard).
            // The resolve runs on THIS ingest thread (control/IO plane) and is
            // best-effort — a fetch/parse miss falls back to the original URL with
            // the post-open discard + reconnect bracket as the remaining guards
            // (invariants #1/#10). A non-master / non-HLS URL is unchanged.
            let open_url = main_demuxer_open_url(u.as_str(), &plan.location, plan.tile_h);
            ffmpeg::format::input_with_dictionary(&open_url.as_str(), opts)
                .map_err(|e| e.to_string())?
        }
        // A YouTube source opens the manifest URL resolved above (a network HLS
        // master) exactly like any other URL — variant-pinned the same way so its
        // master's WebVTT rendition (if any) is never folded into the main context.
        #[cfg(feature = "youtube")]
        SourceLocation::Youtube { .. } => {
            // `resolved_youtube_url` is `Some` for this arm (set just above for the
            // same `Youtube` location); a `let-else` keeps it panic-free.
            let Some(url) = resolved_youtube_url.as_deref() else {
                return Err("youtube source did not resolve a manifest url".to_owned());
            };
            let open_url = main_demuxer_open_url(url, &plan.location, plan.tile_h);
            ffmpeg::format::input_with_dictionary(&open_url.as_str(), opts)
                .map_err(|e| e.to_string())?
        }
        // Unreachable: `ingest_loop` routes synthetic sources to the generator
        // before opening any media. Guarded so the match stays exhaustive.
        SourceLocation::Synthetic(_) => {
            return Err("synthetic source has no media to open".to_owned())
        }
        // Unreachable: `ingest_loop` routes NDI sources to `drive_ndi` (a
        // host-memory receive) before reaching `open_and_stream`. Guarded so the
        // match stays exhaustive.
        #[cfg(feature = "ndi")]
        SourceLocation::Ndi { .. } => {
            return Err("ndi source has no libav media to open".to_owned())
        }
    };

    let (stream_index, params, time_base, declared_fps) = best_video_stream_params(&input)?;

    // Prefer NVDEC hardware decode (`*_cuvid`) so 4K H.264/HEVC decode runs on
    // the GPU ASIC instead of the CPU (efficiency). The selection is gated by the
    // `cuda` feature + a registered cuvid wrapper + a working GPU; a per-deploy
    // env opt-out (`MULTIVIEW_DISABLE_NVDEC`) forces software. On a GPU-free box
    // or any hardware-open failure this degrades to software decode gracefully —
    // the tile keeps running (invariants #1/#2). Decoded CUDA surfaces are
    // downloaded to host NV12 inside the decoder (the budgeted CPU↔GPU copy), so
    // the rest of the pipeline is unchanged (invariant #5).
    let want_hw = multiview_ffmpeg::want_hw_decode(
        std::env::var(multiview_ffmpeg::NVDEC_DISABLE_ENV)
            .ok()
            .as_deref(),
    );
    // Pin NVDEC to the load-aware admission pick's CUDA ordinal so decode opens on
    // the SAME physical GPU the compositor was pinned to — the whole pipeline
    // follows one chosen device (affinity; ADR-0035 Tier-1 / the GPU-placement
    // principle). `None` (no admission pick / GPU-free) selects libav's default
    // CUDA device, in lockstep with the compositor's `None`.
    let cuda_ordinal = plan.cuda_ordinal.as_deref();
    let (decoder, used_hw) =
        StreamVideoDecoder::new_preferring_hw(params, time_base, want_hw, cuda_ordinal)
            .map_err(|e| e.to_string())?;
    // Feed the declared cadence so the decoder's genpts fallback advances at the
    // source's true rate (PAL 25, film 24, …) rather than an NTSC-shaped guess;
    // an unusable rate is ignored inside `with_declared_fps` (invariant #3).
    let mut decoder = decoder.with_declared_fps(Some(declared_fps));
    tracing::info!(
        source = %plan.id,
        hardware = used_hw,
        decoder = decoder.hw_decoder_name().unwrap_or("software"),
        "opened video decoder"
    );
    let mut to_tile = TileScaler::new(plan.tile_w, plan.tile_h);

    // Per-input PTS normalizer (invariant #3, the unified timing model): unwrap a
    // 33-bit MPEG-TS wrap, synthesize a genpts fallback from the declared cadence
    // when a frame carries no PTS, smooth discontinuities, and enforce a strict
    // monotonic guard — producing one clean nanosecond timeline before pace/publish.
    // Every CLI ingest source is MPEG-TS-family: the live transports
    // (RTSP/HLS/TS/SRT/RTMP) carry 33-bit PTS, and file containers carry continuous
    // 64-bit PTS that the 33-bit delta-unwrap passes through unchanged. (No raw
    // `rtp://` 32-bit source kind exists in the CLI path yet, so `Rtp32` is unused.)
    let mut normalizer = multiview_input::normalize::PtsNormalizer::new(
        multiview_input::normalize::WrapBits::Mpeg33,
        time_base,
        declared_fps,
    );

    // Build the in-container subtitle decoder once, from the SAME open container's
    // subtitle stream parameters — DVB-sub bitmap or `ass`/`subrip`/`mov_text`
    // text (#36 Phase 2 + SUR-3c). Its packets are pumped as a sibling of the
    // video packets below — they never go through the video `receive_frame` pump.
    // A build failure logs and disables the route for this open (best-effort; the
    // video still streams). Only under `overlay`.
    #[cfg(feature = "overlay")]
    let mut incontainer_sub = build_incontainer_sub_decoder(plan, &input);

    // Build the embedded CEA-608 (`cc_dec`) decoder once, bound to the VIDEO
    // stream's time-base (the A53 cc-data rides on the video frames, so its PTS is
    // the video PTS). The recovered text cues are published as each decoded frame's
    // A53 side data completes a caption (SUR-3c). A build failure logs and disables
    // the route for this open (best-effort). Only under `overlay`.
    #[cfg(feature = "overlay")]
    let mut embedded_cc = build_embedded_cc_decoder(plan, time_base);

    // HLS WebVTT-rendition isolation (ADR-T011), DEFENCE-IN-DEPTH: the variant-pin
    // pre-open ([`main_demuxer_open_url`]) already stops the master's SUBTITLES
    // rendition from ever reaching this context. This discards any unrouted
    // subtitle stream before the first read regardless — harmless when the pin
    // succeeded (no subtitle stream to discard), and a backstop on the fall-through
    // path (master unfetchable / a non-master playlist that still folds in a
    // subtitle). See the fn doc for the footgun + the routed-keep.
    discard_unrouted_subtitle_streams(plan, &mut input);

    // The wall-clock pacer: maps the first frame's PTS to "now" and releases
    // each subsequent frame when wall-clock catches up to its PTS (invariant #4).
    let mut pacer = PtsWallClock::new();

    // Pump packets, publishing each decoded+scaled frame into the store.
    let mut drained = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        while let Some(decoded) = decoder.receive_frame().map_err(|e| e.to_string())? {
            // Embedded CEA-608: feed this frame's A53 cc-data side data (extracted
            // off the RAW decoded frame before NV12 conversion) to `cc_dec`,
            // anchored at the frame's raw stream PTS. Most frames carry none (a
            // no-op); a decode error on one frame is logged and skipped — embedded
            // captions are intermittent and must never stall ingest (#1/#2/#10).
            #[cfg(feature = "overlay")]
            pump_embedded_cc(plan, embedded_cc.as_mut(), &decoded);
            let image = to_tile.convert(&decoded.frame, tag)?;
            // Normalize the RAW source-tick PTS onto the unified monotonic
            // nanosecond timeline (invariant #3) before pacing/publish; a frame
            // with no usable PTS or a degenerate timebase falls back to the
            // decoder's own rescaled time so ingest never drops a frame.
            let pts = timeline_pts(&mut normalizer, decoded.raw_pts, decoded.meta.pts);
            // Pace to that PTS (invariant #4) so a file/VOD source is not slurped
            // into the ring faster than real time, then publish it stamped with
            // its SOURCE-RELATIVE media time — the timeline the output clock
            // latches against (latch-on-tick; see `publish_time`). Re-check `stop`
            // after the (possibly long) pace wait.
            pacer.wait_for(pts, stop);
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            plan.store.publish(image, pacer.publish_time(pts));
        }
        if drained {
            return Ok(());
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() == stream_index {
                    decoder.send_packet(&packet).map_err(|e| e.to_string())?;
                } else {
                    // A non-video packet: route it to the in-container subtitle
                    // decoder (DVB-sub bitmap or text) if it belongs to that
                    // subtitle stream (sibling branch — it never goes through the
                    // video `receive_frame` pump). A decode error on one cue is
                    // logged and skipped: captions are intermittent and must never
                    // stall ingest.
                    #[cfg(feature = "overlay")]
                    pump_incontainer_sub(plan, incontainer_sub.as_mut(), &packet);
                }
            }
            Err(ffmpeg::Error::Eof) => {
                decoder.send_eof().map_err(|e| e.to_string())?;
                drained = true;
            }
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// Resolve the best video stream's `(index, codec parameters, time-base,
/// declared fps)` from an opened `input`, or `Err` if the container has no video
/// stream. The codec [`Parameters`] and rationals are owned snapshots that borrow
/// nothing from `input`, so the demuxer can keep being read after this returns.
fn best_video_stream_params(
    input: &ffmpeg::format::context::Input,
) -> Result<(usize, ffmpeg::codec::Parameters, Rational, Rational), String> {
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| "input has no video stream".to_owned())?;
    Ok((
        stream.index(),
        stream.parameters(),
        multiview_ffmpeg::from_ff_rational(stream.time_base()),
        multiview_ffmpeg::from_ff_rational(stream.avg_frame_rate()),
    ))
}

/// Mark every UNROUTED subtitle stream in `input` `AVDISCARD_ALL` so libav stops
/// fetching it (HLS WebVTT-rendition isolation, ADR-T011).
///
/// If the open surfaced a `TYPE=SUBTITLES` `WebVTT` rendition into this shared
/// context (the ABC-News-AU footgun), a corrupt/404 `.vtt` would otherwise make
/// `av_read_frame` return that rendition's error for the WHOLE context, killing
/// the video tile. The main demuxer needs nothing from a `WebVTT` rendition — the
/// isolated `caption_loop` reader is the sole `WebVTT` path — so discarding it
/// loses no stream. A routed in-container DVB-sub stream (the MPEG-TS DVB-sub
/// route) is KEPT; audio renditions are never touched (the guard keys strictly on
/// `medium() == Subtitle`). Must be called before the first `packet.read()`:
/// libav's HLS `recheck_discard_flags` fires one-shot on the first packet.
fn discard_unrouted_subtitle_streams(
    plan: &IngestPlan,
    input: &mut ffmpeg::format::context::Input,
) {
    #[cfg(feature = "overlay")]
    let keep_subtitle = plan.incontainer_sub.as_ref().map(|d| d.stream_index);
    #[cfg(not(feature = "overlay"))]
    let keep_subtitle = None;
    let discarded = multiview_ffmpeg::discard_unrouted_subtitles(input, keep_subtitle);
    if discarded > 0 {
        tracing::info!(
            source = %plan.id,
            discarded,
            "discarded unrouted subtitle stream(s) in the main demuxer (HLS rendition isolation)"
        );
    }
}

/// Build the in-container subtitle [`CaptionDecoder`] for `plan`'s route (DVB-sub
/// bitmap or `ass`/`subrip`/`mov_text` text), from the open container's subtitle
/// stream parameters. Returns `None` when the source has no in-container route or
/// the decoder cannot be built (logged, best-effort).
#[cfg(feature = "overlay")]
fn build_incontainer_sub_decoder(
    plan: &IngestPlan,
    input: &ffmpeg::format::context::Input,
) -> Option<multiview_ffmpeg::CaptionDecoder> {
    let route = plan.incontainer_sub.as_ref()?;
    let params = input.stream(route.stream_index)?.parameters();
    match multiview_ffmpeg::CaptionDecoder::from_parameters(
        route.source.clone(),
        params,
        route.time_base,
    ) {
        Ok(dec) => Some(dec),
        Err(err) => {
            tracing::warn!(source = %plan.id, error = %err, "could not build in-container subtitle decoder; no in-container captions");
            None
        }
    }
}

/// Decode one packet on the in-container subtitle route (if the packet belongs to
/// that subtitle stream) and publish any cues (bitmap or text) into the route's
/// store.
#[cfg(feature = "overlay")]
fn pump_incontainer_sub(
    plan: &IngestPlan,
    decoder: Option<&mut multiview_ffmpeg::CaptionDecoder>,
    packet: &ffmpeg::codec::packet::Packet,
) {
    let (Some(route), Some(decoder)) = (plan.incontainer_sub.as_ref(), decoder) else {
        return;
    };
    if packet.stream() != route.stream_index {
        return;
    }
    match decoder.decode(packet) {
        Ok(cues) => crate::captions::publish_window_cues(&route.store, cues),
        Err(err) => {
            tracing::debug!(source = %plan.id, error = %err, "in-container subtitle packet decode error");
        }
    }
}

/// Build the embedded CEA-608 (`cc_dec`) [`CaptionDecoder`] for `plan`'s route,
/// bound to the video stream's `time_base` (the A53 cc-data shares the video
/// PTS). Returns `None` when the source has no embedded-CC route or the decoder
/// cannot be built (logged, best-effort).
#[cfg(feature = "overlay")]
fn build_embedded_cc_decoder(
    plan: &IngestPlan,
    video_time_base: Rational,
) -> Option<multiview_ffmpeg::CaptionDecoder> {
    let route = plan.embedded_cc.as_ref()?;
    match multiview_ffmpeg::CaptionDecoder::for_embedded(
        multiview_ffmpeg::CaptionSource::EmbeddedCc {
            channel: route.channel,
        },
        video_time_base,
    ) {
        Ok(dec) => Some(dec),
        Err(err) => {
            tracing::warn!(source = %plan.id, error = %err, "could not build embedded-CC decoder; no embedded captions");
            None
        }
    }
}

/// Pull the A53 cc-data side data off a decoded video `frame` and feed it to the
/// embedded-CC `cc_dec`, anchored at the frame's raw stream PTS, publishing any
/// recovered TEXT cues into the route's store. A frame with no A53 side data is a
/// no-op; a decode error is logged and skipped (captions are intermittent and
/// must never stall ingest — invariants #1/#2/#10).
#[cfg(feature = "overlay")]
fn pump_embedded_cc(
    plan: &IngestPlan,
    decoder: Option<&mut multiview_ffmpeg::CaptionDecoder>,
    frame: &multiview_ffmpeg::decode_stream::DecodedVideoFrame,
) {
    let (Some(route), Some(decoder)) = (plan.embedded_cc.as_ref(), decoder) else {
        return;
    };
    let Some(bytes) = frame.a53_cc.as_deref() else {
        return;
    };
    match decoder.decode_bytes(bytes, frame.raw_pts) {
        Ok(cues) => crate::captions::publish_window_cues(&route.store, cues),
        Err(err) => {
            tracing::debug!(source = %plan.id, error = %err, "embedded-CC A53 decode error");
        }
    }
}

/// Sleep up to `total`, waking early (in <= 50 ms slices) if `stop` is raised,
/// so ingest teardown stays prompt without a condvar.
pub(crate) fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(50);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let chunk = remaining.min(slice);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

/// A wall-clock pacer keyed on frame PTS (invariant #4 — pace live/VOD by PTS,
/// never `-re`).
///
/// On the first frame it anchors `base_instant = now` to `base_pts = pts`; each
/// later frame is released when `now - base_instant >= pts - base_pts`. A frame
/// whose PTS goes backwards (a discontinuity / wrap) re-anchors rather than
/// stalling, so a misbehaving source can never block ingest for long.
struct PtsWallClock {
    anchor: Option<(Instant, MediaTime)>,
}

impl PtsWallClock {
    fn new() -> Self {
        Self { anchor: None }
    }

    /// Block (in `stop`-checked slices) until wall-clock reaches `pts`'s release
    /// instant. The first call anchors the timeline and returns immediately.
    fn wait_for(&mut self, pts: MediaTime, stop: &AtomicBool) {
        let Some((base_instant, base_pts)) = self.anchor else {
            self.anchor = Some((Instant::now(), pts));
            return;
        };
        // A backwards PTS (discontinuity / wrap) re-anchors rather than stalls.
        if pts < base_pts {
            self.anchor = Some((Instant::now(), pts));
            return;
        }
        // Target media offset from the anchor.
        let delta = pts.saturating_sub(base_pts);
        let target_ns = u64::try_from(delta.as_nanos()).unwrap_or(0);
        let target = base_instant + Duration::from_nanos(target_ns);
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= target {
                return;
            }
            let remaining = target.saturating_duration_since(now);
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    /// The timeline instant to stamp a published frame with: the frame's
    /// **source-relative media time** (`pts - first_pts`), i.e. how far into the
    /// clip this frame sits, measured from the first decoded frame.
    ///
    /// This is the timeline the output clock samples against (latch-on-tick,
    /// streaming-gotchas §1): output tick `N` latches the source frame whose
    /// source-relative media time is nearest-but-not-after `N * tick_period`, so
    /// the tile advances exactly 1:1 with output media time regardless of how
    /// fast the producer decoded. Stamping wall-clock-elapsed here instead would
    /// re-introduce the race whenever the output loop runs slower than real time
    /// (the producer would have published far ahead of the output's own clock).
    ///
    /// A frame whose PTS precedes the anchor (a re-anchor case the caller already
    /// handles in [`Self::wait_for`]) clamps to zero rather than going negative.
    fn publish_time(&self, pts: MediaTime) -> MediaTime {
        match self.anchor {
            Some((_, base_pts)) => pts.saturating_sub(base_pts),
            None => MediaTime::ZERO,
        }
    }
}

/// Lazily-built scaler from a decoded NV12 frame's geometry to the tile size,
/// emitting an [`Nv12Image`] tagged like the canvas.
struct TileScaler {
    tile_w: u32,
    tile_h: u32,
    scaler: Option<Scaler>,
}

impl TileScaler {
    fn new(tile_w: u32, tile_h: u32) -> Self {
        Self {
            tile_w,
            tile_h,
            scaler: None,
        }
    }

    /// Scale `frame` (NV12 or P010 host frame) to the tile size as NV12 and wrap
    /// it as a tagged [`Nv12Image`].
    fn convert(
        &mut self,
        frame: &Video,
        tag: multiview_core::color::ColorInfo,
    ) -> Result<Nv12Image, String> {
        let src = ScaleSpec::new(frame.format(), frame.width(), frame.height());
        let dst = ScaleSpec::new(Pixel::NV12, self.tile_w, self.tile_h);
        let rebuild = match &self.scaler {
            Some(s) => s.source() != src || s.destination() != dst,
            None => true,
        };
        if rebuild {
            self.scaler = Some(Scaler::new(src, dst).map_err(|e| e.to_string())?);
        }
        let sws = self
            .scaler
            .as_mut()
            .ok_or_else(|| "tile scaler unexpectedly absent".to_owned())?;
        let resized = sws.run(frame).map_err(|e| e.to_string())?;
        video_to_nv12(&resized, tag)
    }
}

/// Convert a libav NV12 [`Video`] frame into a CPU-reference [`Nv12Image`],
/// copying its (possibly stride-padded) planes into tightly-packed plane vecs.
fn video_to_nv12(
    frame: &Video,
    tag: multiview_core::color::ColorInfo,
) -> Result<Nv12Image, String> {
    let w = frame.width();
    let h = frame.height();
    let wu = usize::try_from(w).map_err(|_| "width overflow".to_owned())?;
    let hu = usize::try_from(h).map_err(|_| "height overflow".to_owned())?;

    let mut y_plane = vec![0_u8; wu * hu];
    let mut uv_plane = vec![0_u8; wu * hu / 2];
    read_plane(&mut y_plane, frame.data(0), frame.stride(0), wu, hu)?;
    read_plane(&mut uv_plane, frame.data(1), frame.stride(1), wu, hu / 2)?;

    Nv12Image::new(w, h, y_plane, uv_plane, tag).map_err(|e| e.to_string())
}

/// Copy `rows` rows of `row_bytes` from a libav plane `src` (rows `src_stride`
/// apart) into a tightly-packed `dst`.
fn read_plane(
    dst: &mut [u8],
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), String> {
    if src_stride < row_bytes {
        return Err("libav plane stride is narrower than the row".to_owned());
    }
    for row in 0..rows {
        let src_start = row
            .checked_mul(src_stride)
            .ok_or_else(|| "src offset overflow".to_owned())?;
        let dst_start = row
            .checked_mul(row_bytes)
            .ok_or_else(|| "dst offset overflow".to_owned())?;
        let src_row = src
            .get(src_start..src_start + row_bytes)
            .ok_or_else(|| "src row out of range".to_owned())?;
        let dst_row = dst
            .get_mut(dst_start..dst_start + row_bytes)
            .ok_or_else(|| "dst row out of range".to_owned())?;
        dst_row.copy_from_slice(src_row);
    }
    Ok(())
}

#[cfg(all(test, feature = "overlay"))]
mod overlay_clock_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Build a `clock` overlay carrying `params` by deserializing JSON (the
    /// `#[non_exhaustive]` `Overlay` has no cross-crate struct literal).
    fn clock_overlay(params: serde_json::Value) -> multiview_config::Overlay {
        let mut obj = serde_json::Map::new();
        obj.insert("id".to_owned(), serde_json::json!("clk"));
        obj.insert("kind".to_owned(), serde_json::json!("clock"));
        obj.insert("target".to_owned(), serde_json::json!("canvas"));
        if let serde_json::Value::Object(extra) = params {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
        serde_json::from_value(serde_json::Value::Object(obj)).expect("clock overlay deserializes")
    }

    #[test]
    fn no_clock_overlay_yields_none() {
        assert!(analog_clock_from_config(&[], 1280, 720).is_none());
        // A digital clock overlay does NOT request the analog face.
        let digital = clock_overlay(serde_json::json!({ "face": "digital" }));
        assert!(analog_clock_from_config(&[digital], 1280, 720).is_none());
    }

    #[test]
    fn analog_face_param_requests_the_face() {
        let analog = clock_overlay(serde_json::json!({ "face": "analog" }));
        let spec = analog_clock_from_config(&[analog], 1280, 720)
            .expect("an analog clock overlay yields a spec");
        // Default placement is the bottom-right quadrant of the canvas.
        assert!(
            spec.cx() > 640.0 && spec.cy() > 360.0,
            "default to bottom-right: {spec:?}"
        );
        assert!(spec.radius() >= 8.0, "radius is sane");
    }

    #[test]
    fn explicit_placement_params_are_honoured() {
        let analog = clock_overlay(
            serde_json::json!({ "face": "analog", "x": 200, "y": 150, "radius": 64 }),
        );
        let spec = analog_clock_from_config(&[analog], 1280, 720).unwrap();
        assert!((spec.cx() - 200.0).abs() < 0.5, "explicit x honoured");
        assert!((spec.cy() - 150.0).abs() < 0.5, "explicit y honoured");
        assert!(
            (spec.radius() - 64.0).abs() < 0.5,
            "explicit radius honoured"
        );
    }
}

#[cfg(all(test, feature = "overlay"))]
mod fault_detector_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::overlays::TileFault;

    /// A 25/1 fps cadence (40 ms per tick), so the dwell windows below convert to
    /// a small, deterministic number of ticks.
    fn cadence() -> Rational {
        Rational { num: 25, den: 1 }
    }

    /// The output media instant of tick `i` at 25 fps (exact, integer ns).
    fn pts_of(i: u64) -> MediaTime {
        MediaTime::from_nanos(
            i64::try_from(i)
                .unwrap_or(i64::MAX)
                .saturating_mul(40_000_000),
        )
    }

    /// A solid 64x64 NV12 image at luma `y` (chroma neutral), canvas-tagged.
    fn solid(y: u8) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        Nv12Image::solid(64, 64, y, 128, 128, tag).expect("solid frame")
    }

    /// A 64x64 NV12 image whose luma carries a per-pixel gradient seeded by
    /// `seed`, so two frames with different seeds differ in (nearly) every sample
    /// — a genuinely *moving*, *bright* picture (never black, never frozen).
    fn moving(seed: u8) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![0_u8; 64 * 64];
        for (i, px) in y.iter_mut().enumerate() {
            // Bright base (well above the black threshold) + a seed-varying ramp.
            let ramp = u8::try_from(i % 200).unwrap_or(0);
            *px = 120u8
                .saturating_add(ramp / 4)
                .wrapping_add(seed.wrapping_mul(37));
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("moving frame")
    }

    /// A 64x64 NV12 frame for a **near-frozen, real-codec-like** source: a fixed
    /// bright base picture (Y=180) perturbed by two kinds of change keyed on the
    /// frame `tick`, simulating what the `FreezeProbe` actually sees off an encoded
    /// frozen feed (see the instrumentation in the commit message):
    ///
    ///  * every frame, a tiny ±2-level dither on a sparse set of samples — well
    ///    inside the probe's per-sample tolerance, so it reads as unchanged;
    ///  * every 25th frame (a simulated GOP boundary), a LARGER ±12-level shift on
    ///    ~1.5 % of samples — beyond the per-sample tolerance, so that single frame
    ///    spikes the changed-fraction above the freeze threshold.
    ///
    /// A correct detector must still raise FROZEN: the lone per-GOP spike must not
    /// reset the dwell. The pre-fix detector (engine-default probe, no debounce)
    /// would NOT — this is the regression this frame shape guards.
    fn near_frozen(tick: u64) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![180_u8; 64 * 64];
        // Per-frame tiny dither (±2, within tolerance) on every 7th sample, phase
        // by tick so it genuinely differs frame-to-frame but stays "unchanged".
        let phase = u8::try_from(tick % 2).unwrap_or(0);
        for (i, px) in y.iter_mut().enumerate() {
            if i % 7 == 0 {
                *px = if phase == 0 { 180 } else { 182 };
            }
        }
        // Simulated GOP boundary every 25 frames: a larger ±12 shift on a band of
        // samples (~1.6 % of the 4096) — a single-frame spike above tolerance.
        if tick % 25 == 0 && tick > 0 {
            for px in y.iter_mut().take(64) {
                *px = 168; // 180 - 12, beyond the ±6 tolerance
            }
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("near-frozen frame")
    }

    /// A 64x64 NV12 frame for a **barely-moving** source: a fixed bright base with
    /// a small moving band that changes by more than the per-sample tolerance on
    /// ~0.5 % of samples EVERY frame (above the 0.1 % freeze threshold), keyed on
    /// `tick`. This mimics the real silent demo source (a moving testsrc scaled to
    /// a tile: continuous small motion, instrumented median ~0.23 % changed). It
    /// must NOT be flagged frozen — it is the over-loosening guard for the freeze
    /// threshold (a 0.5 % threshold would wrongly call this frozen).
    fn barely_moving(tick: u64) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![160_u8; 64 * 64];
        // ~0.5 % of 4096 = ~20 samples flip between two well-separated values each
        // frame (diff 40 >> tolerance), giving a steady changed-fraction ~0.5 %.
        let bright = u8::try_from(tick % 2).unwrap_or(0) == 0;
        for px in y.iter_mut().take(20) {
            *px = if bright { 200 } else { 60 };
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("barely-moving frame")
    }

    /// Build a single-source store map keyed by `id`, holding-forever so a frozen
    /// source keeps reporting its last-good frame (matches the pipeline).
    fn store_for(id: &str) -> (HashMapStores, Arc<TileStore<Nv12Image>>) {
        let store = Arc::new(TileStore::new(
            id.to_owned(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let mut stores: HashMapStores = std::collections::HashMap::new();
        stores.insert(id.to_owned(), Arc::clone(&store));
        (stores, store)
    }

    /// The states map naming `id` as a LIVE cell-bound source (what `sample`
    /// iterates over).
    fn live_states(
        id: &str,
    ) -> std::collections::HashMap<String, multiview_core::traits::SourceState> {
        let mut s = std::collections::HashMap::new();
        s.insert(id.to_owned(), multiview_core::traits::SourceState::Live);
        s
    }

    /// A per-source override map naming `id`'s declared probes (M10).
    fn source_probes_for(
        id: &str,
        probes: &[multiview_config::probe::Probe],
    ) -> std::collections::HashMap<String, SourceProbeOverride> {
        let layout = Layout {
            name: "t".to_owned(),
            canvas: multiview_core::layout::Canvas {
                width: 64,
                height: 64,
                fps_num: 25,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: multiview_core::layout::FitMode::Contain,
                source: Some(id.to_owned()),
                ..Cell::default()
            }],
        };
        resolve_source_probes(&layout, probes)
    }

    #[test]
    fn declared_black_probe_threshold_and_dwell_drive_the_fault() {
        use multiview_config::probe::{DetectionZone, Dwell, Probe, ProbeKind};
        use multiview_core::alarm::PerceivedSeverity;
        let id = "blk";
        let (stores, store) = store_for(id);
        // The operator declares a black probe with a HIGH luma ceiling (100) and a
        // short 80 ms dwell-up (2 ticks at 25 fps). A Y=80 field is "black" under
        // this declared threshold but NOT under the hardcoded default (16), and it
        // must raise within ~2 ticks — far sooner than the default 0.5 s (12 ticks).
        let probe = Probe::new(
            "blk-probe",
            id,
            ProbeKind::black(100, DetectionZone::default()),
            Dwell::new(80, 80),
            PerceivedSeverity::Critical,
            false,
        );
        let mut det = FaultDetector::new(
            stores,
            std::collections::HashMap::new(),
            cadence(),
            source_probes_for(id, &[probe]),
        );
        let states = live_states(id);

        // Tick 0: present, dwell not yet served. Tick 1 (40 ms) still pending.
        store.publish(solid(80), pts_of(0));
        let t0 = det.sample(pts_of(0), 0, &states);
        assert_eq!(
            t0.get(id).copied().unwrap_or(TileFault::None),
            TileFault::None,
            "Y=80 under the declared threshold 100 is black, but the 80 ms dwell has not elapsed"
        );
        store.publish(solid(80), pts_of(1));
        let t1 = det.sample(pts_of(1), 1, &states);
        assert_eq!(
            t1.get(id).copied().unwrap_or(TileFault::None),
            TileFault::None,
            "still within the declared dwell-up at 40 ms"
        );
        // Tick 2 (80 ms): the declared dwell-up elapses → BLACK raises. The default
        // threshold (16) would NEVER call Y=80 black, so this proves the config
        // threshold is what drove the detection.
        store.publish(solid(80), pts_of(2));
        let t2 = det.sample(pts_of(2), 2, &states);
        assert_eq!(
            t2.get(id).copied(),
            Some(TileFault::Black),
            "the declared black probe (threshold 100, 80 ms dwell) raises on Y=80 at 80 ms"
        );
    }

    #[test]
    fn an_undeclared_source_keeps_the_default_black_threshold() {
        // Regression guard for the seam: a Y=80 field with NO declared probe is NOT
        // black under the hardcoded default threshold (16) — the override path must
        // not change undeclared sources. (A silence fault may fire from the absent
        // meter timeline — orthogonal default behaviour — so we assert specifically
        // that BLACK never raises, which is what the threshold drives.)
        let id = "blk";
        let (stores, store) = store_for(id);
        let mut det = FaultDetector::new(
            stores,
            std::collections::HashMap::new(),
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);
        for i in 0..40 {
            store.publish(solid(80), pts_of(i));
            let last = det.sample(pts_of(i), i, &states);
            assert_ne!(
                last.get(id).copied(),
                Some(TileFault::Black),
                "Y=80 is above the default black threshold (16) → no BLACK fault for an undeclared source"
            );
        }
    }

    #[test]
    fn sustained_all_black_frames_raise_a_black_fault() {
        let id = "blk";
        let (stores, store) = store_for(id);
        let mut det = FaultDetector::new(
            stores,
            std::collections::HashMap::new(),
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        // Drive 40 ticks (1.6 s > the 0.5 s black dwell) publishing an all-black
        // (Y=16) frame each tick so the tile stays LIVE and black.
        let mut last = std::collections::HashMap::new();
        for i in 0..40 {
            store.publish(solid(16), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Black),
            "an all-black source sustained past the dwell must raise a BLACK fault"
        );
    }

    #[test]
    fn sustained_identical_frames_raise_a_frozen_fault() {
        let id = "frz";
        let (stores, store) = store_for(id);
        let mut det = FaultDetector::new(
            stores,
            std::collections::HashMap::new(),
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        // Publish the SAME bright, non-black content every tick (Y=200 solid):
        // successive frames are identical → frozen. Drive 70 ticks (2.8 s > the
        // 2 s freeze dwell).
        let mut last = std::collections::HashMap::new();
        for i in 0..70 {
            store.publish(solid(200), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Frozen),
            "an unchanging bright source past the freeze dwell must raise a FROZEN fault"
        );
    }

    #[test]
    fn sustained_quiet_meter_raises_a_silent_fault() {
        let id = "sil";
        let (stores, store) = store_for(id);
        // A meter timeline pinned below the silence floor for every tick.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 80]);
        let mut det = FaultDetector::new(
            stores,
            timelines,
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        // Publish a MOVING, bright picture so neither black nor freeze fires —
        // only silence should. Drive 30 ticks (1.2 s > the 0.5 s silence dwell).
        let mut last = std::collections::HashMap::new();
        for i in 0..30 {
            store.publish(moving(u8::try_from(i % 251).unwrap_or(0)), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Silent),
            "a moving bright source with a sustained-quiet meter must raise a SILENT fault"
        );
    }

    #[test]
    fn moving_bright_loud_source_reports_no_fault() {
        let id = "ok";
        let (stores, store) = store_for(id);
        // A loud meter timeline (well above the silence floor) for every tick.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-6.0_f64; 80]);
        let mut det = FaultDetector::new(
            stores,
            timelines,
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        // Publish a MOVING, bright picture (changes every tick) and a loud meter:
        // no content fault should ever raise across a long run.
        for i in 0..70 {
            store.publish(moving(u8::try_from(i % 251).unwrap_or(0)), pts_of(i));
            let faults = det.sample(pts_of(i), i, &states);
            assert!(
                faults.get(id).copied().unwrap_or(TileFault::None) == TileFault::None,
                "a healthy moving+bright+loud source must never raise a fault (tick {i})"
            );
        }
    }

    #[test]
    fn near_frozen_source_with_gop_noise_still_raises_frozen() {
        // REGRESSION (commit 08bb78a defect): a genuinely frozen feed delivered as
        // ENCODED video is not byte-identical — each GOP boundary perturbs a small
        // fraction of luma. The first cut shipped the engine-default freeze probe
        // (0.1 % threshold, tol 2) with no debounce, so each per-GOP spike reset
        // the 2 s dwell and the FROZEN badge never appeared on the real source.
        // This drives the SAME near-frozen + per-GOP-spike shape and asserts the
        // tuned probe + debounce still raise FROZEN after the dwell.
        let id = "nearfrz";
        let (stores, store) = store_for(id);
        // Loud meter so silence never fires — freeze must be the only fault.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-6.0_f64; 120]);
        let mut det = FaultDetector::new(
            stores,
            timelines,
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        // Drive 90 ticks (3.6 s > the 2 s freeze dwell), crossing >=3 simulated
        // GOP boundaries (ticks 25/50/75) so the dwell must survive the spikes.
        let mut last = std::collections::HashMap::new();
        for i in 0..90 {
            store.publish(near_frozen(i), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Frozen),
            "a near-frozen encoded source (per-GOP noise spikes) must still raise FROZEN"
        );
    }

    #[test]
    fn barely_moving_quiet_source_is_silent_not_frozen() {
        // OVER-LOOSENING guard (the second-order defect found while fixing the
        // first): a source with small-but-continuous motion and a quiet meter is
        // SILENT, never FROZEN. An over-loose freeze threshold (e.g. 0.5 %) would
        // wrongly call this real moving-but-silent feed frozen.
        let id = "barely";
        let (stores, store) = store_for(id);
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 120]);
        let mut det = FaultDetector::new(
            stores,
            timelines,
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        let mut last = std::collections::HashMap::new();
        for i in 0..90 {
            store.publish(barely_moving(i), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Silent),
            "a barely-moving but quiet source must read SILENT, not FROZEN"
        );
    }

    #[test]
    fn black_takes_precedence_over_silence() {
        // A source that is BOTH black AND silent surfaces BLACK (the higher-
        // precedence content fault), not SILENT.
        let id = "both";
        let (stores, store) = store_for(id);
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 80]);
        let mut det = FaultDetector::new(
            stores,
            timelines,
            cadence(),
            std::collections::HashMap::new(),
        );
        let states = live_states(id);

        let mut last = std::collections::HashMap::new();
        for i in 0..40 {
            store.publish(solid(16), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Black),
            "black must outrank silence when both conditions hold"
        );
    }
}

/// Deterministic tests for the startup **prime-wait** ([`wait_for_prime`]): the
/// #40 fix that holds the very first output tick until every bound tile has
/// published its first frame — but only for a bounded budget, so a dead source
/// can never block startup (invariant #1/#2). The wait's clock + sleep are
/// injected, so these exercise the budget/poll behaviour with NO real sleeping.
#[cfg(test)]
mod prime_wait_tests {
    use std::cell::Cell as StdCell;
    use std::sync::Arc;
    use std::time::Duration;

    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_core::time::MediaTime;
    use multiview_engine::ManualTimeSource;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use super::{wait_for_prime, PRIME_WAIT_POLL};

    /// A tiny NV12 frame to publish so a store reads primed.
    fn frame() -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        Nv12Image::solid(2, 2, 16, 128, 128, tag).expect("solid nv12")
    }

    /// A fresh (cold, unprimed) tile store.
    fn store(id: &str) -> Arc<TileStore<Nv12Image>> {
        Arc::new(TileStore::new(
            id.to_owned(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ))
    }

    #[test]
    fn proceeds_immediately_when_all_stores_are_primed() {
        // Every store has already published a frame: the wait must return at once
        // (waited == 0), report all_primed, and never sleep.
        let a = store("a");
        let b = store("b");
        a.publish(frame(), MediaTime::from_nanos(0));
        b.publish(frame(), MediaTime::from_nanos(0));
        let stores = [&a, &b];

        let clock = ManualTimeSource::new();
        let slept = StdCell::new(false);
        let budget = Duration::from_millis(1_500);
        let outcome = wait_for_prime(&stores, budget, PRIME_WAIT_POLL, &clock, |_| {
            slept.set(true);
        });

        assert!(outcome.all_primed, "all-primed must report all_primed");
        assert_eq!(outcome.primed, 2);
        assert_eq!(outcome.total, 2);
        assert_eq!(
            outcome.waited,
            Duration::ZERO,
            "an already-primed set must not delay the first tick"
        );
        assert!(
            !slept.get(),
            "must not sleep when everything is already primed"
        );
    }

    #[test]
    fn proceeds_after_timeout_when_a_store_never_primes() {
        // One store primes; the other NEVER does (the deliberately-missing /
        // dead-source case). The wait MUST NOT hang: once the budget elapses it
        // returns all_primed == false so the caller starts the clock anyway. The
        // injected sleep advances the manual clock by exactly the requested nap,
        // so the budget is reached deterministically with zero real sleeping.
        let a = store("a");
        let dead = store("dead");
        a.publish(frame(), MediaTime::from_nanos(0));
        // `dead` is intentionally never published into.
        let stores = [&a, &dead];

        let clock = ManualTimeSource::new();
        let naps = StdCell::new(0_u32);
        let budget = Duration::from_millis(1_500);
        let outcome = wait_for_prime(&stores, budget, PRIME_WAIT_POLL, &clock, |nap| {
            // Drive the SAME clock the wait measures against, so the budget is
            // actually reached — this is what proves the loop terminates.
            clock.advance(nap);
            naps.set(naps.get().saturating_add(1));
        });

        assert!(
            !outcome.all_primed,
            "a never-priming source must NOT keep all_primed true (no infinite wait)"
        );
        assert_eq!(outcome.primed, 1, "only the live source primed");
        assert_eq!(outcome.total, 2);
        assert!(
            outcome.waited >= budget,
            "the wait must spend (at least) the full budget before giving up, got {:?}",
            outcome.waited
        );
        // It must have actually polled/slept a bounded number of times to get
        // there (budget / poll), proving it neither span hot nor hung.
        assert!(naps.get() > 0, "must have polled at least once");
        let max_naps = (budget.as_nanos() / PRIME_WAIT_POLL.as_nanos()).max(1) + 2;
        assert!(
            u128::from(naps.get()) <= max_naps,
            "poll count {} must be bounded by ~budget/poll ({max_naps})",
            naps.get()
        );
    }

    #[test]
    fn no_bound_stores_does_not_delay_the_clock() {
        // A run with no cell-bound source (degenerate) must not wait at all.
        let stores: [&Arc<TileStore<Nv12Image>>; 0] = [];
        let clock = ManualTimeSource::new();
        let outcome = wait_for_prime(
            &stores,
            Duration::from_millis(1_500),
            PRIME_WAIT_POLL,
            &clock,
            |_| panic!("must never sleep with no stores"),
        );
        assert!(outcome.all_primed);
        assert_eq!(outcome.total, 0);
        assert_eq!(outcome.waited, Duration::ZERO);
    }
}

/// Tests for the live-ingest reconnect policy (#45): a capped-exponential backoff
/// with equal jitter, a deterministic per-source jitter source, the healthy-reset
/// attempt counter, and the `rw_timeout` ingest-open options. The backoff/jitter/
/// attempt logic is pure, so these run with zero real sleeping and no network.
#[cfg(test)]
mod reconnect_tests {
    use std::time::Duration;

    use super::{
        ingest_open_options, is_hls_location, next_reconnect_attempt, reconnect_backoff, JitterRng,
        SourceLocation, INGEST_RECONNECT_BASE, INGEST_RECONNECT_CAP, INGEST_RECONNECT_HEALTHY,
        INGEST_RECONNECT_MAX_ATTEMPT, INGEST_RW_TIMEOUT,
    };

    #[test]
    fn backoff_uses_equal_jitter_within_the_capped_window() {
        // attempt 0 ⇒ window = BASE; equal jitter splits it in half: a fixed
        // half plus a jittered half in [0, half] ⇒ result in [BASE/2, BASE].
        assert_eq!(reconnect_backoff(0, 0.0), INGEST_RECONNECT_BASE / 2);
        assert_eq!(reconnect_backoff(0, 1.0), INGEST_RECONNECT_BASE);
        let half = INGEST_RECONNECT_BASE / 2;
        assert_eq!(reconnect_backoff(0, 0.5), half + half.mul_f64(0.5));
        // Never zero — no hot reconnect loop even at the lowest jitter.
        assert!(reconnect_backoff(0, 0.0) > Duration::ZERO);
    }

    #[test]
    fn backoff_grows_exponentially_then_saturates_at_the_cap() {
        let a0 = reconnect_backoff(0, 0.0);
        let a1 = reconnect_backoff(1, 0.0);
        let a2 = reconnect_backoff(2, 0.0);
        assert_eq!(a1, a0 * 2, "each attempt doubles the floor");
        assert_eq!(a2, a1 * 2);
        // A huge attempt index saturates at the cap (floor = CAP/2, ceil = CAP)
        // and NEVER overflows / panics.
        assert_eq!(reconnect_backoff(1000, 0.0), INGEST_RECONNECT_CAP / 2);
        assert_eq!(reconnect_backoff(1000, 1.0), INGEST_RECONNECT_CAP);
        assert_eq!(reconnect_backoff(u32::MAX, 1.0), INGEST_RECONNECT_CAP);
    }

    #[test]
    fn backoff_treats_non_finite_or_out_of_range_jitter_as_zero_clamped() {
        let floor = INGEST_RECONNECT_BASE / 2;
        assert_eq!(reconnect_backoff(0, f64::NAN), floor);
        assert_eq!(reconnect_backoff(0, f64::INFINITY), floor);
        assert_eq!(reconnect_backoff(0, -1.0), floor, "negative clamps to 0");
        assert_eq!(
            reconnect_backoff(0, 5.0),
            INGEST_RECONNECT_BASE,
            "clamps to 1"
        );
    }

    #[test]
    fn jitter_rng_is_deterministic_per_seed_decorrelated_and_in_unit_range() {
        let mut a = JitterRng::seeded("source-a");
        let mut a2 = JitterRng::seeded("source-a");
        let seq_a: Vec<f64> = (0..8).map(|_| a.next_unit()).collect();
        let seq_a2: Vec<f64> = (0..8).map(|_| a2.next_unit()).collect();
        // The PRNG is deterministic, so the sequences are bit-identical — compare
        // raw bits (exact, not an epsilon) to make that the assertion.
        let bits = |v: &[f64]| v.iter().map(|f| f.to_bits()).collect::<Vec<u64>>();
        assert_eq!(
            bits(&seq_a),
            bits(&seq_a2),
            "same seed ⇒ identical sequence (so it is testable)"
        );
        for &u in &seq_a {
            assert!((0.0..=1.0).contains(&u), "jitter unit {u} out of [0,1]");
        }
        // Two different sources must not reconnect in lockstep (decorrelation).
        let mut b = JitterRng::seeded("source-b");
        assert_ne!(
            b.next_unit().to_bits(),
            seq_a[0].to_bits(),
            "different sources must not lockstep"
        );
        // The sequence actually varies (not a stuck constant).
        assert!(
            seq_a.windows(2).any(|w| w[0].to_bits() != w[1].to_bits()),
            "jitter must vary across draws"
        );
    }

    #[test]
    fn next_attempt_grows_capped_on_fast_failure_and_resets_when_healthy() {
        let fast = Duration::from_millis(10);
        assert_eq!(next_reconnect_attempt(0, fast), 1);
        assert_eq!(next_reconnect_attempt(3, fast), 4);
        assert_eq!(
            next_reconnect_attempt(INGEST_RECONNECT_MAX_ATTEMPT, fast),
            INGEST_RECONNECT_MAX_ATTEMPT,
            "attempt index is capped so the shift can never overflow",
        );
        // A connection that streamed for >= the healthy threshold resets to 0,
        // so a long-lived source that finally blips reconnects promptly.
        assert_eq!(next_reconnect_attempt(5, INGEST_RECONNECT_HEALTHY), 0);
        assert_eq!(
            next_reconnect_attempt(6, INGEST_RECONNECT_HEALTHY + Duration::from_secs(1)),
            0,
        );
        // Just under healthy still counts as a fast failure (keeps escalating).
        assert_eq!(
            next_reconnect_attempt(
                2,
                INGEST_RECONNECT_HEALTHY.saturating_sub(Duration::from_millis(1))
            ),
            3,
        );
    }

    #[test]
    fn open_options_set_rw_timeout_for_network_sources_only() {
        // A network (URL) source gets rw_timeout (microseconds) so a dead live
        // source fails the open / a blocking read instead of hanging forever.
        let url = SourceLocation::Url("rtsp://example.invalid/stream".to_owned());
        let opts = ingest_open_options(&url);
        let want = INGEST_RW_TIMEOUT.as_micros().to_string();
        assert_eq!(opts.get("rw_timeout"), Some(want.as_str()));

        // A local file gets NO timeout — a slow disk must not abort a finite
        // source mid-decode.
        let path = SourceLocation::Path(std::path::PathBuf::from("/tmp/clip.mp4"));
        let opts = ingest_open_options(&path);
        assert_eq!(opts.get("rw_timeout"), None);
    }

    #[test]
    fn open_options_harden_hls_masters_against_the_webvtt_rendition_footgun() {
        // An HLS master (.m3u8 URL) gets the open-time hardening (ADR-T011): a
        // bounded segment retry and a sane protocol allowlist; plus `strict=normal`
        // as DEFENCE-IN-DEPTH (it dropped the SUBTITLES rendition pre-probe on
        // FFmpeg 7.x, but 8.x removed that gate — the variant-pin pre-open is the
        // 8.x-robust guard). It MUST NOT widen `allowed_extensions` to admit `.vtt`.
        let hls = SourceLocation::Url("https://h.test/live/master.m3u8".to_owned());
        assert!(is_hls_location(&hls), "an .m3u8 URL is an HLS master");
        let opts = ingest_open_options(&hls);
        assert_eq!(opts.get("strict"), Some("normal"));
        assert_eq!(opts.get("seg_max_retry"), Some("8"));
        assert_eq!(
            opts.get("protocol_whitelist"),
            Some("file,http,https,tcp,tls,crypto,data")
        );
        assert_eq!(
            opts.get("allowed_extensions"),
            None,
            "the main demuxer must NEVER widen allowed_extensions to admit .vtt"
        );

        // A query string after the .m3u8 still classifies as HLS.
        let hls_q = SourceLocation::Url("https://h.test/m.m3u8?token=abc".to_owned());
        assert!(is_hls_location(&hls_q));

        // A non-HLS network source gets `rw_timeout` but NONE of the HLS knobs.
        let rtsp = SourceLocation::Url("rtsp://example.invalid/stream".to_owned());
        assert!(!is_hls_location(&rtsp));
        let opts = ingest_open_options(&rtsp);
        assert_eq!(opts.get("strict"), None);
        assert_eq!(opts.get("seg_max_retry"), None);
    }
}

/// Tests for the HLS variant-pin pre-open (ADR-T011, the `FFmpeg`-8.x-robust fix):
/// the MAIN demuxer must open a VIDEO VARIANT media playlist (which carries no
/// SUBTITLES rendition), never the master with its selectable `TYPE=SUBTITLES`
/// group — so libav 8.x (which dropped the `strict` rendition gate) never fetches
/// the broken `.vtt` and aborts the open. The fetch→parse→pick→resolve seam is
/// exercised offline with a canned master (no network, no FFI).
#[cfg(test)]
mod variant_pin_tests {
    use super::resolve_hls_variant_url_with;
    use crate::captions::{FetchedPlaylist, PlaylistFetcher};

    /// A [`PlaylistFetcher`] returning a fixed canned body, echoing the requested
    /// URL as the effective URL (no redirect) — drives the fetch→parse→pick→resolve
    /// seam offline (no network, no FFI).
    struct CannedFetcher(Result<String, String>);
    impl PlaylistFetcher for CannedFetcher {
        fn fetch(&self, url: &str) -> Result<FetchedPlaylist, String> {
            self.0.clone().map(|body| FetchedPlaylist {
                url: url.to_owned(),
                body,
            })
        }
    }

    /// A [`PlaylistFetcher`] simulating a redirecting/CDN-fronted master: the canned
    /// body is reported as fetched from a different **effective** URL. Relative
    /// variant URIs must resolve against this effective base (the ABC/Akamai case).
    struct RedirectingFetcher {
        effective_url: String,
        body: String,
    }
    impl PlaylistFetcher for RedirectingFetcher {
        fn fetch(&self, _url: &str) -> Result<FetchedPlaylist, String> {
            Ok(FetchedPlaylist {
                url: self.effective_url.clone(),
                body: self.body.clone(),
            })
        }
    }

    /// The ABC-News-AU footgun shape: an ABR master with a `TYPE=SUBTITLES`
    /// `WebVTT` rendition (`index_7_0.m3u8`) the `FFmpeg`-8.x HLS demuxer would
    /// otherwise fetch and abort on.
    const ABR_MASTER_WITH_SUBS: &str = concat!(
        "#EXTM3U\n",
        "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",",
        "LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"index_7_0.m3u8\"\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,SUBTITLES=\"subs\"\n",
        "index_0.m3u8\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,SUBTITLES=\"subs\"\n",
        "index_2.m3u8\n",
    );

    #[test]
    fn pins_the_main_demuxer_to_a_video_variant_url_for_the_tile_height() {
        let fetcher = CannedFetcher(Ok(ABR_MASTER_WITH_SUBS.to_owned()));
        // A 360-px tile pins the 360p rung (decode-at-display-resolution), and the
        // URL is the variant media playlist (relative URI resolved against the
        // master's base) — NOT the master, so libav never sees the SUBTITLES group.
        let pinned = resolve_hls_variant_url_with(
            "https://c.test/abc-news/master.m3u8",
            Some(360),
            &fetcher,
        )
        .expect("an ABR master pins a variant");
        assert_eq!(pinned, "https://c.test/abc-news/index_0.m3u8");
        // A 700-px tile pins the 720p rung.
        let pinned = resolve_hls_variant_url_with(
            "https://c.test/abc-news/master.m3u8",
            Some(700),
            &fetcher,
        )
        .expect("an ABR master pins a variant");
        assert_eq!(pinned, "https://c.test/abc-news/index_2.m3u8");
    }

    #[test]
    fn a_relative_variant_uri_resolves_against_the_redirected_master_base() {
        // The box-found defect: `c.mjh.nz/abc-news.m3u8` 302-redirects to a signed
        // Akamai master whose variant URIs are RELATIVE. The variant must resolve
        // against the EFFECTIVE (post-redirect) Akamai base — resolving against the
        // requested `c.mjh.nz` origin yields a 404 that aborts the ingest.
        let fetcher = RedirectingFetcher {
            effective_url: "https://abc-iview.akamaized.net/out/v1/abcd/master.m3u8?hdnea=token"
                .to_owned(),
            body: ABR_MASTER_WITH_SUBS.to_owned(),
        };
        let pinned =
            resolve_hls_variant_url_with("https://c.mjh.nz/abc-news.m3u8", Some(700), &fetcher)
                .expect("a redirected ABR master pins a variant");
        assert_eq!(
            pinned, "https://abc-iview.akamaized.net/out/v1/abcd/index_2.m3u8",
            "the relative variant URI must resolve under the post-redirect Akamai base, \
             not the requested c.mjh.nz origin"
        );
    }

    #[test]
    fn a_relative_variant_uri_resolves_against_the_master_base_without_redirect() {
        // The non-redirect relative-child case must not regress: when the effective
        // URL equals the requested URL, the variant resolves against that base.
        let fetcher = CannedFetcher(Ok(ABR_MASTER_WITH_SUBS.to_owned()));
        let pinned =
            resolve_hls_variant_url_with("https://cdn.test/live/master.m3u8", Some(700), &fetcher)
                .expect("a non-redirecting ABR master pins a variant");
        assert_eq!(pinned, "https://cdn.test/live/index_2.m3u8");
    }

    #[test]
    fn an_absolute_variant_uri_is_used_verbatim() {
        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n",
            "https://cdn.test/v/720.m3u8\n",
        );
        let fetcher = CannedFetcher(Ok(master.to_owned()));
        let pinned = resolve_hls_variant_url_with("https://h.test/m.m3u8", Some(720), &fetcher)
            .expect("variant pinned");
        assert_eq!(pinned, "https://cdn.test/v/720.m3u8");
    }

    #[test]
    fn a_media_playlist_with_no_variants_leaves_the_url_unchanged() {
        // A direct media playlist (segments, no #EXT-X-STREAM-INF) has nothing to
        // pin — return None so the caller opens the original URL as-is.
        let media = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXTINF:6.0,\n",
            "seg0.ts\n",
        );
        let fetcher = CannedFetcher(Ok(media.to_owned()));
        assert!(
            resolve_hls_variant_url_with("https://h.test/index.m3u8", Some(360), &fetcher)
                .is_none()
        );
    }

    #[test]
    fn a_fetch_failure_leaves_the_url_unchanged() {
        // Best-effort: a master that cannot be fetched returns None, so the caller
        // falls back to opening the original URL (the post-open discard +
        // reconnect bracket still apply — never fails the build, invariants #1/#10).
        let fetcher = CannedFetcher(Err("network down".to_owned()));
        assert!(
            resolve_hls_variant_url_with("https://h.test/master.m3u8", Some(360), &fetcher)
                .is_none()
        );
    }

    #[test]
    fn unparseable_master_text_leaves_the_url_unchanged() {
        let fetcher = CannedFetcher(Ok("this is not a playlist".to_owned()));
        assert!(
            resolve_hls_variant_url_with("https://h.test/master.m3u8", Some(360), &fetcher)
                .is_none()
        );
    }
}

/// Tests for the `YouTube` ingest seam (IN-5): a `youtube` source plans as a
/// re-resolvable, live, network ingest location bound by its **watch** URL — never
/// a hand-copied manifest. The resolver core + the re-resolution policy/loop are
/// unit-proven (no network) in `multiview-input`; here we pin only the CLI wiring
/// (plan mapping + network classification), with no network and no `yt-dlp`.
#[cfg(test)]
#[cfg(feature = "youtube")]
mod youtube_tests {
    use std::sync::Arc;

    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::Source;
    use multiview_core::time::Rational;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use super::{ingest_open_options, ingest_plan_for, SourceLocation};

    /// Build a `youtube` `Source` via serde (the type is `#[non_exhaustive]`, so a
    /// struct literal is not available cross-crate — config is produced by
    /// deserialization anyway).
    fn youtube_source(id: &str, url: &str) -> Source {
        let json = serde_json::json!({
            "id": id,
            "kind": "youtube",
            "url": url,
        });
        serde_json::from_value(json).expect("youtube source deserializes")
    }

    #[test]
    fn youtube_source_plans_as_a_live_rebindable_watch_url() {
        let source = youtube_source("yt-live", "https://www.youtube.com/watch?v=abcdEFGH123");
        let store = Arc::new(TileStore::new(
            source.id.clone(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let plan = ingest_plan_for(
            &source,
            320,
            180,
            Arc::clone(&store),
            CanvasColor::default(),
            Rational::new(30, 1),
        )
        .expect("youtube source plans without failing the build");

        // It is bound by the WATCH url (so it can re-resolve), not a manifest, and
        // it is `live` so the ingest loop reconnects + re-resolves forever.
        let SourceLocation::Youtube { watch_url } = &plan.location else {
            panic!("expected a Youtube location for a youtube source");
        };
        assert_eq!(watch_url, "https://www.youtube.com/watch?v=abcdEFGH123");
        assert!(plan.live, "a youtube live source must be live (reconnects)");
    }

    #[test]
    fn youtube_location_is_classified_as_a_network_source() {
        // The resolved master is a `*.googlevideo.com` URL: a network source, so it
        // must get the `rw_timeout` (a stalled live source fails the open / a
        // blocking read rather than hanging the decode thread — invariant #1/#10).
        let location = SourceLocation::Youtube {
            watch_url: "https://www.youtube.com/watch?v=abcdEFGH123".to_owned(),
        };
        let opts = ingest_open_options(&location);
        assert_eq!(
            opts.get("rw_timeout"),
            Some(super::INGEST_RW_TIMEOUT.as_micros().to_string().as_str()),
        );
    }
}

/// Tests for the NDI ingest seam (IN-3): under the `ndi` feature a `ndi` source
/// plans as a live ingest location bound by its source **name**, never erroring
/// the build (the receive→NV12 conversion + the `NdiProducer` are unit-proven in
/// `multiview-input`; here we pin only the CLI plan mapping). With the feature
/// OFF the source is an honest typed refusal. No NDI runtime/network is touched.
#[cfg(test)]
#[cfg(feature = "ndi")]
mod ndi_tests {
    #![allow(
        // reason: a unit test module; the strict workspace lints are relaxed for
        // test code per CLAUDE.md (these inner `#[allow]`s mirror the surrounding
        // `tests` modules in this file).
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    use std::sync::Arc;

    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::Source;
    use multiview_core::time::Rational;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use super::{ingest_plan_for, SourceLocation};

    /// Build an `ndi` `Source` via serde (the kind is `#[non_exhaustive]`, so a
    /// struct literal is not available cross-crate; config is deserialized anyway).
    fn ndi_source(id: &str, name: &str) -> Source {
        let json = serde_json::json!({
            "id": id,
            "kind": "ndi",
            "name": name,
        });
        serde_json::from_value(json).expect("ndi source deserializes")
    }

    #[test]
    fn ndi_source_plans_as_a_live_named_source() {
        // The hard "NDI ingest is not wired" error is replaced under the feature by
        // a real plan: bound by the NDI source NAME, `live` so the ingest loop
        // reconnects forever. It must never fail the build (invariants #1/#10).
        let source = ndi_source("cam-1", "STUDIO (CAM 1)");
        let store = Arc::new(TileStore::new(
            source.id.clone(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let plan = ingest_plan_for(
            &source,
            320,
            180,
            Arc::clone(&store),
            CanvasColor::default(),
            Rational::new(30, 1),
        )
        .expect("ndi source plans without failing the build");

        let SourceLocation::Ndi { name } = &plan.location else {
            panic!("expected an Ndi location for an ndi source");
        };
        assert_eq!(name, "STUDIO (CAM 1)");
        assert!(plan.live, "an ndi source must be live (reconnects)");
    }
}

#[cfg(test)]
mod eng2_timeline_tests {
    //! ENG-2: the ingest publish timeline must be the *normalized* one — raw
    //! source-tick PTS unwrapped onto a monotonic nanosecond timeline (invariant
    //! #3) — not the decoder's bare rescaled PTS. These exercise [`timeline_pts`]'s
    //! wiring of the per-input [`PtsNormalizer`]; the wrap arithmetic itself is
    //! unit-proven in `multiview-input::normalize`.

    use multiview_core::time::{MediaTime, Rational};
    use multiview_input::normalize::{PtsNormalizer, WrapBits};

    use super::timeline_pts;

    /// A long-running live MPEG-TS source wraps its 33-bit PTS at the 90 kHz
    /// clock; the published timeline must stay strictly monotonic across the wrap.
    /// The `fallback` (the decoder's rescaled PTS) is deliberately `ZERO` every
    /// frame, so a regression that dropped the normalizer and returned the
    /// fallback would collapse the timeline to a constant and fail here.
    #[test]
    fn publish_timeline_is_monotonic_across_a_33bit_pts_wrap() {
        let time_base = Rational::new(1, 90_000); // 90 kHz MPEG-TS clock
        let cadence = Rational::new(30, 1); // 30 fps
        let mut normalizer = PtsNormalizer::new(WrapBits::Mpeg33, time_base, cadence);

        let modulus: i64 = 1 << 33; // 33-bit PTS wraps at 2^33 ticks
        let step: i64 = 3_000; // 90 kHz ticks per 30 fps frame

        // Five frames straddling the wrap: the third is the post-wrap masked
        // value (as libav would present it after 2^33 ticks elapse).
        let raws = [modulus - 2 * step, modulus - step, step, 2 * step, 3 * step];

        let out: Vec<MediaTime> = raws
            .into_iter()
            .map(|raw| timeline_pts(&mut normalizer, Some(raw), MediaTime::ZERO))
            .collect();

        for pair in out.windows(2) {
            assert!(
                pair[1] > pair[0],
                "published timeline must be strictly monotonic across a 33-bit wrap, got {out:?}"
            );
        }
    }

    /// A source that emits no usable PTS (`AV_NOPTS`) must still advance one frame
    /// period per frame via the genpts fallback — never collapse to a constant.
    #[test]
    fn missing_pts_advances_via_genpts_fallback() {
        let time_base = Rational::new(1, 90_000);
        let cadence = Rational::new(25, 1); // PAL 25 fps
        let mut normalizer = PtsNormalizer::new(WrapBits::Mpeg33, time_base, cadence);

        let out: Vec<MediaTime> = (0..4)
            .map(|_| timeline_pts(&mut normalizer, None, MediaTime::ZERO))
            .collect();

        for pair in out.windows(2) {
            assert!(
                pair[1] > pair[0],
                "genpts fallback must advance the timeline each frame, got {out:?}"
            );
        }
    }
}

/// RT-8b: the cli bake consumer must drive the program-audio bus by the **output
/// tick index** carried on each [`StreamItem`], not once per surviving (dequeued)
/// frame. Under `DropOnOverload` some video frames are shed, so a surviving-frame-
/// paced `bus.tick()` would emit fewer audio ticks than the tick count and audio
/// would trail video by exactly the dropped ticks' samples (invariant #3
/// violation). [`drive_audio_for_item`] catches up across each gap via
/// [`ProgramBus::tick_to`](multiview_audio::program::ProgramBus::tick_to), so the
/// cumulative emitted samples stay locked to the `SampleClock` ideal regardless of
/// how many ticks were dropped. This proves the fix at the cli-driver seam (the
/// audio-crate primitive is proven separately in `lip_sync_breakaway.rs`).
#[cfg(test)]
mod rt8b_lip_sync_driver_tests {
    use std::sync::Arc;

    use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
    use multiview_audio::program::ProgramBus;
    use multiview_audio::store::AudioStore;
    use multiview_core::time::Rational;

    use super::drive_audio_for_item;

    const FS: u32 = 48_000;

    fn stereo() -> AudioFormat {
        AudioFormat::new(FS, ChannelLayout::Stereo)
    }

    /// The `SampleClock` drift-free ideal cumulative sample count after `ticks`
    /// ticks at `rate` Hz / `num`/`den` fps — `floor(ticks * rate * den / num)`.
    fn ideal_total(ticks: u64, rate: u64, num: u64, den: u64) -> u64 {
        (ticks * rate * den) / num
    }

    /// Under a `DropOnOverload` gap (only the SURVIVING ticks reach the consumer),
    /// driving the bus through the cli's [`drive_audio_for_item`] by each surviving
    /// item's tick index keeps cumulative emitted samples equal to the `SampleClock`
    /// ideal for the LAST tick index reached — it catches up across every dropped
    /// tick. A per-surviving-frame `tick()` would trail by the dropped ticks'
    /// samples; this test forbids that.
    #[test]
    fn cli_driver_is_tick_index_driven_under_dropped_frames() {
        let fmt = stereo();
        // NTSC fractional cadence so a per-tick-scalar shortcut cannot accidentally
        // pass: the per-tick budget alternates 1601/1602 and only the absolute
        // tick-index total is exact.
        let (rate, num, den) = (48_000_u64, 30_000_u64, 1001_u64);
        let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));
        let store = Arc::new(AudioStore::new(fmt, 96_000));
        store
            .publish(&AudioBlock::silence(fmt, 96_000))
            .expect("publish silence");
        bus.add_source("a", Arc::clone(&store), 1.0);

        // The output clock ticked 0..=499; under overload only these tick indices
        // SURVIVED to the bake consumer (every other tick, then larger gaps). The
        // dropped ticks must NOT drop an audio tick — the bus catches up.
        let surviving: [u64; 9] = [0, 1, 4, 8, 9, 49, 136, 300, 499];
        let mut cumulative = 0_u64;
        for &tick_index in &surviving {
            let block = drive_audio_for_item(&mut bus, tick_index);
            cumulative += u64::try_from(block.frame_count()).expect("block len fits u64");
            // After processing the surviving item at `tick_index`, the bus must have
            // emitted exactly the ideal cumulative samples for ticks 0..=tick_index
            // (i.e. `total_at(tick_index + 1)`) — never fewer (drift) despite the
            // skipped ticks before this one.
            let ideal = ideal_total(tick_index + 1, rate, num, den);
            assert_eq!(
                cumulative, ideal,
                "cli audio driver must be tick-index driven: after the surviving frame at \
                 tick {tick_index}, cumulative emitted samples must equal the SampleClock ideal \
                 {ideal} (catch-up across the dropped ticks), got {cumulative}"
            );
        }
    }

    /// With NO drops (every consecutive tick survives), the cli driver emits the
    /// SAME per-tick blocks as the old `bus.tick()` would — byte-identical
    /// behaviour on the steady path (the regression guard for the existing
    /// program-audio tests).
    #[test]
    fn cli_driver_matches_per_tick_when_nothing_is_dropped() {
        let fmt = stereo();
        let (rate, num, den) = (48_000_u64, 30_000_u64, 1001_u64);
        let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));
        let store = Arc::new(AudioStore::new(fmt, 96_000));
        store
            .publish(&AudioBlock::silence(fmt, 96_000))
            .expect("publish silence");
        bus.add_source("a", Arc::clone(&store), 1.0);

        let mut cumulative = 0_u64;
        for tick_index in 0_u64..200 {
            let block = drive_audio_for_item(&mut bus, tick_index);
            cumulative += u64::try_from(block.frame_count()).expect("block len fits u64");
            assert_eq!(
                cumulative,
                ideal_total(tick_index + 1, rate, num, den),
                "steady (no-drop) cli audio must equal the per-tick ideal at tick {tick_index}"
            );
        }
    }
}

#[cfg(test)]
mod live_source_registry_tests {
    //! ADR-W018: the startup ingest supervisor registers per-producer stop
    //! flags — the video thread under the source id, the caption reader under
    //! the derived `{id}/captions` key — so a live remove/edit can tear down
    //! exactly one startup source's producers (including its caption reader).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use multiview_config::MultiviewConfig;

    use super::{ingest_plan_for, IngestSupervisor};
    use crate::live_sources::stop_registry;

    /// A minimal config carrying one never-connecting RTSP source.
    fn rtsp_config() -> MultiviewConfig {
        let doc = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "net1"
kind = "rtsp"
url = "rtsp://[::1]:1/never"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "net1"
[[outputs]]
kind = "hls"
path = "/tmp/live-source-registry.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
        MultiviewConfig::load_from_toml(doc).expect("parse rtsp config")
    }

    /// m6: a STARTUP network ingest thread registers its per-source stop flag,
    /// so a live REMOVE can stop exactly that producer via the registry (the
    /// hub raises the flag; the supervisor's later join returns immediately).
    #[test]
    fn startup_network_ingest_registers_and_stops_via_the_registry() {
        let config = rtsp_config();
        let source = config.sources.first().expect("one source");
        let store = Arc::new(multiview_framestore::TileStore::with_defaults("net1"));
        let plan = ingest_plan_for(
            source,
            64,
            64,
            store,
            multiview_compositor::pipeline::CanvasColor::default(),
            multiview_core::time::Rational::new(25, 1),
        )
        .expect("rtsp ingest plan");

        let registry = stop_registry();
        let supervisor =
            IngestSupervisor::start(vec![plan], Vec::new(), Vec::new(), Vec::new(), &registry);
        let flag = registry
            .lock()
            .expect("registry")
            .get("net1")
            .cloned()
            .expect("the startup ingest thread registers its stop flag");
        // A live remove raises exactly this flag (the hub does this); the
        // ingest loop observes it between (re)connect attempts and exits, so
        // the supervisor's shutdown join returns without the wedge-detach path.
        flag.store(true, Ordering::Release);
        supervisor.shutdown();
    }

    /// M1: the caption reader registers under the derived `{id}/captions` key,
    /// so a live edit/remove of the source also stops its caption reader (the
    /// hub raises every `{id}`-rooted flag).
    #[test]
    fn caption_reader_registers_a_prefixed_stop_flag() {
        let plan = crate::captions::CaptionPlan {
            id: "net1".to_owned(),
            rendition_url: "http://[::1]:1/never/subs.m3u8".to_owned(),
            store: Arc::new(crate::captions::CueStore::new()),
            live: true,
        };
        let registry = stop_registry();
        let supervisor =
            IngestSupervisor::start(Vec::new(), Vec::new(), Vec::new(), vec![plan], &registry);
        let flag = registry
            .lock()
            .expect("registry")
            .get("net1/captions")
            .cloned()
            .expect("the caption reader registers under {id}/captions");
        flag.store(true, Ordering::Release);
        supervisor.shutdown();
    }
}

#[cfg(test)]
mod shed_emission_tests {
    //! The live encode/egress drop-on-overload shed must emit a real `shed.load`
    //! onto the outbound event stream (invariant #9 emission) — change-driven and
    //! rate-limited so a drop storm coalesces into at most one event per window
    //! (inv #10), never a per-dropped-frame flood. These pin the pure decision in
    //! [`shed_load_event`]; the per-tick projection + the drop-oldest publish are
    //! wired in `drive_streaming`.
    use multiview_events::{Event, ShedReason, ShedScope};

    use super::{shed_load_event, SHED_EVENT_EVERY_TICKS};

    #[test]
    fn no_drop_emits_nothing() {
        let (mut last_dropped, mut last_tick) = (0_u64, 0_u64);
        // The counter never advanced: no shed happened, so no event is emitted.
        assert!(shed_load_event(0, 100, &mut last_dropped, &mut last_tick).is_none());
        assert_eq!(last_dropped, 0, "state untouched when nothing was shed");
    }

    #[test]
    fn first_real_drop_emits_an_encoder_overload_program_shed() {
        let (mut last_dropped, mut last_tick) = (0_u64, 0_u64);
        // The first drop fires at tick `SHED_EVENT_EVERY_TICKS` (one window after
        // the t=0 start); it carries the cumulative count + the live reason/scope.
        let event = shed_load_event(3, SHED_EVENT_EVERY_TICKS, &mut last_dropped, &mut last_tick)
            .expect("a real drop emits a shed.load");
        match event {
            Event::ShedLoad(shed) => {
                assert_eq!(shed.reason, ShedReason::EncoderOverload);
                assert_eq!(shed.scope, ShedScope::Program);
                assert_eq!(shed.dropped, 3, "cumulative drop count rides the event");
                assert_eq!(shed.level, 1, "egress shed is the program-touching rung");
            }
            other => panic!("expected ShedLoad, got {other:?}"),
        }
        assert_eq!(last_dropped, 3);
        assert_eq!(last_tick, SHED_EVENT_EVERY_TICKS);
    }

    #[test]
    fn a_drop_storm_coalesces_to_at_most_one_event_per_window() {
        let (mut last_dropped, mut last_tick) = (0_u64, 0_u64);
        // First emit at the end of the opening window.
        assert!(
            shed_load_event(1, SHED_EVENT_EVERY_TICKS, &mut last_dropped, &mut last_tick).is_some()
        );
        // Every tick inside the next window keeps shedding (counter advances) but
        // must NOT emit again until the debounce window has elapsed.
        for t in 1..SHED_EVENT_EVERY_TICKS {
            let tick = SHED_EVENT_EVERY_TICKS + t;
            assert!(
                shed_load_event(1 + t, tick, &mut last_dropped, &mut last_tick).is_none(),
                "a within-window drop must coalesce, not flood (tick {tick})"
            );
        }
        // Once the window elapses AND the counter has advanced, a second event
        // fires carrying the new cumulative total.
        let tick = SHED_EVENT_EVERY_TICKS * 2;
        let event = shed_load_event(99, tick, &mut last_dropped, &mut last_tick)
            .expect("a new window with fresh drops emits again");
        match event {
            Event::ShedLoad(shed) => assert_eq!(shed.dropped, 99),
            other => panic!("expected ShedLoad, got {other:?}"),
        }
    }

    #[test]
    fn a_quiesced_overload_stops_emitting() {
        let (mut last_dropped, mut last_tick) = (5_u64, SHED_EVENT_EVERY_TICKS);
        // The drop counter has not advanced past the last emit: the overload
        // cleared, so no further shed events are emitted even windows later.
        let tick = SHED_EVENT_EVERY_TICKS * 10;
        assert!(
            shed_load_event(5, tick, &mut last_dropped, &mut last_tick).is_none(),
            "a quiesced overload (no new drops) stops emitting"
        );
    }
}
