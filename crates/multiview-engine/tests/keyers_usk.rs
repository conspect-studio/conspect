//! Upstream keyers through the real switcher hook and CPU drive (ADR-0113
//! §3.2/§4, design note §11.2-§11.3, slice K3): the `arm` set, armed keys on
//! CUT and on AUTO MIX / DIP / WIPE in exact scene groups (each composed
//! frame equals `merge(scene_out, scene_in)` with the scenes composed
//! separately through the public compositor), key-only transitions, an
//! unarmed USK below and above an armed one, fly placement, the abort that
//! restores key state, the fresh gate on keys going on, and the tally.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::too_many_lines
)]

#[path = "support/keyers_rig.rs"]
mod rig;

use std::sync::Arc;

use multiview_compositor::blend::{LinearRgba, PremulRgba};
use multiview_compositor::mask::{MaskShape, WipeMask};
use multiview_compositor::pipeline::{
    composite_with, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image, Tile,
};
use multiview_compositor::TileKey;
use multiview_core::switcher::{kernel, ArmedSet, FrameCount, Progress, TransitionDescriptor};
use multiview_core::time::Rational;
use multiview_engine::keyer::{
    usk_slot, DrawRole, TallyRole, REFUSE_KEYER_IN_TRANSITION, REFUSE_KEYER_NO_FRESH_FRAME,
    REFUSE_KEYER_USK_VERB,
};
use multiview_engine::switcher_control::{
    SwitcherVerb, REFUSE_ARM_UNKNOWN_KEY, REFUSE_LOST_DURING_AUTO, TAKE_PRIME_DEADLINE_TICKS,
};
use multiview_engine::CompositedFrame;
use rig::{cmd, drain, dsk, step, usk, Hook, Rig, LUMA_A, LUMA_B, LUMA_FILL, SIDE};

const LUMA_K2: u8 = 180;
/// A canvas rectangle `(x, y, w, h)`.
type Rect = (u32, u32, u32, u32);
/// USK 1 (slot 2) flies in the top-left quarter.
const FLY: Rect = (0, 0, 32, 32);

fn rig() -> Rig {
    Rig::new(&["a", "b", "gfx", "gfx2", "cold"])
}

fn fresh(rig: &Rig, index: u64) {
    rig.publish_background(index);
    rig.publish("gfx", LUMA_FILL, index);
    rig.publish("gfx2", LUMA_K2, index);
}

fn arm(background: bool, keys: &[u32]) -> SwitcherVerb {
    let mut set = ArmedSet::default();
    if background {
        set = set.with_background();
    }
    for &k in keys {
        set = set.with_key(k);
    }
    SwitcherVerb::Arm(Arc::new(set))
}

fn transition(json: serde_json::Value) -> SwitcherVerb {
    SwitcherVerb::SetTransition(serde_json::from_value::<TransitionDescriptor>(json).unwrap())
}

fn rate(n: u32) -> FrameCount {
    FrameCount::try_from(n).unwrap()
}

fn on_air(handle: &multiview_engine::switcher_control::SwitcherControl, key: u32) -> bool {
    handle.snapshot().unwrap().keyers[usk_slot(key).unwrap()]
        .unwrap()
        .on_air
}

// --- the independent reference: each scene composed on its own -----------

fn canvas() -> CanvasColor {
    CanvasColor::default()
}

fn solid(luma: u8, w: u32, h: u32) -> Nv12Image {
    Rig::solid(luma, w, h)
}

/// One scene: a full-canvas background of `bg` and keys `(luma, fly)` above
/// it (linear self keys, opaque fills), composed with no groups.
fn scene(bg: u8, keys: &[(u8, Rect)]) -> Nv12Image {
    let background = solid(bg, SIDE, SIDE);
    let fills: Vec<_> = keys.iter().map(|&(l, _)| solid(l, SIDE, SIDE)).collect();
    let mut tiles = vec![Tile::placed(&background, 0, 0, 1.0)];
    for (fill, &(_, (x, y, w, h))) in fills.iter().zip(keys) {
        tiles.push(Tile::scaled(fill, x, y, w, h, 1.0).with_key(TileKey::linear()));
    }
    composite_with(SIDE, SIDE, canvas(), LinearRgba::TRANSPARENT, &tiles, false).unwrap()
}

fn decoded(img: &Nv12Image, x: u32, y: u32) -> PremulRgba {
    let (y8, cb, cr) = img.sample(x, y).unwrap();
    let rgb = tile_yuv_to_canvas_linear(y8, cb, cr, img.color(), canvas()).unwrap();
    LinearRgba::opaque(rgb[0], rgb[1], rgb[2]).premultiplied()
}

fn lerp(a: PremulRgba, b: PremulRgba, t: f32) -> PremulRgba {
    PremulRgba {
        r: a.r * (1.0 - t) + b.r * t,
        g: a.g * (1.0 - t) + b.g * t,
        b: a.b * (1.0 - t) + b.b * t,
        a: a.a * (1.0 - t) + b.a * t,
    }
}

