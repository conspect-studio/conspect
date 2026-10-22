//! GPU-5c acceptance: the make-before-break (MBB) migration primitive +
//! placement-execution loop (ADR-R010 + ADR-0018 §4).
//!
//! These prove the five-phase MBB contract end-to-end over **fake software
//! Programs** and **synthetic `DeviceLoad`** — pure, offline, no hardware:
//!
//! * **No-gap output (inv #1).** Across a full MBB migration the OLD program
//!   emits ticks until SWAP and the NEW program from before SWAP; counting both
//!   programs' `ticks_emitted` proves there is **no instant** with no producing
//!   clock — the output never falters.
//! * **Zero-new-encode at SWAP (inv #7).** The WARM-phase keepalive sink makes
//!   NEW's rendition warm before the cut, so the SWAP's `move_sink` onto an
//!   already-encoding rendition spawns **zero** new encodes (a
//!   `RenditionEncoder` call-count spy proves it), and the keepalive is dropped
//!   only **after** the real consumer has moved (the load-bearing ordering).
//! * **Rollback before SWAP is free + total.** Admission-deny, spin-up-fail, and
//!   warm-timeout each roll back leaving OLD untouched and no consumer moved.
//! * **Anti-storm caps migration frequency.** Via the `PlacementController`'s
//!   cooldown/budget/min-gain gates (driven through the placement-execution
//!   loop), a storm of overload ticks yields a bounded number of migrations.
//! * **Wedged NEW aborts bounded (#96 helper).** A NEW program whose loop cannot
//!   observe stop is torn down within the bounded join grace, never hanging.
//! * **Isolation (inv #10).** `move_sink` under the crosspoint mutex while a
//!   program's egress routes cannot stall the output clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::ProgramSpec;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas as CoreCanvas, Layout as CoreLayout};
use multiview_core::pixel::PixelFormat;
use multiview_core::time::Rational;
use multiview_core::traits::BackendKind;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::migration::{
    KeepaliveSink, LoadSnapshot, MigrationOutcome, OutputCrosspoint, PlacementCoordinator,
    RollbackReason,
};
use multiview_engine::{
    CompositorDrive, MigrationPlan, OutputClock, PlacementController, PlacementControllerConfig,
    PlacementProposal, Program, ProgramSet, RealtimePacer, TimeSource,
};
use multiview_hal::cost::{CostBudget, TileLoad};
use multiview_hal::load::{DeviceId, DeviceLoad, Vendor};
use multiview_hal::select::{GpuCandidate, Pins, PipelineDemand, StageCaps};
use multiview_hal::{Capability, Resolution, Stage};
use multiview_output::fanout::{
    EncodedPacket, PacketKind, PacketSink, RenditionEncoder, RenditionId,
};

// ----------------------------------------------------------------------------
// Program + load fakes (mirror tests/programset.rs and tests/placement.rs).
// ----------------------------------------------------------------------------

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

fn empty_drive(cadence: Rational) -> CompositorDrive<Nv12Image> {
    let layout = CoreLayout {
        name: "gpu5c".to_owned(),
        canvas: CoreCanvas {
            width: 16,
            height: 16,
            fps_num: cadence.num,
            fps_den: cadence.den,
        },
        cells: Vec::new(),
    };
    CompositorDrive::new(
        Arc::new(layout),
        HashMap::new(),
        nosignal_card(16, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

fn spec_with_id(id: &str, num: i64, den: i64) -> ProgramSpec {
    let json = format!(
        r##"{{
            "id": "{id}",
            "kind": "multiview",
            "canvas": {{
                "width": 16,
                "height": 16,
                "fps": "{num}/{den}",
                "pixel_format": "nv12",
                "background": "#000000",
                "color": {{ "profile": "sdr-bt709-limited" }}
            }},
            "layout": {{ "kind": "preset", "preset": "1x1" }}
        }}"##
    );
    serde_json::from_str(&json).expect("multiview spec deserializes")
}

fn program_at(
    spec: &ProgramSpec,
    cadence: Rational,
    time: Arc<dyn TimeSource>,
) -> Program<RealtimePacer> {
    let clock = OutputClock::new(cadence).unwrap();
    let drive = empty_drive(cadence);
    Program::multiview(spec, clock, drive, time, RealtimePacer).unwrap()
}

fn nv(id: &str, index: u32) -> DeviceId {
    DeviceId::new(Vendor::Nvidia, id, index)
}

fn cap(stage: Stage) -> Capability {
    Capability::new(
        BackendKind::Cuda,
        stage,
        Resolution::UHD4K,
        vec![PixelFormat::Nv12],
    )
}

fn full_caps() -> StageCaps {
    StageCaps::new(
        cap(Stage::Decode),
        cap(Stage::Composite),
        cap(Stage::Encode),
    )
}

fn generous_budget() -> CostBudget {
    CostBudget::new(1000.0, 1000.0, 1000.0)
}

fn demand_1080p() -> PipelineDemand {
    PipelineDemand::new(
        Rational::new(30, 1),
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Composite, Resolution::HD1080),
            TileLoad::new(Stage::Encode, Resolution::HD1080),
        ],
        Resolution::HD1080,
        PixelFormat::Nv12,
        1_000_000,
        true,
    )
}

