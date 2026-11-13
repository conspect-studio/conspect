//! Routed layout compose (ADR-0108 slice 3): identity with Program's
//! compositor, dead cells, alpha / foreign-tag cells, the buffer ring, and the
//! cost of a 1080p compose.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use multiview_compositor::blend::LinearRgba;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_engine::{CompositorDrive, Tick};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::time::Instant;

const W: u32 = 640;
const H: u32 = 360;

fn color() -> CanvasColor {
    CanvasColor::default()
}

fn background() -> LinearRgba {
    LinearRgba::opaque(0.02, 0.02, 0.05)
}

fn spec(w: u32, h: u32) -> LayoutCanvasSpec {
    LayoutCanvasSpec {
        width: w,
        height: h,
        color: color(),
        background: background_codes(color(), background()).unwrap(),
    }
}

fn gradient(w: u32, h: u32, seed: usize) -> Nv12Image {
    let (wu, hu) = (usize::try_from(w).unwrap(), usize::try_from(h).unwrap());
    // Mid-range samples well inside limited range (the reference round trip
    // is exact to one code value there).
    let y = (0..wu * hu)
        .map(|i| u8::try_from(40 + (i * 7 + seed * 13) % 160).unwrap())
        .collect();
    let uv = (0..wu * hu / 2)
        .map(|i| u8::try_from(80 + (i * 3 + seed * 5) % 96).unwrap())
        .collect();
    Nv12Image::new(w, h, y, uv, color().output_tag()).unwrap()
}

#[allow(clippy::many_single_char_names)] // Geometry reads best as x, y, w, h, z.
fn cell(x: f32, y: f32, w: f32, h: f32, z: i32, source: Option<&str>) -> Cell {
    Cell {
        x,
        y,
        w,
        h,
        z,
        fit: FitMode::Fill,
        source: source.map(str::to_owned),
        opacity: 1.0,
    }
}

/// A NewsOne-like layout: a big cell, three small ones (one overlapping the
/// big cell at a higher z), an unbound cell, and a cell bound to a dead source.
fn newsone(w: u32, h: u32) -> Layout {
    Layout {
        name: "NewsOne".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 30000,
            fps_den: 1001,
        },
        cells: vec![
            cell(0.0, 0.0, 0.7, 0.7, 0, Some("a")),
            cell(0.7, 0.0, 0.3, 0.333, 0, Some("b")),
            cell(0.7, 0.333, 0.3, 0.333, 0, Some("c")),
            cell(0.55, 0.55, 0.3, 0.3, 5, Some("b")),
            cell(0.0, 0.7, 0.35, 0.3, 0, Some("dead")),
            cell(0.35, 0.7, 0.2, 0.3, 0, None),
        ],
    }
}

struct Stores {
    map: HashMap<String, Arc<TileStore<Nv12Image>>>,
}

impl Stores {
    fn new(pts: MediaTime) -> Self {
        let mut map = HashMap::new();
        // Source tiles of several sizes: down- and up-scaled into cells.
        for (id, (w, h), seed) in [
            ("a", (W, H), 1),
            ("b", (320, 180), 2),
            ("c", (1280, 720), 3),
        ] {
            let store = Arc::new(TileStore::<Nv12Image>::with_defaults(id));
            store.publish(gradient(w, h, seed), pts);
            map.insert(id.to_owned(), store);
        }
        map.insert(
            "dead".to_owned(),
            Arc::new(TileStore::<Nv12Image>::with_defaults("dead")),
        );
        Self { map }
    }

    fn lookup(&self) -> StoreLookup {
        let map = self.map.clone();
        Arc::new(move |id: &str| map.get(id).cloned())
    }
}

fn routed(layout: Layout) -> Arc<RoutedLayout> {
    let slates = vec![FailoverSlate::Black; layout.cells.len()];
    Arc::new(RoutedLayout {
        id: layout.name.clone(),
        layout,
        slates,
    })
}

fn program_frame(
    layout: &Layout,
    stores: &Stores,
    slates: Vec<FailoverSlate>,
    pts: MediaTime,
) -> Nv12Image {
    let nosignal = Nv12Image::solid(W, H, 16, 128, 128, color().output_tag()).unwrap();
    let drive = CompositorDrive::new(
        Arc::new(layout.clone()),
        stores.map.clone(),
        nosignal,
        color(),
        background(),
    )
    .unwrap()
    .with_cell_slates(slates);
    Nv12Image::clone(&drive.compose(Tick { index: 7, pts }).unwrap().canvas)
}

/// Max per-sample absolute difference of two planes.
fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

