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
#[cfg(feature = "rist")]
use multiview_config::{lower_rist_url, RistOptions};
use multiview_config::{MultiviewConfig, Output, Source, SourceKind};
use multiview_control::EngineStateSnapshot;
use multiview_core::frame::FrameMeta;
use multiview_core::layout::{Cell, Layout};
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_core::traits::SourceState;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, MonotonicTimeSource, MultiviewProgram,
    OutputClock, Pacer, RealtimePacer, RequestedSize, SourceHandle, SourceKey, SourceRegistry,
    StopSignal, TimeSource,
};
use multiview_events::Event;
use multiview_ffmpeg::{
    DecodedVideoFrame, EncodedPacket, ScaleSpec, Scaler, StreamCodecParameters, StreamVideoDecoder,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
#[cfg(all(feature = "ndi", any(feature = "ndi-bindings", test)))]
use multiview_output::ndi::NdiOutput;
use multiview_output::sink::{
    EncodeConfig, PacketMuxOutcome, PacketMuxSink, PacketSource, ProgramEncoder, PushProtocol,
};
use multiview_output::{display_matrix, DisplayMatrix};

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
    /// The chosen device's stable identity — the **island device** every later
    /// runtime decode placement is pinned to (ADR-W018 §7 / ADR-0018: a
    /// runtime add never fragments or migrates the island). `None` in lockstep
    /// with `wgpu_target`.
    device: Option<multiview_hal::DeviceId>,
}

/// The running pipeline's pinned **island** (ADR-W018 §7): the device the
/// decide-once admission pick placed the whole `decode → composite → encode`
/// island on, its CUDA ordinal, and the tile count the startup demand modelled.
/// Published by `drive_streaming` into a shared slot the
/// [`LiveIngestSpawner`] reads, so every runtime decode placement consults the
/// same admission scorer **pinned to this device** — never a different GPU,
/// never a migration.
#[cfg(feature = "gpu")]
#[derive(Debug, Clone)]
struct LiveIsland {
    /// The island device's stable identity (the runtime-placement pin).
    device: multiview_hal::DeviceId,
    /// The island device's CUDA enumeration ordinal (stamped onto an admitted
    /// runtime decode so NVDEC co-locates with the compositor). `None` when
    /// the device resolved no ordinal — then an admit keeps the default CUDA
    /// device, in lockstep with the startup plans.
    cuda_ordinal: Option<String>,
    /// The startup island's tile count (the layout's cells) — the base demand
    /// a runtime add extends by one decode.
    tile_count: usize,
}

/// The shared slot `drive_streaming` publishes the pinned [`LiveIsland`] into
/// (empty until — and unless — admission names a device). Lock-free reads on
/// the hub worker; never touched by the output clock.
#[cfg(feature = "gpu")]
type LiveIslandSlot = Arc<arc_swap::ArcSwapOption<LiveIsland>>;

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
    // ceiling) + VRAM-dominant scoring, which need no perf-class table. ADR-0018
    // makes the hard gates (headroom + capability + NVENC-session) "the real
    // safety", so this permissive Mpix/s budget is effectively inert and the
    // headroom/score do the work. A real per-GPU perf-class `CostBudget` table
    // is future work (not yet built).
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
        device: Some(selection.device),
    }
}

/// Place ONE runtime-added decode (ADR-W018 §7, level 2): consult the **same**
/// [`multiview_hal::select_device`] scorer the startup admission uses, with two
/// changes honouring the GPU-placement principle:
///
/// * the candidate set is **exactly the running island's device** — a runtime
///   add may never fragment or migrate the island, and a single-candidate
///   consult makes naming any other GPU impossible. (ADR-W018 §7 sketched
///   `Pins::pin_pipeline`; a pin deliberately *bypasses* the headroom ceiling
///   — operator-pins-always-win — which would defeat the §7 reject →
///   software-decode ladder, so the implementation restricts the candidate set
///   instead: identical never-fragment guarantee, headroom still gates.)
/// * the demand is the island's current tile set **plus** the new decode
///   (`TileLoad::new(Decode, …)`), re-polling NVML at decision time. The
///   demand opens **no** new encode session (the island's NVENC session is
///   already open and counted in the *measured* snapshot — modelling it again
///   would double-count it against the session ceiling).
///
/// An **admit** returns [`DecodePlacement::Pinned`] with the island's CUDA
/// ordinal (NVDEC co-located with the compositor — ADR-0035 affinity). A
/// **reject** (budget / headroom / the island gone from the snapshot) — AND an
/// admit where the island resolved **no** CUDA ordinal — returns
/// [`DecodePlacement::SoftwareOnly`]: the decoder open for that one source is
/// FORCED to software (the [`decoder_open_args`] gate). This NEVER returns
/// `Default` on the live path: `Default` opens NVDEC on libav's default device,
/// which here is fragmentation/overcommit — the compositor island is *already*
/// pinned at runtime, so an un-pinnable (no-ordinal) admit cannot co-locate
/// NVDEC with it and must fall to software, not the default GPU. (`Default` is
/// correct only at startup, where nothing is pinned yet.) The island is never
/// overcommitted or fragmented and the output never falters (inv #1).
///
/// Runs on the hub worker thread only (it polls NVML) — never on the clock.
#[cfg(feature = "gpu")]
fn select_live_decode_placement(
    load_source: &dyn multiview_hal::LoadSource,
    island: &LiveIsland,
    canvas_w: u32,
    canvas_h: u32,
    cadence: Rational,
) -> DecodePlacement {
    use multiview_core::pixel::PixelFormat;
    use multiview_core::traits::BackendKind;
    use multiview_hal::{
        select_device, Capability, CostBudget, GpuCandidate, Pins, PipelineDemand, PlacementPolicy,
        Resolution, Stage, StageCaps, TileLoad,
    };

    // Fresh measured load at decision time (the ADR-W018 §7 re-poll): a
    // removed source's NVDEC/VRAM consumption has already vanished from these
    // counters when its decoder closed — the measured-load model needs no
    // booking ledger to return budget.
    let loads = load_source.poll();
    let Some(island_load) = loads.iter().find(|l| l.device_id == island.device) else {
        tracing::warn!(
            island = island.device.stable_id(),
            "live decode placement: the island device is absent from the load \
             snapshot at decision time; FORCING software decode for this source \
             (hardware on the default device could overcommit or fragment the island)"
        );
        return DecodePlacement::SoftwareOnly;
    };

    let canvas_res = Resolution::new(canvas_w.max(1), canvas_h.max(1));
    // The same conservative per-engine budget the startup admission uses. The
    // per-engine Mpix/s budget is intentionally permissive: ADR-0018 makes the
    // VRAM-headroom hard gate + VRAM-dominant score the real safety net ("hard
    // gates are the real safety"), so this Mpix/s budget does not gate a typical
    // multiview on a real GPU — a real per-GPU perf-class budget table is future
    // work, not yet built. KNOWN RESIDUAL (disclosed): a VRAM-roomy but
    // decode-engine-saturated GPU passes both this budget and the headroom gate,
    // so a live add CAN be admitted onto a GPU that cannot sustain another
    // decode. That is acceptable for this ship: the never-off-air contract holds
    // (an over-subscribed decode degrades the NEW tile, never the program — the
    // output clock samples last-good, inv #1/#2) and invariant #9's closed-loop
    // degradation sheds the cheapest tile if the saturation bites.
    let budget = CostBudget::new(100_000.0, 100_000.0, 100_000.0);
    let cap = |stage: Stage| {
        Capability::new(
            BackendKind::Cuda,
            stage,
            canvas_res,
            vec![PixelFormat::Nv12],
        )
    };
    // ONE candidate: the island device. select_device physically cannot name
    // another GPU — the affinity hard constraint by construction.
    let candidates = vec![GpuCandidate {
        device_id: island_load.device_id.clone(),
        stage_caps: StageCaps::new(
            cap(Stage::Decode),
            cap(Stage::Composite),
            cap(Stage::Encode),
        ),
        budget,
    }];

    // Demand: the island's current tile set PLUS the new decode. The +2 covers
    // the island's composite + encode loads, exactly as the startup demand
    // models them. `opens_encode_session = false`: no NEW session is opened by
    // a decode-only add (the running session is in the measured snapshot).
    let mut tile_loads: Vec<TileLoad> = Vec::with_capacity(island.tile_count.saturating_add(3));
    for _ in 0..island.tile_count.max(1) {
        tile_loads.push(TileLoad::new(Stage::Decode, canvas_res));
    }
    tile_loads.push(TileLoad::new(Stage::Decode, canvas_res)); // the new source
    tile_loads.push(TileLoad::new(Stage::Composite, canvas_res));
    tile_loads.push(TileLoad::new(Stage::Encode, canvas_res));
    let demand = PipelineDemand::new(cadence, tile_loads, canvas_res, PixelFormat::Nv12, 0, false);

    match select_device(
        &candidates,
        &demand,
        &loads,
        &Pins::none(),
        PlacementPolicy::default(),
    ) {
        Ok(selection) => {
            if let Some(ordinal) = island.cuda_ordinal.clone() {
                tracing::info!(
                    island = island.device.stable_id(),
                    cuda_ordinal = %ordinal,
                    score = selection.score,
                    "live decode placement: admitted onto the running island device \
                     (NVDEC co-located, ADR-W018 §7)"
                );
                DecodePlacement::Pinned(ordinal)
            } else {
                // Admitted by budget/headroom, but the island device resolved
                // no CUDA ordinal — on the LIVE path the compositor island is
                // already pinned, so an un-pinnable decode cannot co-locate and
                // `Default` (libav's default device) would fragment/overcommit.
                // Fail closed to software for this one source (ADR-W018 §7).
                tracing::warn!(
                    island = island.device.stable_id(),
                    score = selection.score,
                    "live decode placement: admitted but the island resolved no \
                     CUDA ordinal; FORCING software decode for this source (the \
                     default device would fragment/overcommit the pinned island)"
                );
                DecodePlacement::SoftwareOnly
            }
        }
        Err(reason) => {
            tracing::warn!(
                island = island.device.stable_id(),
                ?reason,
                "live decode placement REJECTED for this source: its decoder \
                 open is FORCED to software (the island is never overcommitted \
                 and never fragmented; the output never falters — ADR-W018 §7)"
            );
            DecodePlacement::SoftwareOnly
        }
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

    // -----------------------------------------------------------------------
    // ADR-W018 §7 — the LIVE decode placement consult (level 2): a runtime
    // add consults the SAME select_device scorer, candidate-restricted to the
    // running island's device (never fragments/migrates), and a reject
    // degrades THAT source to software decode only.
    // -----------------------------------------------------------------------

    use super::{LiveIngestSpawner, LiveIsland};

    fn island(uuid: &str, index: u32, ordinal: &str) -> LiveIsland {
        LiveIsland {
            device: DeviceId::new(Vendor::Nvidia, uuid, index),
            cuda_ordinal: Some(ordinal.to_owned()),
            tile_count: 4,
        }
    }

    /// An island whose device admits by headroom but resolved NO CUDA ordinal
    /// — the admit-with-no-pin case (finding #2): on the live path this must
    /// fall to software, never the default device.
    fn island_no_ordinal(uuid: &str, index: u32) -> LiveIsland {
        LiveIsland {
            device: DeviceId::new(Vendor::Nvidia, uuid, index),
            cuda_ordinal: None,
            tile_count: 4,
        }
    }

    #[test]
    fn live_decode_placement_admits_but_forces_software_when_the_island_has_no_ordinal() {
        // The idle P2000 PASSES the headroom/budget gate, but its LiveIsland
        // resolved no CUDA ordinal. On the live path the compositor island is
        // already pinned, so an un-pinnable admit must NOT fall to
        // `Default` (= libav default-device NVDEC, fragment/overcommit) — it
        // must FORCE software (finding #2). The decode-open gate confirms
        // (want_hw=false, no ordinal).
        let placement = super::select_live_decode_placement(
            &FakeTwoGpu,
            &island_no_ordinal(FakeTwoGpu::GPU1_UUID, 1),
            1920,
            1080,
            Rational::new(25, 1),
        );
        assert_eq!(
            placement,
            super::DecodePlacement::SoftwareOnly,
            "an admitted-but-un-pinnable live decode must force software, never Default"
        );
        assert_eq!(
            super::decoder_open_args(&placement, None),
            (false, None),
            "the un-pinnable admit's decoder open must not WANT hardware at all"
        );
    }

    #[test]
    fn live_decode_placement_admits_onto_the_idle_island_pinned_to_its_ordinal() {
        // The island is the idle P2000: the consult (island tile set + the new
        // decode, fresh load poll) admits as `Pinned` with the ISLAND's
        // ordinal — NVDEC co-locates with the running compositor, never a
        // different GPU (even though the fake topology has two) — and the
        // decode-open gate keeps the hardware preference + threads the pin.
        let placement = super::select_live_decode_placement(
            &FakeTwoGpu,
            &island(FakeTwoGpu::GPU1_UUID, 1, "1"),
            1920,
            1080,
            Rational::new(25, 1),
        );
        assert_eq!(
            placement,
            super::DecodePlacement::Pinned("1".to_owned()),
            "an admitted live decode must be pinned to the ISLAND device's ordinal"
        );
        let (want_hw, ordinal) = super::decoder_open_args(&placement, None);
        assert_eq!(
            want_hw,
            multiview_ffmpeg::want_hw_decode(None),
            "an admitted placement keeps the canonical hardware preference"
        );
        assert_eq!(
            ordinal,
            Some("1"),
            "the island ordinal reaches the decoder open"
        );
    }

    #[test]
    fn live_decode_placement_rejects_an_over_headroom_island_to_forced_software() {
        // The island is the 95%-VRAM 4060 (over the 0.85 headroom ceiling):
        // the consult REJECTS — and because the candidate set is exactly the
        // island device, the scorer cannot escape to the idle P2000 (that
        // would fragment the pipeline). The reject is the EXPLICIT
        // `SoftwareOnly` placement, and the decode-open gate turns it into a
        // software open (want_hw = false, no ordinal) even though NVDEC is
        // otherwise enabled — `None` ordinal alone would have opened NVDEC on
        // the DEFAULT device, i.e. the over-headroom island itself.
        let placement = super::select_live_decode_placement(
            &FakeTwoGpu,
            &island(FakeTwoGpu::GPU0_UUID, 0, "0"),
            1920,
            1080,
            Rational::new(25, 1),
        );
        assert_eq!(
            placement,
            super::DecodePlacement::SoftwareOnly,
            "an over-headroom island must FORCE software decode — never \
             overcommit the island or migrate to another GPU"
        );
        assert_eq!(
            super::decoder_open_args(&placement, None),
            (false, None),
            "the rejected source's decoder open must not WANT hardware at all"
        );
    }

    #[test]
    fn live_decode_placement_forces_software_when_the_island_vanishes() {
        // No load snapshot carries the island device (NVML gone, device lost):
        // the consult cannot verify the pin — hardware on the default device
        // could overcommit or fragment the (unverifiable) island, so the
        // placement is the explicit forced-software outcome.
        let placement = super::select_live_decode_placement(
            &NullLoadPoller::new(),
            &island(FakeTwoGpu::GPU1_UUID, 1, "1"),
            1920,
            1080,
            Rational::new(25, 1),
        );
        assert_eq!(placement, super::DecodePlacement::SoftwareOnly);
        assert_eq!(super::decoder_open_args(&placement, None), (false, None));
    }

    #[test]
    fn live_spawner_forces_software_when_no_island_was_admitted() {
        // FINDING #1 — empty island must FAIL CLOSED. Under `gpu`, startup
        // admission published NO island (scorer rejection / no NVML / no
        // admissible GPU), so the slot is empty. A runtime-added decode must
        // NOT keep the constructor `Default` (= libav default-device NVDEC, the
        // very GPU admission declined) — it must force `SoftwareOnly`. We also
        // confirm the consult is SKIPPED (no load poll) on the empty path.
        use multiview_compositor::pipeline::CanvasColor;

        let doc = r##"schema_version = 1
[canvas]
width = 320
height = 240
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
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/live-spawner-empty-island.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
        let config =
            multiview_config::MultiviewConfig::load_from_toml(doc).expect("test config parses");
        let layout = config.solve_layout().expect("test layout solves");

        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawner = LiveIngestSpawner {
            layout: std::sync::Arc::new(layout),
            canvas_color: CanvasColor::default(),
            cadence: Rational::new(25, 1),
            // EMPTY island slot — admission named no device.
            island: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            load_source: Box::new(CountingLoadSource {
                polls: std::sync::Arc::clone(&polls),
            }),
        };

        let placement = spawner.decode_placement_for("live1");
        assert_eq!(
            placement,
            super::DecodePlacement::SoftwareOnly,
            "an empty island (admission named none) must FORCE software, never Default"
        );
        assert_eq!(
            super::decoder_open_args(&placement, None),
            (false, None),
            "the empty-island decode open must not WANT hardware at all"
        );
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the empty-island path must SKIP the admission consult (no load poll)"
        );
    }

    /// A spy [`LoadSource`]: counts polls (proof the spawn path re-polls the
    /// admission inputs at decision time) and otherwise answers as the fake
    /// two-GPU host.
    struct CountingLoadSource {
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl LoadSource for CountingLoadSource {
        fn poll(&self) -> Vec<DeviceLoad> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            FakeTwoGpu.poll()
        }

        fn device_target(&self, device: &DeviceId) -> Option<GpuTargetInfo> {
            FakeTwoGpu.device_target(device)
        }
    }

    #[test]
    fn live_spawner_consults_the_admission_path_on_every_decoded_spawn() {
        // THE SEAM PIN (ADR-W018 §7): `LiveIngestSpawner::spawn` — the hub's
        // decoded-producer path — must consult the admission scorer (observed
        // via the injected load-source spy: exactly one fresh poll per spawn)
        // when an island is pinned, and still spawn the SAME supervised
        // ingest producer either way.
        use crate::live_sources::{SourceSpawn, SpawnedProducer};
        use multiview_compositor::pipeline::CanvasColor;

        let doc = r##"schema_version = 1
[canvas]
width = 320
height = 240
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
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/live-spawner-consult.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
        let config =
            multiview_config::MultiviewConfig::load_from_toml(doc).expect("test config parses");
        let layout = config.solve_layout().expect("test layout solves");

        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawner = LiveIngestSpawner {
            layout: std::sync::Arc::new(layout),
            canvas_color: CanvasColor::default(),
            cadence: Rational::new(25, 1),
            island: std::sync::Arc::new(arc_swap::ArcSwapOption::from_pointee(island(
                FakeTwoGpu::GPU1_UUID,
                1,
                "1",
            ))),
            load_source: Box::new(CountingLoadSource {
                polls: std::sync::Arc::clone(&polls),
            }),
        };

        let source: multiview_config::Source = serde_json::from_value(serde_json::json!({
            "id": "live1", "kind": "file", "path": "/nonexistent/clip.ts"
        }))
        .expect("test source parses");
        let store = std::sync::Arc::new(
            multiview_framestore::TileStore::<super::Nv12Image>::with_defaults("live1"),
        );
        let registry = crate::live_sources::stop_registry();
        let produced = crate::live_sources::IngestSpawner::spawn(
            &spawner,
            SourceSpawn { source, store },
            &registry,
        );

        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the decoded spawn path must consult the admission inputs exactly \
             once (a fresh measured-load poll at decision time)"
        );
        let Some(SpawnedProducer { stop, handle }) = produced else {
            panic!("the spawner must spawn the supervised ingest producer");
        };
        assert!(
            registry.lock().is_ok_and(|map| map.contains_key("live1")),
            "the spawned producer registers its per-source stop flag"
        );
        // The nonexistent file fails its open and the finite ingest thread
        // ends on its own; raise the flag anyway and join (bounded by the
        // thread's own prompt exit) so the test leaks nothing.
        stop.store(true, std::sync::atomic::Ordering::Release);
        handle.join().expect("ingest thread joins");
    }
}

#[cfg(test)]
mod encoder_format_tests {
    use super::{encoder_input_format, output_codec};
    use ffmpeg_next::format::Pixel;
    use multiview_config::Output;

    /// Deserialize a JSON `[[outputs]]` fragment into an [`Output`] (the
    /// `#[non_exhaustive]`, `#[serde(tag = "kind")]` enum has no cross-crate
    /// struct literal).
    fn output(json: serde_json::Value) -> Output {
        serde_json::from_value::<Output>(json).expect("output fragment parses")
    }

    #[test]
    fn webrtc_output_codec_drives_the_program_encoder() {
        // ADR-0049: a WHEP-serve `webrtc` output consumes the encode-once program
        // rendition, which MUST be the codec it names (default h264). The program
        // encoder is resolved from the first output naming a codec — so a
        // webrtc-only config must select h264, not silently fall back to mpeg2video
        // (which the SRTP packetizer cannot carry). This is the live-path defect.
        let whep = output(serde_json::json!({
            "kind": "webrtc", "label": "Program WHEP", "codec": "h264"
        }));
        assert_eq!(
            output_codec(&whep),
            Some("h264"),
            "a webrtc output names its codec for the program encoder"
        );
        // The default codec (field omitted) is also surfaced as h264.
        let whep_default = output(serde_json::json!({
            "kind": "webrtc", "label": "Program WHEP"
        }));
        assert_eq!(
            output_codec(&whep_default),
            Some("h264"),
            "a webrtc output's default codec (h264) drives the program encoder"
        );
    }

    #[test]
    fn whip_push_output_codec_drives_the_program_encoder() {
        // ADR-0049: a whip_push output likewise consumes the encoded program; its
        // named codec (default h264) must select the program encoder.
        let push = output(serde_json::json!({
            "kind": "whip_push", "url": "https://[2001:db8::1]:8443/whip/p", "codec": "h264"
        }));
        assert_eq!(
            output_codec(&push),
            Some("h264"),
            "a whip_push output names its codec for the program encoder"
        );
    }

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
        /// The output's stable config id, for the ADR-0060 `output` resource
        /// scope so this sink's libav mux lines name the output.
        id: String,
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
        /// The output's stable config id, for the ADR-0060 `output` resource
        /// scope so this sink's libav segment/mux lines name the output.
        id: String,
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
        /// The output's stable config id, for the ADR-0060 `output` resource
        /// scope so this push's libav mux/transport lines name the output.
        id: String,
        /// The packet-fed push muxer.
        sink: PacketMuxSink,
        /// A short transport label (`rtmp`/`srt`) for the run report + logs.
        label: &'static str,
        /// The destination URL (for the run report + logs).
        url: String,
    },
    /// A WebRTC program output (`webrtc` WHEP-serve / `whip_push`): a **mux-free**
    /// sink that re-stamps each coded [`EncodedPacket`] into an
    /// [`EgressSample`](multiview_webrtc::egress::EgressSample) and pushes it onto
    /// a bounded drop-oldest [`EgressSink`](multiview_webrtc::egress::EgressSink)
    /// — the encode-once program AUs the WHEP-serve driver / `whip_push` client
    /// packetize into SRTP per session (invariant #7: no re-encode; per-viewer
    /// cost is packetization only). A stalled viewer / dead WHIP target drops on
    /// the feed, never stalling the fan-out (invariants #1/#10). Only under
    /// `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    WebRtc {
        /// The output's stable config id, for the ADR-0060 `output` resource
        /// scope (this mux-free sink emits no libav lines, but our own logs on
        /// its runner thread are still attributed to the output).
        id: String,
        /// The bounded drop-oldest egress sink the program AUs are pushed onto.
        sink: multiview_webrtc::egress::EgressSink,
        /// A short label (`webrtc`/`whip_push`) + the output id for the report.
        label: String,
    },
    /// An NDI program output (OUT-4b / NDI-L2): a **raw-canvas** sink that sends
    /// the composited pre-encode NV12 canvas (converted NV12→UYVY at the
    /// host-copy boundary, ADR-0004) to a single NDI source — a distinct canvas
    /// consumer, NOT a packet sink (it never joins the encode-once fan-out, like
    /// the display head). The live sender is gated by an **accepted** NDI license
    /// by construction (`NdiOutput::new`), so this variant only exists once the
    /// `[system.ndi] accept_license` gate has passed. The engine publishes each
    /// canvas into the paired wait-free mailbox; this runner reads the
    /// newest-wins slot, so a slow/absent NDI consumer drops at the tap and can
    /// never back-pressure the engine (invariants #1 + #10). Built only under
    /// `ndi-bindings` (the live `SdkNdiApi` over the licensed SDK table).
    #[cfg(feature = "ndi-bindings")]
    Ndi {
        /// The output's stable config id (ADR-0060 `output` resource scope).
        id: String,
        /// The live, license-gated NDI sender (host-memory canvas frames only).
        sink: NdiOutput<multiview_output::ndi::SdkNdiApi>,
        /// The sink-side reader of the wait-free canvas tap the engine publishes
        /// into; newest-wins (drop-oldest), so the engine never blocks here.
        reader: multiview_output::display::FrameReader<NdiCanvasFrame>,
        /// The output cadence (exact rational) the NDI 100 ns timecode + the
        /// frame-rate descriptor are derived from (invariant #3).
        cadence: Rational,
        /// The advertised NDI source name (for the run report).
        name: String,
    },
    /// An AES67 / ST 2110-30 raw-PCM audio output (#103, ADR-0033/T013): a
    /// **mux-free** sink that multicasts the mixed program audio as L16/L24 RTP.
    /// It consumes NO coded packets — the packet fan-out `rx` is used purely as
    /// the end-of-program pulse (like the NDI / display heads) — because the
    /// program audio arrives out-of-band via the paired
    /// [`Aes67SenderHandle`](multiview_output::aes67::Aes67SenderHandle) the bake
    /// consumer pushes each post-loudnorm block into. The serve side
    /// ([`Aes67Sender`](multiview_output::aes67::Aes67Sender)) drains that shared
    /// drop-oldest FIFO on its OWN media-clock timer and sends UDP; a slow/absent
    /// network drops at the FIFO and can never back-pressure the bake consumer, let
    /// alone the engine (invariants #1 + #10). Only under `aes67`.
    #[cfg(feature = "aes67")]
    Aes67 {
        /// The output's stable config id (ADR-0060 `output` resource scope).
        id: String,
        /// The serve-side sender: drains the shared FIFO the bake consumer's
        /// [`Aes67SenderHandle`](multiview_output::aes67::Aes67SenderHandle) feeds
        /// and frames each packet-time of PCM into a continuous RTP packet.
        sender: multiview_output::aes67::Aes67Sender,
        /// The local bind address (an ephemeral port on the group's-family
        /// wildcard) the sender egresses from.
        local: std::net::SocketAddr,
        /// The multicast `group:port` destination the RTP is sent to.
        dest: std::net::SocketAddr,
        /// The multicast egress interface (default: OS-chosen).
        interface: multiview_output::aes67::transport::MulticastInterface,
        /// A short label + the output id (for the run report + logs).
        label: String,
    },
}

impl RunnableOutput {
    /// The output's stable config id, for the ADR-0060 `output` resource scope
    /// [`run_one_output`] enters so this sink's logs (ours and libav's mux
    /// lines) name the output.
    fn id(&self) -> &str {
        match self {
            Self::File { id, .. } | Self::Hls { id, .. } | Self::Push { id, .. } => id,
            #[cfg(feature = "webrtc-native")]
            Self::WebRtc { id, .. } => id,
            #[cfg(feature = "ndi-bindings")]
            Self::Ndi { id, .. } => id,
            #[cfg(feature = "aes67")]
            Self::Aes67 { id, .. } => id,
        }
    }
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
    /// The **analog** clock faces requested by `[[overlays]]` entries with
    /// `kind = "clock"` + `face = "analog"` — one face per entry, working-set
    /// order (ADR-W022). Empty ⇒ only the default digital clock label is drawn.
    #[cfg(feature = "overlay")]
    analog_clocks: Vec<crate::overlays::AnalogClockSpec>,
    /// ADR-W022: the live overlay working-set slot, seeded with the boot
    /// config's overlays (generation 0). The command drain publishes each
    /// applied overlay change through it (via
    /// [`overlay_apply_slot`](Self::overlay_apply_slot) → the binary's drain
    /// wiring) and the bake consumer re-derives its overlay render state from
    /// it at the next frame — a lock-free `ArcSwap`, so neither side can pace
    /// the other (inv #1/#10).
    #[cfg(feature = "overlay")]
    overlay_apply: crate::live_overlays::OverlayApplySlot,
    /// Per-source last-good-frame stores, keyed by source id. Shared (`Arc`)
    /// between the engine's drive loop (reader) and the ingest threads (writers).
    /// Each entry is an `Arc` clone of the store the [`source_registry`](Self::source_registry)
    /// owns (MP-2, ADR-0030 §3) — this map is a lock-free reader, not the owner.
    stores: HashMapStores,
    /// MP-2 (ADR-0030 §3): the source registry that OWNS each source's shared
    /// [`TileStore`] and its decode sizing (the per-axis supremum across that
    /// source's cells). [`stores`](Self::stores) and each ingest plan hold `Arc`
    /// clones of the registry's stores; the registry is the single owner. Keyed by
    /// source id in this single-program build (one entry per id, exactly the old
    /// `stores` map); the URL-canonical cross-id share is a later milestone.
    source_registry: Arc<SourceRegistry<Nv12Image>>,
    /// The registry references this pipeline holds — one [`SourceHandle`] per
    /// startup source — keeping each registry entry (and its shared store) alive for
    /// the pipeline's lifetime (the program's reference to the source). Dropping the
    /// pipeline drops these, releasing each reference (last-release removes the entry).
    // reason: held purely for that RAII `Drop`; it is never read, and the dead_code
    // lint cannot observe a Drop side-effect (rule 20 justification).
    #[allow(dead_code)]
    source_handles: Vec<SourceHandle<Nv12Image>>,
    /// The per-source producer stop flags (ADR-W018): every startup ingest
    /// thread registers its flag here, and the live-source hub shares the same
    /// registry, so a live `RemoveSource` can tear down exactly one producer.
    stop_registry: crate::live_sources::StopRegistry,
    /// The pinned island slot (ADR-W018 §7): `drive_streaming` publishes the
    /// admission pick's device here so the [`LiveIngestSpawner`] places every
    /// runtime-added decode on the SAME island device (never a fragment/
    /// migration). Empty until — and unless — admission names a device.
    #[cfg(feature = "gpu")]
    live_island: LiveIslandSlot,
    /// Per-source streaming ingest plans: how to open + decode each source, and
    /// the tile size its frames are scaled to. The drive starts one decode
    /// thread per plan; the threads publish into [`Self::stores`] as frames
    /// arrive (never buffered ahead of the clock — the BUG-2 fix).
    ingest_plans: Vec<IngestPlan>,
    /// Per-media-player transport mailboxes (ADR-0057 / ADR-0097), keyed by
    /// player id: the bounded two-class seam (state verbs conflated latest-wins;
    /// targeted load/cue/seek a bounded drop-oldest FIFO) the control-plane
    /// command drain submits transport verbs to, drained by each player's ingest
    /// thread between frames. Exposed to the run wiring via
    /// [`Self::player_mailboxes`] so the command drain
    /// ([`crate::control::command_drain_with_seams`]) can address every declared
    /// channel. Empty when no media players are configured.
    player_mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>>,
    /// The shared WHIP publisher rendezvous (ADR-T014): the control plane's
    /// `WhipProvider` writes a negotiated publisher into it and each webrtc
    /// source's `drive_webrtc` loop reads. Exposed to the run wiring via
    /// [`Self::webrtc_registry`] so the same registry backs the provider. Only
    /// under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    webrtc_registry: crate::webrtc_ingest::WhipRegistry,
    /// The shared WebRTC **output** egress rendezvous (ADR-0049): one
    /// [`EgressFeed`](multiview_webrtc::egress::EgressFeed) per `webrtc`/`whip_push`
    /// output, fed the encode-once program AUs by that output's
    /// `RunnableOutput::WebRtc` sink runner; the run wiring hands it to the
    /// WHEP-serve provider (viewers) + the `whip_push` clients. Only under
    /// `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    egress_registry: crate::webrtc_outputs::EgressRegistry,
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
    /// Media-player channel **audio loop** plans (ADR-T019): how to prime + loop
    /// each player's embedded audio onto the program bus on the same wrap instant
    /// as the video. The drive starts one player-audio loop thread per plan,
    /// writing into that player's [`Self::audio_stores`] entry (routed onto the bus
    /// like any source's audio). Built only for media players whose default asset
    /// declares a vamp window; empty when this run did not opt into program audio.
    player_audio_ingest_plans: Vec<crate::audio::PlayerAudioPlan>,
    /// Per-source **AES67 / ST 2110-30** audio RX plans (#103, ADR-0033/T013):
    /// how to bind + receive each AES67 multicast PCM source. The drive starts one
    /// supervised RX thread per plan (a peer of the audio decode threads) that
    /// depacketizes → rebases (ADR-T013) → publishes into that source's
    /// [`Self::audio_stores`] entry, which the program bus samples like any
    /// source's audio. An AES67 source is AUDIO-ONLY — it takes no video
    /// [`TileStore`], no [`source_registry`](Self::source_registry) entry, and no
    /// layout tile. Built only for `SourceKind::Aes67` sources; spawned only when
    /// this run opted into program audio (else there is no bus to consume the
    /// store). Only under the off-by-default `aes67` feature.
    #[cfg(feature = "aes67")]
    aes67_rx_plans: Vec<Aes67RxPlan>,
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
    /// The bake-consumer push handles for the configured AES67 outputs (#103),
    /// paired with the serve-side `Aes67Sender` inside each `RunnableOutput::Aes67`.
    /// `drive_streaming` hands these to the bake consumer, which pushes every
    /// post-loudnorm program block into each (drop-oldest) — exactly like it feeds
    /// the display heads. A slow/absent AES67 network drops at the FIFO and can
    /// never back-pressure the consumer or the engine (invariants #1 + #10). Empty
    /// when no AES67 output is configured. Only under `aes67`.
    #[cfg(feature = "aes67")]
    aes67_send_handles: Vec<multiview_output::aes67::Aes67SenderHandle>,
    /// The configured DRM/KMS display heads (DEV-B1 / ADR-0044, feature
    /// `display-kms`): **raw-frame** sinks fed the pre-encode NV12 canvas
    /// through wait-free mailboxes — never part of the packet fan-out. Taken
    /// (and started) once at stream start; the sinks live for the run.
    #[cfg(feature = "display-kms")]
    display_plans: Vec<DisplayOutputPlan>,
    /// The engine-side canvas-tap publishers for the configured NDI outputs
    /// (OUT-4b / NDI-L2, feature `ndi-bindings`): the hot loop publishes the
    /// shared pre-encode NV12 canvas `Arc` into each (wait-free, newest-wins),
    /// exactly like the display heads — the matching `FrameReader` lives inside
    /// the `RunnableOutput::Ndi` in [`Self::outputs`]. Taken at stream start and
    /// fed in the per-tick projection; the engine never blocks here (inv #1/#10).
    #[cfg(feature = "ndi-bindings")]
    ndi_publishers: Vec<multiview_output::display::FramePublisher<NdiCanvasFrame>>,
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
    /// The shared outbound presentation epoch (DEV-C1 / ADR-M010): written at
    /// ~1 Hz by the timing-status task, read by the HLS rolling playlist at
    /// each segment close to stamp `EXT-X-PROGRAM-DATE-TIME` from the same
    /// map the control WS publishes. Cloned into each HLS sink at build time.
    epoch: multiview_output::SharedEpoch,
    /// The run's epoch **anchor** slot (DEV-C1): `drive_streaming` stores the
    /// tick-0 seed + the run's monotonic source here when the program clock
    /// seeds; the timing-status task binds to it lazily (lock-free load —
    /// publishing/reading can never block the engine, inv #1/#10).
    epoch_anchor: crate::timing_status::EpochAnchorSlot,
    /// The **program-audio preview tap** (ADR-P006 audio): when set (and this run
    /// carries program audio), the bake consumer pushes each emitted post-loudnorm
    /// [`AudioBlock`](multiview_audio::format::AudioBlock) into this bounded
    /// drop-oldest slot, which the live WHEP egress provider drains to Opus-encode
    /// and send to the peer. `None` on a build/run with no WHEP egress wired, in
    /// which case the consumer pushes nothing (zero overhead). A preview tap off
    /// the bake consumer thread — never the output-clock loop; a slow/absent WHEP
    /// consumer only loses the oldest blocks (inv #1/#10).
    program_audio_preview: Option<crate::preview::ProgramAudioSlot>,
}

type HashMapStores = std::collections::HashMap<String, Arc<TileStore<Nv12Image>>>;

/// Where a source's video decode is allowed to open — the explicit tri-state
/// outcome of decode placement (ADR-W018 §7 / ADR-0018's never-fragment rule).
///
/// `Option<ordinal>` cannot express this: "no ordinal" must distinguish *no
/// placement decision* (hardware on libav's default device is fine — today's
/// GPU-free behaviour) from *placement rejected* (hardware must NOT open: on a
/// single-GPU host the default device IS the over-headroom island — admitting
/// would overcommit it — and on a multi-GPU host the default device may be a
/// **different** GPU, silently fragmenting the pipeline island). The
/// tri-state is closed by design (no `#[non_exhaustive]`): a placement
/// outcome is exactly one of default / pinned / forced-software.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DecodePlacement {
    /// No placement decision: hardware decode (when the build/runtime offers
    /// it) on libav's default device — the GPU-free / no-admission path.
    #[default]
    Default,
    /// Pinned to the admission-chosen island device's CUDA enumeration
    /// ordinal: NVDEC opens co-located with the compositor (affinity).
    Pinned(String),
    /// Placement REJECTED this source: the decoder open is forced to
    /// **software** regardless of the build's hardware capability, so the
    /// island is never overcommitted and never fragmented.
    SoftwareOnly,
}

/// The `(want_hw, cuda_device)` pair [`open_and_stream`] hands
/// [`StreamVideoDecoder::new_preferring_hw`] for a plan's [`DecodePlacement`].
///
/// This is **the** decode-open gate for placement (ADR-W018 §7):
/// [`DecodePlacement::SoftwareOnly`] forces `(false, None)` — the hardware
/// path is never attempted, even when NVDEC is compiled, present, and not
/// env-disabled. The other placements keep the canonical hardware preference
/// ([`multiview_ffmpeg::want_hw_decode`] over the `MULTIVIEW_DISABLE_NVDEC`
/// reading) and thread the pinned ordinal through when one exists (the
/// operator opt-out still wins over a pin).
#[must_use]
pub fn decoder_open_args<'p>(
    placement: &'p DecodePlacement,
    nvdec_disable_env: Option<&str>,
) -> (bool, Option<&'p str>) {
    match placement {
        DecodePlacement::SoftwareOnly => (false, None),
        DecodePlacement::Pinned(ordinal) => (
            multiview_ffmpeg::want_hw_decode(nvdec_disable_env),
            Some(ordinal.as_str()),
        ),
        DecodePlacement::Default => (multiview_ffmpeg::want_hw_decode(nvdec_disable_env), None),
    }
}

/// Everything one source needs to be ingested on its own decode thread: where
/// its media lives, the store to publish into, the tile size to scale to, the
/// canvas color tag, and whether it is a live (never-ending) source.
struct IngestPlan {
    /// The source id (for diagnostics / store keying).
    id: String,
    /// Where the media lives.
    location: SourceLocation,
    /// When `Some`, this source is a **media-player channel** (ADR-0057 /
    /// ADR-0097): [`open_and_stream`] runs the transport-driven decode loop
    /// ([`stream_player`]) instead of the plain pace-and-publish pump — it
    /// consults the thread-local [`MediaPlayer`](crate::player::MediaPlayer)
    /// per decoded frame, performs the in-place loop seek + decoder flush at a
    /// wrap, and stamps each frame from the player's own monotone
    /// output-anchored timeline (bypassing the [`PtsNormalizer`]). The shared
    /// [`TransportMailbox`](crate::player::TransportMailbox) inside the handle
    /// is the only control-plane seam, drained between frames. `None` for every
    /// non-player source (the existing ingest path is unchanged).
    player: Option<crate::player::PlayerHandle>,
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
    /// Where this source's video decode opens ([`DecodePlacement`]):
    /// [`DecodePlacement::Pinned`] to the admission-chosen island device
    /// (ADR-0035 Tier-1 — stamped in [`Pipeline::drive_streaming`] before the
    /// startup threads spawn, or by the live placement consult on an admitted
    /// runtime add), [`DecodePlacement::Default`] when no placement decision
    /// exists (GPU-free / no NVML — in lockstep with the compositor's default
    /// adapter), or [`DecodePlacement::SoftwareOnly`] when the live consult
    /// REJECTED this source (ADR-W018 §7 — hardware decode must not open at
    /// all, lest it land back on the over-headroom island or fragment onto a
    /// different GPU). Consumed by [`open_and_stream`] → [`decoder_open_args`]
    /// → [`StreamVideoDecoder::new_preferring_hw`].
    decode_placement: DecodePlacement,
    /// The shared WHIP publisher rendezvous (ADR-T014), present only for a
    /// `webrtc` source under `webrtc-native`: [`drive_webrtc`] samples this
    /// registry for the source's connected publisher (the negotiated RTP ring).
    /// `None` for every other source kind / build.
    #[cfg(feature = "webrtc-native")]
    webrtc_registry: Option<crate::webrtc_ingest::WhipRegistry>,
    /// The source's `AudioStore` for a `webrtc` source's de-embedded Opus
    /// (ADR-T014 §5): WHIP video + audio arrive on one RTP flow, so the WHIP
    /// drive loop publishes the decoded 48 kHz PCM here directly (unlike file/URL
    /// sources, whose audio is a separate decode thread). `None` when audio is
    /// off / not a webrtc source. Only under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    webrtc_audio_store: Option<Arc<multiview_audio::store::AudioStore>>,
    /// The `[system.ndi] accept_license` flag for this run (ADR-0008 §7.5),
    /// stamped from `config.system` in [`Pipeline::build`]. Defaults to `false`
    /// (declined): an NDI source is refused (`ndi_unlicensed`) and never starts
    /// receiving until the operator accepts.
    #[cfg(feature = "ndi")]
    ndi_accept_license: bool,
    /// The audited acceptance record (who/when) the NDI license gate consumes via
    /// `NdiLicense::from_setting` at the receive construction point
    /// ([`connect_ndi_receiver`]). Empty fields when unaccepted.
    #[cfg(feature = "ndi")]
    ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance,
    /// The **swappable resolved-URL slot** for a `YouTube` source (IN-5b): the
    /// lock-free cell the **proactive** re-resolution loop ([`run_reresolve_loop`])
    /// publishes each freshly resolved `*.googlevideo.com` HLS master into, ahead
    /// of the active URL's `expire` deadline (make-before-break, ADR-0015 P2–P4).
    /// [`open_and_stream`] reads it on every (re)open and prefers a fresh URL over
    /// resolving inline — so when the active manifest ages out and the reconnect
    /// bracket re-enters, the next-up URL is already in hand and the tile reopens
    /// without waiting on a cold `yt-dlp` resolve (it rides last-good across the
    /// brief reopen rather than degrading on a synchronous resolve at the boundary).
    ///
    /// The loop runs on its **own supervised thread** (a sibling of the decode
    /// thread, off the data plane); it only ever *writes* this lock-free slot, so
    /// it can neither pace nor stall the output clock (invariant #1) nor
    /// back-pressure the engine (invariant #10). `None` for every non-`YouTube`
    /// source / build (the slot is allocated per youtube plan in
    /// [`ingest_plan_for`]). The bridge thread is spawned in
    /// [`IngestSupervisor::start`].
    #[cfg(feature = "youtube")]
    youtube_url_slot: Option<Arc<arc_swap::ArcSwapOption<String>>>,
}

/// Everything one AES67 / ST 2110-30 audio source needs to be received on its own
/// supervised RX thread (#103, ADR-0033/T013): the resolved SDP session (PCM
/// format + RTP clock + payload type), the multicast `group:port` to bind + join,
/// and the last-good [`AudioStore`](multiview_audio::store::AudioStore) its
/// rebased 48 kHz PCM is published into (shared with the program bus). Audio-only
/// — there is no [`TileStore`], no cell, and no video decode.
#[cfg(feature = "aes67")]
struct Aes67RxPlan {
    /// The source id (diagnostics + the run's stop-flag key).
    id: String,
    /// The parsed SDP session: PCM format (channels + L16/L24), RTP clock rate,
    /// and dynamic payload type. The multicast binding is deliberately NOT read
    /// from here (the SDP parser ignores the `c=` line — the transport binding is
    /// a config concern); it comes from [`Self::group`].
    session: multiview_input::st2110::sdp::AudioSdpSession,
    /// The resolved multicast `group:port` (from the config `multicast` override)
    /// the receiver binds the port of and joins the group of.
    group: std::net::SocketAddr,
    /// The last-good `AudioStore` the rebased 48 kHz PCM is published into — shared
    /// with the [`ProgramBus`](multiview_audio::program::ProgramBus), which samples
    /// it per tick (a best-effort writer of a lock-free store, inv #1/#10).
    store: Arc<multiview_audio::store::AudioStore>,
}

/// Reject a layout cell bound to an audio-only AES67 source (#103).
///
/// An AES67 / ST 2110-30 source decodes no pixels — it has no `TileStore` — so a
/// layout cell referencing it would carry tile geometry with nothing to
/// composite. This checks the RAW config bindings (`cells[].source.input_id`),
/// independent of geometry solving, so the audio-binding is rejected with a clear
/// message even when the cell's area does not resolve — which `solve_layout` would
/// otherwise report as a generic "unknown grid area" error, masking the root
/// cause. Fail-closed, and decoupled from any tile-sizing predicate.
///
/// # Errors
///
/// [`PipelineError::Config`] naming the source when any cell binds an AES67 source.
#[cfg(feature = "aes67")]
fn ensure_no_cell_binds_an_aes67_source(config: &MultiviewConfig) -> Result<(), PipelineError> {
    for cell in &config.cells {
        let Some(bound) = cell.source.input_id.as_deref() else {
            continue;
        };
        let binds_aes67 = config
            .sources
            .iter()
            .any(|source| source.id == bound && matches!(source.kind, SourceKind::Aes67 { .. }));
        if binds_aes67 {
            return Err(PipelineError::Config(
                multiview_config::ConfigError::Validation(format!(
                    "layout cell bound to audio-only AES67 source `{bound}`: an AES67 / \
                     ST 2110-30 source carries no video and cannot occupy a layout tile"
                )),
            ));
        }
    }
    Ok(())
}

/// Resolve an AES67 source's SDP session + multicast `group:port` binding (#103).
///
/// The SDP (RFC 4566/8866) carries the PCM format, RTP clock, and payload type;
/// the multicast binding comes from the config `multicast` override (the SDP
/// parser ignores the `c=` connection line by design — the transport binding is a
/// config concern, `multiview-input`'s `st2110::sdp` module contract). A
/// missing/malformed SDP or a missing/invalid override is a typed refusal at build
/// time (never a silent skip — the fail-closed contract).
///
/// # Errors
/// [`PipelineError::Ingest`] when the source is not AES67, the SDP does not parse,
/// the `multicast` override is absent, or it is not a valid `group:port`.
#[cfg(feature = "aes67")]
fn resolve_aes67_source(
    source: &Source,
) -> Result<
    (
        multiview_input::st2110::sdp::AudioSdpSession,
        std::net::SocketAddr,
    ),
    PipelineError,
> {
    let SourceKind::Aes67 { sdp, multicast, .. } = &source.kind else {
        return Err(PipelineError::Ingest {
            id: source.id.clone(),
            reason: "resolve_aes67_source called on a non-aes67 source".to_owned(),
        });
    };
    let session = multiview_input::st2110::sdp::AudioSdpSession::parse(sdp).map_err(|e| {
        PipelineError::Ingest {
            id: source.id.clone(),
            reason: format!("aes67 sdp parse failed: {e}"),
        }
    })?;
    let group_str = multicast.as_deref().ok_or_else(|| PipelineError::Ingest {
        id: source.id.clone(),
        reason: "aes67 source requires a `multicast` group:port override (e.g. \
                 \"[ff3e::1]:5004\"); the SDP connection line is not used for the \
                 transport binding"
            .to_owned(),
    })?;
    let group = group_str
        .parse::<std::net::SocketAddr>()
        .map_err(|e| PipelineError::Ingest {
            id: source.id.clone(),
            reason: format!("aes67 multicast `{group_str}` is not a valid group:port: {e}"),
        })?;
    // The RX publishes into the canonical 48 kHz program-audio store and the
    // shared ADR-T013 rebaser only rescales the RTP *timestamp* onto that index —
    // it does NOT resample the PCM. A non-48 kHz wire clock would deliver a sample
    // count per packet that disagrees with the rebased store cadence
    // (overlaps/gaps/wrong pitch), so a non-48 kHz session is rejected fail-closed
    // rather than silently mis-timed. (A resampling RX is a later slice.)
    if session.clock_rate != AES67_STORE_RATE_HZ {
        return Err(PipelineError::Ingest {
            id: source.id.clone(),
            reason: format!(
                "aes67 session clock rate {} Hz is unsupported: only 48 kHz \
                 ST 2110-30 sessions are supported (the RX publishes at the \
                 canonical 48 kHz store rate and does not resample)",
                session.clock_rate
            ),
        });
    }
    Ok((session, group))
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
        // Fail-closed (#103): an AES67 PCM-audio source in a non-`aes67` build is a
        // config this binary cannot honour — reject it clearly up front, never wire
        // it dark (the same contract as a `display` source/output in a non-
        // `display-kms` build).
        crate::outputs::ensure_aes67_sources_supported(&config.sources).map_err(|reason| {
            PipelineError::Config(multiview_config::ConfigError::Validation(reason))
        })?;
        // #103: an AES67 / ST 2110-30 source is AUDIO-ONLY (no TileStore). Reject a
        // layout cell bound to one here — on the RAW config bindings, BEFORE
        // `solve_layout` — so the audio-binding is named clearly and rejected
        // whether or not the cell's geometry resolves (a bad-geometry cell would
        // otherwise be masked by `solve_layout`'s generic area error, and the guard
        // must not depend on tile-sizing at all). Fail-closed.
        #[cfg(feature = "aes67")]
        ensure_no_cell_binds_an_aes67_source(config)?;
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
        // #103: AES67 / ST 2110-30 audio RX plans. Each `SourceKind::Aes67` source
        // is AUDIO-ONLY — the source loop below SKIPS the entire video path for it
        // (no TileStore, no registry entry, no ingest plan) and queues one RX plan
        // here, spawned in `drive_streaming`. Only under the `aes67` feature; a
        // non-`aes67` build rejected any aes67 source up front (the gate above).
        #[cfg(feature = "aes67")]
        let mut aes67_rx_plans: Vec<Aes67RxPlan> = Vec::new();

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

        // MP-2 (ADR-0030 §3): the SourceRegistry owns each source's shared TileStore
        // and its decode sizing. In this single-program build it keys by source id —
        // one entry per id, exactly today's `stores` map; a later milestone folds the
        // URL-canonical StableStreamId so two ids at one location share ONE decode.
        // The decode threads spawn later (drive_streaming) and their stop/join stays
        // with the run's StopRegistry, so store creation goes through `acquire_store`
        // (no decode actor to own yet). Holding each handle keeps its entry alive for
        // the pipeline's lifetime (the program's reference); dropping it releases.
        let source_registry = SourceRegistry::<Nv12Image>::new();
        let mut source_handles: Vec<SourceHandle<Nv12Image>> =
            Vec::with_capacity(config.sources.len());

        for source in &config.sources {
            // #103: an AES67 / ST 2110-30 source is AUDIO-ONLY. It decodes no
            // pixels, so it takes NO video TileStore, NO SourceRegistry entry, and
            // NO layout tile — the MP-2 decode-once video seam (ADR-0030 §3) never
            // sees it. It contributes an AudioStore (routed onto the program bus
            // like any source's audio, so it "joins" the bus at the routing loop
            // below) plus a supervised RX plan (bound + received on its own thread
            // in `drive_streaming`). Skip the entire video path for it.
            #[cfg(feature = "aes67")]
            if matches!(source.kind, SourceKind::Aes67 { .. }) {
                // An AES67 source decodes no pixels, so it takes NO video path (no
                // TileStore / registry entry / layout tile). A cell bound to it is
                // already rejected up front by `ensure_no_cell_binds_an_aes67_source`
                // (before `solve_layout`), so here it is purely audio-only.
                let (session, group) = resolve_aes67_source(source)?;
                let store = crate::audio::new_store();
                audio_stores.insert(source.id.clone(), Arc::clone(&store));
                aes67_rx_plans.push(Aes67RxPlan {
                    id: source.id.clone(),
                    session,
                    group,
                    store,
                });
                continue;
            }
            let (tile_w, tile_h) = cell_pixel_size(&layout, &source.id)
                .unwrap_or((config.canvas.width, config.canvas.height));
            // The registry owns the shared store, sized to the per-axis supremum;
            // `stores` + the ingest plan below hold Arc clones (lock-free readers).
            let source_handle = source_registry.acquire_store(
                SourceKey::from_canonical(source.id.as_str()),
                RequestedSize {
                    width: tile_w,
                    height: tile_h,
                },
                |_requested| -> Result<Arc<TileStore<Nv12Image>>, PipelineError> {
                    Ok(Arc::new(TileStore::new(
                        source.id.clone(),
                        TileThresholds::default(),
                        NoSignalPolicy::HoldForever,
                    )))
                },
            )?;
            let store = Arc::clone(source_handle.store());
            source_handles.push(source_handle);
            #[cfg_attr(not(feature = "overlay"), allow(unused_mut))]
            let mut plan = ingest_plan_for(
                source,
                tile_w,
                tile_h,
                Arc::clone(&store),
                canvas_color,
                cadence,
            )?;

            // Stamp this source's NDI license acceptance from `[system.ndi]`
            // (ADR-0008 §7.5), like `cuda_ordinal` below — the per-source drive
            // loop's gate (`connect_ndi_receiver`) reads it before any receive
            // starts. Absent/false ⇒ declined (the plan's `false` default stands)
            // and the source is refused (`ndi_unlicensed`).
            #[cfg(feature = "ndi")]
            if let Some(ndi) = config.system.as_ref().and_then(|s| s.ndi.as_ref()) {
                plan.ndi_accept_license = ndi.accept_license;
                plan.ndi_acceptance = multiview_input::ndi::license::LicenseAcceptance {
                    accepted_by: ndi.accepted_by.clone().unwrap_or_default(),
                    accepted_at: ndi.accepted_at.clone().unwrap_or_default(),
                };
            }

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

        // Boot-spawn the configured media-player channels (ADR-0057 / ADR-0097):
        // each rolling player gets its own player `IngestPlan` (driven by the
        // transport state machine) + last-good store; every declared player gets
        // a registered transport mailbox the control-plane command drain submits
        // verbs to. A player channel is a source like any other — the engine only
        // samples its store per tick (inv #1/#10).
        let media_player_boot = build_media_player_boot(config, &layout, cadence, canvas_color);
        let player_mailboxes = media_player_boot.mailboxes;
        for (id, store) in media_player_boot.stores {
            stores.insert(id, store);
        }
        ingest_plans.extend(media_player_boot.plans);
        // ADR-T019: register each rolling player's `AudioStore` (so the program-bus
        // routing below picks it up at unity gain, like any source's audio) and
        // collect its audio loop plan (spawned in `drive_streaming`). A player with
        // a silent asset still gets a store; its deck primes empty → it rides
        // silence (no special-casing — exactly as a silent source does).
        let player_audio_ingest_plans = media_player_boot.audio_plans;
        for (id, audio_store) in media_player_boot.audio_stores {
            audio_stores.insert(id, audio_store);
        }

        // WHIP ingest wiring (ADR-T014, `webrtc-native`): one shared publisher
        // rendezvous registry the control plane's WhipProvider writes to and each
        // webrtc source's `drive_webrtc` loop reads from. Stamp it (and, for an
        // audio-accepting source, a freshly-built AudioStore that joins the
        // program bus below) onto every webrtc ingest plan — the peer of the
        // `cuda_ordinal` stamping. Without the feature this is absent.
        #[cfg(feature = "webrtc-native")]
        let webrtc_registry = crate::webrtc_ingest::WhipRegistry::new();
        #[cfg(feature = "webrtc-native")]
        for plan in &mut ingest_plans {
            if let SourceLocation::Webrtc { audio } = &plan.location {
                plan.webrtc_registry = Some(webrtc_registry.clone());
                if *audio {
                    let store = crate::audio::new_store();
                    audio_stores.insert(plan.id.clone(), Arc::clone(&store));
                    plan.webrtc_audio_store = Some(store);
                }
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

        // The shared outbound presentation epoch (DEV-C1 / ADR-M010): one cell
        // per pipeline, written by the timing-status task and read by every
        // HLS rolling playlist (PDT) — one anchor, every surface agrees.
        let epoch = multiview_output::SharedEpoch::new();
        // The WebRTC output egress rendezvous (ADR-0049): one drop-oldest feed per
        // `webrtc`/`whip_push` output, with the paired `EgressSink` keyed by output
        // id for the sink runners. The run wiring reads the registry to bind the
        // WHEP-serve endpoint + spawn the whip_push clients. Under `webrtc-native`.
        #[cfg(feature = "webrtc-native")]
        let (egress_registry, egress_sinks) = crate::webrtc_outputs::build_egress_registry(config);
        let built = build_outputs(
            &config.outputs,
            &epoch,
            #[cfg(feature = "webrtc-native")]
            &egress_sinks,
            config.system.as_ref().and_then(|s| s.ndi.as_ref()),
            cadence,
        )?;
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
        // Read the analog clock faces from the `[[overlays]]` clock entries.
        #[cfg(feature = "overlay")]
        let analog_clocks =
            analog_clocks_from_config(&config.overlays, config.canvas.width, config.canvas.height);

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
            source_registry,
            source_handles,
            stop_registry: crate::live_sources::stop_registry(),
            #[cfg(feature = "gpu")]
            live_island: Arc::new(arc_swap::ArcSwapOption::empty()),
            ingest_plans,
            player_mailboxes,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry,
            #[cfg(feature = "webrtc-native")]
            egress_registry,
            audio_stores,
            audio_ingest_plans,
            tone_ingest_plans,
            player_audio_ingest_plans,
            #[cfg(feature = "aes67")]
            aes67_rx_plans,
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
            #[cfg(feature = "ndi-bindings")]
            ndi_publishers: built.ndi_publishers,
            #[cfg(feature = "aes67")]
            aes67_send_handles: built.aes67_handles,
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
            analog_clocks,
            #[cfg(feature = "overlay")]
            watermark_signal: None,
            #[cfg(feature = "overlay")]
            overlay_apply: crate::live_overlays::overlay_apply_slot(config.overlays.clone()),
            epoch,
            epoch_anchor: crate::timing_status::anchor_slot(),
            program_audio_preview: None,
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

    /// The number of layout tiles (cells) the canvas composites — the decode-stage
    /// demand magnitude the placement controller admits against (GPU-5c).
    #[must_use]
    pub fn tile_count(&self) -> usize {
        self.layout.cells.len()
    }

    /// The output canvas resolution (the placement controller's per-stage demand
    /// resolution, GPU-5c).
    #[must_use]
    pub fn canvas_resolution(&self) -> multiview_hal::Resolution {
        multiview_hal::Resolution::new(
            self.layout.canvas.width.max(1),
            self.layout.canvas.height.max(1),
        )
    }

    /// Whether this run's encode opens an NVENC session (the session-ceiling gate
    /// applies in placement admission, GPU-5c).
    #[must_use]
    pub fn opens_encode_session(&self) -> bool {
        self.encoder.name.ends_with("_nvenc")
    }

    /// The per-media-player transport mailboxes (ADR-0057 / ADR-0097), keyed by
    /// player id: hand this to the control-plane command drain
    /// ([`crate::control::command_drain_with_seams`] /
    /// [`crate::control::command_drain_with_live_sources`]) so a `MediaTransport`
    /// or exit command reaches the addressed running player channel. Cloned
    /// (the `Arc<TransportMailbox>` values are shared with the ingest threads).
    #[must_use]
    pub fn player_mailboxes(
        &self,
    ) -> std::collections::HashMap<String, Arc<crate::player::TransportMailbox>> {
        self.player_mailboxes.clone()
    }

    /// The shared per-source producer stop registry (ADR-W018): hand this to
    /// the [`LiveSourceHub`](crate::live_sources::LiveSourceHub) so a live
    /// remove can tear down a startup producer (ingest thread or generator).
    #[must_use]
    pub fn stop_registry(&self) -> crate::live_sources::StopRegistry {
        Arc::clone(&self.stop_registry)
    }

    /// The run's decoded-ingest spawner for the live-source hub (ADR-W018
    /// level 2): hand this to
    /// [`LiveSourceHub::start_with_ingest`](crate::live_sources::LiveSourceHub::start_with_ingest)
    /// so a runtime-added network/file source spawns the **same** supervised
    /// [`ingest_loop`] the startup path runs (one uniform ingest path), with
    /// its decode placement consulted against the **same** admission scorer —
    /// pinned to the running island's device — that placed the startup island.
    #[must_use]
    pub fn live_ingest_spawner(&self) -> Arc<dyn crate::live_sources::IngestSpawner> {
        Arc::new(LiveIngestSpawner {
            layout: Arc::clone(&self.layout),
            canvas_color: self.canvas_color,
            cadence: self.cadence,
            #[cfg(feature = "gpu")]
            island: Arc::clone(&self.live_island),
            #[cfg(feature = "gpu")]
            load_source: crate::system_metrics::default_load_source(),
        })
    }

    /// The shared WHIP publisher rendezvous registry (ADR-T014). The run wiring
    /// hands this to the control plane's `WhipProvider` so a `POST` rendezvous'd
    /// publisher reaches the matching webrtc source's `drive_webrtc` loop. Only
    /// under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    #[must_use]
    pub fn webrtc_registry(&self) -> crate::webrtc_ingest::WhipRegistry {
        self.webrtc_registry.clone()
    }

    /// The shared WebRTC **output** egress rendezvous (ADR-0049). The run wiring
    /// reads it to build the WHEP-serve provider (browser viewers) and spawn the
    /// `whip_push` clients, both fed the encode-once program over the per-output
    /// drop-oldest feed. Only under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    #[must_use]
    pub fn egress_registry(&self) -> crate::webrtc_outputs::EgressRegistry {
        self.egress_registry.clone()
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

    /// Whether this run carries **program audio** (`enable_program_audio` was
    /// called): the bake consumer mixes + encodes a program-audio stream. The
    /// binary checks this to decide whether to wire a WHEP program-audio tap.
    #[must_use]
    pub fn has_program_audio(&self) -> bool {
        self.encode_cfg.audio.is_some()
    }

    /// Fail closed if this run declares an AES67 source or output but carries no
    /// program audio (#103).
    ///
    /// The ST 2110-30 RX publishes into, and the TX taps, the **program-audio
    /// bus** — which only exists when `enable_program_audio` was called (the
    /// `--program-audio` invocation). Without it the RX would be spawned to
    /// nowhere and the TX would multicast silence, both silently. Rather than go
    /// on air doing neither, refuse the run with a clear message. A no-op when the
    /// config declares no AES67 endpoint (empty plans/handles) or when program
    /// audio is on, so non-AES67 and correctly-configured runs are unaffected.
    ///
    /// # Errors
    ///
    /// [`PipelineError::Config`] when an AES67 endpoint is configured without
    /// program audio.
    #[cfg(feature = "aes67")]
    fn ensure_aes67_has_program_audio(&self) -> Result<(), PipelineError> {
        if self.encode_cfg.audio.is_some()
            || (self.aes67_rx_plans.is_empty() && self.aes67_send_handles.is_empty())
        {
            return Ok(());
        }
        Err(PipelineError::Config(
            multiview_config::ConfigError::Validation(
                "an AES67 / ST 2110-30 source or output requires program audio: run \
                 with `--program-audio` (the ST 2110-30 RX/TX only exists on the \
                 program-audio bus)"
                    .to_owned(),
            ),
        ))
    }

    /// Attach the **program-audio preview tap** (ADR-P006 audio): the bake
    /// consumer pushes each emitted post-loudnorm program
    /// [`AudioBlock`](multiview_audio::format::AudioBlock) into `slot`, which the
    /// live WHEP egress provider drains to Opus-encode + send to the peer. The cli
    /// shares the SAME slot here and in `CliWhepProvider::spawn`, so program audio
    /// reaches a WHEP peer when (and only when) program audio is configured. A
    /// no-op tap when this run has no program audio (the consumer pushes nothing).
    /// The push is off the output-clock loop and bounded drop-oldest, so a
    /// slow/absent WHEP consumer can never back-pressure the engine (inv #1/#10).
    pub fn set_program_audio_preview(&mut self, slot: crate::preview::ProgramAudioSlot) {
        self.program_audio_preview = Some(slot);
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

    /// The source registry that owns this pipeline's per-source shared stores +
    /// their decode sizing (MP-2, ADR-0030 §3). The run/drive path reads it to
    /// resolve a source's shared decode; `stores` holds `Arc` clones of the same
    /// stores for lock-free sampling.
    #[must_use]
    pub fn source_registry(&self) -> &Arc<SourceRegistry<Nv12Image>> {
        &self.source_registry
    }

    /// DEV-C1 (ADR-M010): the shared outbound presentation-epoch cell this
    /// pipeline's HLS sinks stamp `EXT-X-PROGRAM-DATE-TIME` from. The
    /// timing-status task writes it (~1 Hz); reading/writing is a tiny
    /// lock-guarded `Copy` access on off-hot-path threads only.
    #[must_use]
    pub fn shared_epoch(&self) -> multiview_output::SharedEpoch {
        self.epoch.clone()
    }

    /// DEV-C1 (ADR-M010): the run's epoch **anchor** slot. `drive_streaming`
    /// publishes the tick-0 seed + the run's monotonic source into it when the
    /// program clock seeds; the timing-status task binds to it lazily with a
    /// lock-free load (inv #1/#10 — neither side can block the engine).
    #[must_use]
    pub fn epoch_anchor_slot(&self) -> crate::timing_status::EpochAnchorSlot {
        Arc::clone(&self.epoch_anchor)
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

    /// ADR-W022: the **shared** live overlay working-set slot. The binary
    /// threads a clone into the command drain
    /// ([`command_drain_with_seams`](crate::control::command_drain_with_seams))
    /// so `UpsertOverlay`/`RemoveOverlay` publish through it at the frame
    /// boundary; the bake consumer re-derives its overlay render state from it
    /// at the next frame. Reading/writing is a lock-free `ArcSwap` load/store,
    /// so neither side can pace or stall the output clock (inv #1/#10).
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn overlay_apply_slot(&self) -> crate::live_overlays::OverlayApplySlot {
        Arc::clone(&self.overlay_apply)
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
        // #103: an AES67 source/output only functions on the program-audio bus, so
        // fail closed BEFORE the output clock starts if this run carries none —
        // never go on air silently receiving/emitting silence.
        #[cfg(feature = "aes67")]
        self.ensure_aes67_has_program_audio()?;
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
            // one chosen GPU (affinity). No ordinal leaves the plans on
            // `DecodePlacement::Default` (libav's default CUDA device) — in
            // lockstep with `wgpu_target` being `None`, so neither stage pins a GPU.
            if let Some(ordinal) = pick.cuda_ordinal.as_deref() {
                for plan in &mut self.ingest_plans {
                    plan.decode_placement = DecodePlacement::Pinned(ordinal.to_owned());
                }
            }
            // Publish the pinned island (ADR-W018 §7) so every RUNTIME-added
            // decode consults the same admission scorer pinned to THIS device —
            // a live add never fragments or migrates the island. Stays empty
            // when admission named no device (the live spawner then skips the
            // consult, in lockstep with the startup plans' `None`).
            if let Some(device) = pick.device.clone() {
                self.live_island.store(Some(Arc::new(LiveIsland {
                    device,
                    cuda_ordinal: pick.cuda_ordinal.clone(),
                    tile_count: self.layout.cells.len(),
                })));
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
        // Media-player channels with embedded audio (ADR-T019): each loops its
        // asset's vamp-segment audio onto the program bus on the same wrap instant
        // as the video. Spawned ONLY when this run opted into program audio (else no
        // `ProgramBus` consumes the store). Each pairs the player's audio plan with
        // its `AudioStore` (already routed onto the bus below).
        let player_audio_plans: Vec<(
            crate::audio::PlayerAudioPlan,
            Arc<multiview_audio::store::AudioStore>,
        )> = if self.encode_cfg.audio.is_some() {
            std::mem::take(&mut self.player_audio_ingest_plans)
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
        // #103: AES67 / ST 2110-30 audio RX plans (audio-only ST 2110-30 sources).
        // Spawned ONLY when this run opted into program audio — mirroring the audio
        // decode plans above: without a `ProgramBus` consuming the store, the
        // received PCM would go nowhere. Left in place (untouched) when audio is off.
        #[cfg(feature = "aes67")]
        let aes67_plans: Vec<Aes67RxPlan> = if self.encode_cfg.audio.is_some() {
            std::mem::take(&mut self.aes67_rx_plans)
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
            player_audio_plans,
            caption_plans,
            &self.stop_registry,
        );
        // #103: spawn the AES67 RX threads into the SAME supervisor (its bounded
        // stop+join teardown), registered under `{id}` like a video source's
        // producer. An AES67 source has no video decode thread, so `{id}` is its
        // primary producer flag (a live `RemoveSource` stops it).
        #[cfg(feature = "aes67")]
        let supervisor = {
            let mut supervisor = supervisor;
            supervisor.spawn_aes67_receivers(aes67_plans, &self.stop_registry);
            supervisor
        };

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

        // The program-bus EBU R128 loudness telemetry (AUD-8): a read-only
        // compliance meter measuring the EMITTED program (post-loudnorm) and
        // pushing a conflated `audio.loudness` sample (M/S/I/LRA/dBTP +
        // compliance reference) onto the engine event stream at ~10 Hz. Built
        // when (and only when) audio is on, at the same format as the bus + the
        // SAME compliance target/ceiling/tolerance the loudnorm processor uses
        // (so the browser meter colours against exactly that). `None` when audio
        // is off (no loudness lane) OR when the meter fails to build (the run
        // continues without the meter rather than failing — telemetry is
        // best-effort, inv #10). Moves into the bake consumer (it is `Send`); it
        // never runs on the engine hot loop.
        let audio_loudness = self
            .encode_cfg
            .audio
            .as_ref()
            .and_then(|cfg| program_loudness_telemetry(cfg, audio_loudnorm.as_ref()));

        // A cheap clone of the engine's outbound publisher (Arc-backed, drop-
        // oldest): the bake consumer publishes loudness samples through it. The
        // clone shares the same broadcast, so a sample published from the consumer
        // thread reaches every subscriber exactly like a hot-loop event; it can
        // neither block the engine nor be back-pressured by a slow UI (inv #10).
        let loudness_publisher = audio_loudness.is_some().then(|| publisher.clone());

        // DEV-B1 / ADR-0044: start the configured DRM/KMS display heads NOW —
        // startup, before the output clock runs — and keep their handles alive
        // for the whole run (drop = stop + join, off the hot path). Each sink
        // owns its device on a dedicated thread; the engine side holds only
        // wait-free mailbox publishers, fed in `state_of` exactly where the
        // live-preview slot is filled. A startup failure (no such connector,
        // no usable mode, modeset rejected) fails the run like any other
        // misconfigured output; after startup the sinks can never fail the
        // engine (invariants #1 + #10). Audio-enabled heads (DEV-B4) also get
        // an ELD-gated ALSA audio sink wired to their flip clock; the
        // publishers feed from the bake consumer below.
        #[cfg(feature = "display-kms")]
        let started_displays = start_display_sinks(
            std::mem::take(&mut self.display_plans),
            self.cadence,
            display_audio_format(self.encode_cfg.audio.as_ref()),
        )?;
        #[cfg(feature = "display-kms")]
        let display_publishers = started_displays.publishers;
        // Keep the video + audio sink threads alive for the whole run; dropping
        // the handles at end of run stops + joins them (off the hot path).
        #[cfg(feature = "display-kms")]
        let _display_handles = started_displays.handles;
        #[cfg(feature = "display-kms")]
        let _display_audio_handles = started_displays.audio_handles;
        #[cfg(feature = "display-kms")]
        let display_audio_publishers = started_displays.audio_publishers;
        #[cfg(not(feature = "display-kms"))]
        let display_audio_publishers: Vec<
            multiview_output::display::audio::DisplayAudioPublisher,
        > = Vec::new();
        // The display-audio feed (DEV-B4): heads receive the SAME post-loudnorm
        // program block the stream encodes when this run carries program audio;
        // a video-only run with audio-enabled heads gets a dedicated program
        // bus (currently silence, correctly paced by tick index) so the audio
        // path is real either way.
        let display_audio = DisplayAudioFeed {
            dedicated_bus: if audio_bus.is_none() && !display_audio_publishers.is_empty() {
                Some(multiview_audio::program::ProgramBus::new(
                    display_audio_format(self.encode_cfg.audio.as_ref()),
                    self.cadence,
                ))
            } else {
                None
            },
            publishers: display_audio_publishers,
        };

        // OUT-4b / NDI-L2: take the engine-side canvas-tap publishers for the
        // configured NDI outputs. The hot loop publishes the SAME pre-encode
        // canvas `Arc` into each (wait-free, newest-wins) exactly like the display
        // heads; the paired `FrameReader` rides inside each `RunnableOutput::Ndi`
        // sink (which the egress spawns below). The engine never blocks on an NDI
        // send (inv #1/#10). Empty when no NDI output is configured/licensed.
        #[cfg(feature = "ndi-bindings")]
        let ndi_publishers = std::mem::take(&mut self.ndi_publishers);

        // #103: take the AES67 output push handles for the bake consumer. It pushes
        // each post-loudnorm program block into every handle (drop-oldest), exactly
        // like it feeds the display heads; the paired serve-side `Aes67Sender` rides
        // inside each `RunnableOutput::Aes67` sink runner (spawned by the egress
        // below). Empty when no AES67 output is configured. Only under `aes67`.
        #[cfg(feature = "aes67")]
        let aes67_send_handles = std::mem::take(&mut self.aes67_send_handles);

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
            audio_loudness,
            loudness_publisher,
            display_audio,
            self.program_audio_preview.clone(),
            #[cfg(feature = "aes67")]
            aes67_send_handles,
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
        // The timing-status task (DEV-C1 / ADR-M010) needs the run's monotonic
        // source to bracket its wall reads on the SAME timeline tick 0 is
        // seeded on; clone the handle before the program takes ownership.
        let ts_for_anchor = Arc::clone(&ts);
        let mut program =
            MultiviewProgram::new(&self.program_spec, clock, drive, ts, pacer, stop.clone())
                .map_err(|e| PipelineError::Program {
                    program: self.program_spec.id.clone(),
                    reason: e.to_string(),
                })?;
        // Publish the run's epoch anchor (tick-0 seed + monotonic source) into
        // the shared slot — a single lock-free store the off-hot-path
        // timing-status task reads to derive the outbound presentation epoch.
        // Publishing a value can never pace the clock (inv #1/#10).
        self.epoch_anchor
            .store(Some(Arc::new(multiview_engine::epoch::EpochAnchor::new(
                ts_for_anchor,
                program.seed_nanos(),
            ))));

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
        // (The DRM/KMS display heads — and their DEV-B4 audio sinks — were
        // started above, before the egress spawn, so the bake consumer received
        // the audio publishers; `display_publishers` feeds the video mailboxes
        // in `state_of` below.)
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
            // The NDI outputs ride the SAME pre-encode canvas `Arc` through their
            // wait-free mailboxes (newest-wins drop-oldest) — one lock-free swap
            // each, off the hot path; the NDI sink thread converts NV12→UYVY and
            // sends. The engine never awaits the SDK send (OUT-4b, inv #1/#10).
            #[cfg(feature = "ndi-bindings")]
            for ndi in &ndi_publishers {
                ndi.publish(NdiCanvasFrame {
                    canvas: Arc::clone(&canvas),
                    tick_index: frame.tick.index,
                });
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
            analog_clocks: self.analog_clocks.clone(),
            canvas_color: self.canvas_color,
            cadence: self.cadence,
            watermark_signal: self.watermark_signal.clone(),
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
            overlay_apply: Some(Arc::clone(&self.overlay_apply)),
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
    /// The analog clock face placements (one per analog-face entry).
    #[cfg(feature = "overlay")]
    analog_clocks: Vec<crate::overlays::AnalogClockSpec>,
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
    /// ADR-W022: the live overlay working-set slot the consumer re-derives
    /// from (one wait-free load + generation compare per frame). `None` ⇒ no
    /// live seam (the boot-derived overlay set stands for the whole run).
    #[cfg(feature = "overlay")]
    overlay_apply: Option<crate::live_overlays::OverlayApplySlot>,
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
    /// The last live overlay-set generation this baker derived its render
    /// state from (ADR-W022). `None` until the first bake, so a wired slot's
    /// seeded set drives the very first frame (one truth: the slot).
    overlay_generation: Option<u64>,
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
        baker.set_analog_clocks(ctx.analog_clocks.clone());
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
            overlay_generation: None,
        })
    }

    /// Re-derive the overlay render state from the live working-set slot iff
    /// its generation advanced (ADR-W022). The steady-state per-frame cost is
    /// one wait-free `ArcSwap` load plus an integer compare; on a change the
    /// re-derivation is O(overlays) pure math (no I/O, no rasterization), and
    /// the frame baked next is drawn entirely from the new set — a clean
    /// frame-boundary (Class-1) transition. Without a wired slot the
    /// boot-derived state stands.
    fn refresh_overlays(&mut self, canvas_w: u32, canvas_h: u32) {
        let Some(slot) = self.meter.overlay_apply.as_ref() else {
            return;
        };
        let set = slot.load();
        if self.overlay_generation == Some(set.generation()) {
            return;
        }
        self.baker.set_analog_clocks(analog_clocks_from_config(
            set.overlays(),
            canvas_w,
            canvas_h,
        ));
        self.overlay_generation = Some(set.generation());
    }

    /// Bake one streamed tick's overlays into its canvas, returning the overlaid
    /// `Arc<Nv12Image>` — the SAME math as the old post-loop `bake_overlays`, run
    /// once per received item off the hot loop.
    ///
    /// # Errors
    /// Returns [`PipelineError::Engine`] if the baker/sub-pass rejects the canvas.
    fn bake(&mut self, item: &StreamItem) -> Result<Arc<Nv12Image>, PipelineError> {
        use multiview_compositor::overlay::apply_overlays_to_nv12;

        // ADR-W022: pick up a live overlay-set change before drawing, so this
        // frame is baked entirely from one set (frame-boundary apply).
        self.refresh_overlays(item.canvas.width(), item.canvas.height());

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
    #[allow(clippy::too_many_arguments)] // reason: the bake consumer threads the encode, the audio bus + loudnorm + loudness meter, and the publisher — each a distinct, irreducible owned input to the off-hot-path consumer.
    fn spawn(
        ctx: BakeContext,
        runners: Vec<SinkRunner>,
        policy: SendPolicy,
        encoder: ProgramEncoder,
        audio_bus: Option<multiview_audio::program::ProgramBus>,
        audio_loudnorm: Option<multiview_audio::LoudnormProcessor>,
        audio_loudness: Option<crate::loudness_telemetry::LoudnessTelemetry>,
        loudness_publisher: Option<EnginePublisher<EngineStateSnapshot, Event>>,
        display_audio: DisplayAudioFeed,
        program_audio_preview: Option<crate::preview::ProgramAudioSlot>,
        #[cfg(feature = "aes67")] aes67_send_handles: Vec<
            multiview_output::aes67::Aes67SenderHandle,
        >,
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
                    audio_loudness,
                    loudness_publisher,
                    display_audio,
                    program_audio_preview,
                    #[cfg(feature = "aes67")]
                    aes67_send_handles,
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

/// The audio format the display heads' audio sinks are fed in (DEV-B4): the
/// run's program-audio format when this run encodes audio (so the heads hear
/// exactly the stream program), else the canonical 48 kHz stereo a dedicated
/// display bus mixes. Mirrors [`program_audio_bus`]'s layout mapping so pushed
/// blocks always match the sink FIFO's channel count.
fn display_audio_format(
    cfg: Option<&multiview_output::AudioEncodeConfig>,
) -> multiview_audio::AudioFormat {
    let Some(cfg) = cfg else {
        return multiview_audio::AudioFormat::new(48_000, multiview_audio::ChannelLayout::Stereo);
    };
    let layout = match cfg.channels {
        1 => multiview_audio::ChannelLayout::Mono,
        _ => multiview_audio::ChannelLayout::Stereo,
    };
    multiview_audio::AudioFormat::new(cfg.sample_rate, layout)
}

/// The display-head audio feed (DEV-B4): the bounded FIFO publishers of every
/// audio-enabled display output, plus — when the run carries no encode-side
/// program audio — a dedicated tick-driven program bus so the heads still
/// receive correctly-paced program audio (silence until sources are routed,
/// exactly like the encode-side bus). Empty publishers ⇒ the consumer skips
/// the display branch entirely; each push is a bounded short critical section
/// into the drop-oldest FIFO (the sink holds that lock only for in-memory
/// copies, never across a PCM call), so a wedged ALSA device can never reach
/// back to the bake consumer, let alone the engine (invariants #1 + #10).
struct DisplayAudioFeed {
    /// One bounded drop-oldest FIFO publisher per audio-enabled display head.
    publishers: Vec<multiview_output::display::audio::DisplayAudioPublisher>,
    /// `Some` only when the run has no encode-side audio bus AND there are
    /// display publishers to feed.
    dedicated_bus: Option<multiview_audio::program::ProgramBus>,
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

/// Build the program-bus loudness **telemetry** meter (AUD-8) for the run's audio
/// config: a read-only EBU R128 compliance meter (true-peak ON) over the EMITTED
/// program, reporting against the same target/ceiling/tolerance the loudnorm
/// processor uses (so the browser meter colours against exactly that compliance).
///
/// When a [`LoudnormProcessor`](multiview_audio::LoudnormProcessor) is present the
/// reference is read from it (its `target_lufs` / `ceiling_dbtp`); otherwise the
/// streaming defaults (`-16` LUFS, `-1.5` dBTP) — matching [`program_loudnorm`].
/// The live tolerance is the `±1 LU` single-pass live bound (ADR-R006). The format
/// mirrors [`program_audio_bus`]. Returns `None` if the format is unusable (the
/// run then continues with no loudness lane — telemetry is best-effort, inv #10).
fn program_loudness_telemetry(
    cfg: &multiview_output::AudioEncodeConfig,
    loudnorm: Option<&multiview_audio::LoudnormProcessor>,
) -> Option<crate::loudness_telemetry::LoudnessTelemetry> {
    let layout = match cfg.channels {
        1 => multiview_audio::ChannelLayout::Mono,
        _ => multiview_audio::ChannelLayout::Stereo,
    };
    let format = multiview_audio::AudioFormat::new(cfg.sample_rate, layout);
    // The compliance reference: prefer the loudnorm processor's live target +
    // ceiling so the meter and the normaliser agree; else the streaming defaults.
    let (target_lufs, ceiling_dbtp) = loudnorm.map_or(
        (
            multiview_audio::LoudnessTarget::Streaming.lufs(),
            multiview_audio::DEFAULT_TRUE_PEAK_CEILING_DBTP,
        ),
        |n| (n.target_lufs(), n.ceiling_dbtp()),
    );
    // Narrow the f64 reference values to the wire f32 without an `as` cast.
    let to_f32 = |v: f64| v.to_string().parse::<f32>().unwrap_or(0.0);
    crate::loudness_telemetry::LoudnessTelemetry::new(
        format,
        0,
        to_f32(target_lufs),
        to_f32(ceiling_dbtp),
        to_f32(multiview_audio::LIVE_TOLERANCE_LU),
    )
    .ok()
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
#[allow(clippy::too_many_arguments)]
// reason: the consumer owns each pipeline tail input distinctly (encode, audio bus + loudnorm + loudness meter + publisher, display feed, in-flight counter); bundling them would only obscure the data flow.
#[allow(clippy::needless_pass_by_value)] // reason: `loudness_publisher` is owned BY VALUE so it lives in the consumer thread's frame for the whole run (it cannot borrow from `drive_streaming`, which returns before this thread joins); the body only needs `&` access but the ownership is load-bearing.
fn consumer_main(
    ctx: BakeContext,
    hot_rx: &Receiver<StreamItem>,
    sink_txs: Vec<SyncSender<EncodedPacket>>,
    mut encoder: ProgramEncoder,
    mut audio_bus: Option<multiview_audio::program::ProgramBus>,
    mut audio_loudnorm: Option<multiview_audio::LoudnormProcessor>,
    mut audio_loudness: Option<crate::loudness_telemetry::LoudnessTelemetry>,
    loudness_publisher: Option<EnginePublisher<EngineStateSnapshot, Event>>,
    mut display_audio: DisplayAudioFeed,
    program_audio_preview: Option<crate::preview::ProgramAudioSlot>,
    #[cfg(feature = "aes67")] aes67_send_handles: Vec<multiview_output::aes67::Aes67SenderHandle>,
    in_flight: &AtomicI64,
) -> Result<(), PipelineError> {
    let mut baker = StreamBaker::new(ctx)?;
    // A monotonic clock for rate-bounding the loudness emit, sampled OFF the hot
    // loop (this is the bake-consumer thread). The epoch is the consumer start;
    // the conflator only ever sees a non-decreasing `now_ns`.
    let loudness_epoch = Instant::now();
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
            let gain_db = audio_loudnorm
                .as_ref()
                .map(multiview_audio::LoudnormProcessor::current_gain_db);
            let block = match audio_loudnorm.as_mut() {
                Some(norm) => norm.process(block),
                None => block,
            };
            // Program-bus loudness telemetry (AUD-8), still on THIS bake-consumer
            // thread (off the engine hot loop): meter the EMITTED block read-only
            // and, at ~10 Hz (conflated/drop-oldest), publish an `audio.loudness`
            // sample onto the engine event stream so the UI loudness meter lights
            // up. The publish is a single non-blocking broadcast send — it can
            // neither block the engine nor be back-pressured by a slow UI (inv
            // #10); a stalled UI just skips loudness samples. `None` (audio off /
            // meter unavailable) skips this entirely.
            if let (Some(meter), Some(publisher)) =
                (audio_loudness.as_mut(), loudness_publisher.as_ref())
            {
                // Narrow the makeup gain to the wire `f32` without an `as` cast.
                let gain_f32 = gain_db.map(|g| g.to_string().parse::<f32>().unwrap_or(0.0));
                let now_ns = loudness_epoch.elapsed().as_nanos();
                let now_ns = i64::try_from(now_ns).unwrap_or(i64::MAX);
                if let Some(sample) = meter.push(&block, gain_f32, now_ns) {
                    publisher.publish_event(Event::AudioLoudness(sample));
                }
            }
            // DEV-B4: the display heads hear the SAME post-loudnorm program
            // block the stream encodes. Each push is a bounded short critical
            // section into the drop-oldest FIFO (never held across a PCM
            // call) — a wedged HDMI audio device sheds frames and can never
            // back-pressure this consumer, let alone the engine
            // (invariants #1 + #10).
            for publisher in &display_audio.publishers {
                publisher.push_audio(&block);
            }
            // #103: feed the SAME post-loudnorm program block to every AES67
            // output's send FIFO (drop-oldest, `&self`) — exactly like the display
            // heads. The serve-side `Aes67Sender` drains it on its own media-clock
            // timer; a stalled network drops at the FIFO and can never
            // back-pressure this consumer, let alone the engine (invariants #1 +
            // #10). When a run has no program audio the bus is absent and this
            // branch never runs, so the FIFO stays empty and the serve loop emits
            // silence (`Aes67Sender::next_packet_into` silence-fills an underrun) —
            // no dedicated bus is needed here.
            #[cfg(feature = "aes67")]
            for handle in &aes67_send_handles {
                handle.push(&block);
            }
            // ADR-P006 audio: tap the SAME post-loudnorm program block into the
            // WHEP egress preview slot (when wired), so the live WHEP provider can
            // Opus-encode it and send it to the peer. A bounded drop-oldest push
            // off this bake-consumer thread — a slow/absent WHEP consumer only
            // loses the oldest blocks and can never back-pressure this consumer or
            // the engine (invariants #1/#10). `None` (no WHEP egress wired) is a
            // no-op, so a build/run without WHEP egress is byte-identical.
            if let Some(slot) = program_audio_preview.as_ref() {
                // The eviction flag is the "WHEP consumer behind" signal we
                // intentionally ignore here (drop-oldest; inv #10).
                let _ = slot.push(block.clone());
            }
            let audio_packets = encoder
                .encode_audio_interleaved(block.interleaved(), block.frame_count())
                .map_err(|e| PipelineError::Output {
                    kind: "encode-audio",
                    reason: e.to_string(),
                })?;
            fan_packets(&sink_txs, &mut live, audio_packets);
        } else if let Some(bus) = display_audio.dedicated_bus.as_mut() {
            // A video-only run with audio-enabled display heads: drive the
            // dedicated display bus by the same absolute tick index (RT-8b —
            // catching up across shed ticks keeps the sample clock a pure
            // function of the tick counter) and feed the heads. No encode, no
            // packets — the stream stays byte-identical to a video-only run.
            let block = drive_audio_for_item(bus, item.tick_index);
            for publisher in &display_audio.publishers {
                publisher.push_audio(&block);
            }
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
#[allow(clippy::too_many_lines)]
// reason: a flat dispatch `match` over every runnable output kind (file / HLS /
// push / WebRTC / NDI / AES67), each arm a short delegation to that kind's runner;
// its value is reading the whole per-kind mapping in one place. Splitting it would
// scatter the dispatch without reducing complexity.
fn run_one_output(
    output: RunnableOutput,
    rx: Receiver<EncodedPacket>,
    params: &StreamCodecParameters,
    time_base: Rational,
    audio: Option<&(StreamCodecParameters, Rational)>,
) -> Result<SinkRunOutcome, PipelineError> {
    // ADR-0060: run this sink's mux region inside an `output` resource scope so
    // our own logs (via the span) AND libav's synchronous mux/transport lines
    // (via the thread-local ResourceContext the bridge resolves) name this output
    // by its stable config id. Both clear on return (scoped, never stale).
    let _span = tracing::info_span!(
        "output",
        resource_kind = "output",
        resource_id = %output.id(),
    )
    .entered();
    let _resource = multiview_ffmpeg::ResourceGuard::enter(
        multiview_ffmpeg::ResourceContext::output(output.id()),
    );
    match output {
        RunnableOutput::File { id: _, sink, path } => {
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
            id: _,
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
        RunnableOutput::Push {
            id: _,
            sink,
            label,
            url,
        } => Ok(run_push_output(
            &sink, label, &url, rx, params, time_base, audio,
        )),
        #[cfg(feature = "webrtc-native")]
        RunnableOutput::WebRtc { id: _, sink, label } => {
            Ok(run_webrtc_output(&sink, &label, rx, time_base))
        }
        // NDI is a raw-canvas consumer (NV12→UYVY → send), NOT a packet sink: it
        // ignores the coded-packet fan-out and instead reads the latest tapped
        // canvas. The packet `rx` is used purely as the shared end-of-program
        // pulse (one item per emitted tick; it closes at end-of-program), which
        // also paces the send at the output cadence (invariants #1/#3/#10).
        #[cfg(feature = "ndi-bindings")]
        RunnableOutput::Ndi {
            id: _,
            mut sink,
            reader,
            cadence,
            name,
        } => Ok(run_ndi_output(&mut sink, &reader, rx, cadence, &name)),
        // AES67 is a raw-PCM audio consumer (program AudioBlock → L16/L24 RTP), NOT
        // a packet sink: it ignores the coded-packet fan-out and instead drains the
        // program audio the bake consumer pushes into its send FIFO. The packet
        // `rx` is used purely as the shared end-of-program pulse (it closes at
        // end-of-program), which resolves the serve loop's stop future (inv
        // #1/#10). Under `aes67`.
        #[cfg(feature = "aes67")]
        RunnableOutput::Aes67 {
            id: _,
            sender,
            local,
            dest,
            interface,
            label,
        } => Ok(run_aes67_output(sender, local, dest, interface, rx, &label)),
    }
}

/// Drive an AES67 / ST 2110-30 raw-PCM output (#103, ADR-0033/T013): multicast the
/// mixed program audio as a continuous L16/L24 RTP stream until end-of-program.
///
/// **Mux-free** — it consumes NO coded packets. The program audio arrives via the
/// [`Aes67SenderHandle`](multiview_output::aes67::Aes67SenderHandle) the bake
/// consumer pushes each post-loudnorm block into; this runner drains the paired
/// serve-side [`Aes67Sender`](multiview_output::aes67::Aes67Sender)'s FIFO on its
/// OWN media-clock timer ([`Aes67UdpSender::serve`](multiview_output::aes67::transport::Aes67UdpSender::serve))
/// and sends UDP. The `eop` packet receiver is used purely as the end-of-program
/// pulse (one item per emitted tick, closing at end-of-program), which resolves the
/// serve loop's stop future.
///
/// The socket + its send loop are async, so this runs them on a small
/// **current-thread** Tokio runtime on the sink thread (the peer of
/// [`run_ndi_output`]). **Infallible** by design (returns a [`SinkRunOutcome`],
/// never an error): an AES67 output whose socket cannot bind or whose send faults
/// must NOT fail the program — the file/HLS/push outputs keep producing (invariants
/// #1/#10). The serve loop silence-fills any FIFO underrun, so the multicast never
/// gaps even when this run carries no program audio.
///
/// The `eop` receiver is always fully drained (on a blocking task, off the async
/// reactor) so the encode-once fan-out can never wedge on this sink.
#[cfg(feature = "aes67")]
fn run_aes67_output(
    mut sender: multiview_output::aes67::Aes67Sender,
    local: std::net::SocketAddr,
    dest: std::net::SocketAddr,
    interface: multiview_output::aes67::transport::MulticastInterface,
    eop: Receiver<EncodedPacket>,
    label: &str,
) -> SinkRunOutcome {
    use multiview_output::aes67::transport::Aes67UdpSender;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(output = label, error = %e, "aes67 output runtime build failed; skipping");
            // Drain the end-of-program pulses so the fan-out never wedges.
            let frames = eop.into_iter().count();
            return SinkRunOutcome {
                line: format!("{label}: aes67 runtime unavailable"),
                playlist: None,
                frames,
            };
        }
    };
    runtime.block_on(async move {
        let udp = match Aes67UdpSender::bind(local, dest).await {
            Ok(u) => u.with_interface(interface),
            Err(e) => {
                tracing::warn!(output = label, error = %e, dest = %dest, "aes67 bind failed; skipping");
                // Drain the pulses off the reactor so the fan-out never wedges.
                let frames = tokio::task::spawn_blocking(move || eop.into_iter().count())
                    .await
                    .unwrap_or(0);
                return SinkRunOutcome {
                    line: format!("{label}: aes67 bind failed"),
                    playlist: None,
                    frames,
                };
            }
        };
        tracing::info!(output = label, dest = %dest, "aes67 output sending");
        // The end-of-program pulses are a std mpsc — drain them on a blocking task
        // (off the async reactor) so a `recv` never blocks it; the task ends (and
        // yields the pulse count) when the channel closes at end-of-program.
        let mut eop_task = tokio::task::spawn_blocking(move || eop.into_iter().count());
        // Serve on the media-clock timer (its own absolute-deadline cadence +
        // multicast egress config) with a never-resolving stop, so it runs until
        // end-of-program cancels it OR it faults. `select!` stops it on whichever
        // comes first, capturing the pulse count for the report.
        let frames = tokio::select! {
            joined = &mut eop_task => joined.unwrap_or(0),
            result = udp.serve(&mut sender, std::future::pending::<()>()) => {
                if let Err(e) = result {
                    tracing::warn!(output = label, error = %e, "aes67 serve faulted; stopping output");
                }
                // Wait for end-of-program to fully drain the fan-out (never wedge).
                (&mut eop_task).await.unwrap_or(0)
            }
        };
        SinkRunOutcome {
            line: format!("{label}: {frames} tick(s) multicast to {dest}"),
            playlist: None,
            frames,
        }
    })
}

/// Drive a WebRTC program output (`webrtc` / `whip_push`) over its fan-out channel
/// of coded packets: re-stamp each [`EncodedPacket`] into an
/// [`EgressSample`](multiview_webrtc::egress::EgressSample) (the AU bytes + its
/// 90 kHz video / 48 kHz audio RTP timestamp derived from the packet's
/// tick-stamped PTS, invariant #3) and push it onto the bounded drop-oldest
/// [`EgressSink`](multiview_webrtc::egress::EgressSink) the WHEP-serve driver /
/// `whip_push` client drains. No re-encode (invariant #7): the SAME coded bytes
/// the file/HLS/push sinks mux are packetized into SRTP per session.
///
/// **Infallible** by design (returns a [`SinkRunOutcome`], never an error): a
/// WebRTC output with no viewers / a dead WHIP target must not fail the program.
/// On end-of-program (the channel closes) the egress feed is closed so the
/// sessions tear down. A slow consumer drops on the feed — never stalls this
/// off-hot-path drain, the fan-out, or the output clock (invariants #1/#10).
#[cfg(feature = "webrtc-native")]
fn run_webrtc_output(
    sink: &multiview_webrtc::egress::EgressSink,
    label: &str,
    rx: Receiver<EncodedPacket>,
    time_base: Rational,
) -> SinkRunOutcome {
    use multiview_ffmpeg::StreamKind;
    use multiview_webrtc::egress::{EgressMedia, EgressSample};

    // The video RTP clock is 90 kHz; the encoder time-base is `1/cadence`, so a
    // packet PTS in encoder ticks rescales to 90 kHz video RTP units. Audio
    // packets are stamped in `1/sample_rate` already; the AAC→Opus distinction
    // does not change the 48 kHz Opus RTP clock the driver writes them at.
    let video_rtp = Rational::new(1, 90_000);
    let audio_rtp = Rational::new(1, 48_000);
    let mut delivered = 0usize;
    let mut dropped = 0u64;
    // Block on each fanned packet; the iterator ends when the channel closes
    // (end-of-program), consuming `rx` so it is dropped here.
    for packet in rx {
        delivered += 1;
        let Some(bytes) = packet.payload() else {
            continue;
        };
        let (media, rtp_timestamp) = match packet.kind() {
            StreamKind::Video => {
                let ts =
                    multiview_core::time::rescale(packet.pts().unwrap_or(0), time_base, video_rtp);
                (EgressMedia::Video, u32::try_from(ts.max(0)).unwrap_or(0))
            }
            StreamKind::Audio => {
                // Audio packets carry their PTS in the audio encoder time-base
                // (1/sample_rate); it is already the 48 kHz Opus RTP clock unit, so
                // the rescale is identity — kept explicit for the timeline contract.
                let ts =
                    multiview_core::time::rescale(packet.pts().unwrap_or(0), audio_rtp, audio_rtp);
                (EgressMedia::Audio, u32::try_from(ts.max(0)).unwrap_or(0))
            }
            // A future elementary-stream kind is not carried over WebRTC.
            _ => continue,
        };
        let sample = EgressSample {
            media,
            rtp_timestamp,
            keyframe: matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe(),
            data: bytes.to_vec(),
        };
        if sink.push(sample) {
            dropped = dropped.saturating_add(1);
        }
    }
    // End-of-program: close the feed so the sessions tear down cleanly.
    sink.close();
    if dropped > 0 {
        tracing::debug!(
            output = label,
            dropped,
            "webrtc egress dropped (slow/absent consumers)"
        );
    }
    SinkRunOutcome {
        line: format!("{label}: {delivered} packet(s) fanned to WebRTC egress"),
        playlist: None,
        frames: delivered,
    }
}

/// One composited program canvas tapped for the NDI output sink (OUT-4b /
/// NDI-L2): an `Arc` clone of the **same** pre-encode NV12 canvas the encode
/// fan-out + preview slot + display heads share (no extra pixel copy on the hot
/// loop), plus the tick index its NDI 100 ns timecode is re-stamped from
/// (invariant #3). Published into a wait-free single-slot mailbox (drop-oldest:
/// newest-wins), so the engine never blocks on — and is never back-pressured by —
/// a slow/absent NDI consumer (invariants #1 + #10), exactly like the display
/// head canvas tap (ADR-0044).
#[cfg(all(feature = "ndi", any(feature = "ndi-bindings", test)))]
#[derive(Debug, Clone)]
pub(crate) struct NdiCanvasFrame {
    /// The shared pre-encode NV12 canvas (one `Arc` clone, no copy).
    pub(crate) canvas: Arc<Nv12Image>,
    /// The output-clock tick index this canvas was produced on (invariant #3 —
    /// the NDI timecode is derived from this, never a wall clock / input PTS).
    pub(crate) tick_index: u64,
}

/// Derive the NDI 100 ns timecode for output `tick_index` at the exact-rational
/// output `cadence` (invariant #3 — tick-derived, never wall clock or a raw input
/// PTS; never float fps). The NDI timecode unit is 100 ns, so one tick is
/// `den/num` seconds = `tick * den * 10_000_000 / num` (100 ns units), computed
/// in `i128` to stay overflow-free over an unbounded run, then clamped to the
/// `i64` the descriptor carries. A non-positive denominator/numerator (never
/// produced by a validated cadence) degrades to `0` rather than dividing by zero.
#[cfg(all(feature = "ndi", any(feature = "ndi-bindings", test)))]
fn ndi_timecode_100ns(tick_index: u64, cadence: Rational) -> i64 {
    if cadence.num <= 0 || cadence.den <= 0 {
        return 0;
    }
    // 100 ns units per second = 10_000_000. `i128` keeps the product exact for
    // any realistic tick count + cadence before the divide.
    let units = i128::from(tick_index)
        .saturating_mul(i128::from(cadence.den))
        .saturating_mul(10_000_000)
        / i128::from(cadence.num);
    i64::try_from(units).unwrap_or(i64::MAX)
}

/// Drive the NDI output sink (OUT-4b / NDI-L2): publish the latest tapped program
/// canvas to the live NDI sender as each end-of-program pulse arrives, converting
/// NV12→UYVY at the host-copy boundary and stamping the tick-derived NDI 100 ns
/// timecode (invariant #3). **Off the hot path** — the engine's projection only
/// ever does a wait-free `publish` into the canvas mailbox; this runner reads the
/// newest-wins slot, so a slow/wedged NDI send drops older canvases at the tap
/// rather than back-pressuring the engine (invariants #1 + #10).
///
/// `eop` is the end-of-program signal: the per-sink fan-out [`Receiver`] the
/// egress hands every sink. The NDI sink consumes **no packets** (it sends the
/// raw canvas, not the encode-once AUs — a distinct canvas consumer, like the
/// display head), so the receiver is used purely as the shared lifecycle pulse:
/// one item per emitted tick, ending when the channel closes (end-of-program),
/// which also paces the send at the output cadence. A send error from the SDK
/// (e.g. the sender was torn down) is logged and the loop ends — it never panics
/// and never fails the program (the file/HLS/push outputs keep producing).
///
/// **Infallible** by design (returns a [`SinkRunOutcome`], never an error): an NDI
/// output that cannot send must not fail the run.
#[cfg(all(feature = "ndi", any(feature = "ndi-bindings", test)))]
fn run_ndi_output<A, T>(
    out: &mut NdiOutput<A>,
    reader: &multiview_output::display::FrameReader<NdiCanvasFrame>,
    eop: Receiver<T>,
    cadence: Rational,
    label: &str,
) -> SinkRunOutcome
where
    A: multiview_output::ndi::NdiApi,
{
    let frame_rate_n = u32::try_from(cadence.num.max(0)).unwrap_or(0);
    let frame_rate_d = u32::try_from(cadence.den.max(1)).unwrap_or(1);
    let mut sent = 0usize;
    let mut last_seq = 0u64;
    let mut send_errors = 0u64;
    // Block on each end-of-program pulse (one per emitted tick): the iterator ends
    // when the channel closes, consuming `eop` so it is dropped here. Each pulse,
    // sample the newest-wins canvas slot and send it if it advanced — older
    // canvases between pulses were already dropped at the single-slot tap.
    for _pulse in eop {
        let Some((frame, seq)) = reader.latest() else {
            continue; // no canvas published yet (pre-roll); nothing to send.
        };
        if seq == last_seq {
            continue; // no newer canvas since the last send (drop-oldest, idle).
        }
        last_seq = seq;
        // Borrow the shared canvas planes for the conversion; a malformed canvas
        // is a typed refusal from the seam, never a panic (invariant #1).
        let image: &Nv12Image = &frame.canvas;
        let canvas = match multiview_output::ndi::Nv12Canvas::new(
            image.width(),
            image.height(),
            image.y_plane(),
            image.uv_plane(),
        ) {
            Ok(c) => c,
            Err(e) => {
                send_errors = send_errors.saturating_add(1);
                tracing::warn!(output = label, error = %e, "ndi canvas rejected; skipping frame");
                continue;
            }
        };
        let timecode = ndi_timecode_100ns(frame.tick_index, cadence);
        match out.send_canvas(&canvas, timecode, frame_rate_n, frame_rate_d) {
            Ok(()) => sent = sent.saturating_add(1),
            Err(e) => {
                // A send failure (sender torn down, runtime error) ends the NDI
                // sink, never the program: log + stop. The file/HLS/push outputs
                // keep producing (invariants #1/#10).
                tracing::warn!(output = label, error = %e, "ndi send failed; stopping NDI output");
                send_errors = send_errors.saturating_add(1);
                break;
            }
        }
    }
    // End-of-program (or send fault): close the sender so the SDK handle is freed
    // off the hot path. Idempotent; `Drop` would also close it.
    out.close();
    if send_errors > 0 {
        tracing::debug!(
            output = label,
            send_errors,
            "ndi output finished with send faults"
        );
    }
    SinkRunOutcome {
        line: format!("{label}: {sent} canvas frame(s) sent to NDI"),
        playlist: None,
        frames: sent,
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
    // A sequence of near-identical per-role spawn loops (video / audio / tone /
    // player-audio / captions), each registering a stop flag + ExitGuard and
    // pushing a producer handle: clearer kept whole than split across five helpers
    // that would each take the registry + the producers vec.
    #[allow(clippy::too_many_lines)]
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
        // Media-player channels whose loaded asset has audio (ADR-T019): each
        // loops its embedded audio on its own thread onto the program bus, on the
        // same wrap instant as the video. Empty when no player carries audio.
        player_audio_plans: Vec<(
            crate::audio::PlayerAudioPlan,
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
                .saturating_add(player_audio_plans.len())
                .saturating_add(caption_plans.len()),
        );
        for plan in plans {
            // IN-5b: spawn the PROACTIVE youtube re-resolution loop as a supervised
            // SIBLING of this source's decode thread (before the decode thread takes
            // ownership of `plan`). It shares the plan's lock-free swappable-URL slot
            // and publishes a fresh `*.googlevideo.com` master into it AHEAD of the
            // active URL's expiry (make-before-break), so the decode loop's reconnect
            // picks up the next-up URL without a cold resolve at the boundary. It is
            // registered under the derived `{id}/youtube-reresolve` key (ADR-W018) so
            // a live remove/edit of the source raises every `{id}`-rooted flag and
            // stops it too. Off the data plane — only ever WRITES the slot (inv
            // #1/#10). Only under the off-by-default `youtube` feature.
            #[cfg(feature = "youtube")]
            if let SourceLocation::Youtube { watch_url } = &plan.location {
                if let Some(slot) = plan.youtube_url_slot.clone() {
                    let id = plan.id.clone();
                    let watch_url = watch_url.clone();
                    let rr_stop = Arc::new(AtomicBool::new(false));
                    let rr_exited = crate::live_sources::register_stop(
                        registry,
                        &format!("{id}/youtube-reresolve"),
                        &rr_stop,
                    );
                    // Build the ExitGuard BEFORE spawn: its Drop flips `exited` whether the
                    // thread runs (drops on exit) OR Builder::spawn fails (the closure owning
                    // it is dropped) — so a failed spawn never orphans an exited=false entry
                    // that teardown would busy-wait to the grace deadline (ADR-W018 §5).
                    let rr_exit_guard = crate::live_sources::ExitGuard::new(&rr_exited);
                    let rr_thread_stop = Arc::clone(&rr_stop);
                    let rr_builder =
                        std::thread::Builder::new().name(format!("multiview-yt-reresolve-{id}"));
                    match rr_builder.spawn(move || {
                        let _exit = rr_exit_guard;
                        youtube_reresolve_thread(&watch_url, &slot, &rr_thread_stop);
                    }) {
                        Ok(handle) => producers.push((rr_stop, handle)),
                        Err(e) => {
                            // The proactive loop is an optimisation: if its thread
                            // cannot spawn, the decode thread's inline
                            // resolve-on-reconnect (IN-5) still keeps the tile alive
                            // across expiry (briefly degrading at the boundary). Log
                            // + continue — never fail the run (invariant #1).
                            tracing::error!(error = %e, source = %id, "could not spawn youtube re-resolution thread; falling back to resolve-on-reconnect");
                        }
                    }
                }
            }
            // The decode thread itself — the SAME supervised producer the live
            // hub spawns (ADR-W018 level 2): `spawn_ingest_producer` registers
            // the per-source stop flag and runs `ingest_loop`.
            if let Some((stop, handle)) = spawn_ingest_producer(plan, registry) {
                producers.push((stop, handle));
            }
        }
        for (plan, store) in audio_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            // Registered under the derived `{id}/audio` key (ADR-W018): a live
            // remove/edit of the source raises every `{id}`-rooted flag, so its
            // audio decode thread stops too — never left mixing a stale source's
            // audio onto the program bus under the replacement.
            let exited =
                crate::live_sources::register_stop(registry, &format!("{id}/audio"), &stop);
            // ExitGuard built BEFORE spawn: its Drop flips `exited` even if Builder::spawn
            // fails (the closure owning it is dropped) — no orphaned latch (ADR-W018 §5).
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-audio-{id}"));
            match builder.spawn(move || {
                let _exit = exit_guard;
                crate::audio::audio_ingest_loop(&plan, &store, &thread_stop);
            }) {
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
            let exited = crate::live_sources::register_stop(registry, &format!("{id}/tone"), &stop);
            // ExitGuard built BEFORE spawn (flips `exited` even if spawn fails) (ADR-W018 §5).
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-tone-{id}"));
            match builder.spawn(move || {
                let _exit = exit_guard;
                crate::audio::tone_publish_loop(&plan, &store, &thread_stop);
            }) {
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
        // Media-player channels with embedded audio (ADR-T019): each loops its
        // asset's `[vamp_in, vamp_out)` audio onto the program bus on the same wrap
        // instant as the video. Spawned exactly like the per-source audio decode
        // threads, registered under the derived `{id}/player-audio` key (ADR-W018).
        // The driver is `ffmpeg`-gated (it decodes the asset); without `ffmpeg` a
        // player carries no audio and rides silence (consistent with every other
        // decode path), so the plan is consumed (`stop`/store dropped) but no thread
        // spawns.
        for (plan, store) in player_audio_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            let exited =
                crate::live_sources::register_stop(registry, &format!("{id}/player-audio"), &stop);
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            #[cfg(feature = "ffmpeg")]
            {
                let builder =
                    std::thread::Builder::new().name(format!("multiview-player-audio-{id}"));
                match builder.spawn(move || {
                    let _exit = exit_guard;
                    crate::audio::player_audio_loop(&plan, &store, &thread_stop);
                }) {
                    Ok(handle) => producers.push((stop, handle)),
                    Err(e) => {
                        // A player-audio thread that cannot spawn is logged and
                        // skipped: its channel loops video normally and rides audio
                        // silence (best-effort — invariant #1; never gates output).
                        tracing::error!(error = %e, player = %id, "could not spawn media-player audio loop thread");
                    }
                }
            }
            #[cfg(not(feature = "ffmpeg"))]
            {
                // No decode without `ffmpeg`: drop the guard (flips `exited`) and the
                // store; the player rides audio silence.
                let _ = (exit_guard, thread_stop, store);
            }
        }
        for plan in caption_plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            // Registered under the derived `{id}/captions` key (ADR-W018): a
            // live remove/edit of the source raises every `{id}`-rooted flag,
            // so its caption reader stops too — never left decoding a stale
            // URL's cues over the replacement picture.
            let exited =
                crate::live_sources::register_stop(registry, &format!("{id}/captions"), &stop);
            // ExitGuard built BEFORE spawn (flips `exited` even if spawn fails) (ADR-W018 §5).
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-captions-{id}"));
            match builder.spawn(move || {
                let _exit = exit_guard;
                crate::captions::caption_loop(&plan, &thread_stop);
            }) {
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

#[cfg(feature = "aes67")]
impl IngestSupervisor {
    /// Spawn one supervised AES67 / ST 2110-30 RX thread per plan (#103), pushing
    /// each into the SAME `producers` vec every other ingest thread lives in — so
    /// they share the identical bounded stop+join teardown ([`Self::join_all`]).
    ///
    /// Each registers its per-thread stop flag under the source id in the run's
    /// shared registry (ADR-W018 — a live `RemoveSource` raises exactly this
    /// flag), like [`spawn_ingest_producer`] does for a video source (an AES67
    /// source has no video thread, so `{id}` is its primary producer). The RX
    /// thread only ever WRITES its lock-free `AudioStore`, so it can neither pace
    /// nor stall the output clock (inv #1/#10); a thread that cannot spawn is
    /// logged and skipped (its source rides silence on the bus, never failing the
    /// run — invariant #1).
    fn spawn_aes67_receivers(
        &mut self,
        plans: Vec<Aes67RxPlan>,
        registry: &crate::live_sources::StopRegistry,
    ) {
        for plan in plans {
            let stop = Arc::new(AtomicBool::new(false));
            let id = plan.id.clone();
            let exited = crate::live_sources::register_stop(registry, &id, &stop);
            // ExitGuard built BEFORE spawn (flips `exited` even if Builder::spawn
            // fails — the dropped closure drops the guard) (ADR-W018 §5).
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-aes67-rx-{id}"));
            match builder.spawn(move || {
                let _exit = exit_guard;
                drive_aes67_rx(&plan, &thread_stop);
            }) {
                Ok(handle) => self.producers.push((stop, handle)),
                Err(e) => {
                    tracing::error!(error = %e, source = %id, "could not spawn aes67 rx thread");
                }
            }
        }
    }
}

/// Spawn ONE supervised ingest producer thread for `plan`: create its
/// per-source stop flag, register it under the source id in the run's shared
/// stop registry (ADR-W018 — a live remove/edit raises exactly this flag), and
/// run [`ingest_loop`] on a named thread.
///
/// This is **the** per-source producer construction — the startup
/// [`IngestSupervisor::start`] and the live-source hub's
/// [`LiveIngestSpawner`] both call it, so a runtime-added source runs exactly
/// the supervised ingest the startup path builds (same reconnect bracket,
/// jitter, PTS normalization, rw-timeout) — never a second-quality copy.
///
/// A thread that cannot spawn is logged and yields `None`: its tile simply
/// rides `NO_SIGNAL` (slate) rather than failing the run (invariant #1 — the
/// output clock is independent of inputs).
fn spawn_ingest_producer(
    plan: IngestPlan,
    registry: &crate::live_sources::StopRegistry,
) -> Option<(Arc<AtomicBool>, JoinHandle<()>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let id = plan.id.clone();
    let exited = crate::live_sources::register_stop(registry, &id, &stop);
    // ExitGuard built BEFORE spawn: its Drop flips `exited` on thread exit OR if
    // Builder::spawn fails (the dropped closure drops the guard) — so a failed spawn
    // never orphans an exited=false entry the hub would busy-wait on (ADR-W018 §5 /
    // ADR-T002 — the hub cannot join a startup thread, so it waits on this latch).
    let exit_guard = crate::live_sources::ExitGuard::new(&exited);
    let thread_stop = Arc::clone(&stop);
    let builder = std::thread::Builder::new().name(format!("multiview-ingest-{id}"));
    match builder.spawn(move || {
        let _exit = exit_guard;
        ingest_loop(&plan, &thread_stop);
    }) {
        Ok(handle) => Some((stop, handle)),
        Err(e) => {
            tracing::error!(error = %e, source = %id, "could not spawn ingest thread");
            None
        }
    }
}

/// The run's decoded-ingest spawner (ADR-W018 level 2), handed to the
/// live-source hub by the binary: it turns a runtime
/// [`SourceSpawn`](crate::live_sources::SourceSpawn) into a running producer
/// through **exactly** the startup construction — [`ingest_plan_for`] builds
/// the plan, [`select_live_decode_placement`] consults the same admission scorer
/// (pinned to the running island's device), and [`spawn_ingest_producer`]
/// spawns the same supervised [`ingest_loop`]. Runs on the hub worker thread
/// only — heavy/blocking work never touches the output clock (inv #1).
struct LiveIngestSpawner {
    /// The solved startup layout: tile geometry for a bound cell, the canvas
    /// fallback for an unbound one — the same sizing rule the startup build
    /// applies (a freshly added source is typically unbound until the
    /// follow-up route, so it decodes at canvas size, exactly like an unbound
    /// startup source).
    layout: Arc<Layout>,
    /// The canvas colour the source's frames are tagged in.
    canvas_color: CanvasColor,
    /// The output cadence (generator pacing / plan metadata).
    cadence: Rational,
    /// The pinned island slot `drive_streaming` publishes (ADR-W018 §7).
    #[cfg(feature = "gpu")]
    island: LiveIslandSlot,
    /// The GPU load source the placement consult re-polls at decision time
    /// (NVML in production; injectable for the placement-consult tests).
    #[cfg(feature = "gpu")]
    load_source: Box<dyn multiview_hal::LoadSource + Send + Sync>,
}

impl crate::live_sources::IngestSpawner for LiveIngestSpawner {
    fn spawn(
        &self,
        spawn: crate::live_sources::SourceSpawn,
        registry: &crate::live_sources::StopRegistry,
    ) -> Option<crate::live_sources::SpawnedProducer> {
        let crate::live_sources::SourceSpawn { source, store } = spawn;
        let (tile_w, tile_h) = cell_pixel_size(&self.layout, &source.id)
            .unwrap_or((self.layout.canvas.width, self.layout.canvas.height));
        #[cfg_attr(not(feature = "gpu"), allow(unused_mut))]
        // reason: without `gpu` there is no placement consult, so the plan is
        // never re-stamped after construction; the binding must still be `mut`
        // for the gpu arm below.
        let mut plan = match ingest_plan_for(
            &source,
            tile_w,
            tile_h,
            store,
            self.canvas_color,
            self.cadence,
        ) {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!(
                    source = %source.id,
                    error = %e,
                    "live ingest spawn refused: the source cannot be planned; \
                     the tile rides the slate"
                );
                return None;
            }
        };
        // Hardware re-assessment on every change (ADR-W018 §7), under `gpu`.
        #[cfg(feature = "gpu")]
        {
            plan.decode_placement = self.decode_placement_for(&plan.id);
        }
        spawn_ingest_producer(plan, registry)
            .map(|(stop, handle)| crate::live_sources::SpawnedProducer { stop, handle })
    }
}

#[cfg(feature = "gpu")]
impl LiveIngestSpawner {
    /// Decide a runtime-added source's [`DecodePlacement`] (ADR-W018 §7), under
    /// `gpu` where an admission decision exists. An admit pins the island
    /// ordinal; a reject (or an un-pinnable admit) forces `SoftwareOnly` so the
    /// decode never lands on the over-headroom island.
    ///
    /// EMPTY ISLAND FAILS CLOSED: `gpu` admission ran at startup
    /// (`drive_streaming`) and published a `LiveIsland` only when it named a
    /// device; an EMPTY slot means admission was *attempted and named none*
    /// (scorer rejection / no NVML / no admissible GPU). Defaulting to
    /// `DecodePlacement::Default` there would open NVDEC on libav's default
    /// device — exactly the over-subscribed/wrong GPU the admission declined —
    /// so a runtime-added decode forces `SoftwareOnly` instead (fail closed).
    /// The GPU-free build carries no `island` field at all and keeps the
    /// constructor `Default` (no NVDEC compiled to mis-place).
    fn decode_placement_for(&self, source_id: &str) -> DecodePlacement {
        if let Some(island) = self.island.load_full() {
            select_live_decode_placement(
                self.load_source.as_ref(),
                &island,
                self.layout.canvas.width,
                self.layout.canvas.height,
                self.cadence,
            )
        } else {
            tracing::warn!(
                source = %source_id,
                "live decode placement: GPU admission named no island device \
                 (rejected / no NVML); FORCING software decode for this \
                 runtime-added source (the default device would over-subscribe \
                 or mis-place the decode — ADR-W018 §7)"
            );
            DecodePlacement::SoftwareOnly
        }
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

/// The decode size for `source_id`: the **per-axis supremum** of the pixel sizes
/// of every cell that binds it, or [`None`] if no cell binds it.
///
/// A source can tile into several cells of different sizes (e.g. a big PGM cell
/// plus a small PIP). Decoding once at the per-axis max satisfies the largest
/// consumer on each axis (ADR-0030 §3); each cell then scales down at composite.
/// Taking the FIRST binding cell would under-decode every larger tile bound to
/// the same source.
fn cell_pixel_size(layout: &Layout, source_id: &str) -> Option<(u32, u32)> {
    layout
        .cells
        .iter()
        .filter(|c| c.source.as_deref() == Some(source_id))
        .map(|c| cell_dims(c, layout.canvas.width, layout.canvas.height))
        .reduce(|(aw, ah), (bw, bh)| (aw.max(bw), ah.max(bh)))
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
/// Read the analog clock faces from the config `[[overlays]]` list: one face
/// per entry whose `kind == "clock"` and whose `face` param is `"analog"`, in
/// working-set order (ADR-W022 — EVERY analog entry renders; no first-wins).
///
/// Placement comes from each entry's optional `x`/`y`/`radius` params (canvas
/// pixels); a missing placement defaults that face to the bottom-right corner
/// sized to the canvas. An optional `tz_minutes` param sets the timezone
/// offset (default UTC). Returns an empty set when no analog clock is
/// requested (the digital label still renders). Without the `overlay` feature
/// this is never called.
// `pub` so a render-level integration test can drive the EXACT derivation the
// bake consumer's `refresh_overlays` uses (slot overlay set → ordered analog
// face specs → `OverlayBaker::draw_list`), proving an equal-z overlay reorder
// changes the RENDERED draw order, not just the published slot (task #130).
#[cfg(feature = "overlay")]
#[must_use]
pub fn analog_clocks_from_config(
    overlays: &[multiview_config::Overlay],
    canvas_w: u32,
    canvas_h: u32,
) -> Vec<crate::overlays::AnalogClockSpec> {
    use multiview_overlay::clock::TimeZoneOffset;

    let cw = u32_to_f32(canvas_w);
    let ch = u32_to_f32(canvas_h);
    // A face sized to ~22% of the shorter canvas side by default.
    let default_radius = cw.min(ch) * 0.11;

    overlays
        .iter()
        .filter(|o| {
            o.kind == "clock"
                && o.params
                    .get("face")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|f| f.eq_ignore_ascii_case("analog"))
        })
        .map(|entry| {
            // Config placement is whole-pixel / whole-minute; round each param
            // to an i32 and widen it losslessly to f32 (no `as` cast), or fall
            // back to the default.
            let param_f32 = |key: &str| -> Option<f32> {
                entry
                    .params
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .map(|v| i32_to_f32(round_f64_to_i32(v)))
            };
            let radius = param_f32("radius").unwrap_or(default_radius).max(8.0);
            // Default placement: bottom-right corner, inset by radius + margin.
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
            crate::overlays::AnalogClockSpec::new(zone, cx, cy, radius)
        })
        .collect()
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
        | Output::Srt { codec, .. }
        // WebRTC outputs consume the encode-once program rendition (invariant #7,
        // ADR-0049) — they spawn no encoder, so the program MUST be encoded as the
        // codec they name (default h264). A webrtc-only config therefore selects
        // h264, not the mpeg2video fallback the SRTP packetizer cannot carry.
        | Output::Webrtc { codec, .. }
        | Output::WhipPush { codec, .. } => Some(codec.as_str()),
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
    /// File/HLS/push sinks fed the one encoded packet stream (invariant #7). The
    /// `ndi-bindings` NDI sink also lives here (as `RunnableOutput::Ndi`) so it
    /// shares the existing per-sink thread + end-of-program lifecycle — but it
    /// consumes the canvas tap below, never the packets.
    packet: Vec<RunnableOutput>,
    /// DRM/KMS display heads, started as raw-frame sinks at stream start.
    #[cfg(feature = "display-kms")]
    display: Vec<DisplayOutputPlan>,
    /// The engine-side wait-free publishers of each NDI output's canvas tap
    /// (paired with the `FrameReader` inside its `RunnableOutput::Ndi`). The hot
    /// loop publishes the shared pre-encode canvas `Arc` into each, exactly like
    /// the display head publishers — newest-wins, so the engine never blocks
    /// (invariants #1 + #10). Built only under `ndi-bindings`.
    #[cfg(feature = "ndi-bindings")]
    ndi_publishers: Vec<multiview_output::display::FramePublisher<NdiCanvasFrame>>,
    /// The bake-consumer-side **push handles** of each AES67 output's send FIFO
    /// (#103), paired with the serve-side `Aes67Sender` inside its
    /// `RunnableOutput::Aes67`. The bake consumer pushes each post-loudnorm program
    /// block into every handle (drop-oldest, `&self`), exactly like it feeds the
    /// display heads — a stalled network drops at the FIFO and can never
    /// back-pressure the consumer or the engine (invariants #1 + #10). Built only
    /// under `aes67`.
    #[cfg(feature = "aes67")]
    aes67_handles: Vec<multiview_output::aes67::Aes67SenderHandle>,
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
    /// Whether HDMI/DP audio is enabled on this head (the config `audio` block
    /// is present). The audio sink only runs when this is set AND the ELD is
    /// valid (DEV-B4 / display-out §5); an EDID-less head has no audio path.
    audio_enabled: bool,
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
        audio,
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
        // The presence of the `audio` block enables HDMI/DP audio (ELD-gated at
        // runtime); a display output never carries selectable discrete tracks
        // (capability-validated upstream), so the mode is not inspected here.
        audio_enabled: audio.is_some(),
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

/// Everything [`start_display_sinks`] lit: the per-head video flip loops (and
/// their mailbox publishers) plus the DEV-B4 audio sinks (and their bounded
/// drop-oldest FIFO publishers) of the heads whose plan enabled audio. The handle vectors
/// own the threads — keep them alive for the run; dropping them stops + joins.
#[cfg(feature = "display-kms")]
struct StartedDisplaySinks {
    /// One running flip loop per head.
    handles: Vec<multiview_output::display::DisplaySinkHandle>,
    /// The matching wait-free frame mailboxes (same order as `handles`).
    publishers: Vec<multiview_output::display::FramePublisher<CanvasFrame>>,
    /// The running ELD-gated ALSA audio sinks (audio-enabled heads only).
    audio_handles: Vec<multiview_output::display::audio::DisplayAudioSink>,
    /// The matching bounded drop-oldest audio FIFO publishers.
    audio_publishers: Vec<multiview_output::display::audio::DisplayAudioPublisher>,
}

/// The display-audio FIFO depth in frames (per channel): ~170 ms @ 48 kHz.
/// Bounds the worst-case added audio latency AND the drop point under a
/// wedged/slow device (drop-oldest — the engine-side push never blocks).
#[cfg(feature = "display-kms")]
const DISPLAY_AUDIO_FIFO_FRAMES: usize = 8_192;

/// Open and light every configured display head (feature `display-kms`):
/// scan `/dev/dri` for the connector-owning card, probe + select the mode
/// (EDID preferred / exact-rational `cadence` match / CVT-RB forced), run the
/// `TEST_ONLY` validation and the one startup modeset, and spawn the
/// dedicated flip-loop thread per head (ADR-0044 §1). Startup-only; runs
/// before the output clock starts.
///
/// An audio-enabled head (DEV-B4) additionally gets an ELD-gated ALSA audio
/// sink: the connector's ALSA endpoints are discovered (vc4 card-per-port or
/// the HDA `eld#D.P` scan), the sink is wired to this head's flip clock for
/// the scanout-skew servo term, and fed `audio_format` program blocks. Audio
/// is **best-effort by construction** (display-out §5): an EDID-less head
/// (forced mode — no ELD exists) and a connector with no discoverable ALSA
/// endpoint run video-only with a log line, never an error — and a present
/// ELD is still re-checked live by the sink itself (hotplug), so audio only
/// flows while the pipe is lit AND the ELD is valid.
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
    audio_format: multiview_audio::AudioFormat,
) -> Result<StartedDisplaySinks, PipelineError> {
    use multiview_output::display::kms::KmsDisplayDevice;
    use multiview_output::display::{DisplaySink, DisplaySinkConfig};
    let mut started = StartedDisplaySinks {
        handles: Vec::with_capacity(plans.len()),
        publishers: Vec::with_capacity(plans.len()),
        audio_handles: Vec::new(),
        audio_publishers: Vec::new(),
    };
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
        if plan.audio_enabled {
            start_display_audio(&plan.output_id, &handle, audio_format, &mut started);
        }
        started.handles.push(handle);
        started.publishers.push(publisher);
    }
    Ok(started)
}

/// Start the DEV-B4 ALSA audio sink for one lit head, appending its handle +
/// publisher to `started`. Best-effort: every miss (EDID-less head, no ALSA
/// endpoint) logs and returns — the head runs video-only, never an error.
#[cfg(feature = "display-kms")]
fn start_display_audio(
    output_id: &str,
    handle: &multiview_output::display::DisplaySinkHandle,
    audio_format: multiview_audio::AudioFormat,
    started: &mut StartedDisplaySinks,
) {
    use multiview_output::display::audio::alsa::discover_for_connector;
    use multiview_output::display::audio::{DisplayAudioConfig, DisplayAudioSink, FlipClock};

    let head = handle.head();
    if !head.from_edid {
        // The documented field condition (display-out §5/§6): a forced-mode
        // (EDID-less) head publishes no ELD, so it has NO audio path — video
        // only, stated rather than a surprise.
        tracing::info!(
            output = %output_id,
            connector = %head.connector,
            "display head runs a forced (EDID-less) mode: no ELD, so no audio path; video only"
        );
        return;
    }
    let Some(found) = discover_for_connector(&head.connector) else {
        tracing::warn!(
            output = %output_id,
            connector = %head.connector,
            "display audio enabled but no ALSA endpoint was discovered for the connector; \
             head runs video-only"
        );
        return;
    };
    tracing::info!(
        output = %output_id,
        connector = %head.connector,
        card = %found.card_id,
        "display audio: ALSA endpoints discovered (ELD-gated sink starting)"
    );
    // The head's flip telemetry is the scanout clock the audio servo's skew
    // term anchors against (display-out §5: the three-clock problem).
    let stats = handle.stats();
    let flip: FlipClock = Box::new(move || stats.snapshot().last_flip_ns);
    let (audio_handle, audio_publisher) = DisplayAudioSink::start_with_flip_clock(
        DisplayAudioConfig {
            output_id: output_id.to_owned(),
            format: audio_format,
            fifo_capacity_frames: DISPLAY_AUDIO_FIFO_FRAMES,
            // Matches the video sink's poll: bounds stop latency and how fast
            // an idle/ELD-waiting sink reacts, without busy-waiting (the
            // blocking PCM write paces the steady state on hardware).
            poll_interval: Duration::from_millis(4),
        },
        found.eld,
        found.pcm,
        Some(flip),
    );
    started.audio_handles.push(audio_handle);
    started.audio_publishers.push(audio_publisher);
}

/// Resolve a RIST pre-shared-key `secret_ref` to its plaintext passphrase
/// (RIST-3, ADR-0095 §3). The PSK is **never** stored in the config or logs; it
/// is read at run time from the secret store the deployment configures.
///
/// This build resolves an `env:VAR` reference from the environment (no extra
/// dependency, works in every deployment), exactly like the managed-device
/// credential resolver. An `op://…` reference (or any other scheme) returns
/// `None` here — the run then cannot inject the key and the `rist://` open
/// surfaces a libav error the ingest loop rides as `NO_SIGNAL`/reconnect (never
/// a crash), rather than silently sending an unencrypted feed.
#[cfg(feature = "rist")]
fn resolve_rist_secret(secret_ref: &str) -> Option<String> {
    // `env:VAR` — read the passphrase from a single environment variable.
    let var = secret_ref.strip_prefix("env:")?;
    std::env::var(var).ok()
}

/// Lower a `rist://` base URL + typed [`RistOptions`] into the `AVIO` URL libav
/// opens, resolving any PSK `secret_ref` (RIST-3, ADR-0095 §3). When `redact` is
/// `true` the PSK is `***`-masked for logging — the plaintext never reaches a
/// log line.
///
/// # Errors
///
/// [`multiview_config::RistUrlError`] when the typed options cannot be lowered
/// (a non-empty bonding list on the Tier-0 build, or encryption configured with
/// a `secret_ref` that did not resolve). Each call site maps it to the
/// appropriate [`PipelineError`] (ingest vs output).
#[cfg(feature = "rist")]
fn rist_resolved_url(
    base_url: &str,
    rist: Option<&RistOptions>,
    redact: bool,
) -> Result<String, multiview_config::RistUrlError> {
    let Some(opts) = rist else {
        return Ok(base_url.to_owned());
    };
    // Resolve the PSK (if any) once; the lowering injects it into the URL.
    let resolved = opts
        .encryption
        .as_ref()
        .and_then(|enc| resolve_rist_secret(&enc.secret_ref));
    lower_rist_url(base_url, opts, resolved.as_deref(), redact)
}

/// Build the `RunnableOutput::Push` for an `Output::Rist` (RIST-3): lower the
/// typed options (resolving the PSK) to the `AVIO` URL the libav `mpegts` muxer
/// opens over `rist://`, fed the SAME encoded packets as every other push
/// (invariant #7). Only the redacted URL is ever logged / reported.
///
/// # Errors
///
/// [`PipelineError::Output`] when the options cannot be lowered (bonding on the
/// Tier-0 build, or an unresolved encryption `secret_ref`).
#[cfg(feature = "rist")]
fn build_rist_push(
    output: &Output,
    url: &str,
    rist: Option<&RistOptions>,
) -> Result<RunnableOutput, PipelineError> {
    let to_err = |e: multiview_config::RistUrlError| PipelineError::Output {
        kind: "rist",
        reason: e.to_string(),
    };
    let lowered = rist_resolved_url(url, rist, false).map_err(to_err)?;
    let redacted = rist_resolved_url(url, rist, true).map_err(to_err)?;
    tracing::info!(transport = "rist", url = %redacted, "wiring rist push output");
    // OUTMETA: RIST carries MPEG-TS, so the SDT/PMT metadata applies (no
    // rotation tag — pixels-only orientation is the compositor's concern).
    let (meta, matrix) = output_mux_meta(output);
    Ok(RunnableOutput::Push {
        id: output.id(),
        sink: PacketMuxSink::push(PushProtocol::Rist, lowered).with_output_metadata(meta, matrix),
        label: "rist",
        url: redacted,
    })
}

/// Translate one config [`Output`]'s OUTMETA intent into the output-layer
/// apply values (ADR-0088 / ADR-0089): the muxer dictionary entries
/// ([`MuxMetadata`]) for the **`Applied`** metadata fields, and the tag-path
/// display-rotation [`DisplayMatrix`] when the orientation resolves to the
/// **tag** mechanism on this transport.
///
/// The config [`Output::metadata_plan`] already says which fields land on this
/// transport (the rest are surfaced as `Dropped`); here we map each `Applied`
/// field to the concrete libav dict key its carrier uses. A `Dropped` field is
/// never pushed (it was already reported). Orientation: a `tag`/`auto`-on-a-tag-
/// transport mechanism emits the display matrix; the **pixels** mechanism is a
/// compositor concern (a rotated rendition), not a mux tag, so it yields `None`
/// here. Returns an empty pair when the output carries neither.
fn output_mux_meta(output: &Output) -> (multiview_output::MuxMetadata, Option<DisplayMatrix>) {
    use multiview_config::{MetadataField, OrientationMechanism};

    let mut mux = multiview_output::MuxMetadata::new();
    if let Some(meta) = output.metadata() {
        let plan = output.metadata_plan();
        // The libav dict key for each config field is transport-family-specific.
        // The MPEG-TS family (SRT/RIST) uses the SDT/PMT keys mpegtsenc reads;
        // every other container uses the generic `title`/`comment`/`language`.
        let is_mpegts = matches!(output, Output::Srt { .. } | Output::Rist { .. });
        let title_key = if is_mpegts { "service_name" } else { "title" };
        let provider_key = if is_mpegts {
            "service_provider"
        } else {
            "author"
        };
        let desc_key = if is_mpegts { "service_name" } else { "comment" };

        let applied = |f: &Option<MetadataField>| matches!(f, Some(MetadataField::Applied { .. }));
        // Push only the fields the plan said landed; a malformed (interior-NUL)
        // value is surfaced via the log and dropped, never failing the run
        // (values are already validation-clean at config time). The
        // `(applied, value, key)` table is collected first so the fallible
        // pushes borrow `mux` one at a time (no closure capturing `mux`).
        let format_pushes: [(bool, Option<&String>, &str); 3] = [
            (applied(&plan.title), meta.title.as_ref(), title_key),
            (
                applied(&plan.provider),
                meta.provider.as_ref(),
                provider_key,
            ),
            (
                applied(&plan.description),
                meta.description.as_ref(),
                desc_key,
            ),
        ];
        for (is_applied, value, key) in format_pushes {
            if is_applied {
                if let Some(v) = value {
                    if let Err(e) = mux.push_format(key, v) {
                        tracing::warn!(key, error = %e, "skipping unencodable output metadata entry");
                    }
                }
            }
        }
        // service_id maps to the mpegts SDT/PMT program number; mpegtsenc reads
        // it from the format `service_id` key (carried by the TS family only).
        if applied(&plan.service_id) {
            if let Some(id) = meta.service_id {
                if let Err(e) = mux.push_format("service_id", id.to_string()) {
                    tracing::warn!(error = %e, "skipping unencodable service_id metadata");
                }
            }
        }
        // Language is a per-stream tag (the program video stream, index 0).
        if applied(&plan.language) {
            if let Some(v) = &meta.language {
                if let Err(e) = mux.push_stream(0, "language", v) {
                    tracing::warn!(error = %e, "skipping unencodable language metadata");
                }
            }
        }
        let _ = meta.timed; // timed-metadata side stream is the cue-injection path (ADR-0088 §4).
    }

    // Orientation tag path: emit the display matrix only when the resolved
    // mechanism is `Tag` (auto-on-a-tag-transport or explicit tag). The pixels
    // mechanism is handled in the compositor as a rotated rendition.
    let matrix = output.orientation().and_then(|o| {
        if o.is_identity() {
            return None;
        }
        match o.mechanism(output.orientation_tag_capability()) {
            OrientationMechanism::Tag => Some(display_matrix(o.turn)),
            // The pixels mechanism (and any future one) rotates the rendition in
            // the compositor, not via a mux tag — no display matrix here.
            _ => None,
        }
    });
    (mux, matrix)
}

/// Build the `RunnableOutput::Hls` for an HLS/LL-HLS output: create the segment
/// dir, then a rolling-live segment sink (HLS-0/1, ADR-0032) carrying the
/// OUTMETA per-output metadata + tag-path display matrix. The sink shares the
/// pipeline epoch cell so each closed segment is PDT-stamped from the same
/// outbound epoch the control WS publishes (DEV-C1 / ADR-M010).
fn build_hls_output(
    output: &Output,
    path: &str,
    epoch: &multiview_output::SharedEpoch,
) -> Result<RunnableOutput, PipelineError> {
    let (dir, prefix, playlist_path) = hls_paths(Path::new(path));
    std::fs::create_dir_all(&dir).map_err(|e| PipelineError::Output {
        kind: "hls",
        reason: format!("creating {}: {e}", dir.display()),
    })?;
    let (meta, matrix) = output_mux_meta(output);
    Ok(RunnableOutput::Hls {
        id: output.id(),
        sink: PacketMuxSink::segment_live(
            dir,
            prefix,
            playlist_path.clone(),
            HLS_LIVE_WINDOW,
            epoch.clone(),
        )
        .with_output_metadata(meta, matrix),
        playlist_path,
    })
}

/// Build a live push sink ([`RunnableOutput::Push`]) for an RTMP/SRT output: the
/// muxer targeting `url` over `protocol`, carrying the output's declarative
/// metadata (ADR-0088), fed the SAME encode-once packets as every other transport
/// (invariant #7).
fn build_push_output(
    output: &Output,
    protocol: PushProtocol,
    url: &str,
    label: &'static str,
) -> RunnableOutput {
    let (meta, matrix) = output_mux_meta(output);
    RunnableOutput::Push {
        id: output.id(),
        sink: PacketMuxSink::push(protocol, url.to_owned()).with_output_metadata(meta, matrix),
        label,
        url: url.to_owned(),
    }
}

/// Build the runnable sinks from the config outputs.
///
/// HLS/LL-HLS segment to disk; **RTMP and SRT push outputs are run** via the
/// [`PushSink`] (the same encode-once-mux-many drive loop the file/HLS sinks use —
/// invariant #7 — only the muxer targets a network URL). A **display** output
/// (DEV-B1 / ADR-0044) is built as a raw-frame DRM/KMS plan in a `display-kms`
/// build and is a hard error otherwise (never silently skipped — the gate in
/// [`crate::outputs`]). NDI out (OUT-4b) is a raw-canvas sink built under the
/// live `ndi-bindings` binding (an accepted `[system.ndi]` license gates it by
/// construction); a seam-only `ndi` build or an unaccepted/absent runtime is an
/// honest skip. The RTSP *server* is genuinely not implemented (its own
/// RTP/RTSP protocol stack) and is honestly skipped with a log line rather than
/// pretended-runnable — a config mixing an unsupported output with a supported
/// one still produces that supported output.
#[allow(clippy::too_many_lines)]
// reason: a flat build loop matching every configured output kind (display / HLS /
// push / RIST / WebRTC / RTSP / NDI / AES67) to its runnable, each arm a short
// build/skip; the value is reading the whole per-kind assembly in one place.
fn build_outputs(
    outputs: &[Output],
    epoch: &multiview_output::SharedEpoch,
    #[cfg(feature = "webrtc-native")] egress_sinks: &std::collections::HashMap<
        String,
        multiview_webrtc::egress::EgressSink,
    >,
    // The `[system.ndi]` license-acceptance settings (OUT-4b / NDI-L2): the live
    // NDI sender is gated by an accepted license by construction. Unused without
    // the `ndi-bindings` live binding.
    #[cfg_attr(not(feature = "ndi-bindings"), allow(unused_variables))] ndi_system: Option<
        &multiview_config::NdiSystemConfig,
    >,
    // The output cadence (exact rational): the NDI sink derives its tick-stamped
    // 100 ns timecode + frame-rate descriptor from it (invariant #3). Unused
    // without `ndi-bindings`.
    #[cfg_attr(not(feature = "ndi-bindings"), allow(unused_variables))] cadence: Rational,
) -> Result<BuiltOutputs, PipelineError> {
    // A display output in a non-display-kms build is a configuration the
    // binary cannot honour: fail the build clearly, never skip (DEV-B1).
    crate::outputs::ensure_display_outputs_supported(outputs).map_err(|reason| {
        PipelineError::Output {
            kind: "display",
            reason,
        }
    })?;
    // Same fail-closed contract for an AES67 raw-PCM output in a non-`aes67` build
    // (#103): reject clearly rather than warn-skip it into a dead stream.
    crate::outputs::ensure_aes67_outputs_supported(outputs).map_err(|reason| {
        PipelineError::Output {
            kind: "aes67",
            reason,
        }
    })?;
    // #103: distinct AES67 senders on ONE multicast group:port must advertise
    // distinct RTP SSRCs (a receiver demuxes by SSRC within a group — RFC 3550 §8).
    // The per-output SSRC is a 32-bit fold of id + group:port, so a same-group
    // collision is astronomically unlikely but not impossible — reject it
    // fail-closed here (config-time, off the hot path; inv #1/#10) rather than emit
    // two ambiguous senders. Senders on DIFFERENT groups may share an SSRC.
    #[cfg(feature = "aes67")]
    ensure_no_aes67_ssrc_collision(outputs)?;
    let mut runnable = Vec::new();
    #[cfg(feature = "display-kms")]
    let mut display_plans = Vec::new();
    #[cfg(feature = "ndi-bindings")]
    let mut ndi_publishers = Vec::new();
    #[cfg(feature = "aes67")]
    let mut aes67_handles = Vec::new();
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
                runnable.push(build_hls_output(output, path, epoch)?);
            }
            Output::Rtmp { url, .. } => {
                runnable.push(build_push_output(output, PushProtocol::Rtmp, url, "rtmp"));
            }
            Output::Srt { url, .. } => {
                runnable.push(build_push_output(output, PushProtocol::Srt, url, "srt"));
            }
            // RIST push (ADR-0095): the open-standard sibling of the SRT push.
            // The typed options lower to the `rist://…?…` AVIO URL (the PSK
            // resolved from its secret_ref); `PushProtocol::Rist` fans the SAME
            // encoded packets through the `mpegts` muxer (invariant #7). The
            // unredacted URL goes to the sink; only the redacted form is logged.
            #[cfg(feature = "rist")]
            Output::Rist { url, rist, .. } => {
                runnable.push(build_rist_push(output, url, rist.as_ref())?);
            }
            // Without the `rist` feature the librist obligation is not built in,
            // so a rist output is an honest skip (never a silent pretend-run),
            // exactly like the RTSP-server / NDI outputs below.
            #[cfg(not(feature = "rist"))]
            Output::Rist { .. } => {
                tracing::warn!(
                    "rist output requires the `rist` feature (off by default); skipping"
                );
            }
            // WebRTC program outputs (ADR-0049): a mux-free fan-out sink that
            // re-stamps the encode-once program packets into the output's bounded
            // drop-oldest egress feed (invariant #7). The WHEP-serve driver /
            // whip_push client (wired in the run path) drains the paired feed and
            // packetizes per session. Under `webrtc-native` only; without the
            // feature these are honestly skipped (no native transport linked).
            #[cfg(feature = "webrtc-native")]
            Output::Webrtc { .. } | Output::WhipPush { .. } => {
                let id = output.id();
                if let Some(sink) = egress_sinks.get(&id) {
                    let label = match output {
                        Output::WhipPush { .. } => format!("whip_push {id}"),
                        _ => format!("webrtc {id}"),
                    };
                    runnable.push(RunnableOutput::WebRtc {
                        id: id.clone(),
                        sink: sink.clone(),
                        label,
                    });
                } else {
                    tracing::warn!(
                        output = %id,
                        "webrtc/whip_push output has no registered egress feed; skipping"
                    );
                }
            }
            Output::RtspServer { .. } => {
                tracing::warn!(
                    "rtsp_server output is not implemented (an RTSP server is its own \
                     RTP/RTSP protocol stack); skipping"
                );
            }
            // NDI output (OUT-4b / NDI-L2): a raw-canvas sink, license-gated by
            // construction. Delegated to `handle_ndi_output`, which (under the live
            // `ndi-bindings` binding) builds the real `SdkNdiApi` sender behind the
            // `[system.ndi] accept_license` gate, or honestly skips with a logged
            // reason (seam-only `ndi`, no live binding, or an unaccepted/absent
            // runtime) — never a silent pretend-run, never a panic (inv #1/#10).
            Output::Ndi { .. } => {
                handle_ndi_output(
                    output,
                    #[cfg(feature = "ndi-bindings")]
                    ndi_system,
                    #[cfg(feature = "ndi-bindings")]
                    cadence,
                    #[cfg(feature = "ndi-bindings")]
                    &mut runnable,
                    #[cfg(feature = "ndi-bindings")]
                    &mut ndi_publishers,
                );
            }
            // AES67 / ST 2110-30 raw-PCM audio output (#103, ADR-0033/T013): a
            // mux-free multicast sink of the mixed program audio. The serve-side
            // `Aes67Sender` is built here (sync); its push handle is threaded to
            // the bake consumer, and the async multicast socket is bound later in
            // `run_aes67_output`. Under `aes67` only; a non-`aes67` build rejected
            // any aes67 output up front (the gate above), so the fall-through below
            // is defensive.
            #[cfg(feature = "aes67")]
            Output::Aes67 { .. } => {
                let (runnable_out, handle) = build_aes67_output(output)?;
                runnable.push(runnable_out);
                aes67_handles.push(handle);
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
        #[cfg(feature = "ndi-bindings")]
        ndi_publishers,
        #[cfg(feature = "aes67")]
        aes67_handles,
    })
}

/// AES67 TX channel count (#103): FIXED at 2 — the program bus is stereo, and the
/// bake consumer pushes that stereo block into the sender, whose channel count MUST
/// match or the push is silently dropped. Multi-channel / discrete-track AES67 is a
/// later slice.
#[cfg(feature = "aes67")]
const AES67_TX_CHANNELS: usize = 2;

/// AES67 TX dynamic RTP payload type (#103): 97, a common dynamic PT for L24 audio
/// (the receiver reads the format from the SDP, not the PT; any 96..=127 works).
#[cfg(feature = "aes67")]
const AES67_TX_PAYLOAD_TYPE: u8 = 97;

/// AES67 TX send-FIFO depth in frames (#103): 4800 frames = 100 ms @ 48 kHz — a
/// bounded drop-oldest buffer between the bake consumer (push) and the serve timer
/// (drain), so a stalled network sheds oldest rather than growing memory (inv #5).
#[cfg(feature = "aes67")]
const AES67_TX_CAPACITY_FRAMES: usize = 4_800;

/// AES67 TX default frames-per-packet (#103): 48 = 1 ms @ 48 kHz (Class-A ptime),
/// the fallback when the `ptime_ms * rate / 1000` widening ever saturates (it never
/// does for a validated ptime).
#[cfg(feature = "aes67")]
const AES67_DEFAULT_FRAMES_PER_PACKET: usize = 48;

/// Build the [`RunnableOutput::Aes67`] for an `Output::Aes67` (#103,
/// ADR-0033/T013): the serve-side [`Aes67Sender`](multiview_output::aes67::Aes67Sender)
/// framed from the output's depth/ptime + the fixed 48 kHz **stereo** program bus,
/// paired with the bake-consumer push
/// [`Aes67SenderHandle`](multiview_output::aes67::Aes67SenderHandle) the program
/// audio is fed into. Mux-free (no encode stage) — it multicasts the mixed program
/// to a `group:port`.
///
/// The sender is created here (sync, always-compiled) so its handle can be threaded
/// to the bake consumer; the async multicast socket is bound later, in
/// [`run_aes67_output`] (a current-thread runtime on the sink thread). `channels` is
/// FIXED at 2 to match the stereo program-bus block the consumer pushes —
/// [`Aes67SenderHandle::push`](multiview_output::aes67::Aes67SenderHandle::push)
/// silently drops a block whose channel count differs, so the two must agree.
///
/// # Errors
/// [`PipelineError::Output`] when the `multicast` group:port is malformed, the PCM
/// depth is unsupported, or the sender parameters are out of range
/// (`Aes67ConfigError`).
#[cfg(feature = "aes67")]
fn build_aes67_output(
    output: &Output,
) -> Result<(RunnableOutput, multiview_output::aes67::Aes67SenderHandle), PipelineError> {
    use multiview_output::aes67::{Aes67Sender, PcmDepth};

    let Output::Aes67 {
        multicast,
        depth,
        ptime_ms,
        ..
    } = output
    else {
        return Err(PipelineError::Output {
            kind: "aes67",
            reason: "build_aes67_output called on a non-aes67 output".to_owned(),
        });
    };
    let id = output.id();
    // The multicast destination `group:port` (a required schema field).
    let dest = multicast
        .parse::<std::net::SocketAddr>()
        .map_err(|e| PipelineError::Output {
            kind: "aes67",
            reason: format!(
                "aes67 output `{id}` multicast `{multicast}` is not a valid group:port: {e}"
            ),
        })?;
    // Egress from an ephemeral port on the group's-family wildcard.
    let local: std::net::SocketAddr = if dest.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    // L24 (Class-A interop default) unless the config says L16; a future/unknown
    // depth is a typed refusal (never silently mishandled).
    let pcm_depth = if depth.eq_ignore_ascii_case("l16") {
        PcmDepth::L16
    } else if depth.eq_ignore_ascii_case("l24") {
        PcmDepth::L24
    } else {
        return Err(PipelineError::Output {
            kind: "aes67",
            reason: format!(
                "aes67 output `{id}` has unsupported PCM depth `{depth}` (expected L16 or L24)"
            ),
        });
    };
    // A zero packet time is nonsensical (and the `.max(1)` below would silently
    // coerce it to a 1-frame ~0.02 ms packet flood). Reject it fail-closed instead.
    // (`ptime_ms * 48000 / 1000` is exactly `ptime_ms * 48` for any u32, so a
    // NON-zero ptime never truncates; an oversized one is caught by
    // `Aes67Sender::new`'s frames-per-packet bound below.)
    if *ptime_ms == 0 {
        return Err(PipelineError::Output {
            kind: "aes67",
            reason: format!("aes67 output `{id}` has a zero packet time (`ptime_ms` must be >= 1)"),
        });
    }
    // frames_per_packet = ptime_ms * rate / 1000 (Class-A ptime = 1 ms → 48 frames
    // @ 48 kHz). Computed in u64 to stay exact, clamped ≥ 1.
    let frames_per_packet =
        usize::try_from(u64::from(*ptime_ms).saturating_mul(u64::from(AES67_STORE_RATE_HZ)) / 1000)
            .unwrap_or(AES67_DEFAULT_FRAMES_PER_PACKET)
            .max(1);
    let sender = Aes67Sender::new(
        AES67_TX_CHANNELS,
        pcm_depth,
        AES67_TX_PAYLOAD_TYPE,
        aes67_ssrc_for(&id, dest),
        AES67_STORE_RATE_HZ,
        frames_per_packet,
        AES67_TX_CAPACITY_FRAMES,
    )
    .map_err(|e| PipelineError::Output {
        kind: "aes67",
        reason: format!("aes67 output `{id}` sender config invalid: {e}"),
    })?;
    let handle = sender.handle();
    let runnable = RunnableOutput::Aes67 {
        id: id.clone(),
        sender,
        local,
        dest,
        interface: multiview_output::aes67::transport::MulticastInterface::Unspecified,
        label: format!("aes67 {id}"),
    };
    Ok((runnable, handle))
}

/// A stable, non-zero RTP SSRC for an AES67 output, folded from its id AND its
/// multicast `group:port` (#103).
///
/// Uses **FNV-1a** (a small, fully-specified algorithm — mirroring the SAP
/// [`stable_hash`](multiview_input::sap::stable_hash) approach, P2-F4) over a
/// **stable byte encoding** of the address: the family tag + the raw IP octets +
/// the big-endian port, NOT `SocketAddr`'s `Display` string (whose formatting —
/// especially IPv6 zero-compression — is not a stable cross-version/-target byte
/// form). So the mapping is stable across toolchain versions, targets, and
/// restarts: a receiver keyed on the SSRC keeps seeing one sender as the same
/// stream across a rebuild. `DefaultHasher` (`SipHash`) is explicitly **not** a
/// stable cross-version contract, so it must never back a wire identifier. The
/// multicast group+port is folded in (not just the id) so distinct outputs get
/// distinct SSRCs **with overwhelming probability** (a 32-bit RTP SSRC space) — the
/// deterministic fold does NOT by itself *guarantee* uniqueness. The one HARD
/// guarantee is build-time and fail-closed ([`ensure_no_aes67_ssrc_collision`]): no
/// two AES67 outputs **configured in this build** that share a multicast
/// `group:port` may fold to the same SSRC. It does NOT detect or resolve a
/// collision with an EXTERNAL / third-party sender on that group — there is no
/// runtime RTCP SSRC collision detection or reselection here. Clamped away from `0`
/// (an ambiguous-but-legal SSRC).
#[cfg(feature = "aes67")]
fn aes67_ssrc_for(id: &str, dest: std::net::SocketAddr) -> u32 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    // id, then a separator so ("ab", …) and ("a", "b…") cannot alias to one digest.
    let mut hash = fnv1a_absorb(FNV_OFFSET, id.as_bytes());
    hash = fnv1a_absorb(hash, b"@");
    // Address family tag + raw IP octets (the stable wire bytes) + big-endian port.
    hash = match dest.ip() {
        std::net::IpAddr::V4(v4) => fnv1a_absorb(fnv1a_absorb(hash, &[4]), &v4.octets()),
        std::net::IpAddr::V6(v6) => fnv1a_absorb(fnv1a_absorb(hash, &[6]), &v6.octets()),
    };
    hash = fnv1a_absorb(hash, &dest.port().to_be_bytes());
    // Fold the 64-bit digest into 32 bits, then force non-zero.
    let folded = (hash ^ (hash >> 32)) & u64::from(u32::MAX);
    u32::try_from(folded).unwrap_or(1).max(1)
}

/// One FNV-1a absorb pass: XOR-then-multiply each byte into `hash` (the offset
/// basis / running digest is supplied by the caller). Split out so the SSRC fold
/// composes id + address-family + octets + port without repeating the loop.
#[cfg(feature = "aes67")]
fn fnv1a_absorb(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Fail closed if two AES67 outputs on the **same multicast group:port** fold to
/// the **same RTP SSRC** (#103).
///
/// [`aes67_ssrc_for`] folds to 32 bits, which is distinct with overwhelming
/// probability but not *guaranteed* unique. An RTP receiver demuxes by SSRC
/// **within one multicast group** (RFC 3550 §8), so only a same-`(group:port)`
/// collision is ambiguous — two senders on **different** groups may share an SSRC
/// harmlessly and are NOT rejected (no false positives). This runs once at config
/// time (off the hot path; inv #1/#10). Outputs with a malformed `multicast` are
/// skipped here — [`build_aes67_output`] reports that separately.
///
/// # Errors
///
/// [`PipelineError::Config`] naming BOTH colliding output ids and their shared
/// group + SSRC.
#[cfg(feature = "aes67")]
fn ensure_no_aes67_ssrc_collision(outputs: &[Output]) -> Result<(), PipelineError> {
    // Keyed on (multicast dest, SSRC): a duplicate key is two senders on ONE group
    // that folded to ONE SSRC — the only ambiguous case.
    let mut seen: std::collections::HashMap<(std::net::SocketAddr, u32), String> =
        std::collections::HashMap::new();
    for output in outputs {
        let Output::Aes67 { multicast, .. } = output else {
            continue;
        };
        // A malformed multicast is a typed refusal in `build_aes67_output`; this
        // SSRC-uniqueness pass only considers parseable destinations.
        let Ok(dest) = multicast.parse::<std::net::SocketAddr>() else {
            continue;
        };
        let id = output.id();
        let ssrc = aes67_ssrc_for(&id, dest);
        if let Some(other) = seen.insert((dest, ssrc), id.clone()) {
            return Err(PipelineError::Config(
                multiview_config::ConfigError::Validation(format!(
                    "aes67 outputs `{other}` and `{id}` collide on multicast group \
                     `{dest}` with the same RTP SSRC {ssrc}: distinct senders on one \
                     multicast group must have distinct SSRCs — rename one output"
                )),
            ));
        }
    }
    Ok(())
}

/// Build one live NDI output sink (OUT-4b / NDI-L2): enforce the
/// `[system.ndi] accept_license` gate, load the runtime, create the sender, and
/// return the [`RunnableOutput::Ndi`] (carrying the sink + the canvas-tap reader)
/// paired with the engine-side [`FramePublisher`] the hot loop publishes into.
///
/// The license gate is checked FIRST (ADR-0008 §7.5): an unaccepted/incomplete
/// `accept_license` is refused BEFORE the runtime is touched, so an unaccepted
/// operator never loads the proprietary SDK — mirroring the ingest gate
/// ([`connect_ndi_receiver`]). The accepted [`NdiLicense`] then gates
/// `NdiOutput::new` by construction: there is no way to build a sender without it.
///
/// # Errors
/// A human-readable reason string when the license is not accepted, the runtime
/// cannot be loaded, or the sender cannot be created — the caller logs it and
/// **skips this output** (the run continues; invariants #1/#10). Never panics.
#[cfg(feature = "ndi-bindings")]
fn build_ndi_output(
    output: &Output,
    name: &str,
    ndi_system: Option<&multiview_config::NdiSystemConfig>,
    cadence: Rational,
) -> Result<
    (
        RunnableOutput,
        multiview_output::display::FramePublisher<NdiCanvasFrame>,
    ),
    String,
> {
    use multiview_output::ndi::license::LicenseAcceptance;
    use multiview_output::ndi::{NdiCapability, NdiLicense, SdkNdiApi};

    // License gate FIRST (ADR-0008 §7.5): refuse with the typed reason before any
    // runtime load. Absent `[system.ndi]` ⇒ not accepted (empty audit fields).
    let (accept_license, acceptance) = ndi_system.map_or_else(
        || {
            (
                false,
                LicenseAcceptance {
                    accepted_by: String::new(),
                    accepted_at: String::new(),
                },
            )
        },
        |s| {
            (
                s.accept_license,
                LicenseAcceptance {
                    accepted_by: s.accepted_by.clone().unwrap_or_default(),
                    accepted_at: s.accepted_at.clone().unwrap_or_default(),
                },
            )
        },
    );
    let license = NdiLicense::from_setting(accept_license, acceptance)
        .map_err(|_| "ndi_unlicensed: [system.ndi] accept_license is not accepted".to_owned())?;

    // Runtime load (only AFTER the gate passes). An absent/unusable runtime is a
    // typed status, surfaced as the skip reason — never a panic, never a block.
    let capability =
        NdiCapability::load().map_err(|status| format!("NDI runtime unavailable ({status:?})"))?;

    // The accepted license gates `NdiOutput::new` by construction.
    let sink = NdiOutput::new(license, SdkNdiApi::new(capability), name.to_owned())
        .map_err(|e| format!("NDI sender create failed: {e}"))?;

    // The wait-free canvas tap: the engine publishes into `publisher`, the sink
    // runner reads `reader` (newest-wins, drop-oldest) — the engine never blocks.
    let (publisher, reader) = multiview_output::display::frame_mailbox::<NdiCanvasFrame>();
    Ok((
        RunnableOutput::Ndi {
            id: output.id(),
            sink,
            reader,
            cadence,
            name: name.to_owned(),
        },
        publisher,
    ))
}

/// Handle one `Output::Ndi` in [`build_outputs`], dispatching on the NDI feature
/// set so the `build_outputs` match stays a single arm (OUT-4b / NDI-L2):
///
/// - With the live `ndi-bindings` binding: build the license-gated
///   [`RunnableOutput::Ndi`] via [`build_ndi_output`] and push it (+ its
///   canvas-tap publisher) into the accumulators, or log an honest skip reason
///   (unaccepted license / absent runtime / create failure) and continue.
/// - With seam-only `ndi` (no live binding): an honest skip — the `SdkNdiApi`
///   cannot be constructed without the SDK function table.
/// - Without `ndi`: an honest skip — the runtime-loaded SDK is not built in.
///
/// Never a panic, never a silent pretend-run, never a crash of the other outputs
/// (invariants #1/#10).
fn handle_ndi_output(
    output: &Output,
    #[cfg(feature = "ndi-bindings")] ndi_system: Option<&multiview_config::NdiSystemConfig>,
    #[cfg(feature = "ndi-bindings")] cadence: Rational,
    #[cfg(feature = "ndi-bindings")] runnable: &mut Vec<RunnableOutput>,
    #[cfg(feature = "ndi-bindings")] ndi_publishers: &mut Vec<
        multiview_output::display::FramePublisher<NdiCanvasFrame>,
    >,
) {
    #[cfg(feature = "ndi-bindings")]
    {
        let Output::Ndi { name, .. } = output else {
            return;
        };
        match build_ndi_output(output, name, ndi_system, cadence) {
            Ok((sink, publisher)) => {
                runnable.push(sink);
                ndi_publishers.push(publisher);
            }
            Err(reason) => tracing::warn!(
                output = %output.id(),
                %reason,
                "ndi output not started; the run continues without it"
            ),
        }
    }
    #[cfg(all(feature = "ndi", not(feature = "ndi-bindings")))]
    {
        let _ = output;
        tracing::warn!(
            "ndi output requires the `ndi-bindings` feature + a resolvable NDI runtime \
             (the live SDK function table); the seam-only `ndi` build cannot send. Skipping"
        );
    }
    #[cfg(not(feature = "ndi"))]
    {
        let _ = output;
        tracing::warn!(
            "ndi output requires the `ndi` feature (off by default; runtime-loaded SDK); \
             skipping"
        );
    }
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
        // A push / WebRTC output has no on-disk directory to derive a program
        // file from; only an HLS output anchors the self-contained `program.ts`.
        RunnableOutput::File { .. } | RunnableOutput::Push { .. } => None,
        #[cfg(feature = "webrtc-native")]
        RunnableOutput::WebRtc { .. } => None,
        // The NDI sink is a raw-canvas consumer, not a packet/disk muxer — it
        // anchors no `program.ts`.
        #[cfg(feature = "ndi-bindings")]
        RunnableOutput::Ndi { .. } => None,
        // The AES67 sink is a raw-PCM multicast consumer, not a packet/disk muxer —
        // it anchors no `program.ts`.
        #[cfg(feature = "aes67")]
        RunnableOutput::Aes67 { .. } => None,
    });
    if let Some(path) = file_path {
        runnable.insert(
            0,
            RunnableOutput::File {
                // The self-contained anchor is synthetic (derived from the first
                // HLS output, not a config output), so it carries a synthetic
                // resource id for the ADR-0060 output scope.
                id: "program.ts".to_owned(),
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

/// Allocate the **swappable resolved-URL slot** for a `YouTube` source (IN-5b),
/// or `None` for every other source kind.
///
/// The slot starts **empty**: the first (re)open resolves inline (cold start) and
/// the proactive re-resolution loop (spawned in [`IngestSupervisor::start`])
/// publishes the next-up `*.googlevideo.com` master into it `lead` seconds before
/// the active URL's `expire` deadline — so the reconnect that follows expiry finds
/// a fresh URL already in hand (make-before-break), no cold resolve at the boundary
/// (ADR-0015 P2–P4). Lock-free, written only by the loop (invariants #1/#10).
#[cfg(feature = "youtube")]
fn youtube_url_slot_for(location: &SourceLocation) -> Option<Arc<arc_swap::ArcSwapOption<String>>> {
    match location {
        SourceLocation::Youtube { .. } => Some(Arc::new(arc_swap::ArcSwapOption::empty())),
        _ => None,
    }
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
// A flat, one-arm-per-`SourceKind` dispatch plus the single feature-gated
// `IngestPlan` struct literal: inherently long and clearer kept whole than split
// across helpers (adding the `player` field tipped it one line over the 100 bound).
#[allow(clippy::too_many_lines)]
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
        // RIST ingest (ADR-0095): the open-standard sibling of SRT. The typed
        // options lower to the `rist://…?…` AVIO URL (PSK resolved from its
        // secret_ref); it then rides the SAME libav demuxer + supervised
        // reconnect + last-good store as every other network source — its ARQ
        // recovery is an input jitter buffer, never the output clock
        // (invariants #1/#2/#3; bad-inputs-are-the-purpose). `live` so the
        // ingest loop reconnects forever.
        #[cfg(feature = "rist")]
        SourceKind::Rist { url, rist } => {
            let lowered = rist_resolved_url(url, rist.as_ref(), false).map_err(|e| {
                PipelineError::Ingest {
                    id: source.id.clone(),
                    reason: e.to_string(),
                }
            })?;
            (SourceLocation::Url(lowered), true)
        }
        // Without the `rist` feature the librist obligation is not built in, so a
        // rist source is an honest typed refusal (never a silent skip), exactly
        // like an NDI/webrtc source without its feature.
        #[cfg(not(feature = "rist"))]
        SourceKind::Rist { .. } => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "RIST ingest requires the `rist` feature (off by default)".to_owned(),
            })
        }
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
        // receive fault, an absent runtime, or an unaccepted NDI license (the
        // runtime gate, stamped onto the plan from `[system.ndi]` after build)
        // degrades the tile on the ingest thread, never failing the build
        // (invariants #1/#10). Only wired under the off-by-default `ndi` feature.
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
        // A WHIP ingest (`kind = "webrtc"`) source: Multiview is the server, so
        // there is nothing to dial — `live` so the drive loop runs forever
        // (waiting for / ingesting publishers). The decode happens off the render
        // thread in `drive_webrtc` (ADR-T014). Only under `webrtc-native`.
        #[cfg(feature = "webrtc-native")]
        SourceKind::Webrtc { audio, .. } => (SourceLocation::Webrtc { audio: *audio }, true),
        // With `webrtc-native` OFF the str0m endpoint is not built, so a webrtc
        // source is an honest typed refusal (never a silent skip).
        #[cfg(not(feature = "webrtc-native"))]
        SourceKind::Webrtc { .. } => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "WHIP ingest requires the `webrtc-native` feature (off by default)"
                    .to_owned(),
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

    // A YouTube source gets an empty swappable-URL slot (IN-5b); every other kind
    // has none. Computed before the struct literal moves `location`.
    #[cfg(feature = "youtube")]
    let youtube_url_slot = youtube_url_slot_for(&location);

    Ok(IngestPlan {
        id: source.id.clone(),
        location,
        // Not a media-player channel (those are built from `config.media_players`).
        player: None,
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
        // No placement decision yet: the load-aware admission pick (decide-
        // once, in `drive_streaming`) stamps `Pinned(ordinal)` onto every plan
        // before the startup threads spawn; the live consult stamps a runtime
        // add's outcome. `Default` is the default-device / GPU-free path, in
        // lockstep with the compositor's default adapter.
        decode_placement: DecodePlacement::Default,
        // The WHIP publisher rendezvous + audio store are stamped after build
        // (the registry/store are owned by the run wiring), like `cuda_ordinal`.
        #[cfg(feature = "webrtc-native")]
        webrtc_registry: None,
        #[cfg(feature = "webrtc-native")]
        webrtc_audio_store: None,
        // Declined by default; the NDI license acceptance (ADR-0008 §7.5) is
        // stamped from `config.system` after build (like `cuda_ordinal`), so an
        // NDI source is refused (`ndi_unlicensed`) until the operator accepts.
        #[cfg(feature = "ndi")]
        ndi_accept_license: false,
        #[cfg(feature = "ndi")]
        ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
            accepted_by: String::new(),
            accepted_at: String::new(),
        },
        #[cfg(feature = "youtube")]
        youtube_url_slot,
    })
}

/// Map the config-schema [`EofPolicy`](multiview_config::media::EofPolicy) onto
/// the player transport core's [`EofPolicy`](crate::player::EofPolicy).
fn map_eof_policy(policy: multiview_config::media::EofPolicy) -> crate::player::EofPolicy {
    use crate::player::EofPolicy as P;
    use multiview_config::media::EofPolicy as C;
    match policy {
        C::Loop => P::Loop,
        C::Black => P::Black,
        C::AutoOff => P::AutoOff,
        // `HoldLastFrame` (the default) and any future `#[non_exhaustive]` variant
        // this build does not know about both hold the last-good frame.
        _ => P::HoldLastFrame,
    }
}

/// The result of boot-spawning the configured media-player channels: the player
/// [`IngestPlan`]s (each with a `player` handle) and the per-player
/// [`TransportMailbox`](crate::player::TransportMailbox) the control-plane
/// command drain submits transport verbs to, keyed by player id.
struct MediaPlayerBoot {
    plans: Vec<IngestPlan>,
    stores: Vec<(String, Arc<TileStore<Nv12Image>>)>,
    mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>>,
    /// Per-player **audio** stores (ADR-T019): a player with a rolling asset gets
    /// an `AudioStore` the program bus routes (so its looped audio joins the mix);
    /// the `player_audio_loop` thread fills it. Empty for an idle player.
    audio_stores: Vec<(String, Arc<multiview_audio::store::AudioStore>)>,
    /// Per-player audio loop plans (ADR-T019): how to prime + loop each rolling
    /// player's embedded audio, carrying the SAME vamp geometry + mailbox the video
    /// uses so audio wraps on the same instant. One per rolling player.
    audio_plans: Vec<crate::audio::PlayerAudioPlan>,
}

/// Build the boot-time media-player channels from `config.media_players`
/// (ADR-0057 / ADR-0097).
///
/// Each player that has a `default` asset **with a declared `out_point_frames`**
/// boot-spawns a player [`IngestPlan`] that plays (or vamps, per `loop_default`)
/// the asset over its `[in_point, out_point)` window, looping the
/// `[vamp_in, vamp_out)` sub-window (defaulting to the whole clip). A player with
/// no default, or whose default asset has no declared out-point (the asset's
/// frame count is not known without probing — probe-at-load is post-MVP), boots
/// **idle**: its mailbox is still registered (so a later `load`/transport command
/// is honoured once probe-at-load lands), but no plan rolls now — surfaced with a
/// warning, never a silent skip.
///
/// All players' mailboxes are returned regardless, so the control-plane command
/// drain can address every declared channel.
// One per-player block that resolves the asset, validates the geometry, builds
// the store + mailbox + handle, and constructs the (feature-gated) player
// `IngestPlan` literal: inherently long and clearer kept whole than split across
// helpers that would each take most of these locals.
#[allow(clippy::too_many_lines)]
fn build_media_player_boot(
    config: &MultiviewConfig,
    layout: &Layout,
    cadence: Rational,
    canvas_color: CanvasColor,
) -> MediaPlayerBoot {
    let mut boot = MediaPlayerBoot {
        plans: Vec::new(),
        stores: Vec::new(),
        mailboxes: std::collections::HashMap::new(),
        audio_stores: Vec::new(),
        audio_plans: Vec::new(),
    };
    let library = config.media_library.as_ref();
    let root = library.and_then(|l| l.root.as_deref());

    for player in &config.media_players {
        // Every declared channel gets a registered mailbox so the control plane
        // can address it, even when it boots idle.
        let mailbox = Arc::new(crate::player::TransportMailbox::new());
        boot.mailboxes
            .insert(player.id.clone(), Arc::clone(&mailbox));

        // Resolve the default asset (if any) and require a declared out-point to
        // boot a rolling channel.
        let Some(asset_id) = player.default.as_deref() else {
            tracing::info!(player = %player.id, "media player boots idle (no default asset)");
            continue;
        };
        let asset = library.and_then(|l| l.assets.iter().find(|a| a.id == asset_id));
        let Some(asset) = asset else {
            tracing::warn!(
                player = %player.id, asset = %asset_id,
                "media player boots idle: default asset not found in the media library"
            );
            continue;
        };
        let Some(out_point) = asset.out_point_frames else {
            tracing::warn!(
                player = %player.id, asset = %asset_id,
                "media player boots idle: default asset declares no out_point_frames \
                 (probe-at-load is post-MVP)"
            );
            continue;
        };
        let in_point = asset.in_point_frames.unwrap_or(0);
        // The vamp window defaults to the whole trimmed clip when unset.
        let vamp_in = asset.vamp_in_frames.unwrap_or(in_point);
        let vamp_out = asset.vamp_out_frames.unwrap_or(out_point);
        let geometry = match crate::player::PlayoutGeometry::new(
            in_point, out_point, vamp_in, vamp_out, cadence,
        ) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    player = %player.id, asset = %asset_id, error = %e,
                    "media player boots idle: invalid playout geometry"
                );
                continue;
            }
        };

        // Resolve the asset path against the library root.
        let path = match root {
            Some(r) => std::path::Path::new(r).join(&asset.path),
            None => std::path::PathBuf::from(&asset.path),
        };
        // The libav-openable audio location (the SAME media as the video; the audio
        // peer opens its own `!Send` libav context) for the player audio loop.
        let audio_location = path.to_string_lossy().into_owned();

        let (tile_w, tile_h) = cell_pixel_size(layout, &player.id)
            .unwrap_or((config.canvas.width, config.canvas.height));
        let store = Arc::new(TileStore::new(
            player.id.clone(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let eof_policy = map_eof_policy(player.eof_policy);
        let handle = crate::player::PlayerHandle::new(
            player.id.clone(),
            geometry,
            eof_policy,
            player.loop_default,
            mailbox,
        );

        // ADR-T019 §1: this rolling player gets an `AudioStore` (routed onto the
        // bus like any source) + an audio loop plan carrying the SAME vamp geometry
        // AND the SAME `PlayerControlBus` the video handle publishes to — so the
        // audio rail samples the video's authoritative transport state and wraps/
        // exits on the same instant by construction. A silent asset rides silence
        // (the deck primes empty).
        let audio_control_bus = Arc::clone(&handle.control_bus);
        let audio_store = crate::audio::new_store();
        boot.audio_stores
            .push((player.id.clone(), Arc::clone(&audio_store)));
        boot.audio_plans.push(crate::audio::PlayerAudioPlan {
            id: player.id.clone(),
            location: audio_location,
            vamp_in_frames: geometry.vamp_in(),
            vamp_out_frames: geometry.vamp_out(),
            cadence: geometry.cadence(),
            output_cadence: cadence,
            control_bus: audio_control_bus,
        });

        boot.stores.push((player.id.clone(), Arc::clone(&store)));
        boot.plans.push(IngestPlan {
            id: player.id.clone(),
            location: SourceLocation::Path(path),
            player: Some(handle),
            tile_w,
            tile_h,
            store,
            // A media player is a finite asset played under transport control;
            // it is not a live (reconnect-forever) source — the player loop owns
            // its own loop/hold lifecycle.
            live: false,
            #[cfg(feature = "overlay")]
            incontainer_sub: None,
            #[cfg(feature = "overlay")]
            embedded_cc: None,
            canvas_color,
            cadence,
            decode_placement: DecodePlacement::Default,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry: None,
            #[cfg(feature = "webrtc-native")]
            webrtc_audio_store: None,
            #[cfg(feature = "ndi")]
            ndi_accept_license: false,
            #[cfg(feature = "ndi")]
            ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
                accepted_by: String::new(),
                accepted_at: String::new(),
            },
            #[cfg(feature = "youtube")]
            youtube_url_slot: None,
        });
        tracing::info!(
            player = %player.id, asset = %asset_id,
            in_point, out_point, vamp_in, vamp_out,
            looping = player.loop_default,
            "media player boot-spawned"
        );
    }
    boot
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
        // RIST audio is decoded from the SAME lowered `rist://` URL as its video
        // (the audio peer opens its own libav context). A lowering error (bonding
        // / unresolved PSK) means no audio path — the source rides silence on the
        // bus rather than failing the build (the video plan surfaces the error).
        #[cfg(feature = "rist")]
        SourceKind::Rist { url, rist } => match rist_resolved_url(url, rist.as_ref(), false) {
            Ok(lowered) => (lowered, true),
            Err(_) => return None,
        },
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
        // A WHIP/webrtc source is a depacketized RTP video receive (no libav
        // container to open here), so it carries no in-container subtitle
        // stream — mirroring the NDI arm. Pre-existing #181-era cross-feature
        // gap (the overlay-gated match lacked the webrtc-native variant arm, so
        // `--features overlay,webrtc-native` failed E0004); folded in here as the
        // `pipeline.rs` owner (task #137), not a media-player change.
        #[cfg(feature = "webrtc-native")]
        SourceLocation::Webrtc { .. } => return None,
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
    /// A **WHIP ingest** (`kind = "webrtc"`) contribution source (ADR-T014):
    /// Multiview is the server, so there is nothing to dial — a publisher
    /// (browser / OBS) `POST`s to the derived endpoint and the negotiated RTP
    /// ring is rendezvous'd to this source's drive loop via the shared
    /// [`WhipRegistry`](crate::webrtc_ingest::WhipRegistry). The drive loop
    /// ([`drive_webrtc`]) waits for a publisher, decodes its depacketized
    /// H.264/Opus into the standard `TileStore`/`AudioStore`, and rides
    /// `NO_SIGNAL` between publishers. Only wired under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    Webrtc {
        /// Whether the publisher's Opus audio is accepted (from the source kind).
        audio: bool,
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
        // WHIP ingest never opens a libav input (str0m surfaces RTP; the drive
        // loop decodes per-packet), so it gets no libav open options.
        #[cfg(feature = "webrtc-native")]
        SourceLocation::Webrtc { .. } => false,
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
        // WHIP ingest opens no libav input, so it is never an HLS master.
        #[cfg(feature = "webrtc-native")]
        SourceLocation::Webrtc { .. } => return false,
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
/// Enter the per-source **resource scope** for the current decode thread
/// (ADR-0060 §3.1, mechanism A): a thread-local [`ResourceContext`] tagged with
/// the source's stable config id, set for the lifetime of the returned RAII
/// [`ResourceGuard`] and cleared on its drop. Every libav line emitted
/// synchronously on this thread while the guard is live (the HLS open, a decode
/// error, an AVIO statistic) is attributed to this source by the libav→`tracing`
/// bridge — so the operator sees *which* tile is flapping, not just the codec
/// family. The guard is a cheap thread-local swap (no allocation on enter/drop),
/// safe to hold across the whole decode/reconnect region; it never blocks and
/// never back-pressures the engine (invariant #10).
///
/// The returned [`ResourceGuard`] is itself `#[must_use]` (dropping it early
/// ends the attribution scope), so this fn carries no redundant `#[must_use]`.
fn ingest_resource_scope(id: &str) -> multiview_ffmpeg::ResourceGuard {
    multiview_ffmpeg::ResourceGuard::enter(multiview_ffmpeg::ResourceContext::source(id))
}

fn ingest_loop(plan: &IngestPlan, stop: &AtomicBool) {
    // ADR-0060: run this source's whole decode/reconnect region inside a resource
    // scope so our own logs (via the `source` span) AND libav's synchronous lines
    // (via the thread-local ResourceContext the bridge resolves) name this source
    // by its stable config id. The span is entered for the thread's lifetime; the
    // guard sets the thread-local the libav bridge reads. Both clear on return.
    let _span = tracing::info_span!(
        "source",
        resource_kind = "source",
        resource_id = %plan.id,
    )
    .entered();
    let _resource = ingest_resource_scope(&plan.id);
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
    // WHIP ingest (ADR-T014): Multiview is the server, so there is nothing to
    // dial. Route it to its own supervised drive loop, which waits for a
    // publisher (rendezvous'd via the WhipRegistry), decodes its depacketized
    // H.264/Opus into the last-good store, and rides NO_SIGNAL between
    // publishers. It only writes the lock-free store — it never paces or stalls
    // the output clock nor back-pressures the engine (inv #1/#2/#10). Only under
    // `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    if let SourceLocation::Webrtc { audio } = &plan.location {
        drive_webrtc(plan, *audio, stop);
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
        match connect_ndi_receiver(name, plan.ndi_accept_license, plan.ndi_acceptance.clone()) {
            Ok((license, receiver)) => {
                let mut producer = multiview_input::ndi::NdiProducer::new(license, receiver);
                drive_ndi_producer(plan, &mut producer, stop);
            }
            Err(status) => {
                // Runtime absent / unusable / unlicensed / no live receiver yet: log
                // the honest status token and let the tile degrade. We do NOT spin —
                // the reconnect backoff below bounds retry frequency (so this warn
                // repeats at most once per backoff cycle, settling to ~1/30s), and
                // `stop`/prime-wait are never blocked. An unaccepted license is
                // terminal for this run: the acceptance is stamped onto the plan at
                // build time, so re-evaluating it each retry yields the same refusal
                // until a config reload restarts the process (`system` is a
                // restart-class diff section; live acceptance/revocation propagation
                // is a deferred follow-up).
                tracing::warn!(
                    source = %plan.id,
                    ndi_source = name,
                    status = status.status_label(),
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

/// Attempt to connect a live NDI receiver for source `name`, gated by the NDI
/// license acceptance.
///
/// The license gate is checked FIRST (ADR-0008 §7.5): an unaccepted or incomplete
/// `[system.ndi] accept_license` is refused with the
/// [`NdiLoadStatus::Unlicensed`](multiview_input::ndi::NdiLoadStatus) status
/// (`ndi_unlicensed`) BEFORE the runtime is probed, so an unaccepted source never
/// touches the SDK.
///
/// HONEST SCOPE (live-only deferred half): binding a real
/// [`NdiReceiver`](multiview_input::ndi::NdiReceiver) onto the resolved
/// `multiview-ndi-sys` function table needs the proprietary SDK ABI + a running NDI
/// network — neither exists in CI — so once the gate passes this probes the runtime
/// and returns the typed status without a live receiver yet. With the runtime
/// absent (the default case) it reports the unavailable status so the tile degrades;
/// the converter + `NdiProducer` drive shape that *consume* a receiver are complete
/// and tested in `multiview-input`. It never panics or blocks.
#[cfg(feature = "ndi")]
fn connect_ndi_receiver(
    _name: &str,
    accept_license: bool,
    acceptance: multiview_input::ndi::license::LicenseAcceptance,
) -> Result<
    (
        multiview_input::ndi::NdiLicense,
        Box<dyn multiview_input::ndi::NdiReceiver + Send>,
    ),
    multiview_input::ndi::NdiLoadStatus,
> {
    // License gate FIRST (ADR-0008 §7.5): an unaccepted/incomplete acceptance is
    // refused with the `ndi_unlicensed` status BEFORE the runtime is probed, so an
    // unaccepted source never touches the SDK. When the live receiver binding (the
    // deferred half) lands, re-evaluate the gate and return Ok((license, receiver))
    // so the accepted guard gates `NdiProducer::new` by construction.
    multiview_input::ndi::NdiLicense::from_setting(accept_license, acceptance)
        .map_err(|_| multiview_input::ndi::NdiLoadStatus::Unlicensed)?;
    // Runtime probe; the live SDK-backed receiver binding is the deferred half, so
    // there is no receiver to pair with the (accepted) license yet — surface the
    // probe status (RuntimeNotFound when absent, Available when present) so the drive
    // loop logs the honest reason and the tile degrades.
    let status = multiview_input::ndi::NdiCapability::probe();
    Err(status)
}

#[cfg(all(test, feature = "ndi"))]
mod ndi_license_gate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multiview_input::ndi::license::LicenseAcceptance;
    use multiview_input::ndi::NdiLoadStatus;

    fn complete_audit() -> LicenseAcceptance {
        LicenseAcceptance {
            accepted_by: "ops".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn connect_refuses_unlicensed_before_probing_the_runtime() {
        // accept_license = false: the gate refuses with the distinct `ndi_unlicensed`
        // status BEFORE the runtime is probed — never RuntimeNotFound, never a panic.
        // (ADR-0008 §7.5: the license axis is checked first and is its own status.)
        // `match` (not `expect_err`): the Ok arm carries a `Box<dyn NdiReceiver>`,
        // which is not `Debug`.
        match connect_ndi_receiver("STUDIO (CAM 1)", false, complete_audit()) {
            Ok(_) => panic!("an unaccepted NDI source must be refused"),
            Err(status) => assert_eq!(
                status,
                NdiLoadStatus::Unlicensed,
                "unaccepted must surface ndi_unlicensed, distinct from the runtime axis"
            ),
        }
    }

    #[test]
    fn connect_passes_the_license_gate_when_accepted() {
        // accept_license = true + complete audit: the license gate passes, so the
        // result is NEVER the Unlicensed refusal. The live receiver binding is the
        // deferred half, so this still reports the runtime probe status on a host
        // with no runtime — but never the license refusal.
        match connect_ndi_receiver("STUDIO (CAM 1)", true, complete_audit()) {
            Ok(_) => { /* a live receiver bound (SDK box): the gate passed */ }
            Err(status) => assert_ne!(
                status,
                NdiLoadStatus::Unlicensed,
                "an accepted + audited source must pass the license gate"
            ),
        }
    }
}

#[cfg(all(test, feature = "ndi"))]
mod ndi_egress_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::sync::Arc;

    use multiview_compositor::pipeline::Nv12Image;
    use multiview_core::time::Rational;
    use multiview_output::display::frame_mailbox;
    use multiview_output::ndi::license::LicenseAcceptance;
    use multiview_output::ndi::{FakeNdiApi, NdiLicense, NdiOutput};

    use super::{ndi_timecode_100ns, run_ndi_output, NdiCanvasFrame};

    fn accepted() -> NdiLicense {
        NdiLicense::accept(LicenseAcceptance {
            accepted_by: "ops".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        })
        .expect("a complete acceptance yields a license")
    }

    /// A 4x2 NV12 canvas whose Y plane carries a per-pixel ramp keyed by `seed`,
    /// so a content-aware reader could prove this exact frame survived the
    /// NV12→UYVY conversion (the live roundtrip asserts that; the offline drive
    /// asserts the send shape).
    fn ramp_canvas(seed: u8) -> Nv12Image {
        let (w, h) = (4u32, 2u32);
        let y: Vec<u8> = (0..(w * h))
            .map(|i| seed.wrapping_add(u8::try_from(i % 256).unwrap_or(0)))
            .collect();
        let uv = vec![128u8; usize::try_from(w * h / 2).unwrap()];
        Nv12Image::new(w, h, y, uv, multiview_core::color::ColorInfo::default()).unwrap()
    }

    /// The NDI 100 ns timecode is derived from the TICK index + the exact-rational
    /// cadence (invariant #3) — never from wall clock or a raw input PTS.
    #[test]
    fn timecode_is_tick_derived_exact_rational() {
        // 30000/1001 (NTSC): one frame is 1001/30000 s = 333_666.6… (100 ns units).
        let cadence = Rational {
            num: 30_000,
            den: 1_001,
        };
        assert_eq!(ndi_timecode_100ns(0, cadence), 0);
        // tick 3 → 3 * 1001 * 10_000_000 / 30000 = 1_001_000 (integer truncation).
        assert_eq!(ndi_timecode_100ns(3, cadence), 1_001_000);
        // A 25 fps cadence: one frame = 400_000 (100 ns units), exact.
        let p25 = Rational { num: 25, den: 1 };
        assert_eq!(ndi_timecode_100ns(1, p25), 400_000);
        assert_eq!(ndi_timecode_100ns(5, p25), 2_000_000);
    }

    /// The drive loop converts each published canvas (NV12→UYVY) and sends it to
    /// the NDI sender with the tick-derived timecode, ending when the
    /// end-of-program signal (the packet receiver) disconnects. No panic, no
    /// block.
    #[test]
    fn drive_loop_converts_and_sends_each_published_canvas() {
        let cadence = Rational { num: 25, den: 1 };
        let (publisher, reader) = frame_mailbox::<NdiCanvasFrame>();
        // The end-of-program signal: a closed packet channel.
        let (eop_tx, eop_rx) = std::sync::mpsc::sync_channel(4);

        // Publish three distinct canvases BEFORE the loop drains — the loop must
        // send the latest at each end-signal pulse, never block.
        let handle = std::thread::spawn(move || {
            let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "Multiview Test").unwrap();
            let outcome = run_ndi_output(&mut out, &reader, eop_rx, cadence, "ndi-test");
            (outcome, out.api().sent.clone())
        });

        for (tick, seed) in [(0u64, 10u8), (1, 20), (2, 30)] {
            publisher.publish(NdiCanvasFrame {
                canvas: Arc::new(ramp_canvas(seed)),
                tick_index: tick,
            });
            // One end-signal pulse per published frame.
            eop_tx.send(()).unwrap();
            // Give the drain a beat to observe the new sequence + pulse.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(eop_tx); // end-of-program

        let (outcome, sent) = handle.join().unwrap();
        // At least one frame was sent; every send is the canvas geometry (4x2)
        // converted to UYVY (the seam validated it) with a tick-derived timecode.
        assert!(!sent.is_empty(), "the drive loop must send the canvas");
        assert!(sent.iter().all(|s| s.0 == 4 && s.1 == 2));
        // Timecodes are tick-derived (multiples of one 25 fps frame = 400_000),
        // never raw input PTS.
        assert!(sent.iter().all(|s| s.3 % 400_000 == 0));
        assert!(outcome.frames >= 1);
    }

    /// A canvas tap is bounded drop-oldest (the mailbox is single-slot,
    /// newest-wins): publishing many frames before a single drain pulse sends
    /// only the LATEST — older frames are dropped, never queued (invariant #10).
    #[test]
    fn canvas_tap_is_bounded_drop_oldest() {
        let cadence = Rational { num: 25, den: 1 };
        let (publisher, reader) = frame_mailbox::<NdiCanvasFrame>();
        let (eop_tx, eop_rx) = std::sync::mpsc::sync_channel(1);

        // Publish FIVE canvases with no drain in between, then a SINGLE pulse:
        // the latest (tick 4) is the only one the loop can observe.
        for (tick, seed) in [(0u64, 1u8), (1, 2), (2, 3), (3, 4), (4, 5)] {
            publisher.publish(NdiCanvasFrame {
                canvas: Arc::new(ramp_canvas(seed)),
                tick_index: tick,
            });
        }
        let handle = std::thread::spawn(move || {
            let mut out = NdiOutput::new(accepted(), FakeNdiApi::new(), "Multiview Test").unwrap();
            run_ndi_output(&mut out, &reader, eop_rx, cadence, "ndi-test");
            out.api().sent.clone()
        });
        eop_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        drop(eop_tx);

        let sent = handle.join().unwrap();
        // Exactly the latest canvas was sent (tick 4 → timecode 4*400_000), the
        // four older ones were dropped at the single-slot tap (drop-oldest).
        assert_eq!(sent.len(), 1, "only the latest canvas survives the tap");
        assert_eq!(sent[0].3, 4 * 400_000);
    }
}

/// Drive a WHIP ingest source (ADR-T014): wait for a publisher, decode its
/// depacketized H.264/Opus into the last-good store, ride `NO_SIGNAL` between
/// publishers.
///
/// Multiview is the WHIP **server**, so there is nothing to dial. The supervised
/// loop samples the [`WhipRegistry`](crate::webrtc_ingest::WhipRegistry) for this
/// source's connected publisher (rendezvous'd by the `WhipProvider` on a `POST`);
/// when one appears it builds the pure `WebRtcProducer` over the publisher's
/// drop-oldest RTP ring and pumps it until the publisher goes (the ring ends) —
/// then loops back to waiting (the tile rides STALE → `NO_SIGNAL` via the store
/// policy; a `webrtc` source is never RECONNECTING — there is nothing to dial).
/// It only ever *writes* the lock-free store, so it can neither pace nor stall
/// the output clock (invariant #1) nor back-pressure the engine (invariant #10);
/// the publisher's RTP ring is bounded drop-oldest (invariant #2/#5).
#[cfg(feature = "webrtc-native")]
fn drive_webrtc(plan: &IngestPlan, _audio: bool, stop: &AtomicBool) {
    use multiview_input::webrtc::transport::WebRtcProducer;
    use multiview_webrtc::transport::RtpRingEngine;

    let Some(registry) = plan.webrtc_registry.clone() else {
        // No registry wired (should not happen for a webrtc plan) — degrade the
        // tile honestly rather than spin.
        tracing::warn!(source = %plan.id, "webrtc source has no publisher registry; tile degrades");
        return;
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Sample for a connected publisher. A quiet wait (no publisher) naps and
        // re-checks — it never extends the startup prime-wait nor spins.
        let Some(publisher) = registry.take(&plan.id) else {
            sleep_interruptible(Duration::from_millis(50), stop);
            continue;
        };
        tracing::info!(source = %plan.id, "webrtc publisher connected; ingesting");
        let negotiated = publisher.negotiated_session();
        let engine: Box<dyn multiview_input::webrtc::transport::MediaEngine + Send> =
            Box::new(RtpRingEngine::new(publisher.ring.clone()));
        let mut producer = WebRtcProducer::new(engine, &negotiated);
        drive_webrtc_producer(plan, &mut producer, &publisher.ring, stop);
        tracing::info!(source = %plan.id, "webrtc publisher disconnected; tile holds last-good → NO_SIGNAL");
    }
}

/// Pump a [`WebRtcProducer`] into the source's `TileStore` (video) and
/// `AudioStore` (Opus), until the publisher's ring ends or `stop` is raised.
///
/// Video access units feed a `multiview-ffmpeg` [`H264PacketDecoder`] →
/// NV12 (SPS geometry, VUI colour) → scaled to the tile → normalized PTS
/// (`PtsNormalizer`, `WrapBits::Rtp32`) → published into the last-good store.
/// Opus frames feed an `OpusDecoder` → 48 kHz stereo PCM → the ADR-T013
/// [`RtpAudioRebaser`] → `AudioStore::publish_at`. Every hand-off is sampled,
/// never pacing (inv #1/#10); a decode error on one unit is logged and the unit
/// dropped (the tile holds last-good — bad inputs are the product, inv #2).
#[cfg(feature = "webrtc-native")]
#[allow(clippy::too_many_lines)]
// reason: a straight-line per-event pump (lazy-build the H.264/Opus decoders,
// route each MediaEvent to its decode→publish helper) whose value is reading it
// top-to-bottom in one place, matching `ingest_loop`/`consumer_main`. Splitting
// it would scatter the session lifecycle across helpers without improving clarity.
fn drive_webrtc_producer(
    plan: &IngestPlan,
    producer: &mut multiview_input::webrtc::transport::WebRtcProducer,
    ring: &multiview_webrtc::transport::RtpRing,
    stop: &AtomicBool,
) {
    use multiview_input::webrtc::route::MediaEvent;
    use multiview_input::webrtc::transport::VIDEO_CLOCK_RATE;

    let tag = CanvasColor::default().output_tag();
    // The packet-fed H.264 decoder: geometry/colour come from the bitstream
    // (SPS/VUI), never declared — the ADR-T014 fix. Built lazily on the first
    // video unit so a video-less (audio-only) publisher allocates none.
    let video_tb = Rational::new(1, i64::from(VIDEO_CLOCK_RATE));
    let mut decoder: Option<multiview_ffmpeg::H264PacketDecoder> = None;
    let mut to_tile = TileScaler::new(plan.tile_w, plan.tile_h);
    // The video PTS normalizer (invariant #3): the 32-bit RTP wrap is unwrapped,
    // re-anchored on the depacketizer's discontinuity flag, monotonic-guarded.
    let mut normalizer = multiview_input::normalize::PtsNormalizer::new(
        multiview_input::normalize::WrapBits::Rtp32,
        video_tb,
        plan.cadence,
    );
    let start = Instant::now();

    // Audio: the Opus decoder + the shared ADR-T013 rebaser onto the store's
    // absolute frame index. Built lazily on the first audio unit; only when the
    // source carries an AudioStore (audio = true).
    let mut opus: Option<multiview_ffmpeg::OpusDecoder> = None;
    let mut rebaser = multiview_input::rtp_audio::RtpAudioRebaser::new(
        multiview_ffmpeg::OPUS_SAMPLE_RATE,
        multiview_ffmpeg::OPUS_SAMPLE_RATE,
    );

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Publisher gone (ring closed + drained): end this session.
        if ring.is_ended() {
            return;
        }
        let event = match producer.next_event() {
            Ok(Some(event)) => event,
            // Nothing ready this poll: nap briefly and re-check (never spin).
            Ok(None) => {
                sleep_interruptible(Duration::from_millis(5), stop);
                continue;
            }
            Err(err) => {
                tracing::warn!(source = %plan.id, error = %err, "webrtc producer faulted");
                return;
            }
        };
        match event {
            MediaEvent::VideoAccessUnit(unit) => {
                let dec = match decoder.as_mut() {
                    Some(d) => d,
                    None => match multiview_ffmpeg::H264PacketDecoder::new(video_tb) {
                        Ok(d) => decoder.insert(d),
                        Err(err) => {
                            tracing::warn!(source = %plan.id, error = %err, "h264 decoder open failed");
                            return;
                        }
                    },
                };
                if unit.discontinuity {
                    normalizer.mark_discontinuity();
                }
                // Feed the access unit; a structurally-corrupt AU is a recoverable
                // condition — log (rate-limited by the libav bridge) and ride
                // last-good (inv #2).
                if let Err(err) = dec.push(&unit.data, unit.raw_pts) {
                    tracing::debug!(source = %plan.id, error = %err, "h264 push dropped an access unit");
                    continue;
                }
                publish_webrtc_video(plan, dec, &mut to_tile, &mut normalizer, tag, start, stop);
            }
            MediaEvent::AudioFrame(unit) => {
                let Some(store) = plan.webrtc_audio_store.as_ref() else {
                    continue; // audio = false / no store: drop (answered inactive).
                };
                let dec = match opus.as_mut() {
                    Some(d) => d,
                    None => {
                        match multiview_ffmpeg::OpusDecoder::new(Rational::new(
                            1,
                            i64::from(multiview_ffmpeg::OPUS_SAMPLE_RATE),
                        )) {
                            Ok(d) => opus.insert(d),
                            Err(err) => {
                                tracing::warn!(source = %plan.id, error = %err, "opus decoder open failed");
                                continue;
                            }
                        }
                    }
                };
                publish_webrtc_audio(plan, &unit, dec, &mut rebaser, store);
            }
            // `MediaEvent` is `#[non_exhaustive]`: a future media kind we do not
            // decode is sampled away (never an error, never a stall — inv #1/#2).
            _ => {}
        }
    }
}

/// Drain the H.264 decoder, scaling + normalizing each decoded NV12 frame and
/// publishing it into the last-good store. Never paces the engine.
#[cfg(feature = "webrtc-native")]
fn publish_webrtc_video(
    plan: &IngestPlan,
    decoder: &mut multiview_ffmpeg::H264PacketDecoder,
    to_tile: &mut TileScaler,
    normalizer: &mut multiview_input::normalize::PtsNormalizer,
    tag: multiview_core::color::ColorInfo,
    start: Instant,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match decoder.receive_frame() {
            Ok(Some(picture)) => {
                let Ok(image) = to_tile.convert(&picture.frame, tag) else {
                    tracing::debug!(source = %plan.id, "webrtc tile scale dropped a frame");
                    continue;
                };
                // Normalize the verbatim 32-bit RTP PTS onto the unified ns
                // timeline (anchored to the source's ingest start). A frame with
                // no usable PTS falls back to the decoder's own rescaled time.
                let pts = timeline_pts(normalizer, picture.raw_pts, picture.meta.pts);
                // Stamp publish with the source-relative instant; the OUTPUT clock
                // paces emission, never this stamp (inputs are sampled, inv #1).
                let _ = start;
                plan.store.publish(image, pts);
            }
            Ok(None) => return, // decoder needs more input.
            Err(err) => {
                tracing::debug!(source = %plan.id, error = %err, "webrtc video decode error");
                return;
            }
        }
    }
}

/// Decode one Opus frame and publish the 48 kHz PCM into the source's
/// `AudioStore` at the rebased absolute frame index (ADR-T013). Never paces.
#[cfg(feature = "webrtc-native")]
fn publish_webrtc_audio(
    plan: &IngestPlan,
    unit: &multiview_input::webrtc::route::MediaUnit,
    decoder: &mut multiview_ffmpeg::OpusDecoder,
    rebaser: &mut multiview_input::rtp_audio::RtpAudioRebaser,
    store: &multiview_audio::store::AudioStore,
) {
    // The rebaser maps the packet's RTP timestamp to the store's absolute frame.
    // The Opus RTP clock has no SSRC surfaced at this seam, so a single logical
    // stream is assumed (a real SSRC change re-anchors via the discontinuity
    // flag the depacketizer raises).
    let raw_ts = u32::try_from(unit.raw_pts.unwrap_or(0) & i64::from(u32::MAX)).unwrap_or(0);
    let anchor = rebaser.rebase(raw_ts, 0, unit.discontinuity);
    if let Err(err) = decoder.push(&unit.data, unit.raw_pts) {
        tracing::debug!(source = %plan.id, error = %err, "opus push dropped a frame");
        return;
    }
    let mut frame = anchor.store_frame;
    loop {
        match decoder.receive_block() {
            Ok(Some(samples)) => {
                let Some(block) = webrtc_audio_block(&samples) else {
                    continue;
                };
                let frames = i64::try_from(block.frame_count()).unwrap_or(0);
                if let Err(err) = store.publish_at(frame, &block) {
                    tracing::debug!(source = %plan.id, error = %err, "webrtc audio publish rejected");
                    return;
                }
                frame = frame.saturating_add(frames);
            }
            Ok(None) => return,
            Err(err) => {
                tracing::debug!(source = %plan.id, error = %err, "webrtc audio decode error");
                return;
            }
        }
    }
}

/// Bridge a decoded 48 kHz stereo interleaved-`f32` Opus block into the
/// canonical [`AudioBlock`] the `AudioStore` consumes. Returns `None`
/// (panic-free) on a degenerate shape.
#[cfg(feature = "webrtc-native")]
fn webrtc_audio_block(
    samples: &multiview_ffmpeg::AudioSamplesF32,
) -> Option<multiview_audio::format::AudioBlock> {
    use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
    let format = AudioFormat::new(samples.rate, ChannelLayout::Stereo);
    AudioBlock::from_interleaved(format, samples.interleaved.clone()).ok()
}

/// The canonical AES67 audio store / program-bus sample rate (Hz): 48 kHz — the
/// rate [`crate::audio::new_store`] mints and the program bus mixes, so a rebased
/// AES67 block is published at exactly this rate (the store's format contract).
#[cfg(feature = "aes67")]
const AES67_STORE_RATE_HZ: u32 = 48_000;

/// The AES67 RX socket→packet channel-bridge depth (#103): a bounded drop-oldest
/// buffer of depacketized RTP units between the async receive loop (writer) and
/// the [`Aes67AudioProducer`] drain (reader). Bounded so a burst can never grow
/// memory (inv #5); AES67 Class-A packets are 1 ms, so 512 units is ~0.5 s of
/// slack — far more than the 2 ms drain cadence needs, and it drops-oldest under
/// any stall rather than back-pressuring the socket.
#[cfg(feature = "aes67")]
const AES67_RX_BRIDGE_CAP: usize = 512;

/// Receive one AES67 / ST 2110-30 multicast PCM source into its `AudioStore`
/// (#103, ADR-0033/T013), until `stop` is raised or the socket faults.
///
/// Binds a UDP receiver to the multicast port, joins the group, and pumps
/// depacketized audio units through the shared ADR-T013
/// [`RtpAudioRebaser`](multiview_input::rtp_audio::RtpAudioRebaser) — the SAME
/// seam the WebRTC-Opus path uses ([`publish_webrtc_audio`]), except AES67 passes
/// the **real packet SSRC** (not the hardcoded `0`), so an SSRC change re-anchors
/// the store's absolute-frame timeline. Each rebased block is published into the
/// source's last-good [`AudioStore`](multiview_audio::store::AudioStore), which the
/// program bus samples. Every hand-off is sampled, never pacing (inv #1/#10); a
/// malformed packet is skipped by the producer (bad inputs are the product, inv
/// #2), and a socket fault ends the session (the tile-less store then silence-fills
/// on the bus).
///
/// The receiver socket + its receive loop are async, but this runs on the ingest
/// thread (a `std::thread`, the control/IO plane), so it drives them on a small
/// **current-thread** Tokio runtime (like [`resolve_youtube_master`]) — the output
/// data plane is never involved. A `select!` drives the receive loop concurrently
/// with a 2 ms drain poll on the one thread, so the socket makes progress without a
/// dedicated runtime worker.
#[cfg(feature = "aes67")]
fn drive_aes67_rx(plan: &Aes67RxPlan, stop: &AtomicBool) {
    use multiview_input::st2110::transport::{MulticastInterface, RtpReceiver};
    use multiview_input::st2110::Aes67AudioProducer;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(source = %plan.id, error = %e, "aes67 rx runtime build failed; source silent");
            return;
        }
    };

    // Receiving multicast binds the port on the WILDCARD of the group's address
    // family (IPv6 `[::]` / IPv4 `0.0.0.0`) and then joins the group — never the
    // group literal (a portable multicast receive).
    let port = plan.group.port();
    let local: std::net::SocketAddr = if plan.group.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, port).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, port).into()
    };

    runtime.block_on(async {
        let receiver = match RtpReceiver::bind(local).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(source = %plan.id, error = %e, local = %local, "aes67 rx bind failed; source silent");
                return;
            }
        };
        if let Err(e) = receiver.join_multicast(plan.group.ip(), MulticastInterface::Unspecified) {
            tracing::warn!(source = %plan.id, error = %e, group = %plan.group, "aes67 rx multicast join failed; source silent");
            return;
        }
        tracing::info!(source = %plan.id, group = %plan.group, "aes67 rx receiving");
        let (packet_source, receive_loop) = receiver.channel_bridge(AES67_RX_BRIDGE_CAP);
        let mut receive_loop = receive_loop;
        let mut producer = Aes67AudioProducer::new(
            Box::new(packet_source),
            plan.session.format,
            plan.session.payload_type,
        );
        // The ADR-T013 rebaser maps each packet's 32-bit RTP media timestamp onto
        // the store's absolute 48 kHz frame index. Wire rate is the SDP clock; the
        // store is canonical 48 kHz.
        let mut rebaser = multiview_input::rtp_audio::RtpAudioRebaser::new(
            plan.session.clock_rate,
            AES67_STORE_RATE_HZ,
        );
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                // The receive loop drives the socket → packet channel. It resolves
                // only on a socket fault or when the packet source is dropped —
                // end the session (the store then silence-fills on the bus).
                () = &mut receive_loop => {
                    tracing::info!(source = %plan.id, "aes67 rx socket ended; source holds silence");
                    return;
                }
                // Drain every unit ready this poll and publish it. A 2 ms nap
                // yields to the runtime so `receive_loop` makes progress (never
                // spins); AES67 Class-A packets are 1 ms, so this keeps pace.
                () = tokio::time::sleep(Duration::from_millis(2)) => {
                    if !drain_aes67_producer(plan, &mut producer, &mut rebaser) {
                        return; // producer fault: end the session.
                    }
                }
            }
        }
    });
}

/// Drain every AES67 audio unit ready this poll, rebasing + publishing each into
/// the source's store. Returns `false` on a producer fault (end the session),
/// `true` to keep receiving. Never paces the engine (inv #1/#10); a rejected
/// publish or degenerate block is logged/skipped, never fatal (inv #2).
#[cfg(feature = "aes67")]
fn drain_aes67_producer(
    plan: &Aes67RxPlan,
    producer: &mut multiview_input::st2110::Aes67AudioProducer,
    rebaser: &mut multiview_input::rtp_audio::RtpAudioRebaser,
) -> bool {
    loop {
        match producer.next_audio() {
            Ok(Some(frame)) => {
                // AES67 passes the REAL SSRC (not the WebRTC path's hardcoded 0),
                // so a mid-stream SSRC change re-anchors the store timeline.
                let anchor = rebaser.rebase(frame.raw_timestamp, frame.ssrc, frame.discontinuity);
                let Some(block) = aes67_audio_block(&frame) else {
                    continue; // degenerate shape: skip (bad inputs are the product).
                };
                if let Err(e) = plan.store.publish_at(anchor.store_frame, &block) {
                    tracing::debug!(source = %plan.id, error = %e, "aes67 audio publish rejected");
                }
            }
            Ok(None) => return true, // nothing ready this poll.
            Err(e) => {
                tracing::warn!(source = %plan.id, error = %e, "aes67 producer faulted");
                return false;
            }
        }
    }
}

/// Bridge a depacketized [`Aes67AudioFrame`](multiview_input::st2110::Aes67AudioFrame)
/// (interleaved canonical f32, frame-major) into the [`AudioBlock`] the
/// `AudioStore` consumes, at the store's canonical 48 kHz **stereo**. The per-source
/// store + program bus are stereo, so the block must be stereo whatever the source
/// channel count: mono is duplicated L=R, a >2-channel stream keeps its first two
/// channels (a full downmix is a later slice). Returns `None` (panic-free) on a
/// degenerate shape.
#[cfg(feature = "aes67")]
fn aes67_audio_block(
    frame: &multiview_input::st2110::Aes67AudioFrame,
) -> Option<multiview_audio::format::AudioBlock> {
    use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
    let channels = usize::from(frame.format.channels).max(1);
    let stereo = interleave_to_stereo(&frame.samples, channels);
    let format = AudioFormat::new(AES67_STORE_RATE_HZ, ChannelLayout::Stereo);
    AudioBlock::from_interleaved(format, stereo).ok()
}

/// Convert an interleaved, frame-major f32 buffer at `channels` into interleaved
/// STEREO (`L,R,…`): stereo passes through, mono duplicates each sample to both
/// channels, and >2 channels keep the first two. `channels` is clamped to ≥1 so
/// the frame stride is never zero.
#[cfg(feature = "aes67")]
fn interleave_to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    if channels == 2 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let mut out = Vec::with_capacity(frames.saturating_mul(2));
    for f in 0..frames {
        let base = f.saturating_mul(channels);
        let left = samples.get(base).copied().unwrap_or(0.0);
        let right = if channels == 1 {
            left
        } else {
            samples.get(base.saturating_add(1)).copied().unwrap_or(left)
        };
        out.push(left);
        out.push(right);
    }
    out
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

/// Pick the `*.googlevideo.com` HLS master URL to open for a `YouTube` source on a
/// (re)connect, **preferring the proactive re-resolution slot** (IN-5b) over an
/// inline `yt-dlp` resolve.
///
/// The proactive [`run_reresolve_loop`](multiview_input::youtube::reresolve::run_reresolve_loop)
/// bridge thread publishes a freshly resolved master into `slot` `lead` seconds
/// before the active URL's `expire` deadline. So when the active manifest ages out
/// and 403s, the reconnect bracket re-enters and finds the **next-up** URL already
/// in hand — the tile reopens onto it without waiting on a cold synchronous resolve
/// at the boundary (make-before-break: the new URL was resolved ahead of need).
///
/// `slot` is `None` only when the `youtube` plan carries no slot (it always does);
/// an **empty** slot (cold start before the loop's first publish, or no loop) falls
/// back to a synchronous inline resolve via `resolve_inline`, so the very first
/// open never waits on the loop and a source with no running loop still works.
///
/// This is a pure selection over an injected `resolve_inline` closure so it is
/// unit-testable with no network/subprocess (the live resolve is gated separately).
#[cfg(feature = "youtube")]
fn youtube_open_url<F>(
    slot: Option<&arc_swap::ArcSwapOption<String>>,
    resolve_inline: F,
) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String>,
{
    if let Some(fresh) = slot.and_then(arc_swap::ArcSwapOption::load_full) {
        // The proactive loop has a fresh, make-before-break-resolved URL in hand.
        return Ok((*fresh).clone());
    }
    // Cold start (slot empty) or no loop: resolve inline this once.
    resolve_inline()
}

/// Run the supervised **proactive** `YouTube` re-resolution loop on this thread
/// (IN-5b): the async↔sync bridge that keeps a long-running tile's
/// `*.googlevideo.com` HLS URL fresh ahead of its `expire` deadline.
///
/// This is the CLI seam onto
/// [`run_reresolve_loop`](multiview_input::youtube::reresolve::run_reresolve_loop):
/// it builds a small current-thread Tokio runtime (the loop's only `await`s are its
/// own hard-timeout-bounded resolves + an interruptible sleep) and drives the loop,
/// publishing each freshly resolved master into the lock-free `slot` via a swap
/// closure. [`open_and_stream`] reads that slot on every (re)open
/// ([`youtube_open_url`]), so the active manifest is replaced make-before-break.
///
/// It runs on its **own supervised `std::thread`** (a sibling of the decode thread,
/// off the data plane), carries its own `stop` flag, and only ever *writes* the
/// lock-free slot — so it can neither pace nor stall the output clock (invariant
/// #1) nor back-pressure the engine (invariant #10). A hung `yt-dlp` is killed by
/// the resolver's hard timeout, never awaited (invariant #10); an extraction
/// failure degrades the tile and backs off inside the loop, never panicking. The
/// thread returns when `stop` is observed.
#[cfg(feature = "youtube")]
fn youtube_reresolve_thread(
    watch_url: &str,
    slot: &Arc<arc_swap::ArcSwapOption<String>>,
    stop: &AtomicBool,
) {
    use multiview_input::youtube::reresolve::{
        run_reresolve_loop, ProcessResolver, ReresolveConfig, SystemUnixClock,
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // No runtime ⇒ no proactive refresh, but the source still works: the
            // decode thread's inline resolve-on-reconnect (the IN-5 fallback) keeps
            // the tile alive across expiry (it briefly degrades at the boundary).
            // Never panics, never fails the run (invariant #1).
            tracing::warn!(
                %watch_url,
                error = %e,
                "youtube re-resolution runtime unavailable; falling back to resolve-on-reconnect"
            );
            return;
        }
    };

    let resolver = ProcessResolver::default();
    let clock = SystemUnixClock;
    let swap_slot = Arc::clone(slot);
    let outcome = runtime.block_on(run_reresolve_loop(
        watch_url,
        ReresolveConfig::default(),
        &resolver,
        &clock,
        stop,
        move |url: String| {
            // Lock-free publish of the next-up master; the decode thread reads it on
            // its next (re)open. Cheap + non-blocking — the loop holds no lock.
            swap_slot.store(Some(Arc::new(url)));
        },
    ));
    if let Err(e) = outcome {
        // The loop never returns `Err` for an extraction failure (those degrade the
        // tile + back off inside it); a returned error is a fatal-config fault.
        // Log it — the tile rides the decode thread's inline resolve fallback.
        tracing::warn!(%watch_url, error = %e, "youtube re-resolution loop ended with error");
    }
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
#[allow(clippy::too_many_lines)]
// reason: the cohesive run-path open-and-stream routine (open + variant-pin +
// decoder build + caption-route build + the packet/frame pump) read top-to-bottom
// in one place; the feature-gated guarded-unreachable arms (synthetic/ndi/webrtc
// never reach here) tip the count without changing the linear flow.
fn open_and_stream(
    plan: &IngestPlan,
    tag: multiview_core::color::ColorInfo,
    stop: &AtomicBool,
) -> Result<(), String> {
    multiview_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    // A YouTube source picks its `*.googlevideo.com` HLS master here, on every
    // (re)connect (ADR-0015), PREFERRING the proactive re-resolution slot (IN-5b):
    // the supervised `youtube_reresolve_thread` publishes a freshly resolved master
    // into `plan.youtube_url_slot` `lead` seconds AHEAD of the active URL's `expire`
    // deadline (make-before-break), so when the active manifest 403s and the
    // reconnect bracket in `ingest_loop` re-enters, the next-up URL is already in
    // hand — the tile reopens onto it without waiting on a cold synchronous resolve.
    // When the slot is empty (cold start before the loop's first publish, or no
    // loop) it falls back to an inline `yt-dlp` resolve, on THIS ingest thread (the
    // control/IO plane) under a hard timeout (a hung `yt-dlp` is killed, never
    // awaited); it never touches the output data plane (invariants #1/#10). A
    // resolve failure returns `Err`, which the reconnect bracket backs off and
    // retries while the tile rides last-good → NO_SIGNAL.
    #[cfg(feature = "youtube")]
    let resolved_youtube_url = match &plan.location {
        SourceLocation::Youtube { watch_url } => {
            Some(youtube_open_url(plan.youtube_url_slot.as_deref(), || {
                resolve_youtube_master(watch_url)
            })?)
        }
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
        // Unreachable: `ingest_loop` routes WHIP sources to `drive_webrtc` (str0m
        // surfaces RTP; the drive loop decodes per-packet) before reaching
        // `open_and_stream`. Guarded so the match stays exhaustive.
        #[cfg(feature = "webrtc-native")]
        SourceLocation::Webrtc { .. } => {
            return Err("webrtc source has no libav media to open".to_owned())
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
    // The placement-aware decode-open gate (ADR-W018 §7): a `Pinned` plan
    // opens NVDEC on the admission-chosen island device (affinity; ADR-0035
    // Tier-1); a `Default` plan keeps the env-gated hardware preference on
    // libav's default device (the GPU-free / no-admission path); a
    // `SoftwareOnly` plan (a live placement REJECT) never attempts hardware —
    // NVDEC on the default device would overcommit a single-GPU island or
    // fragment onto a different GPU (ADR-0018 never-fragment). On a GPU-free box
    // or any hardware-open failure the open degrades to software gracefully —
    // the tile keeps running (invariants #1/#2).
    let nvdec_env = std::env::var(multiview_ffmpeg::NVDEC_DISABLE_ENV).ok();
    let (want_hw, cuda_ordinal) = decoder_open_args(&plan.decode_placement, nvdec_env.as_deref());
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

    // MEDIA-PLAYER CHANNEL (ADR-0057 / ADR-0097): when this source is a player,
    // run the transport-driven decode loop instead of the plain pump. It owns
    // its OWN monotone output-anchored timeline (so it bypasses `normalizer`
    // entirely — see `stream_player`), performs the in-place loop seek +
    // decoder flush at a wrap, and drains the transport mailbox between frames.
    if plan.player.is_some() {
        return stream_player(
            plan,
            tag,
            &mut input,
            &mut decoder,
            &mut to_tile,
            stream_index,
            &mut pacer,
            stop,
        );
    }

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

/// Convert a source frame index to a libav `AV_TIME_BASE` (microsecond) seek
/// target at the given `cadence`, using exact integer rationals — never float
/// fps (invariant #3). `target_us = frame * 1_000_000 * cadence.den /
/// cadence.num`, computed in `i128` to avoid overflow, saturated into `i64`.
fn frame_to_seek_us(frame: u64, cadence: Rational) -> i64 {
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den);
    if num <= 0 {
        return 0;
    }
    let us = i128::from(frame)
        .saturating_mul(1_000_000)
        .saturating_mul(den)
        / num;
    i64::try_from(us).unwrap_or(i64::MAX)
}

/// The media-player fail-safe: after this many consecutive open→decode→EOF laps
/// that publish ZERO frames, the asset cannot be played at its declared geometry
/// (truncated / zero-frame / corrupt, or an in-point/target past the real clip
/// end) — `stream_player` gives up re-seeking and holds last-good, so the ingest
/// thread can never spin seek→EOF→seek (rule 26 / inv #1).
const MAX_UNPRODUCTIVE_LAPS: u32 = 3;

/// The **media-player** decode loop (ADR-0057 / ADR-0097): drives a
/// pre-declared player channel's transport over the open container.
///
/// Unlike the plain pump in [`open_and_stream`], this loop consults the
/// thread-local [`MediaPlayer`](crate::player::MediaPlayer) for every decoded
/// frame and performs the [`PlayerAction`](crate::player::PlayerAction) it
/// returns:
///
/// - [`Publish`](crate::player::PlayerAction::Publish): pace to the player's
///   **output-anchored** stamp (`anchor + emitted × frame_period`, monotone
///   across loop laps) and publish — the stamp comes straight from the player's
///   own synthesized timeline, so **no source PTS is routed through
///   `PtsNormalizer`** and the wrapped-PTS monotonic-clamp hazard cannot arise
///   (ADR-0097 §5 refinement). Invariant #4 pacing is preserved (the stamp is
///   monotone media time the `PtsWallClock` paces on directly).
/// - [`SeekFlushTo`](crate::player::PlayerAction::SeekFlushTo): the in-place
///   loop/vamp wrap — seek the container to the in-point, **flush the decoder**
///   (the non-negotiable ADR-0097 rule 2: drop stale reordered B-frames), reset
///   the source-frame cursor, and keep decoding. The boundary frame is
///   discarded, never published.
/// - [`Hold`](crate::player::PlayerAction::Hold): republish the held last-good
///   frame at an advancing stamp (paused / EOF-held) so the tile reads LIVE.
/// - [`Ended`](crate::player::PlayerAction::Ended): the channel ended under a
///   non-looping policy — heartbeat-republish the terminal frame so the tile
///   holds LIVE on the freshness ladder.
///
/// The transport [`mailbox`](crate::player::TransportMailbox) is drained once
/// per outer iteration (between frames) and applied to the player. The loop
/// never blocks on a client and never paces the output clock (invariants
/// #1/#10): it only ever writes the lock-free store, exactly like every other
/// ingest path.
#[allow(clippy::too_many_arguments)]
// mirrors open_and_stream's decode-context threading
// One cohesive decode loop: drain the mailbox, then per decoded frame run the
// frame-accurate-seek alignment + the PlayerAction (publish/wrap/hold/end), then
// feed the next packet. Splitting the per-action arms into helpers would thread
// most of these locals (input/decoder/pacer/store/pending_target/last_image)
// through each and obscure the single-pass control flow; kept whole.
#[allow(clippy::too_many_lines)]
fn stream_player(
    plan: &IngestPlan,
    tag: multiview_core::color::ColorInfo,
    input: &mut ffmpeg::format::context::Input,
    decoder: &mut StreamVideoDecoder,
    to_tile: &mut TileScaler,
    stream_index: usize,
    pacer: &mut PtsWallClock,
    stop: &AtomicBool,
) -> Result<(), String> {
    use crate::player::{PlayerAction, TransportVerb};

    // The handle is present (the caller only enters here when `plan.player` is
    // `Some`); a `let-else` keeps it panic-free.
    let Some(handle) = plan.player.as_ref() else {
        return Ok(());
    };
    let cadence = handle.geometry.cadence();
    let mut player = handle.build_player();
    // Begin playout: vamp (loop the vamp segment) when the channel loops on
    // start, else a single forward play-through. Anchored at the zero output
    // media time; the pacer maps that to wall-clock on the first publish. A
    // vamping channel starts at the vamp-in (it loops the [vamp_in, vamp_out)
    // window); a plain play starts at the clip in-point.
    let start_frame = if handle.loop_on_start {
        player.vamp(multiview_core::time::MediaTime::ZERO);
        handle.geometry.vamp_in()
    } else {
        player.play(multiview_core::time::MediaTime::ZERO);
        handle.geometry.in_point()
    };

    // The source frame index of the NEXT frame the decoder will yield. Starts at
    // the start frame and is reset by a wrap seek; advances 1:1 with decoded
    // frames otherwise.
    let mut source_frame = start_frame;
    // ADR-T019 §1/§2 (single-authority transport coupling): the video rail is the
    // SOLE mailbox consumer and PUBLISHES its applied transport state to the
    // wait-free `control_bus` the audio rail samples. `last_published_at` tracks the
    // video's most recent output media-time; `armed_anchor` LATCHES that time at the
    // edge the exit is armed (stable while armed — the audio arms at the SAME
    // boundary, with no per-frame churn — MAJOR-5); `last_audio_control` is the last
    // `(AudioTransport, exit_armed, anchor)` published, so the bus is bumped only on
    // a real change (including a CHANGED anchor — a re-arm).
    let mut last_published_at = multiview_core::time::MediaTime::ZERO;
    let mut armed_anchor: Option<multiview_core::time::MediaTime> = None;
    let mut last_audio_control: Option<(
        crate::player::AudioTransport,
        bool,
        Option<multiview_core::time::MediaTime>,
    )> = None;
    // A pending **frame-accurate seek target**: when `Some(t)`, the decoder has
    // just been seeked to a keyframe at-or-before `t` (libav seeks land on a
    // keyframe ≤ target), so the loop **decodes-and-discards** every frame whose
    // index is `< t` and only publishes once the decoded frame's index reaches
    // `t` — so the first published frame after a seek/loop-wrap is EXACTLY the
    // in-point, even for a non-keyframe in-point (frame-accurate vamp/exit, no
    // pre-target frame at the seam). `None` ⇒ frames advance 1:1.
    let mut pending_target: Option<u64> = Some(start_frame);
    let frame_period_ns = handle.geometry.frame_period_ns();
    // The asset's PTS origin: the canonical-ns PTS of the FIRST decoded frame
    // (frame 0). A container (e.g. MPEG-TS) commonly starts PTS at a non-zero
    // offset, so a source frame index is `(pts − first_pts) / frame_period`, not
    // the absolute PTS — captured once, then frame indices are 0-based and the
    // frame-accurate seek-to-target math is offset-independent.
    let mut first_pts_ns: Option<i64> = None;
    // The last-good frame **handle** (an `Arc`, shared with the store) for the
    // held/ended heartbeat republish: republishing is an `Arc::clone` (a refcount
    // bump), never a pixel-plane copy — no per-frame heap allocation on the hold
    // path (bounded-memory data plane, inv #5 / rule 22).
    let mut last_image: Option<Arc<Nv12Image>> = None;
    // Whether the decoder has been fully drained (EOF reached with no loop, or a
    // fail-safe gave up): the loop falls through to `heartbeat_player_hold`, which
    // holds last-good forever — the tile rides the framestore state machine and
    // the output clock keeps ticking undisturbed (inv #1).
    let mut drained = false;
    // FAIL-SAFE bounds against a bad/truncated/corrupt asset spinning the ingest
    // thread (rule 26 — bad inputs are the purpose; inv #1 — never stall output):
    // `discarded` counts frames dropped since the last seek while aligning to a
    // frame-accurate target; `unproductive_laps` counts consecutive open→decode→
    // EOF cycles that published ZERO frames. Either exceeding its bound is a clear
    // "this asset cannot satisfy this geometry" signal → stop seeking and HOLD
    // last-good, never busy-re-arm seek→EOF→seek.
    let mut discarded: u64 = 0;
    let mut produced_this_lap: u64 = 0;
    let mut unproductive_laps: u32 = 0;
    // `true` once demux EOF has flushed the decoder this lap: the next decode
    // pass drains the decoder's DELAYED (reordered / B-frame) frames — which
    // libav only emits after the EOF flush — before the lap is judged
    // productive/unproductive. (A good B-frame clip's publishable frames can be
    // buffered until this flush; judging at demux EOF alone would wrongly call
    // such a lap empty.)
    let mut eof_flushing = false;
    // The discard budget: a generous multiple of the declared clip length plus a
    // floor, so a legitimate keyframe-bracketed seek (discarding back to the prior
    // keyframe) always succeeds, but an unreachable target (past clip end) or a
    // PTS/index mismatch cannot discard forever.
    let max_discard = handle
        .geometry
        .out_point()
        .saturating_mul(2)
        .saturating_add(256);

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }

        // Drain the transport mailbox and apply each verb to the player. This is
        // the only control-plane seam; O(pending), it takes the mailbox's short
        // uncontended mutex for a `mem::take`, and never awaits or back-pressures
        // the output clock (inv #1/#10).
        for verb in handle.mailbox.drain() {
            match verb {
                TransportVerb::Play => player.play(multiview_core::time::MediaTime::ZERO),
                TransportVerb::Vamp => player.vamp(multiview_core::time::MediaTime::ZERO),
                TransportVerb::Pause => player.pause(),
                TransportVerb::Stop => player.stop(),
                TransportVerb::ArmExit => player.arm_exit(),
                TransportVerb::TakeExit => player.take_exit(),
                TransportVerb::CancelExit => player.cancel_exit(),
                // load/cue/seek carry a target the executor honours by seeking;
                // the MVP player plays one bound asset, so a Seek re-targets the
                // cursor and a Cue/Load re-cues to the in-point. A failed seek is
                // logged and skipped (the tile holds last-good — inv #1/#2).
                TransportVerb::Seek { frame } => {
                    apply_player_seek(input, decoder, frame, cadence, &mut pending_target);
                }
                TransportVerb::Cue { frame } => {
                    let target = frame.unwrap_or_else(|| handle.geometry.in_point());
                    apply_player_seek(input, decoder, target, cadence, &mut pending_target);
                }
                TransportVerb::Load { .. } => {
                    let in_point = handle.geometry.in_point();
                    apply_player_seek(input, decoder, in_point, cadence, &mut pending_target);
                }
            }
        }

        // ADR-T019 §1/§2: after applying the drained verbs, publish the player's
        // authoritative transport state to the audio rail's control bus — but only
        // when it actually changed (the bus is wait-free and cheap, but bumping the
        // generation needlessly would make the audio re-apply each block). LATCH the
        // exit anchor at the edge the exit is armed (the video's then-current output
        // media-time), so it is stable while armed (no per-frame churn) yet a re-arm
        // at a different boundary updates it and re-reaches the deck (MAJOR-5).
        armed_anchor = if player.exit_armed() {
            // Latch on the false→true edge; hold the latched value while it stays armed.
            Some(armed_anchor.unwrap_or(last_published_at))
        } else {
            None
        };
        publish_audio_control(
            handle,
            player.state(),
            armed_anchor.unwrap_or(last_published_at),
            &mut last_audio_control,
        );

        // Pull every frame the decoder currently has, deciding per frame.
        while let Some(decoded) = decoder.receive_frame().map_err(|e| e.to_string())? {
            // Establish the asset's PTS origin from the very first decoded frame
            // (a container may start PTS at a non-zero offset).
            let origin = *first_pts_ns.get_or_insert_with(|| decoded.meta.pts.as_nanos());
            // Frame-accurate seek: after a seek/loop-wrap, libav landed on a
            // keyframe at-or-before the requested in-point, so decode-and-discard
            // every frame whose index is before the target; only once a decoded
            // frame reaches the target do we align and start publishing — so no
            // pre-target frame is ever shown at the seam.
            if let Some(target) = pending_target {
                let idx = frame_index_of(&decoded, origin, frame_period_ns);
                if idx < target {
                    // FAIL-SAFE: bound the decode-discard so an unreachable target
                    // (past clip end) or a PTS/index mismatch cannot discard
                    // forever. Give up → drain + hold last-good (inv #1/#2).
                    discarded = discarded.saturating_add(1);
                    if discarded > max_discard {
                        tracing::warn!(
                            player = %plan.id, target, discarded,
                            "media player: seek target unreachable (discard budget exhausted) — \
                             holding last-good"
                        );
                        drained = true;
                        break;
                    }
                    continue;
                }
                // Target reached: aligned. Reset the discard budget for the next
                // seek.
                pending_target = None;
                source_frame = idx;
                discarded = 0;
            }
            match player.on_decoded(source_frame) {
                PlayerAction::Publish { at } => {
                    let image = to_tile.convert(&decoded.frame, tag)?;
                    // Pace to the player's own monotone stamp (invariant #4),
                    // then publish it (latch-on-tick, invariant #1). Re-check
                    // `stop` after the (possibly long) pace wait. Wrap the frame
                    // in an `Arc` ONCE and publish that handle; the held last-good
                    // is the same `Arc` (a refcount bump, not a pixel copy).
                    pacer.wait_for(at, stop);
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let image = Arc::new(image);
                    last_image = Some(Arc::clone(&image));
                    plan.store.publish_arc(image, at);
                    // Track the video's output media-time for the audio exit anchor
                    // (ADR-T019 §1): the audio arms its exit at the next vamp
                    // boundary at-or-after this same media-time.
                    last_published_at = at;
                    source_frame = source_frame.saturating_add(1);
                    produced_this_lap = produced_this_lap.saturating_add(1);
                }
                PlayerAction::SeekFlushTo { frame } => {
                    // The in-place loop/vamp wrap: discard this boundary frame,
                    // seek to the in-point, flush the decoder (ADR-0097 rule 2),
                    // and keep decoding (the decode-discard above lands the first
                    // published frame exactly on the in-point). A wrap that
                    // published frames is a healthy lap → reset the unproductive
                    // counter; break to re-read packets from the new position.
                    apply_player_seek(input, decoder, frame, cadence, &mut pending_target);
                    discarded = 0;
                    unproductive_laps = 0;
                    produced_this_lap = 0;
                    break;
                }
                PlayerAction::Hold { at } => {
                    // The decoded frame is not shown (paused / EOF-held): hold
                    // the last-good handle, republished at the advancing stamp so
                    // the tile reads LIVE. Republish is an `Arc::clone` (refcount
                    // bump) — no pixel-plane copy. With no prior frame, drop it.
                    if let Some(image) = &last_image {
                        pacer.wait_for(at, stop);
                        if stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        plan.store.publish_arc(Arc::clone(image), at);
                    }
                }
                PlayerAction::Ended => {
                    // `auto_off`: the channel reports Ended and releases — the
                    // switcher applies the bus/keyer consequence (ADR-0057 §4).
                    // The tile holds its last-good frame forever (HoldForever).
                    return Ok(());
                }
            }
        }

        // The decoder has been flushed at demux EOF and its DELAYED frames have
        // now been drained by the `while` above — so the lap can be judged
        // productive/unproductive on the COMPLETE frame set (B-frame / reordered
        // frames buffered until the flush are counted). Decide the loop here.
        if eof_flushing {
            eof_flushing = false;
            if player.is_playing_state() {
                // FAIL-SAFE: a full open→decode→EOF-flush lap that published ZERO
                // frames is an unplayable asset for this geometry (truncated /
                // zero-frame / corrupt, or an in-point/target past the real clip
                // end). Re-seeking forever would spin the ingest thread (rule 26 /
                // inv #1). After `MAX_UNPRODUCTIVE_LAPS` such laps, give up → hold
                // last-good, never busy-re-arm seek→EOF→seek. A lap that published
                // ANY frame (during decode OR the flush) is productive.
                if produced_this_lap == 0 {
                    unproductive_laps = unproductive_laps.saturating_add(1);
                    if unproductive_laps >= MAX_UNPRODUCTIVE_LAPS {
                        tracing::warn!(
                            player = %plan.id, laps = unproductive_laps,
                            "media player: asset produced no frames for this geometry — \
                             holding last-good (no further re-seek)"
                        );
                        drained = true;
                    }
                } else {
                    unproductive_laps = 0;
                }
                if !drained {
                    // Re-arm the next loop from the in-point (this re-seek flushes
                    // the decoder again, clearing the EOF state).
                    produced_this_lap = 0;
                    let in_point = handle.geometry.in_point();
                    apply_player_seek(input, decoder, in_point, cadence, &mut pending_target);
                }
            } else {
                // Not looping: the play-through ended — drain and hold.
                drained = true;
            }
        }

        if drained {
            // The decoder is fully drained and the player is not looping (a wrap
            // would have re-seeded packets). Hold the tile LIVE: a finite,
            // non-looping play-through keeps republishing its terminal frame at
            // cadence via the heartbeat below until `stop`.
            heartbeat_player_hold(plan, &mut player, last_image.as_ref(), pacer, stop);
            return Ok(());
        }

        // While flushing the decoder's delayed frames after demux EOF, do not
        // read more packets — loop back to drain the decoder first.
        if eof_flushing {
            continue;
        }

        // Feed the decoder the next packet(s).
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(input) {
            Ok(()) => {
                if packet.stream() == stream_index {
                    decoder.send_packet(&packet).map_err(|e| e.to_string())?;
                }
                // Non-video packets are ignored on the player path (a player
                // channel renders video; its audio rides the per-source audio
                // thread, ADR-0057 Decision 6).
            }
            Err(ffmpeg::Error::Eof) => {
                // Demux EOF: flush the decoder so its DELAYED (reordered /
                // B-frame) frames emit, then loop back to DRAIN + publish them
                // before judging this lap (the productivity decision happens above
                // once `eof_flushing` frames are drained). This is the same
                // EOF-flush behaviour the decoder-flush tests document.
                decoder.send_eof().map_err(|e| e.to_string())?;
                eof_flushing = true;
            }
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// Publish the video player's authoritative transport state to the audio rail's
/// wait-free control bus (ADR-T019 §1), but only when it changed since the last
/// publish (so the audio re-applies on a real transition, not every frame). Maps
/// the video [`MediaPlayerState`] onto the audio-rail subset
/// ([`AudioTransport`](crate::player::AudioTransport)): publishing/vamping →
/// `Vamping` (carrying the exit anchor when an exit is armed); paused/held/cued/
/// loading → `Paused`; stopped (re-cued) → `Stopped`; the EOF terminals settle to
/// `Paused` (the audio bus contributes silence while the video tile holds its
/// terminal frame). The exit anchor is the video's most recent output media-time,
/// so the audio arms at the SAME next-vamp boundary the video reaches.
fn publish_audio_control(
    handle: &crate::player::PlayerHandle,
    state: crate::player::MediaPlayerState,
    armed_anchor: multiview_core::time::MediaTime,
    last: &mut Option<(
        crate::player::AudioTransport,
        bool,
        Option<multiview_core::time::MediaTime>,
    )>,
) {
    use crate::player::{AudioTransport, EofPolicy, MediaPlayerState};
    use multiview_core::time::MediaTime;
    // (audio_state, exit_armed, exit_anchor). For a forever-loop the audio vamps
    // with no exit; for an armed vamp exit the audio arms at the video's LATCHED
    // arm anchor (`armed_anchor`, stable while armed — the SAME next boundary); for a
    // one-shot `Playing` (non-`Loop` policy) the audio arms at ZERO so it settles to
    // silence after the first lap (mirroring the video playing `[in,out)` once then
    // holding its terminal frame). The video's re-cue state (`Cued`, where
    // `MediaPlayer::stop()` lands) maps to `Stopped` (an audio RE-CUE), distinct from
    // a genuine `Paused` (hold-in-place) — MAJOR-4.
    let loops_forever = matches!(handle.eof_policy, EofPolicy::Loop);
    let (audio_state, exit_armed, anchor) = match state {
        MediaPlayerState::Playing if loops_forever => (AudioTransport::Vamping, false, None),
        MediaPlayerState::Playing => (AudioTransport::Vamping, true, Some(MediaTime::ZERO)),
        MediaPlayerState::Vamping { exit_armed: true } => {
            (AudioTransport::Vamping, true, Some(armed_anchor))
        }
        MediaPlayerState::Vamping { exit_armed: false } => (AudioTransport::Vamping, false, None),
        // The video re-cue (Cued) → an audio re-cue (Stopped), not pause.
        MediaPlayerState::Cued => (AudioTransport::Stopped, false, None),
        MediaPlayerState::Paused
        | MediaPlayerState::Holding
        | MediaPlayerState::Black
        | MediaPlayerState::Idle
        | MediaPlayerState::Loading
        | MediaPlayerState::Ended => (AudioTransport::Paused, false, None),
    };
    // Suppress on the FULL (state, exit_armed, anchor) triple so a CHANGED arm anchor
    // (a re-arm / move-exit at a different boundary) always reaches the deck even
    // when (state, exit_armed) is unchanged (MAJOR-5) — while an unchanged control is
    // still suppressed (no per-frame generation churn, because `armed_anchor` is
    // latched at the arm edge by the caller, not re-sampled every frame).
    let key = (audio_state, exit_armed, anchor);
    if *last == Some(key) {
        return;
    }
    *last = Some(key);
    handle.control_bus.publish(audio_state, anchor);
}

#[cfg(test)]
mod publish_audio_control_tests {
    //! MAJOR-5 (ADR-T019 §2): a CHANGED exit-arm anchor must always reach the audio
    //! rail even when `(audio_state, exit_armed)` is unchanged — the round-2
    //! suppression keyed only on `(audio_state, exit_armed)` and dropped a re-arm at
    //! a different boundary. The fix suppresses only on the FULL `(state,
    //! exit_armed, anchor)` triple. Also pins MAJOR-4: a re-cue (`Cued`) maps to
    //! `AudioTransport::Stopped` (re-cue), not `Paused`.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::sync::Arc;

    use multiview_core::time::{MediaTime, Rational};

    use super::publish_audio_control;
    use crate::player::{
        AudioTransport, EofPolicy, MediaPlayerState, PlayerHandle, PlayoutGeometry,
        TransportMailbox,
    };

    fn handle(eof: EofPolicy) -> PlayerHandle {
        // A 1 s vamp window at 48 fps (in=0,out=48,vamp 0..48) — geometry irrelevant
        // to the suppression logic, only valid construction matters.
        let geometry = PlayoutGeometry::new(0, 48, 0, 48, Rational::new(48, 1)).unwrap();
        PlayerHandle::new(
            "p".to_owned(),
            geometry,
            eof,
            true,
            Arc::new(TransportMailbox::new()),
        )
    }

    /// A re-published armed-vamp-exit with a CHANGED anchor (a re-arm / move-exit at
    /// a different boundary) must bump the bus generation — it is NOT suppressed by
    /// the `(Vamping, exit_armed=true)` tuple repeating.
    #[test]
    fn a_changed_arm_anchor_propagates_even_when_state_is_unchanged() {
        let handle = handle(EofPolicy::Loop);
        let mut last = None;

        // First arm at anchor A1.
        let a1 = MediaTime::from_nanos(1_000_000_000);
        publish_audio_control(
            &handle,
            MediaPlayerState::Vamping { exit_armed: true },
            a1,
            &mut last,
        );
        let after_first = handle.control_bus.load();
        assert_eq!(after_first.state, AudioTransport::Vamping);
        assert_eq!(
            after_first.exit_arm_anchor,
            Some(a1),
            "the first arm publishes anchor A1"
        );
        let gen1 = after_first.generation;

        // Re-arm at a DIFFERENT anchor A2, SAME (state, exit_armed). Must publish.
        let a2 = MediaTime::from_nanos(3_000_000_000);
        publish_audio_control(
            &handle,
            MediaPlayerState::Vamping { exit_armed: true },
            a2,
            &mut last,
        );
        let after_second = handle.control_bus.load();
        assert!(
            after_second.generation > gen1,
            "a changed arm anchor must bump the generation (not be suppressed) — MAJOR-5"
        );
        assert_eq!(
            after_second.exit_arm_anchor,
            Some(a2),
            "the changed anchor A2 must reach the bus"
        );
    }

    /// An UNCHANGED control (same state, same anchor) is suppressed — no needless
    /// per-frame generation churn (the other half of the MAJOR-5 contract).
    #[test]
    fn an_unchanged_control_is_suppressed() {
        let handle = handle(EofPolicy::Loop);
        let mut last = None;
        let a = MediaTime::from_nanos(1_000_000_000);
        publish_audio_control(
            &handle,
            MediaPlayerState::Vamping { exit_armed: true },
            a,
            &mut last,
        );
        let gen1 = handle.control_bus.load().generation;
        // Same state + same anchor again: suppressed.
        publish_audio_control(
            &handle,
            MediaPlayerState::Vamping { exit_armed: true },
            a,
            &mut last,
        );
        assert_eq!(
            handle.control_bus.load().generation,
            gen1,
            "an unchanged control must be suppressed (no per-frame churn)"
        );
    }

    /// MAJOR-4: the video's re-cue state (`Cued`, where `MediaPlayer::stop()` lands)
    /// maps to `AudioTransport::Stopped` (re-cue), NOT `Paused` (hold-in-place).
    #[test]
    fn cued_maps_to_stopped_not_paused() {
        let handle = handle(EofPolicy::Loop);
        let mut last = None;
        publish_audio_control(&handle, MediaPlayerState::Cued, MediaTime::ZERO, &mut last);
        assert_eq!(
            handle.control_bus.load().state,
            AudioTransport::Stopped,
            "the video re-cue (Cued) must route to an audio re-cue (Stopped), not pause (MAJOR-4)"
        );
        // Paused/Holding still map to Paused (hold).
        let mut last2 = None;
        publish_audio_control(
            &handle,
            MediaPlayerState::Paused,
            MediaTime::ZERO,
            &mut last2,
        );
        assert_eq!(
            handle.control_bus.load().state,
            AudioTransport::Paused,
            "an actual pause still maps to Paused (hold position)"
        );
    }
}

/// Seek the open container to `frame` (in-point of a loop/vamp, a cue, or a user
/// seek) and **flush the decoder** so no stale reordered frame from before the
/// seek leaks past it (ADR-0097 rule 2, non-negotiable). Arms the **pending
/// frame-accurate target** at `frame`: libav lands on a keyframe at-or-before
/// `frame`, so the decode loop discards forward to `frame` before publishing,
/// making the first published frame exactly the requested in-point even for a
/// non-keyframe in-point. A failed seek is logged and skipped — the tile holds
/// last-good rather than crashing (invariants #1/#2); the supervised reconnect
/// bracket in [`ingest_loop`] is the deeper recovery.
fn apply_player_seek(
    input: &mut ffmpeg::format::context::Input,
    decoder: &mut StreamVideoDecoder,
    frame: u64,
    cadence: Rational,
    pending_target: &mut Option<u64>,
) {
    let target_us = frame_to_seek_us(frame, cadence);
    if let Err(reason) = input.seek(target_us, ..) {
        tracing::warn!(%reason, frame, "media-player seek failed; holding last-good");
        return;
    }
    if let Err(reason) = decoder.flush() {
        tracing::warn!(%reason, "media-player decoder flush failed after seek");
    }
    *pending_target = Some(frame);
}

/// The 0-based source frame index of a decoded frame: `(meta.pts − origin) /
/// frame_period`, rounded to nearest, where `origin` is the asset's first-frame
/// PTS (a container may start PTS at a non-zero offset). Used to align a
/// frame-accurate seek to its target.
fn frame_index_of(
    decoded: &multiview_ffmpeg::DecodedVideoFrame,
    origin_ns: i64,
    frame_period_ns: i64,
) -> u64 {
    if frame_period_ns <= 0 {
        return 0;
    }
    let rel_ns = decoded.meta.pts.as_nanos().saturating_sub(origin_ns).max(0);
    // Round to nearest: (rel + period/2) / period.
    let idx = rel_ns.saturating_add(frame_period_ns / 2) / frame_period_ns;
    u64::try_from(idx).unwrap_or(0)
}

/// Keep a drained, non-looping player's tile LIVE: heartbeat-republish its
/// terminal frame at cadence with advancing stamps (media-playout §7.3) until
/// `stop`, so it never ages to STALE on the freshness ladder. An `auto_off`
/// player (whose `on_heartbeat` returns [`PlayerAction::Ended`]) instead returns
/// immediately — it has released, and the store holds its last frame forever
/// (`HoldForever`). With no frame ever decoded (a zero-length asset), returns
/// immediately too.
fn heartbeat_player_hold(
    plan: &IngestPlan,
    player: &mut crate::player::MediaPlayer,
    last_image: Option<&Arc<Nv12Image>>,
    pacer: &mut PtsWallClock,
    stop: &AtomicBool,
) {
    use crate::player::PlayerAction;
    let Some(image) = last_image else {
        return;
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // A held player heartbeats `Hold { at }` with an advancing stamp; an
        // `auto_off` player heartbeats `Ended` (released — stop the heartbeat,
        // the tile holds last-good via HoldForever). Any other action is not
        // expected on a drained player; stop rather than busy-loop.
        let PlayerAction::Hold { at } = player.on_heartbeat() else {
            return;
        };
        pacer.wait_for(at, stop);
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Republish the held handle — an `Arc::clone` (refcount bump), never a
        // pixel-plane copy.
        plan.store.publish_arc(Arc::clone(image), at);
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
    fn no_clock_overlay_yields_an_empty_set() {
        assert!(analog_clocks_from_config(&[], 1280, 720).is_empty());
        // A digital clock overlay does NOT request an analog face.
        let digital = clock_overlay(serde_json::json!({ "face": "digital" }));
        assert!(analog_clocks_from_config(&[digital], 1280, 720).is_empty());
    }

    #[test]
    fn analog_face_param_requests_the_face() {
        let analog = clock_overlay(serde_json::json!({ "face": "analog" }));
        let specs = analog_clocks_from_config(&[analog], 1280, 720);
        let spec = *specs
            .first()
            .expect("an analog clock overlay yields a spec");
        assert_eq!(specs.len(), 1);
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
        let specs = analog_clocks_from_config(&[analog], 1280, 720);
        let spec = *specs.first().expect("spec");
        assert!((spec.cx() - 200.0).abs() < 0.5, "explicit x honoured");
        assert!((spec.cy() - 150.0).abs() < 0.5, "explicit y honoured");
        assert!(
            (spec.radius() - 64.0).abs() < 0.5,
            "explicit radius honoured"
        );
    }

    #[test]
    fn every_analog_entry_yields_a_face_in_set_order() {
        // MAJOR-1: ALL analog-face entries render — no first-wins. The specs
        // come back in working-set order, each honouring its own placement,
        // with the interleaved digital entry contributing nothing.
        let a = clock_overlay(
            serde_json::json!({ "face": "analog", "x": 100, "y": 100, "radius": 32 }),
        );
        let digital = clock_overlay(serde_json::json!({ "face": "digital" }));
        let b = clock_overlay(
            serde_json::json!({ "face": "analog", "x": 900, "y": 500, "radius": 48 }),
        );
        let specs = analog_clocks_from_config(&[a, digital, b], 1280, 720);
        assert_eq!(specs.len(), 2, "both analog entries yield faces");
        assert!(
            (specs[0].cx() - 100.0).abs() < 0.5,
            "first face keeps its x"
        );
        assert!(
            (specs[1].cx() - 900.0).abs() < 0.5,
            "second face keeps its x"
        );
        assert!((specs[1].radius() - 48.0).abs() < 0.5);
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

    use super::{ingest_open_options, ingest_plan_for, youtube_open_url, SourceLocation};

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

    // -----------------------------------------------------------------------
    // IN-5b: the proactive re-resolution wiring — the swappable-URL slot on the
    // plan, the open-URL selection that prefers it (make-before-break), and the
    // bridge that runs `run_reresolve_loop` over the slot off the data plane.
    // -----------------------------------------------------------------------

    #[test]
    fn youtube_plan_carries_a_swappable_url_slot() {
        // IN-5b: the proactive re-resolution loop needs a lock-free slot on the
        // plan to publish each fresh master into; a youtube plan must allocate one
        // (empty at build — filled by the loop ahead of expiry).
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

        let slot = plan
            .youtube_url_slot
            .as_ref()
            .expect("a youtube plan must carry a swappable-url slot");
        // Empty at build: the first open resolves inline; the loop fills it ahead
        // of expiry (make-before-break).
        assert!(
            slot.load().is_none(),
            "the slot starts empty (cold start resolves inline)"
        );
    }

    #[test]
    fn open_url_prefers_the_proactive_slot_over_inline_resolve() {
        // IN-5b make-before-break: when the proactive loop has published a fresh
        // master into the slot, the (re)open uses IT and NEVER calls the inline
        // resolver — the tile reopens onto the next-up URL without a cold resolve.
        let slot = arc_swap::ArcSwapOption::empty();
        slot.store(Some(Arc::new(
            "https://r1.googlevideo.com/fresh.m3u8?expire=9999".to_owned(),
        )));

        let inline_called = std::cell::Cell::new(false);
        let chosen = youtube_open_url(Some(&slot), || {
            inline_called.set(true);
            Ok("https://INLINE.example/should-not-be-used.m3u8".to_owned())
        })
        .expect("a populated slot yields its url");

        assert_eq!(chosen, "https://r1.googlevideo.com/fresh.m3u8?expire=9999");
        assert!(
            !inline_called.get(),
            "a populated slot must NOT trigger an inline resolve (make-before-break)"
        );
    }

    #[test]
    fn open_url_falls_back_to_inline_resolve_when_slot_is_empty() {
        // Cold start (slot empty, before the loop's first publish) OR no loop: the
        // (re)open resolves inline this once, so the very first open never waits on
        // the loop and a source with no running loop still works (IN-5 fallback).
        let slot = arc_swap::ArcSwapOption::<String>::empty();
        let inline_called = std::cell::Cell::new(false);
        let chosen = youtube_open_url(Some(&slot), || {
            inline_called.set(true);
            Ok("https://r2.googlevideo.com/cold.m3u8?expire=8888".to_owned())
        })
        .expect("an empty slot falls back to the inline resolve");

        assert_eq!(chosen, "https://r2.googlevideo.com/cold.m3u8?expire=8888");
        assert!(
            inline_called.get(),
            "an empty slot must fall back to the inline resolve"
        );
    }

    #[test]
    fn open_url_propagates_an_inline_resolve_failure() {
        // When the slot is empty AND the inline resolve fails (binary absent /
        // extraction broke), the error propagates so the reconnect bracket backs
        // off and the tile degrades — never a panic, never a silent success.
        let slot = arc_swap::ArcSwapOption::<String>::empty();
        let err = youtube_open_url(Some(&slot), || Err("yt-dlp unavailable".to_owned()))
            .expect_err("an empty slot with a failing inline resolve must error");
        assert_eq!(err, "yt-dlp unavailable");
    }

    /// A scripted fake resolver for driving the bridge over the slot with no
    /// network/subprocess — returns canned results in order, recording call count.
    struct FakeResolver {
        results: std::sync::Mutex<std::collections::VecDeque<Result<ResolvedHls, YoutubeError>>>,
        calls: std::sync::atomic::AtomicU32,
    }

    impl multiview_input::youtube::reresolve::Resolver for FakeResolver {
        async fn resolve(&self, _url: &str) -> Result<ResolvedHls, YoutubeError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.results
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or(Err(YoutubeError::Unavailable("exhausted".to_owned())))
        }
    }

    use multiview_input::youtube::reresolve::{run_reresolve_loop, ManualClock, ReresolveConfig};
    use multiview_input::youtube::{LiveStatus, ResolvedHls, YoutubeError};

    fn resolved(tag: &str, expire_unix: i64) -> ResolvedHls {
        ResolvedHls::new(
            format!("https://{tag}.googlevideo.com/index.m3u8?expire={expire_unix}"),
            LiveStatus::Live,
            Some(expire_unix),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn reresolve_loop_publishes_fresh_urls_into_the_plan_slot() {
        // IN-5b end-to-end (offline): the bridge's swap closure — the SAME closure
        // shape `youtube_reresolve_thread` installs — writes each freshly resolved
        // master into the plan's lock-free slot, make-before-break. With `expire`
        // below the injected clock the deadline is already past, so the second
        // resolve fires WITHOUT a timed wait, exercising the slot-publish wiring
        // (the lead-time math is covered by the pure schedule tests in
        // multiview-input). The slot ends holding the LATEST master.
        let first = resolved("r1", 0);
        let second = resolved("r2", 0);
        let resolver = FakeResolver {
            results: std::sync::Mutex::new(
                vec![Ok(first.clone()), Ok(second.clone())]
                    .into_iter()
                    .collect(),
            ),
            calls: std::sync::atomic::AtomicU32::new(0),
        };

        // The plan slot the decode thread reads via `youtube_open_url`.
        let slot: Arc<arc_swap::ArcSwapOption<String>> = Arc::new(arc_swap::ArcSwapOption::empty());
        let swap_slot = Arc::clone(&slot);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let clock = ManualClock::new(1000);
        let cfg = ReresolveConfig {
            lead: std::time::Duration::from_secs(1),
            ttl_guard: std::time::Duration::from_secs(10),
            error_burst_threshold: 100,
        };

        let handle = tokio::spawn(async move {
            run_reresolve_loop(
                "https://youtube.com/watch?v=v",
                cfg,
                &resolver,
                &clock,
                &stop_loop,
                move |url: String| {
                    // The exact swap-into-slot closure the CLI bridge installs.
                    swap_slot.store(Some(Arc::new(url)));
                },
            )
            .await
        });

        // Let both resolves land (deadline already past → no wait), then stop.
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        handle.await.expect("loop joins").expect("loop ok");

        // The slot holds the LATEST master — the decode thread's next (re)open uses
        // it (make-before-break); the loop only ever wrote the lock-free slot
        // (bounded, non-blocking — inv #10).
        let held = slot.load_full().expect("slot holds the latest master");
        assert_eq!(*held, second.manifest_url);
    }

    #[tokio::test(start_paused = true)]
    async fn reresolve_loop_leaves_slot_empty_when_resolution_fails() {
        // A first-resolve failure must publish NOTHING into the slot (the decode
        // thread then falls back to its inline resolve / the tile degrades) and
        // must never panic — the loop backs off and stops cleanly (inv #1/#10).
        let resolver = FakeResolver {
            results: std::sync::Mutex::new(
                vec![Err(YoutubeError::Resolve("n-sig rotated".to_owned()))]
                    .into_iter()
                    .collect(),
            ),
            calls: std::sync::atomic::AtomicU32::new(0),
        };
        let slot: Arc<arc_swap::ArcSwapOption<String>> = Arc::new(arc_swap::ArcSwapOption::empty());
        let swap_slot = Arc::clone(&slot);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let clock = ManualClock::new(1000);

        let handle = tokio::spawn(async move {
            run_reresolve_loop(
                "https://youtube.com/watch?v=dead",
                ReresolveConfig::default(),
                &resolver,
                &clock,
                &stop_loop,
                move |url: String| {
                    swap_slot.store(Some(Arc::new(url)));
                },
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        handle.await.expect("loop joins").expect("loop ok");

        assert!(
            slot.load().is_none(),
            "a failed resolve publishes no url; the slot stays empty"
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

/// Tests for the RIST ingest+egress wiring (RIST-3, ADR-0095 Tier-0): under the
/// `rist` feature a `rist` source plans as a live network ingest location whose
/// URL carries the lowered `librist` options, and a `rist` push output builds a
/// `PushProtocol::Rist` runnable fed the same encoded packets (invariant #7).
/// The PSK is resolved from a `secret_ref` (`env:VAR`) and never logged. No
/// network/peer is touched here (offline like the SRT tests).
#[cfg(test)]
#[cfg(feature = "rist")]
mod rist_tests {
    #![allow(
        // reason: a unit test module; the strict workspace lints are relaxed for
        // test code per CLAUDE.md (these mirror the surrounding `tests` modules).
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    use std::sync::Arc;

    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::{MultiviewConfig, RistOptions, Source};
    use multiview_core::time::Rational;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use super::{
        build_outputs, ingest_plan_for, rist_resolved_url, RunnableOutput, SourceLocation,
    };

    fn rist_source(json: serde_json::Value) -> Source {
        serde_json::from_value(json).expect("rist source deserializes")
    }

    #[test]
    fn rist_source_plans_as_a_live_network_url_with_lowered_options() {
        let source = rist_source(serde_json::json!({
            "id": "rist-in",
            "kind": "rist",
            "url": "rist://[::1]:5000",
            "rist": { "profile": "main", "buffer_ms": 1000 },
        }));
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
        .expect("rist source plans without failing the build");

        let SourceLocation::Url(url) = &plan.location else {
            panic!("expected a Url location for a rist source");
        };
        assert!(
            url.starts_with("rist://[::1]:5000?"),
            "base preserved: {url}"
        );
        assert!(url.contains("rist_profile=1"), "main ⇒ 1: {url}");
        assert!(url.contains("buffer_size=1000"), "{url}");
        assert!(plan.live, "a rist source must be live (reconnects)");
    }

    #[test]
    fn rist_psk_is_resolved_from_env_and_redacted_in_logs() {
        // Set a unique env var so the resolution is deterministic in CI.
        let var = "RIST_TEST_PSK_RESOLVE";
        std::env::set_var(var, "the-pre-shared-key");
        let opts: RistOptions = serde_json::from_value(serde_json::json!({
            "profile": "main",
            "encryption": { "aes_bits": "aes256", "secret_ref": format!("env:{var}") },
        }))
        .expect("opts");

        let live = rist_resolved_url("rist://[::1]:5000", Some(&opts), false)
            .expect("the resolved secret lowers into the url");
        assert!(live.contains("secret=the-pre-shared-key"), "{live}");

        // The redacted (loggable) form hides the plaintext PSK.
        let logged =
            rist_resolved_url("rist://[::1]:5000", Some(&opts), true).expect("redacted url");
        assert!(logged.contains("secret=***"), "{logged}");
        assert!(!logged.contains("the-pre-shared-key"), "{logged}");
        std::env::remove_var(var);
    }

    #[test]
    fn a_rist_push_output_builds_as_a_runnable_push() {
        // A push-only RIST config must BUILD (the keystone — `build_outputs`
        // produces a Push runnable, not a skip). A live handshake is the
        // `#[ignore]`d hardware test; here we only prove the wiring exists.
        let toml = r##"
schema_version = 1
[canvas]
width = 320
height = 240
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
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "rist"
url = "rist://[::1]:6000"
codec = "mpeg2video"
[outputs.rist]
profile = "main"
buffer_ms = 700
"##;
        let config = MultiviewConfig::load_from_toml(toml).expect("parse");
        config.validate().expect("validates");
        let epoch = multiview_output::SharedEpoch::default();
        #[cfg(feature = "webrtc-native")]
        let egress_sinks = std::collections::HashMap::new();
        let built = build_outputs(
            &config.outputs,
            &epoch,
            #[cfg(feature = "webrtc-native")]
            &egress_sinks,
            config.system.as_ref().and_then(|s| s.ndi.as_ref()),
            Rational { num: 25, den: 1 },
        )
        .expect("rist push output builds");
        let push_count = built
            .packet
            .iter()
            .filter(|r| matches!(r, RunnableOutput::Push { label, .. } if *label == "rist"))
            .count();
        assert_eq!(push_count, 1, "exactly one rist push runnable is built");
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
        let supervisor = IngestSupervisor::start(
            vec![plan],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &registry,
        );
        let flag = registry
            .lock()
            .expect("registry")
            .get("net1")
            .cloned()
            .expect("the startup ingest thread registers its stop flag");
        // A live remove raises exactly this flag (the hub does this); the
        // ingest loop observes it between (re)connect attempts and exits, so
        // the supervisor's shutdown join returns without the wedge-detach path.
        flag.stop.store(true, Ordering::Release);
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
        let supervisor = IngestSupervisor::start(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![plan],
            &registry,
        );
        let flag = registry
            .lock()
            .expect("registry")
            .get("net1/captions")
            .cloned()
            .expect("the caption reader registers under {id}/captions");
        flag.stop.store(true, Ordering::Release);
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

#[cfg(all(test, feature = "overlay"))]
mod live_overlay_bake_tests {
    //! ADR-W022: the bake consumer re-derives its overlay render state from the
    //! live [`OverlayApplySlot`](crate::live_overlays::OverlayApplySlot) on a
    //! generation change — proven at the pixel level on the baked NV12 frame.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::HashMap;
    use std::sync::Arc;

    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_core::time::Rational;

    use super::{BakeContext, StreamBaker, StreamItem};

    /// A pixel region (x, y, w, h) the tests assert over — placed well clear
    /// of the top-left digital-clock chrome (whose readout changes with the
    /// wall clock and would otherwise alias the diff).
    type Region = (u32, u32, u32, u32);

    /// The single-face region: a box around a face at (200, 120) radius 40 on
    /// a 320x180 canvas.
    const FACE: Region = (150, 70, 100, 100);
    /// Region around face A at (80, 120) radius 30 (the two-clock test).
    const FACE_A: Region = (45, 85, 70, 70);
    /// Region around face B at (240, 120) radius 30 (the two-clock test).
    const FACE_B: Region = (205, 85, 70, 70);

    /// Count samples inside `region` whose full (y, u, v) tuple differs
    /// between frames — chroma included, so a restore is proven on every
    /// plane, not luma alone.
    fn region_diff(a: &Nv12Image, b: &Nv12Image, region: Region) -> usize {
        let (x0, y0, w, h) = region;
        let mut differ = 0_usize;
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let pa = a.sample(x, y).expect("in bounds");
                let pb = b.sample(x, y).expect("in bounds");
                if pa != pb {
                    differ += 1;
                }
            }
        }
        differ
    }

    /// A bake context with no tiles and no boot analog clock, wired to `slot`.
    fn bake_context(slot: &crate::live_overlays::OverlayApplySlot) -> BakeContext {
        BakeContext {
            tile_specs: Vec::new(),
            meter_db_timelines: HashMap::new(),
            subtitles: None,
            sidecar_target: None,
            analog_clocks: Vec::new(),
            canvas_color: CanvasColor::default(),
            cadence: Rational::new(25, 1),
            // No Conspect watermark in these pixel tests: the asserted face
            // regions must change ONLY with the live overlay slot (S3 has its
            // own coverage), and the 320x180 test canvas matches `item`.
            watermark_signal: None,
            canvas_width: 320,
            canvas_height: 180,
            overlay_apply: Some(Arc::clone(slot)),
        }
    }

    /// One streamed tick over a uniform dark canvas.
    fn item(tick_index: u64) -> StreamItem {
        let tag = CanvasColor::default().output_tag();
        StreamItem {
            canvas: Arc::new(Nv12Image::solid(320, 180, 16, 128, 128, tag).expect("solid")),
            tick_index,
            source_states: HashMap::new(),
            captions: HashMap::new(),
            caption_bitmaps: HashMap::new(),
            faults: HashMap::new(),
        }
    }

    /// An analog wall-clock overlay document at the given placement.
    fn analog_clock_at(id: &str, x: i64, y: i64, radius: i64) -> multiview_config::Overlay {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": "clock", "target": "canvas",
            "face": "analog", "x": x, "y": y, "radius": radius
        }))
        .expect("valid overlay document")
    }

    /// An analog wall-clock overlay document centred in [`FACE`].
    fn analog_clock_doc() -> multiview_config::Overlay {
        analog_clock_at("clk", 200, 120, 40)
    }

    /// A frame baked from an EMPTY overlay set (the bare-canvas reference).
    fn blank_frame() -> Arc<Nv12Image> {
        let empty = crate::live_overlays::overlay_apply_slot(Vec::new());
        let mut bare = StreamBaker::new(bake_context(&empty)).expect("bare baker");
        bare.bake(&item(0)).expect("bare bake")
    }

    #[test]
    fn stream_baker_rederives_the_analog_clock_from_the_live_slot() {
        // Generation 0: an empty boot set ⇒ no analog face anywhere.
        let slot = crate::live_overlays::overlay_apply_slot(Vec::new());
        let mut baker = StreamBaker::new(bake_context(&slot)).expect("baker");

        let before = baker.bake(&item(0)).expect("bake gen 0");

        // LIVE APPLY: publish a set carrying the analog clock. The next baked
        // frame must ink the face (the bezel ring + hands) into the region.
        let _gen = crate::live_overlays::publish_set(&slot, vec![analog_clock_doc()]);
        let with_face = baker.bake(&item(1)).expect("bake gen 1");
        let inked = region_diff(&before, &with_face, FACE);
        assert!(
            inked > 50,
            "the live-applied analog face must visibly ink the region \
             (differing samples: {inked})"
        );

        // LIVE REMOVE: publish the empty set again — the face disappears and
        // the region returns to the bare-canvas pixels (every plane).
        let _gen = crate::live_overlays::publish_set(&slot, Vec::new());
        let removed = baker.bake(&item(2)).expect("bake gen 2");
        assert_eq!(
            region_diff(&before, &removed, FACE),
            0,
            "removing the overlay must restore the bare-canvas face region"
        );
    }

    #[test]
    fn two_analog_clocks_both_ink_and_removing_one_keeps_the_other() {
        // MAJOR-1: EVERY analog-face clock entry renders its own face — there
        // is no first-wins. Two clocks at distinct placements both ink, and
        // removing one keeps the other while restoring the removed region.
        let blank = blank_frame();
        let slot = crate::live_overlays::overlay_apply_slot(Vec::new());
        let mut baker = StreamBaker::new(bake_context(&slot)).expect("baker");

        let _gen = crate::live_overlays::publish_set(
            &slot,
            vec![
                analog_clock_at("clk_a", 80, 120, 30),
                analog_clock_at("clk_b", 240, 120, 30),
            ],
        );
        let both = baker.bake(&item(0)).expect("bake both");
        let a_ink = region_diff(&blank, &both, FACE_A);
        let b_ink = region_diff(&blank, &both, FACE_B);
        assert!(
            a_ink > 50,
            "the FIRST analog face must ink its region (got {a_ink})"
        );
        assert!(
            b_ink > 50,
            "the SECOND analog face must ink its region too — no first-wins \
             (got {b_ink})"
        );

        // Remove clk_b: clk_a keeps drawing, clk_b's region restores exactly.
        let _gen =
            crate::live_overlays::publish_set(&slot, vec![analog_clock_at("clk_a", 80, 120, 30)]);
        let only_a = baker.bake(&item(1)).expect("bake only_a");
        assert!(
            region_diff(&blank, &only_a, FACE_A) > 50,
            "removing the OTHER clock must not disturb the kept face"
        );
        assert_eq!(
            region_diff(&blank, &only_a, FACE_B),
            0,
            "the removed clock's region must restore to the bare canvas"
        );
    }

    #[test]
    fn seeded_slot_drives_the_first_bake() {
        // The context carries no boot analog clock, but the slot's
        // generation-0 set does: the FIRST bake must already honour the slot
        // (the seeded set is the boot truth — one source of truth).
        let slot = crate::live_overlays::overlay_apply_slot(vec![analog_clock_doc()]);
        let mut baker = StreamBaker::new(bake_context(&slot)).expect("baker");
        let first = baker.bake(&item(0)).expect("bake");
        assert!(
            region_diff(&blank_frame(), &first, FACE) > 50,
            "the seeded slot set must drive the very first bake"
        );
    }

    #[test]
    fn same_generation_bake_skips_rederivation() {
        // The generation gate genuinely skips: perturb the baker's derived
        // face state BEHIND the gate and prove a same-generation bake does
        // NOT restore it (no re-derive happened), while a new generation does.
        let blank = blank_frame();
        let slot = crate::live_overlays::overlay_apply_slot(vec![analog_clock_doc()]);
        let mut baker = StreamBaker::new(bake_context(&slot)).expect("baker");

        let first = baker.bake(&item(0)).expect("bake gen 0");
        assert!(region_diff(&blank, &first, FACE) > 50, "gen-0 face inks");

        // PERTURB: clear the derived faces directly. If the gate works, the
        // next bake (same generation) must NOT re-derive them from the slot.
        baker.baker.set_analog_clocks(Vec::new());
        let second = baker.bake(&item(1)).expect("bake same gen");
        assert_eq!(
            region_diff(&blank, &second, FACE),
            0,
            "a same-generation bake must skip re-derivation (gate holds)"
        );

        // A NEW generation re-derives from the slot: the face returns.
        let _gen = crate::live_overlays::publish_set(&slot, vec![analog_clock_doc()]);
        let third = baker.bake(&item(2)).expect("bake new gen");
        assert!(
            region_diff(&blank, &third, FACE) > 50,
            "a new generation must re-derive (the face returns)"
        );
    }
}

/// ADR-0060: every per-source decode thread must run inside a resource scope so
/// libav lines emitted synchronously on that thread (HLS opens, decode errors)
/// are attributed to the source's stable config id via the thread-local
/// [`ResourceContext`] (mechanism A) — not left context-free. The scope is the
/// [`ResourceGuard`] [`ingest_resource_scope`] enters at the top of
/// [`ingest_loop`]; this proves the guard sets the right id while live and
/// clears it on drop (scoped, never stale — a line after the thread releases the
/// source must not inherit it).
#[cfg(test)]
mod obs_resource_scope_tests {
    use multiview_ffmpeg::current_resource;

    use super::ingest_resource_scope;

    /// While the guard is alive, the thread's current resource is the source's
    /// config id with kind `source`; after it drops, the thread is unattributed
    /// again (so an unrelated later line falls through to weaker attribution
    /// rather than inheriting a stale id, ADR-0060 §3.1).
    #[test]
    fn ingest_scope_attributes_then_clears_the_source_id() {
        assert!(
            current_resource().is_none(),
            "no resource is owned before entering the scope"
        );
        {
            let _scope = ingest_resource_scope("cnn");
            let ctx = current_resource().expect("the guard sets a resource while live");
            assert_eq!(ctx.id(), "cnn", "scoped to the source's stable config id");
            assert_eq!(ctx.kind(), "source", "attributed as a source resource");
        }
        assert!(
            current_resource().is_none(),
            "the guard clears the resource on drop (scoped, never stale)"
        );
    }
}

/// OUTMETA (ADR-0088/0089) cli-wiring tests: `output_mux_meta` translates a
/// config `Output`'s metadata plan + orientation into the muxer apply values —
/// per-transport key mapping (TS SDT vs generic container tags), Dropped fields
/// never pushed, and the tag-path display matrix only on a tag-capable
/// transport. Pure (no engine/ffmpeg); proves the minimal cli read is correct.
#[cfg(test)]
mod outmeta_wiring_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::output_mux_meta;
    use multiview_output::{display_matrix_degrees, MetadataScope};

    fn output_from(json: serde_json::Value) -> multiview_config::Output {
        serde_json::from_value(json).expect("output deserializes")
    }

    #[test]
    fn maps_hls_fields_to_generic_keys_and_tags_orientation() {
        let out = output_from(serde_json::json!({
            "kind": "hls",
            "path": "/hls/mv",
            "codec": "h264",
            "metadata": {
                "title": "Studio A",
                "provider": "Aperim",
                "description": "Conf feed",
                "language": "eng",
            },
            "orientation": { "turn": "cw90", "mode": "auto" },
        }));
        let (meta, matrix) = output_mux_meta(&out);
        let entries = meta.entries();
        let has = |scope_is_format: bool, key: &str, value: &str| {
            entries.iter().any(|e| {
                let scope_ok = match e.scope {
                    MetadataScope::Format => scope_is_format,
                    MetadataScope::Stream { .. } => !scope_is_format,
                    _ => false,
                };
                scope_ok && e.key == key && e.value == value
            })
        };
        assert!(has(true, "title", "Studio A"), "title→title (format)");
        assert!(has(true, "comment", "Conf feed"), "description→comment");
        assert!(has(false, "language", "eng"), "language→stream language");
        // provider is Dropped on HLS ⇒ never pushed.
        assert!(
            !entries.iter().any(|e| e.value == "Aperim"),
            "HLS has no provider carrier — the dropped field is not pushed"
        );
        let m = matrix.expect("HLS auto cw90 ⇒ a display-matrix tag");
        assert_eq!(display_matrix_degrees(m), 90);
    }

    #[test]
    fn maps_mpegts_to_sdt_keys_and_never_tags_orientation() {
        let out = output_from(serde_json::json!({
            "kind": "srt",
            "url": "srt://[::1]:9000",
            "codec": "h264",
            "metadata": { "title": "Studio A", "provider": "Aperim", "service_id": 7 },
            "orientation": { "turn": "cw90", "mode": "pixels" },
        }));
        let (meta, matrix) = output_mux_meta(&out);
        let entries = meta.entries();
        assert!(
            entries
                .iter()
                .any(|e| e.key == "service_name" && e.value == "Studio A"),
            "TS title → SDT service_name"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "service_provider" && e.value == "Aperim"),
            "TS provider → SDT service_provider"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "service_id" && e.value == "7"),
            "TS service_id carried"
        );
        assert!(
            matrix.is_none(),
            "the pixels mechanism is a rendition, not a mux tag — no display matrix"
        );
    }

    #[test]
    fn empty_without_metadata_or_orientation() {
        let out = output_from(serde_json::json!({
            "kind": "hls",
            "path": "/hls/mv",
            "codec": "h264",
        }));
        let (meta, matrix) = output_mux_meta(&out);
        assert!(meta.is_empty());
        assert!(matrix.is_none());
    }
}

/// End-to-end proof of the **media-player loop executor** ([`stream_player`])
/// against a REAL libav decoder — the load-bearing wiring test for ADR-0097's
/// in-place loop (seek + `avcodec_flush_buffers` + keep decoding).
///
/// The pure transport core already proves bit-exactly that the published stamp
/// sequence is monotone and `prev + frame_period` across a loop seam
/// (`media_player_transport::stamps_are_monotonic_across_a_loop_lap`). What this
/// test adds is proof that the EXECUTOR genuinely loops a real decoder: a finite
/// clip, played as a vamp, must publish **more frames than the clip is long** —
/// which is only possible if, on reaching the out-point, the loop seeks back,
/// flushes the decoder, and keeps decoding fresh frames past the seam (rather
/// than hitting EOF and stopping). It also confirms the tile reads LIVE (real
/// decoded frames), and that the published media-time advances past one
/// clip-length (the seam did not freeze/clamp).
#[cfg(all(test, feature = "ffmpeg"))]
mod media_player_loop_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::too_many_lines
    )]

    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ffmpeg_next as ffmpeg;
    use multiview_core::time::{MediaTime, Rational};
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use crate::player::{EofPolicy, PlayoutGeometry, TransportMailbox};

    /// A cheap content fingerprint of an NV12 image: FNV-1a over the Y plane's
    /// in-stride bytes. Two frames with the same picture hash equal; different
    /// `testsrc` frames (a moving pattern) hash differently — enough to identify
    /// WHICH source frame a published tile is, without OCR.
    fn nv12_fingerprint(img: &super::Nv12Image) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in img.y_plane() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Decode the first `count` frames of `clip` to scaled NV12 (the same tile
    /// size the player uses) and return their fingerprints, indexed by source
    /// frame number — the independent reference for "which frame is on the tile".
    fn reference_fingerprints(clip: &Path, tile_w: u32, tile_h: u32, count: u64) -> Vec<u64> {
        use multiview_ffmpeg::StreamVideoDecoder;
        multiview_ffmpeg::ensure_initialized().unwrap();
        let mut input = ffmpeg::format::input(&clip).unwrap();
        let (stream_index, params, time_base, declared_fps) =
            super::best_video_stream_params(&input).unwrap();
        let (decoder, _hw) =
            StreamVideoDecoder::new_preferring_hw(params, time_base, false, None).unwrap();
        let mut decoder = decoder.with_declared_fps(Some(declared_fps));
        let tag = super::CanvasColor::default().output_tag();
        let mut to_tile = super::TileScaler::new(tile_w, tile_h);
        let mut out = Vec::new();
        let mut drained = false;
        'outer: loop {
            while let Some(decoded) = decoder.receive_frame().unwrap() {
                let img = to_tile.convert(&decoded.frame, tag).unwrap();
                out.push(nv12_fingerprint(&img));
                if out.len() as u64 >= count {
                    break 'outer;
                }
            }
            if drained {
                break;
            }
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match packet.read(&mut input) {
                Ok(()) => {
                    if packet.stream() == stream_index {
                        decoder.send_packet(&packet).unwrap();
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    decoder.send_eof().unwrap();
                    drained = true;
                }
                Err(e) => panic!("reference decode read error: {e}"),
            }
        }
        out
    }

    /// Generate a short, all-keyframe-free `mpeg2video` clip with B-frames at
    /// 25 fps. `-bf 2` forces a reorder window so the post-seek decoder flush is
    /// load-bearing (stale B-frames would otherwise leak past the loop seam).
    fn generate_loop_clip(path: &Path, frames: u32) {
        let dur = f64::from(frames) / 25.0;
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(format!("testsrc=size=160x120:rate=25:duration={dur}"))
            .args([
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg2video",
                "-bf",
                "2",
                "-g",
                "6",
                "-f",
                "mpegts",
            ])
            .arg(path)
            .status()
            .expect("spawn ffmpeg to generate the loop clip");
        assert!(status.success(), "ffmpeg failed to generate the loop clip");
        assert!(path.exists(), "loop clip was not written");
    }

    #[test]
    fn a_vamping_player_loops_frame_accurately_over_a_non_keyframe_vamp_window() {
        // A 12-frame clip at 25 fps, GOP = 6 → keyframes at source frames 0 and
        // 6 ONLY. Vamp the window [3, 9): both vamp_in (3) and the post-wrap
        // re-entry are NON-KEYFRAMES, so a correct loop MUST keyframe-seek (to 0
        // for the 3..6 span) then DECODE-AND-DISCARD forward to frame 3 — never
        // publishing a pre-target frame (0,1,2) at the seam. This is the exact
        // case the naive `*source_frame = frame` after a keyframe seek got wrong.
        const CLIP_FRAMES: u32 = 12;
        const VAMP_IN: u64 = 3;
        const VAMP_OUT: u64 = 9;
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("loop.ts");
        generate_loop_clip(&clip, CLIP_FRAMES);

        let tile_w = 160;
        let tile_h = 120;
        // Independent reference: the fingerprint of every source frame [0, 12),
        // so we can identify WHICH source frame each published tile is.
        let refs = reference_fingerprints(&clip, tile_w, tile_h, u64::from(CLIP_FRAMES));
        assert!(
            refs.len() >= usize::try_from(CLIP_FRAMES).unwrap(),
            "reference decode produced too few frames: {}",
            refs.len()
        );
        // Sanity: testsrc frames differ, so the reference fingerprints in the
        // pre-target span and the vamp window must be distinguishable.
        let in_window: std::collections::HashSet<u64> = (VAMP_IN..VAMP_OUT)
            .map(|i| refs[usize::try_from(i).unwrap()])
            .collect();
        let pre_target: std::collections::HashSet<u64> = (0..VAMP_IN)
            .map(|i| refs[usize::try_from(i).unwrap()])
            .collect();
        assert!(
            pre_target.is_disjoint(&in_window),
            "test fixture invalid: pre-target and vamp-window frames are not distinguishable"
        );

        let cadence = Rational::new(25, 1);
        let geometry =
            PlayoutGeometry::new(0, u64::from(CLIP_FRAMES), VAMP_IN, VAMP_OUT, cadence).unwrap();
        let mailbox = Arc::new(TransportMailbox::new());
        let handle = crate::player::PlayerHandle::new(
            "player-test".to_owned(),
            geometry,
            EofPolicy::Loop,
            true, // loop_on_start → vamp the [3,9) window
            Arc::clone(&mailbox),
        );

        let store = Arc::new(TileStore::new(
            "player-test",
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));

        let plan = super::IngestPlan {
            id: "player-test".to_owned(),
            location: super::SourceLocation::Path(clip.clone()),
            player: Some(handle),
            tile_w,
            tile_h,
            store: Arc::clone(&store),
            live: false,
            #[cfg(feature = "overlay")]
            incontainer_sub: None,
            #[cfg(feature = "overlay")]
            embedded_cc: None,
            canvas_color: super::CanvasColor::default(),
            cadence,
            decode_placement: super::DecodePlacement::Default,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry: None,
            #[cfg(feature = "webrtc-native")]
            webrtc_audio_store: None,
            #[cfg(feature = "ndi")]
            ndi_accept_license: false,
            #[cfg(feature = "ndi")]
            ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
                accepted_by: String::new(),
                accepted_at: String::new(),
            },
            #[cfg(feature = "youtube")]
            youtube_url_slot: None,
        };

        // A recorder thread snapshots the published tile's (stamp, fingerprint)
        // as fast as it can while the player runs — across several laps this
        // captures the published CONTENT and the monotone published media-time.
        let captured: Arc<Mutex<Vec<(i64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let rec_store = Arc::clone(&store);
        let rec_captured = Arc::clone(&captured);
        let rec_stop = Arc::new(AtomicBool::new(false));
        let rec_stop_t = Arc::clone(&rec_stop);
        let recorder = std::thread::Builder::new()
            .name("media-player-recorder".to_owned())
            .spawn(move || {
                let far = MediaTime::from_nanos(600_000_000_000);
                let mut last_seq = 0u64;
                while !rec_stop_t.load(Ordering::Acquire) {
                    let seq = rec_store.sequence();
                    if seq != last_seq {
                        last_seq = seq;
                        if let Some(frame) = rec_store.read_at(far).frame() {
                            // The stamp is the store's last published instant.
                            let stamp = rec_store
                                .elapsed_since_frame(far)
                                .map_or(0, |e| far.as_nanos() - e.as_nanos());
                            let fp = nv12_fingerprint(frame);
                            rec_captured.lock().unwrap().push((stamp, fp));
                        }
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
            .unwrap();

        // Drive the player ~5 vamp-window-lengths of wall time (window = 6 × 40 ms
        // = 240 ms) so it loops several times across the non-keyframe seam.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let player_thread = std::thread::Builder::new()
            .name("media-player-loop-test".to_owned())
            .spawn(move || super::ingest_loop(&plan, &stop_thread))
            .unwrap();

        std::thread::sleep(Duration::from_millis(1_500));
        stop.store(true, Ordering::Release);
        player_thread.join().unwrap();
        rec_stop.store(true, Ordering::Release);
        recorder.join().unwrap();

        let captured = captured.lock().unwrap().clone();

        // PROOF 1 — it LOOPED across the non-keyframe seam: more frames published
        // than one vamp window holds (only possible via keyframe-seek + flush +
        // decode-discard + re-decode, not EOF-stop).
        let published = store.sequence();
        let window_len = VAMP_OUT - VAMP_IN;
        assert!(
            published > window_len,
            "a vamping player must loop past one vamp window: published {published} \
             for a {window_len}-frame window (the seek+flush+discard loop did not run)"
        );

        // PROOF 2 — FRAME-ACCURATE: every captured published tile is a frame in
        // the vamp window [3, 9); NONE is a pre-target keyframe-span frame (0,1,2).
        // A naive seek (`*source_frame = frame` after a keyframe seek) would
        // publish frame 0/1/2 at the seam — this asserts it never does.
        assert!(
            !captured.is_empty(),
            "the recorder captured no published frames"
        );
        for &(_, fp) in &captured {
            assert!(
                !pre_target.contains(&fp),
                "a PRE-TARGET frame (source < vamp_in={VAMP_IN}) was published — \
                 the seek is not frame-accurate (published a keyframe-span frame at the seam)"
            );
            assert!(
                in_window.contains(&fp),
                "a published tile is not a frame in the vamp window [{VAMP_IN}, {VAMP_OUT})"
            );
        }

        // PROOF 3 — the published media-time is MONOTONE across the seam (never
        // froze/clamped): captured stamps are non-decreasing and the timeline
        // advanced well past one window (no nanosecond-clamp at the wrap).
        for w in captured.windows(2) {
            assert!(
                w[1].0 >= w[0].0,
                "published media-time must be monotone across the loop seam: {:?} then {:?}",
                w[0].0,
                w[1].0
            );
        }
        let max_stamp = captured.iter().map(|&(s, _)| s).max().unwrap();
        let one_window_ns = i64::try_from(window_len).unwrap() * 40_000_000;
        assert!(
            max_stamp > one_window_ns,
            "the published timeline must advance past one window (got {max_stamp} ns ≤ \
             {one_window_ns} ns — the seam froze/clamped)"
        );
    }

    #[test]
    fn an_unplayable_asset_holds_last_good_without_spinning_the_ingest_thread() {
        // FAIL-SAFE (rule 26 / inv #1): a vamp window whose frames DO NOT EXIST in
        // the asset (here a 4-frame clip but a declared out-point of 12 + vamp
        // [8, 12)) must NOT cycle seek→EOF→seek forever on the ingest thread — the
        // decode-discard can never reach frame 8, and every open→decode→EOF lap
        // publishes zero frames. The player must bound the discard / unproductive
        // laps, give up, and HOLD last-good — the ingest thread TERMINATES ON ITS
        // OWN (no `stop` set), leaving the output clock undisturbed. Against the
        // pre-fix code this loops forever and the assertion below times out.
        const REAL_FRAMES: u32 = 4;
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("short.ts");
        generate_loop_clip(&clip, REAL_FRAMES);

        let cadence = Rational::new(25, 1);
        // Declared geometry far exceeds the real 4-frame clip: BOTH the in-point
        // (8) and the vamp window [8, 12) are entirely past the real end, so every
        // open→decode→EOF lap (which re-seeks to the in-point) decodes only frames
        // 0..3, discards them all (idx < 8), and publishes NOTHING — a genuine
        // unproductive spin the fail-safe must break.
        let geometry = PlayoutGeometry::new(8, 12, 8, 12, cadence).unwrap();
        let mailbox = Arc::new(TransportMailbox::new());
        let handle = crate::player::PlayerHandle::new(
            "vt-bad".to_owned(),
            geometry,
            EofPolicy::Loop,
            true,
            Arc::clone(&mailbox),
        );
        let store = Arc::new(TileStore::new(
            "vt-bad",
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let plan = super::IngestPlan {
            id: "vt-bad".to_owned(),
            location: super::SourceLocation::Path(clip.clone()),
            player: Some(handle),
            tile_w: 160,
            tile_h: 120,
            store: Arc::clone(&store),
            live: false,
            #[cfg(feature = "overlay")]
            incontainer_sub: None,
            #[cfg(feature = "overlay")]
            embedded_cc: None,
            canvas_color: super::CanvasColor::default(),
            cadence,
            decode_placement: super::DecodePlacement::Default,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry: None,
            #[cfg(feature = "webrtc-native")]
            webrtc_audio_store: None,
            #[cfg(feature = "ndi")]
            ndi_accept_license: false,
            #[cfg(feature = "ndi")]
            ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
                accepted_by: String::new(),
                accepted_at: String::new(),
            },
            #[cfg(feature = "youtube")]
            youtube_url_slot: None,
        };

        // Drive the player WITHOUT ever setting `stop`: the fail-safe must end the
        // thread on its own when the asset cannot satisfy the geometry.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let player_thread = std::thread::Builder::new()
            .name("media-player-failsafe-test".to_owned())
            .spawn(move || super::ingest_loop(&plan, &stop_thread))
            .unwrap();

        // Poll for self-termination for up to ~5 s. A correct fail-safe bounds the
        // discard + unproductive laps and returns well within this; a spin never
        // finishes (the pre-fix RED).
        let mut finished = false;
        for _ in 0..500 {
            if player_thread.is_finished() {
                finished = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            finished,
            "the player ingest thread did NOT terminate on an unplayable asset — \
             it is spinning seek→EOF→seek (the fail-safe is missing)"
        );
        // Stop is a no-op (already finished); join to reap.
        stop.store(true, Ordering::Release);
        player_thread.join().unwrap();

        // The output path is undisturbed: the tile rides the framestore state
        // machine (last-good / NO_SIGNAL). A reader never blocks — confirm the
        // store responds (here: no real frame was ever decodable, so it holds
        // NO_SIGNAL under the Slate-vs-HoldForever read, never a hang).
        let _ = store.read_at(MediaTime::from_nanos(1_000_000_000));
    }

    #[test]
    fn a_b_frame_clip_publishes_its_eof_flush_delayed_frames_each_lap() {
        // A `-bf 2` clip holds its trailing reorder-window frames in the decoder
        // until the EOF FLUSH (decoder_flush.rs proves `sent − emitted ≥ 1` frames
        // are buffered awaiting EOF). The player MUST drain + publish those
        // flush-delayed frames before re-seeking the next lap — otherwise the last
        // frame(s) of the window are LOST every lap (and, in the extreme, a lap
        // that publishes nothing until the flush is wrongly judged unproductive
        // and given up). Vamp the WHOLE 6-frame clip and assert that, across
        // several laps, EVERY source frame's content is published — including the
        // reorder-buffered ones. Against the pre-flush judgment the buffered
        // frames are flushed away (lost) and the captured set is incomplete.
        const REAL_FRAMES: u64 = 6;
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("bframe.ts");
        generate_loop_clip(&clip, u32::try_from(REAL_FRAMES).unwrap());

        let tile_w = 160;
        let tile_h = 120;
        let refs = reference_fingerprints(&clip, tile_w, tile_h, REAL_FRAMES);
        assert!(
            refs.len() >= usize::try_from(REAL_FRAMES).unwrap(),
            "reference decode produced too few frames: {}",
            refs.len()
        );
        let want: std::collections::HashSet<u64> = refs.iter().copied().collect();

        let cadence = Rational::new(25, 1);
        let geometry = PlayoutGeometry::new(0, REAL_FRAMES, 0, REAL_FRAMES, cadence).unwrap();
        let mailbox = Arc::new(TransportMailbox::new());
        let handle = crate::player::PlayerHandle::new(
            "vt-bframe".to_owned(),
            geometry,
            EofPolicy::Loop,
            true,
            Arc::clone(&mailbox),
        );
        let store = Arc::new(TileStore::new(
            "vt-bframe",
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let plan = super::IngestPlan {
            id: "vt-bframe".to_owned(),
            location: super::SourceLocation::Path(clip.clone()),
            player: Some(handle),
            tile_w,
            tile_h,
            store: Arc::clone(&store),
            live: false,
            #[cfg(feature = "overlay")]
            incontainer_sub: None,
            #[cfg(feature = "overlay")]
            embedded_cc: None,
            canvas_color: super::CanvasColor::default(),
            cadence,
            decode_placement: super::DecodePlacement::Default,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry: None,
            #[cfg(feature = "webrtc-native")]
            webrtc_audio_store: None,
            #[cfg(feature = "ndi")]
            ndi_accept_license: false,
            #[cfg(feature = "ndi")]
            ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
                accepted_by: String::new(),
                accepted_at: String::new(),
            },
            #[cfg(feature = "youtube")]
            youtube_url_slot: None,
        };

        // Capture every distinct published frame fingerprint across the run.
        let captured: Arc<Mutex<std::collections::HashSet<u64>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let rec_store = Arc::clone(&store);
        let rec_captured = Arc::clone(&captured);
        let rec_stop = Arc::new(AtomicBool::new(false));
        let rec_stop_t = Arc::clone(&rec_stop);
        let recorder = std::thread::Builder::new()
            .name("media-player-bframe-recorder".to_owned())
            .spawn(move || {
                let far = MediaTime::from_nanos(600_000_000_000);
                let mut last_seq = 0u64;
                while !rec_stop_t.load(Ordering::Acquire) {
                    let seq = rec_store.sequence();
                    if seq != last_seq {
                        last_seq = seq;
                        if let Some(frame) = rec_store.read_at(far).frame() {
                            rec_captured.lock().unwrap().insert(nv12_fingerprint(frame));
                        }
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
            })
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let player_thread = std::thread::Builder::new()
            .name("media-player-bframe-test".to_owned())
            .spawn(move || super::ingest_loop(&plan, &stop_thread))
            .unwrap();

        // ~10 laps (clip = 6 × 40 ms = 240 ms): plenty for the poller to catch
        // every per-lap frame including the flush-delayed ones.
        std::thread::sleep(Duration::from_millis(2_500));
        let still_running = !player_thread.is_finished();
        stop.store(true, Ordering::Release);
        player_thread.join().unwrap();
        rec_stop.store(true, Ordering::Release);
        recorder.join().unwrap();

        // It kept looping a GOOD clip (never wrongly given up).
        assert!(
            still_running,
            "a good B-frame clip was WRONGLY given up / held — the fail-safe judged \
             the lap before the EOF flush drained the delayed frames"
        );
        // COMPLETENESS: every source frame's content was published across laps —
        // including the reorder-buffered frames that emit only on the EOF flush.
        // Against the pre-fix code those frames are flushed away and never appear.
        let got = captured.lock().unwrap().clone();
        let missing: Vec<u64> = want.difference(&got).copied().collect();
        assert!(
            missing.is_empty(),
            "the player dropped {} of {} source frames — flush-delayed (reorder) \
             frames were lost instead of published before the re-seek",
            missing.len(),
            want.len()
        );
    }
}

/// Boot-spawn of media-player channels from `config.media_players` (ADR-0057 /
/// ADR-0097): [`build_media_player_boot`] turns a configured player + its
/// default library asset into a player [`IngestPlan`] (carrying a `player`
/// handle) + a registered transport mailbox. Feature-independent (no libav).
#[cfg(test)]
mod media_player_boot_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::MultiviewConfig;

    /// A canvas + one cell + a media library (one clip with a declared
    /// out-point + vamp window) + a media player defaulting to it.
    fn config_with_player() -> MultiviewConfig {
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
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "vt-1"
[media_library]
root = "/srv/media"
[[media_library.assets]]
id = "opener"
kind = "clip"
path = "opener.ts"
in_point_frames = 0
out_point_frames = 100
vamp_in_frames = 10
vamp_out_frames = 90
[[media_players]]
id = "vt-1"
default = "opener"
loop_default = true
"##;
        MultiviewConfig::load_from_toml(doc).expect("parse media-player config")
    }

    #[test]
    fn build_media_player_boot_spawns_a_rolling_player_with_geometry_and_mailbox() {
        let config = config_with_player();
        let layout = config.solve_layout().expect("layout solves");
        let cadence = config.canvas.fps.rational();
        let boot =
            super::build_media_player_boot(&config, &layout, cadence, CanvasColor::default());

        // One rolling player plan + one store + one mailbox.
        assert_eq!(
            boot.plans.len(),
            1,
            "the player with a declared-geometry asset rolls"
        );
        assert_eq!(boot.stores.len(), 1);
        assert!(
            boot.mailboxes.contains_key("vt-1"),
            "the player's transport mailbox is registered for the control plane"
        );

        let plan = &boot.plans[0];
        assert_eq!(plan.id, "vt-1");
        assert!(
            !plan.live,
            "a media player is not a reconnect-forever live source"
        );
        let handle = plan
            .player
            .as_ref()
            .expect("the plan carries a player handle");
        // The geometry is the asset's declared window (the vamp sub-window loops).
        assert_eq!(handle.geometry.in_point(), 0);
        assert_eq!(handle.geometry.out_point(), 100);
        assert_eq!(handle.geometry.vamp_in(), 10);
        assert_eq!(handle.geometry.vamp_out(), 90);
        assert_eq!(handle.geometry.cadence(), cadence);
        assert!(
            handle.loop_on_start,
            "loop_default = true ⇒ the channel vamps on start"
        );
        // The asset path resolves against the library root.
        let super::SourceLocation::Path(p) = &plan.location else {
            panic!("expected a Path location for the player's asset");
        };
        assert_eq!(p.to_str().unwrap(), "/srv/media/opener.ts");

        // ADR-T019: the rolling player ALSO gets an audio store + audio loop plan
        // (so its embedded audio joins the program bus on the same wrap instant),
        // carrying the SAME vamp geometry as the video and the SAME asset path.
        assert_eq!(
            boot.audio_stores.len(),
            1,
            "the rolling player gets an audio store"
        );
        assert_eq!(boot.audio_stores[0].0, "vt-1");
        assert_eq!(
            boot.audio_plans.len(),
            1,
            "the rolling player gets an audio loop plan"
        );
        let ap = &boot.audio_plans[0];
        assert_eq!(ap.id, "vt-1");
        assert_eq!(
            ap.location, "/srv/media/opener.ts",
            "audio decodes the SAME asset as the video"
        );
        assert_eq!(
            ap.vamp_in_frames, 10,
            "audio loops the SAME vamp window as the video"
        );
        assert_eq!(ap.vamp_out_frames, 90);
        assert_eq!(
            ap.cadence, cadence,
            "audio uses the asset cadence for the frame→sample map"
        );
        // ADR-T019 §1: the audio plan shares the SAME `PlayerControlBus` Arc as the
        // video handle — the single authority that makes the rails wrap/exit on the
        // same instant by construction (not a second destructive mailbox drain).
        assert!(
            std::sync::Arc::ptr_eq(&ap.control_bus, &handle.control_bus),
            "the audio rail follows the video handle's control bus (same Arc)"
        );
    }

    #[test]
    fn a_player_without_a_default_asset_boots_idle_but_keeps_its_mailbox() {
        let mut config = config_with_player();
        // Drop the default asset binding: the channel boots idle (no plan) but
        // its mailbox stays registered so a later load command is honoured.
        config.media_players[0].default = None;
        let layout = config.solve_layout().expect("layout solves");
        let cadence = config.canvas.fps.rational();
        let boot =
            super::build_media_player_boot(&config, &layout, cadence, CanvasColor::default());
        assert!(boot.plans.is_empty(), "no default asset ⇒ no rolling plan");
        assert!(
            boot.mailboxes.contains_key("vt-1"),
            "an idle player still registers its mailbox"
        );
        // An idle player contributes no audio store/plan either (ADR-T019): nothing
        // to loop until a load command binds an asset (probe-at-load is post-MVP).
        assert!(
            boot.audio_stores.is_empty(),
            "an idle player has no audio store"
        );
        assert!(
            boot.audio_plans.is_empty(),
            "an idle player has no audio loop plan"
        );
    }
}

/// Data-plane handle-sharing (ADR-0097 / inv #5 / rule 22): the media-player
/// publish + hold path must share the decoded frame by `Arc` handle, never copy
/// the NV12 pixel planes per frame. Pure store + image logic (no libav).
#[cfg(test)]
mod media_player_handle_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_core::time::MediaTime;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    fn test_image(width: u32, height: u32) -> Nv12Image {
        let y_len = usize::try_from(width * height).unwrap();
        let y = vec![0x80u8; y_len];
        let uv = vec![0x80u8; y_len / 2];
        Nv12Image::new(width, height, y, uv, CanvasColor::default().output_tag()).unwrap()
    }

    #[test]
    fn publish_arc_holds_the_same_buffer_handle_no_pixel_copy() {
        // The executor wraps each decoded frame in an `Arc` ONCE and publishes
        // that handle; the held last-good + every heartbeat republish are
        // `Arc::clone`s of the SAME handle (a refcount bump, not a pixel-plane
        // copy). Prove the store holds the exact buffer that was published —
        // reading it back yields a pointer-equal `Arc`, so the hold/republish
        // path allocates ZERO new NV12 buffers per frame.
        let store: TileStore<Nv12Image> =
            TileStore::new("h", TileThresholds::default(), NoSignalPolicy::HoldForever);
        let frame = Arc::new(test_image(64, 48));

        // Publish the handle (what `stream_player` does: `publish_arc`).
        store.publish_arc(Arc::clone(&frame), MediaTime::from_nanos(0));

        // Read it back; the store must hold the SAME underlying buffer.
        let read = store.read_at(MediaTime::from_nanos(0));
        let held = read.frame().expect("a frame is held");
        assert!(
            Arc::ptr_eq(held, &frame),
            "the store must hold the published Arc itself (no pixel-plane copy)"
        );

        // A heartbeat republish (hold path) is `Arc::clone` of the same handle —
        // republishing it leaves the store pointing at the same buffer still.
        store.publish_arc(Arc::clone(&frame), MediaTime::from_nanos(40_000_000));
        let read2 = store.read_at(MediaTime::from_nanos(40_000_000));
        let held2 = read2.frame().expect("a frame is held");
        assert!(
            Arc::ptr_eq(held2, &frame),
            "the heartbeat republish must reuse the same buffer handle (refcount bump only)"
        );

        // The only owners are `frame` (this test) + the store's slot/ring — the
        // strong count reflects shared handles, never duplicated pixel buffers.
        assert!(
            Arc::strong_count(&frame) >= 2,
            "the buffer is shared by handle (strong_count = {})",
            Arc::strong_count(&frame)
        );
    }
}

#[cfg(test)]
mod mp2_slice2_registry_adoption {
    //! MP-2 SLICE 2 (ADR-0030 §3): the CLI `Pipeline` adopts the engine
    //! `SourceRegistry` as the owner of per-source `TileStore` creation + sizing.
    //!  * `cell_pixel_size` sizes a source's decode to the **per-axis supremum**
    //!    across ALL cells that bind it — never just the first-found cell.
    //!  * `Pipeline::build` mints ONE registry that OWNS the shared store (the
    //!    `stores` map + the ingest plan just hold `Arc` clones of it), sized to
    //!    that supremum — the single-program output path is otherwise unchanged.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A 2x2 grid where source `cam1` is bound by two cells of DIFFERENT sizes:
    /// area `a` = 400x480 and area `d` = 800x240 on a 1200x720 canvas. The
    /// per-axis supremum is 800x480 — a size NEITHER binding cell has, so a
    /// first-found (`.find`) result can never equal it: the test discriminates
    /// the bug regardless of which binding cell iterates first.
    fn two_cells_one_source_config() -> multiview_config::MultiviewConfig {
        let doc = r##"schema_version = 1
[canvas]
width = 1200
height = 720
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "2fr"]
rows = ["2fr", "1fr"]
areas = ["a b", "c d"]
[[sources]]
id = "cam1"
kind = "bars"
[[cells]]
id = "small"
area = "a"
[cells.source]
input_id = "cam1"
[[cells]]
id = "wide"
area = "d"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "hls"
path = "/tmp/mp2-slice2-two-cells.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
        multiview_config::MultiviewConfig::load_from_toml(doc).expect("test config parses")
    }

    #[test]
    fn cell_pixel_size_is_the_per_axis_supremum_across_all_binding_cells() {
        let config = two_cells_one_source_config();
        let layout = config.solve_layout().expect("layout solves");

        // Every cell that binds `cam1`, in layout order — the order `.find` walks.
        let bound: Vec<(u32, u32)> = layout
            .cells
            .iter()
            .filter(|c| c.source.as_deref() == Some("cam1"))
            .map(|c| cell_dims(c, layout.canvas.width, layout.canvas.height))
            .collect();
        assert!(bound.len() >= 2, "config must bind cam1 from >= 2 cells");

        let supremum = bound
            .iter()
            .copied()
            .reduce(|(aw, ah), (bw, bh)| (aw.max(bw), ah.max(bh)))
            .unwrap();
        let first = *bound.first().unwrap();
        assert_ne!(
            first, supremum,
            "test precondition: the first-found cell must differ from the supremum \
             so the assertion discriminates the .find() bug"
        );

        let got = cell_pixel_size(&layout, "cam1").expect("cam1 is bound");
        assert_eq!(
            got, supremum,
            "cell_pixel_size must decode at the per-axis supremum across ALL binding \
             cells (ADR-0030 §3), not the first-found cell {first:?}"
        );
    }

    #[test]
    fn pipeline_build_registry_owns_the_shared_store_sized_to_the_supremum() {
        // The single-program build mints ONE SourceRegistry that OWNS each source's
        // shared TileStore: the `stores` map (sampled by the compositor drive) and
        // the ingest plan hold Arc CLONES of the registry's store, and the registry
        // records the decode target = the per-axis supremum across the source's
        // cells. Two cells binding `cam1` therefore share ONE registry entry / ONE
        // store, sized to the supremum — the decode-once seam at the pipeline level.
        let config = two_cells_one_source_config();
        let pipeline = Pipeline::build(&config).expect("pipeline builds");
        let key = multiview_engine::SourceKey::from_canonical("cam1");

        // Exactly one registry entry for the one source, regardless of how many
        // cells bind it.
        assert_eq!(
            pipeline.source_registry.active_len(),
            1,
            "one source => one registry entry, even when two cells bind it"
        );

        // The decode target the registry records is the per-axis supremum — the
        // same value `cell_pixel_size` derives for the ingest plan's tile geometry.
        let expect = cell_pixel_size(&pipeline.layout, "cam1").expect("cam1 is bound");
        assert_eq!(
            pipeline.source_registry.requested_supremum(&key),
            Some(multiview_engine::RequestedSize {
                width: expect.0,
                height: expect.1,
            }),
            "the registry decodes at the per-axis supremum across cam1's cells"
        );

        // The `stores` map holds the registry's OWN store Arc (not an independent
        // TileStore) — the registry is the single owner, the map a lock-free reader.
        let mapped = pipeline.stores.get("cam1").expect("cam1 has a store");
        let owned = pipeline
            .source_registry
            .store(&key)
            .expect("the registry owns cam1's store");
        assert!(
            std::sync::Arc::ptr_eq(mapped, &owned),
            "the stores map must hold the registry's store Arc (decode-once ownership)"
        );
    }
}

#[cfg(all(test, feature = "aes67"))]
mod aes67_wiring_guardrails {
    //! #103: an AES67 audio source is AUDIO-ONLY. It must contribute an
    //! [`AudioStore`](multiview_audio::store::AudioStore) (routed onto the program
    //! bus like any source's audio) but NEVER a video [`TileStore`], never a
    //! [`SourceRegistry`] entry, and never a layout tile. These guardrails protect
    //! the just-merged MP-2 decode-once / tile / per-axis-supremum seam (ADR-0030
    //! §3): the video path must stay byte-identical for the non-AES67 sources of a
    //! mixed config — the aes67 branch is a pure skip-the-video-path addition.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A minimal valid AES67 Class-A L24 stereo SDP (RFC 4566/8866), IPv6-first
    /// (`c=IN IP6`) — the same fixture shape `multiview-input`'s SDP parser
    /// round-trips. CR/LF line endings are real bytes (the parser accepts them).
    const AES67_SDP: &str = "v=0\r\n\
o=- 1 1 IN IP6 2001:db8::1\r\n\
s=Multiview AES67\r\n\
c=IN IP6 ff3e::1\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 98\r\n\
a=rtpmap:98 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:AA-BB-CC-DD-EE-FF-00-11:0\r\n\
a=mediaclk:direct=0\r\n";

    /// A config with ONE video source (`cam1`, bars) bound to the single cell and
    /// ONE audio-only AES67 source (`aes67-in`) bound to no cell. The SDP is
    /// injected as a TOML literal (`'''…'''`) string so its newlines survive.
    fn video_plus_aes67_config() -> multiview_config::MultiviewConfig {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 640
height = 480
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
id = "cam1"
kind = "bars"
[[sources]]
id = "aes67-in"
kind = "aes67"
multicast = "[ff3e::1]:5004"
sdp = '''
{AES67_SDP}'''
[[cells]]
id = "only"
area = "a"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "hls"
path = "/tmp/aes67-guardrail.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        multiview_config::MultiviewConfig::load_from_toml(&doc).expect("test config parses")
    }

    #[test]
    fn aes67_source_makes_an_audio_store_but_no_video_tile_or_registry_entry() {
        let config = video_plus_aes67_config();
        let pipeline = Pipeline::build(&config).expect("pipeline builds with an aes67 source");

        // AUDIO-ONLY: the aes67 source joins the program-audio path (an AudioStore
        // keyed by its id, which the bus routes when this run carries audio) ...
        assert!(
            pipeline.audio_stores.contains_key("aes67-in"),
            "an aes67 source must contribute an AudioStore (program-bus routed)"
        );
        // ... but contributes NO video TileStore (it decodes no pixels).
        assert!(
            !pipeline.stores.contains_key("aes67-in"),
            "an aes67 source must NOT create a video TileStore (audio-only)"
        );

        // ... and NO SourceRegistry entry — the MP-2 decode-once video seam never
        // sees it, so the registry still holds exactly the ONE video source.
        let aes67_key = multiview_engine::SourceKey::from_canonical("aes67-in");
        assert!(
            pipeline.source_registry.store(&aes67_key).is_none(),
            "an aes67 source must never take a video decode registry entry"
        );
        assert_eq!(
            pipeline.source_registry.active_len(),
            1,
            "only the one video source takes a registry entry (aes67 excluded)"
        );

        // ... and NO layout tile (it is bound to no cell — audio has no geometry).
        assert!(
            cell_pixel_size(&pipeline.layout, "aes67-in").is_none(),
            "an aes67 source is bound to no cell → no layout tile"
        );

        // The video source is UNAFFECTED: it keeps its TileStore + registry entry
        // (the video path is byte-identical to a config without the aes67 source).
        assert!(
            pipeline.stores.contains_key("cam1"),
            "the video source keeps its TileStore (video path byte-identical)"
        );
        let cam_key = multiview_engine::SourceKey::from_canonical("cam1");
        assert!(
            pipeline.source_registry.store(&cam_key).is_some(),
            "the video source keeps its registry entry"
        );
    }

    /// A 2x2 grid where `cam1` is bound by two DIFFERENT-sized cells (area `a` =
    /// 400x480, area `d` = 800x240 on 1200x720 → per-axis supremum 800x480), PLUS
    /// an audio-only aes67 source — proving the aes67 branch never disturbs the
    /// MP-2 decode-once/supremum video seam.
    fn two_cells_plus_aes67_config() -> multiview_config::MultiviewConfig {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 1200
height = 720
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "2fr"]
rows = ["2fr", "1fr"]
areas = ["a b", "c d"]
[[sources]]
id = "cam1"
kind = "bars"
[[sources]]
id = "aes67-in"
kind = "aes67"
multicast = "[ff3e::1]:5004"
sdp = '''
{AES67_SDP}'''
[[cells]]
id = "small"
area = "a"
[cells.source]
input_id = "cam1"
[[cells]]
id = "wide"
area = "d"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "hls"
path = "/tmp/aes67-mixed.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        multiview_config::MultiviewConfig::load_from_toml(&doc).expect("test config parses")
    }

    #[test]
    fn mixed_config_still_decode_once_shares_the_video() {
        let config = two_cells_plus_aes67_config();
        let pipeline = Pipeline::build(&config).expect("pipeline builds (mixed aes67 + video)");
        let cam_key = multiview_engine::SourceKey::from_canonical("cam1");

        // ONE registry entry for the ONE video source, regardless of how many
        // cells bind it — and the audio-only aes67 source adds none.
        assert_eq!(
            pipeline.source_registry.active_len(),
            1,
            "two cells binding one video source (+ an aes67 source) => ONE entry"
        );

        // The decode target the registry records is the per-axis supremum across
        // cam1's two binding cells — unchanged by the aes67 source's presence.
        let expect = cell_pixel_size(&pipeline.layout, "cam1").expect("cam1 is bound");
        assert_eq!(
            pipeline.source_registry.requested_supremum(&cam_key),
            Some(multiview_engine::RequestedSize {
                width: expect.0,
                height: expect.1,
            }),
            "the registry still decodes cam1 at the per-axis supremum (MP-2 intact)"
        );

        // The `stores` map still holds the registry's OWN store Arc (decode-once
        // ownership) — the aes67 branch did not fork it.
        let mapped = pipeline.stores.get("cam1").expect("cam1 has a store");
        let owned = pipeline
            .source_registry
            .store(&cam_key)
            .expect("the registry owns cam1's store");
        assert!(
            std::sync::Arc::ptr_eq(mapped, &owned),
            "the stores map still holds the registry's store Arc (MP-2 decode-once)"
        );

        // The aes67 source remains audio-only: an AudioStore, no video store.
        assert!(pipeline.audio_stores.contains_key("aes67-in"));
        assert!(!pipeline.stores.contains_key("aes67-in"));
    }

    /// A config that (wrongly) binds a layout CELL to the audio-only aes67 source.
    /// An AES67 source decodes no pixels, so a cell referencing it would carry tile
    /// geometry with NO backing `TileStore` (the source loop skips the video path
    /// for it). The build must reject this fail-closed rather than leave a dangling
    /// tile. `cam1` occupies area `a`; `aes67-in` is wrongly bound to area `b`.
    fn aes67_source_bound_to_a_cell_config() -> multiview_config::MultiviewConfig {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 1200
height = 720
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "cam1"
kind = "bars"
[[sources]]
id = "aes67-in"
kind = "aes67"
multicast = "[ff3e::1]:5004"
sdp = '''
{AES67_SDP}'''
[[cells]]
id = "video"
area = "a"
[cells.source]
input_id = "cam1"
[[cells]]
id = "oops-audio"
area = "b"
[cells.source]
input_id = "aes67-in"
[[outputs]]
kind = "hls"
path = "/tmp/aes67-cell-reject.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        multiview_config::MultiviewConfig::load_from_toml(&doc).expect("test config parses")
    }

    #[test]
    fn a_layout_cell_bound_to_an_aes67_source_is_rejected() {
        let config = aes67_source_bound_to_a_cell_config();
        let err = Pipeline::build(&config)
            .err()
            .expect("a layout cell bound to an audio-only aes67 source must fail the build");
        // Fail-closed and honest: the error names the offending audio-only source.
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("aes67-in"),
            "the rejection names the audio-only source wrongly bound to a cell: {rendered}"
        );
    }

    /// Like [`aes67_source_bound_to_a_cell_config`] but the aes67-bound cell
    /// references an UNKNOWN grid area, so its geometry does not resolve. The
    /// audio-binding rejection must still fire (and name the source) rather than be
    /// masked by a generic geometry-resolution error — the guard must key on the
    /// source binding, not on tile geometry (`solve_layout` errors on the bad area
    /// first, so a geometry-coupled guard never sees the cell).
    fn aes67_cell_with_unresolved_geometry_config() -> multiview_config::MultiviewConfig {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 640
height = 480
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
id = "cam1"
kind = "bars"
[[sources]]
id = "aes67-in"
kind = "aes67"
multicast = "[ff3e::1]:5004"
sdp = '''
{AES67_SDP}'''
[[cells]]
id = "video"
area = "a"
[cells.source]
input_id = "cam1"
[[cells]]
id = "oops-audio"
area = "does-not-exist"
[cells.source]
input_id = "aes67-in"
[[outputs]]
kind = "hls"
path = "/tmp/aes67-unresolved-cell.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        multiview_config::MultiviewConfig::load_from_toml(&doc).expect("test config parses")
    }

    #[test]
    fn a_geometry_unresolved_cell_bound_to_an_aes67_source_is_rejected_by_name() {
        let config = aes67_cell_with_unresolved_geometry_config();
        let err = Pipeline::build(&config)
            .err()
            .expect("a cell bound to an audio-only aes67 source must fail the build");
        // The rejection must name the AUDIO-BINDING root cause — not be masked by a
        // generic "unknown grid area" geometry error — so the guard is effective
        // even when the bound cell's geometry does not resolve (decoupled from
        // tile-sizing, which `solve_layout` errors on first).
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("aes67-in"),
            "the rejection names the audio-only source, not just a geometry error: {rendered}"
        );
    }

    #[test]
    fn an_aes67_source_without_program_audio_is_rejected_before_going_on_air() {
        let config = video_plus_aes67_config();
        let pipeline = Pipeline::build(&config).expect("pipeline builds with an aes67 source");
        // No program audio: the ST 2110-30 RX has no `ProgramBus` to publish into,
        // so the run must fail closed rather than silently receive/emit silence.
        assert!(
            pipeline.ensure_aes67_has_program_audio().is_err(),
            "an aes67 source with no program audio is a fail-closed run error"
        );

        let mut with_audio = Pipeline::build(&config).expect("pipeline builds");
        with_audio.enable_program_audio();
        assert!(
            with_audio.ensure_aes67_has_program_audio().is_ok(),
            "with program audio enabled, the aes67 source is accepted"
        );
    }
}

#[cfg(all(test, feature = "aes67"))]
mod aes67_tx_and_helpers {
    //! #103 TX + pure-helper coverage (no socket): `build_aes67_output` maps an
    //! `Output::Aes67` to a mux-free `RunnableOutput::Aes67` + a bake-consumer push
    //! handle (rejecting a bad depth / malformed multicast); `Pipeline::build`
    //! threads that handle onto the pipeline; and the RX bridging helpers
    //! (`resolve_aes67_source`, `interleave_to_stereo`, `aes67_audio_block`) behave
    //! correctly. The socket-bound RX/TX loops are hardware/network-gated.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const AES67_SDP: &str = "v=0\r\n\
o=- 1 1 IN IP6 2001:db8::1\r\n\
s=Multiview AES67\r\n\
c=IN IP6 ff3e::1\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 98\r\n\
a=rtpmap:98 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:AA-BB-CC-DD-EE-FF-00-11:0\r\n\
a=mediaclk:direct=0\r\n";

    /// An `Output::Aes67` value (via serde, bypassing config validation so the
    /// malformed cases reach `build_aes67_output`).
    fn aes67_output_value(multicast: &str, depth: &str) -> Output {
        serde_json::from_value(serde_json::json!({
            "kind": "aes67",
            "label": "aes-out",
            "multicast": multicast,
            "depth": depth,
        }))
        .expect("aes67 output parses")
    }

    /// A `SourceKind::Aes67` source value, optionally carrying the multicast override.
    fn aes67_source_value(multicast: Option<&str>) -> Source {
        let mut map = serde_json::Map::new();
        map.insert("id".to_owned(), serde_json::json!("aes-in"));
        map.insert("kind".to_owned(), serde_json::json!("aes67"));
        map.insert("sdp".to_owned(), serde_json::json!(AES67_SDP));
        if let Some(m) = multicast {
            map.insert("multicast".to_owned(), serde_json::json!(m));
        }
        serde_json::from_value(serde_json::Value::Object(map)).expect("aes67 source parses")
    }

    #[test]
    fn build_aes67_output_makes_a_mux_free_runnable_and_a_push_handle() {
        let output = aes67_output_value("[ff3e::1]:5004", "L24");
        let (runnable, _handle) = build_aes67_output(&output).expect("valid aes67 output builds");
        let RunnableOutput::Aes67 {
            dest, local, id, ..
        } = runnable
        else {
            panic!("expected a RunnableOutput::Aes67");
        };
        assert_eq!(
            dest,
            "[ff3e::1]:5004".parse().unwrap(),
            "dest is the configured multicast group:port"
        );
        assert!(
            local.is_ipv6(),
            "an ipv6 group egresses from an ipv6 wildcard"
        );
        assert!(
            local.ip().is_unspecified(),
            "egress binds the family wildcard"
        );
        assert_eq!(local.port(), 0, "egress uses an ephemeral local port");
        assert_eq!(id, "aes-out", "the id derives from the label when unset");
    }

    #[test]
    fn build_aes67_output_rejects_an_unknown_pcm_depth() {
        let output = aes67_output_value("[ff3e::1]:5004", "L99");
        assert!(
            build_aes67_output(&output).is_err(),
            "an unknown PCM depth is a typed refusal (not a silent default)"
        );
    }

    #[test]
    fn build_aes67_output_rejects_a_malformed_multicast() {
        let output = aes67_output_value("not-a-socket-addr", "L24");
        assert!(
            build_aes67_output(&output).is_err(),
            "a malformed multicast group:port is a typed refusal"
        );
    }

    /// Build an `Output::Aes67` with an explicit id + multicast (via serde).
    fn aes67_output_named(id: &str, multicast: &str) -> Output {
        serde_json::from_value(serde_json::json!({
            "kind": "aes67",
            "id": id,
            "label": id,
            "multicast": multicast,
        }))
        .expect("aes67 output parses")
    }

    #[test]
    fn aes67_outputs_that_collide_on_ssrc_within_one_group_are_rejected() {
        // A PINNED brute-forced collision: at [ff3e::1]:5004 the ids "bzhq" and
        // "fnmw" fold to the SAME 32-bit RTP SSRC (17_309_552). Two senders on ONE
        // multicast group with one SSRC are ambiguous to an RTP receiver (RFC 3550
        // §8), so the build must reject them, naming both. (Found offline by a
        // birthday search over short ascii ids; the search loop is NOT committed.)
        let g: std::net::SocketAddr = "[ff3e::1]:5004".parse().unwrap();
        assert_eq!(
            aes67_ssrc_for("bzhq", g),
            aes67_ssrc_for("fnmw", g),
            "the pinned collision holds under the real fold"
        );

        let err = ensure_no_aes67_ssrc_collision(&[
            aes67_output_named("bzhq", "[ff3e::1]:5004"),
            aes67_output_named("fnmw", "[ff3e::1]:5004"),
        ])
        .expect_err("a same-group ssrc collision is rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("bzhq") && msg.contains("fnmw"),
            "the rejection names BOTH colliding outputs: {msg}"
        );

        // Distinct, non-colliding ids on the same group pass.
        assert!(
            ensure_no_aes67_ssrc_collision(&[
                aes67_output_named("bzhq", "[ff3e::1]:5004"),
                aes67_output_named("cam-audio", "[ff3e::1]:5004"),
            ])
            .is_ok(),
            "distinct non-colliding same-group ids pass"
        );

        // The SAME colliding pair on DIFFERENT multicast groups is harmless — an RTP
        // receiver demuxes per group, so those SSRCs never coexist. Not a conflict.
        assert!(
            ensure_no_aes67_ssrc_collision(&[
                aes67_output_named("bzhq", "[ff3e::1]:5004"),
                aes67_output_named("fnmw", "[ff3e::2]:5004"),
            ])
            .is_ok(),
            "the same collision on different groups is not a conflict"
        );
    }

    #[test]
    fn pipeline_build_rejects_two_aes67_outputs_that_collide_on_ssrc() {
        // The SSRC-collision guard is wired into `build_outputs`, not merely
        // callable: a FULL `Pipeline::build` of a config with the pinned colliding
        // pair on one multicast group fails closed. (Kills the mutant that drops the
        // `ensure_no_aes67_ssrc_collision` call from `build_outputs`.)
        let doc = r##"schema_version = 1
[canvas]
width = 640
height = 480
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
id = "cam1"
kind = "bars"
[[cells]]
id = "only"
area = "a"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "aes67"
id = "bzhq"
label = "bzhq"
multicast = "[ff3e::1]:5004"
[[outputs]]
kind = "aes67"
id = "fnmw"
label = "fnmw"
multicast = "[ff3e::1]:5004"
"##;
        let config = multiview_config::MultiviewConfig::load_from_toml(doc).expect("config parses");
        // `Pipeline` is not `Debug`, so `.err().expect(...)` (not `expect_err`).
        let err = Pipeline::build(&config)
            .err()
            .expect("colliding aes67 outputs fail the build");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("bzhq") && msg.contains("fnmw"),
            "the build rejection names both colliding outputs: {msg}"
        );
    }

    #[test]
    fn aes67_ssrc_is_stable_nonzero_and_folds_in_the_multicast_binding() {
        let g1: std::net::SocketAddr = "[ff3e::1]:5004".parse().unwrap();
        let g1_alt_port: std::net::SocketAddr = "[ff3e::1]:5006".parse().unwrap();
        let g2: std::net::SocketAddr = "[ff3e::2]:5004".parse().unwrap();
        // Never the ambiguous 0.
        assert_ne!(
            aes67_ssrc_for("out-a", g1),
            0,
            "ssrc is never the ambiguous 0"
        );
        // Deterministic for the same (id, group:port).
        assert_eq!(
            aes67_ssrc_for("out-a", g1),
            aes67_ssrc_for("out-a", g1),
            "the same id + binding always advertises the same ssrc"
        );
        // The fold mixes id AND group:port, so different inputs map to different
        // SSRCs with overwhelming probability (a 32-bit space) — NOT a guarantee.
        // These particular inputs differ (a hard same-group guarantee is enforced
        // separately by `ensure_no_aes67_ssrc_collision`, tested above).
        // Distinct output ids on one group differ (here).
        assert_ne!(
            aes67_ssrc_for("out-a", g1),
            aes67_ssrc_for("out-b", g1),
            "these distinct output ids fold to distinct ssrcs"
        );
        // A different port folds in, so these differ.
        assert_ne!(
            aes67_ssrc_for("out-a", g1),
            aes67_ssrc_for("out-a", g1_alt_port),
            "this different port folds to a different ssrc"
        );
        // A different group folds in, so these differ.
        assert_ne!(
            aes67_ssrc_for("out-a", g1),
            aes67_ssrc_for("out-a", g2),
            "this different group folds to a different ssrc"
        );
        // Pinned FNV-1a value over STABLE address bytes (family + raw IP octets +
        // big-endian port), NOT the `SocketAddr` Display string (whose formatting
        // is not a stable cross-version/-target byte encoding). The mapping is a
        // fixed contract — a `DefaultHasher`/SipHash fold, or a Display-string fold,
        // is not stable across releases, so a rebuild could change every sender's
        // advertised SSRC. Changing this constant is a deliberate, reviewed change.
        assert_eq!(
            aes67_ssrc_for("out", g1),
            3_149_784_790,
            "the ssrc is a pinned FNV-1a fold of id + address family/octets/port"
        );
    }

    #[test]
    fn pipeline_build_threads_the_aes67_send_handle_onto_the_pipeline() {
        // A video source + cell (so the layout solves) plus an AES67 audio output:
        // the output's serve-side sender rides `outputs`, its push handle is threaded
        // onto the pipeline for the bake consumer to feed.
        let doc = r##"schema_version = 1
[canvas]
width = 640
height = 480
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
id = "cam1"
kind = "bars"
[[cells]]
id = "only"
area = "a"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "aes67"
label = "aes-out"
multicast = "[ff3e::1]:5004"
"##;
        let config = multiview_config::MultiviewConfig::load_from_toml(doc).expect("config parses");
        let pipeline = Pipeline::build(&config).expect("pipeline builds with an aes67 output");
        assert_eq!(
            pipeline.aes67_send_handles.len(),
            1,
            "the aes67 output's bake-consumer push handle is threaded onto the pipeline"
        );
        assert!(
            pipeline
                .outputs
                .iter()
                .any(|o| matches!(o, RunnableOutput::Aes67 { .. })),
            "the aes67 output is a mux-free runnable sink"
        );
    }

    #[test]
    fn resolve_aes67_source_reads_the_sdp_and_requires_a_multicast_override() {
        // Valid: the SDP parses (format/clock/payload) and the multicast override
        // gives the transport binding (the SDP c= line is deliberately not used).
        let src = aes67_source_value(Some("[ff3e::1]:5004"));
        let (session, group) = resolve_aes67_source(&src).expect("valid aes67 source resolves");
        assert_eq!(session.clock_rate, 48_000);
        assert_eq!(session.payload_type, 98);
        assert_eq!(session.format.channels, 2);
        assert_eq!(group, "[ff3e::1]:5004".parse().unwrap());

        // A missing multicast override is a typed refusal (not derived from the SDP).
        assert!(
            resolve_aes67_source(&aes67_source_value(None)).is_err(),
            "a missing multicast override is rejected (the SDP c= line is not used)"
        );
        // A malformed override is rejected.
        assert!(
            resolve_aes67_source(&aes67_source_value(Some("nope"))).is_err(),
            "a malformed multicast override is rejected"
        );
    }

    #[test]
    fn interleave_to_stereo_maps_mono_stereo_and_multichannel() {
        // Stereo passes through unchanged.
        assert_eq!(
            interleave_to_stereo(&[0.1, 0.2, 0.3, 0.4], 2),
            vec![0.1, 0.2, 0.3, 0.4]
        );
        // Mono duplicates each sample to both L and R.
        assert_eq!(
            interleave_to_stereo(&[0.5, 0.6], 1),
            vec![0.5, 0.5, 0.6, 0.6]
        );
        // >2 channels keep the first two channels per frame (f0: 1,2 ; f1: 5,6).
        assert_eq!(
            interleave_to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], 4),
            vec![1.0, 2.0, 5.0, 6.0]
        );
        // Zero channels clamps to mono — never divides by zero.
        assert_eq!(interleave_to_stereo(&[0.9], 0), vec![0.9, 0.9]);
    }

    #[test]
    fn aes67_audio_block_builds_a_48k_stereo_block() {
        use multiview_input::st2110::v30::{Aes3Format, SampleDepth};
        let frame = multiview_input::st2110::Aes67AudioFrame {
            raw_timestamp: 0,
            ssrc: 42,
            discontinuity: false,
            format: Aes3Format {
                channels: 2,
                depth: SampleDepth::L24,
            },
            samples: vec![0.1, -0.1, 0.2, -0.2], // 2 stereo frames
        };
        let block = aes67_audio_block(&frame).expect("builds a canonical block");
        assert_eq!(block.frame_count(), 2, "two stereo frames");
        assert_eq!(block.format().channel_count(), 2, "canonical stereo");
        assert_eq!(block.format().sample_rate(), 48_000, "canonical 48 kHz");
        assert_eq!(block.interleaved(), &[0.1, -0.1, 0.2, -0.2]);
    }

    /// A well-formed AES67 SDP at an arbitrary L24 clock rate (the parser accepts
    /// 48 kHz and 96 kHz).
    fn aes67_source_value_at_rate(clock_rate: u32) -> Source {
        let sdp = format!(
            "v=0\r\n\
o=- 1 1 IN IP6 2001:db8::1\r\n\
s=Multiview AES67\r\n\
c=IN IP6 ff3e::1\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 98\r\n\
a=rtpmap:98 L24/{clock_rate}/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:AA-BB-CC-DD-EE-FF-00-11:0\r\n\
a=mediaclk:direct=0\r\n"
        );
        let mut map = serde_json::Map::new();
        map.insert("id".to_owned(), serde_json::json!("aes-in"));
        map.insert("kind".to_owned(), serde_json::json!("aes67"));
        map.insert("sdp".to_owned(), serde_json::json!(sdp));
        map.insert("multicast".to_owned(), serde_json::json!("[ff3e::1]:5004"));
        serde_json::from_value(serde_json::Value::Object(map)).expect("aes67 source parses")
    }

    #[test]
    fn resolve_aes67_source_rejects_a_non_48khz_session() {
        // A 48 kHz session resolves (the canonical store rate).
        assert!(
            resolve_aes67_source(&aes67_source_value_at_rate(48_000)).is_ok(),
            "a 48 kHz aes67 session is supported"
        );
        // A 96 kHz session PARSES, but the RX rebaser only rescales the RTP
        // timestamp onto the 48 kHz store index — it does NOT resample the PCM.
        // Publishing 96 samples/ms against a +48/ms store anchor overlaps/gaps the
        // store, so a non-48 kHz session must be rejected fail-closed, not accepted.
        assert!(
            resolve_aes67_source(&aes67_source_value_at_rate(96_000)).is_err(),
            "a non-48 kHz aes67 session is rejected (the RX path does not resample)"
        );
    }

    /// An `Output::Aes67` value with an explicit `ptime_ms` (via serde, bypassing
    /// config validation so degenerate cases reach `build_aes67_output`).
    fn aes67_output_value_with_ptime(ptime_ms: u32) -> Output {
        serde_json::from_value(serde_json::json!({
            "kind": "aes67",
            "label": "aes-out",
            "multicast": "[ff3e::1]:5004",
            "depth": "L24",
            "ptime_ms": ptime_ms,
        }))
        .expect("aes67 output parses")
    }

    #[test]
    fn build_aes67_output_rejects_a_zero_ptime() {
        // A 1 ms ptime builds (48 frames/packet @ 48 kHz).
        assert!(
            build_aes67_output(&aes67_output_value_with_ptime(1)).is_ok(),
            "a 1 ms packet time builds"
        );
        // A zero packet time is nonsensical; the framing must reject it fail-closed
        // rather than silently coerce it to a 1-frame (~0.02 ms) packet flood.
        assert!(
            build_aes67_output(&aes67_output_value_with_ptime(0)).is_err(),
            "a zero packet time is a typed refusal, not a silent coercion to 1 frame"
        );
    }
}
