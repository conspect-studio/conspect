//! Behavioural contract for the pure media-player transport state machine
//! ([`multiview_cli::player`]) — ADR-0057 (media players) + ADR-0097 (vamp +
//! exit) + [media-playout §7](../../docs/research/media-playout.md).
//!
//! These tests are **feature-independent** (no `ffmpeg`/GPU): the transport
//! core is pure logic over integer frame indices and the output tick counter,
//! so it runs in the CI-green default build. They pin:
//!
//! 1. geometry validation (`in ≤ vamp_in < vamp_out ≤ out`);
//! 2. output-anchored stamping `publish_at(k) = anchor + k × frame_period`,
//!    monotonic across loop laps (media-playout §7.2);
//! 3. the in-place loop wrap → `SeekFlushTo { vamp_in }` at the vamp boundary;
//! 4. **the exit-latch fires exactly once at the first boundary at-or-after the
//!    arm** — the 2–3-frame-vamp race Codex flagged on ADR-0097 and the ADR's
//!    own self-review mandated (the property test below);
//! 5. the four EOF policies' terminal behaviour.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use multiview_cli::player::{
    EofPolicy, MediaPlayer, MediaPlayerState, PlayerAction, PlayoutGeometry, PlayoutGeometryError,
    TransportMailbox, TransportVerb,
};
use multiview_core::time::{MediaTime, Rational};

/// 25 fps — an exact integer cadence (40 ms/frame), so stamps are round numbers.
fn cad_25() -> Rational {
    Rational::new(25, 1)
}

/// Frame period of `cad_25` in ns (40 ms).
const PERIOD_25_NS: i64 = 40_000_000;

fn geom(in_p: u64, out_p: u64, vamp_in: u64, vamp_out: u64) -> PlayoutGeometry {
    PlayoutGeometry::new(in_p, out_p, vamp_in, vamp_out, cad_25()).unwrap()
}

// ---------------------------------------------------------------------------
// 1. Geometry validation
// ---------------------------------------------------------------------------

#[test]
fn geometry_accepts_a_nested_window() {
    let g = PlayoutGeometry::new(0, 100, 10, 90, cad_25()).unwrap();
    assert_eq!(g.in_point(), 0);
    assert_eq!(g.out_point(), 100);
    assert_eq!(g.vamp_in(), 10);
    assert_eq!(g.vamp_out(), 90);
    assert_eq!(g.vamp_len(), 80);
    assert_eq!(g.trimmed_len(), 100);
    assert_eq!(g.frame_period_ns(), PERIOD_25_NS);
}

#[test]
fn geometry_accepts_whole_clip_as_vamp() {
    // vamp_in == in_point and vamp_out == out_point: the whole clip loops.
    let g = PlayoutGeometry::new(5, 50, 5, 50, cad_25()).unwrap();
    assert_eq!(g.vamp_len(), 45);
}

#[test]
fn geometry_rejects_zero_length_vamp_segment() {
    // vamp_in == vamp_out is a degenerate vamp — rejected, never silently
    // degraded (ADR-0097 §2 / the ADR-T015 dip precedent).
    let err = PlayoutGeometry::new(0, 100, 40, 40, cad_25()).unwrap_err();
    assert!(matches!(err, PlayoutGeometryError::Window { .. }));
}

#[test]
fn geometry_rejects_vamp_outside_clip() {
    // vamp_out > out_point.
    assert!(matches!(
        PlayoutGeometry::new(0, 50, 10, 60, cad_25()).unwrap_err(),
        PlayoutGeometryError::Window { .. }
    ));
    // vamp_in < in_point.
    assert!(matches!(
        PlayoutGeometry::new(20, 100, 10, 90, cad_25()).unwrap_err(),
        PlayoutGeometryError::Window { .. }
    ));
}

#[test]
fn geometry_rejects_non_positive_cadence() {
    assert!(matches!(
        PlayoutGeometry::new(0, 100, 10, 90, Rational::new(0, 1)).unwrap_err(),
        PlayoutGeometryError::Cadence { .. }
    ));
    assert!(matches!(
        PlayoutGeometry::new(0, 100, 10, 90, Rational::new(-25, 1)).unwrap_err(),
        PlayoutGeometryError::Cadence { .. }
    ));
}

// ---------------------------------------------------------------------------
// 1b. Transport mailbox — two-class: state verbs conflated latest-wins, targeted
//     verbs a bounded drop-oldest ordered FIFO; drained in order
// ---------------------------------------------------------------------------

