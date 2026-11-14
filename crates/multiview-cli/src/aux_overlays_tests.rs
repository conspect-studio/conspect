#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! The routed-layout overlay bake: per-cell chrome from the layout's own
//! layers, the compose's per-cell state, the consumer's signals; failure is
//! the un-overlaid picture, never a held bus.

use super::*;
use crate::aux_layout::{LayoutCanvas, LayoutCanvasSpec};
use crate::routing_aux::StoreLookup;
use multiview_compositor::overlay::subpass::{OverlayColor, OverlayDrawList, OverlayPrimitive};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_framestore::TileStore;

const W: u32 = 1920;
const H: u32 = 1080;
/// The tally colours `overlays.rs` draws (red = Program, green = Preview).
const TALLY_RED: OverlayColor = OverlayColor::opaque(0.9, 0.12, 0.12);
const TALLY_GREEN: OverlayColor = OverlayColor::opaque(0.1, 0.85, 0.2);

/// A `NewsOne`-like 1 + 8 layout: one big cell top-left, three down the right,
/// five along the bottom. Source `s<i>` is bound to cell `i`.
pub(crate) fn newsone(overlays: LayoutOverlays, w: u32, h: u32) -> Arc<RoutedLayout> {
    let cell = |i: usize, x: f32, y: f32, cw: f32, ch: f32| Cell {
        x,
        y,
        w: cw,
        h: ch,
        source: Some(format!("s{i}")),
        ..Cell::default()
    };
    let mut cells = vec![cell(0, 0.0, 0.0, 0.75, 0.75)];
    for (i, y) in [0.0f32, 0.25, 0.5].into_iter().enumerate() {
        cells.push(cell(i + 1, 0.75, y, 0.25, 0.25));
    }
    for (i, x) in [0.0f32, 0.2, 0.4, 0.6, 0.8].into_iter().enumerate() {
        cells.push(cell(i + 4, x, 0.75, 0.2, 0.25));
    }
    let slates = vec![multiview_config::FailoverSlate::Black; cells.len()];
    Arc::new(RoutedLayout {
        id: "news".to_owned(),
        layout: Layout {
            name: "news".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 25,
                fps_den: 1,
            },
            cells,
        },
        slates,
        audio: multiview_config::LayoutAudio::default(),
        overlays,
        revision: 0,
    })
}

pub(crate) fn spec(w: u32, h: u32) -> LayoutCanvasSpec {
    LayoutCanvasSpec {
        width: w,
        height: h,
        color: CanvasColor::default(),
        background: [16, 128, 128],
    }
}

pub(crate) fn labels() -> LabelLookup {
    // s3 has no name: the bake falls back to its id.
    Arc::new(|id: &str| (id != "s3").then(|| format!("Name {id}")))
}

fn inputs() -> AuxOverlayInputs {
    AuxOverlayInputs {
        signals: signals_slot(),
        wanted: Arc::new(AtomicBool::new(false)),
        wanted_sources: wanted_sources(),
        labels: labels(),
        watermark: None,
        wall_clock: WallClockSource::system(),
        canvas_color: CanvasColor::default(),
        source_probes: Arc::default(),
    }
}

fn tile(w: u32, h: u32, luma: u8) -> Nv12Image {
    Nv12Image::solid(w, h, luma, 128, 128, CanvasColor::default().output_tag()).unwrap()
}

/// Stores for `s0..s8` except `missing`, each with one frame at t=0.
pub(crate) fn stores(
    missing: &[&str],
    w: u32,
    h: u32,
) -> (StoreLookup, Vec<Arc<TileStore<Nv12Image>>>) {
    let all: Vec<Arc<TileStore<Nv12Image>>> = (0..9)
        .map(|i| Arc::new(TileStore::<Nv12Image>::with_defaults(format!("s{i}"))))
        .collect();
    for store in &all {
        store.publish(tile(w / 4, h / 4, 120), MediaTime::from_nanos(0));
    }
    let missing: Vec<String> = missing.iter().map(|s| (*s).to_owned()).collect();
    let lookup_stores = all.clone();
    let lookup: StoreLookup = Arc::new(move |id: &str| {
        if missing.iter().any(|m| m == id) {
            return None;
        }
        lookup_stores.iter().find(|s| s.id() == id).cloned()
    });
    (lookup, all)
}

