//! P0 in the router (#344 D2b): the real dispatcher and router, fake
//! encoders, a real `PassthroughLive` over a scripted fake tap. No GPU, no
//! libav encoder (the slate is a scripted closed GOP).

use super::*;
use multiview_ffmpeg::idr::CodecKind;
use multiview_output::passthrough::match_rules::{ColourTags, PassCodec, Scan};
use multiview_output::passthrough::{CompressedTap, SlotSpec, SourceTraits, TapAu};

use crate::passthrough_live::{FormatSlate, PassthroughLive, PassthroughSpec, PassthroughState};

const SPS_A: &[u8] = &[
    0x67, 0x64, 0x00, 0x1f, 0xac, 0xb2, 0x00, 0xa0, 0x0b, 0x76, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00,
    0x20, 0x00, 0x00, 0x07, 0x81, 0xe3, 0x06, 0x49,
];
const SPS_B: &[u8] = &[
    0x67, 0x64, 0x00, 0x1f, 0xac, 0xe4, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00,
    0x40, 0x00, 0x00, 0x0f, 0x03, 0xc6, 0x0c, 0x44, 0x80,
];
const SPS_SLATE: &[u8] = &[
    0x67, 0x42, 0xc0, 0x0d, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00,
    0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
];
const PPS: &[u8] = &[0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0];
/// Source GOP in frames (= ticks).
const GOP: u64 = 10;

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [0_u8, 0, 0, 1].into_iter().chain(n.iter().copied()))
        .collect()
}

fn slate() -> FormatSlate {
    let mut packets = vec![annexb(&[SPS_SLATE, PPS, &[0x65, 0x88, 0xEE]])];
    packets.extend((0..4).map(|_| annexb(&[&[0x41, 0x9a, 0xEE]])));
    FormatSlate::new(packets, CodecKind::H264).unwrap()
}

fn traits() -> SourceTraits {
    SourceTraits {
        codec: PassCodec::H264,
        profile: 100,
        constraint_flags: 0,
        level: 31,
        width: 16,
        height: 16,
        frame_rate: CADENCE,
        scan: Scan::Progressive,
        colour: tags(),
        bit_depth: 8,
        chroma: 1,
    }
}

fn tags() -> ColourTags {
    ColourTags {
        primaries: Some(1),
        transfer: Some(1),
        matrix: Some(1),
        range: Some(0),
    }
}

fn slot() -> SlotSpec {
    SlotSpec {
        codec: PassCodec::H264,
        max_profile: 100,
        max_level: 51,
        width: 16,
        height: 16,
        fps: CADENCE,
        scan: Scan::Progressive,
        colour: tags(),
        bit_depth: 8,
        chroma: 1,
        bitrate_ceiling_kbps: 100_000,
        max_gop_ticks: 2 * GOP,
        b_frames_allowed: true,
    }
}

fn eligible() -> PassEligibility {
    PassEligibility::Eligible {
        slot: slot(),
        settings: 0,
    }
}

/// The scripted source: frame `k` (one per tick, 90 kHz at the 5 fps
/// cadence), an IDR every [`GOP`], parameter set B from `change` on.
/// `stall` frames never arrive; `silent` means nothing arrives at all.
#[derive(Default)]
struct Script {
    stall: Option<std::ops::Range<u64>>,
    change: Option<u64>,
    silent: bool,
}

impl Script {
    fn au(&self, k: u64, now_ns: u64) -> Option<TapAu> {
        if self.silent || self.stall.as_ref().is_some_and(|s| s.contains(&k)) {
            return None;
        }
        let (sps, gen) = if self.change.is_some_and(|c| k >= c) {
            (SPS_B, 2)
        } else {
            (SPS_A, 1)
        };
        let key = k % GOP == 0;
        let tag = u8::try_from(k % 200).unwrap();
        let data = if key {
            annexb(&[sps, PPS, &[0x65, 0x88, tag]])
        } else {
            annexb(&[&[0x41, 0x9a, tag]])
        };
        let dts = i64::try_from(k).unwrap() * 18_000;
        Some(TapAu {
            data: Arc::from(data),
            dts,
            pts: dts,
            time_base: Rational::new(1, 90_000),
            strict_idr: key,
            reference: true,
            params_gen: gen,
            arrival_ns: now_ns,
        })
    }
}

/// The fake source: its tap, the clock both sides read, its script.
struct FakeSource {
    tap: Arc<CompressedTap>,
    now: Arc<AtomicU64>,
    script: Arc<Mutex<Script>>,
    /// Passthrough opens performed (the slate "bake" count).
    opens: Arc<AtomicUsize>,
    /// The error a failing bake returns.
    bake_error: Arc<Mutex<String>>,
}

