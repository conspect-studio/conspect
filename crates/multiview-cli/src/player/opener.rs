//! The runtime-load opener of one media player (design 2.2, 3.1–3.3, 6): the
//! single, boot-spawned, parked thread that opens, probes and primes a
//! [`ClipSpec`] **off the ingest loop**, then stages the result through the
//! existing [`LoadCoordinator`] hand-off.
//!
//! # Why a separate thread
//!
//! The player ingest thread must never open a file, probe a container or drop a
//! libav context (design 3.3): those are blocking or unbounded, and the ingest
//! thread feeds a tile the output clock samples (invariants #1, #10). The opener
//! owns all of it. Exactly one opener exists per player, spawned off the ingest
//! thread at boot, and it parks on `thread::park` when idle. `wake` only loads
//! its immutable thread handle from `OnceLock` and unparks it; it cannot wait on
//! the management lock used by `join`. This intentionally changes the original
//! lazy-spawn design: spawning on the ingest thread could block a live tile.
//!
//! # Cancellation and deadlines
//!
//! The open goes through [`Demuxer::open_cancellable`] with a
//! [`CancelToken`] over the coordinator's shared `cancel_gen` and this opener's
//! stop flag (design 3.2). A newer load stores a newer generation, which aborts
//! the in-flight open at the next interrupt-callback poll; the opener also
//! re-checks the generation between every phase and every packet. Open and probe
//! run under [`OpenerConfig::open_timeout`]; prime has a total phase budget
//! [`OpenerConfig::prime_timeout`], checked at packet/frame boundaries, while
//! each libav read uses [`OpenerConfig::read_timeout`]. A final read may overshoot
//! the total budget by at most one read timeout if libav polls its callback.
//! Uncancellable kernel calls have no finite exit promise (design 3.2).
//!
//! # Who drops libav values
//!
//! Only the opener. Every retired path is a call **into** the opener:
//!
//! * a displaced untaken stage is returned by [`LoadCoordinator::stage`] and
//!   dropped here, after the lock is released;
//! * a stale stage is returned by [`LoadCoordinator::sweep_stale`] (the ingest
//!   side's `try_take` never takes or drops it);
//! * the old clip at commit arrives through [`LoadCoordinator::retire`];
//! * the final clip at shutdown arrives through [`LoadCoordinator::put_shutdown`].
//!
//! The opener drains the retire queue and the shutdown slot every time it wakes
//! and before it serves a request, so teardown never runs inline on the ingest
//! thread.
//!
//! # Scope and resource-cost deviation
//!
//! The opener stages its first frame already scaled and tagged for the tile;
//! the ingest commit never scales it. PCM deck staging and executor wiring are
//! separate increments, so until they land this helper is **not** a runtime
//! loader. Design 3.5 proposed lazy opener creation (two threads/player while
//! idle); creating it off the ingest thread at boot adds one parked thread per
//! declared player, at most two under the current config bound. It avoids both
//! a blocking `thread::spawn` and the inherited management mutex on the ingest
//! thread. Lazy creation can return when a non-ingest control-path bootstrap
//! exists; it cannot be moved into the ingest drain without risking cadence.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use multiview_audio::loopdeck::{
    LoopDeck, DEFAULT_CROSSFADE_FRAMES, MAX_LOOP_SECONDS, SAMPLE_RATE,
};
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::time::Rational;
use multiview_ffmpeg::{
    from_ff_rational, AudioFileDecoder, CancelToken, DecodedVideoFrame, DemuxOptions, Demuxer,
    FfmpegError, StreamVideoDecoder,
};
use multiview_framestore::SwapCell;

use super::probe::{resolve_geometry, validate_alpha, validate_cadence, ProbeError, ProbedClip};
use super::{
    player_audio_budget, AudioDeckLease, ClipSpec, Generation, LoadCoordinator, LoadRequest,
    PlayoutGeometry,
};
use crate::audio::canonical_format;
use crate::pipeline::TileScaler;

/// The audio deck and its global budget lease move together at video commit.
/// The lease stays alive on the audio rail until the old deck is dropped.
pub struct StagedAudio {
    /// The complete vamp deck, empty only for a genuinely silent clip.
    pub deck: LoopDeck,
    /// Actual retained PCM bytes charged process-wide; released on Drop.
    pub lease: Option<AudioDeckLease>,
    /// Shared immutable capability survives the complete audio generation.
    pub artifact_lease: Option<super::ImmutableArtifactLease>,
}

/// What the opener owned and produced for one generation (design 2.2 step 3).
///
/// The staged value is moved, never `Arc`ed, because every libav context needs
/// unique ownership. Video and audio travel as one value, so a newer stage can
/// never separate them; the audio deck joins this type in the audio unit
/// (design step 7).
pub struct StagedClip {
    /// Immutable origin resolved before admission; never a public path.
    pub reference: Option<super::SlotReference>,
    /// The opened container, seeked to the clip in-point and primed.
    pub demuxer: Demuxer,
    /// The software video decoder bound to the container's best video stream.
    pub decoder: StreamVideoDecoder,
    /// The container stream index `decoder` consumes.
    pub stream_index: usize,
    /// The validated integer-frame playout window of the clip.
    pub geometry: PlayoutGeometry,
    /// Landing transport intent from the resolved load request.
    pub then: super::LoadThen,
    /// The probed facts the geometry was resolved from.
    pub probed: ProbedClip,
    /// The primed first frame at [`PlayoutGeometry::in_point`], decoded on the
    /// opener; retained for tests and future exact-frame resume.
    pub first_frame: DecodedVideoFrame,
    /// First image already tile-sized, tagged in the canvas colour space;
    /// conversion runs on the opener, never on ingest at commit.
    pub first_image: Arc<Nv12Image>,
    /// The complete, clip-compatible vamp deck. Silence for a clip without audio.
    pub audio: LoopDeck,
    /// Move into the audio thread with `audio`; retained/retired deck bytes
    /// remain charged until that thread drops the old deck.
    pub audio_lease: Option<AudioDeckLease>,
    /// Reusable scaler moved with the decoder, retired with this clip.
    pub(crate) scaler: TileScaler,
    /// Descriptor capability outlives demuxer/decoder destruction (field drop order).
    pub artifact_lease: Option<super::clip::ImmutableArtifactLease>,
}

