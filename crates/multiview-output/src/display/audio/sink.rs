//! The ALSA HDMI audio sink: the run loop that wires the ELD gate, the bounded
//! drop-oldest FIFO, the buffer-level servo → multiview-audio adaptive
//! resampler, and the xrun-recovery machine into one consumer that **can never
//! back-pressure the engine** (invariants #1 + #10).
//!
//! The loop is generic over two trait seams — [`EldSource`] (does the head have
//! a lit audio path, and what can it take?) and [`AlsaSink`] (open / write /
//! recover / close a PCM) — so its entire behaviour is CI-tested over scripted
//! mocks. The real `/proc/asound` ELD reader and the libasound PCM live in
//! [`super::alsa`] behind the `display-kms` feature and run only on hardware.
//!
//! The engine side holds only a [`DisplayAudioPublisher`]: a bounded push into
//! a drop-oldest FIFO that **never blocks on the device** — the publisher and
//! the drain thread share one mutex held only for short in-memory copies,
//! never across a PCM call. A wedged/silent ALSA device drops audio (bounded
//! FIFO) and the engine never notices — exactly the display-sink isolation
//! shape, applied to audio.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use multiview_audio::{AdaptiveResampler, AudioBlock, AudioFormat, ChannelMatrix};

use super::eld::EldCapability;
use super::fifo::AudioFifo;
use super::servo::{drain_ratio, BufferServo};
use super::tracker::SkewTracker;
use super::xrun::{PcmOutcome, XrunRecovery, XrunState};

/// A reader of the display's most recent kernel flip timestamp in nanoseconds
/// (`display::sink` `last_flip_ns`; `0` until the first flip lands). The skew
/// half of the three-clock servo anchors against it so audio tracks the
/// *scanout* clock; without one (CI, or a sink without flip telemetry) the
/// servo holds sync on the FIFO term alone.
pub type FlipClock = Box<dyn Fn() -> u64 + Send>;

/// The negotiated PCM parameters the sink opens the device with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PcmParams {
    /// Sample rate in Hz (the canonical 48 kHz, gated by the ELD).
    pub sample_rate: u32,
    /// Channel count (clamped to the ELD ceiling).
    pub channels: u16,
    /// ALSA period size in frames (low-latency: 256–512).
    pub period_frames: u32,
    /// Number of periods in the device ring buffer (3–4 → ~16–43 ms).
    pub periods: u32,
}

/// A scripted/real source of the connector's ELD audio capability.
///
/// The real impl reads `/proc/asound/cardN/eld#C.P` ([`super::alsa`]); a mock
/// returns a fixed capability. Returning [`None`] means **no audio path** (an
/// EDID-less head, or the pipe not yet lit) — the sink stays silent.
pub trait EldSource: Send {
    /// Read the current capability, or [`None`] if there is no audio path.
    fn read_capability(&mut self) -> Option<EldCapability>;
}

/// A scripted/real ALSA PCM playback device.
///
/// The real impl drives libasound (`hdmi:CARD=…` / the vc4 card config —
/// [`super::alsa`]); a mock records writes. All methods are infallible from the
/// loop's view *except* `open`: a per-attempt [`PcmOutcome`] carries the write
/// result so the [`XrunRecovery`] machine handles faults without ever
/// propagating a panic.
pub trait AlsaSink: Send {
    /// Open the PCM with the negotiated parameters.
    ///
    /// # Errors
    ///
    /// A human-readable message when the device cannot be opened/configured —
    /// the sink then stays silent (it never crashes the run).
    fn open(&mut self, params: PcmParams) -> Result<(), String>;

    /// Write interleaved float frames to the PCM, returning the outcome.
    ///
    /// May accept fewer frames than offered ([`PcmOutcome::Wrote`] carries the
    /// count); the drain loop re-offers the unwritten tail. Implementations
    /// must return within a **bounded** time — the real backend drives a
    /// nonblocking PCM paced by bounded `snd_pcm_wait` slices — so teardown is
    /// never held hostage inside a device call.
    fn write(&mut self, interleaved: &[f32], channels: usize) -> PcmOutcome;

    /// Recover the PCM after an underrun/suspend (prepare/resume).
    fn recover(&mut self) -> PcmOutcome;

    /// Close the PCM (drain not required — the sink is being torn down).
    fn close(&mut self);

