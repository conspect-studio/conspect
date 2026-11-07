//! Release-mode cost of one synthetic clock/timer render at tile size, and of
//! the generator's steady-state per-tick work (republish vs render).
//!
//! Run: `cargo run --release -p multiview-cli --features overlay --example synth_render_cost`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::items_after_statements
)]

use std::time::Instant;

use multiview_cli::synth::{generator_loop, render_at, SynthRenderer, SyntheticKind};
use multiview_compositor::pipeline::CanvasColor;
use multiview_config::MultiviewConfig;
use multiview_core::time::Rational;
use multiview_framestore::TileStore;
use multiview_overlay::clock::WallTime;

fn kind_of(fields: &str) -> SyntheticKind {
    let doc = format!(
        r##"schema_version = 1
[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
{fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let src = cfg.sources.into_iter().next().expect("source");
    SyntheticKind::from_source_kind(&src.kind).expect("synthetic kind")
}

/// Process user+system CPU seconds (all threads) from `/proc/self/stat`.
fn cpu_secs() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    ticks / 100.0
}

fn main() {
    let cad = Rational {
        num: 30000,
        den: 1001,
    };
    let canvas = CanvasColor::default();
    let cases = [
        ("clock dual+label+offset", kind_of("kind = \"clock\"\nface = \"dual\"\ntimezone = \"Australia/Sydney\"\nlabel = \"Sydney\"\nshow_offset = true\nnumerals = true")),
        ("clock digital", kind_of("kind = \"clock\"\nface = \"digital\"")),
        ("timer hh:mm:ss", kind_of("kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"12:00:00\"\ntimezone = \"UTC\"\nformat = \"hh_mm_ss\"\nlabel = \"ON AIR IN\"")),
    ];
    let n = 200_i64;
    const NF: f64 = 200.0;
    for ((w, h), (name, kind)) in [(640_u32, 360_u32), (1280, 720)]
        .into_iter()
        .flat_map(|size| cases.iter().map(move |c| (size, c)))
    {
        // Fresh-engine path (one `render_at` per displayed second).
        let (t0, c0) = (Instant::now(), cpu_secs());
        for s in 0..n {
            let _ = render_at(
                kind,
                w,
                h,
                canvas,
                WallTime::from_unix_seconds(1_700_000_000 + s),
                0,
                cad,
            )
            .unwrap();
        }
        let fresh = t0.elapsed().as_secs_f64() * 1e3 / NF;
        let fresh_cpu = (cpu_secs() - c0) * 1e3 / NF;
        // Reused-renderer path (the generator's per-thread state).
        let mut r = SynthRenderer::new();
        let _ = r
            .render_at(
                kind,
                w,
                h,
                canvas,
                WallTime::from_unix_seconds(1_699_999_999),
                0,
                cad,
            )
            .unwrap();
        let (t0, c0) = (Instant::now(), cpu_secs());
        for s in 0..n {
            let _ = r
                .render_at(
                    kind,
                    w,
                    h,
                    canvas,
                    WallTime::from_unix_seconds(1_700_000_000 + s),
                    0,
                    cad,
                )
                .unwrap();
        }
        let reused = t0.elapsed().as_secs_f64() * 1e3 / NF;
        let reused_cpu = (cpu_secs() - c0) * 1e3 / NF;
        println!(
            "{w}x{h} {name:24} fresh: {fresh:6.3} ms wall {fresh_cpu:6.2} ms cpu | \
             reused: {reused:6.3} ms wall {reused_cpu:6.2} ms cpu  (per render)"
        );
    }

    // Steady-state generator cost: the real loop at 29.97 fps for 5 s per case.
    for (name, kind) in &cases {
        let store = TileStore::with_defaults("bench");
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (t0, c0) = (Instant::now(), cpu_secs());
        std::thread::scope(|scope| {
            scope.spawn(|| generator_loop(kind.clone(), &store, 640, 360, canvas, cad, &stop));
            std::thread::sleep(std::time::Duration::from_secs(5));
            stop.store(true, std::sync::atomic::Ordering::Release);
        });
        let secs = t0.elapsed().as_secs_f64();
        let cpu = (cpu_secs() - c0) / secs * 100.0;
        println!("generator 640x360 {name:24} {cpu:6.2} % of one core");
    }
}