/// The luma of `merge(scene_out, scene_in)` at `(x, y)` with weight `t`,
/// encoded through the composite's own back half (a 1-tile composite of a
/// solid built from the merged linear value is avoided: the encode is the
/// public `canvas_linear_to_output_yuv`).
fn merged_luma(out: &Nv12Image, inc: &Nv12Image, x: u32, y: u32, t: f32) -> u8 {
    let p = lerp(decoded(out, x, y), decoded(inc, x, y), t).unpremultiplied();
    multiview_compositor::pipeline::canvas_linear_to_output_yuv([p.r, p.g, p.b], canvas()).unwrap()
        [0]
}

fn luma(frame: &CompositedFrame, x: u32, y: u32) -> u8 {
    frame.canvas.sample(x, y).unwrap().0
}

/// Points inside the fly (8, 8) and outside it (48, 48).
const PROBES: [(u32, u32); 2] = [(8, 8), (48, 48)];

fn assert_merged(
    frame: &CompositedFrame,
    out: &Nv12Image,
    inc: &Nv12Image,
    t: impl Fn(u32, u32) -> f32,
    what: &str,
) {
    for (x, y) in PROBES {
        let want = merged_luma(out, inc, x, y, t(x, y));
        let got = luma(frame, x, y);
        assert!(got.abs_diff(want) <= 1, "{what} ({x},{y}): {got} vs {want}");
    }
}

fn build(
    rig: &Rig,
    usks: &[multiview_config::UpstreamKeyerConfig],
) -> (
    multiview_engine::switcher_control::SwitcherControl,
    Hook,
    multiview_engine::CompositorDrive<Nv12Image>,
) {
    rig.build_usk(&[], usks, 20.0, "a", "b")
}

// --- arm --------------------------------------------------------------------

#[test]
fn arm_validates_keys_and_is_refused_during_a_take() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    for (op, verb) in [
        ("dup", arm(false, &[1, 1])),
        ("range", arm(false, &[0])),
        ("undefined", arm(false, &[2])),
        ("empty", arm(false, &[])),
        ("ok", arm(true, &[1])),
    ] {
        handle.try_send(cmd(op, verb)).unwrap();
    }
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let got = drain(&mut outcomes).0;
    let by = |op: &str| got.iter().find(|o| o.operation_id == op).unwrap().clone();
    assert!(by("dup").error.as_deref().unwrap().contains("distinct"));
    assert!(by("range").error.is_some());
    assert_eq!(
        by("undefined").error.as_deref(),
        Some(REFUSE_ARM_UNKNOWN_KEY)
    );
    assert!(by("empty").error.as_deref().unwrap().contains("armed"));
    assert!(by("ok").completed);
    let next = handle.snapshot().unwrap().next;
    assert!(next.background && next.keys[0]);
    // During a take the arm set is frozen.
    handle
        .try_send(cmd(
            "mix",
            transition(serde_json::json!({"kind": "mix", "rate": 4})),
        ))
        .unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate(4))))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    handle.try_send(cmd("late", arm(true, &[]))).unwrap();
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    let late = drain(&mut outcomes).0;
    let late = late.iter().find(|o| o.operation_id == "late").unwrap();
    assert!(
        late.error.as_deref().unwrap().contains("active take"),
        "{late:?}"
    );
}

// --- CUT ----------------------------------------------------------------------

#[test]
fn an_armed_key_flips_on_the_cut_tick_at_its_fly_and_an_unarmed_one_stays() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(
        &rig,
        &[
            usk("usk1", "gfx", 1, Some(FLY)),
            usk("usk2", "gfx2", 2, Some((32, 32, 32, 32))),
        ],
    );
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 1);
    let frame = step(&mut hook, &mut drive, 1);
    assert!(on_air(&handle, 1) && !on_air(&handle, 2));
    // Key-only CUT: the background stays A; USK1 keyed in its fly only.
    assert!(luma(&frame, 8, 8).abs_diff(LUMA_FILL) < 4);
    assert!(luma(&frame, 48, 48).abs_diff(LUMA_A) < 4);
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
    let tally = handle.snapshot().unwrap().tally;
    assert!(tally.program.contains(TallyRole::Fill(2)));
    // Background + both keys: B, USK1 off, USK2 on.
    handle.try_send(cmd("arm2", arm(true, &[1, 2]))).unwrap();
    handle.try_send(cmd("cut2", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 2);
    let frame = step(&mut hook, &mut drive, 2);
    assert!(!on_air(&handle, 1) && on_air(&handle, 2));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
    assert!(luma(&frame, 8, 8).abs_diff(LUMA_B) < 4);
    assert!(luma(&frame, 48, 48).abs_diff(LUMA_K2) < 4);
    assert_eq!(
        hook.driver
            .keyers()
            .last_good(usk_slot(1).unwrap())
            .map(|_| ()),
        None,
        "a USK going off releases its pair"
    );
}

// --- AUTO MIX / DIP / WIPE against merge(scene_out, scene_in) ------------------