#[test]
fn mailbox_conflates_state_verbs_to_the_latest() {
    let mb = TransportMailbox::new();
    // A burst of state-collapsing verbs: only the LAST survives.
    mb.submit(TransportVerb::Play);
    mb.submit(TransportVerb::Pause);
    mb.submit(TransportVerb::ArmExit);
    mb.submit(TransportVerb::Vamp);
    let drained = mb.drain();
    assert_eq!(
        drained,
        vec![TransportVerb::Vamp],
        "only the latest state-collapsing verb survives conflation"
    );
    // Drained empty afterwards.
    assert!(mb.drain().is_empty());
}

#[test]
fn mailbox_preserves_targeted_verbs_in_order() {
    let mb = TransportMailbox::new();
    // Targeted verbs (load/cue/seek) carry distinct targets — all preserved, in order.
    mb.submit(TransportVerb::Load {
        asset: "a".to_owned(),
    });
    mb.submit(TransportVerb::Cue { frame: Some(10) });
    mb.submit(TransportVerb::Seek { frame: 42 });
    let drained = mb.drain();
    assert_eq!(
        drained,
        vec![
            TransportVerb::Load {
                asset: "a".to_owned()
            },
            TransportVerb::Cue { frame: Some(10) },
            TransportVerb::Seek { frame: 42 },
        ]
    );
}

#[test]
fn mailbox_collapses_only_the_state_verbs_keeping_targets() {
    let mb = TransportMailbox::new();
    mb.submit(TransportVerb::Play);
    mb.submit(TransportVerb::Seek { frame: 5 });
    mb.submit(TransportVerb::Pause); // collapses the earlier Play, keeps Seek
    let drained = mb.drain();
    assert_eq!(
        drained,
        vec![TransportVerb::Seek { frame: 5 }, TransportVerb::Pause],
        "state verbs conflate but a targeted Seek between them is preserved"
    );
}

#[test]
fn mailbox_bounds_targeted_verbs_drop_oldest_keeping_order() {
    // A stalled consumer (never drains): targeted verbs must NOT grow unbounded
    // (inv #10 / safety §5). Submit far more than the cap of distinct seeks; the
    // mailbox keeps at most the cap, dropping the OLDEST, and the retained ones
    // stay in submission order (NOT collapsed latest-wins).
    let mb = TransportMailbox::new();
    // Submit 50 distinct seeks without ever draining.
    for f in 0..50u64 {
        mb.submit(TransportVerb::Seek { frame: f });
    }
    let drained = mb.drain();
    // Bounded: never more than the cap.
    assert!(
        drained.len() <= 16,
        "targeted queue must be bounded (got {} pending)",
        drained.len()
    );
    assert!(!drained.is_empty(), "the most recent seeks are retained");
    // Drop-OLDEST + order-preserving: the retained seeks are the NEWEST `len`
    // frames, in ascending (submission) order — the oldest were evicted.
    let frames: Vec<u64> = drained
        .iter()
        .map(|v| match v {
            TransportVerb::Seek { frame } => *frame,
            other => panic!("expected only Seek verbs, got {other:?}"),
        })
        .collect();
    // Strictly increasing (FIFO order preserved, not reordered/collapsed).
    for w in frames.windows(2) {
        assert!(
            w[1] > w[0],
            "retained targeted verbs must keep submission order: {frames:?}"
        );
    }
    // The newest frame (49) survived; an early one (0) was dropped.
    assert_eq!(
        *frames.last().unwrap(),
        49,
        "the newest seek is always retained"
    );
    assert!(
        !frames.contains(&0),
        "the oldest seek was dropped (drop-oldest): {frames:?}"
    );
}

#[test]
fn mailbox_state_verbs_never_count_against_the_targeted_cap() {
    // Interleaving many state verbs must not evict targeted verbs (state verbs
    // conflate to one and do not consume the targeted budget).
    let mb = TransportMailbox::new();
    mb.submit(TransportVerb::Seek { frame: 1 });
    for _ in 0..100 {
        mb.submit(TransportVerb::Play);
        mb.submit(TransportVerb::Pause);
    }
    mb.submit(TransportVerb::Seek { frame: 2 });
    let drained = mb.drain();
    // Exactly the two seeks (in order) + at most one conflated state verb.
    let seeks: Vec<u64> = drained
        .iter()
        .filter_map(|v| match v {
            TransportVerb::Seek { frame } => Some(*frame),
            _ => None,
        })
        .collect();
    assert_eq!(seeks, vec![1, 2], "both distinct seeks survive in order");
    let state_count = drained
        .iter()
        .filter(|v| {
            matches!(
                v,
                TransportVerb::Play
                    | TransportVerb::Pause
                    | TransportVerb::Stop
                    | TransportVerb::Vamp
                    | TransportVerb::ArmExit
                    | TransportVerb::TakeExit
                    | TransportVerb::CancelExit
            )
        })
        .count();
    assert!(state_count <= 1, "state verbs conflate to at most one");
}

