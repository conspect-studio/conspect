//! The **per-output clock** (issue #343, `output-routing.md` §12.6, ADR-0109).
//!
//! An output whose frame rate differs from Program's cannot ride Program's tick
//! counter: it gets an output clock of its own — a dedicated, named OS thread
//! (`clock_thread::spawn`) with its own [`OutputClock`] at the output's rate,
//! paced on the same shared [`TimeSource`] as Program and seeded with
//! Program's seed, so output tick 0 is aligned with Program tick 0.
//!
//! * Tick `k` is due at the **absolute** deadline
//!   `seed + pts_out(k) + latch_delay` ([`OutputClock::deadline_nanos`]):
//!   integer arithmetic from the tick counter, never float fps, re-read after
//!   every wakeup, so sleep jitter never accumulates. The latch delay is a fixed
//!   pipeline delay ([`LATCH_DELAY`]) so the Program frame for `pts_out(k)` has
//!   been baked when the tick latches it; it never enters a stamp: the tick's
//!   PTS is exactly `pts_out(k)`.
//! * After a wake later than [`MAX_CATCHUP_TICKS`] periods past tick `k`'s
//!   deadline (a wedged callback, a suspended host) the clock
//!   [`skip_to`](OutputClock::skip_to)s the tick that is due now and counts the
//!   skipped indices; the consumer gap-fills them. Without the skip a wedged
//!   lane would emit a burst of stale ticks on recovery.
//! * The clock owns no lock Program holds and never awaits anything. Its
//!   callback must not block either (it offers to bounded queues); if it does,
//!   only this lane is late — Program and every other lane run on their own
//!   threads.
//! * Counters ([`OutputLaneStats`]) are wait-free atomics plus a
//!   [`TickLateness`] histogram against the deadline.
//! * Stop is observed within [`STOP_POLL`] of being raised (while pacing); a
//!   callback that never returns is shed by the bounded
//!   [`OutputLaneHandle::join_within`], never joined forever. A panicking
//!   callback ends only its own lane (the panic is captured by the clock thread
//!   and reported by the join).

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_core::time::Rational;

use crate::clock::{OutputClock, Tick, TimeSource};
use crate::clock_thread::{self, ClockThread, ClockThreadPanic, STOP_POLL};
use crate::lateness::TickLateness;
use crate::runtime::StopSignal;

/// The fixed pipeline delay between an output tick's PTS and its wall-clock
/// deadline: the Program frame stamped `<= pts_out(k)` has been baked (and
/// published to the [`ProgramLatch`](crate::program_latch::ProgramLatch)) by
/// then with a wide margin. Never part of a stamp.
pub const LATCH_DELAY: Duration = Duration::from_millis(100);

/// How many periods late a lane wake may be before the clock skips forward to
/// the tick due now (counted in [`OutputLaneStats::skipped_ticks`]).
pub const MAX_CATCHUP_TICKS: u64 = 8;

/// The shortest [`LanePacing::Stepped`] sleep: a zero step is clamped up to
/// this so a stepped lane can never busy-spin.
pub const MIN_STEP: Duration = Duration::from_millis(1);

/// How the lane waits for a tick's deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LanePacing {
    /// Production: [`clock_thread::sleep_until`] (absolute-deadline
    /// `clock_nanosleep` on Linux, sleep-then-spin elsewhere).
    #[default]
    Precise,
    /// Deterministic tests on a [`ManualTimeSource`](crate::ManualTimeSource):
    /// re-read the source after real sleeps of `step` (clamped to
    /// [`MIN_STEP`]..=[`STOP_POLL`]); never spins hot when the manual clock is
    /// frozen.
    Stepped(Duration),
}

/// The configuration of one per-output clock.
#[derive(Debug, Clone)]
pub struct OutputLaneClock {
    /// Lane name (the thread is named `clock-out-<name>`, truncated to 15 bytes).
    pub name: String,
    /// The output's frame rate (exact rational, never float fps).
    pub cadence: Rational,
    /// Program's seed on the shared [`TimeSource`] timeline.
    pub seed_nanos: i64,
    /// Fixed delay between a tick's PTS and its deadline ([`LATCH_DELAY`]).
    pub latch_delay: Duration,
    /// Catch-up bound ([`MAX_CATCHUP_TICKS`]).
    pub max_catchup_ticks: u64,
    /// How the lane waits ([`LanePacing::Precise`] in production).
    pub pacing: LanePacing,
}