/// Why a load failed (design section 6 `last_error`). The current clip is never
/// disturbed by a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadFailureReason {
    /// The path does not exist.
    NotFound,
    /// The container or its header could not be parsed.
    InvalidData,
    /// The open or prime deadline expired.
    Timeout,
    /// The clip frame rate is not the canvas cadence.
    WrongCadence,
    /// The clip is variable frame rate.
    Vfr,
    /// The clip declares no frame rate.
    NoCadence,
    /// The clip carries an alpha plane.
    Alpha,
    /// The container has no video stream.
    NoVideoStream,
    /// The clip length is unknown and no out-point was declared.
    UnknownLength,
    /// The resolved playout window is not nested.
    InvalidWindow,
    /// The video decoder could not be opened.
    DecoderOpen,
    /// Priming produced no decodable frame.
    NoFrames,
    /// Any other libav failure.
    Other,
}

/// One reported load failure, tagged with the generation that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadFailure {
    /// The generation whose open failed.
    pub generation: Generation,
    /// The classified reason.
    pub reason: LoadFailureReason,
    /// The underlying detail, for the log line and the status cell.
    pub detail: String,
}

/// The opener's observable state (design section 6). One wait-free snapshot is
/// published per transition; the ingest side and tests read the latest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenerStatus {
    /// The generation currently opening, or [`Generation::NONE`] when parked.
    pub open_in_progress: Generation,
    /// The newest generation successfully staged, or [`Generation::NONE`].
    pub staged: Generation,
    /// The newest generation whose attempt finished (staged, failed or
    /// abandoned).
    pub finished: Generation,
    /// The most recent failure; `None` until the first, and cleared by the next
    /// success.
    pub last_failure: Option<LoadFailure>,
}

/// Timeouts and the canvas cadence the opener validates against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenerConfig {
    /// The canvas frame rate a clip must match exactly (design 4.3).
    pub canvas_cadence: Rational,
    /// Deadline for the open and stream probe (design 3.2).
    pub open_timeout: Duration,
    /// Deadline for the seek, prime and audio decode (design 3.2).
    pub prime_timeout: Duration,
    /// Per-read deadline once the clip is being read (design 3.2).
    pub read_timeout: Duration,
    /// Native tile dimensions; all scaling happens off ingest before staging.
    pub tile_width: u32,
    /// Native tile height in pixels.
    pub tile_height: u32,
    /// Canvas colour information for the primed image.
    pub canvas_color: CanvasColor,
}

impl OpenerConfig {
    /// Shared player per-read and cumulative lap I/O budget (design 3.2).
    pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

    /// Design defaults: 5 s open/probe, 10 s seek/prime, 2 s per read.
    #[must_use]
    pub fn new(canvas_cadence: Rational) -> Self {
        Self {
            canvas_cadence,
            open_timeout: Duration::from_secs(5),
            prime_timeout: Duration::from_secs(10),
            read_timeout: Self::READ_TIMEOUT,
            tile_width: 1920,
            tile_height: 1080,
            canvas_color: CanvasColor::default(),
        }
    }

    /// Configure tile geometry and tag before any clip is staged.
    #[must_use]
    pub const fn with_tile(mut self, width: u32, height: u32, color: CanvasColor) -> Self {
        self.tile_width = width;
        self.tile_height = height;
        self.canvas_color = color;
        self
    }
}

impl Default for OpenerConfig {
    fn default() -> Self {
        Self::new(Rational::FPS_25)
    }
}

/// A load request rejected before it entered the bounded opener queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// The control-plane bootstrap did not start the opener successfully.
    Unstarted,
    /// The player is stopping and will not accept another load.
    Stopped,
}

/// One player's opener: a boot-spawned parked thread, managed off-ingest.
/// The ingest path only accesses atomics, bounded cells and `Thread::unpark`.
pub struct PlayerOpener {
    id: String,
    coordinator: Arc<LoadCoordinator<StagedClip>>,
    stop: Arc<AtomicBool>,
    config: OpenerConfig,
    status: Arc<SwapCell<Arc<OpenerStatus>>>,
    spawns: AtomicUsize,
    /// Published exactly once at startup. Ingest never acquires the join lock.
    thread: OnceLock<Thread>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl PlayerOpener {
    /// Build an opener for `id` that shares `coordinator` with the ingest side.
    /// Call [`Self::start`] off-ingest before accepting any requests.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        coordinator: Arc<LoadCoordinator<StagedClip>>,
        config: OpenerConfig,
    ) -> Self {
        Self {
            id: id.into(),
            coordinator,
            stop: Arc::new(AtomicBool::new(false)),
            config,
            status: Arc::new(SwapCell::from_pointee(OpenerStatus::default())),
            spawns: AtomicUsize::new(0),
            thread: OnceLock::new(),
            join: Mutex::new(None),
        }
    }

    /// The opener's stop flag, so the process stop registry can share it
    /// (`{id}/player-open`, design 3.5).
    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Ingest side: submit `spec` and wake the already-started opener. No locks,
    /// spawning, file IO, or libav teardown on this path. A boot-start failure
    /// returns a typed refusal rather than panicking on the ingest thread.
    ///
    /// # Errors
    ///
    /// Returns `Unstarted` or `Stopped` without queueing any work.
    pub fn request(&self, spec: ClipSpec) -> Result<Generation, RequestError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(RequestError::Stopped);
        }
        if self.thread.get().is_none() {
            return Err(RequestError::Unstarted);
        }
        let generation = self.coordinator.submit(spec);
        self.wake();
        Ok(generation)
    }

    /// Wake the parked opener after a request, retirement or shutdown handoff.
    /// This never takes the management mutex or waits for the worker.
    pub fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    /// Set the stop flag and wake the opener. The opener exits when its current
    /// libav call returns; there is no join here (design 3.5).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    /// The latest published opener state.
    #[must_use]
    pub fn status(&self) -> OpenerStatus {
        (*self.status.load()).clone()
    }

    /// Whether the opener thread has been spawned.
    #[must_use]
    pub fn is_spawned(&self) -> bool {
        self.spawns.load(Ordering::Acquire) > 0
    }

    /// How many times the opener thread was spawned (always `0` or `1`; a
    /// diagnostic that pins "one opener per player").
    #[must_use]
    pub fn spawn_count(&self) -> usize {
        self.spawns.load(Ordering::Acquire)
    }

    /// Stop and join the opener. Returns the thread's panic if it panicked.
    ///
    /// # Errors
    ///
    /// Returns the join error if the opener thread panicked.
    pub fn join(self) -> thread::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.wake();
        let mut join = self.join.lock().unwrap_or_else(PoisonError::into_inner);
        match join.take() {
            Some(join) => join.join(),
            None => Ok(()),
        }
    }

    /// Spawn exactly once, from boot/control setup and never on the ingest loop.
    ///
    /// # Errors
    ///
    /// Returns the spawn error; the caller must refuse the player load rather
    /// than silently accept a request with no opener able to service it.
    pub fn start(&self) -> std::io::Result<()> {
        let mut slot = self.join.lock().unwrap_or_else(PoisonError::into_inner);
        if self.thread.get().is_some() {
            return Ok(());
        }
        let coordinator = Arc::clone(&self.coordinator);
        let stop = Arc::clone(&self.stop);
        let status = Arc::clone(&self.status);
        let config = self.config;
        let name = format!("multiview-player-open-{}", self.id);
        let join = thread::Builder::new()
            .name(name)
            .spawn(move || open_loop(&coordinator, &stop, &status, config))?;
        // Publish the handle before the ingest producer can receive any load.
        if self.thread.set(join.thread().clone()).is_err() {
            // A second publish is impossible while `join` is locked and the
            // initial guard observed an empty cell. Do not panic at the FFI
            // boundary or silently leave an untracked worker if violated.
            self.stop.store(true, Ordering::Release);
            join.thread().unpark();
            *slot = Some(join);
            return Err(std::io::Error::other("player opener already started"));
        }
        self.spawns.fetch_add(1, Ordering::AcqRel);
        *slot = Some(join);
        Ok(())
    }
}