    /// The device's current playback delay in frames (`snd_pcm_delay`): how
    /// many delivered frames have not yet reached the speaker. Feeds the skew
    /// measurement; [`None`] (the default, and any mock) means the skew is
    /// computed from delivered frames alone — the anchor cancels the constant
    /// buffer depth to first order, so this stays a refinement, not a
    /// requirement.
    fn delay_frames(&mut self) -> Option<i64> {
        None
    }
}

/// Static configuration for one display-audio sink.
#[derive(Debug, Clone)]
pub struct DisplayAudioConfig {
    /// The owning display output's stable id (telemetry labels).
    pub output_id: String,
    /// The canonical program-audio format the engine pushes (48 kHz float).
    pub format: AudioFormat,
    /// FIFO capacity in frames (per channel). Bounds the worst-case audio
    /// latency and the drop point under a wedged device.
    pub fifo_capacity_frames: usize,
    /// How long the loop waits between drains when the FIFO is short — also the
    /// stop-flag latency. A few ms keeps the device ring fed without busy-waiting.
    pub poll_interval: Duration,
}

/// Wait-free audio-sink telemetry.
#[derive(Debug, Default)]
struct AudioSinkStats {
    /// `true` once the ELD gate is open and the PCM is running.
    audio_active: AtomicBool,
    /// Frames written to the PCM.
    frames_written: AtomicU64,
    /// Frames dropped at the FIFO (bounded drop-oldest).
    dropped_frames: AtomicU64,
    /// Successful xrun recoveries.
    recoveries: AtomicU64,
}

/// One coherent read of the audio-sink counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioStatsSnapshot {
    /// Whether audio is currently flowing (ELD valid + PCM running).
    pub audio_active: bool,
    /// Frames written to the PCM.
    pub frames_written: u64,
    /// Frames dropped at the FIFO.
    pub dropped_frames: u64,
    /// Successful xrun recoveries.
    pub recoveries: u64,
}

/// The engine-side handle: a bounded push of program-audio blocks into the
/// sink's drop-oldest FIFO. Cloneable and `Send`/`Sync` — the engine pushes
/// from the output-clock thread and **never blocks on the device**: the only
/// contention is one mutex the drain thread holds for short in-memory copies
/// (fill read + frame pop), never across a PCM call (invariants #1 + #10).
#[derive(Debug, Clone)]
pub struct DisplayAudioPublisher {
    fifo: Arc<Mutex<AudioFifo>>,
    channels: usize,
}

impl DisplayAudioPublisher {
    /// Push interleaved program audio for this tick. Bounded: a full FIFO
    /// drops its oldest frames rather than blocking the caller, and the push
    /// is one short mutex-guarded in-memory copy — the lock is never held
    /// across a device call, so a wedged PCM cannot reach the engine.
    pub fn push_block(&self, interleaved: &[f32]) {
        if let Ok(mut fifo) = self.fifo.lock() {
            fifo.push(interleaved);
        }
    }

    /// Push a multiview-audio [`AudioBlock`] (the engine program-bus type).
    pub fn push_audio(&self, block: &AudioBlock) {
        self.push_block(block.interleaved());
    }

    /// Total frames dropped at the FIFO so far (telemetry).
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.fifo.lock().map_or(0, |f| f.dropped_frames())
    }

    /// The channel count the sink expects.
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }
}