/// Frames 0, mid, (switch point) and end of a background + key transition: A
/// (USK1 off) → B + USK1 on. Every frame equals `lerp(scene_out, scene_in, t)`
/// with `t` from the transition (independent: kernel weights, a written-out
/// wipe reveal).
#[test]
fn a_key_going_on_with_the_background_merges_exactly_on_mix_dip_and_wipe() {
    let n = 8_u32;
    let half = Rational::new(1, 2);
    let out = scene(LUMA_A, &[]);
    let inc = scene(LUMA_B, &[(LUMA_FILL, FLY)]);
    for kind in ["mix", "dip", "wipe"] {
        let rig = rig();
        let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
        let descriptor = match kind {
            "mix" => serde_json::json!({"kind": "mix", "rate": n}),
            "dip" => serde_json::json!({"kind": "dip", "rate": n, "color": "#000000",
                "switch_point": {"num": half.num, "den": half.den}}),
            _ => serde_json::json!({"kind": "wipe", "pattern": "linear_edge", "rate": n}),
        };
        handle.try_send(cmd("set", transition(descriptor))).unwrap();
        handle.try_send(cmd("arm", arm(true, &[1]))).unwrap();
        handle
            .try_send(cmd("auto", SwitcherVerb::Auto(rate(n))))
            .unwrap();
        for k in 0..n {
            fresh(&rig, u64::from(k));
            let frame = step(&mut hook, &mut drive, u64::from(k));
            let p = kernel::opacity(Progress::new(k, rate(n)));
            if k == 0 || k == n / 2 || k + 1 == n {
                match kind {
                    "mix" => assert_merged(&frame, &out, &inc, |_, _| p, "mix"),
                    "dip" => {
                        // lerp(lerp(out, black, d), in, i), written out.
                        let (d, i) = kernel::dip_weights(k, n, half);
                        for (x, y) in PROBES {
                            let black = PremulRgba {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            };
                            let want =
                                lerp(lerp(decoded(&out, x, y), black, d), decoded(&inc, x, y), i)
                                    .unpremultiplied();
                            let want = multiview_compositor::pipeline::canvas_linear_to_output_yuv(
                                [want.r, want.g, want.b],
                                canvas(),
                            )
                            .unwrap()[0];
                            let got = luma(&frame, x, y);
                            assert!(
                                got.abs_diff(want) <= 1,
                                "dip k={k} ({x},{y}): {got} vs {want}"
                            );
                        }
                    }
                    _ => {
                        let mask = WipeMask::new(
                            MaskShape::Horizontal,
                            p,
                            SIDE,
                            SIDE,
                            [0.5, 0.5],
                            false,
                            0.0,
                            0.0,
                        );
                        assert_merged(&frame, &out, &inc, |x, y| mask.coverage(x, y), "wipe");
                    }
                }
            }
            let row = drive.keyer_draws().iter().flatten().find(|d| d.keyer == 2);
            assert_eq!(row.map(|d| d.role), Some(DrawRole::In), "{kind} k={k}");
            assert!(!on_air(&handle, 1));
        }
        fresh(&rig, u64::from(n));
        let end = step(&mut hook, &mut drive, u64::from(n));
        assert!(on_air(&handle, 1), "{kind}: flips on the terminal tick");
        assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
        for (x, y) in PROBES {
            assert!(
                luma(&end, x, y).abs_diff(luma_of(&inc, x, y)) <= 1,
                "{kind} end ({x},{y})"
            );
        }
    }
}

fn luma_of(img: &Nv12Image, x: u32, y: u32) -> u8 {
    img.sample(x, y).unwrap().0
}

/// The reverse: B + USK1 on → A with USK1 going off, on a MIX at mid.
#[test]
fn a_key_going_off_with_the_background_merges_exactly() {
    let rig = rig();
    let n = 8_u32;
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    assert!(on_air(&handle, 1));
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({"kind": "mix", "rate": n})),
        ))
        .unwrap();
    handle.try_send(cmd("arm2", arm(true, &[1]))).unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate(n))))
        .unwrap();
    let out = scene(LUMA_A, &[(LUMA_FILL, FLY)]);
    let inc = scene(LUMA_B, &[]);
    for k in 0..n {
        let index = 1 + u64::from(k);
        fresh(&rig, index);
        let frame = step(&mut hook, &mut drive, index);
        let p = kernel::opacity(Progress::new(k, rate(n)));
        assert_merged(&frame, &out, &inc, |_, _| p, &format!("off k={k}"));
        let row = drive.keyer_draws().iter().flatten().find(|d| d.keyer == 2);
        assert_eq!(row.map(|d| d.role), Some(DrawRole::Out));
    }
    fresh(&rig, 1 + u64::from(n));
    step(&mut hook, &mut drive, 1 + u64::from(n));
    assert!(!on_air(&handle, 1));
}