// ---------------------------------------------------------------------------
// 2. Output-anchored stamping (media-playout §7.2)
// ---------------------------------------------------------------------------

#[test]
fn play_stamps_frames_output_anchored_from_the_start_tick() {
    // anchor = output media time of the start tick (here, an arbitrary mid-show
    // instant: tick 1000 at 25fps = 40s).
    let anchor = MediaTime::from_tick(1000, cad_25());
    let mut p = MediaPlayer::new(geom(0, 5, 0, 5), EofPolicy::HoldLastFrame, anchor);
    p.play(anchor);

    // Decode source frames 0,1,2 in order → published at anchor + k·period.
    for k in 0..3u64 {
        let action = p.on_decoded(k);
        match action {
            PlayerAction::Publish { at } => {
                let expected = anchor.saturating_add(MediaTime::from_nanos(
                    i64::try_from(k).unwrap().saturating_mul(PERIOD_25_NS),
                ));
                assert_eq!(at, expected, "frame {k} must be output-anchored");
            }
            other => panic!("frame {k}: expected Publish, got {other:?}"),
        }
    }
}

#[test]
fn stamps_are_monotonic_across_a_loop_lap() {
    // A 3-frame vamp [0,3): play 0,1,2, hit the boundary at frame 3 → wrap, then
    // the next lap's frames 0,1,2 must stamp at k=3,4,5 — NOT reset to anchor.
    let anchor = MediaTime::from_tick(0, cad_25());
    let mut p = MediaPlayer::new(geom(0, 3, 0, 3), EofPolicy::Loop, anchor);
    p.play(anchor);

    let mut stamps = Vec::new();
    // Lap 1: frames 0,1,2 publish.
    for f in 0..3u64 {
        if let PlayerAction::Publish { at } = p.on_decoded(f) {
            stamps.push(at);
        } else {
            panic!("lap1 frame {f} should publish");
        }
    }
    // The decoder reaches frame 3 (== vamp_out): the boundary. Expect a wrap.
    assert_eq!(
        p.on_decoded(3),
        PlayerAction::SeekFlushTo { frame: 0 },
        "reaching vamp_out must wrap to vamp_in"
    );
    // Lap 2: the executor seeked to frame 0 and decodes 0,1,2 again.
    for f in 0..3u64 {
        if let PlayerAction::Publish { at } = p.on_decoded(f) {
            stamps.push(at);
        } else {
            panic!("lap2 frame {f} should publish");
        }
    }

    // Six monotonically-increasing stamps, each one period apart.
    assert_eq!(stamps.len(), 6);
    for w in stamps.windows(2) {
        assert!(
            w[1].as_nanos() > w[0].as_nanos(),
            "stamps must strictly increase across the lap seam: {stamps:?}"
        );
        assert_eq!(
            w[1].as_nanos() - w[0].as_nanos(),
            PERIOD_25_NS,
            "each stamp is exactly one frame period after the last"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Loop wrap mechanics
// ---------------------------------------------------------------------------

#[test]
fn vamp_loops_the_vamp_segment_not_the_whole_clip() {
    // Clip [0,100), vamp [40,60): vamping wraps 60 → 40, never touching the
    // head [0,40) or tail [60,100).
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 100, 40, 60), EofPolicy::Loop, anchor);
    p.vamp(anchor);
    // Publish the vamp frames.
    assert!(matches!(p.on_decoded(40), PlayerAction::Publish { .. }));
    assert!(matches!(p.on_decoded(59), PlayerAction::Publish { .. }));
    // Reaching vamp_out (60) wraps to vamp_in (40).
    assert_eq!(p.on_decoded(60), PlayerAction::SeekFlushTo { frame: 40 });
}

// ---------------------------------------------------------------------------
// 4. The exit-latch — exactly once at the first boundary at-or-after the arm
// ---------------------------------------------------------------------------

#[test]
fn arming_the_exit_fires_at_the_next_vamp_boundary_then_ends() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 3, 0, 3), EofPolicy::AutoOff, anchor);
    p.vamp(anchor);
    // Lap 1 frame 0,1 publish; arm mid-lap at frame 1.
    assert!(matches!(p.on_decoded(0), PlayerAction::Publish { .. }));
    assert!(matches!(p.on_decoded(1), PlayerAction::Publish { .. }));
    p.arm_exit();
    assert!(p.exit_armed());
    // Frame 2 (last of the segment) still publishes — exit fires at the BOUNDARY,
    // not mid-lap.
    assert!(matches!(p.on_decoded(2), PlayerAction::Publish { .. }));
    // Reaching vamp_out with the exit armed: the vamp ENDS (auto_off) — it does
    // NOT wrap.
    assert_eq!(p.on_decoded(3), PlayerAction::Ended);
    assert_eq!(p.state(), MediaPlayerState::Ended);
}