/// A running display-audio sink: owns the drain thread. Dropping the handle
/// (or calling [`stop`](Self::stop)) signals the thread and joins it — a
/// healthy loop notices within about one `poll_interval`; a thread wedged
/// inside a device call is **detached and logged** after the hard
/// [`STOP_JOIN_BOUND`], so teardown can never hang on a hostile device.
#[derive(Debug)]
pub struct DisplayAudioSink {
    stats: Arc<AudioSinkStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DisplayAudioSink {
    /// Start a display-audio sink over the given ELD source and ALSA device.
    ///
    /// Returns the running handle plus the bounded, never-blocking-on-the-
    /// device [`DisplayAudioPublisher`] the engine pushes each tick's program
    /// audio into. The sink **never fails to start**: a missing ELD or an
    /// un-openable device leaves it silent but alive (the display picture is
    /// unaffected) — audio is best-effort by construction (display-out §5).
    #[must_use]
    pub fn start<E, A>(config: DisplayAudioConfig, eld: E, alsa: A) -> (Self, DisplayAudioPublisher)
    where
        E: EldSource + 'static,
        A: AlsaSink + 'static,
    {
        Self::start_with_flip_clock(config, eld, alsa, None)
    }

    /// [`start`](Self::start), additionally wired to the display's flip clock
    /// (`display::sink` `last_flip_ns`) so the servo's skew term re-aligns the
    /// audio sample clock to the *scanout* clock over the long run. [`None`]
    /// keeps the FIFO-fill term as the sole sync input (CI, or a head without
    /// flip telemetry).
    #[must_use]
    pub fn start_with_flip_clock<E, A>(
        config: DisplayAudioConfig,
        eld: E,
        alsa: A,
        flip_clock: Option<FlipClock>,
    ) -> (Self, DisplayAudioPublisher)
    where
        E: EldSource + 'static,
        A: AlsaSink + 'static,
    {
        let channels = config.format.channel_count().max(1);
        let fifo = Arc::new(Mutex::new(AudioFifo::new(
            config.fifo_capacity_frames,
            channels,
        )));
        let stats = Arc::new(AudioSinkStats::default());
        let stop = Arc::new(AtomicBool::new(false));

        let publisher = DisplayAudioPublisher {
            fifo: Arc::clone(&fifo),
            channels,
        };

        let thread = {
            let fifo = Arc::clone(&fifo);
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(format!("display-audio-{}", config.output_id))
                .spawn(move || {
                    drain_loop(
                        &config,
                        eld,
                        alsa,
                        flip_clock.as_ref(),
                        &fifo,
                        &stats,
                        &stop,
                    );
                })
                .ok()
        };

        (
            Self {
                stats,
                stop,
                thread,
            },
            publisher,
        )
    }

    /// Snapshot the sink's telemetry counters.
    #[must_use]
    pub fn stats(&self) -> AudioStatsSnapshot {
        AudioStatsSnapshot {
            audio_active: self.stats.audio_active.load(Ordering::Relaxed),
            frames_written: self.stats.frames_written.load(Ordering::Relaxed),
            dropped_frames: self.stats.dropped_frames.load(Ordering::Relaxed),
            recoveries: self.stats.recoveries.load(Ordering::Relaxed),
        }
    }

    /// Stop the drain loop and join the thread (bounded — see
    /// [`STOP_JOIN_BOUND`]).
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Bounded join: a healthy loop honours the stop flag within about one
        // poll_interval, but a hostile/buggy driver can stall a device call
        // arbitrarily — never let it hold the caller (possibly a runtime
        // teardown path) hostage. Past the bound, detach-and-log: the thread
        // exits on its own when the device call finally returns.
        let deadline = Instant::now() + STOP_JOIN_BOUND;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if thread.is_finished() {
            if thread.join().is_err() {
                tracing::error!("display-audio sink thread panicked during the run");
            }
        } else {
            tracing::error!(
                "display-audio drain thread is wedged inside a device call at teardown; \
                 detaching it so stop() stays bounded (it exits when the call returns)"
            );
            drop(thread);
        }
    }
}

/// The hard bound on joining the drain thread at teardown. A healthy loop
/// exits well inside this (it polls the stop flag at least every
/// `poll_interval`); only a device call wedged inside the driver can exceed
/// it, and then the thread is detached and logged instead of blocking the
/// caller.
const STOP_JOIN_BOUND: Duration = Duration::from_millis(500);

impl Drop for DisplayAudioSink {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// How many output frames the loop drains per iteration when audio is flowing
/// (one 10 ms block at 48 kHz). Kept small so the servo reacts promptly and the
/// device ring stays low-latency.
///
/// **Gain coupling:** the servo's per-iteration closed-loop fill gain is
/// `kp × 1e-6 × DRAIN_FRAMES / fifo_capacity` (a ppm correction moves that
/// fraction of fill per drained quantum) — ≈ 2.3 × 10⁻⁴ with [`BufferServo`]'s
/// `kp = 4000` and the pipeline's 8192-frame FIFO, i.e. heavily damped with a
/// ~40 s settling constant. Changing this quantum, the servo gains, or the
/// FIFO capacity moves that loop gain together; pushing it toward 1 makes the
/// servo oscillate.
const DRAIN_FRAMES: usize = 480;

/// How many steady-state iterations pass between ELD re-reads (hotplug watch).
/// At the device-paced ~10 ms per iteration this is a re-check every ~2.5 s —
/// frequent enough to notice an unplug/replug, far off any hot path.
const ELD_RECHECK_ITERATIONS: u32 = 256;

/// Why a steady-state drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainEnd {
    /// The stop flag was raised — tear down.
    Stopped,
    /// The ELD went away or changed (hotplug) — close the PCM and re-gate.
    EldChanged,
}

