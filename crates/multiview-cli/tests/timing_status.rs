//! DEV-C1 (ADR-M010): the `timing.status` publication — `{stream_id, epoch,
//! link_offset_ns, clock_source, clock_quality, groups}` pushed onto the
//! engine's drop-oldest outbound broadcast (the same channel the control WS
//! pump forwards, conflated latest-wins per ADR-RT007) at ~1 Hz.
//!
//! Pins:
//! * `publish_once` publishes a correct `Event::TimingStatus` AND mirrors the
//!   epoch into the shared HLS PDT cell (one anchor, every surface agrees);
//! * the event is conflated (ring-excluded latest-wins telemetry);
//! * isolation (inv #10): a never-reading subscriber lags (drop-oldest) and
//!   can never block the publisher;
//! * the spawned task anchors lazily off the run's epoch-anchor slot and
//!   publishes without ever touching the engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::timing_status::{self, TimingStatusOptions};
use multiview_control::EngineStateSnapshot;
use multiview_engine::epoch::{
    EpochAnchor, EpochPolicy, EpochSampler, EpochSamplerConfig, WallClockSampler, WallSample,
};
use multiview_engine::sysref::{NtpQuery, NtpReading, SystemRefConfig};
use multiview_engine::{EnginePublisher, ManualTimeSource, StopSignal, TryRecvError};
use multiview_events::{ClockQuality, ClockSource, Event};
use multiview_output::SharedEpoch;

/// A deterministic wall sampler.
struct FakeWall {
    mono_mid_ns: i64,
    wall_ns: i64,
}

impl WallClockSampler for FakeWall {
    fn sample(&mut self) -> WallSample {
        WallSample {
            mono_before_ns: self.mono_mid_ns,
            wall_ns: self.wall_ns,
            mono_after_ns: self.mono_mid_ns,
        }
    }
}

struct NoNtp;

impl NtpQuery for NoNtp {
    fn read(&mut self) -> Option<NtpReading> {
        None
    }
}

fn sampler(seed_nanos: i64, wall_ns: i64) -> EpochSampler<FakeWall, NoNtp> {
    EpochSampler::new(
        seed_nanos,
        FakeWall {
            mono_mid_ns: seed_nanos,
            wall_ns,
        },
        NoNtp,
        // The assumed-locked badge classifier + a UTC PHC fixture (no PTP is
        // attached in these tests); the conservative epoch-path default is
        // pinned separately by the finding-3 test below, which uses
        // `EpochSamplerConfig::default()`.
        EpochSamplerConfig {
            policy: EpochPolicy::new_default(),
            sys: SystemRefConfig::new_default(),
            ptp_utc_offset_ns: 0,
        },
    )
}

#[test]
fn publish_once_emits_a_correct_timing_status_and_mirrors_the_hls_epoch() {
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let mut s = sampler(0, 1_781_049_600_000_000_000);

    let status = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 150_000_000);

    let evt = sub.try_recv().expect("one published event");
    let Event::TimingStatus(ts) = &*evt.event else {
        panic!("expected timing.status, got {:?}", evt.event);
    };
    assert_eq!(ts.stream_id, "main");
    assert_eq!(ts.link_offset_ns, 150_000_000);
    assert_eq!(ts.epoch, status.epoch);
    assert_eq!(
        ts.epoch.wall_at(0),
        1_781_049_600_000_000_000,
        "tick 0 maps to the sampled wall instant (seed == mono_mid here)"
    );
    assert_eq!(ts.clock_source, ClockSource::System);
    assert_eq!(ts.clock_quality, ClockQuality::Locked);
    assert!(ts.groups.is_empty(), "no sync groups are measured yet");
    // Conflated latest-wins telemetry, excluded from the lossless replay ring.
    assert!(evt.event.is_conflated());
    // The SAME epoch feeds the HLS PDT path — one anchor, every surface agrees.
    assert_eq!(hls_epoch.get(), Some(status.epoch));
}

#[test]
fn a_build_without_an_ntp_reading_publishes_a_non_locked_quality_by_default() {
    // Review finding 3: a non-`ntp` build has no kernel discipline read at
    // all (`SysNtpQuery` yields `None`), so the EPOCH path's default config
    // must publish a conservative quality — never an ASSUMED "locked" on a
    // possibly-undisciplined host. (The overlay badge keeps its own
    // deployment-assumption default; only the machine-readable timing.status
    // feed is conservative.)
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let mut s = EpochSampler::new(
        0,
        FakeWall {
            mono_mid_ns: 0,
            wall_ns: 1_781_049_600_000_000_000,
        },
        NoNtp,
        EpochSamplerConfig::default(),
    );

    let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);

    let evt = sub.try_recv().expect("one published event");
    let Event::TimingStatus(ts) = &*evt.event else {
        panic!("expected timing.status, got {:?}", evt.event);
    };
    assert_ne!(
        ts.clock_quality,
        ClockQuality::Locked,
        "no measurement available must never publish an assumed LOCKED quality"
    );
    assert_eq!(
        ts.clock_quality,
        ClockQuality::Freerun,
        "the honest no-measurement state is freerun"
    );
    assert_eq!(ts.clock_source, ClockSource::System);
}