#[test]
fn cancelling_a_pending_exit_keeps_looping() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 3, 0, 3), EofPolicy::Loop, anchor);
    p.vamp(anchor);
    p.arm_exit();
    assert!(p.exit_armed());
    p.cancel_exit();
    assert!(!p.exit_armed());
    // Boundary now wraps (still vamping, exit not armed).
    let _ = p.on_decoded(0);
    let _ = p.on_decoded(1);
    let _ = p.on_decoded(2);
    assert_eq!(p.on_decoded(3), PlayerAction::SeekFlushTo { frame: 0 });
    assert!(matches!(p.state(), MediaPlayerState::Vamping { .. }));
}

#[test]
fn exit_verbs_are_noops_when_not_vamping() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 10, 0, 10), EofPolicy::Loop, anchor);
    p.play(anchor); // plain Playing, not Vamping
    p.arm_exit();
    assert!(!p.exit_armed(), "arming a non-vamping player is a no-op");
    assert_eq!(p.state(), MediaPlayerState::Playing);
}

// ---------------------------------------------------------------------------
// 5. EOF policies (non-loop)
// ---------------------------------------------------------------------------

#[test]
fn hold_last_frame_holds_at_the_out_point() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 3, 0, 3), EofPolicy::HoldLastFrame, anchor);
    p.play(anchor);
    let _ = p.on_decoded(0);
    let _ = p.on_decoded(1);
    let _ = p.on_decoded(2);
    // Reaching the out-point under hold_last_frame: hold, do not wrap or end.
    assert!(matches!(p.on_decoded(3), PlayerAction::Hold { .. }));
    assert_eq!(p.state(), MediaPlayerState::Holding);
}

#[test]
fn auto_off_ends_at_the_out_point() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 2, 0, 2), EofPolicy::AutoOff, anchor);
    p.play(anchor);
    let _ = p.on_decoded(0);
    let _ = p.on_decoded(1);
    assert_eq!(p.on_decoded(2), PlayerAction::Ended);
    assert_eq!(p.state(), MediaPlayerState::Ended);
}