/// The drain loop: gate on the ELD, negotiate + open the PCM, run the
/// steady-state drain, and **re-gate on hotplug** (an ELD that disappears or
/// changes closes the PCM and returns to the gate — a changed monitor
/// renegotiates). Nothing here can reach back into the engine — a missing ELD
/// or an un-openable device leaves the sink silent but alive.
fn drain_loop<E, A>(
    config: &DisplayAudioConfig,
    mut eld: E,
    mut alsa: A,
    flip_clock: Option<&FlipClock>,
    fifo: &Arc<Mutex<AudioFifo>>,
    stats: &AudioSinkStats,
    stop: &AtomicBool,
) where
    E: EldSource,
    A: AlsaSink,
{
    let channels = config.format.channel_count().max(1);
    // Warn once per distinct unusable capability / open failure, not per poll.
    let mut warned_rate: Option<EldCapability> = None;
    let mut warned_open = false;

    while !stop.load(Ordering::Acquire) {
        // --- ELD gate: an EDID-less head (no ELD) means NO audio path. Poll for
        // it (a hotplug can light the pipe later), but stay silent + alive until
        // valid, discarding pushed audio so the FIFO never grows and the engine
        // push stays unblocked.
        let Some(capability) = eld.read_capability().filter(EldCapability::has_audio) else {
            discard_fifo(fifo);
            std::thread::sleep(config.poll_interval);
            continue;
        };

        // --- Negotiate a format the sink can take (channels clamp DOWN to the
        // ELD ceiling, and a below-minimum ask is raised to the HDMI stereo
        // minimum — the sink folds/upmixes to whatever was negotiated). An ELD
        // that cannot take our canonical rate keeps the sink silent but
        // polling — a hotplugged 48 kHz-capable monitor lights it later.
        let req_channels = u8::try_from(channels).unwrap_or(u8::MAX);
        let Some((rate, neg_channels)) =
            capability.negotiate(config.format.sample_rate(), req_channels)
        else {
            if warned_rate.as_ref() != Some(&capability) {
                tracing::warn!(
                    output = %config.output_id,
                    monitor = %capability.monitor_name(),
                    "display sink ELD does not advertise the program-audio rate; audio silent"
                );
                warned_rate = Some(capability);
            }
            discard_fifo(fifo);
            std::thread::sleep(config.poll_interval);
            continue;
        };
        let out_channels = usize::from(neg_channels.max(1));
        let params = PcmParams {
            sample_rate: rate,
            channels: neg_channels.into(),
            period_frames: 480,
            periods: 4,
        };

        if let Err(e) = alsa.open(params) {
            if !warned_open {
                tracing::warn!(
                    output = %config.output_id,
                    error = %e,
                    "display-audio PCM open failed; audio silent (picture unaffected), retrying"
                );
                warned_open = true;
            }
            // Keep the FIFO drained and retry on a slow cadence — a module-load
            // race can make the device appear after the head lit.
            discard_until(fifo, stop, config.poll_interval, Duration::from_secs(1));
            continue;
        }
        warned_open = false;
        stats.audio_active.store(true, Ordering::Relaxed);
        tracing::info!(
            output = %config.output_id,
            monitor = %capability.monitor_name(),
            rate,
            channels = out_channels,
            "display-audio path lit"
        );

        let end = steady_state_drain(
            config,
            &mut eld,
            &mut alsa,
            flip_clock,
            fifo,
            stats,
            stop,
            &capability,
            rate,
            out_channels,
        );

        alsa.close();
        stats.audio_active.store(false, Ordering::Relaxed);
        match end {
            DrainEnd::Stopped => return,
            DrainEnd::EldChanged => {
                tracing::info!(
                    output = %config.output_id,
                    "display-audio ELD changed/lost; re-gating (hotplug)"
                );
            }
        }
    }
}