impl Drop for PlayerOpener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }
}

/// Why an open attempt produced no staged clip without being a failure.
enum OpenAbort {
    /// The request was superseded by a newer generation or the player is
    /// stopping: the opener drops the work and reports nothing.
    Abandoned,
    /// A real failure: reported in [`OpenerStatus::last_failure`].
    Failed {
        /// The classified reason.
        reason: LoadFailureReason,
        /// The underlying detail.
        detail: String,
    },
}

/// The opener's loop: drain retirement, take one request, open/probe/prime,
/// stage, repeat. Parks when there is no request and exits on `stop`.
fn open_loop(
    coordinator: &LoadCoordinator<StagedClip>,
    stop: &Arc<AtomicBool>,
    status: &SwapCell<Arc<OpenerStatus>>,
    config: OpenerConfig,
) {
    let mut state = OpenerStatus::default();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let observed = coordinator.latest();
        drain_retired(coordinator);
        drain_shutdown(coordinator);
        if let Some(stale) = coordinator.sweep_stale() {
            drop(stale);
        }
        let Some(request) = coordinator.take_request() else {
            if observed == coordinator.latest() {
                coordinator.acknowledge_quiescence(observed);
            }
            if state.open_in_progress != Generation::NONE {
                state.open_in_progress = Generation::NONE;
                publish(status, &state);
            }
            thread::park();
            continue;
        };
        state.open_in_progress = request.generation;
        publish(status, &state);
        match open_one(&request, coordinator, stop, &config) {
            Ok(clip) => {
                if stop.load(Ordering::Acquire) || coordinator.is_superseded(request.generation) {
                    drop(clip);
                } else {
                    if let Some(displaced) = coordinator.stage(request.generation, clip) {
                        drop(displaced);
                    }
                    state.staged = request.generation;
                    state.last_failure = None;
                }
            }
            Err(OpenAbort::Abandoned) => {}
            Err(OpenAbort::Failed { reason, detail }) => {
                state.last_failure = Some(LoadFailure {
                    generation: request.generation,
                    reason,
                    detail,
                });
            }
        }
        // A newer request may supersede the attempt after a libav error but
        // before status publication. Never report an obsolete failure as the
        // current load's failure.
        if coordinator.is_superseded(request.generation)
            && state
                .last_failure
                .as_ref()
                .is_some_and(|f| f.generation == request.generation)
        {
            state.last_failure = None;
        }
        state.finished = state.finished.max(request.generation);
        state.open_in_progress = Generation::NONE;
        publish(status, &state);
    }
    drain_retired(coordinator);
    drain_shutdown(coordinator);
    if let Some(stale) = coordinator.sweep_stale() {
        drop(stale);
    }
}

/// Publish one status snapshot (wait-free `SwapCell` store, operator rate).
fn publish(cell: &SwapCell<Arc<OpenerStatus>>, state: &OpenerStatus) {
    cell.store(Arc::new(state.clone()));
}

/// Drop every retired clip on the calling (opener) thread. Returns the count.
fn drain_retired(coordinator: &LoadCoordinator<StagedClip>) -> usize {
    let mut drained = 0;
    while let Some(clip) = coordinator.pop_retired() {
        drop(clip);
        coordinator.acknowledge_retired();
        drained += 1;
    }
    drained
}

/// Drop the shutdown clip, if the ingest side handed one over. Returns the count.
fn drain_shutdown(coordinator: &LoadCoordinator<StagedClip>) -> usize {
    match coordinator.take_shutdown_bundle() {
        Some(bundle) => {
            let mut count = 0;
            for clip in bundle.into_iter().flatten() {
                drop(clip);
                count += 1;
            }
            count
        }
        None => 0,
    }
}