pub(crate) fn signals() -> AuxOverlaySignals {
    let mut out = AuxOverlaySignals::default();
    out.meters
        .insert("s0".to_owned(), MeterChannels::from_db(&[-3.0, -9.0]));
    out.meters
        .insert("s5".to_owned(), MeterChannels::from_db(&[-20.0]));
    // The consumer reports faults only for Program's sources (its detector
    // samples Program's cells): s1 is on Program and FROZEN there.
    out.faults.insert("s1".to_owned(), TileFault::Frozen);
    out.program.insert("s1".to_owned());
    out
}

fn glyphs_in(list: &OverlayDrawList, x: i32, y: i32, w: u32, h: u32) -> usize {
    list.primitives
        .iter()
        .filter(|p| match p {
            OverlayPrimitive::Glyph { dest_x, dest_y, .. } => {
                i64::from(*dest_x) >= i64::from(x)
                    && i64::from(*dest_x) < i64::from(x) + i64::from(w)
                    && i64::from(*dest_y) >= i64::from(y)
                    && i64::from(*dest_y) < i64::from(y) + i64::from(h)
            }
            _ => false,
        })
        .count()
}

fn lines_of(list: &OverlayDrawList, color: OverlayColor) -> Vec<(i32, i32, u32, u32)> {
    list.primitives
        .iter()
        .filter_map(|p| match p {
            OverlayPrimitive::Line { rect, color: c } if *c == color => {
                Some((rect.x, rect.y, rect.width, rect.height))
            }
            _ => None,
        })
        .collect()
}

/// Compose + bake once; return the bake, the composed and baked pictures,
/// and the draw list for the same dynamics.
fn bake_once(
    layers: LayoutOverlays,
    missing: &[&str],
    preview: Option<&str>,
) -> (
    LayoutOverlayBake,
    Arc<Nv12Image>,
    Arc<Nv12Image>,
    OverlayDrawList,
) {
    let body = newsone(layers, W, H);
    let (lookup, _stores) = stores(missing, W, H);
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    canvas.keep_frames(true);
    let pts = MediaTime::from_nanos(40_000_000);
    let picture = canvas.compose(&lookup, pts).unwrap();
    let mut bake = LayoutOverlayBake::new(&body, &inputs()).expect("layers on: a bake");
    let baked = bake.bake(
        &picture,
        pts,
        canvas.cell_states(),
        canvas.cell_frames(),
        &signals(),
        preview,
    );
    let dynamics = bake.last_dynamics().clone();
    let list = bake
        .baker
        .draw_list(pts, &dynamics, &HashMap::new(), &HashMap::new())
        .unwrap();
    (bake, picture, baked, list)
}

fn cell_px(body: &RoutedLayout, i: usize) -> (i32, i32, u32, u32) {
    let (x, y, w, h) = crate::aux_layout::cell_rect(&body.layout.cells[i], W, H);
    (i32::try_from(x).unwrap(), i32::try_from(y).unwrap(), w, h)
}