/// Key-only transitions (background not armed): A stays in both scenes, the
/// key mixes at `p`; a DIP draws as a MIX (design note §4); a WIPE reveals
/// the key behind the edge. The background never swaps and Program audio
/// sees no crossfade.
#[test]
fn key_only_transitions_mix_the_key_and_a_dip_draws_as_mix() {
    let n = 8_u32;
    let half = Rational::new(1, 2);
    let out = scene(LUMA_A, &[]);
    let inc = scene(LUMA_A, &[(LUMA_FILL, FLY)]);
    for kind in ["mix", "dip", "wipe"] {
        let rig = rig();
        let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
        let descriptor = match kind {
            "mix" => serde_json::json!({"kind": "mix", "rate": n}),
            "dip" => serde_json::json!({"kind": "dip", "rate": n,
                "switch_point": {"num": half.num, "den": half.den}}),
            _ => serde_json::json!({"kind": "wipe", "pattern": "linear_edge", "rate": n}),
        };
        handle.try_send(cmd("set", transition(descriptor))).unwrap();
        handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
        handle
            .try_send(cmd("auto", SwitcherVerb::Auto(rate(n))))
            .unwrap();
        for k in 0..n {
            fresh(&rig, u64::from(k));
            let frame = step(&mut hook, &mut drive, u64::from(k));
            assert!(
                frame.mix.is_none(),
                "{kind}: no background crossfade for audio"
            );
            let p = kernel::opacity(Progress::new(k, rate(n)));
            if kind == "wipe" {
                let mask = WipeMask::new(
                    MaskShape::Horizontal,
                    p,
                    SIDE,
                    SIDE,
                    [0.5, 0.5],
                    false,
                    0.0,
                    0.0,
                );
                assert_merged(
                    &frame,
                    &out,
                    &inc,
                    |x, y| mask.coverage(x, y),
                    "key-only wipe",
                );
            } else {
                // MIX and the key-only DIP: lerp(out, in, p), never a dip.
                assert_merged(
                    &frame,
                    &out,
                    &inc,
                    |_, _| p,
                    &format!("key-only {kind} k={k}"),
                );
            }
            let tally = handle.snapshot().unwrap().tally;
            assert!(
                !tally.program.contains(TallyRole::Preview),
                "{kind}: PVW not on air"
            );
            assert_eq!(
                tally.program.contains(TallyRole::Fill(2)),
                p > 0.0,
                "{kind} k={k}"
            );
        }
        fresh(&rig, u64::from(n));
        step(&mut hook, &mut drive, u64::from(n));
        assert!(on_air(&handle, 1));
        assert_eq!(handle.snapshot().unwrap().program.as_str(), "a", "{kind}");
    }
}

/// An unarmed on-air USK below the armed one is in both scenes; one above it
/// is folded once onto the merge (`top`). The armed USK goes on at mid MIX.
#[test]
fn unarmed_usks_below_and_above_an_armed_one_fold_in_the_right_place() {
    let rig = rig();
    let n = 8_u32;
    let full = (0, 0, SIDE, SIDE);
    let below = usk("below", "gfx2", 1, Some(full));
    let armed = usk("armed", "gfx", 2, Some(FLY));
    let above = usk("above", "gfx2", 3, Some((32, 32, 32, 32)));
    let (handle, mut hook, mut drive) = build(&rig, &[below, armed, above]);
    // Below and above on air (key indices 1 and 3).
    handle.try_send(cmd("arm", arm(false, &[1, 3]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({"kind": "mix", "rate": n})),
        ))
        .unwrap();
    handle.try_send(cmd("arm2", arm(false, &[2]))).unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate(n))))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let k = n / 2;
    let index = 1 + u64::from(k);
    fresh(&rig, index);
    let frame = step(&mut hook, &mut drive, index);
    let roles: Vec<_> = drive
        .keyer_draws()
        .iter()
        .flatten()
        .map(|d| (d.keyer, d.role))
        .collect();
    assert_eq!(
        roles,
        vec![(2, DrawRole::Both), (3, DrawRole::In), (4, DrawRole::Top)],
        "below = both, armed = in, above = top"
    );
    let p = kernel::opacity(Progress::new(k, rate(n)));
    // `below` covers the canvas, so both scenes are it (plus `armed` in the
    // incoming one); `above` is then drawn over the merge.
    let out = scene(LUMA_A, &[(LUMA_K2, full)]);
    let inc = scene(LUMA_A, &[(LUMA_K2, full), (LUMA_FILL, FLY)]);
    let want = merged_luma(&out, &inc, 8, 8, p);
    assert!(luma(&frame, 8, 8).abs_diff(want) <= 1);
    assert!(luma(&frame, 48, 48).abs_diff(LUMA_K2) <= 1, "above on top");
    assert!(on_air(&handle, 1) && on_air(&handle, 3) && !on_air(&handle, 2));
}