/// Open, probe, validate, build the decoder and prime the first frame, all on
/// the opener thread.
fn open_one(
    request: &LoadRequest,
    coordinator: &LoadCoordinator<StagedClip>,
    stop: &Arc<AtomicBool>,
    config: &OpenerConfig,
) -> Result<StagedClip, OpenAbort> {
    let generation = request.generation;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    let token = CancelToken::new(coordinator.cancel_gen(), generation.0, Arc::clone(stop));
    let options = DemuxOptions::new()
        .with_open_timeout(config.open_timeout)
        .with_rw_timeout(config.read_timeout);
    let mut demuxer = Demuxer::open_cancellable(&request.spec.path, options, token)
        .map_err(|error| classify_open_error(&request.spec.path, &error))?;
    let facts = probe_facts(&demuxer)?;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    validate_alpha(&facts.pix_fmt, facts.has_alpha)
        .map_err(|error| failed(probe_reason(&error), error))?;
    let cadence = validate_cadence(
        config.canvas_cadence,
        facts.avg_frame_rate,
        facts.nominal_frame_rate,
    )
    .map_err(|error| failed(probe_reason(&error), error))?;
    let probed = ProbedClip {
        codec: facts.codec,
        width: facts.width,
        height: facts.height,
        cadence,
        frames: facts.frames,
        has_audio: facts.has_audio,
        has_alpha: facts.has_alpha,
        pix_fmt: facts.pix_fmt,
    };
    let geometry = resolve_geometry(&request.spec, &probed, config.canvas_cadence)
        .map_err(|error| failed(probe_reason(&error), error))?;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    let mut decoder = StreamVideoDecoder::new(facts.parameters, facts.time_base)
        .map_err(|error| failed(LoadFailureReason::DecoderOpen, error))?
        .with_declared_fps(Some(cadence));
    demuxer.arm_deadline(config.prime_timeout);
    let primed = prime(
        &mut demuxer,
        &mut decoder,
        facts.stream_index,
        geometry.in_point(),
        config.prime_timeout,
        generation,
        coordinator,
        stop,
    );
    demuxer.disarm_deadline();
    let first_frame = primed?;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    let mut scaler = TileScaler::new(config.tile_width, config.tile_height);
    let first_image = Arc::new(
        scaler
            .convert(&first_frame.frame, config.canvas_color.output_tag())
            .map_err(|error| failed(LoadFailureReason::InvalidData, error))?,
    );
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    let (audio, audio_lease) = stage_audio(request, &probed, geometry, config, coordinator, stop)?;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    // Clear the generation check before staging: a later load must never abort
    // reads of a committed clip (design 3.2).
    demuxer.disarm_cancel();
    Ok(StagedClip {
        reference: request.spec.reference.clone().or_else(|| {
            request
                .spec
                .asset_id
                .clone()
                .map(|id| super::SlotReference::Asset { id })
        }),
        demuxer,
        decoder,
        stream_index: facts.stream_index,
        geometry,
        then: request.spec.then,
        probed,
        first_frame,
        first_image,
        audio,
        audio_lease,
        scaler,
        artifact_lease: request.spec.artifact_lease.clone(),
    })
}

/// Exact rounded sample index of an integer-frame timestamp (48 kHz), using
/// integer arithmetic identical to the existing audio deck mapping.
fn audio_sample_of(frames: u64, cadence: Rational) -> u64 {
    let (Ok(num), Ok(den)) = (u64::try_from(cadence.num), u64::try_from(cadence.den)) else {
        return 0;
    };
    if num == 0 || den == 0 {
        return 0;
    }
    let numer = u128::from(frames)
        .saturating_mul(u128::from(SAMPLE_RATE))
        .saturating_mul(u128::from(den));
    let denom = u128::from(num);
    u64::try_from(numer.saturating_add(denom / 2) / denom).unwrap_or(u64::MAX)
}

const MAX_STAGED_AUDIO_BYTES: usize = 256 * 1024 * 1024;

/// Decode an advertised audio stream into a complete, bounded vamp deck before
/// staging video. Failure cannot commit video with stale audio. A silent clip
/// carries an empty deck; a clip that advertises audio must yield its full window.
#[allow(clippy::too_many_lines)] // One bounded admission/decode/deck transaction; split only with a clear ownership boundary.
fn stage_audio(
    request: &LoadRequest,
    probed: &ProbedClip,
    geometry: PlayoutGeometry,
    config: &OpenerConfig,
    coordinator: &LoadCoordinator<StagedClip>,
    stop: &Arc<AtomicBool>,
) -> Result<(LoopDeck, Option<AudioDeckLease>), OpenAbort> {
    let format = canonical_format();
    if !probed.has_audio {
        return Ok((LoopDeck::empty(format), None));
    }
    let body = audio_sample_of(geometry.vamp_out() - geometry.vamp_in(), geometry.cadence());
    if body == 0 || body > MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE) {
        return Err(failed(
            LoadFailureReason::InvalidWindow,
            "audio vamp exceeds the 600 s cap",
        ));
    }
    let in_sample = usize::try_from(audio_sample_of(geometry.vamp_in(), geometry.cadence()))
        .map_err(|error| failed(LoadFailureReason::InvalidWindow, error))?;
    let end_sample = usize::try_from(audio_sample_of(geometry.vamp_out(), geometry.cadence()))
        .map_err(|error| failed(LoadFailureReason::InvalidWindow, error))?
        .saturating_add(DEFAULT_CROSSFADE_FRAMES);
    let loop_len =
        usize::try_from(body).map_err(|error| failed(LoadFailureReason::InvalidWindow, error))?;
    let generation = request.generation;
    let channels = format.channel_count();
    let capacity = end_sample
        .checked_sub(in_sample)
        .and_then(|frames| frames.checked_mul(channels))
        .ok_or_else(|| {
            failed(
                LoadFailureReason::InvalidWindow,
                "audio window size overflow",
            )
        })?;
    // This cap is an admission bound, not a reservation. The existing deck and
    // the new deck coexist until the audio thread adopts, and both count.
    // Memory-slot global accounting will supply a lower per-slot cap in step 6.
    let bytes = capacity
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| failed(LoadFailureReason::InvalidWindow, "audio byte size overflow"))?;
    if bytes > MAX_STAGED_AUDIO_BYTES {
        return Err(failed(
            LoadFailureReason::InvalidWindow,
            format!("audio prime {bytes} bytes exceeds {MAX_STAGED_AUDIO_BYTES}-byte deck limit"),
        ));
    }
    // LoopDeck::with_segment copies the body and builds a seam; charge the
    // temporary window plus a full body copy plus a worst-case seam copy.
    let transient = bytes.checked_mul(3).ok_or_else(|| {
        failed(
            LoadFailureReason::InvalidWindow,
            "audio transient charge overflow",
        )
    })?;
    let mut lease =
        player_audio_budget()
            .reserve(transient)
            .map_err(|(requested, used, cap)| {
                failed(
                    LoadFailureReason::InvalidWindow,
                    format!(
                        "player PCM budget refused {requested} bytes: {used} used of {cap} cap"
                    ),
                )
            })?;
    let token = CancelToken::new(coordinator.cancel_gen(), generation.0, Arc::clone(stop));
    let options = DemuxOptions::new()
        .with_open_timeout(config.open_timeout)
        .with_rw_timeout(config.read_timeout);
    let mut decoder = AudioFileDecoder::open_cancellable(
        &request.spec.path,
        u32::try_from(SAMPLE_RATE).unwrap_or(48_000),
        2,
        options,
        token,
    )
    .map_err(|error| classify_open_error(&request.spec.path, &error))?;
    let started = Instant::now();
    let mut window = Vec::with_capacity(capacity);
    let mut cursor = 0usize;
    while cursor < end_sample {
        check_prime_budget(started.elapsed(), config.prime_timeout)?;
        if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
            return Err(OpenAbort::Abandoned);
        }
        let block = decoder
            .next_block()
            .map_err(|error| classify_read_error(&error))?;
        let Some(block) = block else {
            break;
        };
        let count = block.frame_count();
        let end = cursor.saturating_add(count);
        let from = cursor.max(in_sample);
        let to = end.min(end_sample);
        if from < to {
            let left = (from - cursor).saturating_mul(channels);
            let right = (to - cursor).saturating_mul(channels);
            let segment = block.interleaved.get(left..right).ok_or_else(|| {
                failed(LoadFailureReason::InvalidData, "ragged decoded audio block")
            })?;
            window.extend_from_slice(segment);
        }
        cursor = end;
    }
    check_prime_budget(started.elapsed(), config.prime_timeout)?;
    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
        return Err(OpenAbort::Abandoned);
    }
    let deck = LoopDeck::from_window(format, &window, loop_len, DEFAULT_CROSSFADE_FRAMES);
    if deck.loop_frames() == 0 {
        return Err(failed(
            LoadFailureReason::InvalidData,
            "advertised audio vamp window is incomplete",
        ));
    }
    decoder.disarm_cancel();
    let retained = deck.allocated_bytes();
    // The transient window is no longer needed once the deck owns its body and
    // seam. Report only the actual allocated Vec capacities after this point.
    drop(window);
    lease
        .shrink_to(retained)
        .map_err(|error| failed(LoadFailureReason::InvalidWindow, error))?;
    Ok((deck, Some(lease)))
}