#[test]
fn a_routed_layout_equals_the_same_layout_on_program() {
    // Deterministic compose identity (#306): the routed canvas and Program's
    // CPU compositor give the same picture: identical geometry (cell rects,
    // nearest-neighbour source mapping, z order, chroma siting, slates,
    // background) and samples equal to within the reference's one-code-value
    // linear-light round trip.
    let pts = MediaTime::from_nanos(1_000_000_000);
    let stores = Stores::new(pts);
    let layout = newsone(W, H);
    let body = routed(layout.clone());
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    let ours = canvas.compose(&stores.lookup(), pts).unwrap();
    let theirs = program_frame(&layout, &stores, body.slates.clone(), pts);
    assert_eq!((ours.width(), ours.height()), (W, H));
    assert_eq!(ours.color(), theirs.color(), "tagged like Program");
    assert!(
        max_diff(ours.y_plane(), theirs.y_plane()) <= 1,
        "luma within one code value"
    );
    assert!(
        max_diff(ours.uv_plane(), theirs.uv_plane()) <= 1,
        "chroma within one code value"
    );
    // The comparison is not vacuous: the picture has real structure and a
    // geometry error of one pixel shows up as a large difference.
    let distinct: std::collections::HashSet<u8> = ours.y_plane().iter().copied().collect();
    assert!(distinct.len() > 50, "a real picture");
    let mut shifted = newsone(W, H);
    shifted.cells[0].w = 0.69;
    let other = program_frame(&shifted, &stores, body.slates.clone(), pts);
    assert!(
        max_diff(ours.y_plane(), other.y_plane()) > 1,
        "geometry is checked"
    );
    // Two composes of the same tick are byte-identical (determinism).
    let again = canvas.compose(&stores.lookup(), pts).unwrap();
    assert!(again.y_plane() == ours.y_plane() && again.uv_plane() == ours.uv_plane());
    assert_eq!(canvas.counters().slate_cells, 4, "dead + unbound, twice");
}

#[test]
fn odd_cell_edges_match_program_chroma_siting() {
    // Cells whose edges fall on odd pixels: the 2x2 chroma block is written
    // by whichever cell covers its bottom-right pixel, as on Program.
    let pts = MediaTime::from_nanos(0);
    let stores = Stores::new(pts);
    let mut layout = newsone(W, H);
    layout.cells = vec![
        cell(0.0, 0.0, 0.5016, 0.5042, 0, Some("a")),
        cell(0.5016, 0.0, 0.4984, 0.5042, 0, Some("b")),
        cell(0.0, 0.5042, 1.0, 0.4958, 0, Some("c")),
    ];
    let body = routed(layout.clone());
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    let ours = canvas.compose(&stores.lookup(), pts).unwrap();
    let (x, _, _, h) = cell_rect(&layout.cells[1], W, H);
    assert!(
        x % 2 == 1 && h % 2 == 1,
        "the test really has odd edges: {x} {h}"
    );
    let theirs = program_frame(&layout, &stores, body.slates.clone(), pts);
    assert!(max_diff(ours.y_plane(), theirs.y_plane()) <= 1);
    assert!(max_diff(ours.uv_plane(), theirs.uv_plane()) <= 1);
}

#[test]
fn a_dead_or_undrawable_cell_shows_its_slate_and_never_blocks() {
    let pts = MediaTime::from_nanos(0);
    let stores = Stores::new(pts);
    // `b` turns into a tile with another colour tag; `c` into one with alpha.
    let mut other = color().output_tag();
    other.range = multiview_core::color::ColorRange::Full;
    assert_ne!(other, color().output_tag());
    let g = gradient(320, 180, 2);
    stores.map["b"].publish(
        Nv12Image::new(320, 180, g.y_plane().to_vec(), g.uv_plane().to_vec(), other).unwrap(),
        pts,
    );
    let mut layout = newsone(W, H);
    layout.cells.truncate(2);
    let mut slates = vec![FailoverSlate::Black; 2];
    slates[1] = FailoverSlate::Bars;
    let body = Arc::new(RoutedLayout {
        id: "x".to_owned(),
        layout: layout.clone(),
        slates: slates.clone(),
    });
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    let ours = canvas.compose(&stores.lookup(), pts).unwrap();
    assert_eq!(canvas.counters().undrawable_cells, 1);
    // The foreign-tag cell shows its bars slate, as Program would show it
    // for a down cell: equal to Program composing the cell unbound.
    let mut unbound = layout.clone();
    unbound.cells[1].source = None;
    let theirs = program_frame(&unbound, &stores, slates, pts);
    assert!(max_diff(ours.y_plane(), theirs.y_plane()) <= 1);
    assert_eq!(ours.color(), color().output_tag(), "never mislabelled");
}