impl OutputLaneClock {
    /// A lane at `cadence` seeded with Program's `seed_nanos`, with the
    /// default latch delay, catch-up bound and precise pacing.
    #[must_use]
    pub fn new(name: impl Into<String>, cadence: Rational, seed_nanos: i64) -> Self {
        Self {
            name: name.into(),
            cadence,
            seed_nanos,
            latch_delay: LATCH_DELAY,
            max_catchup_ticks: MAX_CATCHUP_TICKS,
            pacing: LanePacing::Precise,
        }
    }

    /// The absolute deadline of tick `index` on the shared timeline:
    /// `seed + pts_out(index) + latch_delay`.
    #[must_use]
    pub fn deadline_nanos(&self, clock: &OutputClock, index: u64) -> i64 {
        clock
            .deadline_nanos(index, self.seed_nanos)
            .saturating_add(duration_nanos(self.latch_delay))
    }
}

/// Wait-free counters of one lane.
#[derive(Debug, Default)]
pub struct OutputLaneStats {
    ticks: AtomicU64,
    skipped_ticks: AtomicU64,
    lateness: TickLateness,
    exhausted: std::sync::atomic::AtomicBool,
}

impl OutputLaneStats {
    /// Ticks emitted (callbacks made).
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Acquire)
    }

    /// Tick indices skipped by the catch-up (never emitted).
    #[must_use]
    pub fn skipped_ticks(&self) -> u64 {
        self.skipped_ticks.load(Ordering::Acquire)
    }

    /// Whether the lane ended because its timeline was exhausted: the next
    /// tick's PTS or deadline no longer strictly increases (saturated `i64`
    /// arithmetic on an extreme seed/time). Never true in a real run (~292
    /// years of ticks); it turns a would-be hot loop of identical stamps into
    /// an explicit, logged end of this lane only.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Acquire)
    }

    /// The wake-lateness histogram against each tick's deadline.
    #[must_use]
    pub fn lateness(&self) -> &TickLateness {
        &self.lateness
    }
}

/// Why a lane's bounded join did not end cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneJoinError {
    /// The callback panicked; the lane ended (no other lane is affected).
    Panicked(String),
    /// The lane did not end within the grace (a wedged callback); it was
    /// detached, never joined forever.
    Detached,
}

impl std::fmt::Display for LaneJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked(message) => write!(f, "output clock panicked: {message}"),
            Self::Detached => f.write_str("output clock did not stop within its grace; detached"),
        }
    }
}

impl std::error::Error for LaneJoinError {}

/// The handle to a running per-output clock.
#[derive(Debug)]
pub struct OutputLaneHandle {
    thread: ClockThread<()>,
    own_stop: StopSignal,
    stats: Arc<OutputLaneStats>,
    name: String,
}

impl OutputLaneHandle {
    /// The lane's counters (shared, wait-free).
    #[must_use]
    pub fn stats(&self) -> Arc<OutputLaneStats> {
        Arc::clone(&self.stats)
    }

    /// Whether the lane's thread has ended (stopped or panicked). Wait-free.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Ask this lane (only) to stop; observed within [`STOP_POLL`] while
    /// pacing, or when a running callback returns.
    pub fn stop(&self) {
        self.own_stop.stop();
    }

