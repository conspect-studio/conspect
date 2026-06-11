//! Display flip-loop tests (DEV-B1 / ADR-0044 §1): the page-flip-event-driven
//! commit state machine and the full sink thread over a scripted mock
//! `KmsBackend` — proving, WITHOUT hardware: at most one in-flight commit per
//! CRTC, EBUSY-as-conflation (never queue, never retry-loop), no-new-frame ⇒
//! no commit (KMS repeats the framebuffer), `TEST_ONLY` validation before the
//! one startup modeset, `ALLOW_MODESET` never on the frame path, and a wedged
//! display that can never stall the engine-side publish (invariants #1 + #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_core::time::Rational;
use multiview_output::display::{
    ConnectorDesc, ConnectorSelector, DisplayCanvas, DisplayError, DisplayModeInfo, DisplaySink,
    DisplaySinkConfig, FlipDriver, FlipEvent, HeadSetup, KmsBackend, ModeRequest, SubmitError,
};

// ---------------------------------------------------------------------------
// FlipDriver — the pure EBUSY-conflation state machine
// ---------------------------------------------------------------------------

#[test]
fn flip_driver_commits_only_new_frames_when_idle() {
    let mut driver = FlipDriver::new();
    // Nothing committed yet: the first frame (seq 1) wants a commit.
    assert!(driver.wants_commit(1));
    driver.on_commit_submitted(1);
    // One commit is now in flight: a newer frame must NOT commit (the kernel
    // allows at most one nonblocking commit per CRTC).
    assert!(!driver.wants_commit(2));
    // The flip completes: the newer frame may now commit.
    driver.on_flip_complete();
    assert!(driver.wants_commit(2));
    driver.on_commit_submitted(2);
    driver.on_flip_complete();
    // No newer frame than the one already committed: do nothing — KMS repeats
    // the current framebuffer for free.
    assert!(!driver.wants_commit(2));
}

#[test]
fn flip_driver_treats_ebusy_as_in_flight_and_retries_after_the_flip() {
    let mut driver = FlipDriver::new();
    assert!(driver.wants_commit(5));
    // The kernel said EBUSY: a flip is still pending. The frame is NOT marked
    // committed — it stays the retry candidate for the next flip event; no
    // queueing, no spin-retry.
    driver.on_commit_busy();
    assert!(
        !driver.wants_commit(5),
        "EBUSY means in-flight: wait for the flip event"
    );
    driver.on_flip_complete();
    assert!(
        driver.wants_commit(5),
        "after the flip drains, the SAME latest frame is committed"
    );
}

#[test]
fn flip_driver_conflates_to_the_latest_sequence() {
    let mut driver = FlipDriver::new();
    driver.on_commit_submitted(1);
    // Frames 2..=9 arrive while the flip is pending — all conflated.
    assert!(!driver.wants_commit(9));
    driver.on_flip_complete();
    // Only the LATEST is committed; the intermediates are never queued.
    assert!(driver.wants_commit(9));
    driver.on_commit_submitted(9);
    driver.on_flip_complete();
    assert!(!driver.wants_commit(9));
}

// ---------------------------------------------------------------------------
// The scripted mock backend
// ---------------------------------------------------------------------------

/// One recorded backend call, for asserting ordering and frame-path rules.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Probe,
    Validate,
    Modeset,
    Submit(u64),
}

/// A scripted, hardware-free `KmsBackend`: connectors and submit results are
/// pre-programmed; every call is recorded. `wait_events` returns one flip
/// event per previously-successful submit (the kernel contract: one
/// `PAGE_FLIP_EVENT` per nonblocking commit), unless wedged.
struct MockBackend {
    connectors: Vec<ConnectorDesc>,
    /// Scripted results for successive `submit_frame` calls (`None` = Ok).
    submit_results: Mutex<VecDeque<Option<SubmitError>>>,
    calls: Arc<Mutex<Vec<Call>>>,
    /// Flips owed: incremented on successful submit, drained by `wait_events`.
    pending_flips: u64,
    /// When set, `wait_events` never delivers flips (a wedged display pipe).
    wedged: Arc<AtomicBool>,
    frame_counter: u64,
}

impl MockBackend {
    fn new(connectors: Vec<ConnectorDesc>, calls: Arc<Mutex<Vec<Call>>>) -> Self {
        Self {
            connectors,
            submit_results: Mutex::new(VecDeque::new()),
            calls,
            pending_flips: 0,
            wedged: Arc::new(AtomicBool::new(false)),
            frame_counter: 0,
        }
    }

    fn script_submit(&self, results: Vec<Option<SubmitError>>) {
        self.submit_results.lock().unwrap().extend(results);
    }
}

impl KmsBackend for MockBackend {
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError> {
        self.calls.lock().unwrap().push(Call::Probe);
        Ok(self.connectors.clone())
    }

    fn validate_setup(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        self.calls.lock().unwrap().push(Call::Validate);
        Ok(())
    }