#[test]
fn a_newsone_layout_bakes_one_label_meter_flag_and_tally_per_cell() {
    let layers = LayoutOverlays::default();
    let (bake, picture, baked, list) = bake_once(layers, &["s7"], None);
    assert_eq!(
        bake.counters(),
        AuxOverlayCounters {
            baked: 1,
            errors: 0
        }
    );
    assert!(!Arc::ptr_eq(&picture, &baked), "a new, overlaid picture");
    assert_ne!(picture.y_plane(), baked.y_plane(), "the overlays are drawn");

    // One label per bound cell, with the display name (the id when unnamed).
    let tiles = bake.baker.tiles();
    assert_eq!(tiles.len(), 9);
    for (i, tile) in tiles.iter().enumerate() {
        let want = if i == 3 {
            "s3".to_owned()
        } else {
            format!("Name s{i}")
        };
        assert_eq!(tile.label, want);
    }
    let body = newsone(layers, W, H);
    for i in 0..9 {
        let (x, y, w, h) = cell_px(&body, i);
        let band = (h / 8).clamp(14, 40);
        let label_y = y + i32::try_from(h - band).unwrap();
        assert!(
            glyphs_in(&list, x, label_y, w / 2, band) > 0,
            "cell {i} has its label"
        );
        assert!(
            glyphs_in(&list, x, y, w / 2, band) > 0,
            "cell {i} has its state chip"
        );
    }

    // Meters come from the signal map; others ride the floor.
    let d = bake.last_dynamics();
    assert_eq!(d["s0"].channels, MeterChannels::from_db(&[-3.0, -9.0]));
    assert_eq!(d["s5"].channels, MeterChannels::from_db(&[-20.0]));
    assert_eq!(d["s4"].channels, MeterChannels::EMPTY);
    // Flags: the store state compose read (s7's store is missing).
    assert_eq!(d["s0"].state, SourceState::Live);
    assert_eq!(d["s7"].state, SourceState::NoSignal);
    // Program's verdict for its own source; a source off Program shows the
    // layout's (here clean after one frame).
    assert_eq!(d["s1"].fault, TileFault::Frozen);
    assert_eq!(d["s2"].fault, TileFault::None);
    assert_eq!(d["s0"].fault, TileFault::None);
    // Tally: s1 is on Program, nobody else.
    assert_eq!(d["s1"].tally, TileTally::Program);
    assert_eq!(d["s0"].tally, TileTally::Off);
    let red = lines_of(&list, TALLY_RED);
    assert_eq!(red.len(), 4, "one four-sided red border");
    let (x, y, w, h) = cell_px(&body, 1);
    for (lx, ly, lw, lh) in red {
        assert!(lx >= x && ly >= y, "inside the Program cell");
        assert!(i64::from(lx) + i64::from(lw) <= i64::from(x) + i64::from(w));
        assert!(i64::from(ly) + i64::from(lh) <= i64::from(y) + i64::from(h));
    }
    assert_eq!(lines_of(&list, TALLY_GREEN).len(), 0);
}

#[test]
fn the_preview_source_gets_a_green_tally_and_program_wins_over_preview() {
    let (bake, _, _, list) = bake_once(LayoutOverlays::default(), &[], Some("s4"));
    assert_eq!(bake.last_dynamics()["s4"].tally, TileTally::Preview);
    assert_eq!(lines_of(&list, TALLY_GREEN).len(), 4);
    let (bake, _, _, _) = bake_once(LayoutOverlays::default(), &[], Some("s1"));
    assert_eq!(bake.last_dynamics()["s1"].tally, TileTally::Program);
}

#[test]
fn the_clock_band_is_present_iff_the_clock_layer_is_on() {
    let on = LayoutOverlays::default();
    let off = LayoutOverlays { clock: false, ..on };
    let band = |list: &OverlayDrawList| glyphs_in(list, 0, 0, 600, 80);
    let (_, _, _, with) = bake_once(on, &[], None);
    let (_, _, _, without) = bake_once(off, &[], None);
    assert!(
        band(&with) > band(&without),
        "the clock adds glyphs to the top-left band ({} vs {})",
        band(&with),
        band(&without)
    );
    // Labels off removes every label but keeps chips; flags off removes chips.
    let (_, _, _, no_labels) = bake_once(
        LayoutOverlays {
            labels: false,
            clock: false,
            ..on
        },
        &[],
        None,
    );
    let body = newsone(on, W, H);
    let (x, y, w, h) = cell_px(&body, 0);
    let chip = (h / 8).clamp(14, 40);
    let label_y = y + i32::try_from(h - chip).unwrap();
    assert_eq!(glyphs_in(&no_labels, x, label_y, w / 2, chip), 0);
    assert!(glyphs_in(&no_labels, x, y, w / 2, chip) > 0);
}

