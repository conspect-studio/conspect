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
