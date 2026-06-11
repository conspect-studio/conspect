//! The display sink: startup (probe → mode-select → `TEST_ONLY` validate →
//! the one `ALLOW_MODESET` modeset) on the caller's thread, then a dedicated
//! flip-loop thread that owns the device for the rest of the run
//! (ADR-0044 §1).
//!
//! The loop is generic over [`KmsBackend`], so its entire behaviour —
//! conflation, EBUSY handling, no-new-frame repeat, modeset discipline — is
//! CI-tested over a scripted mock; only the [`super::kms`] backend touches
//! hardware. The engine side holds just a [`FramePublisher`] (wait-free); the
//! sink can wedge, crash, or stall without ever back-pressuring the engine
//! (invariants #1 + #10) — its failure mode is a frozen monitor showing the
//! last framebuffer while program output continues untouched.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::canvas::DisplayCanvas;
use super::device::{ConnectorSelector, DisplayError, HeadSetup, KmsBackend, SubmitError};
use super::mailbox::{frame_mailbox, FramePublisher, FrameReader};
use super::mode::{select_mode, ForcedMode, ModeRequest, SelectedMode};
use super::FlipDriver;
use multiview_core::time::Rational;

/// Configuration for one display sink (one connector = one head = one sink;
/// walls are one sink per head — no canvas spanning, brief §9).
#[derive(Debug, Clone)]
pub struct DisplaySinkConfig {
    /// The owning output's stable id (diagnostics/telemetry labels).
    pub output_id: String,
    /// Which connector to drive.
    pub connector: ConnectorSelector,
    /// The mode request (auto / exact override).
    pub mode: ModeRequest,
    /// The CVT-RB forced mode for an EDID-less chain, if configured.
    pub forced_mode: Option<ForcedMode>,
    /// The engine output cadence, for exact-rational refresh matching.
    pub engine_cadence: Option<Rational>,
    /// The bounded event-wait used by the flip loop. Also bounds how quickly
    /// an idle pipe notices a new mailbox frame; a few milliseconds is right.
    pub poll_interval: Duration,
}

/// Wait-free flip-loop telemetry counters (ADR-0044: flip timestamps and
/// conflation are exported from day one).
#[derive(Debug, Default)]
pub struct DisplayStats {
    /// Successful nonblocking commits.
    commits: AtomicU64,
    /// Page-flip completions observed.
    flips: AtomicU64,
    /// Commits answered `EBUSY` (mailbox conflation events).
    busy_conflations: AtomicU64,
    /// Device-level submit failures (held last-good instead).
    submit_errors: AtomicU64,
    /// Nanoseconds of the most recent kernel flip timestamp.
    last_flip_ns: AtomicU64,
}

/// One coherent read of the sink counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatsSnapshot {
    /// Successful nonblocking commits.
    pub commits: u64,
    /// Page-flip completions observed.
    pub flips: u64,
    /// Commits answered `EBUSY` (conflation events).
    pub busy_conflations: u64,
    /// Device-level submit failures.
    pub submit_errors: u64,
    /// The most recent kernel flip timestamp, in nanoseconds.
    pub last_flip_ns: u64,
}

impl DisplayStats {
    /// Snapshot the counters (relaxed reads; telemetry only).
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            commits: self.commits.load(Ordering::Relaxed),
            flips: self.flips.load(Ordering::Relaxed),
            busy_conflations: self.busy_conflations.load(Ordering::Relaxed),
            submit_errors: self.submit_errors.load(Ordering::Relaxed),
            last_flip_ns: self.last_flip_ns.load(Ordering::Relaxed),
        }
    }
}

/// The display sink entry point. See [`DisplaySink::start`].
#[derive(Debug)]
pub struct DisplaySink;

/// The running sink: owns the flip thread. Dropping the handle (or calling
/// [`DisplaySinkHandle::stop`]) stops and joins the thread — the loop notices
/// the flag within one bounded `poll_interval` wait.
#[derive(Debug)]
pub struct DisplaySinkHandle {
    head: HeadSetup,
    stats: Arc<DisplayStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DisplaySinkHandle {
    /// The head (connector + committed timing) this sink drives.
    #[must_use]
    pub fn head(&self) -> &HeadSetup {
        &self.head
    }

    /// The sink's telemetry counters.
    #[must_use]
    pub fn stats(&self) -> Arc<DisplayStats> {
        Arc::clone(&self.stats)
    }

    /// Stop the flip loop and join the thread (bounded by the loop's
    /// `poll_interval`-sized waits).
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // A panicked sink thread must never propagate into the engine's
            // teardown path; the join error is logged and dropped.
            if thread.join().is_err() {
                tracing::error!("display sink thread panicked during the run");
            }
        }
    }
}