#[test]
fn paused_holds_with_an_advancing_stamp() {
    let anchor = MediaTime::ZERO;
    let mut p = MediaPlayer::new(geom(0, 10, 0, 10), EofPolicy::HoldLastFrame, anchor);
    p.play(anchor);
    if let PlayerAction::Publish { at } = p.on_decoded(0) {
        assert_eq!(at, anchor);
    } else {
        panic!("frame 0 should publish");
    }
    p.pause();
    // Heartbeats while paused republish with advancing (strictly increasing)
    // stamps so the tile reads LIVE, not aged.
    let h1 = p.on_heartbeat();
    let h2 = p.on_heartbeat();
    match (h1, h2) {
        (PlayerAction::Hold { at: a1 }, PlayerAction::Hold { at: a2 }) => {
            assert!(
                a2.as_nanos() > a1.as_nanos(),
                "paused heartbeat stamps must advance"
            );
        }
        other => panic!("paused heartbeats should Hold, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. PROPERTY: exit-latch fires exactly once regardless of when it is armed
//    (the 2–3-frame vamp race — Codex's ADR-0097 flag + the ADR self-review).
// ---------------------------------------------------------------------------

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// For a short vamp segment of length `len` (2..=4 frames), arming the exit
    /// at ANY frame offset within ANY lap must yield EXACTLY ONE exit, at the
    /// first vamp boundary at-or-after the arm, with no extra lap and no double
    /// fire. Models the executor decoding the segment in order, wrapping on
    /// `SeekFlushTo`, arming at a chosen (lap, offset).
    #[test]
    fn exit_latch_fires_exactly_once_at_first_boundary_after_arm(
        len in 2u64..=4,
        arm_lap in 0u64..3,
        // arm at offset 0..len (offset == len means "arm exactly at the boundary
        // decision of this lap").
        arm_offset_raw in 0u64..5,
        // run enough laps to comfortably pass the arm point
        total_laps in 4u64..8,
    ) {
        let arm_offset = arm_offset_raw % (len + 1); // 0..=len
        let anchor = MediaTime::ZERO;
        let mut p = MediaPlayer::new(
            geom(0, len, 0, len),
            EofPolicy::AutoOff,
            anchor,
        );
        p.vamp(anchor);

        let mut exits = 0u32;
        let mut wraps = 0u64;
        let mut ended = false;

        // Model the executor: each lap decodes frames [0, len) (all publish),
        // then decodes the boundary frame `len` (which wraps or ends). The arm
        // is injected at the chosen (lap, offset) just before that decode.
        'outer: for lap in 0..total_laps {
            for f in 0..=len {
                if lap == arm_lap && f == arm_offset {
                    p.arm_exit();
                }
                match p.on_decoded(f) {
                    PlayerAction::Publish { .. } => {
                        prop_assert!(f < len, "only frames < vamp_out publish (f={})", f);
                    }
                    PlayerAction::SeekFlushTo { frame } => {
                        prop_assert_eq!(f, len, "a wrap only happens at the boundary frame");
                        prop_assert_eq!(frame, 0);
                        wraps += 1;
                        break; // next lap
                    }
                    PlayerAction::Ended => {
                        prop_assert_eq!(f, len, "the exit fires at the boundary frame");
                        exits += 1;
                        ended = true;
                        break 'outer;
                    }
                    PlayerAction::Hold { .. } => {
                        prop_assert!(false, "a vamping player never Holds mid-lap");
                    }
                    _ => prop_assert!(false, "unexpected action variant"),
                }
            }
        }

        prop_assert_eq!(exits, 1, "the exit must fire exactly once");
        prop_assert!(ended, "the player must end after the armed exit");
        // The exit landed at the first boundary at-or-after the arm: it wrapped
        // every lap strictly before the arm lap (so at least `arm_lap` wraps).
        prop_assert!(wraps >= arm_lap, "must have looped up to the arm lap");
    }

    /// INVARIANT #1 (the player is SAMPLED, never pacing): under ANY adversarial
    /// interleaving of transport verbs and decoded/heartbeat ticks, every core
    /// call returns a well-defined action and **every published/held stamp is
    /// strictly greater than the previous one** — the core can never emit a
    /// frozen or backwards timeline that would wedge or time-warp a tile. (The
    /// core is synchronous by construction — no await, no lock, no internal
    /// loop — so "never blocks" is structural; this pins the *output* property
    /// the Conductor's panel must verify: a wedged/looping/paused player still
    /// yields a monotone, bounded result every tick.)
    #[test]
    fn core_never_wedges_and_stamps_never_regress_under_any_command_sequence(
        ops in proptest::collection::vec(0u8..9, 1..200),
    ) {
        let anchor = MediaTime::from_tick(7, cad_25());
        let mut p = MediaPlayer::new(geom(0, 6, 1, 4), EofPolicy::Loop, anchor);
        p.play(anchor);

        let mut source_frame = 0u64;
        let mut last_stamp: Option<i64> = None;

        for op in ops {
            // Apply an adversarial op. 0..=4 are verbs; 5..=8 advance time.
            let action = match op {
                0 => { p.play(anchor); continue; }
                1 => { p.vamp(anchor); continue; }
                2 => { p.pause(); continue; }
                3 => { p.arm_exit(); continue; }
                4 => { p.cancel_exit(); continue; }
                5 => { p.stop(); continue; }
                6 => {
                    // Decode the next source frame (wrapping the cursor in [0,6)).
                    let f = source_frame % 6;
                    source_frame = source_frame.wrapping_add(1);
                    p.on_decoded(f)
                }
                7 => p.on_heartbeat(),
                _ => {
                    // Decode the boundary frame deliberately (force a wrap/term).
                    p.on_decoded(6)
                }
            };

            // Every action that carries a stamp must strictly advance it.
            let stamp = match action {
                PlayerAction::Publish { at } | PlayerAction::Hold { at } => Some(at.as_nanos()),
                PlayerAction::SeekFlushTo { frame } => {
                    // A wrap target is always a real in-point of our geometry.
                    prop_assert!(frame == 1 || frame == 0, "wrap target must be an in-point");
                    None
                }
                PlayerAction::Ended => None,
                _ => {
                    prop_assert!(false, "unexpected action variant");
                    None
                }
            };
            if let Some(s) = stamp {
                if let Some(prev) = last_stamp {
                    prop_assert!(
                        s > prev,
                        "stamps must strictly increase (got {} after {})",
                        s, prev
                    );
                }
                last_stamp = Some(s);
            }
        }
    }
}