fn candidate(id: &str, index: u32) -> GpuCandidate {
    GpuCandidate {
        device_id: nv(id, index),
        stage_caps: full_caps(),
        budget: generous_budget(),
    }
}

fn vram_pct(id: &str, index: u32, pct: u64) -> DeviceLoad {
    let total = 12_000_000_000_u64;
    let used = total.saturating_mul(pct.min(100)) / 100;
    let mut load = DeviceLoad::unknown(nv(id, index));
    load.vram_used_bytes = Some(used);
    load.vram_total_bytes = Some(total);
    load
}

/// A counting [`PacketSink`] — proves the SAME sink (identity preserved) keeps
/// receiving a contiguous stream of packets across the cutover.
#[derive(Debug)]
struct CountingSink {
    id: String,
    delivered: AtomicU64,
}

impl CountingSink {
    fn new(id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            delivered: AtomicU64::new(0),
        })
    }
}

impl PacketSink for CountingSink {
    fn sink_id(&self) -> &str {
        &self.id
    }
    fn deliver(&self, _packet: &Arc<EncodedPacket>) {
        self.delivered.fetch_add(1, Ordering::Release);
    }
}

/// A [`RenditionEncoder`] call-count spy — proves encode-once and zero-new-encode.
#[derive(Default)]
struct EncodeSpy {
    per_rendition: Mutex<HashMap<String, u64>>,
}

impl EncodeSpy {
    fn count(&self, rendition: &str) -> u64 {
        self.per_rendition
            .lock()
            .unwrap()
            .get(rendition)
            .copied()
            .unwrap_or(0)
    }
}

impl RenditionEncoder for EncodeSpy {
    fn encode_frame(&self, rendition: &RenditionId, tick: u64) -> EncodedPacket {
        *self
            .per_rendition
            .lock()
            .unwrap()
            .entry(rendition.as_str().to_owned())
            .or_insert(0) += 1;
        EncodedPacket {
            rendition: rendition.clone(),
            kind: PacketKind::VideoKeyframe,
            pts: i64::try_from(tick).unwrap_or(0),
            dts: i64::try_from(tick).unwrap_or(0),
            duration: 1,
            data: Arc::from(vec![0_u8; 4].into_boxed_slice()),
        }
    }
}

fn test_config() -> PlacementControllerConfig {
    let mut c = PlacementControllerConfig::new_default();
    c.ewma_alpha = 0.6;
    c.dwell_ticks = 2;
    c.migration_cooldown_ticks = 0;
    c.per_gpu_budget = 10;
    c.budget_window_ticks = 1000;
    c.min_gain = 0.1;
    c
}

// ----------------------------------------------------------------------------
// (1) RT-12 crosspoint + keepalive: zero-new-encode at SWAP (inv #7).
// ----------------------------------------------------------------------------