/// The steady-state drain: each iteration read the FIFO fill + measured skew,
/// turn the servo's drain demand into the resampler ratio (the **reciprocal**
/// — see [`drain_ratio`]), pull a fixed quantum, resample (reusing
/// multiview-audio's [`AdaptiveResampler`]), fold/upmix channels to the
/// negotiated count, and write to the PCM — carrying short-write tails and
/// recovering from any xrun. Runs until the stop flag or the ELD changes;
/// never blocks the engine push.
#[allow(clippy::too_many_arguments)]
// reason: the drain owns one each of the seams + the negotiated parameters; a
// one-shot bundling struct would only rename the same ten things.
fn steady_state_drain<E, A>(
    config: &DisplayAudioConfig,
    eld: &mut E,
    alsa: &mut A,
    flip_clock: Option<&FlipClock>,
    fifo: &Arc<Mutex<AudioFifo>>,
    stats: &AudioSinkStats,
    stop: &AtomicBool,
    capability: &EldCapability,
    rate: u32,
    out_channels: usize,
) -> DrainEnd
where
    E: EldSource,
    A: AlsaSink,
{
    let channels = config.format.channel_count().max(1);
    let mut servo = BufferServo::new();
    let mut resampler = AdaptiveResampler::new(config.format);
    let mut scratch = vec![0.0f32; DRAIN_FRAMES.saturating_mul(channels)];
    // Map the program channels onto the negotiated PCM layout when they differ
    // — folding down to an ELD-clamped count or upmixing up to the HDMI stereo
    // minimum; `None` means identity (the common stereo↔stereo case).
    let fold = match fold_matrix(channels, out_channels) {
        Ok(fold) => fold,
        Err(reason) => {
            // Unreachable by construction (counts are 1..=255); refuse to write
            // garbage into a mismatched device — silent is the safe failure.
            tracing::error!(
                output = %config.output_id,
                reason,
                "display-audio channel fold could not be built; audio silent"
            );
            idle_until_stop(fifo, stop, config.poll_interval);
            return DrainEnd::Stopped;
        }
    };
    let mut session = PcmSession {
        recovery: XrunRecovery::new(),
        tracker: SkewTracker::new(),
    };
    let mut recheck = 0u32;

    while !stop.load(Ordering::Acquire) {
        // Hotplug watch: re-read the ELD periodically; gone or changed ends the
        // drain so the loop re-gates (a changed monitor renegotiates).
        recheck = recheck.wrapping_add(1);
        if recheck % ELD_RECHECK_ITERATIONS == 0
            && eld
                .read_capability()
                .filter(EldCapability::has_audio)
                .as_ref()
                != Some(capability)
        {
            return DrainEnd::EldChanged;
        }

        // Servo: fill + measured skew → drain demand → reciprocal → resampler
        // ratio (see `drain_ratio` for the sign physics). The skew tracker
        // observes the latest flip-clock value (0 = no flip clock / not lit)
        // plus the PCM's current delay once per iteration.
        let flip_ns = flip_clock.map_or(0, |flip| flip());
        let skew =
            session
                .tracker
                .skew_input(flip_ns, alsa.delay_frames(), resampler.ratio(), rate);
        let dropped = match fifo.lock() {
            Ok(mut f) => {
                let fill = f.fill_fraction();
                resampler.set_ratio(drain_ratio(servo.correction(fill, skew)));
                let _ = f.pop_into(&mut scratch);
                f.dropped_frames()
            }
            Err(_) => 0,
        };
        stats.dropped_frames.store(dropped, Ordering::Relaxed);

        // Resample at the applied ratio (reusing multiview-audio's resampler),
        // fold to the negotiated channel count, then write. A ragged scratch
        // never occurs (DRAIN_FRAMES × channels), so the fallbacks below are
        // defensive, not load-bearing.
        let block = AudioBlock::from_interleaved(config.format, scratch.clone())
            .unwrap_or_else(|_| AudioBlock::silence(config.format, DRAIN_FRAMES));
        let out_block = resampler.process(&block);
        let folded;
        let samples: &[f32] = match &fold {
            Some(matrix) => match matrix.apply_interleaved(out_block.interleaved()) {
                Ok(out) => {
                    folded = out;
                    &folded
                }
                // A ragged block cannot come out of the resampler; skip the
                // write rather than feed a mismatched layout to the device.
                Err(_) => &[],
            },
            None => out_block.interleaved(),
        };

        if !samples.is_empty() {
            write_quantum(
                &config.output_id,
                alsa,
                stats,
                &mut session,
                resampler.ratio(),
                samples,
                out_channels,
                stop,
                config.poll_interval,
            );
        }

        // Degraded: back off (and report inactive) rather than spinning on a
        // dead device. Healthy: a short wait keeps the device ring fed without
        // busy-waiting (on hardware the nonblocking PCM write paces itself
        // against the device ring via bounded `snd_pcm_wait` slices). Either
        // way the engine push is never blocked, and the stop flag is honoured
        // within `poll_interval` even mid-backoff.
        let wait = if session.recovery.state() == XrunState::Degraded {
            stats.audio_active.store(false, Ordering::Relaxed);
            session.recovery.backoff().max(config.poll_interval)
        } else {
            stats.audio_active.store(true, Ordering::Relaxed);
            config.poll_interval
        };
        sleep_with_stop(stop, wait, config.poll_interval);
    }
    DrainEnd::Stopped
}