impl Drop for DisplaySinkHandle {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

impl DisplaySink {
    /// Start one display sink over `backend`:
    ///
    /// 1. probe connectors and resolve [`DisplaySinkConfig::connector`];
    /// 2. select the timing (EDID preferred / exact-rational cadence match /
    ///    CVT-RB forced fallback — [`select_mode`]);
    /// 3. `TEST_ONLY`-validate, then apply the **one** `ALLOW_MODESET`
    ///    commit (both on this thread — startup is off the frame path by
    ///    construction);
    /// 4. spawn the dedicated flip-loop thread, which owns the device from
    ///    here on.
    ///
    /// Returns the running handle plus the wait-free [`FramePublisher`] the
    /// engine publishes each tick's canvas into.
    ///
    /// # Errors
    ///
    /// [`DisplayError`] when the connector cannot be resolved, no usable mode
    /// exists, validation fails, or the modeset fails. A startup failure is a
    /// run-configuration error; runtime failures after startup never
    /// propagate (the sink holds last-good and keeps trying).
    pub fn start<F, B>(
        mut backend: B,
        config: DisplaySinkConfig,
    ) -> Result<(DisplaySinkHandle, FramePublisher<F>), DisplayError>
    where
        F: DisplayCanvas + Send + Sync + 'static,
        B: KmsBackend + 'static,
    {
        // Consume the config wholesale: every field is either moved into the
        // sink thread or used during startup.
        let DisplaySinkConfig {
            output_id,
            connector,
            mode,
            forced_mode,
            engine_cadence,
            poll_interval,
        } = config;
        let connectors = backend.probe_connectors()?;
        let desc = match &connector {
            ConnectorSelector::Auto => {
                connectors.iter().find(|c| c.connected).ok_or_else(|| {
                    DisplayError::NoneConnected {
                        probed: connectors.iter().map(|c| c.name.clone()).collect(),
                    }
                })?
            }
            ConnectorSelector::Name(name) => {
                let found = connectors.iter().find(|c| &c.name == name).ok_or_else(|| {
                    DisplayError::ConnectorNotFound {
                        requested: name.clone(),
                        available: connectors.iter().map(|c| c.name.clone()).collect(),
                    }
                })?;
                if !found.connected {
                    return Err(DisplayError::NotConnected { name: name.clone() });
                }
                found
            }
        };
        let selected = select_mode(&desc.modes, &mode, forced_mode.as_ref(), engine_cadence)?;
        let from_edid = matches!(selected, SelectedMode::Edid(_));
        let setup = HeadSetup {
            connector: desc.name.clone(),
            mode: selected.mode().clone(),
            from_edid,
        };
        // TEST_ONLY first: the exact plane/format/mode combination is proven
        // before any hardware state changes (brief §1 step 5).
        backend.validate_setup(&setup)?;
        // The ONE ALLOW_MODESET commit — startup only, never the frame path.
        backend.apply_modeset(&setup)?;
        tracing::info!(
            output = %output_id,
            connector = %setup.connector,
            mode = %setup.mode.describe(),
            from_edid,
            "display head lit"
        );

        let (publisher, reader) = frame_mailbox::<F>();
        let stats = Arc::new(DisplayStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            let poll = poll_interval;
            std::thread::Builder::new()
                .name(format!("display-{}", setup.connector))
                .spawn(move || flip_loop(backend, &reader, &stop, &stats, poll, &output_id))
                .map_err(|e| DisplayError::Device(format!("spawning the sink thread: {e}")))?
        };
        Ok((
            DisplaySinkHandle {
                head: setup,
                stats,
                stop,
                thread: Some(thread),
            },
            publisher,
        ))
    }
}

/// The dedicated sink-thread loop (ADR-0044 §1): bounded event wait → drain
/// flip completions → when idle and a **newer** mailbox frame exists, write +
/// `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`. `EBUSY` = conflation; device
/// errors are counted and the last-good framebuffer stays on glass; nothing
/// here can reach back into the engine.
fn flip_loop<F, B>(
    mut backend: B,
    reader: &FrameReader<F>,
    stop: &AtomicBool,
    stats: &DisplayStats,
    poll_interval: Duration,
    output_id: &str,
) where
    F: DisplayCanvas + Send + Sync,
    B: KmsBackend,
{
    let mut driver = FlipDriver::new();
    while !stop.load(Ordering::Acquire) {
        match backend.wait_events(poll_interval) {
            Ok(events) => {
                for event in events {
                    driver.on_flip_complete();
                    stats.flips.fetch_add(1, Ordering::Relaxed);
                    let ns = u64::try_from(event.timestamp.as_nanos()).unwrap_or(u64::MAX);
                    stats.last_flip_ns.store(ns, Ordering::Relaxed);
                }
            }
            Err(e) => {
                // An event-channel failure is survivable: log, back off one
                // interval, keep the last framebuffer on glass.
                tracing::warn!(output = %output_id, error = %e, "display event wait failed");
                std::thread::sleep(poll_interval);
            }
        }
        let Some((frame, seq)) = reader.latest() else {
            continue;
        };
        if !driver.wants_commit(seq) {
            continue;
        }
        match backend.submit_frame(&*frame) {
            Ok(()) => {
                driver.on_commit_submitted(seq);
                stats.commits.fetch_add(1, Ordering::Relaxed);
            }
            Err(SubmitError::Busy) => {
                driver.on_commit_busy();
                stats.busy_conflations.fetch_add(1, Ordering::Relaxed);
            }
            Err(SubmitError::Device(e)) => {
                stats.submit_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    output = %output_id,
                    error = %e,
                    "display frame commit failed; holding the last framebuffer"
                );
            }
        }
    }
}