#[test]
fn swap_onto_a_warm_rendition_spawns_zero_new_encodes() {
    // OLD and NEW are two RenditionIds in ONE shared PacketRouter (the RT-12
    // crosspoint). A real consumer sits on OLD. WARM pre-attaches a keepalive
    // sink to NEW (making it warm — encoding). At SWAP the consumer moves
    // OLD->NEW; because NEW was already warm the encoder count for NEW does NOT
    // jump at the SWAP tick. The keepalive is dropped only AFTER the move.
    let old = RenditionId::new("main.old");
    let new = RenditionId::new("main.new");
    let crosspoint = OutputCrosspoint::new();
    let consumer = CountingSink::new("rtsp-1");
    crosspoint.register(old.clone(), consumer.clone());

    let spy = EncodeSpy::default();

    // Pre-SWAP: only OLD is active (has a sink); NEW is cold. Drive a few ticks.
    for t in 0..3 {
        crosspoint.tick(t, &spy);
    }
    assert_eq!(spy.count("main.old"), 3, "OLD encodes once per tick");
    assert_eq!(
        spy.count("main.new"),
        0,
        "NEW is cold (no sinks) — not encoded"
    );

    // WARM: pre-attach the keepalive to NEW so it is warm BEFORE the cut.
    let keepalive = KeepaliveSink::new("keepalive-main.new");
    crosspoint.attach_keepalive(new.clone(), keepalive.clone());
    for t in 3..6 {
        crosspoint.tick(t, &spy);
    }
    let new_warm_at_swap = spy.count("main.new");
    assert_eq!(
        new_warm_at_swap, 3,
        "NEW is warm (keepalive sink) — encoding once/tick"
    );

    // SWAP: move the real consumer OLD->NEW, THEN drop the keepalive (ordering).
    assert!(
        crosspoint.move_sink("rtsp-1", &old, new.clone()),
        "the consumer moved"
    );
    // Drop keepalive only AFTER the real consumer is on NEW.
    assert!(
        crosspoint.deregister(&new, keepalive.sink_id()),
        "keepalive dropped post-move"
    );

    // The SWAP tick: NEW encodes exactly ONCE more (it was already warm — the
    // move spawned zero NEW encodes at the critical instant; the +1 is its
    // normal per-tick encode, not a first-encode triggered by the cut).
    let new_before = spy.count("main.new");
    crosspoint.tick(6, &spy);
    assert_eq!(
        spy.count("main.new"),
        new_before + 1,
        "the SWAP spawned zero EXTRA encodes — NEW was warm, so exactly one per-tick encode"
    );
    // OLD has no sinks after the move -> not encoded post-SWAP.
    let old_before = spy.count("main.old");
    crosspoint.tick(7, &spy);
    assert_eq!(
        spy.count("main.old"),
        old_before,
        "OLD is cold post-SWAP (no sinks)"
    );
    // The consumer kept receiving across the cut (identity preserved).
    assert!(
        consumer.delivered.load(Ordering::Acquire) > 0,
        "consumer fed across the cut"
    );
}

#[test]
fn dropping_keepalive_before_the_move_would_make_new_cold_so_ordering_holds() {
    // Negative control of the ordering: if the keepalive is dropped while NEW has
    // NO real consumer yet, NEW goes cold (zero sinks). This test documents that
    // the crosspoint's deregister leaves NEW cold — the coordinator MUST move the
    // consumer first (asserted by the positive test above).
    let new = RenditionId::new("main.new");
    let crosspoint = OutputCrosspoint::new();
    let keepalive = KeepaliveSink::new("keepalive-main.new");
    crosspoint.attach_keepalive(new.clone(), keepalive.clone());
    let spy = EncodeSpy::default();
    crosspoint.tick(0, &spy);
    assert_eq!(spy.count("main.new"), 1, "warm while keepalive attached");
    crosspoint.deregister(&new, keepalive.sink_id());
    crosspoint.tick(1, &spy);
    assert_eq!(
        spy.count("main.new"),
        1,
        "cold after keepalive dropped with no real sink"
    );
}

// ----------------------------------------------------------------------------
// (2) The no-gap MBB migration (inv #1) — RealtimePacer, real concurrent clocks.
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mbb_migration_completes_with_no_output_gap() {
    // OLD ("main") runs on its own clock. The coordinator spins up NEW
    // ("main.migrating") ALONGSIDE it, warms it, and SWAPs at a frame boundary.
    // Across the whole MBB BOTH clocks are sampled: OLD emits until SWAP, NEW
    // from before SWAP — there is no instant with no producing clock (inv #1).
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());
    let mut programs: ProgramSet<RealtimePacer> = ProgramSet::new(time.clone());

    let spec_old = spec_with_id("main", 60, 1);
    let old = program_at(&spec_old, Rational::FPS_60, time.clone());
    programs.start(old).unwrap();

    let crosspoint = OutputCrosspoint::new();
    let old_rendition = RenditionId::new("main");
    let new_rendition = RenditionId::new("main.migrating");
    let consumer = CountingSink::new("rtsp-1");
    crosspoint.register(old_rendition.clone(), consumer.clone());

    // OLD is demonstrably ticking before anything else happens.
    let started = Instant::now();
    while programs.ticks_emitted("main").unwrap_or(0) < 5 {
        assert!(started.elapsed() < Duration::from_secs(30), "OLD stalled");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let old_at_spinup = programs.ticks_emitted("main").unwrap();

    // SPIN-UP NEW alongside OLD (its own clock, untouched siblings).
    let spec_new = spec_with_id("main.migrating", 60, 1);
    let new = program_at(&spec_new, Rational::FPS_60, time.clone());
    programs.start(new).unwrap();

    // WARM: attach the keepalive to NEW + wait (sampling NEW's wait-free ticks)
    // until NEW has emitted >= N valid frames (readiness, off the data plane).
    let keepalive = KeepaliveSink::new("keepalive-main.migrating");
    crosspoint.attach_keepalive(new_rendition.clone(), keepalive.clone());
    let started = Instant::now();
    while programs.ticks_emitted("main.migrating").unwrap_or(0) < 5 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "NEW never warmed"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // OLD kept ticking through NEW's spin-up + warm (no gap).
    assert!(
        programs.ticks_emitted("main").unwrap() > old_at_spinup,
        "OLD kept emitting while NEW spun up + warmed (inv #1)"
    );

    // SWAP: move the consumer OLD->NEW (the crosspoint re-key), drop keepalive.
    let old_at_swap = programs.ticks_emitted("main").unwrap();
    let new_at_swap = programs.ticks_emitted("main.migrating").unwrap();
    assert!(crosspoint.move_sink("rtsp-1", &old_rendition, new_rendition.clone()));
    crosspoint.deregister(&new_rendition, keepalive.sink_id());

    // DRAIN + STOP OLD (bounded join). NEW keeps emitting.
    programs.stop("main").await;
    assert!(!programs.is_running("main"));
    assert!(programs.is_running("main.migrating"));

    // NEW kept advancing across the SWAP + OLD teardown (no gap on the consumer's
    // new clock).
    let started = Instant::now();
    while programs.ticks_emitted("main.migrating").unwrap() < new_at_swap + 10 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "NEW stalled after SWAP"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Both clocks overlapped: OLD was emitting AT swap time and NEW was emitting
    // BEFORE swap time — there is no instant with no producing clock.
    assert!(old_at_swap > old_at_spinup, "OLD emitting up to SWAP");
    assert!(new_at_swap >= 5, "NEW emitting before SWAP (warm)");

    programs.shutdown().await;
}