impl FakeSource {
    fn new(script: Script) -> Self {
        Self {
            tap: Arc::new(CompressedTap::new()),
            now: Arc::new(AtomicU64::new(0)),
            script: Arc::new(Mutex::new(script)),
            opens: Arc::new(AtomicUsize::new(0)),
            bake_error: Arc::new(Mutex::new("encoder not found: libx264".to_owned())),
        }
    }

    /// The router's passthrough hooks over this source (`bake`: whether the
    /// slate bake succeeds; a failing one is a missing encoder).
    fn hooks(&self, bake: bool) -> PassHooks {
        let tap = Arc::clone(&self.tap);
        let now = Arc::clone(&self.now);
        let opens = Arc::clone(&self.opens);
        let bake_error = Arc::clone(&self.bake_error);
        PassHooks {
            open: Box::new(move |req: &PassRequest| {
                assert_eq!(req.source, "cam");
                opens.fetch_add(1, Ordering::Relaxed);
                if !bake {
                    return Err(bake_error.lock().unwrap().clone());
                }
                let reader = tap.subscribe().ok_or_else(|| "tap full".to_owned())?;
                let clock = Arc::clone(&now);
                let live = PassthroughLive::new(
                    PassthroughSpec {
                        cadence: CADENCE,
                        codec: CodecKind::H264,
                        stream_params: None,
                    },
                    reader,
                    &slate(),
                    Box::new(move || clock.load(Ordering::Acquire)),
                )
                .unwrap()
                .with_matcher(req.slot.clone(), Box::new(|| Some(traits())))
                .with_slate_move_points(false);
                let status = live.status();
                let live: Box<dyn LiveEncoder> = Box::new(live);
                Ok((live, status))
            }),
            traits: Arc::new(|id: &str| (id == "cam").then(traits)),
        }
    }

    /// Offer frame `t` (the source runs on the output clock).
    fn tick(&self, t: u64) {
        let now = t * 200_000_000;
        self.now.store(now, Ordering::Release);
        if let Some(au) = self.script.lock().unwrap().au(t, now) {
            self.tap.offer(au);
        }
    }
}

struct P0 {
    x: Harness,
    src: FakeSource,
    t: u64,
}

impl P0 {
    fn new(outputs: usize, max: usize, script: Script, bake: bool) -> Self {
        let src = FakeSource::new(script);
        let x = harness_full(
            outputs,
            max,
            None,
            false,
            Some(src.hooks(bake)),
            vec![eligible(); outputs],
            None,
        );
        Self { x, src, t: 0 }
    }

    fn step(&mut self) {
        self.src.tick(self.t);
        self.x.tick(self.t);
        self.t += 1;
    }

    fn run_until(&mut self, limit: u64, mut done: impl FnMut(&mut Self) -> bool) -> Option<u64> {
        let end = self.t + limit;
        while self.t < end {
            self.step();
            if done(self) {
                return Some(self.t - 1);
            }
        }
        None
    }

    fn view(&self, o: usize) -> OutputRouteView {
        self.x.h.snapshot().outputs[o].clone()
    }

    fn take_cam(&mut self, o: usize) -> u64 {
        self.x.h.try_take(take("cam", o, src())).unwrap();
        let at = self
            .run_until(OPEN_DEADLINE_TICKS, |p| {
                p.x.outcomes
                    .try_iter()
                    .any(|o| o.outcome == TakeResult::Applied)
            })
            .expect("the take onto the picture bridge applies");
        assert_eq!(self.view(o).bus, "src:cam");
        at
    }

    fn on_p0(&self, o: usize) -> bool {
        self.view(o).path.as_deref() == Some("P0")
    }

    fn status(&self, o: usize) -> Arc<crate::passthrough_live::PassthroughStatus> {
        self.x.r.passthrough_status(o).expect("a passthrough key")
    }

    /// Invariant #1 on output `o`: every delivered window carries exactly
    /// one video packet and the DTS run is exactly one per tick, with no
    /// gap and no repeat, across every move (Program, picture bridge, P0,
    /// slate, fallback). The dispatcher closes a window once its tick is
    /// complete, so the last window is at most a few ticks behind `self.t`.
    fn assert_one_packet_per_tick(&self, o: usize) {
        let dts: Vec<&Vec<i64>> = self
            .x
            .rec
            .dts
            .iter()
            .filter(|w| w.0 == o)
            .map(|w| &w.1)
            .collect();
        assert!(dts.len() > 10, "output {o}: windows delivered");
        for (i, w) in dts.iter().enumerate() {
            assert_eq!(w.len(), 1, "output {o} window {i}: one packet {w:?}");
        }
        let run: Vec<i64> = dts.iter().map(|w| w[0]).collect();
        for pair in run.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "output {o}: DTS one per tick {pair:?}"
            );
        }
        let last = u64::try_from(*run.last().unwrap()).unwrap();
        assert!(
            last + 8 >= self.t,
            "output {o}: windows keep closing ({last} vs {})",
            self.t
        );
    }

    /// The windows output `o` received: per window, its (pts, len) packets.
    fn packets(&self, o: usize) -> Vec<(i64, bool)> {
        self.x
            .windows_of(o)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }
}

