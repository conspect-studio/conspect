#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_bus::DualBusDrive;
use multiview_engine::CompositorDrive;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;

fn drive(
    id: &str,
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
) -> CompositorDrive<Nv12Image> {
    let layout = Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            source: Some(id.into()),
            ..Cell::default()
        }],
    };
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
        Arc::new(layout),
        stores,
        card,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[test]
fn two_buses_share_source_stores_and_tick_without_pgm_fallback() {
    let thresholds = TileThresholds::from_millis(500, 2000, 10000).unwrap();
    let a = Arc::new(TileStore::<Nv12Image>::new(
        "a",
        thresholds,
        NoSignalPolicy::Slate,
    ));
    let b = Arc::new(TileStore::<Nv12Image>::new(
        "b",
        thresholds,
        NoSignalPolicy::Slate,
    ));
    let color = ColorInfo::default().resolve_defaults(64, 64);
    a.publish(
        Nv12Image::solid(64, 64, 190, 128, 128, color).unwrap(),
        MediaTime::ZERO,
    );
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), Arc::clone(&a)), ("b".into(), Arc::clone(&b))]);
    let bus = DualBusDrive::new(
        drive("a", stores.clone()),
        drive("b", stores),
        RunBackend::cpu(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();
    let mut state = SwitcherState::new(SourceRef::new("a"), SourceRef::new("b"), vec![]);
    let tick = Tick {
        index: 0,
        pts: MediaTime::ZERO,
    };
    let mut bus = bus;
    let sample = state.sample(tick);
    let frames = bus
        .compose_selected(tick, state.program(), state.preview(), sample)
        .unwrap();
    assert_eq!(frames.program.tick, frames.preview.tick);
    assert_eq!(frames.program.bus_id, "program");
    assert_eq!(frames.preview.bus_id, "preview");
    let y_program = frames.program.canvas.sample(8, 8).unwrap().0;
    let y_preview = frames.preview.canvas.sample(8, 8).unwrap().0;
    assert!(
        y_program > y_preview + 50,
        "PGM and PVW must represent distinct real canvases: {y_program}, {y_preview}"
    );
    let rate = FrameCount::try_from(10).unwrap();
    state
        .set_transition(multiview_core::switcher::TransitionDescriptor::Mix { rate })
        .unwrap();
    state.auto(tick).unwrap();
    let mid = Tick {
        index: 5,
        pts: MediaTime::from_nanos(200_000_000),
    };
    let sample = state.sample(mid);
    let mixed = bus
        .compose_selected(mid, state.program(), state.preview(), sample)
        .unwrap();
    let y_mid = mixed.program.canvas.sample(8, 8).unwrap().0;
    assert!(
        y_mid < y_program && y_mid > y_preview,
        "linear-light mix must be between both scenes"
    );
    assert_eq!(mixed.preview.canvas.sample(8, 8).unwrap().0, y_preview);
    let end = Tick {
        index: 10,
        pts: MediaTime::from_nanos(400_000_000),
    };
    let completed = state.sample(end);
    assert!(completed.completed);
    let frames = bus
        .compose_selected(end, state.program(), state.preview(), completed)
        .unwrap();
    assert_eq!(frames.program.canvas.sample(8, 8).unwrap().0, y_preview);
    assert_eq!(frames.preview.canvas.sample(8, 8).unwrap().0, y_program);
    let reservations = bus.drives_mut().0.scratch_reservations();
    for index in 11..36 {
        let tick = Tick {
            index,
            pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * 40_000_000),
        };
        let sample = state.sample(tick);
        let frames = bus
            .compose_selected(tick, state.program(), state.preview(), sample)
            .unwrap();
        assert_eq!(frames.program.canvas.sample(8, 8).unwrap().0, y_preview);
        assert_eq!(frames.preview.canvas.sample(8, 8).unwrap().0, y_program);
    }
    assert_eq!(bus.drives_mut().0.scratch_reservations(), reservations);
    let second_start = Tick {
        index: 36,
        pts: MediaTime::from_nanos(1_440_000_000),
    };
    state.auto(second_start).unwrap();
    let next = Tick {
        index: 46,
        pts: MediaTime::from_nanos(1_840_000_000),
    };
    let sample = state.sample(next);
    let second = bus
        .compose_selected(next, state.program(), state.preview(), sample)
        .unwrap();
    assert_eq!(second.program.canvas.sample(8, 8).unwrap().0, y_program);
    assert_eq!(second.preview.canvas.sample(8, 8).unwrap().0, y_preview);
    assert_eq!(bus.drives_mut().0.scratch_reservations(), reservations);
}

#[test]
fn a_cut_remains_selected_on_subsequent_ticks_without_scene_reallocation() {
    let stores = HashMap::new();
    let mut bus = DualBusDrive::new(
        drive("a", stores.clone()),
        drive("b", stores),
        RunBackend::cpu(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();
    // Both missing sources are an honest slate, not a PGM→PVW substitution.
    let mut state = SwitcherState::new(SourceRef::new("a"), SourceRef::new("b"), vec![]);
    assert!(bus.bind_sources(state.program(), state.preview()).is_err());
    let cut = state.cut().unwrap();
    assert!(cut.completed);
    assert_eq!(state.program().as_str(), "b");
    assert_eq!(state.preview().as_str(), "a");
}
