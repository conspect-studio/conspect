#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_bus::DualBusDrive;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_runtime::SwitcherRuntime;
use multiview_engine::CompositorDrive;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;
fn scene(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}
fn drive(
    id: &str,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
) -> CompositorDrive<Nv12Image> {
    let card = Nv12Image::solid(
        64,
        64,
        16,
        128,
        128,
        ColorInfo::default().resolve_defaults(64, 64),
    )
    .unwrap();
    CompositorDrive::new(
        scene(id),
        stores,
        card,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}
fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * 40_000_000),
    }
}
#[test]
fn real_paired_frames_gate_202_and_cut_waits_for_prime_then_persists() {
    let thresholds = TileThresholds::from_millis(500, 2000, 10000).unwrap();
    let a = Arc::new(TileStore::new("a", thresholds, NoSignalPolicy::Slate));
    let b = Arc::new(TileStore::new("b", thresholds, NoSignalPolicy::Slate));
    let color = ColorInfo::default().resolve_defaults(64, 64);
    a.publish(
        Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(),
        MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), Arc::clone(&a)), ("b".into(), Arc::clone(&b))]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    driver.set_prime_stores(stores.clone());
    let bus = DualBusDrive::new(
        drive("a", stores.clone()),
        drive("b", stores),
        RunBackend::cpu(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();
    let mut runtime = SwitcherRuntime::new(driver, bus, 1);
    assert!(
        !handle.is_ready(),
        "configuration alone never claims paired frame capability"
    );
    let first = runtime.tick(tick(0)).unwrap();
    assert_eq!(first.program.bus_id, "main");
    assert_eq!(first.preview.bus_id, "preview");
    assert_eq!(handle.provider_status().unwrap().last_tick, 0);
    let program_y = first.program.canvas.sample(8, 8).unwrap().0;
    let mut outcome = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "cut1".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
        })
        .unwrap();
    for n in 1..6 {
        let frames = runtime.tick(tick(n)).unwrap();
        assert_eq!(frames.program.canvas.sample(8, 8).unwrap().0, program_y);
        assert_eq!(frames.program.tick, frames.preview.tick);
        assert!(outcome.try_recv().is_err());
    }
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        MediaTime::ZERO,
    );
    let taken = runtime.tick(tick(6)).unwrap();
    let pvw_y = taken.program.canvas.sample(8, 8).unwrap().0;
    assert_ne!(pvw_y, program_y);
    assert_eq!(taken.preview.canvas.sample(8, 8).unwrap().0, program_y);
    assert_eq!(outcome.try_recv().unwrap().event.tick, 6);
    for n in 7..30 {
        let frame = runtime.tick(tick(n)).unwrap();
        assert_eq!(frame.program.canvas.sample(8, 8).unwrap().0, pvw_y);
        assert_eq!(frame.preview.canvas.sample(8, 8).unwrap().0, program_y);
    }
}