/// A take of a matching source onto an eligible output lands on the
/// encoded picture bridge at once, then moves onto the passthrough key at a
/// run-start IDR only after the key's meter matched (>= 10 s of history),
/// and the encoded key lingers out: 0 encoder keys for that output. One
/// packet per tick throughout on the output (its stamps never go back).
#[test]
fn a_matching_source_moves_onto_p0_after_metering_and_the_encoder_lingers_out() {
    let mut p = P0::new(2, 6, Script::default(), true);
    for _ in 0..5 {
        p.step();
    }
    let applied = p.take_cam(1);
    assert!(!p.on_p0(1), "the take lands on the picture bridge first");
    assert_eq!(p.x.h.snapshot().dynamic_encoders_live, 1, "+1 encoder");
    let pass = p.view(1).passthrough.expect("a passthrough candidate");
    assert!(
        matches!(pass.state.as_str(), "opening" | "metering"),
        "{pass:?}"
    );
    // The meter needs 10 s of history (50 ticks at 5 fps): never before.
    let moved = p
        .run_until(200, |p| p.on_p0(1))
        .expect("moved onto P0 after the meter matched");
    assert!(
        moved >= applied + 50,
        "not before 10 s of history: {applied} -> {moved}"
    );
    let status = p.status(1);
    assert_eq!(status.verdict(), Some(Ok(())));
    assert_eq!(p.view(1).passthrough.unwrap().state, "passthrough");
    assert_eq!(
        p.x.h.snapshot().passthrough.keys_live,
        1,
        "one passthrough key"
    );
    // The move landed on a run-start strict IDR (the key's first run).
    assert_eq!(status.runs.load(Ordering::Relaxed), 1);
    // Linger: the encoded source key closes, the output holds no encoder.
    p.run_until(LINGER_TICKS + 5, |_| false);
    let snap = p.x.h.snapshot();
    assert_eq!(
        snap.dynamic_encoders_live, 0,
        "0 encoder sessions after the linger"
    );
    assert!(p.on_p0(1));
    assert!(
        status.aus.load(Ordering::Relaxed) > 300,
        "AUs passed through"
    );
    // Every tick carried exactly one packet with a strictly increasing PTS
    // on the moved output (bridge -> P0 included).
    let pts: Vec<i64> = p.packets(1).iter().map(|x| x.0).collect();
    assert!(pts.windows(2).all(|w| w[1] > w[0]), "monotonic: {pts:?}");
    // Audio by source time: the key publishes the emitted AU's exact delay
    // (it arrived on its tick and waits the clock's target depth).
    // (The worker reads the shared clock while this thread advances it, so
    // the published delay is the target depth within one tick; the exact
    // value is pinned by passthrough_live's deterministic test.)
    let d = p.x.r.passthrough_audio_delay(1).expect("a delay on P0");
    let target = u32::try_from(multiview_output::passthrough::clock::TARGET_DEPTH).unwrap();
    assert!(d.abs_diff(target) <= 1, "delay {d}");
    assert_eq!(p.x.r.passthrough_audio_delay(0), None, "not on a key");
    // The other output stayed on Program.
    assert_eq!(p.view(0).bus, "program");
    assert!(p.view(0).passthrough.is_none());
    p.assert_one_packet_per_tick(0);
    p.assert_one_packet_per_tick(1);
}

/// A key bridging on its slate while an output reads it brings in the
/// on-demand picture bridge (the encoded source key, charged to the
/// patching principal); the output returns to P0 not before 30 on-time
/// ticks after the stall.
#[test]
fn a_stall_bridges_on_the_picture_bridge_and_returns_after_30_stable_ticks() {
    let mut p = P0::new(1, 6, Script::default(), true);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    // Let the encoded key linger out first: the bridge must re-open it.
    p.run_until(LINGER_TICKS + 5, |_| false);
    assert_eq!(p.x.h.snapshot().dynamic_encoders_live, 0);
    let stall_at = p.t;
    p.src.script.lock().unwrap().stall = Some(stall_at..stall_at + 20);
    let away = p
        .run_until(OPEN_DEADLINE_TICKS + 20, |p| !p.on_p0(0))
        .expect("moved to the picture bridge");
    assert_eq!(p.view(0).bus, "src:cam");
    assert_eq!(p.x.h.snapshot().passthrough.bridges, 1);
    assert_eq!(
        p.x.h.snapshot().dynamic_encoders_live,
        1,
        "the bridge session"
    );
    let principal = p.x.r.dynamic.iter().map(|d| d.principal.clone()).next();
    assert_eq!(
        principal.as_deref(),
        Some("p"),
        "charged to the patching principal"
    );
    let back = p.run_until(400, |p| p.on_p0(0)).unwrap_or_else(|| {
        let s = p.status(0);
        panic!(
            "not back: {:?} stable {} state {:?} verdict {:?} allow {} runs {}",
            p.view(0),
            s.stable_ticks(),
            s.state(),
            s.verdict(),
            s.runs_allowed_from(),
            s.runs.load(Ordering::Relaxed)
        )
    });
    assert!(
        back >= stall_at + 20 + 30,
        "not before 30 on-time ticks: stall {stall_at}, away {away}, back {back}"
    );
    assert_eq!(p.x.h.snapshot().passthrough.moves_in, 2);
    p.assert_one_packet_per_tick(0);
}