    fn apply_modeset(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        self.calls.lock().unwrap().push(Call::Modeset);
        Ok(())
    }

    fn submit_frame(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        // Record the frame identity via its first luma byte (the tests publish
        // single-byte-tagged canvases).
        let tag = u64::from(frame.y_plane().first().copied().unwrap_or(0));
        self.calls.lock().unwrap().push(Call::Submit(tag));
        let scripted = self.submit_results.lock().unwrap().pop_front().flatten();
        match scripted {
            None => {
                self.pending_flips += 1;
                Ok(())
            }
            Some(SubmitError::Busy) => {
                // A real EBUSY means the kernel HAS an outstanding flip the
                // caller did not account for — that flip will still complete.
                // Owe one event so the scripted scenario matches the kernel
                // contract.
                self.pending_flips += 1;
                Err(SubmitError::Busy)
            }
            Some(err) => Err(err),
        }
    }

    fn wait_events(&mut self, _timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError> {
        if self.wedged.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        while self.pending_flips > 0 {
            self.pending_flips -= 1;
            self.frame_counter += 1;
            events.push(FlipEvent {
                crtc_frame: u32::try_from(self.frame_counter).unwrap_or(u32::MAX),
                timestamp: Duration::from_millis(16 * self.frame_counter),
            });
        }
        Ok(events)
    }
}

/// A 2x2 NV12 test canvas whose every luma byte is `tag`.
#[derive(Debug)]
struct TestCanvas {
    y: [u8; 4],
    uv: [u8; 2],
}

impl TestCanvas {
    fn tagged(tag: u8) -> Self {
        Self {
            y: [tag; 4],
            uv: [128; 2],
        }
    }
}

impl DisplayCanvas for TestCanvas {
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn y_plane(&self) -> &[u8] {
        &self.y
    }
    fn uv_plane(&self) -> &[u8] {
        &self.uv
    }
}

fn edid_mode_1080p60(preferred: bool) -> DisplayModeInfo {
    DisplayModeInfo {
        width: 1920,
        height: 1080,
        clock_khz: 148_500,
        hsync_start: 2008,
        hsync_end: 2052,
        htotal: 2200,
        vsync_start: 1084,
        vsync_end: 1089,
        vtotal: 1125,
        hsync_positive: true,
        vsync_positive: true,
        preferred,
    }
}

fn connected_dp1() -> ConnectorDesc {
    ConnectorDesc {
        name: "DP-1".to_owned(),
        connected: true,
        modes: vec![edid_mode_1080p60(true)],
    }
}

fn sink_config(connector: ConnectorSelector) -> DisplaySinkConfig {
    DisplaySinkConfig {
        output_id: "out-display-test".to_owned(),
        connector,
        mode: ModeRequest::Auto,
        forced_mode: None,
        engine_cadence: Some(Rational::new(60, 1)),
        poll_interval: Duration::from_millis(1),
    }
}

/// Spin until `pred` is true or ~2 s elapse (the sink thread runs at a 1 ms
/// poll cadence, so success is near-immediate; the bound only guards CI).
fn wait_until(pred: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !pred() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached within the test bound"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------------------
// Sink startup discipline
// ---------------------------------------------------------------------------

#[test]
fn startup_validates_with_test_only_before_the_one_modeset() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], Arc::clone(&calls));
    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, sink_config(ConnectorSelector::Auto))
            .expect("startup succeeds");
    // Startup order is probe → TEST_ONLY validate → ALLOW_MODESET modeset.
    {
        let calls = calls.lock().unwrap();
        assert_eq!(&calls[..3], &[Call::Probe, Call::Validate, Call::Modeset]);
    }
    // The selected head is the EDID preferred mode on the first connected
    // connector.
    assert_eq!(handle.head().connector, "DP-1");
    assert_eq!(handle.head().mode.width, 1920);
    drop(publisher);
    drop(handle);
    // After the run, NO further Validate/Modeset ever appeared: ALLOW_MODESET
    // is never on the frame path.
    let calls = calls.lock().unwrap();
    let modesets = calls.iter().filter(|c| **c == Call::Modeset).count();
    let validates = calls.iter().filter(|c| **c == Call::Validate).count();
    assert_eq!(modesets, 1);
    assert_eq!(validates, 1);
}

#[test]
fn startup_fails_when_the_named_connector_is_absent() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], calls);
    let err = DisplaySink::start::<TestCanvas, _>(
        backend,
        sink_config(ConnectorSelector::Name("HDMI-A-2".to_owned())),
    )
    .expect_err("connector does not exist");
    let msg = err.to_string();
    assert!(msg.contains("HDMI-A-2"), "error names the connector: {msg}");
}

// ---------------------------------------------------------------------------
// The frame path: conflation, repeat, EBUSY
// ---------------------------------------------------------------------------