/// The per-PCM-session mutable drain state: the xrun-recovery machine plus the
/// skew tracker (re-anchored after any xrun, because the device position jumps
/// across a recover).
struct PcmSession {
    /// The xrun-recovery state machine for this PCM.
    recovery: XrunRecovery,
    /// The sample-vs-scanout skew bookkeeping for the servo's slow term.
    tracker: SkewTracker,
}

/// How many consecutive zero-progress writes (`Wrote(0)`, a full device ring
/// taking nothing) are retried — each paced by one `poll_interval` sleep —
/// before the remainder of a quantum is dropped. Bounds the time one quantum
/// can occupy the loop while keeping a routinely-short-writing device fed.
const MAX_STALLED_WRITES: u32 = 8;

/// Write one prepared quantum to the PCM, advancing the recovery machine, the
/// skew tracker, and the telemetry counters. A **short write re-offers the
/// unwritten tail** (a nonblocking device routinely takes part of a quantum
/// once its ring fills) so no popped content is silently discarded; the retry
/// is bounded by the stop flag, by [`MAX_STALLED_WRITES`] zero-progress
/// attempts, and by an xrun (which drops the remainder — the device position
/// jumped anyway). An xrun triggers the recover action and drops the skew
/// anchor; nothing here can fail the loop (every outcome is a state
/// transition, never a panic).
#[allow(clippy::too_many_arguments)]
// reason: the quantum write owns the seams + per-session state the drain loop
// already holds; a one-shot bundling struct would only rename the same things.
fn write_quantum<A>(
    output_id: &str,
    alsa: &mut A,
    stats: &AudioSinkStats,
    session: &mut PcmSession,
    applied: multiview_audio::RatioPpm,
    samples: &[f32],
    out_channels: usize,
    stop: &AtomicBool,
    poll: Duration,
) where
    A: AlsaSink,
{
    let channels = out_channels.max(1);
    let mut offset = 0usize; // samples handed to the device so far
    let mut stalled_writes = 0u32;
    while offset < samples.len() && !stop.load(Ordering::Acquire) {
        let outcome = alsa.write(samples.get(offset..).unwrap_or(&[]), out_channels);
        let action = session.recovery.on_outcome(outcome);
        let mut quantum_over = false;
        match outcome {
            PcmOutcome::Wrote(frames) => {
                let written = u64::try_from(frames).unwrap_or(0);
                stats.frames_written.fetch_add(written, Ordering::Relaxed);
                session.tracker.on_written(written, applied);
                offset = offset
                    .saturating_add(frames.saturating_mul(channels))
                    .min(samples.len());
                if frames == 0 {
                    stalled_writes = stalled_writes.saturating_add(1);
                    if stalled_writes >= MAX_STALLED_WRITES {
                        tracing::debug!(
                            output = %output_id,
                            "display-audio device accepting no frames; dropping the quantum tail"
                        );
                        quantum_over = true;
                    } else {
                        // Give the device ring time to drain before re-offering
                        // the tail (stop-flag honoured throughout).
                        sleep_with_stop(stop, poll, poll);
                    }
                } else {
                    stalled_writes = 0;
                }
            }
            PcmOutcome::Underrun | PcmOutcome::Suspended => {
                tracing::debug!(output = %output_id, "display-audio xrun; recovering");
                // The device position jumps across a recover: re-anchor. The
                // remainder of this quantum is dropped (continuity is broken
                // regardless); the next iteration pops fresh content.
                session.tracker.on_xrun();
                quantum_over = true;
            }
            PcmOutcome::Recovered | PcmOutcome::RecoverFailed => {
                quantum_over = true;
            }
        }

        if action.recover {
            let rec = alsa.recover();
            let _ = session.recovery.on_outcome(rec);
            if rec == PcmOutcome::Recovered {
                stats
                    .recoveries
                    .store(session.recovery.recoveries(), Ordering::Relaxed);
            }
        }
        if quantum_over {
            return;
        }
    }
}