/// At the encoder cap the output stays on the slate (0 sessions) and the
/// readback says why; the key returns by itself when the source does.
#[test]
fn at_the_encoder_cap_a_fault_stays_on_the_slate_with_the_reason() {
    let mut p = P0::new(1, 6, Script::default(), true);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    p.run_until(LINGER_TICKS + 5, |_| false);
    // No encoder capacity left for anyone.
    p.x.r.cfg.max_encoders = 0;
    let stall_at = p.t;
    p.src.script.lock().unwrap().stall = Some(stall_at..stall_at + 10);
    p.run_until(60, |p| {
        p.view(0)
            .passthrough
            .is_some_and(|v| v.reason.as_deref() == Some(crate::output_routing::NO_BRIDGE_SESSION))
    })
    .expect("bridging on slate: no encoder session");
    let v = p.view(0);
    assert_eq!(v.passthrough.unwrap().state, "slate");
    assert!(p.on_p0(0), "still on the passthrough key (its slate)");
    assert_eq!(p.x.h.snapshot().dynamic_encoders_live, 0, "0 sessions");
    assert_eq!(p.x.h.snapshot().passthrough.bridges_refused, 1);
    // The source comes back: the key itself returns after 30 stable ticks.
    p.run_until(200, |p| {
        p.view(0)
            .passthrough
            .is_some_and(|v| v.state == "passthrough")
    })
    .expect("passes through again");
    assert!(p.status(0).slate_ticks.load(Ordering::Relaxed) >= 10);
    p.assert_one_packet_per_tick(0);
}

/// A parameter change on the key: the output goes to the picture bridge
/// and returns not before 300 stable ticks with the new parameters.
#[test]
fn a_parameter_change_returns_not_before_300_ticks() {
    let mut p = P0::new(1, 6, Script::default(), true);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    let change = (p.t / GOP + 1) * GOP;
    p.src.script.lock().unwrap().change = Some(change);
    p.run_until(OPEN_DEADLINE_TICKS + 20, |p| !p.on_p0(0))
        .expect("moved to the picture bridge");
    let back = p
        .run_until(800, |p| p.on_p0(0))
        .expect("back on P0 after the parameter change");
    assert!(
        back >= change + 300,
        "not before 300 ticks: change {change}, back {back}"
    );
    assert_eq!(p.status(0).run_gen(), 2, "the run carries the new sets");
    assert!(p.x.h.snapshot().parameter_set_changes >= 1);
    p.assert_one_packet_per_tick(0);
}