/// The owned stream facts a probe established (nothing borrows the demuxer).
struct VideoFacts {
    stream_index: usize,
    parameters: ffmpeg::codec::Parameters,
    time_base: Rational,
    avg_frame_rate: Option<Rational>,
    nominal_frame_rate: Option<Rational>,
    codec: String,
    width: u32,
    height: u32,
    frames: Option<u64>,
    has_audio: bool,
    has_alpha: bool,
    pix_fmt: String,
}

/// Read the best video stream's facts from an open container.
fn probe_facts(demuxer: &Demuxer) -> Result<VideoFacts, OpenAbort> {
    let input = demuxer.input();
    let Some(stream) = input.streams().best(ffmpeg::media::Type::Video) else {
        return Err(failed(
            LoadFailureReason::NoVideoStream,
            "container has no video stream".to_owned(),
        ));
    };
    let stream_index = stream.index();
    let parameters = stream.parameters();
    let time_base = from_ff_rational(stream.time_base());
    let avg_frame_rate = rate_opt(stream.avg_frame_rate());
    let nominal_frame_rate = rate_opt(stream.rate());
    let frames = u64::try_from(stream.frames())
        .ok()
        .filter(|frames| *frames > 0);
    let has_audio = input
        .streams()
        .any(|stream| stream.parameters().medium() == ffmpeg::media::Type::Audio);
    let codec = format!("{:?}", parameters.id());
    let context = ffmpeg::codec::context::Context::from_parameters(parameters.clone())
        .map_err(|error| failed(LoadFailureReason::DecoderOpen, error))?;
    let video = context
        .decoder()
        .video()
        .map_err(|error| failed(LoadFailureReason::DecoderOpen, error))?;
    let width = video.width();
    let height = video.height();
    let pixel = video.format();
    let pix_fmt = pixel
        .descriptor()
        .map_or_else(|| format!("{pixel:?}"), |desc| desc.name().to_owned());
    let has_alpha = pixel
        .descriptor()
        .is_some_and(|desc| desc.nb_components() >= 4);
    Ok(VideoFacts {
        stream_index,
        parameters,
        time_base,
        avg_frame_rate,
        nominal_frame_rate,
        codec,
        width,
        height,
        frames,
        has_audio,
        has_alpha,
        pix_fmt,
    })
}

/// Seek to the in-point (when it is not the first frame) and decode forward to
/// it, returning the frame at the in-point. Checks the generation and `stop`
/// between every packet.
#[allow(clippy::too_many_arguments)] // Demux, decode, budget and cancellation owners must remain explicit.
fn prime(
    demuxer: &mut Demuxer,
    decoder: &mut StreamVideoDecoder,
    stream_index: usize,
    in_point: u64,
    phase_timeout: Duration,
    generation: Generation,
    coordinator: &LoadCoordinator<StagedClip>,
    stop: &Arc<AtomicBool>,
) -> Result<DecodedVideoFrame, OpenAbort> {
    // Decode from the beginning for exact integer-frame positioning. Seeking to
    // a timestamp may land on a preceding keyframe, making a plain `in_point`
    // discard from that position silently choose the wrong frame. This bounded
    // prime rejects an in-point beyond the declared clip window at probe.
    let mut discarded: u64 = 0;
    let phase_started = Instant::now();
    loop {
        check_prime_budget(phase_started.elapsed(), phase_timeout)?;
        if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
            return Err(OpenAbort::Abandoned);
        }
        // Demuxer::read_packet_for owns its per-read timeout. It overrides the
        // callback deadline, so the total budget is checked at each boundary.
        // A single libav read remains bounded by `read_timeout` when it polls.
        let packet = match demuxer.read_packet_for(stream_index) {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => return Err(classify_read_error(&error)),
        };
        decoder
            .send_packet(&packet.packet)
            .map_err(|error| failed(LoadFailureReason::InvalidData, error))?;
        loop {
            match decoder.receive_frame() {
                Ok(Some(frame)) => {
                    check_prime_budget(phase_started.elapsed(), phase_timeout)?;
                    if stop.load(Ordering::Acquire) || coordinator.is_superseded(generation) {
                        return Err(OpenAbort::Abandoned);
                    }
                    if discarded >= in_point {
                        return Ok(frame);
                    }
                    discarded += 1;
                }
                Ok(None) => break,
                Err(error) => return Err(failed(LoadFailureReason::InvalidData, error)),
            }
        }
    }
    check_prime_budget(phase_started.elapsed(), phase_timeout)?;
    Err(failed(
        LoadFailureReason::NoFrames,
        format!("no decodable frame at in-point {in_point}"),
    ))
}

