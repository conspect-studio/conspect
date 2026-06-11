//! Tests for the PTP **PHC sampling** seam (the off-by-default `ptp` feature):
//! the `PhcSource` -> `PtpSample` mapping and the `PhcSampler` that feeds a
//! `ReferenceTracker` and publishes its status — all over an **injected** fake
//! PHC source, so the sampling-and-discipline path is exercised deterministically
//! with no PTP NIC.
//!
//! The single *live* PHC read against a real `/dev/ptpN` (`RealPhcSource`) is
//! compile-verified by the feature build and exercised only by the `#[ignore]`d
//! `live_phc_*` test below: this environment has no PTP-capable NIC, so the live
//! read is a hardware tier (run it on a host with a PHC via
//! `cargo test -p multiview-engine --features ptp -- --ignored live_phc`).
#![cfg(feature = "ptp")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_engine::ptp::phc::{PhcError, PhcReading, PhcSampler, PhcSource};
use multiview_engine::ptp::{LockState, ReferenceConfig, ServoConfig};

/// A fully controlled fake PHC source: it replays a scripted queue of readings
/// (or errors), so a test owns exactly what each `read()` returns.
struct FakePhc {
    /// Scripted results, consumed front-to-back; a `None` once exhausted.
    script: std::collections::VecDeque<Result<PhcReading, PhcError>>,
}

impl FakePhc {
    fn new(readings: Vec<Result<PhcReading, PhcError>>) -> Self {
        Self {
            script: readings.into_iter().collect(),
        }
    }
}

impl PhcSource for FakePhc {
    fn read(&mut self) -> Result<PhcReading, PhcError> {
        self.script
            .pop_front()
            .unwrap_or(Err(PhcError::Read("script exhausted".into())))
    }
}

/// Build a reading whose system-bracket midpoint is `local_ns` and whose PHC
/// instant is `master_ns`, with a tiny symmetric bracket (so offset == local -
/// master and delay == half the bracket).
fn reading(local_ns: i64, master_ns: i64, bracket_ns: i64) -> PhcReading {
    let half = bracket_ns / 2;
    PhcReading {
        sys_before_ns: local_ns - half,
        phc_ns: master_ns,
        sys_after_ns: local_ns + half,
    }
}

fn cfg() -> ReferenceConfig {
    ReferenceConfig {
        servo: ServoConfig {
            alpha_recip: 2,
            step_threshold_ns: 1_000_000,
            delay_outlier_pct: 0,
        },
        lock_tolerance_ns: 50_000,
        lock_samples: 3,
        stale_after_ns: 2_000_000_000,
        holdover_window_ns: 5_000_000_000,
    }
}

#[test]
fn reading_maps_offset_and_delay() {
    // local midpoint 1_000_000, master 999_000 -> offset = +1000 (local ahead).
    // bracket 400 -> delay = 200.
    let r = reading(1_000_000, 999_000, 400);
    assert_eq!(r.offset_ns(), 1_000, "offset is local midpoint - master");
    assert_eq!(r.delay_ns(), 200, "delay is half the bracket span");
    let s = r.to_sample();
    assert_eq!(s.offset_ns, 1_000);
    assert_eq!(s.delay_ns, 200);
}

#[test]
fn negative_bracket_clamps_delay() {
    // An out-of-order bracket (after < before) must not yield a negative delay.
    let r = PhcReading {
        sys_before_ns: 1_000,
        phc_ns: 0,
        sys_after_ns: 500,
    };
    assert_eq!(r.delay_ns(), 0);
}

#[test]
fn sampler_locks_then_holds_over_then_freeruns() {
    let mut sampler = PhcSampler::new(
        FakePhc::new(vec![
            Ok(reading(1_000_000, 1_000_000, 100)), // offset 0
            Ok(reading(2_000_000, 1_999_000, 100)), // offset +1000
            Ok(reading(3_000_000, 2_998_000, 100)), // offset +2000
        ]),
        cfg(),
    );
    // Initial published status is Freerun.
    let handle = sampler.status_handle();
    let initial = handle.latest().expect("initial status published");
    assert_eq!(initial.state, LockState::Freerun);

    // Three good readings -> Locked. The reader sees the published status track.
    sampler.sample_once(0);
    assert_eq!(sampler.state(), LockState::Acquiring);
    sampler.sample_once(100_000_000);
    sampler.sample_once(200_000_000);
    assert_eq!(sampler.state(), LockState::Locked);
    let s = handle.latest().expect("status published");
    assert_eq!(s.state, LockState::Locked);
    assert!(s.disciplined);

    // Script exhausted: subsequent reads error -> staleness advances. After the
    // stale window the reference coasts into Holdover, then past the holdover
    // window it is abandoned to Freerun.
    sampler.sample_once(2_500_000_000);
    assert_eq!(sampler.state(), LockState::Holdover);
    sampler.sample_once(6_000_000_000);
    assert_eq!(sampler.state(), LockState::Freerun);
    let s = handle.latest().expect("status published");
    assert_eq!(s.state, LockState::Freerun);
    assert!(!s.disciplined);
}

#[test]
fn read_error_does_not_panic_and_advances_staleness() {
    // A source that always errors keeps the reference in Freerun (it never
    // anchors) and never panics.
    let mut sampler = PhcSampler::new(
        FakePhc::new(vec![Err(PhcError::Read("boom".into()))]),
        cfg(),
    );
    let s = sampler.sample_once(0);
    assert_eq!(s.state, LockState::Freerun);
    let s = sampler.sample_once(10_000_000_000);
    assert_eq!(s.state, LockState::Freerun);
}

#[test]
fn published_status_is_wait_free_readable_by_a_clone() {
    // The badge consumer holds a cloned handle and reads the latest snapshot
    // without ever blocking the sampler (invariant #10): a plain atomic load.
    let mut sampler = PhcSampler::new(
        FakePhc::new(vec![
            Ok(reading(0, 0, 50)),
            Ok(reading(1_000, 0, 50)),
            Ok(reading(2_000, 0, 50)),
        ]),
        cfg(),
    );
    let badge = sampler.status_handle();
    for i in 0..3i64 {
        sampler.sample_once(i * 100_000_000);
    }
    let snapshot = badge.latest().expect("badge sees a snapshot");
    assert_eq!(snapshot.state, sampler.state());
}

/// **Live PHC read** — requires a real PTP Hardware Clock; ignored by default.
///
/// This environment (CI / devcontainer) has no PTP-capable NIC, so the actual
/// `clock_gettime` on `/dev/ptp0` cannot run here. On a host with a PHC, run:
/// `cargo test -p multiview-engine --features ptp -- --ignored live_phc_reads_dev_ptp0`.
/// It opens `/dev/ptp0`, takes one real reading, and asserts the system bracket
/// is sane (non-decreasing) and a sample is produced.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a real PTP Hardware Clock (/dev/ptp0); no PTP NIC in CI"]
fn live_phc_reads_dev_ptp0() {
    use multiview_engine::ptp::phc::RealPhcSource;
    let mut src = RealPhcSource::open("/dev/ptp0").expect("open /dev/ptp0");
    let reading = src.read().expect("read PHC");
    assert!(
        reading.sys_after_ns >= reading.sys_before_ns,
        "system bracket must be non-decreasing"
    );
    // The reading produces a finite sample.
    let sample = reading.to_sample();
    assert!(sample.delay_ns >= 0, "delay is non-negative");
    let _ = sample.offset_ns;
}