/// A take of another feed out of P0 is an ordinary move away; the
/// passthrough key then closes at once and unsubscribes the tap.
#[test]
fn a_take_out_of_p0_closes_the_key_and_unsubscribes_the_tap() {
    let mut p = P0::new(1, 6, Script::default(), true);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    assert_eq!(p.src.tap.consumer_count(), 1);
    p.x.h.try_take(take("out", 0, PictureBus::Program)).unwrap();
    p.run_until(60, |p| {
        p.x.outcomes
            .try_iter()
            .any(|o| o.operation_id == "out" && o.outcome == TakeResult::Applied)
    })
    .expect("moved to Program");
    p.run_until(3, |_| false);
    assert_eq!(p.view(0).bus, "program");
    assert!(p.view(0).passthrough.is_none(), "candidate gone");
    assert_eq!(p.x.h.snapshot().passthrough.keys_live, 0);
    // The reaper drops the worker (and its reader) off the clock.
    let mut gone = false;
    for _ in 0..200 {
        if p.src.tap.consumer_count() == 0 {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(gone, "the tap is unsubscribed");
    p.assert_one_packet_per_tick(0);
}

/// A failing slate bake (no H.264 encoder linked) opens no candidate key:
/// the output stays on its picture bridge and the readback says why.
#[test]
fn a_failed_slate_bake_leaves_the_output_on_its_encoded_path_with_the_reason() {
    let mut p = P0::new(1, 6, Script::default(), false);
    p.step();
    p.take_cam(0);
    p.run_until(60, |p| {
        p.view(0)
            .passthrough
            .is_some_and(|v| v.state == "unavailable")
    })
    .expect("the candidate reports its failure");
    let v = p.view(0).passthrough.unwrap();
    assert!(
        v.reason
            .as_deref()
            .is_some_and(|r| r.starts_with("slate-bake-failed") && !r.contains("cam")),
        "{v:?}"
    );
    p.run_until(100, |_| false);
    assert!(!p.on_p0(0));
    assert_eq!(p.src.opens.load(Ordering::Relaxed), 1, "not retried");
    assert_eq!(p.x.h.snapshot().passthrough.open_failures, 1);
    assert_eq!(p.x.h.snapshot().passthrough.keys_live, 0);
    p.assert_one_packet_per_tick(0);
}

/// An output that is not eligible never gets a candidate.
#[test]
fn an_ineligible_output_never_gets_a_candidate() {
    let src = FakeSource::new(Script::default());
    let x = harness_full(
        1,
        6,
        None,
        false,
        Some(src.hooks(true)),
        vec![PassEligibility::NotSupported],
        None,
    );
    let mut p = P0 { x, src, t: 0 };
    p.step();
    p.take_cam(0);
    p.run_until(120, |_| false);
    assert!(p.view(0).passthrough.is_none());
    assert_eq!(p.src.opens.load(Ordering::Relaxed), 0);
    assert_eq!(
        p.x.h.passthrough_static(0, "cam"),
        Err(crate::output_routing::NOT_SUPPORTED_ON_MEDIUM)
    );
}

/// Chaos (invariants #1/#10): a source that never sends (the key bridges on
/// its slate forever) never moves the output onto P0, never delays the
/// router's tick, and the base output's Program packets stay one per tick.
#[test]
fn a_silent_tap_never_blocks_the_router_or_reaches_p0() {
    let mut p = P0::new(2, 6, Script::default(), true);
    p.step();
    p.take_cam(1);
    p.src.script.lock().unwrap().silent = true;
    p.run_until(300, |_| false);
    assert!(!p.on_p0(1));
    let v = p.view(1).passthrough.unwrap();
    assert_eq!(v.state, "metering");
    // Program and the candidate's output: one packet per tick throughout
    // (the router never waits on the key's tap).
    p.assert_one_packet_per_tick(0);
    p.assert_one_packet_per_tick(1);
    let program: Vec<i64> = p.packets(0).iter().map(|x| x.0).collect();
    assert!(
        program.windows(2).all(|w| w[1] == w[0] + 1),
        "Program cadence unchanged"
    );
    assert!(
        u64::try_from(program.len()).unwrap() + 2 >= p.t,
        "{} of {}",
        program.len(),
        p.t
    );
    // The key itself still emits one AU per tick (its slate).
    let s = p.status(1);
    assert_eq!(s.state(), PassthroughState::Slate);
}

/// Over [`MAX_PASSTHROUGH_KEYS`] eligible outputs on one source open only
/// that many keys (the cap is separate from `max_encoders`).
#[test]
fn passthrough_keys_are_capped_separately_from_encoders() {
    let n = MAX_PASSTHROUGH_KEYS + 1;
    let mut p = P0::new(n, 6, Script::default(), true);
    p.step();
    for o in 0..n {
        // Distinct principals: the global cap, not the per-principal one.
        p.x.h
            .try_take(take_by(&format!("t{o}"), o, &format!("p{o}")))
            .unwrap();
    }
    p.run_until(120, |_| false);
    // The tap holds at most 8 consumers too; the router never asks for more.
    let snap = p.x.h.snapshot();
    assert_eq!(snap.dynamic_encoders_live, 1, "one shared encoded key");
    assert!(snap.passthrough.keys_live <= MAX_PASSTHROUGH_KEYS);
    assert_eq!(
        snap.outputs
            .iter()
            .filter(|v| v.passthrough.is_some())
            .count(),
        MAX_PASSTHROUGH_KEYS
    );
}

/// The plan and the readback share the engine's predicate (#344 D2b): a
/// source onto an eligible output is not P0 before the key's meter matched
/// (`not-enough-history`), P0 with 0 encoders / 0 converts once it did;
/// an output that is not eligible says `passthrough-not-supported-on-medium`;
/// `GET /patch`'s `current_path` reports P0 while the output is on the key.
#[test]
fn the_plan_reports_p0_from_the_engines_own_predicate() {
    use multiview_control::output_routing::{OutputRouting, PlanPath, RoutedFeed};
    let src = FakeSource::new(Script::default());
    let x = harness_full(
        2,
        6,
        None,
        false,
        Some(src.hooks(true)),
        vec![eligible(), PassEligibility::NotSupported],
        None,
    );
    let mut p = P0 { x, src, t: 0 };
    let run = |id: &str| RunOutput {
        id: Some(id.into()),
        settings: 0,
        feed_format: None,
        base_format: None,
        routable: true,
        program_audio: false,
        codec_name: String::new(),
    };
    let slot = output_routing_slot();
    slot.store(Some(Arc::new(
        p.x.h.clone().with_outputs(vec![run("hls"), run("file")]),
    )));
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
    let cam = RoutedFeed::Source("cam".into());
    p.step();
    let before = adapter.plan_slot("hls", &cam).unwrap();
    assert_ne!(before.path, PlanPath::P0);
    assert_eq!(before.passthrough.as_deref(), Some("not-enough-history"));
    let other = adapter.plan_slot("file", &cam).unwrap();
    assert_eq!(
        other.passthrough.as_deref(),
        Some(crate::output_routing::NOT_SUPPORTED_ON_MEDIUM)
    );
    // A non-source feed carries no passthrough miss.
    assert_eq!(
        adapter
            .plan_slot("hls", &RoutedFeed::Program)
            .unwrap()
            .passthrough,
        None
    );
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    let now = adapter.plan_slot("hls", &cam).unwrap();
    assert_eq!(now.path, PlanPath::P0);
    assert!(!now.cost.needs_encoder);
    assert_eq!(now.converts, 0);
    assert_eq!(now.passthrough, None);
    assert_eq!(adapter.current_path("hls"), Some(PlanPath::P0));
    let live = adapter.current_passthrough("hls").unwrap();
    assert_eq!(live.state, "passthrough");
    assert_eq!(adapter.current_passthrough("file"), None);
}

/// Review R1-I-3: a bake error whose 160th byte falls inside a multi-byte
/// character never panics the router tick; the reason is cut at a char
/// boundary and stays bounded.
#[test]
fn a_multibyte_bake_error_is_truncated_at_a_char_boundary() {
    let mut p = P0::new(1, 6, Script::default(), false);
    // "slate-bake-failed: " is 19 bytes; 'é' is 2: byte 160 splits one.
    *p.src.bake_error.lock().unwrap() = "é".repeat(200);
    p.step();
    p.take_cam(0);
    p.run_until(60, |p| {
        p.view(0)
            .passthrough
            .is_some_and(|v| v.state == "unavailable")
    })
    .expect("the failure is reported, the tick did not panic");
    let reason = p.view(0).passthrough.unwrap().reason.unwrap();
    assert!(reason.len() <= 160, "{}", reason.len());
    assert!(reason.starts_with("slate-bake-failed: é"));
}

/// Review R1-I-1/A-3: an atomic set that takes a P0 output to another feed
/// ends the candidate exactly like a single take: the old key is retired,
/// its tap unsubscribed, and a later fault/recovery of the old source never
/// moves the output back to it.
#[test]
fn a_set_take_off_p0_retires_the_candidate() {
    let mut p = P0::new(1, 6, Script::default(), true);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    p.x.h
        .try_take_set(TakeSet {
            operation_id: "set".to_owned(),
            principal: "q".to_owned(),
            members: vec![take("m", 0, PictureBus::Program)],
            arm: None,
        })
        .unwrap();
    p.run_until(60, |p| p.view(0).bus == "program")
        .expect("the set lands");
    // The old source stalls and recovers: the output must stay on Program.
    let at = p.t;
    p.src.script.lock().unwrap().stall = Some(at..at + 10);
    p.run_until(200, |p| {
        assert_eq!(p.view(0).bus, "program", "never back to the old source");
        false
    });
    assert!(p.view(0).passthrough.is_none(), "candidate retired");
    assert_eq!(p.x.h.snapshot().passthrough.keys_live, 0);
    p.assert_one_packet_per_tick(0);
}

fn take_by(op: &str, output: usize, principal: &str) -> OutputTake {
    OutputTake {
        principal: principal.to_owned(),
        ..take(op, output, src())
    }
}

/// Review R1-A-1: one principal cannot hold every passthrough key: its
/// candidates are capped at [`MAX_PASSTHROUGH_KEYS_PER_PRINCIPAL`], and a
/// candidate that meters without ever matching is reclaimed after
/// [`PASS_METER_LIMIT_TICKS`] so another principal's output gets one.
#[test]
fn passthrough_candidates_are_budgeted_per_principal_and_reclaimed() {
    let n = MAX_PASSTHROUGH_KEYS;
    let mut p = P0::new(n + 1, 12, Script::default(), true);
    p.src.script.lock().unwrap().silent = true;
    p.step();
    for o in 0..n {
        p.x.h
            .try_take(take_by(&format!("a{o}"), o, "alice"))
            .unwrap();
    }
    p.x.h.try_take(take_by("b", n, "bob")).unwrap();
    p.run_until(60, |_| false);
    let held = |p: &P0, who: std::ops::Range<usize>| {
        let snap = p.x.h.snapshot();
        who.filter(|&o| snap.outputs[o].passthrough.is_some())
            .count()
    };
    assert_eq!(
        held(&p, 0..n),
        crate::output_routing::MAX_PASSTHROUGH_KEYS_PER_PRINCIPAL,
        "one principal's share"
    );
    assert_eq!(held(&p, n..n + 1), 1, "another principal gets a candidate");
    // A never-matching candidate is reclaimed (the source is silent).
    p.run_until(crate::output_routing::PASS_METER_LIMIT_TICKS + 60, |p| {
        p.view(n)
            .passthrough
            .is_some_and(|v| v.state == "unavailable")
    })
    .expect("reclaimed after the metering limit");
    assert!(p
        .view(n)
        .passthrough
        .unwrap()
        .reason
        .is_some_and(|r| r.contains("no-match")));
}

/// Review R1-A-2: a picture bridge that reuses another principal's
/// lingering encoded key adopts it only within the patching principal's
/// cap (and is then charged to it); at the cap it stays on the slate.
#[test]
fn a_bridge_never_adopts_another_principals_key_past_the_cap() {
    let mut p = P0::new(2, 6, Script::default(), true);
    p.step();
    // Output 0 on P0 patched by "p"; its own encoded key lingers out.
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    p.run_until(LINGER_TICKS + 5, |_| false);
    // "other" takes cam onto output 1 and leaves: its key lingers.
    p.x.h.try_take(take_by("o1", 1, "other")).unwrap();
    p.run_until(OPEN_DEADLINE_TICKS, |p| p.view(1).bus == "src:cam")
        .expect("other's take lands");
    p.x.h.try_take(take_by("o2", 1, "other")).unwrap();
    p.x.h
        .try_take(OutputTake {
            principal: "other".to_owned(),
            ..take("o3", 1, PictureBus::Program)
        })
        .unwrap();
    p.run_until(60, |p| p.view(1).bus == "program")
        .expect("other left");
    let lingering =
        p.x.r
            .dynamic
            .iter()
            .find(|d| d.bus == src())
            .map(|d| (d.key, d.principal.clone()));
    assert_eq!(lingering.as_ref().map(|l| l.1.as_str()), Some("other"));
    // "p" is at its cap (0 sessions allowed per principal).
    p.x.r.cfg.max_per_principal = 0;
    let at = p.t;
    p.src.script.lock().unwrap().stall = Some(at..at + 10);
    p.run_until(60, |p| {
        p.view(0)
            .passthrough
            .is_some_and(|v| v.reason.as_deref() == Some(crate::output_routing::NO_BRIDGE_SESSION))
    })
    .expect("stays on the slate at the cap");
    assert!(p.on_p0(0), "never moved onto the adopted key");
    assert!(
        p.x.r.dynamic.iter().all(|d| d.principal == "other"),
        "never re-attributed"
    );
}

/// A P0 harness whose router also resets formats live (a fake encoder at
/// any settings).
fn p0_reformat(outputs: usize) -> P0 {
    let src = FakeSource::new(Script::default());
    let x = harness_full(
        outputs,
        6,
        None,
        false,
        Some(src.hooks(true)),
        vec![eligible(); outputs],
        Some(Box::new(|_, _| {
            let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
                forced: false,
                encoded: Arc::new(AtomicUsize::new(0)),
            });
            Ok(enc)
        })),
    );
    P0 { x, src, t: 0 }
}