/// A pure, injectably tested total prime budget. The native per-read deadline
/// can overshoot this boundary by at most one read timeout when libav polls its
/// callback; an uncancellable kernel read has no guaranteed exit deadline.
fn check_prime_budget(elapsed: Duration, budget: Duration) -> Result<(), OpenAbort> {
    if elapsed >= budget {
        Err(failed(
            LoadFailureReason::Timeout,
            format!("clip prime exceeded total phase budget {budget:?}"),
        ))
    } else {
        Ok(())
    }
}

/// Classify an open failure. A missing path is `not_found`; an existing path
/// that libav cannot parse is `invalid_data` (design section 8).
fn classify_open_error(path: &Path, error: &FfmpegError) -> OpenAbort {
    match error {
        FfmpegError::Cancelled { .. } => OpenAbort::Abandoned,
        FfmpegError::ReadTimeout { .. } => failed(LoadFailureReason::Timeout, error.to_string()),
        FfmpegError::OpenInput { .. } if path.exists() => {
            failed(LoadFailureReason::InvalidData, error.to_string())
        }
        FfmpegError::OpenInput { .. } => failed(LoadFailureReason::NotFound, error.to_string()),
        _ => failed(LoadFailureReason::Other, error.to_string()),
    }
}

/// Classify a read failure during priming.
fn classify_read_error(error: &FfmpegError) -> OpenAbort {
    match error {
        FfmpegError::Cancelled { .. } => OpenAbort::Abandoned,
        FfmpegError::ReadTimeout { .. } => failed(LoadFailureReason::Timeout, error.to_string()),
        _ => failed(LoadFailureReason::InvalidData, error.to_string()),
    }
}

/// Build a failure abort with a displayable detail.
#[allow(clippy::needless_pass_by_value)] // Own displayable details from both static literals and temporary errors.
fn failed(reason: LoadFailureReason, detail: impl ToString) -> OpenAbort {
    OpenAbort::Failed {
        reason,
        detail: detail.to_string(),
    }
}

/// Map a pure probe refusal onto the reported reason.
fn probe_reason(error: &ProbeError) -> LoadFailureReason {
    match error {
        ProbeError::Cadence { .. } => LoadFailureReason::WrongCadence,
        ProbeError::NoCadence => LoadFailureReason::NoCadence,
        ProbeError::Vfr { .. } => LoadFailureReason::Vfr,
        ProbeError::Alpha { .. } => LoadFailureReason::Alpha,
        ProbeError::NoVideo => LoadFailureReason::NoVideoStream,
        ProbeError::UnknownLength => LoadFailureReason::UnknownLength,
        ProbeError::Window(_) => LoadFailureReason::InvalidWindow,
    }
}