/// ADR-0104 §2: an AUTO aborted on its terminal tick (incoming not fresh)
/// restores the armed keys' state: they never flip and their pair goes.
#[test]
fn an_aborted_auto_restores_key_state() {
    let mut rig = rig();
    rig.fast("b");
    let n = 8_u32;
    let (handle, mut hook, mut drive) = build(
        &rig,
        &[
            usk("on", "gfx2", 1, Some((32, 32, 32, 32))),
            usk("off", "gfx", 2, Some(FLY)),
        ],
    );
    let mut outcomes = handle.outcomes();
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    assert!(on_air(&handle, 1));
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({"kind": "mix", "rate": n})),
        ))
        .unwrap();
    handle.try_send(cmd("arm2", arm(true, &[1, 2]))).unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate(n))))
        .unwrap();
    // B is fresh when the AUTO starts (tick 1), then never again: by the
    // terminal tick (1 + n) its 2-tick hold has long expired.
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let mut last = None;
    for k in 2..=1 + u64::from(n) {
        rig.publish("a", LUMA_A, k);
        rig.publish("gfx", LUMA_FILL, k);
        rig.publish("gfx2", LUMA_K2, k);
        last = Some(step(&mut hook, &mut drive, k));
    }
    let got = drain(&mut outcomes).0;
    let auto = got.iter().find(|o| o.operation_id == "auto").unwrap();
    assert_eq!(
        auto.error.as_deref(),
        Some(REFUSE_LOST_DURING_AUTO),
        "{got:?}"
    );
    assert_eq!(auto.tick, 1 + u64::from(n));
    assert!(on_air(&handle, 1), "key 1 stays on");
    assert!(!on_air(&handle, 2), "key 2 stays off");
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
    assert!(!handle.snapshot().unwrap().in_flight);
    let frame = last.unwrap();
    assert!(
        luma(&frame, 48, 48).abs_diff(LUMA_K2) < 4,
        "key 1 still drawn"
    );
    assert!(luma(&frame, 8, 8).abs_diff(LUMA_A) < 4, "key 2 not drawn");
    assert!(drive
        .keyer_draws()
        .iter()
        .flatten()
        .all(|d| d.role == DrawRole::Top));
    assert!(
        hook.driver
            .keyers()
            .last_good(usk_slot(2).unwrap())
            .is_none(),
        "the key that never went on holds no pair"
    );
}

/// The fresh gate (design note §4): a take carrying a key going on waits for
/// that key's fill, then is refused at exactly +50 ticks, with 0 lost ticks;
/// a key going off is never gated.
#[test]
fn a_take_with_a_key_going_on_waits_for_its_fill_and_is_refused_at_the_deadline() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "cold", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    let mut ticks = Vec::new();
    let mut refused = None;
    for k in 0..=TAKE_PRIME_DEADLINE_TICKS + 2 {
        rig.publish_background(k);
        let frame = step(&mut hook, &mut drive, k);
        ticks.push(frame.tick.index);
        for o in drain(&mut outcomes).0 {
            if o.operation_id == "cut" {
                refused = Some(o);
            }
        }
    }
    assert!(ticks.windows(2).all(|w| w[1] == w[0] + 1), "0 lost ticks");
    let refused = refused.unwrap();
    assert_eq!(refused.tick, TAKE_PRIME_DEADLINE_TICKS);
    assert!(refused.error.is_some());
    assert!(!on_air(&handle, 1));
    // With the fill live the held CUT lands on the first fresh tick.
    handle.try_send(cmd("cut2", SwitcherVerb::Cut)).unwrap();
    let base = TAKE_PRIME_DEADLINE_TICKS + 3;
    rig.publish_background(base);
    step(&mut hook, &mut drive, base);
    assert!(!on_air(&handle, 1), "held while cold");
    rig.publish_background(base + 1);
    rig.publish("cold", LUMA_FILL, base + 1);
    step(&mut hook, &mut drive, base + 1);
    assert!(on_air(&handle, 1), "lands once fresh");
    // Going off is never gated, even with the fill dead.
    handle.try_send(cmd("cut3", SwitcherVerb::Cut)).unwrap();
    for k in base + 2..base + 80 {
        rig.publish_background(k);
        step(&mut hook, &mut drive, k);
    }
    assert!(!on_air(&handle, 1));
    let _ = REFUSE_KEYER_NO_FRESH_FRAME;
}

/// A USK refuses `keyer_auto`/`keyer_tie` and a `keyer_cut` during a
/// background transition; outside one a `keyer_cut` flips it (design note
/// §8).
#[test]
fn usk_keyer_verbs_follow_the_design() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    let slot = u8::try_from(usk_slot(1).unwrap()).unwrap();
    handle
        .try_send(cmd(
            "auto",
            SwitcherVerb::KeyerAuto {
                keyer: slot,
                on: true,
                rate: None,
            },
        ))
        .unwrap();
    handle
        .try_send(cmd(
            "tie",
            SwitcherVerb::KeyerTie {
                keyer: slot,
                tie: true,
            },
        ))
        .unwrap();
    handle
        .try_send(cmd(
            "cut",
            SwitcherVerb::KeyerCut {
                keyer: slot,
                on: true,
            },
        ))
        .unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let got = drain(&mut outcomes).0;
    let by = |op: &str| got.iter().find(|o| o.operation_id == op).unwrap().clone();
    assert_eq!(by("auto").error.as_deref(), Some(REFUSE_KEYER_USK_VERB));
    assert_eq!(by("tie").error.as_deref(), Some(REFUSE_KEYER_USK_VERB));
    assert!(by("cut").completed);
    assert!(on_air(&handle, 1));
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({"kind": "mix", "rate": 6})),
        ))
        .unwrap();
    handle
        .try_send(cmd("bg", SwitcherVerb::Auto(rate(6))))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    handle
        .try_send(cmd(
            "cut2",
            SwitcherVerb::KeyerCut {
                keyer: slot,
                on: false,
            },
        ))
        .unwrap();
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    let got = drain(&mut outcomes).0;
    let cut2 = got.iter().find(|o| o.operation_id == "cut2").unwrap();
    assert_eq!(cut2.error.as_deref(), Some(REFUSE_KEYER_IN_TRANSITION));
    assert!(on_air(&handle, 1));
}