fn reformat_req(op: &str, output: usize) -> Reformat {
    Reformat {
        operation_id: op.to_owned(),
        output_index: output,
        settings: ReformatSettings {
            codec_name: "h264".to_owned(),
            width: 8,
            height: 16,
            bit_rate: 1_000_000,
            base_format: None,
        },
        feed_format: None,
        principal: "p".to_owned(),
    }
}

fn reset_lands(p: &mut P0) {
    p.run_until(REFORMAT_DEADLINE_TICKS, |p| {
        p.x.outcomes
            .try_iter()
            .any(|o| o.outcome == TakeResult::ResetApplied)
    })
    .expect("the reset lands");
}

/// Review R1-M-3: P0 eligibility is the boot slot; a live format reset
/// changes the slot, so it disables P0 for that output (recorded in §12.5):
/// a reset before the take means no candidate, a reset while on P0 retires
/// the candidate, and the plan reports the output not supported.
#[test]
fn a_live_reset_disables_passthrough_for_that_output() {
    // Reset before the take: no candidate ever.
    let mut p = p0_reformat(1);
    p.step();
    p.x.h.try_reformat(reformat_req("r", 0)).unwrap();
    reset_lands(&mut p);
    p.take_cam(0);
    p.run_until(120, |_| false);
    assert!(
        p.view(0).passthrough.is_none(),
        "no candidate after a reset"
    );
    assert_eq!(p.src.opens.load(Ordering::Relaxed), 0);
    assert_eq!(
        p.x.h.passthrough_static(0, "cam"),
        Err(crate::output_routing::NOT_SUPPORTED_ON_MEDIUM)
    );
    // Reset while on P0: the candidate is retired, the output leaves P0.
    let mut p = p0_reformat(1);
    p.step();
    p.take_cam(0);
    p.run_until(200, |p| p.on_p0(0)).expect("on P0");
    p.x.h.try_reformat(reformat_req("r", 0)).unwrap();
    reset_lands(&mut p);
    p.run_until(10, |_| false);
    assert!(!p.on_p0(0));
    assert!(p.view(0).passthrough.is_none());
    assert_eq!(p.x.h.snapshot().passthrough.keys_live, 0);
    p.assert_one_packet_per_tick(0);
}