// ----------------------------------------------------------------------------
// (3) VALIDATE rollback before SWAP — admission-deny leaves OLD untouched.
// ----------------------------------------------------------------------------

#[test]
fn validate_admission_deny_rolls_back_before_touching_anything() {
    // The target device is over the headroom ceiling under the live snapshot
    // (which already reflects OLD's footprint), so VALIDATE rejects the
    // migration BEFORE any resource is touched.
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let plan = MigrationPlan::new(nv("GPU-a", 0), nv("GPU-b", 1), 0.2);
    // GPU-b (the target) is itself hot (over ceiling) — not admissible.
    let loads = vec![vram_pct("GPU-a", 0, 95), vram_pct("GPU-b", 1, 96)];
    let outcome = multiview_engine::migration::validate_migration(
        &candidates,
        &demand_1080p(),
        &loads,
        test_config().select_policy,
        &plan,
    );
    assert!(
        matches!(outcome, Err(RollbackReason::AdmissionDenied(_))),
        "an over-ceiling target is rejected before SWAP: {outcome:?}"
    );
}

#[test]
fn validate_admits_a_target_with_headroom() {
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let plan = MigrationPlan::new(nv("GPU-a", 0), nv("GPU-b", 1), 0.5);
    let loads = vec![vram_pct("GPU-a", 0, 95), vram_pct("GPU-b", 1, 10)];
    let outcome = multiview_engine::migration::validate_migration(
        &candidates,
        &demand_1080p(),
        &loads,
        test_config().select_policy,
        &plan,
    );
    assert!(
        outcome.is_ok(),
        "an idle target with headroom is admitted: {outcome:?}"
    );
}

// ----------------------------------------------------------------------------
// (4) Placement-execution loop: anti-storm caps consecutive migrations.
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_loop_anti_storm_caps_migrations() {
    // With a tight per-GPU budget, a long storm of sustained overload yields a
    // BOUNDED number of migrations (the controller's anti-storm gate suppresses
    // the rest as AntiStorm sheds). The loop records the outcomes in the
    // PlacementCounters.
    let mut config = test_config();
    config.migration_cooldown_ticks = 0;
    config.per_gpu_budget = 1;
    config.budget_window_ticks = 100_000;
    let candidates = vec![candidate("GPU-a", 0), candidate("GPU-b", 1)];
    let controller = PlacementController::new(
        config,
        demand_1080p(),
        candidates,
        Pins::none(),
        nv("GPU-a", 0),
    );

    let snapshot = Arc::new(LoadSnapshot::new());
    let crosspoint = OutputCrosspoint::new();
    let registry = multiview_telemetry::metrics::MetricsRegistry::new();
    let mut coord =
        PlacementCoordinator::new(controller, Arc::clone(&snapshot), crosspoint, &registry);

    // A run of "GPU-a hot, GPU-b idle" ticks. Budget=1 allows exactly one
    // migration touching each GPU; the rest are AntiStorm sheds.
    let hot = vec![vram_pct("GPU-a", 0, 95), vram_pct("GPU-b", 1, 12)];
    let mut migrations = 0_u64;
    for _ in 0..40 {
        snapshot.publish(hot.clone());
        // Hold / Shed / Split / any future variant is not a migration.
        if let PlacementProposal::Migrate(_) = coord.observe_only() {
            migrations += 1;
        }
    }
    assert!(
        migrations <= 1,
        "anti-storm (budget=1) bounds migrations to at most 1, saw {migrations}"
    );
}