/// Build the program→PCM channel map for a negotiated count that differs from
/// the program layout, in either direction:
///
/// * **Fold down** (ELD ceiling below the program count): every input channel
///   routes to `min(i, out-1)` with equal-gain normalisation per output, so
///   stereo→mono is the 0.5/0.5 average.
/// * **Upmix** (negotiation raised the ask to the HDMI stereo minimum): each
///   output takes input `min(o, in-1)` at unity gain — mono is duplicated
///   onto both subframes of the IEC 60958 pair. No normalisation: each output
///   carries exactly one input.
///
/// `Ok(None)` when the counts already match (identity).
///
/// # Errors
///
/// Returns a static reason if the matrix rejects a route — unreachable for the
/// 1..=255-channel inputs the negotiation produces, but never panics.
fn fold_matrix(
    in_channels: usize,
    out_channels: usize,
) -> Result<Option<ChannelMatrix>, &'static str> {
    if in_channels == out_channels || out_channels == 0 || in_channels == 0 {
        return Ok(None);
    }
    if in_channels < out_channels {
        let routes: Vec<(usize, usize, f32)> = (0..out_channels)
            .map(|o| (o.min(in_channels.saturating_sub(1)), o, 1.0))
            .collect();
        return ChannelMatrix::from_routes(in_channels, out_channels, &routes)
            .map(Some)
            .map_err(|_| "channel route out of range");
    }
    // Count how many inputs land on each output so the fold preserves level.
    let mut counts = vec![0usize; out_channels];
    for i in 0..in_channels {
        let to = i.min(out_channels.saturating_sub(1));
        if let Some(c) = counts.get_mut(to) {
            *c += 1;
        }
    }
    let mut routes = Vec::with_capacity(in_channels);
    for i in 0..in_channels {
        let to = i.min(out_channels.saturating_sub(1));
        let n = counts.get(to).copied().unwrap_or(1).max(1);
        let gain = 1.0 / f32_from_count(n);
        routes.push((i, to, gain));
    }
    ChannelMatrix::from_routes(in_channels, out_channels, &routes)
        .map(Some)
        .map_err(|_| "channel route out of range")
}

/// `usize → f32` for tiny channel-fold counts (≤ 255), exact.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: channel counts are ≤ 255; the conversion is exact and no fallible
// `From<usize> for f32` exists.
fn f32_from_count(n: usize) -> f32 {
    n as f32
}

/// Sleep for `wait`, polling the stop flag every `poll` so teardown is never
/// held hostage by a long (degraded-state) backoff.
fn sleep_with_stop(stop: &AtomicBool, wait: Duration, poll: Duration) {
    let poll = poll.max(Duration::from_millis(1));
    let deadline = Instant::now() + wait;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(poll));
    }
}

/// Drain and discard the FIFO so it never grows while audio is silent.
fn discard_fifo(fifo: &Arc<Mutex<AudioFifo>>) {
    if let Ok(mut f) = fifo.lock() {
        let mut sink = vec![0.0f32; f.fill_frames().saturating_mul(f.channels())];
        let _ = f.pop_into(&mut sink);
    }
}

/// Discard pushed audio for up to `span` (or until stop), polling at `poll` —
/// the open-retry pacing that keeps the FIFO bounded while the device is away.
fn discard_until(fifo: &Arc<Mutex<AudioFifo>>, stop: &AtomicBool, poll: Duration, span: Duration) {
    let deadline = Instant::now() + span;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        discard_fifo(fifo);
        std::thread::sleep(poll.max(Duration::from_millis(1)));
    }
}

/// Stay alive but silent until the stop flag, discarding pushed audio so the
/// engine push never blocks and the FIFO never grows.
fn idle_until_stop(fifo: &Arc<Mutex<AudioFifo>>, stop: &AtomicBool, poll: Duration) {
    while !stop.load(Ordering::Acquire) {
        discard_fifo(fifo);
        std::thread::sleep(poll.max(Duration::from_millis(1)));
    }
}