#[test]
fn an_all_off_layout_has_no_bake() {
    let body = newsone(LayoutOverlays::none(), W, H);
    assert!(LayoutOverlayBake::new(&body, &inputs()).is_none());
    // An edited body that turns every layer off drops an existing bake.
    let on = newsone(LayoutOverlays::default(), W, H);
    let mut bake = LayoutOverlayBake::new(&on, &inputs()).unwrap();
    assert!(!bake.rebuild(&body, &labels()));
    let tally_only = newsone(
        LayoutOverlays {
            tally: true,
            ..LayoutOverlays::none()
        },
        W,
        H,
    );
    assert!(bake.rebuild(&tally_only, &labels()));
    assert_eq!(bake.layers(), tally_only.overlays);
}

#[test]
fn a_bake_error_publishes_the_un_overlaid_picture() {
    let body = newsone(LayoutOverlays::default(), W, H);
    let mut bake = LayoutOverlayBake::new(&body, &inputs()).unwrap();
    // A mislabelled picture: an unresolved colour tag the blend refuses.
    let bad = Arc::new(Nv12Image::solid(W, H, 16, 128, 128, ColorInfo::default()).unwrap());
    let out = bake.bake(
        &bad,
        MediaTime::from_nanos(0),
        &[],
        &[],
        &AuxOverlaySignals::default(),
        None,
    );
    assert!(Arc::ptr_eq(&out, &bad), "the same picture, un-overlaid");
    assert_eq!(
        bake.counters(),
        AuxOverlayCounters {
            baked: 0,
            errors: 1
        }
    );
    // A missing cell state reads NO SIGNAL, never LIVE.
    assert_eq!(bake.last_dynamics()["s0"].state, SourceState::NoSignal);
}

#[test]
fn the_publisher_is_inert_until_a_layout_wants_signals_and_aux_is_active() {
    let slot = signals_slot();
    let wanted = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(true));
    let publisher = AuxSignalsPublisher::new(
        Arc::clone(&slot),
        Arc::clone(&wanted),
        wanted_sources(),
        Arc::clone(&active),
    );
    assert!(!publisher.wanted());
    wanted.store(true, Ordering::Relaxed);
    assert!(publisher.wanted());
    active.store(false, Ordering::Relaxed);
    assert!(!publisher.wanted(), "nothing routed: no work");
    publisher.publish(signals());
    assert!(slot.load().program.contains("s1"));
}