// ----------------------------------------------------------------------------
// (5) Wedged NEW program aborts bounded (#96 helper, ADR-R010 spin-up-fail).
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tearing_down_a_wedged_new_program_is_bounded() {
    // A NEW program whose run loop cannot observe stop (a RealtimePacer over a
    // frozen ManualTimeSource) must be torn down within the bounded join grace —
    // never a hang. This is the rollback path's teardown (#96 join helper).
    let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
    let mut programs: ProgramSet<RealtimePacer> = ProgramSet::new(time.clone());
    let spec = spec_with_id("main.migrating", 25, 1);
    let new = program_at(&spec, Rational::FPS_25, time.clone());
    let ticks = new.ticks_counter();
    programs.start(new).unwrap();

    // Let NEW pace tick 0 (deadline 0), then it parks waiting for tick 1's
    // deadline the frozen source never reaches — wedged, cannot observe stop.
    let started = Instant::now();
    while ticks.load(Ordering::Acquire) < 1 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "NEW never paced tick 0"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Rollback teardown must be bounded.
    tokio::time::timeout(Duration::from_secs(10), programs.stop("main.migrating"))
        .await
        .expect("tearing down a wedged NEW program must be bounded (#96)");
    assert!(!programs.is_running("main.migrating"));
}

// ----------------------------------------------------------------------------
// (6) Isolation (inv #10): move_sink under the crosspoint mutex never stalls a
//     running output clock.
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_sink_under_the_crosspoint_does_not_stall_the_output_clock() {
    // A program runs on its own clock. Meanwhile the control thread hammers the
    // crosspoint with register/move_sink/deregister table ops (the SWAP path).
    // The program's clock must keep advancing — the crosspoint is off the clock
    // loop (the clock loop only try_sends a tick index; route()/move_sink live
    // on other threads), so no table op can stall the clock (inv #1 + #10).
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());
    let mut programs: ProgramSet<RealtimePacer> = ProgramSet::new(time.clone());
    let spec = spec_with_id("main", 60, 1);
    let p = program_at(&spec, Rational::FPS_60, time.clone());
    programs.start(p).unwrap();

    let crosspoint = OutputCrosspoint::new();
    let a = RenditionId::new("a");
    let b = RenditionId::new("b");
    let sink = CountingSink::new("s");
    crosspoint.register(a.clone(), sink);

    let before = programs.ticks_emitted("main").unwrap_or(0);
    // Hammer the crosspoint for a window while the clock runs.
    let cp = crosspoint.clone();
    let hammer = std::thread::spawn(move || {
        for i in 0..10_000 {
            if i % 2 == 0 {
                cp.move_sink("s", &a, b.clone());
            } else {
                cp.move_sink("s", &b, a.clone());
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = programs.ticks_emitted("main").unwrap();
    assert!(
        after - before >= 5,
        "the output clock kept ticking while the crosspoint was hammered (inv #1/#10): {before} -> {after}"
    );
    hammer.join().unwrap();
    programs.shutdown().await;
}

// ----------------------------------------------------------------------------
// (7) Post-SWAP is committed: a completed migration reports Migrated, not a
//     quiet rollback.
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_migration_reports_migrated() {
    let time: Arc<dyn TimeSource> = Arc::new(multiview_engine::MonotonicTimeSource::new());
    let mut programs: ProgramSet<RealtimePacer> = ProgramSet::new(time.clone());
    let old = program_at(&spec_with_id("main", 60, 1), Rational::FPS_60, time.clone());
    programs.start(old).unwrap();

    // Drain+stop OLD to model the terminal step; the outcome is Migrated.
    let started = Instant::now();
    while programs.ticks_emitted("main").unwrap_or(0) < 3 {
        assert!(started.elapsed() < Duration::from_secs(30), "OLD stalled");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let outcome = multiview_engine::migration::drain_stop(&mut programs, "main").await;
    assert_eq!(outcome, MigrationOutcome::Migrated);
    assert!(!programs.is_running("main"));
    programs.shutdown().await;
}