    /// The lane name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stop the lane and wait at most `grace` for it to end. Polls
    /// [`Self::is_finished`]; never a join that cannot return. Beyond the grace
    /// the thread is detached (it holds only its own state).
    ///
    /// # Errors
    ///
    /// [`LaneJoinError::Panicked`] if the callback panicked,
    /// [`LaneJoinError::Detached`] if the lane outlived `grace`.
    pub fn join_within(self, grace: Duration) -> Result<(), LaneJoinError> {
        self.stop();
        let started = Instant::now();
        while !self.thread.is_finished() {
            if started.elapsed() >= grace {
                tracing::warn!(lane = %self.name, "output clock wedged past its grace; detached");
                return Err(LaneJoinError::Detached);
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        // Finished: `clock_thread::spawn` delivers the result BEFORE raising
        // `finished`, so this resolves on its first poll (never parks).
        clock_thread::block_on(self.thread.join())
            .map_err(|ClockThreadPanic(message)| LaneJoinError::Panicked(message))
    }
}

fn duration_nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

/// The largest tick index whose deadline is `<= now`: the closed-form
/// inversion of [`OutputLaneClock::deadline_nanos`],
/// `k = floor((now - seed - delay) · num / (den · 1e9))`, in i128 (no
/// overflow for any i64 inputs), clamped to `u64`, then corrected by at most
/// ONE step each way for the deadline's rounding — never an unbounded loop,
/// whatever the seed, delay or `now` (saturated deadlines included).
fn due_index(cfg: &OutputLaneClock, clock: &OutputClock, now: i64) -> u64 {
    let delay = i128::try_from(cfg.latch_delay.as_nanos()).unwrap_or(i128::MAX);
    let elapsed = i128::from(now) - i128::from(cfg.seed_nanos) - delay.min(i128::from(i64::MAX));
    if elapsed <= 0 {
        return 0;
    }
    let cadence = clock.cadence();
    let num = elapsed.saturating_mul(i128::from(cadence.num));
    let den = i128::from(cadence.den).saturating_mul(1_000_000_000).max(1);
    let mut index = u64::try_from(num / den).unwrap_or(u64::MAX);
    // `pts_at` rounds to the nearest nanosecond, so the exact floor may be one
    // tick off either way; one bounded correction each way.
    if index < u64::MAX && cfg.deadline_nanos(clock, index + 1) <= now {
        index += 1;
    } else if index > 0 && cfg.deadline_nanos(clock, index) > now {
        index -= 1;
    }
    index
}

/// Wait until `deadline` on `time`, or either stop is raised. Both stops are
/// checked between bounded real-time steps (at most [`STOP_POLL`], the
/// portable spin included), so a raised stop is observed within one
/// [`STOP_POLL`] even on a time source that never reaches the deadline.
fn pace(
    pacing: LanePacing,
    deadline: i64,
    time: &dyn TimeSource,
    run_stop: &StopSignal,
    own_stop: &StopSignal,
) {
    let stopped = || run_stop.is_stopped() || own_stop.is_stopped();
    match pacing {
        LanePacing::Precise => clock_thread::sleep_until_or(deadline, time, &stopped),
        LanePacing::Stepped(step) => {
            let step = step.clamp(MIN_STEP, STOP_POLL);
            while !stopped() && time.now_nanos() < deadline {
                std::thread::sleep(step);
            }
        }
    }
}

/// Start a per-output clock: a dedicated clock thread calling `on_tick` once
/// per output tick (see the module docs). The lane ends when `stop` (the run's
/// stop) or [`OutputLaneHandle::stop`] is raised.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] for an invalid cadence; otherwise the OS
/// refused to create the thread.
pub fn spawn_output_lane<F>(
    cfg: OutputLaneClock,
    time: Arc<dyn TimeSource>,
    stop: StopSignal,
    mut on_tick: F,
) -> io::Result<OutputLaneHandle>
where
    F: FnMut(Tick) + Send + 'static,
{
    let mut clock = OutputClock::new(cfg.cadence)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let stats = Arc::new(OutputLaneStats::default());
    let own_stop = StopSignal::new();
    let thread_name = format!("clock-out-{}", cfg.name);
    let name = cfg.name.clone();
    let body_stats = Arc::clone(&stats);
    let body_stop = own_stop.clone();
    let thread = clock_thread::spawn(&thread_name, move || {
        let stopped = || stop.is_stopped() || body_stop.is_stopped();
        loop {
            if stopped() {
                return;
            }
            let index = clock.next_index();
            let deadline = cfg.deadline_nanos(&clock, index);
            pace(cfg.pacing, deadline, time.as_ref(), &stop, &body_stop);
            if stopped() {
                return;
            }
            let now = time.now_nanos();
            let catchup_limit =
                cfg.deadline_nanos(&clock, index.saturating_add(cfg.max_catchup_ticks));
            if now > catchup_limit {
                let due = due_index(&cfg, &clock, now).max(index);
                body_stats
                    .skipped_ticks
                    .fetch_add(due - index, Ordering::AcqRel);
                clock.skip_to(due);
            }
            let tick = clock.tick();
            let tick_deadline = cfg.deadline_nanos(&clock, tick.index);
            body_stats.lateness.record(now, tick_deadline);
            body_stats.ticks.fetch_add(1, Ordering::AcqRel);
            on_tick(tick);
            // Timeline exhaustion (P2-A1): if the next tick would not be
            // strictly later — in PTS or in deadline — every further tick
            // would be due at once with a repeated stamp. End this lane
            // explicitly instead of looping hot.
            let next = clock.next_index();
            if next == tick.index
                || clock.pts_at(next) <= tick.pts
                || cfg.deadline_nanos(&clock, next) <= tick_deadline
            {
                body_stats.exhausted.store(true, Ordering::Release);
                tracing::error!(
                    lane = %cfg.name,
                    tick = tick.index,
                    "output clock timeline exhausted (saturated PTS/deadline); lane ended"
                );
                return;
            }
        }
    })?;
    Ok(OutputLaneHandle {
        thread,
        own_stop,
        stats,
        name,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::clock::{ManualTimeSource, MonotonicTimeSource};
    use multiview_core::time::MediaTime;
    use std::sync::mpsc;

    const STEP: LanePacing = LanePacing::Stepped(Duration::from_millis(1));

    fn manual_cfg(cadence: Rational, seed: i64) -> OutputLaneClock {
        let mut cfg = OutputLaneClock::new("test", cadence, seed);
        cfg.pacing = STEP;
        cfg
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        cond()
    }

    /// Step a manual clock to each tick's deadline in turn and check the
    /// emitted index and PTS are exactly `k / fps` (integer arithmetic).
    fn exact_ticks(cadence: Rational) {
        let seed = 7_000_000_i64;
        let manual = Arc::new(ManualTimeSource::new());
        let time: Arc<dyn TimeSource> = manual.clone();
        let cfg = manual_cfg(cadence, seed);
        let reference = OutputClock::new(cadence).unwrap();
        let (tx, rx) = mpsc::channel();
        let lane = spawn_output_lane(cfg.clone(), time, StopSignal::new(), move |tick| {
            let _ = tx.send(tick);
        })
        .unwrap();
        let stats = lane.stats();
        for k in 0..120_u64 {
            manual.set(cfg.deadline_nanos(&reference, k));
            assert!(wait_for(|| stats.ticks() == k + 1), "tick {k} not emitted");
            let tick = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(tick.index, k);
            // pts_out(k) = k · den / num seconds, exactly.
            let want =
                i128::from(k) * i128::from(cadence.den) * 1_000_000_000 / i128::from(cadence.num);
            assert!((i128::from(tick.pts.as_nanos()) - want).abs() <= 1, "k={k}");
            assert_eq!(
                tick.pts,
                MediaTime::from_tick(i64::try_from(k).unwrap(), cadence)
            );
        }
        assert_eq!(stats.skipped_ticks(), 0);
        assert_eq!(lane.join_within(Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn ticks_are_exact_at_25() {
        exact_ticks(Rational::FPS_25);
    }

    #[test]
    fn ticks_are_exact_at_ntsc() {
        exact_ticks(Rational::FPS_29_97);
    }

    #[test]
    fn the_first_deadline_is_seed_plus_latch_delay_and_pts_excludes_it() {
        let seed = 3_000_000_000_i64;
        let manual = Arc::new(ManualTimeSource::new());
        let time: Arc<dyn TimeSource> = manual.clone();
        let cfg = manual_cfg(Rational::FPS_25, seed);
        let (tx, rx) = mpsc::channel();
        let lane = spawn_output_lane(cfg, time, StopSignal::new(), move |tick| {
            let _ = tx.send(tick);
        })
        .unwrap();
        let first = seed + duration_nanos(LATCH_DELAY);
        manual.set(first - 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(lane.stats().ticks(), 0, "ticked before seed + latch delay");
        manual.set(first);
        let tick = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(tick.index, 0);
        assert_eq!(tick.pts, MediaTime::ZERO);
        // Tick 1 is one period later; its PTS is exactly 40 ms.
        manual.set(first + 40_000_000);
        let tick = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((tick.index, tick.pts.as_nanos()), (1, 40_000_000));
        assert_eq!(lane.join_within(Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn a_wake_beyond_the_catchup_bound_skips_to_the_tick_due_now() {
        let manual = Arc::new(ManualTimeSource::new());
        let time: Arc<dyn TimeSource> = manual.clone();
        let cfg = manual_cfg(Rational::FPS_25, 0);
        let reference = OutputClock::new(Rational::FPS_25).unwrap();
        let (tx, rx) = mpsc::channel();
        let lane = spawn_output_lane(cfg.clone(), time, StopSignal::new(), move |tick| {
            let _ = tx.send(tick);
        })
        .unwrap();
        // Within the bound (8 periods late): no skip, ticks emitted back to back.
        manual.set(cfg.deadline_nanos(&reference, 8));
        for k in 0..=8 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().index, k);
        }
        assert_eq!(lane.stats().skipped_ticks(), 0);
        // Jump 50 ticks ahead (+ a little): the clock skips to tick 59.
        manual.set(cfg.deadline_nanos(&reference, 59) + 1_000);
        let tick = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(tick.index, 59);
        assert_eq!(tick.pts, reference.pts_at(59));
        assert_eq!(lane.stats().skipped_ticks(), 59 - 9);
        assert_eq!(lane.join_within(Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn stop_is_observed_promptly_on_a_frozen_manual_clock() {
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let run_stop = StopSignal::new();
        let lane = spawn_output_lane(
            manual_cfg(Rational::FPS_25, 1_000_000_000),
            time,
            run_stop.clone(),
            |_| {},
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        run_stop.stop();
        assert!(wait_for(|| lane.is_finished()));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(lane.join_within(Duration::from_secs(1)), Ok(()));

        // The handle's own stop, on precise pacing against a real source.
        let real: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let lane = spawn_output_lane(
            OutputLaneClock::new("stop", Rational::FPS_25, 3_600_000_000_000),
            real,
            StopSignal::new(),
            |_| {},
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        assert_eq!(lane.join_within(Duration::from_secs(2)), Ok(()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_invalid_cadence_is_refused() {
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let err = spawn_output_lane(
            OutputLaneClock::new("bad", Rational::new(0, 1), 0),
            time,
            StopSignal::new(),
            |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    fn real_lane(
        name: &str,
        time: &Arc<dyn TimeSource>,
        seed: i64,
        on_tick: impl FnMut(Tick) + Send + 'static,
    ) -> OutputLaneHandle {
        let mut cfg = OutputLaneClock::new(name, Rational::FPS_25, seed);
        cfg.latch_delay = Duration::ZERO;
        spawn_output_lane(cfg, Arc::clone(time), StopSignal::new(), on_tick).unwrap()
    }

    /// Chaos (R3): a lane whose callback wedges for 1 s skips the ticks it
    /// missed instead of bursting them; an independent lane on the same time
    /// source keeps its full count. Bounds are loose enough for a loaded
    /// macOS runner (counts, not percentiles).
    #[test]
    fn a_wedged_lane_skips_and_never_affects_an_independent_lane() {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let seed = time.now_nanos();
        let mut wedged_once = false;
        let wedged = real_lane("wedged", &time, seed, move |tick| {
            if tick.index == 2 && !wedged_once {
                wedged_once = true;
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let healthy = real_lane("healthy", &time, seed, |_| {});
        std::thread::sleep(Duration::from_millis(1_600));
        let (w, h) = (wedged.stats(), healthy.stats());
        let elapsed_ticks = u64::try_from((time.now_nanos() - seed) / 40_000_000).unwrap();
        // The healthy lane emitted (about) one tick per period throughout.
        assert!(
            h.ticks() + 4 >= elapsed_ticks.min(40),
            "healthy {} of {elapsed_ticks}",
            h.ticks()
        );
        assert!(h.ticks() <= elapsed_ticks + 1);
        assert_eq!(h.skipped_ticks(), 0);
        // The wedged lane skipped the ~25 ticks it slept through.
        assert!(w.skipped_ticks() >= 15, "skipped {}", w.skipped_ticks());
        assert!(w.ticks() + w.skipped_ticks() <= elapsed_ticks + 1);
        assert!(w.ticks() < h.ticks());
        assert_eq!(wedged.join_within(Duration::from_secs(2)), Ok(()));
        assert_eq!(healthy.join_within(Duration::from_secs(2)), Ok(()));
    }

    #[test]
    fn a_panicking_callback_ends_only_its_lane() {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let seed = time.now_nanos();
        let doomed = real_lane("doomed", &time, seed, |tick| {
            assert!(tick.index < 3, "lane callback failure");
        });
        let healthy = real_lane("survivor", &time, seed, |_| {});
        assert!(wait_for(|| doomed.is_finished()));
        let at_panic = healthy.stats().ticks();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!healthy.is_finished());
        assert!(healthy.stats().ticks() >= at_panic + 3, "survivor stalled");
        assert_eq!(doomed.stats().ticks(), 4);
        match doomed.join_within(Duration::from_secs(1)) {
            Err(LaneJoinError::Panicked(message)) => {
                assert!(message.contains("lane callback failure"), "{message}");
            }
            other => panic!("expected a captured panic, got {other:?}"),
        }
        assert_eq!(healthy.join_within(Duration::from_secs(2)), Ok(()));
    }

    #[test]
    fn a_callback_that_never_returns_is_detached_within_the_grace() {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let seed = time.now_nanos();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let lane = real_lane("stuck", &time, seed, move |_| {
            let _ = entered_tx.send(());
            // Wedged until the test releases it (or drops the sender).
            let _ = release_rx.lock().map(|rx| rx.recv());
        });
        // The callback is running (wedged) before the stop.
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        assert_eq!(
            lane.join_within(Duration::from_millis(200)),
            Err(LaneJoinError::Detached)
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_secs(2),
            "{took:?}"
        );
        // Released, the detached thread observes its stop and ends.
        drop(release_tx);
    }

    #[test]
    fn run_stop_is_bounded_with_precise_pacing_on_a_frozen_clock() {
        // A frozen manual source never reaches tick 0's deadline; Precise
        // pacing must still observe the RUN stop (not only the lane's own).
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let run_stop = StopSignal::new();
        let lane = spawn_output_lane(
            OutputLaneClock::new("frozen", Rational::FPS_25, 0),
            time,
            run_stop.clone(),
            |_| {},
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!lane.is_finished());
        let started = Instant::now();
        run_stop.stop();
        assert!(wait_for(|| lane.is_finished()));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(lane.stats().ticks(), 0);
        assert_eq!(lane.join_within(Duration::from_secs(1)), Ok(()));
    }

    #[test]
    fn a_zero_step_is_clamped_and_never_spins() {
        struct Counting(Arc<ManualTimeSource>, Arc<AtomicU64>);
        impl TimeSource for Counting {
            fn now_nanos(&self) -> i64 {
                self.1.fetch_add(1, Ordering::Relaxed);
                self.0.now_nanos()
            }
        }
        let manual = Arc::new(ManualTimeSource::new());
        let mut cfg = OutputLaneClock::new("zero", Rational::FPS_25, 0);
        cfg.pacing = LanePacing::Stepped(Duration::ZERO);
        let reads = Arc::new(AtomicU64::new(0));
        let counting: Arc<dyn TimeSource> = Arc::new(Counting(manual, Arc::clone(&reads)));
        let lane = spawn_output_lane(cfg, counting, StopSignal::new(), |_| {}).unwrap();
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        // Bound the reads by the MEASURED elapsed time: a >= 1 ms step reads
        // the source about twice per elapsed millisecond (a stall of the test
        // thread only raises the allowance); a busy loop reads millions.
        let n = reads.load(Ordering::Relaxed);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let allowed = elapsed_ms.saturating_add(10).saturating_mul(4);
        assert!(n <= allowed, "{n} time reads in {elapsed_ms} ms: spinning");
        assert_eq!(lane.join_within(Duration::from_secs(1)), Ok(()));
    }

    /// P2-A1: on extreme seeds/time the timeline saturates; the lane ends
    /// itself (exhausted) after a bounded number of callbacks, never a hot
    /// loop of identical stamps.
    #[test]
    fn a_saturated_timeline_ends_the_lane_after_bounded_callbacks() {
        for (seed, now) in [(0, i64::MAX), (i64::MIN, i64::MAX), (i64::MIN, 0)] {
            let manual = Arc::new(ManualTimeSource::new());
            manual.set(now);
            let time: Arc<dyn TimeSource> = manual;
            let mut cfg = OutputLaneClock::new("sat", Rational::FPS_25, seed);
            cfg.latch_delay = Duration::ZERO;
            cfg.pacing = STEP;
            let calls = Arc::new(AtomicU64::new(0));
            let counted = Arc::clone(&calls);
            let lane = spawn_output_lane(cfg, time, StopSignal::new(), move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let n = calls.load(Ordering::Relaxed);
            if now == i64::MAX {
                assert!(wait_for(|| lane.is_finished()), "seed {seed}: lane ended");
                assert!(lane.stats().exhausted(), "seed {seed}");
                assert!(n <= 2, "seed {seed}: {n} callbacks");
            } else {
                // seed i64::MIN, now 0: paced normally (one catch-up tick);
                // never a hot loop.
                assert!(n <= 2, "seed {seed}: {n} callbacks");
            }
            assert_eq!(lane.join_within(Duration::from_secs(1)), Ok(()));
        }
    }

    #[test]
    fn join_within_accepts_an_unbounded_grace() {
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let run_stop = StopSignal::new();
        run_stop.stop();
        let lane =
            spawn_output_lane(manual_cfg(Rational::FPS_25, 0), time, run_stop, |_| {}).unwrap();
        assert!(wait_for(|| lane.is_finished()));
        assert_eq!(lane.join_within(Duration::MAX), Ok(()));
    }

    #[test]
    fn due_index_is_bounded_on_extreme_inputs() {
        let clock = OutputClock::new(Rational::FPS_29_97).unwrap();
        let fast = OutputClock::new(Rational::new(120, 1)).unwrap();
        for seed in [i64::MIN, -1, 0, 1, i64::MAX] {
            for now in [i64::MIN, 0, i64::MAX] {
                for delay in [Duration::ZERO, LATCH_DELAY, Duration::MAX] {
                    for c in [&clock, &fast] {
                        let mut cfg = OutputLaneClock::new("x", c.cadence(), seed);
                        cfg.latch_delay = delay;
                        let started = Instant::now();
                        let k = due_index(&cfg, c, now);
                        assert!(started.elapsed() < Duration::from_millis(50));
                        // Never past the source: a returned tick > 0 is due.
                        if k > 0 {
                            assert!(
                                cfg.deadline_nanos(c, k) <= now
                                    || cfg.deadline_nanos(c, k) == i64::MAX
                            );
                        }
                    }
                }
            }
        }
        // seed = i64::MIN: deadlines saturate; the result is the exact floor
        // of the i128 inversion, clamped, with no loop toward u64::MAX.
        let cfg = OutputLaneClock::new("x", Rational::FPS_25, i64::MIN);
        let k = due_index(&cfg, &OutputClock::new(Rational::FPS_25).unwrap(), i64::MAX);
        assert!(k > 0);
    }

    #[test]
    fn due_index_is_the_exact_floor() {
        let cfg = OutputLaneClock::new("x", Rational::FPS_29_97, 5);
        let clock = OutputClock::new(Rational::FPS_29_97).unwrap();
        for k in [0_u64, 1, 2, 999, 1_000, 30_000, 1_000_000] {
            let d = cfg.deadline_nanos(&clock, k);
            assert_eq!(due_index(&cfg, &clock, d), k);
            assert_eq!(due_index(&cfg, &clock, d + 1), k);
            if k > 0 {
                assert_eq!(due_index(&cfg, &clock, d - 1), k - 1);
            }
        }
        assert_eq!(due_index(&cfg, &clock, 0), 0);
    }
}