/// A USK and a DSK together: the USK is drawn below the DSK whatever their
/// z, and the DSK's pixels lie above the USK's.
#[test]
fn usks_draw_below_dsks() {
    let rig = rig();
    let (handle, mut hook, mut drive) = rig.build_usk(
        &[dsk("dsk1", "gfx2", None, -5)],
        &[usk("usk1", "gfx", 10, None)],
        20.0,
        "a",
        "b",
    );
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    handle
        .try_send(cmd("dsk", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    fresh(&rig, 0);
    let frame = step(&mut hook, &mut drive, 0);
    let order: Vec<_> = drive
        .keyer_draws()
        .iter()
        .flatten()
        .map(|d| d.keyer)
        .collect();
    assert_eq!(order, vec![2, 0], "USK first, then DSK");
    assert!(luma(&frame, 8, 8).abs_diff(LUMA_K2) < 4, "the DSK on top");
    assert!(handle.snapshot().unwrap().keyers[0].unwrap().on_air && on_air(&handle, 1));
}

/// ADR-0112 §5 x ADR-0113 §3: the stinger clip is part of the M/E
/// transition, so it rolls over the M/E scene (an on-air USK included) and
/// stays below the downstream keyers. Draw order: USKs, the clip, DSKs.
#[test]
fn a_stinger_clip_covers_an_on_air_usk_and_stays_below_a_dsk() {
    const LUMA_CLIP: u8 = 90;
    let rig = Rig::new(&["a", "b", "gfx", "gfx2", "p"]);
    // The USK fills the top-left quarter; the DSK the bottom-right one.
    let (handle, mut hook, mut drive) = rig.build_usk(
        &[dsk("dsk1", "gfx2", None, 0)].map(|mut d| {
            d.fly = Some(multiview_config::KeyerRect {
                x: 32,
                y: 32,
                w: 32,
                h: 32,
            });
            d
        }),
        &[usk("usk1", "gfx", 1, Some(FLY))],
        20.0,
        "a",
        "b",
    );
    let fresh_all = |index: u64| {
        fresh(&rig, index);
        rig.publish("p", LUMA_CLIP, index);
    };
    // Tick 0: the USK goes on with a CUT, the DSK goes on.
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    handle
        .try_send(cmd("dsk", SwitcherVerb::KeyerCut { keyer: 0, on: true }))
        .unwrap();
    fresh_all(0);
    let frame = step(&mut hook, &mut drive, 0);
    assert!(on_air(&handle, 1), "the USK is on air");
    assert!(luma(&frame, 8, 8).abs_diff(LUMA_FILL) < 4, "USK drawn");
    // Pre-roll 1, clip 8, trigger 2, mix 2: the clip is drawn from tick 2.
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({
                "kind": "stinger", "player": "p", "pre_roll": 1,
                "clip_duration": 8, "trigger_point": 2, "mix_rate": 2,
            })),
        ))
        .unwrap();
    handle
        .try_send(cmd("auto", SwitcherVerb::Auto(rate(9))))
        .unwrap();
    let mut seen = 0;
    for index in 1..=6 {
        fresh_all(index);
        let frame = step(&mut hook, &mut drive, index);
        let clip_on = luma(&frame, 16, 48).abs_diff(LUMA_CLIP) < 4;
        if clip_on {
            seen += 1;
            assert!(
                luma(&frame, 8, 8).abs_diff(LUMA_CLIP) < 4,
                "tick {index}: the opaque clip covers the on-air USK, got {}",
                luma(&frame, 8, 8)
            );
            assert!(
                luma(&frame, 48, 48).abs_diff(LUMA_K2) < 4,
                "tick {index}: the DSK stays above the clip, got {}",
                luma(&frame, 48, 48)
            );
        }
    }
    assert!(seen >= 3, "the clip was drawn on {seen} ticks");
}

/// Review K3-INV-1: an `arm` while a take is held by the fresh gate is
/// refused (`take-held`), so the held CUT lands with the set it was
/// admitted with: usk1 (cold) only, never usk2.
#[test]
fn arm_is_refused_while_a_take_is_held_and_the_held_take_keeps_its_set() {
    use multiview_engine::switcher_control::REFUSE_ARM_TAKE_HELD;
    let rig = rig();
    let (handle, mut hook, mut drive) = build(
        &rig,
        &[
            usk("usk1", "cold", 1, Some(FLY)),
            usk("usk2", "gfx2", 2, None),
        ],
    );
    let mut outcomes = handle.outcomes();
    handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    handle.try_send(cmd("cut", SwitcherVerb::Cut)).unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    assert!(!on_air(&handle, 1), "held: usk1's fill is cold");
    handle.try_send(cmd("rearm", arm(false, &[2]))).unwrap();
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    let got = drain(&mut outcomes).0;
    let rearm = got.iter().find(|o| o.operation_id == "rearm").unwrap();
    assert_eq!(rearm.error.as_deref(), Some(REFUSE_ARM_TAKE_HELD));
    assert!(handle.snapshot().unwrap().next.keys[0] && !handle.snapshot().unwrap().next.keys[1]);
    // The fill comes up: the held CUT lands with its original set.
    fresh(&rig, 3);
    rig.publish("cold", LUMA_FILL, 3);
    step(&mut hook, &mut drive, 3);
    let got = drain(&mut outcomes).0;
    assert!(got.iter().any(|o| o.operation_id == "cut" && o.completed));
    assert!(on_air(&handle, 1), "usk1 went on");
    assert!(!on_air(&handle, 2), "usk2 never toggled");
}