/// Review R1-I-2: a passthrough key whose worker wedges (never returns from
/// `encode`) is closed after `WEDGED_ENCODER_TICKS`: the output falls back
/// to Program at a RAP, every output keeps one packet per tick, and the
/// router never waits on the wedged worker.
#[test]
fn a_wedged_passthrough_worker_falls_back_and_never_stalls_the_router() {
    let src = FakeSource::new(Script::default());
    let wedge = Arc::new(AtomicBool::new(false));
    let w = Arc::clone(&wedge);
    let mut hooks = src.hooks(true);
    let mut inner = std::mem::replace(&mut hooks.open, Box::new(|_| Err(String::new())));
    hooks.open = Box::new(move |req| {
        let (live, status) = inner(req)?;
        let live: Box<dyn LiveEncoder> = Box::new(Wedgeable {
            inner: live,
            wedge: Arc::clone(&w),
        });
        Ok((live, status))
    });
    let x = harness_full(2, 6, None, false, Some(hooks), vec![eligible(); 2], None);
    let mut p = P0 { x, src, t: 0 };
    p.step();
    p.take_cam(1);
    p.run_until(200, |p| p.on_p0(1)).expect("on P0");
    wedge.store(true, Ordering::Release);
    p.run_until(u64::from(WEDGED_ENCODER_TICKS) + 60, |p| {
        p.view(1).bus == "program"
    })
    .expect("falls back to Program");
    p.run_until(20, |_| false);
    assert_eq!(
        p.x.h.snapshot().passthrough.keys_live,
        0,
        "the wedged key is closed"
    );
    assert!(p.x.h.snapshot().dead_encoders >= 1);
    p.assert_one_packet_per_tick(0);
    // The wedged key's own output: the pre-existing ADR-0108 wedged-key
    // rule (any live key) holds its windows until the key is closed at
    // WEDGED_ENCODER_TICKS; the hole is bounded by it, signalled as a loss,
    // and one packet per tick resumes on Program.
    let run: Vec<i64> =
        p.x.rec
            .dts
            .iter()
            .filter(|w| w.0 == 1)
            .flat_map(|w| w.1.iter().copied())
            .collect();
    let holes: Vec<i64> = run
        .windows(2)
        .map(|w| w[1] - w[0] - 1)
        .filter(|&h| h != 0)
        .collect();
    assert!(holes.len() <= 1, "{holes:?}");
    assert!(
        holes
            .iter()
            .all(|&h| h > 0 && h <= i64::from(WEDGED_ENCODER_TICKS) + 8),
        "bounded hole {holes:?}"
    );
    assert!(p.x.rec.losses.contains(&1), "the hole is signalled");
    wedge.store(false, Ordering::Release);
}