/// Cost (no timing assert): one aux bake of a 1 + 8 `NewsOne` layout at
/// 1920x1080 with every default layer on. Run with `--nocapture`.
#[test]
fn measure_one_1080p_newsone_aux_bake() {
    let body = newsone(LayoutOverlays::default(), W, H);
    let (lookup, _stores) = stores(&[], W, H);
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    canvas.keep_frames(true);
    let started = std::time::Instant::now();
    let mut bake = LayoutOverlayBake::new(&body, &inputs()).unwrap();
    println!(
        "aux overlay bake construction (fonts): {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let signals = signals();
    let runs: u32 = 20;
    let mut total = std::time::Duration::ZERO;
    let mut compose_total = std::time::Duration::ZERO;
    for i in 0..=runs {
        let pts = MediaTime::from_nanos(i64::from(i) * 40_000_000);
        let started = std::time::Instant::now();
        let picture = canvas.compose(&lookup, pts).unwrap();
        let composed = started.elapsed();
        let started = std::time::Instant::now();
        let out = bake.bake(
            &picture,
            pts,
            canvas.cell_states(),
            canvas.cell_frames(),
            &signals,
            None,
        );
        let elapsed = started.elapsed();
        drop(out);
        if i > 0 {
            // The first bake warms the glyph atlas.
            total += elapsed;
            compose_total += composed;
        }
    }
    assert_eq!(bake.counters().errors, 0);
    println!(
        "aux overlay bake, 1920x1080 NewsOne 1+8, all default layers: {:.2} ms/bake \
         (compose {:.2} ms) over {runs} bakes",
        total.as_secs_f64() * 1000.0 / f64::from(runs),
        compose_total.as_secs_f64() * 1000.0 / f64::from(runs),
    );
}

/// Chaos (deterministic): a source that stops publishing ages through the
/// store's ladder; the chip reads exactly the state the compose saw, and at
/// NO SIGNAL the cell shows its slate while the bake still publishes.
#[test]
fn a_dying_source_reads_no_signal_in_its_chip_while_the_bake_continues() {
    let body = newsone(LayoutOverlays::default(), W, H);
    let (lookup, all) = stores(&[], W, H);
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    canvas.keep_frames(true);
    let mut bake = LayoutOverlayBake::new(&body, &inputs()).unwrap();
    let signals = signals();
    let mut seen = Vec::new();
    // 25 fps; every source but s0 keeps publishing; s0 stopped at t=0.
    for i in 0..=300i64 {
        let pts = MediaTime::from_nanos(i * 40_000_000);
        for store in all.iter().skip(1) {
            store.publish(tile(W / 4, H / 4, 120), pts);
        }
        let picture = canvas.compose(&lookup, pts).expect("one frame per tick");
        let out = bake.bake(
            &picture,
            pts,
            canvas.cell_states(),
            canvas.cell_frames(),
            &signals,
            None,
        );
        assert!(!Arc::ptr_eq(&out, &picture));
        let state = bake.last_dynamics()["s0"].state;
        if seen.last() != Some(&state) {
            seen.push(state);
        }
        assert_eq!(bake.last_dynamics()["s1"].state, SourceState::Live);
    }
    assert_eq!(seen.first(), Some(&SourceState::Live));
    assert_eq!(seen.last(), Some(&SourceState::NoSignal), "{seen:?}");
    assert_eq!(
        bake.counters(),
        AuxOverlayCounters {
            baked: 301,
            errors: 0
        }
    );
    assert!(
        canvas.counters().slate_cells > 0,
        "the dead cell shows its slate"
    );
}

/// A2: whatever the consumer tracks, the published snapshot holds only the
/// sources the baking layouts bind (bounded by layouts × cells).
#[test]
fn the_signals_snapshot_is_bounded_by_the_wanted_sources() {
    let slot = signals_slot();
    let sources = wanted_sources();
    sources.store(Arc::new(
        ["s0", "s1", "s2"].iter().map(|s| (*s).to_owned()).collect(),
    ));
    let publisher = AuxSignalsPublisher::new(
        Arc::clone(&slot),
        Arc::new(AtomicBool::new(true)),
        Arc::clone(&sources),
        Arc::new(AtomicBool::new(true)),
    );
    // Source churn: 1000 distinct ids tracked by the consumer.
    let mut meters = HashMap::new();
    let mut faults = HashMap::new();
    let mut program = HashMap::new();
    for i in 0..1000 {
        let id = format!("churn-{i}");
        meters.insert(id.clone(), MeterChannels::from_db(&[-10.0]));
        faults.insert(id.clone(), TileFault::Black);
        program.insert(id, multiview_core::traits::SourceState::Live);
    }
    meters.insert("s0".to_owned(), MeterChannels::from_db(&[-3.0]));
    faults.insert("s2".to_owned(), TileFault::Frozen);
    program.insert("s1".to_owned(), multiview_core::traits::SourceState::Live);
    // The Program map is only probed by the wanted ids (never iterated), so
    // 1000 unrelated Program sources add no work and no entries.
    publisher.publish_for(&meters, &faults, &program);
    let snapshot = slot.load();
    let wanted = sources.load();
    assert!(snapshot.meters.keys().all(|id| wanted.contains(id)));
    assert!(snapshot.faults.keys().all(|id| wanted.contains(id)));
    assert!(snapshot.program.iter().all(|id| wanted.contains(id)));
    assert!(
        snapshot.meters.len() + snapshot.faults.len() + snapshot.program.len() <= 3 * wanted.len()
    );
    assert_eq!(snapshot.meters.len(), 1);
    assert_eq!(snapshot.faults["s2"], TileFault::Frozen);
    assert!(snapshot.program.contains("s1"));
}

/// M1: a black source that is NOT on Program (the consumer's snapshot has no
/// fault for it) reads BLACK on the routed layout after the 0.5 s dwell —
/// from the layout's own detection over the frames compose read. A source
/// with a healthy picture stays clear; the snapshot still supplies silence.
#[test]
fn an_aux_only_black_source_shows_black_after_the_dwell() {
    let body = newsone(LayoutOverlays::default(), W, H);
    let all: Vec<Arc<TileStore<Nv12Image>>> = (0..9)
        .map(|i| Arc::new(TileStore::<Nv12Image>::with_defaults(format!("s{i}"))))
        .collect();
    let lookup_stores = all.clone();
    let lookup: StoreLookup =
        Arc::new(move |id: &str| lookup_stores.iter().find(|s| s.id() == id).cloned());
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    canvas.keep_frames(true);
    let mut bake = LayoutOverlayBake::new(&body, &inputs()).unwrap();
    // Nothing on Program; s5 is silent per the consumer (it reads PCM).
    let mut remote = AuxOverlaySignals::default();
    remote.faults.insert("s5".to_owned(), TileFault::Silent);
    let mut black_at = None;
    for i in 0..60i64 {
        let pts = MediaTime::from_nanos(i * 40_000_000);
        for (n, store) in all.iter().enumerate() {
            // s0 is black; the rest move (a changing mid-grey picture).
            let luma = if n == 0 {
                16
            } else {
                u8::try_from(100 + (i % 2) * 40).unwrap()
            };
            store.publish(tile(W / 4, H / 4, luma), pts);
        }
        let picture = canvas.compose(&lookup, pts).unwrap();
        let _ = bake.bake(
            &picture,
            pts,
            canvas.cell_states(),
            canvas.cell_frames(),
            &remote,
            None,
        );
        if black_at.is_none() && bake.last_dynamics()["s0"].fault == TileFault::Black {
            black_at = Some(i);
        }
    }
    let at = black_at.expect("s0 reads BLACK on the layout");
    assert!(at >= 12, "not before the 0.5 s dwell (tick {at})");
    assert!(at <= 16, "soon after the dwell (tick {at})");
    let d = bake.last_dynamics();
    assert_eq!(d["s1"].fault, TileFault::None, "a moving picture is clear");
    assert_eq!(
        d["s5"].fault,
        TileFault::Silent,
        "silence from the snapshot"
    );

    // A source NOT on Program shows the layout's own picture verdict, even
    // over a stale snapshot fault (MR2-1: the snapshot speaks only for
    // Program's sources, plus silence).
    let mut stale = AuxOverlaySignals::default();
    stale.faults.insert("s1".to_owned(), TileFault::Frozen);
    let pts = MediaTime::from_nanos(61 * 40_000_000);
    for store in &all {
        store.publish(tile(W / 4, H / 4, 90), pts);
    }
    let picture = canvas.compose(&lookup, pts).unwrap();
    let _ = bake.bake(
        &picture,
        pts,
        canvas.cell_states(),
        canvas.cell_frames(),
        &stale,
        None,
    );
    assert_eq!(bake.last_dynamics()["s1"].fault, TileFault::None);
}

/// Drive `ticks` ticks of `body` with every store publishing a solid `luma`
/// frame (s0 uses `s0_luma`), baking with `signals`; return the bake.
fn drive_solid(
    body: &Arc<RoutedLayout>,
    inputs: &AuxOverlayInputs,
    s0_luma: u8,
    signals: &AuxOverlaySignals,
    ticks: i64,
) -> LayoutOverlayBake {
    let cells = body.layout.cells.len();
    let all: Vec<Arc<TileStore<Nv12Image>>> = (0..cells)
        .map(|i| Arc::new(TileStore::<Nv12Image>::with_defaults(format!("s{i}"))))
        .collect();
    let lookup_stores = all.clone();
    let lookup: StoreLookup =
        Arc::new(move |id: &str| lookup_stores.iter().find(|s| s.id() == id).cloned());
    let mut canvas = LayoutCanvas::new(Arc::clone(body), spec(W, H)).unwrap();
    canvas.keep_frames(true);
    let mut bake = LayoutOverlayBake::new(body, inputs).expect("a bake");
    for i in 0..ticks {
        let pts = MediaTime::from_nanos(i * 40_000_000);
        for (n, store) in all.iter().enumerate() {
            let luma = if n == 0 {
                s0_luma
            } else {
                u8::try_from(100 + (i % 2) * 40).unwrap()
            };
            store.publish(tile(W / 4, H / 4, luma), pts);
        }
        let picture = canvas.compose(&lookup, pts).unwrap();
        let _ = bake.bake(
            &picture,
            pts,
            canvas.cell_states(),
            canvas.cell_frames(),
            signals,
            None,
        );
    }
    bake
}

/// MR2-1 (a): a source on Program shows exactly the consumer's verdict
/// (Program's own detector, with its declared probes and latches),
/// regardless of what the layout's frames say — a configured or latched
/// Program fault is never hidden by a local "clear", and a local fault never
/// contradicts Program's verdict.
#[test]
fn a_program_source_keeps_the_consumers_fault_regardless_of_local_frames() {
    let body = newsone(LayoutOverlays::default(), W, H);
    // s1's picture moves (locally clear); Program says FROZEN (e.g. latched).
    let mut on_program = AuxOverlaySignals::default();
    on_program.program.insert("s1".to_owned());
    on_program.faults.insert("s1".to_owned(), TileFault::Frozen);
    // s0 is black locally, but on Program with no fault: Program's verdict.
    on_program.program.insert("s0".to_owned());
    let bake = drive_solid(&body, &inputs(), 16, &on_program, 60);
    let d = bake.last_dynamics();
    assert_eq!(
        d["s1"].fault,
        TileFault::Frozen,
        "Program's latched fault stands"
    );
    assert_eq!(
        d["s0"].fault,
        TileFault::None,
        "a local BLACK never overrides Program's verdict for a Program source"
    );
    assert_eq!(
        bake.faults.fault("s0"),
        Some(TileFault::Black),
        "the local detector did see black (it is just not authoritative here)"
    );
}

/// MR2-1 (b): an aux-only source honours its operator-declared black probe
/// (threshold and dwell) exactly as Program would, not the defaults: a Y=80
/// picture is black under a declared threshold of 100 (never under the
/// default 16) and raises after the declared 80 ms dwell (~2 ticks), far
/// sooner than the default 0.5 s.
#[test]
fn an_aux_only_source_follows_its_declared_black_probe() {
    use multiview_config::probe::{DetectionZone, Dwell, Probe, ProbeKind};
    use multiview_core::alarm::PerceivedSeverity;
    let body = newsone(LayoutOverlays::default(), W, H);
    let probe = Probe::new(
        "s0-black",
        "s0",
        ProbeKind::black(100, DetectionZone::default()),
        Dwell::new(80, 80),
        PerceivedSeverity::Critical,
        false,
    );
    let mut declared = inputs();
    declared.source_probes = Arc::new(crate::fault_probes::resolve_source_probes(
        &body.layout,
        &[probe],
    ));
    let none = AuxOverlaySignals::default();
    // Declared: black within ~2-3 ticks.
    let bake = drive_solid(&body, &declared, 80, &none, 4);
    assert_eq!(
        bake.last_dynamics()["s0"].fault,
        TileFault::Black,
        "Y=80 is black under the declared threshold 100, raised after the 80 ms dwell"
    );
    // Default: Y=80 is never black (threshold 16), even after a long run.
    let bake = drive_solid(&body, &inputs(), 80, &none, 30);
    assert_eq!(bake.last_dynamics()["s0"].fault, TileFault::None);
    // Default dwell: a real black (Y=16) is NOT yet raised after 4 ticks.
    let bake = drive_solid(&body, &inputs(), 16, &none, 4);
    assert_eq!(bake.last_dynamics()["s0"].fault, TileFault::None);
}

/// A layout of `n` full-canvas cells, each bound to its own source `s<i>`.
fn many_cells(n: usize) -> Arc<RoutedLayout> {
    let mut body = Arc::unwrap_or_clone(newsone(LayoutOverlays::default(), W, H));
    body.layout.cells = (0..n)
        .map(|i| Cell {
            x: 0.0,
            y: 0.0,
            w: 0.1,
            h: 0.1,
            z: i32::try_from(i).unwrap(),
            source: Some(format!("s{i}")),
            ..Cell::default()
        })
        .collect();
    body.slates = vec![multiview_config::FailoverSlate::Black; n];
    Arc::new(body)
}

/// AR2-1: the overlay bake has a cell budget. A 65-cell layout still
/// composes (one frame per tick) but gets no overlay bake (counted); a
/// 64-cell layout bakes.
#[test]
fn a_layout_over_the_cell_budget_composes_but_does_not_bake() {
    assert_eq!(MAX_BAKED_CELLS, 64);
    let over = many_cells(MAX_BAKED_CELLS + 1);
    let before = skipped_bakes().too_many_cells.load(Ordering::Relaxed);
    assert!(
        LayoutOverlayBake::new(&over, &inputs()).is_none(),
        "65 cells: no bake"
    );
    assert!(
        skipped_bakes().too_many_cells.load(Ordering::Relaxed) > before,
        "counted"
    );
    let (lookup, _keep) = stores(&[], W, H);
    let mut canvas = LayoutCanvas::new(Arc::clone(&over), spec(W, H)).unwrap();
    assert!(
        canvas.compose(&lookup, MediaTime::ZERO).is_some(),
        "65 cells still compose"
    );

    let at = many_cells(MAX_BAKED_CELLS);
    let mut bake = LayoutOverlayBake::new(&at, &inputs()).expect("64 cells: a bake");
    // An edit growing it past the budget drops the bake.
    assert!(!bake.rebuild(&over, &labels()));
    const { assert!(MAX_WANTED_SOURCES == 8 * 64) };
}

/// AR2-1: a source bound by two cells is observed once per tick (the
/// per-body first-index dedupe, no per-tick scan) and both cells show the
/// same verdict.
#[test]
fn a_source_bound_twice_is_observed_once() {
    let mut body = Arc::unwrap_or_clone(newsone(LayoutOverlays::default(), W, H));
    // Cell 8 rebinds s0 (cells 0 and 8 both show s0).
    body.layout.cells[8].source = Some("s0".to_owned());
    let body = Arc::new(body);
    let index = BodyIndex::of(&body);
    assert_eq!(index.first.len(), 8, "9 cells, 8 distinct sources");
    assert_eq!(index.state_of.len(), 8);
    // Black for 15 ticks (560 ms, past the 0.5 s dwell): the verdict is
    // right; the freeze window proves one observation per tick (observing it
    // twice per tick would fill the window twice as fast).
    let bake = drive_solid(&body, &inputs(), 16, &AuxOverlaySignals::default(), 15);
    assert_eq!(bake.last_dynamics()["s0"].fault, TileFault::Black);
    let window = bake.faults.per_source["s0"].window.len();
    assert_eq!(window, 12, "one observation per tick (window capped at 12)");
    let bake = drive_solid(&body, &inputs(), 16, &AuxOverlaySignals::default(), 6);
    assert_eq!(
        bake.faults.per_source["s0"].window.len(),
        6,
        "6 ticks: 6 observations, not 12"
    );
}