#[test]
fn the_configured_ptp_utc_offset_reaches_the_sampler_config() {
    // Review finding 1, the WIRING half: the validated `[timing]
    // ptp_utc_offset_s` knob must actually flow into the sampler the spawned
    // task constructs — a UTC-PHC deployment that sets `0` must not be
    // silently run with the 37 s default. Every other epoch-path default (the
    // conservative classifier, the standard re-anchor policy) is preserved.
    let options = TimingStatusOptions {
        stream_id: "main".to_owned(),
        link_offset_ns: 0,
        ptp_phc: Some("/dev/ptp0".to_owned()),
        ptp_utc_offset_ns: 0,
    };
    assert_eq!(
        options.epoch_sampler_config(),
        EpochSamplerConfig {
            ptp_utc_offset_ns: 0,
            ..EpochSamplerConfig::new_default()
        },
        "the configured TAI−UTC offset must override exactly the timescale field"
    );
}

#[test]
fn a_stalled_subscriber_lags_and_never_blocks_the_publisher() {
    // Ring depth 2; publish 10 epochs while the subscriber never reads. The
    // publisher must complete every publish (drop-oldest), and the subscriber
    // observes a Lagged gap — never a full ring back-pressuring the producer.
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(2);
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let mut s = sampler(0, 1_781_049_600_000_000_000);
    for _ in 0..10 {
        let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);
    }
    match sub.try_recv() {
        Err(TryRecvError::Lagged(missed)) => {
            assert!(missed >= 8, "drop-oldest must have shed the backlog");
        }
        other => panic!("expected a Lagged gap, got {other:?}"),
    }
    // After the lag the subscriber resumes at the oldest retained event.
    assert!(sub.try_recv().is_ok());
}

#[test]
fn a_stepped_epoch_bumps_the_hls_generation_for_the_discontinuity_seam() {
    // Review finding 4: an epoch STEP is a Class-2-like re-anchor
    // (wall-clock-sync §3) — `publish_once` must publish it through the
    // stepped seam (a new SharedEpoch generation) so the HLS driver marks the
    // next closed segment `EXT-X-DISCONTINUITY`; hold/slew keeps the
    // generation.
    struct SteppingWall {
        calls: u32,
    }
    impl WallClockSampler for SteppingWall {
        fn sample(&mut self) -> WallSample {
            self.calls += 1;
            // First sample anchors; the second jumps 1 s (gross step); the
            // third holds at the stepped instant.
            let wall = if self.calls == 1 {
                1_781_049_600_000_000_000
            } else {
                1_781_049_601_000_000_000
            };
            WallSample {
                mono_before_ns: 0,
                wall_ns: wall,
                mono_after_ns: 0,
            }
        }
    }
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
    let hls_epoch = SharedEpoch::new();
    let mut s = EpochSampler::new(
        0,
        SteppingWall { calls: 0 },
        NoNtp,
        EpochSamplerConfig::default(),
    );

    let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);
    let (_, gen_anchored) = hls_epoch
        .get_with_generation()
        .expect("anchored epoch published");

    let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);
    let (_, gen_stepped) = hls_epoch
        .get_with_generation()
        .expect("stepped epoch published");
    assert_ne!(
        gen_anchored, gen_stepped,
        "a STEP must bump the epoch generation (the HLS discontinuity seam)"
    );

    let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);
    let (_, gen_held) = hls_epoch
        .get_with_generation()
        .expect("held epoch published");
    assert_eq!(
        gen_stepped, gen_held,
        "hold/slew must keep the generation (no discontinuity)"
    );
}

#[tokio::test]
async fn the_spawned_task_publishes_from_the_anchor_slot() {
    let publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>> =
        Arc::new(EnginePublisher::new(8));
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let stop = StopSignal::new();

    // The run publishes its anchor (seed + the run's monotonic source) into the
    // slot when the clock seeds; the task picks it up lazily.
    let slot = timing_status::anchor_slot();
    let time: Arc<dyn multiview_engine::TimeSource> = Arc::new(ManualTimeSource::new());
    slot.store(Some(Arc::new(EpochAnchor::new(time, 0))));

    let handle = timing_status::spawn(
        Arc::clone(&publisher),
        Arc::clone(&slot),
        hls_epoch.clone(),
        TimingStatusOptions {
            stream_id: "main".to_owned(),
            link_offset_ns: 150_000_000,
            ptp_phc: None,
            ptp_utc_offset_ns: multiview_engine::epoch::DEFAULT_PTP_UTC_OFFSET_NS,
        },
        stop.clone(),
    );

    // The first publication arrives within the first sampling period.
    let evt = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("a timing.status within the first period")
        .expect("subscription alive");
    let Event::TimingStatus(ts) = &*evt.event else {
        panic!("expected timing.status, got {:?}", evt.event);
    };
    assert_eq!(ts.stream_id, "main");
    assert_eq!(ts.link_offset_ns, 150_000_000);
    assert!(hls_epoch.get().is_some(), "the HLS PDT cell is fed too");

    stop.stop();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("task stops on the StopSignal")
        .expect("task join");
}