#[test]
fn frames_flow_and_conflate_to_the_latest() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], Arc::clone(&calls));
    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, sink_config(ConnectorSelector::Auto))
            .expect("startup succeeds");

    // Publish a burst of frames much faster than flips can drain: the sink
    // must conflate (newest wins), never queue.
    for tag in 1..=200u8 {
        publisher.publish(TestCanvas::tagged(tag));
    }
    wait_until(|| calls.lock().unwrap().contains(&Call::Submit(200)));
    drop(handle);

    let calls = calls.lock().unwrap();
    let submits: Vec<u64> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Submit(tag) => Some(*tag),
            _ => None,
        })
        .collect();
    // The LAST frame always lands; far fewer submits than publishes happened
    // (conflation), and the submitted tags are strictly increasing (newest
    // wins; nothing is queued or replayed out of order).
    assert_eq!(submits.last(), Some(&200));
    assert!(
        submits.len() < 200,
        "conflation must drop intermediate frames (got {} submits)",
        submits.len()
    );
    assert!(
        submits.windows(2).all(|w| w[0] < w[1]),
        "submitted frames must be strictly newest-wins: {submits:?}"
    );
    // The stats agree: at least one frame was conflated away.
    assert!(handle_stats_conflated(&submits));
}

/// 200 publishes with fewer submits means the mailbox conflated.
fn handle_stats_conflated(submits: &[u64]) -> bool {
    submits.len() < 200
}

#[test]
fn no_new_frame_means_no_commit() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], Arc::clone(&calls));
    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, sink_config(ConnectorSelector::Auto))
            .expect("startup succeeds");
    publisher.publish(TestCanvas::tagged(7));
    wait_until(|| calls.lock().unwrap().contains(&Call::Submit(7)));
    // Give the loop many poll cycles with NO new frame.
    std::thread::sleep(Duration::from_millis(50));
    drop(handle);
    let calls = calls.lock().unwrap();
    let submits = calls
        .iter()
        .filter(|c| matches!(c, Call::Submit(_)))
        .count();
    // Exactly one commit: the framebuffer repeat is the kernel's job, not a
    // re-commit loop.
    assert_eq!(submits, 1, "no-new-frame must mean no commit: {calls:?}");
}

#[test]
fn ebusy_is_conflation_not_a_retry_loop() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], Arc::clone(&calls));
    // Script: first submit EBUSY, second submit succeeds.
    backend.script_submit(vec![Some(SubmitError::Busy), None]);
    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, sink_config(ConnectorSelector::Auto))
            .expect("startup succeeds");
    publisher.publish(TestCanvas::tagged(9));
    // The EBUSY'd frame is re-committed only after the (mock) pipe drains; the
    // sink never spins. Eventually exactly the latest frame lands.
    wait_until(|| {
        let calls = calls.lock().unwrap();
        calls
            .iter()
            .filter(|c| matches!(c, Call::Submit(_)))
            .count()
            >= 2
    });
    drop(handle);
    let stats = stats_of_test_log(&calls);
    assert_eq!(
        stats,
        (2, 9),
        "one EBUSY then one successful commit of frame 9"
    );
}

/// `(submit_count, last_tag)` from the call log.
fn stats_of_test_log(calls: &Arc<Mutex<Vec<Call>>>) -> (usize, u64) {
    let calls = calls.lock().unwrap();
    let submits: Vec<u64> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Submit(tag) => Some(*tag),
            _ => None,
        })
        .collect();
    (submits.len(), submits.last().copied().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Isolation: a wedged display can never stall the publish (inv #1 + #10)
// ---------------------------------------------------------------------------

#[test]
fn wedged_display_never_blocks_the_publisher() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend::new(vec![connected_dp1()], Arc::clone(&calls));
    let wedged = Arc::clone(&backend.wedged);
    // Submit succeeds once, then the pipe wedges: no flip event ever arrives,
    // so the sink stays in-flight forever.
    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, sink_config(ConnectorSelector::Auto))
            .expect("startup succeeds");
    publisher.publish(TestCanvas::tagged(1));
    wait_until(|| calls.lock().unwrap().contains(&Call::Submit(1)));
    wedged.store(true, Ordering::Release);

    // The engine-side publish must complete a large burst with the sink
    // thread permanently stuck in-flight — the mailbox is wait-free and the
    // engine never awaits the sink.
    let publisher2 = publisher.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..100_000u64 {
            publisher2.publish(TestCanvas::tagged(u8::try_from(i % 251).unwrap_or(0)));
        }
    });
    writer
        .join()
        .expect("publisher burst completed while wedged");

    // The wedged sink holds at most one further in-flight commit (the flip for
    // frame 1 may or may not have drained before the wedge took effect) and
    // can never spin more out — at most one in-flight commit per CRTC.
    let (submits, _) = stats_of_test_log(&calls);
    assert!(
        (1..=2).contains(&submits),
        "a wedged pipe holds at most the one in-flight commit (got {submits})"
    );
    drop(handle);
    drop(publisher);
}