/// Review R1-I-2: an opener thread that never returns (a wedged bake)
/// never delays the router: the candidate times out, the output stays on
/// its picture bridge with one packet per tick, Program is untouched.
#[test]
fn a_wedged_passthrough_open_never_stalls_the_router() {
    let src = FakeSource::new(Script::default());
    let (_hold, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let hooks = PassHooks {
        open: Box::new(move |_| {
            let _ = wait.lock().unwrap().recv(); // never returns while the test runs
            Err("unreachable".to_owned())
        }),
        traits: Arc::new(|id: &str| (id == "cam").then(traits)),
    };
    let x = harness_full(2, 6, None, false, Some(hooks), vec![eligible(); 2], None);
    let mut p = P0 { x, src, t: 0 };
    p.step();
    p.take_cam(1);
    p.run_until(crate::output_routing::PASS_OPEN_DEADLINE_TICKS + 40, |_| {
        false
    });
    assert!(!p.on_p0(1));
    assert_eq!(p.view(1).bus, "src:cam", "still on the picture bridge");
    p.assert_one_packet_per_tick(0);
    p.assert_one_packet_per_tick(1);
}

/// A live encoder that blocks in `encode` while `wedge` is set.
struct Wedgeable {
    inner: Box<dyn LiveEncoder>,
    wedge: Arc<AtomicBool>,
}

impl VideoEncodeWorker for Wedgeable {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        while self.wedge.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
}

impl LiveEncoder for Wedgeable {
    fn start_at(&mut self, t0: u64) {
        self.inner.start_at(t0);
    }
    fn time_base(&self) -> Rational {
        self.inner.time_base()
    }
    fn move_gate(&self) -> MoveGate {
        self.inner.move_gate()
    }
    fn parameter_sets(&self) -> Option<ParameterSets> {
        self.inner.parameter_sets()
    }
}
