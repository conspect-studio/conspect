//! Keyer draws and the off-clock scene epoch (issue #415 review MC-1 ×
//! ADR-0113 K2a): a keyed frame runs on the GPU backend's off-clock CPU
//! fallback (the K1 CPU key path), which may hand a late result to a later
//! frame of the SAME scene. A change of what the keyers draw is a scene
//! change: a late result composed under other keyer draws is never adopted,
//! while a new fill frame alone (source content) still is.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::key::KeyKind;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_compositor::readback::SimulatedGpuControl;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::keyer::{KeyerDraw, KeyerFly, KeyerParams, MAX_DSK};
use multiview_engine::{CompositedFrame, CompositorDrive, OutputClock, Tick};

const SIDE: u32 = 16;
/// The CPU fallback's cost for the first keyed job: far past the clock's
/// wait (3/4 of a 30 fps period), so that job is still running, then
/// finished, while later frames are composed.
const SLOW: Duration = Duration::from_millis(300);

fn image(y: u8) -> Nv12Image {
    Nv12Image::solid(
        SIDE,
        SIDE,
        y,
        128,
        128,
        ColorInfo::default().resolve_defaults(1920, 1080),
    )
    .unwrap()
}

fn drive(backend: RunBackend) -> CompositorDrive<Nv12Image> {
    CompositorDrive::new(
        Arc::new(Layout {
            name: "keyer-epoch".to_owned(),
            canvas: Canvas {
                width: SIDE,
                height: SIDE,
                fps_num: Rational::FPS_30.num,
                fps_den: Rational::FPS_30.den,
            },
            cells: Vec::new(),
        }),
        HashMap::new(),
        image(16),
        CanvasColor::default(),
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .unwrap()
    .with_backend(backend)
}

fn tick(index: u64) -> Tick {
    let clock = OutputClock::new(Rational::FPS_30).unwrap();
    Tick {
        index,
        pts: clock.pts_at(index),
    }
}

/// One full-canvas self-keyed (linear) keyer of `fill` at `weight`.
fn draws(fill: &Arc<Nv12Image>, weight: f32) -> [Option<KeyerDraw>; MAX_DSK] {
    let mut out = [const { None }; MAX_DSK];
    out[0] = Some(KeyerDraw {
        keyer: 0,
        fill: Arc::clone(fill),
        key: None,
        rect: KeyerFly {
            x: 0,
            y: 0,
            w: SIDE,
            h: SIDE,
        },
        weight,
        params: KeyerParams {
            kind: KeyKind::Linear,
            invert: false,
            mask: None,
        },
    });
    out
}

fn luma(frame: &CompositedFrame) -> u8 {
    frame.canvas.sample(8, 8).unwrap().0
}

/// The reference picture of `draws` on the CPU backend.
fn reference(draws: &mut [Option<KeyerDraw>; MAX_DSK]) -> u8 {
    let mut cpu = drive(RunBackend::cpu());
    cpu.set_keyer_draws(draws);
    luma(&cpu.compose(tick(0)).unwrap())
}

/// Start a slow keyed job under `first` that misses the clock's wait, then
/// return the drive and its simulated GPU control.
fn with_a_late_keyed_job(
    first: &mut [Option<KeyerDraw>; MAX_DSK],
) -> (CompositorDrive<Nv12Image>, SimulatedGpuControl) {
    let (backend, gpu) = RunBackend::simulated_gpu(Duration::ZERO).unwrap();
    gpu.set_cpu_fallback_cost(SLOW);
    let mut drive = drive(backend);
    drive.set_keyer_draws(first);
    let missed = drive.compose(tick(0));
    assert!(
        missed.is_err(),
        "the slow keyed job misses the clock's wait (a counted repeat)"
    );
    (drive, gpu)
}

/// Compose until a frame is delivered (bounded); every delivered frame is
/// returned for checking.
fn first_delivered(drive: &CompositorDrive<Nv12Image>, from: u64) -> CompositedFrame {
    for i in 0..400 {
        if let Ok(frame) = drive.compose(tick(from + i)) {
            return frame;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no keyed frame delivered within the bound");
}

#[test]
fn a_late_keyed_result_under_other_keyer_draws_is_never_adopted() {
    let fill = Arc::new(image(235));
    let on_air = reference(&mut draws(&fill, 1.0));
    let half = reference(&mut draws(&fill, 0.5));
    assert_ne!(on_air, half, "the two keyer states draw different pictures");

    let (mut drive, gpu) = with_a_late_keyed_job(&mut draws(&fill, 1.0));
    // The keyer's weight changes (a keyer AUTO step): a new scene.
    drive.set_keyer_draws(&mut draws(&fill, 0.5));
    // While the weight-1 job still runs, a frame of the new state is not
    // handed its result.
    if let Ok(frame) = drive.compose(tick(1)) {
        assert_eq!(luma(&frame), half, "never the weight-1 picture");
    }
    gpu.set_cpu_fallback_cost(Duration::ZERO);
    // Let the weight-1 job finish: its result is now stale, never adopted.
    std::thread::sleep(SLOW * 2);
    let frame = first_delivered(&drive, 2);
    assert_eq!(
        luma(&frame),
        half,
        "the delivered frame is composed under the current keyer draws"
    );
    assert!(!frame.repeat);
}

#[test]
fn a_late_keyed_result_with_the_same_keyer_draws_is_adopted() {
    // Control for the test above: a new fill frame alone is source content,
    // so the same keyer state still adopts the late result (the fallback
    // keeps delivering frames while it is slower than the clock).
    let old_fill = Arc::new(image(235));
    let new_fill = Arc::new(image(126));
    let old_picture = reference(&mut draws(&old_fill, 1.0));
    let new_picture = reference(&mut draws(&new_fill, 1.0));
    assert_ne!(old_picture, new_picture);

    let (mut drive, _gpu) = with_a_late_keyed_job(&mut draws(&old_fill, 1.0));
    drive.set_keyer_draws(&mut draws(&new_fill, 1.0));
    std::thread::sleep(SLOW * 2);
    let frame = drive
        .compose(tick(1))
        .expect("the finished result is adopted");
    assert_eq!(
        luma(&frame),
        old_picture,
        "the late result of the same keyer scene is adopted"
    );
}