#[test]
fn the_ring_holds_when_every_buffer_is_in_use_and_reuses_a_freed_one() {
    let pts = MediaTime::from_nanos(0);
    let stores = Stores::new(pts);
    let mut canvas = LayoutCanvas::new(routed(newsone(W, H)), spec(W, H)).unwrap();
    let lookup = stores.lookup();
    let mut held: Vec<_> = (0..LAYOUT_RING_BUFFERS)
        .map(|_| canvas.compose(&lookup, pts).unwrap())
        .collect();
    assert!(
        canvas.compose(&lookup, pts).is_none(),
        "hold, never allocate"
    );
    assert_eq!(canvas.counters().held_no_buffer, 1);
    let first = Arc::as_ptr(&held.remove(0));
    assert_eq!(Arc::as_ptr(&canvas.compose(&lookup, pts).unwrap()), first);
}

#[test]
fn a_layout_for_another_canvas_is_never_composed() {
    assert!(LayoutCanvas::new(routed(newsone(1280, 720)), spec(W, H)).is_none());
}

#[test]
fn integer_drawable_refuses_translucent_cells() {
    let mut layout = newsone(W, H);
    assert!(routed(layout.clone()).integer_drawable());
    layout.cells[2].opacity = 0.5;
    assert!(!routed(layout).integer_drawable());
}

/// The cost of a 6-cell 1080p routed layout on one core, measured here
/// (debug builds are ~10x slower; the bound is loose on purpose and the
/// number is printed for the PR). The design budget is half a tick.
#[test]
fn a_1080p_layout_compose_costs_less_than_a_tick_on_one_core() {
    let pts = MediaTime::from_nanos(0);
    let stores = Stores::new(pts);
    let mut canvas = LayoutCanvas::new(routed(newsone(1920, 1080)), spec(1920, 1080)).unwrap();
    let lookup = stores.lookup();
    let mut samples = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let frame = canvas.compose(&lookup, pts);
        samples.push(started.elapsed());
        drop(frame);
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    eprintln!(
        "routed 1080p 6-cell layout compose: p50 {p50:?}, max {:?}",
        samples.last()
    );
    let bound = if cfg!(debug_assertions) { 400 } else { 33 };
    assert!(
        p50 < std::time::Duration::from_millis(bound),
        "p50 {p50:?} over {bound} ms"
    );
}

/// #368 review (lens 3): a canvas wider or taller than 65534 composes one
/// frame and returns: no `u32` wrap, no endless block walk.
#[test]
fn a_huge_canvas_composes_and_returns() {
    for (w, h) in [(65_536, 2), (2, 65_536), (65_538, 4)] {
        let pts = MediaTime::from_nanos(0);
        let stores = Stores::new(pts);
        let mut layout = newsone(w, h);
        layout.cells.truncate(3);
        let mut canvas = LayoutCanvas::new(routed(layout), spec(w, h)).unwrap();
        let started = Instant::now();
        let frame = canvas.compose(&stores.lookup(), pts).expect("one frame");
        assert_eq!((frame.width(), frame.height()), (w, h));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}

/// #368 review (lens 1): a cell whose floored rect is zero wide is drawn at
/// the source's own size (clipped), as the reference compositor does; a
/// one-row cell on an even row leaves the block chroma to the pixel below,
/// as the reference's last writer does.
#[test]
fn sub_pixel_and_one_row_cells_match_program() {
    let pts = MediaTime::from_nanos(0);
    let stores = Stores::new(pts);
    let mut layout = newsone(W, H);
    layout.cells = vec![
        cell(0.2, 0.1, 0.5 / 640.0, 0.3, 0, Some("b")),
        cell(0.0, 0.25, 1.0, 0.0028, 1, Some("c")),
        cell(0.6, 0.6, 0.3, 0.3, 0, Some("a")),
    ];
    let (_, _, w0, _) = cell_rect(&layout.cells[0], W, H);
    assert_eq!(w0, 0, "the first cell really floors to zero width");
    let (_, y1, _, h1) = cell_rect(&layout.cells[1], W, H);
    assert_eq!((y1 % 2, h1), (0, 1), "a one-row cell on an even row");
    let body = routed(layout.clone());
    let mut canvas = LayoutCanvas::new(Arc::clone(&body), spec(W, H)).unwrap();
    let ours = canvas.compose(&stores.lookup(), pts).unwrap();
    let theirs = program_frame(&layout, &stores, body.slates.clone(), pts);
    assert!(max_diff(ours.y_plane(), theirs.y_plane()) <= 1);
    assert!(max_diff(ours.uv_plane(), theirs.uv_plane()) <= 1);
}