/// A take bound to the arm set and definitions the caller was authorized
/// against (as the control plane builds it).
fn bound(
    handle: &multiview_engine::switcher_control::SwitcherControl,
    verb: SwitcherVerb,
) -> SwitcherVerb {
    SwitcherVerb::Bound(Box::new(multiview_engine::switcher_control::BoundVerb {
        verb,
        arm_version: Some(handle.snapshot().unwrap().arm_version),
        defs_version: Some(handle.keyer_definitions().unwrap().version),
    }))
}

/// Re-point USK 1 to `fill` (a `keyer_set`, built off the clock).
fn repoint_usk1(
    handle: &multiview_engine::switcher_control::SwitcherControl,
    fill: &str,
    allow_on_air: bool,
) -> SwitcherVerb {
    let def = handle
        .resolve_usk(&usk("usk1", fill, 1, Some(FLY)), "me1", "main")
        .unwrap();
    let slot = u8::try_from(usk_slot(1).unwrap()).unwrap();
    handle.keyer_set_verb(
        &handle.keyer_definitions().unwrap(),
        slot,
        def,
        allow_on_air,
    )
}

/// Review round 2 (MEDIA-3 / AUTHZ-3 / K3-BOUND-1): a bound CUT or AUTO
/// held by the fresh gate keeps the versions it was authorized against. A
/// `keyer_set` re-pointing the armed, off-air USK while it is held (legal:
/// the keyer is not engaged) makes the held take refuse
/// `keyer-definitions-changed` when it would apply; it never puts the new
/// fill on Program.
#[test]
fn a_held_take_is_refused_when_an_armed_key_is_redefined_while_held() {
    use multiview_engine::keyer::REFUSE_KEYER_DEFINITION_CHANGED;
    for take in [SwitcherVerb::Cut, SwitcherVerb::Auto(rate(4))] {
        let rig = rig();
        let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "cold", 1, Some(FLY))]);
        let mut outcomes = handle.outcomes();
        handle
            .try_send(cmd(
                "set",
                transition(serde_json::json!({"kind": "mix", "rate": 4})),
            ))
            .unwrap();
        handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
        fresh(&rig, 0);
        step(&mut hook, &mut drive, 0);
        handle
            .try_send(cmd("take", bound(&handle, take.clone())))
            .unwrap();
        fresh(&rig, 1);
        step(&mut hook, &mut drive, 1);
        assert!(!on_air(&handle, 1), "held: usk1's fill is cold");
        // Another caller re-points the off-air key to a live fill.
        handle
            .try_send(cmd("repoint", repoint_usk1(&handle, "gfx", false)))
            .unwrap();
        fresh(&rig, 2);
        step(&mut hook, &mut drive, 2);
        let got = drain(&mut outcomes).0;
        assert!(
            got.iter()
                .any(|o| o.operation_id == "repoint" && o.completed),
            "{got:?}"
        );
        for k in 3..12 {
            fresh(&rig, k);
            step(&mut hook, &mut drive, k);
        }
        let got = drain(&mut outcomes).0;
        let outcome = got.iter().find(|o| o.operation_id == "take").unwrap();
        assert_eq!(
            outcome.error.as_deref(),
            Some(REFUSE_KEYER_DEFINITION_CHANGED),
            "{take:?}: {got:?}"
        );
        assert_eq!(outcome.tick, 3);
        assert!(!on_air(&handle, 1), "{take:?}: the new fill never went on");
    }
}

/// Review round 2 (K3-BOUND-1): a USK `keyer_cut` held for a fresh fill
/// keeps the definitions version it was authorized against: a `keyer_set`
/// (`on_air = "allow"`) replacing the held keyer's definition makes the
/// held cut refuse `keyer-definitions-changed`, never land on the new fill.
#[test]
fn a_held_usk_keyer_cut_is_refused_when_its_keyer_is_redefined() {
    use multiview_engine::keyer::REFUSE_KEYER_DEFINITION_CHANGED;
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "cold", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    let slot = u8::try_from(usk_slot(1).unwrap()).unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    handle
        .try_send(cmd(
            "kcut",
            bound(
                &handle,
                SwitcherVerb::KeyerCut {
                    keyer: slot,
                    on: true,
                },
            ),
        ))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    assert!(!on_air(&handle, 1), "held: the fill is cold");
    handle
        .try_send(cmd("repoint", repoint_usk1(&handle, "gfx", true)))
        .unwrap();
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    for k in 3..8 {
        fresh(&rig, k);
        step(&mut hook, &mut drive, k);
    }
    let got = drain(&mut outcomes).0;
    assert!(
        got.iter()
            .any(|o| o.operation_id == "repoint" && o.completed),
        "{got:?}"
    );
    let kcut = got.iter().find(|o| o.operation_id == "kcut").unwrap();
    assert_eq!(
        kcut.error.as_deref(),
        Some(REFUSE_KEYER_DEFINITION_CHANGED),
        "{got:?}"
    );
    assert_eq!(kcut.tick, 3);
    assert!(!on_air(&handle, 1));
}