/// A declared frame rate as an exact rational, or `None` when libav reports a
/// zero/absent one.
fn rate_opt(rate: ffmpeg::Rational) -> Option<Rational> {
    let value = from_ff_rational(rate);
    (value.num > 0 && value.is_valid()).then_some(value)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::path::PathBuf;
    use std::time::Instant;

    use multiview_ffmpeg::test_fixtures::{
        generate_player_clip, write_corrupt_clip, PlayerClipSpec,
    };

    use super::super::{AudioCommitSlot, LoadThen, Staged};
    use super::*;

    /// The fixture cadence, matching every clip these tests generate.
    const CADENCE: Rational = Rational::new(25, 1);

    /// A hang guard only: the pass condition is always the observed event.
    const HANG_GUARD: Duration = Duration::from_secs(30);

    fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < HANG_GUARD,
                "hang guard: {what} was not observed within {HANG_GUARD:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// One opener plus its shared coordinator and scratch directory.
    struct Fixture {
        opener: PlayerOpener,
        coordinator: Arc<LoadCoordinator<StagedClip>>,
        audio: AudioCommitSlot<()>,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new(canvas: Rational) -> Self {
            let coordinator = Arc::new(LoadCoordinator::new());
            let opener = PlayerOpener::new(
                "player-1",
                Arc::clone(&coordinator),
                OpenerConfig::new(canvas),
            );
            opener.start().expect("spawn opener off ingest");
            Self {
                opener,
                coordinator,
                audio: AudioCommitSlot::new(),
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn clip(&self, name: &str, frames: u32, luma: u8) -> PathBuf {
            let path = self.dir.path().join(format!("{name}.mov"));
            generate_player_clip(&path, &PlayerClipSpec::ffv1(CADENCE, frames, luma)).unwrap();
            path
        }

        fn spec(&self, name: &str, frames: u32, luma: u8) -> ClipSpec {
            ClipSpec::new(self.clip(name, frames, luma), LoadThen::Cue)
        }

        fn wait_staged(&self, generation: Generation) {
            wait_for("the generation to stage", || {
                self.opener.status().staged >= generation
            });
        }

        fn wait_finished(&self, generation: Generation) {
            wait_for("the generation to finish", || {
                self.opener.status().finished >= generation
            });
        }

        fn take_newest(&self) -> Staged<StagedClip> {
            self.coordinator
                .try_take(&self.audio)
                .expect("the newest generation must be takeable")
        }

        fn finish(self) {
            self.opener.join().expect("the opener thread stops");
        }
    }

    #[test]
    fn opener_is_started_before_the_first_request_and_spawns_only_once() {
        let fixture = Fixture::new(CADENCE);
        assert!(fixture.opener.is_spawned(), "boot starts opener off ingest");
        assert_eq!(fixture.opener.spawn_count(), 1);
        fixture.opener.start().expect("start is idempotent");
        let first = fixture.opener.request(fixture.spec("a", 4, 10)).unwrap();
        fixture.wait_staged(first);
        assert!(fixture.opener.is_spawned());
        assert_eq!(fixture.opener.spawn_count(), 1);
        let second = fixture.opener.request(fixture.spec("b", 6, 20)).unwrap();
        fixture.wait_staged(second);
        assert_eq!(fixture.opener.spawn_count(), 1, "one opener per player");
        fixture.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn immutable_artifact_fd_is_opened_natively_and_pinned_through_retirement() {
        use multiview_webengine::artifact::{ArtifactStore, Limits, Provenance};
        use std::io::Write;
        let fixture = Fixture::new(CADENCE);
        let source = fixture.clip("immutable", 4, 10);
        let bytes = std::fs::read(source).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut store = ArtifactStore::open(
            root.path(),
            Limits {
                bytes: 1000000,
                count: 100,
            },
        )
        .unwrap();
        let mut writer = store
            .begin(
                Provenance {
                    owner: "o".into(),
                    fill: "f".into(),
                    job: "j".into(),
                    generation: 1,
                },
                bytes.len() as u64,
                |_| true,
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
        let artifact = writer.complete().unwrap();
        let spec = ClipSpec::from_artifact(
            store.pin(&artifact.id, "o", |_| true).unwrap(),
            LoadThen::Cue,
        );
        let generation = fixture.opener.request(spec).unwrap();
        fixture.wait_staged(generation);
        let staged = fixture.take_newest();
        assert!(staged.clip.artifact_lease.is_some());
        assert_eq!(staged.clip.first_frame.frame.width(), 64);
        assert!(store.remove(&artifact.id, "o", |_| true).is_err());
        drop(staged);
        fixture.finish();
        store.remove(&artifact.id, "o", |_| true).unwrap();
    }

    #[test]
    fn open_good_clip_stages_the_probed_primed_clip() {
        let fixture = Fixture::new(CADENCE);
        let generation = fixture.opener.request(fixture.spec("good", 4, 10)).unwrap();
        fixture.wait_staged(generation);
        let status = fixture.opener.status();
        assert_eq!(status.staged, generation);
        assert_eq!(status.last_failure, None);
        let staged = fixture.take_newest();
        assert_eq!(staged.generation, generation);
        let clip = staged.clip;
        assert_eq!(clip.probed.cadence, CADENCE);
        assert_eq!((clip.probed.width, clip.probed.height), (64, 48));
        assert_eq!(clip.probed.frames, Some(4));
        assert!(!clip.probed.has_audio);
        assert!(!clip.probed.has_alpha);
        assert_eq!(clip.stream_index, 0);
        assert_eq!(clip.geometry.in_point(), 0);
        assert_eq!(clip.geometry.out_point(), 4);
        assert_eq!(clip.geometry.cadence(), CADENCE);
        assert_eq!(clip.first_frame.frame.width(), 64);
        assert_eq!(clip.first_frame.frame.height(), 48);
        assert_eq!(clip.first_image.width(), 1920);
        assert_eq!(clip.first_image.height(), 1080);
        fixture.finish();
    }

    #[test]
    fn advertised_audio_is_staged_with_the_video_and_charged_until_drop() {
        let fixture = Fixture::new(CADENCE);
        let path = fixture.dir.path().join("audio.mov");
        let mut spec = PlayerClipSpec::ffv1(CADENCE, 25, 30);
        spec.audio_level = Some(0.25);
        generate_player_clip(&path, &spec).unwrap();
        let before = player_audio_budget().used();
        let gen = fixture
            .opener
            .request(ClipSpec::new(path, LoadThen::Cue))
            .unwrap();
        fixture.wait_staged(gen);
        let staged = fixture.take_newest();
        assert!(staged.clip.probed.has_audio);
        assert_eq!(staged.clip.audio.loop_frames(), 48_000);
        let charged = staged.clip.audio_lease.as_ref().unwrap().bytes();
        assert_eq!(charged, staged.clip.audio.allocated_bytes());
        assert!(charged > 0);
        drop(staged);
        assert_eq!(player_audio_budget().used(), before);
        fixture.finish();
    }

    #[test]
    fn wrong_cadence_is_rejected_and_nothing_stages() {
        let fixture = Fixture::new(Rational::new(50, 1));
        let generation = fixture.opener.request(fixture.spec("pal", 4, 10)).unwrap();
        fixture.wait_finished(generation);
        let status = fixture.opener.status();
        assert_eq!(status.staged, Generation::NONE);
        let failure = status.last_failure.expect("a wrong-cadence load fails");
        assert_eq!(failure.generation, generation);
        assert_eq!(failure.reason, LoadFailureReason::WrongCadence);
        assert!(failure.detail.contains("25/1"), "{}", failure.detail);
        assert!(failure.detail.contains("50/1"), "{}", failure.detail);
        assert!(
            fixture.coordinator.try_take(&fixture.audio).is_none(),
            "nothing was staged"
        );
        fixture.finish();
    }

    #[test]
    fn corrupt_file_is_rejected_and_the_accepted_clip_is_untouched() {
        let fixture = Fixture::new(CADENCE);
        let accepted_gen = fixture.opener.request(fixture.spec("good", 4, 10)).unwrap();
        fixture.wait_staged(accepted_gen);
        let accepted = fixture.take_newest();
        let corrupt = fixture.dir.path().join("corrupt.mov");
        write_corrupt_clip(&corrupt, 4096).unwrap();
        let bad_gen = fixture
            .opener
            .request(ClipSpec::new(corrupt, LoadThen::Cue))
            .unwrap();
        fixture.wait_finished(bad_gen);
        let status = fixture.opener.status();
        assert_eq!(status.staged, accepted_gen, "no new clip staged");
        let failure = status.last_failure.expect("a corrupt load fails");
        assert_eq!(failure.generation, bad_gen);
        assert_eq!(failure.reason, LoadFailureReason::InvalidData);
        assert!(
            fixture.coordinator.try_take(&fixture.audio).is_none(),
            "the failed open staged nothing"
        );
        assert_eq!(accepted.generation, accepted_gen, "accepted clip untouched");
        assert_eq!(accepted.clip.probed.frames, Some(4));
        fixture.finish();
    }

    #[test]
    fn missing_file_reports_not_found_and_keeps_the_accepted_clip() {
        let fixture = Fixture::new(CADENCE);
        let accepted_gen = fixture.opener.request(fixture.spec("good", 4, 10)).unwrap();
        fixture.wait_staged(accepted_gen);
        let accepted = fixture.take_newest();
        let missing_gen = fixture
            .opener
            .request(ClipSpec::new(
                fixture.dir.path().join("absent.mov"),
                LoadThen::Cue,
            ))
            .unwrap();
        fixture.wait_finished(missing_gen);
        let failure = fixture
            .opener
            .status()
            .last_failure
            .expect("a missing file fails");
        assert_eq!(failure.generation, missing_gen);
        assert_eq!(failure.reason, LoadFailureReason::NotFound);
        assert_eq!(accepted.generation, accepted_gen);
        assert_eq!(accepted.clip.geometry.out_point(), 4);
        fixture.finish();
    }

    #[test]
    fn a_superseded_generation_never_commits_and_the_newest_stages() {
        let fixture = Fixture::new(CADENCE);
        let obsolete = fixture.opener.request(fixture.spec("old", 4, 10)).unwrap();
        let newest = fixture.opener.request(fixture.spec("new", 6, 20)).unwrap();
        assert!(newest > obsolete);
        fixture.wait_staged(newest);
        let status = fixture.opener.status();
        assert_eq!(status.staged, newest);
        assert_eq!(
            status.last_failure, None,
            "a superseded open is not a failure"
        );
        // Only the newest generation may ever be taken; a stale stage is left
        // for the opener to sweep (it is never dropped inline here).
        let staged = fixture.take_newest();
        assert_eq!(staged.generation, newest);
        assert_eq!(staged.clip.probed.frames, Some(6));
        fixture.finish();
    }

    #[test]
    fn the_opener_drains_retired_and_shutdown_clips() {
        let fixture = Fixture::new(CADENCE);
        let first_gen = fixture.opener.request(fixture.spec("a", 4, 10)).unwrap();
        fixture.wait_staged(first_gen);
        let first = fixture.take_newest();
        let second_gen = fixture.opener.request(fixture.spec("b", 5, 20)).unwrap();
        fixture.wait_staged(second_gen);
        let second = fixture.take_newest();
        // The ingest side hands both old clips back instead of dropping them.
        assert!(fixture.coordinator.retire(first.clip).is_ok());
        assert!(fixture.coordinator.retire(second.clip).is_ok());
        let third_gen = fixture.opener.request(fixture.spec("c", 6, 30)).unwrap();
        fixture.wait_staged(third_gen);
        assert!(
            fixture.coordinator.pop_retired().is_none(),
            "the opener drained the retire queue on its own thread"
        );
        assert_eq!(fixture.take_newest().generation, third_gen);
        fixture.finish();
    }

    #[test]
    fn shutdown_clip_is_dropped_only_after_the_opener_wakes() {
        let fixture = Fixture::new(CADENCE);
        let generation = fixture
            .opener
            .request(fixture.spec("shutdown", 4, 10))
            .unwrap();
        fixture.wait_staged(generation);
        let staged = fixture.take_newest();
        assert!(fixture.coordinator.put_shutdown(staged.clip).is_ok());
        fixture.opener.wake();
        fixture
            .opener
            .join()
            .expect("opener drains shutdown before exit");
        assert!(fixture.coordinator.take_shutdown().is_none());
    }

    #[test]
    fn a_full_retire_queue_defers_the_commit_without_touching_the_staged_clip() {
        // No opener thread here: this pins the commit gate exactly as the
        // ingest side sees it, using real staged clips opened through `open_one`.
        let coordinator = LoadCoordinator::<StagedClip>::new();
        let audio = AudioCommitSlot::<()>::new();
        let stop = Arc::new(AtomicBool::new(false));
        let config = OpenerConfig::new(CADENCE);
        let dir = tempfile::tempdir().unwrap();
        let mut opened = Vec::new();
        for (name, frames, luma) in [("a", 4_u32, 10_u8), ("b", 5, 20), ("c", 6, 30)] {
            let path = dir.path().join(format!("{name}.mov"));
            generate_player_clip(&path, &PlayerClipSpec::ffv1(CADENCE, frames, luma)).unwrap();
            let generation = coordinator.submit(ClipSpec::new(path, LoadThen::Cue));
            let request = coordinator.take_request().unwrap();
            let clip = open_one(&request, &coordinator, &stop, &config)
                .unwrap_or_else(|_| panic!("fixture {name} must open"));
            opened.push((generation, clip));
        }
        let (_, retired_a) = opened.remove(0);
        let (_, retired_b) = opened.remove(0);
        let (staged_gen, staged_clip) = opened.remove(0);
        assert!(coordinator.retire(retired_a).is_ok());
        assert!(coordinator.retire(retired_b).is_ok());
        assert!(coordinator.stage(staged_gen, staged_clip).is_none());
        assert!(
            coordinator.try_take(&audio).is_none(),
            "a full retire queue defers the commit"
        );
        assert_eq!(
            coordinator.latest(),
            staged_gen,
            "the staged clip stays for the newest generation"
        );
        let freed = coordinator.pop_retired().expect("one retired clip");
        drop(freed);
        assert_eq!(
            coordinator.try_take(&audio).unwrap().generation,
            staged_gen,
            "the commit resumes when a slot frees"
        );
    }

    #[test]
    fn unstarted_request_refuses_without_panicking_or_submitting() {
        let coordinator = Arc::new(LoadCoordinator::<StagedClip>::new());
        let opener = PlayerOpener::new(
            "unstarted",
            Arc::clone(&coordinator),
            OpenerConfig::new(CADENCE),
        );
        assert_eq!(
            opener.request(ClipSpec::new("/absent", LoadThen::Cue)),
            Err(RequestError::Unstarted)
        );
        assert_eq!(coordinator.latest(), Generation::NONE);
        assert!(coordinator.take_request().is_none());
    }

    #[test]
    fn total_prime_budget_is_checked_independently_of_per_read_deadlines() {
        let budget = Duration::from_secs(10);
        assert!(check_prime_budget(Duration::from_secs(9), budget).is_ok());
        assert!(matches!(
            check_prime_budget(budget, budget),
            Err(OpenAbort::Failed {
                reason: LoadFailureReason::Timeout,
                ..
            })
        ));
        assert!(matches!(
            check_prime_budget(Duration::from_secs(12), budget),
            Err(OpenAbort::Failed {
                reason: LoadFailureReason::Timeout,
                ..
            })
        ));
    }

    #[test]
    fn wake_does_not_wait_for_the_join_management_lock() {
        let fixture = Fixture::new(CADENCE);
        let guard = fixture.opener.join.lock().unwrap();
        // This reproduces the inherited implementation's deadlock: `wake`
        // used to lock `worker`, the same management lock held here. The test
        // calls wake on this thread, so a regression hangs at this exact line.
        fixture.opener.wake();
        assert!(fixture.opener.is_spawned());
        drop(guard);
        fixture.finish();
    }

    #[test]
    fn stalled_retirement_is_explicitly_woken() {
        let fixture = Fixture::new(CADENCE);
        let generation = fixture.opener.request(fixture.spec("a", 4, 10)).unwrap();
        fixture.wait_staged(generation);
        let staged = fixture.take_newest();
        assert!(fixture.coordinator.retire(staged.clip).is_ok());
        fixture.opener.wake();
        wait_for("retirement to drain without another load", || {
            fixture.coordinator.retired_len() == 0
        });
        fixture.finish();
    }

    #[test]
    fn staged_clip_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<StagedClip>();
    }
}
