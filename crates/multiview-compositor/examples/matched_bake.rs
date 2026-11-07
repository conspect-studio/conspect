//! Identical local-only release-bake comparison harness; NOT full-canvas oracle.
#![allow(
    clippy::similar_names,
    clippy::as_conversions,
    clippy::unnecessary_cast,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::print_stdout
)] // Local-only benchmark harness uses bounded fixture indices and intentional output.
use multiview_compositor::overlay::color_memo::prewarm_overlay_decode_table;
use multiview_compositor::overlay::subpass::{
    apply_overlays_to_nv12, OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use std::time::Instant;

const W: usize = 1920;
const H: usize = 1080;
const GREEN: OverlayColor = OverlayColor::opaque(0., 1., 0.);
fn push_rect(list: &mut OverlayDrawList, x: i32, y: i32, w: u32, h: u32) {
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(x, y, w, h),
        corner_radius: 0,
        color: GREEN,
    });
}
fn fixture(n: usize) -> OverlayDrawList {
    let mut list = OverlayDrawList::new();
    // Six NewsOne-style 640x540 cells: true CLI track width 18px,
    // chip 40px, pad 8px, track height 444px; two 8px stereo lanes.
    for row in 0..2 {
        for col in 0..3 {
            let x = (col * 640) as i32;
            let y = (row * 540) as i32;
            push_rect(&mut list, x + 8, y + 8, 110, 32);
            list.push(OverlayPrimitive::Glyph {
                dest_x: x + 12,
                dest_y: y + 13,
                width: 60,
                height: 20,
                coverage: vec![(120 + n % 136) as u8; 1200],
                color: GREEN,
            });
            for lane in 0..2 {
                let lx = x + 614 + (lane * 9) as i32;
                push_rect(&mut list, lx, y + 48, 8, 444);
                for (offset, height) in [(240, 204), (370, 74), (445, 13)] {
                    push_rect(&mut list, lx, y + 48 + offset, 7, height);
                }
            }
        }
    }
    // Caption bitmap crossing the lower-left tile boundary, clipped at x=0.
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(-16, 980, 700, 74),
        src_width: 2,
        src_height: 2,
        rgba: vec![
            80 + n as u8,
            60,
            30,
            180,
            30,
            40,
            50,
            210,
            0,
            10,
            30,
            160,
            90,
            20,
            10,
            220,
        ],
        alpha: 0.8,
    });
    list
}
fn image(n: usize) -> Nv12Image {
    let mut y = vec![0u8; W * H];
    for (i, v) in y.iter_mut().enumerate() {
        *v = (16 + (i / 64 + n * 13) % 220) as u8;
    }
    let uv = vec![128u8; W * H / 2];
    Nv12Image::new(
        W as u32,
        H as u32,
        y,
        uv,
        CanvasColor::default().output_tag(),
    )
    .unwrap()
}
fn report(name: &str, images: &[Nv12Image], lists: &[OverlayDrawList], repeats: usize) {
    let canvas = CanvasColor::default();
    for i in 0..8 {
        std::hint::black_box(
            apply_overlays_to_nv12(&images[i % images.len()], &lists[i % lists.len()], canvas)
                .unwrap(),
        );
    }
    let mut samples = Vec::with_capacity(repeats);
    for i in 0..repeats {
        let start = Instant::now();
        std::hint::black_box(
            apply_overlays_to_nv12(&images[i % images.len()], &lists[i % lists.len()], canvas)
                .unwrap(),
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name} reps={repeats} median_ms={:.4} p95_ms={:.4} mean_ms={:.4}",
        samples[repeats / 2],
        samples[(repeats * 95 / 100).min(repeats - 1)],
        samples.iter().sum::<f64>() / repeats as f64
    );
}
fn report_all_fresh() {
    let canvas = CanvasColor::default();
    for i in 0..8 {
        let img = image(i);
        let list = fixture(i);
        std::hint::black_box(apply_overlays_to_nv12(&img, &list, canvas).unwrap());
    }
    let mut samples = Vec::with_capacity(32);
    for i in 0..32 {
        let start = Instant::now();
        let img = image(i + 11);
        let list = fixture(i + 11);
        std::hint::black_box(apply_overlays_to_nv12(&img, &list, canvas).unwrap());
        samples.push(start.elapsed().as_secs_f64() * 1000.);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "all_fresh_frame_caption_and_bake reps=32 median_ms={:.4} p95_ms={:.4} mean_ms={:.4}",
        samples[16],
        samples[30],
        samples.iter().sum::<f64>() / 32.
    );
}
fn main() {
    // Production builds the exact decode table in the background on the first
    // bake; prewarm it here (outside timing) so samples are steady-state.
    let start = Instant::now();
    let ready = prewarm_overlay_decode_table(CanvasColor::default());
    println!(
        "decode_table_prewarm ready={ready} ms={:.1}",
        start.elapsed().as_secs_f64() * 1000.
    );
    let image0 = image(0);
    let list0 = fixture(0);
    report("static_reused_input_and_caption", &[image0], &[list0], 32);
    let frames: Vec<_> = (0..32).map(image).collect();
    let lists: Vec<_> = (0..32).map(fixture).collect();
    report("distinct_frame_and_caption_every_bake", &frames, &lists, 32);
    report_all_fresh();
    println!("geometry 1920x1080; six 640x540 tiles, stereo lanes 8x444 each, caption clipped 684x74; source/list preparation excluded from bake timing");
}