/// Review round 2 (MEDIA-2): a held USK `keyer_cut` whose fill becomes
/// fresh while a background AUTO is in flight is refused
/// `keyer-in-transition` (the admission rule, applied when it would land),
/// never an abrupt overlay mid-transition.
#[test]
fn a_held_usk_keyer_cut_never_lands_during_a_background_auto() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "cold", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    let slot = u8::try_from(usk_slot(1).unwrap()).unwrap();
    handle
        .try_send(cmd(
            "set",
            transition(serde_json::json!({"kind": "mix", "rate": 8})),
        ))
        .unwrap();
    handle
        .try_send(cmd(
            "kcut",
            SwitcherVerb::KeyerCut {
                keyer: slot,
                on: true,
            },
        ))
        .unwrap();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    assert!(!on_air(&handle, 1), "held: the fill is cold");
    handle
        .try_send(cmd("bg", SwitcherVerb::Auto(rate(8))))
        .unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    assert!(handle.snapshot().unwrap().in_flight);
    // The fill comes up mid-AUTO.
    for k in 2..6 {
        fresh(&rig, k);
        rig.publish("cold", LUMA_FILL, k);
        step(&mut hook, &mut drive, k);
        assert!(!on_air(&handle, 1), "tick {k}: no overlay mid-transition");
    }
    let got = drain(&mut outcomes).0;
    let kcut = got.iter().find(|o| o.operation_id == "kcut").unwrap();
    assert_eq!(
        kcut.error.as_deref(),
        Some(REFUSE_KEYER_IN_TRANSITION),
        "{got:?}"
    );
    assert_eq!(kcut.tick, 2);
}

/// Review round 3 (AUTHZ-R3-1): an `arm` is bound to the keyer definitions
/// its sources were authorized against. A `keyer_set` re-pointing the key
/// that drains before it makes the arm refuse `keyer-definitions-changed`,
/// leaving the armed set unchanged (never arming the new source on the
/// arming caller's authority).
#[test]
fn an_arm_bound_to_older_definitions_is_refused_and_arms_nothing() {
    use multiview_engine::keyer::REFUSE_KEYER_DEFINITION_CHANGED;
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "cold", 1, Some(FLY))]);
    let mut outcomes = handle.outcomes();
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    // The arm is authorized against the current definitions ...
    let armed = SwitcherVerb::Bound(Box::new(multiview_engine::switcher_control::BoundVerb {
        verb: arm(false, &[1]),
        arm_version: None,
        defs_version: Some(handle.keyer_definitions().unwrap().version),
    }));
    // ... but a re-point is queued ahead of it.
    handle
        .try_send(cmd("repoint", repoint_usk1(&handle, "gfx", false)))
        .unwrap();
    handle.try_send(cmd("arm", armed)).unwrap();
    fresh(&rig, 1);
    step(&mut hook, &mut drive, 1);
    let got = drain(&mut outcomes).0;
    assert!(
        got.iter()
            .any(|o| o.operation_id == "repoint" && o.completed),
        "{got:?}"
    );
    let arm_outcome = got.iter().find(|o| o.operation_id == "arm").unwrap();
    assert_eq!(
        arm_outcome.error.as_deref(),
        Some(REFUSE_KEYER_DEFINITION_CHANGED),
        "{got:?}"
    );
    assert!(
        !handle.snapshot().unwrap().next.keys[0],
        "nothing was armed"
    );
}

/// Review round 4 (K3-R3-2): an `arm` on an idle machine stages only the
/// previous arm set (no `SwitcherState` clone on the clock). A rejected
/// frame still restores that set and the arm version exactly, alone and
/// when a full stage follows in the same tick.
#[test]
fn a_rejected_frame_restores_an_arm_staged_without_a_machine_clone() {
    let rig = rig();
    let (handle, mut hook, mut drive) = build(&rig, &[usk("usk1", "gfx", 1, Some(FLY))]);
    fresh(&rig, 0);
    step(&mut hook, &mut drive, 0);
    let before = handle.snapshot().unwrap();
    let t1 = rig::tick(1);
    for with_full_stage in [false, true] {
        handle.try_send(cmd("arm", arm(false, &[1]))).unwrap();
        if with_full_stage {
            handle
                .try_send(cmd(
                    "mix",
                    transition(serde_json::json!({"kind": "mix", "rate": 4})),
                ))
                .unwrap();
        }
        hook.driver.drain_at_tick(t1);
        hook.driver.reject_composed_tick(t1, "compose failed");
    }
    fresh(&rig, 2);
    step(&mut hook, &mut drive, 2);
    let after = handle.snapshot().unwrap();
    assert_eq!(after.next, before.next, "the arm set is restored");
    assert_eq!(after.arm_version, before.arm_version, "and its version");
}
