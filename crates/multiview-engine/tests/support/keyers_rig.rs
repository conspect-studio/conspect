//! Shared rig for the downstream keyer tests (ADR-0113, K2a): a 64x64 canvas
//! at 25 fps, one full-canvas Program cell, the real switcher frame hook and
//! the CPU drive, driven tick by tick with exact PTS (`index × 40 ms`).
//! Included with `#[path]` by `keyers_dsk.rs`, `keyers_gate_loss.rs` and
//! `keyers_chaos.rs`; not every helper is used by every file.
#![allow(dead_code)]
// reason: one rig shared by three test crates through `#[path]`; each uses a
// subset.

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::{
    DownstreamKeyerConfig, KeyerLossPolicy, KeyerLossTrigger, KeyerSize, KeyerType,
};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::isolation::{EventSubscription, TryRecvError};
use multiview_engine::keyer;
use multiview_engine::runtime::FrameControl;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{CompositedFrame, CompositorDrive};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const SIDE: u32 = 64;
pub(crate) const TICK_NS: i64 = 40_000_000;
pub(crate) const LUMA_A: u8 = 60;
pub(crate) const LUMA_B: u8 = 120;
pub(crate) const LUMA_FILL: u8 = 235;
pub(crate) const LUMA_BG: u8 = 16;

pub(crate) type Legacy = fn(&mut CompositorDrive<Nv12Image>);
pub(crate) type Hook = SwitcherFrameHook<Legacy>;

fn noop(_: &mut CompositorDrive<Nv12Image>) {}

pub(crate) fn pts(index: u64) -> MediaTime {
    MediaTime::from_nanos(i64::try_from(index).unwrap() * TICK_NS)
}

pub(crate) fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: pts(index),
    }
}

pub(crate) fn layout(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: "program".into(),
        canvas: Canvas {
            width: SIDE,
            height: SIDE,
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

pub(crate) struct Rig {
    pub(crate) stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    pub(crate) color: ColorInfo,
}

impl Rig {
    /// Stores with the default thresholds (hold 500 ms, stale 2 s,
    /// no-signal 10 s), so at 40 ms a source turns stale 13 ticks and
    /// reconnecting 50 ticks after its last frame.
    pub(crate) fn new(ids: &[&str]) -> Self {
        let color = ColorInfo::default().resolve_defaults(SIDE, SIDE);
        let stores = ids
            .iter()
            .map(|id| {
                (
                    (*id).to_owned(),
                    Arc::new(TileStore::<Nv12Image>::with_defaults(*id)),
                )
            })
            .collect();
        Self { stores, color }
    }

    /// Replace `id` with a store whose ladder runs in 2/4/6 ticks.
    pub(crate) fn fast(&mut self, id: &str) {
        let thresholds = TileThresholds::new(
            MediaTime::from_nanos(2 * TICK_NS),
            MediaTime::from_nanos(4 * TICK_NS),
            MediaTime::from_nanos(6 * TICK_NS),
        )
        .unwrap();
        self.stores.insert(
            id.to_owned(),
            Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate)),
        );
    }

    pub(crate) fn store(&self, id: &str) -> &Arc<TileStore<Nv12Image>> {
        &self.stores[id]
    }

    pub(crate) fn solid(luma: u8, w: u32, h: u32) -> Nv12Image {
        let color = ColorInfo::default().resolve_defaults(w, h);
        Nv12Image::solid(w, h, luma, 128, 128, color).unwrap()
    }

    /// Publish a fresh `SIDE`x`SIDE` frame of `luma` into `id` at tick `index`.
    pub(crate) fn publish(&self, id: &str, luma: u8, index: u64) -> Arc<Nv12Image> {
        self.publish_sized(id, luma, index, SIDE, SIDE)
    }

    pub(crate) fn publish_sized(
        &self,
        id: &str,
        luma: u8,
        index: u64,
        w: u32,
        h: u32,
    ) -> Arc<Nv12Image> {
        let frame = Arc::new(Self::solid(luma, w, h));
        self.stores[id].publish_arc(Arc::clone(&frame), pts(index));
        frame
    }

    /// Keep the two background sources fresh at tick `index`.
    pub(crate) fn publish_background(&self, index: u64) {
        self.publish("a", LUMA_A, index);
        self.publish("b", LUMA_B, index);
    }

    /// A drive, a control handle and the real hook with these keyers
    /// admitted against `budget`. PGM `program`, PVW `preview`.
    pub(crate) fn build(
        &self,
        keyers: &[DownstreamKeyerConfig],
        budget: f32,
        program: &str,
        preview: &str,
    ) -> (SwitcherControl, Hook, CompositorDrive<Nv12Image>) {
        self.build_with_cell(keyers, budget, program, preview, "pgm")
    }

    pub(crate) fn build_with_cell(
        &self,
        keyers: &[DownstreamKeyerConfig],
        budget: f32,
        program: &str,
        preview: &str,
        cell: &str,
    ) -> (SwitcherControl, Hook, CompositorDrive<Nv12Image>) {
        self.build_inner(keyers, budget, program, preview, cell, None)
    }

    /// Like [`Self::build`], but the drive has no store for `hidden`, while
    /// the driver's prime gate does: a take onto `hidden` passes the gate,
    /// then its Program rebind fails and the frame is rejected (the
    /// `failed_rebind` pattern), on any tick the test chooses.
    pub(crate) fn build_hiding(
        &self,
        keyers: &[DownstreamKeyerConfig],
        budget: f32,
        program: &str,
        preview: &str,
        hidden: &str,
    ) -> (SwitcherControl, Hook, CompositorDrive<Nv12Image>) {
        self.build_inner(keyers, budget, program, preview, "pgm", Some(hidden))
    }

    fn build_inner(
        &self,
        keyers: &[DownstreamKeyerConfig],
        budget: f32,
        program: &str,
        preview: &str,
        cell: &str,
        hidden: Option<&str>,
    ) -> (SwitcherControl, Hook, CompositorDrive<Nv12Image>) {
        let mut drive_stores = self.stores.clone();
        if let Some(hidden) = hidden {
            drive_stores.remove(hidden);
        }
        let drive = CompositorDrive::new(
            layout(program),
            drive_stores,
            Self::solid(LUMA_BG, SIDE, SIDE),
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
        )
        .unwrap()
        .with_cell_ids(vec![Some("pgm".into())]);
        let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
            SourceRef::new(program),
            SourceRef::new(preview),
            vec![],
        ));
        driver.set_prime_stores(self.stores.clone());
        let defs = keyers
            .iter()
            .map(|cfg| keyer::KeyerDef::from_config(cfg, &self.stores, SIDE, SIDE).unwrap())
            .collect();
        driver.set_keyers(keyer::admit(defs, SIDE, SIDE, budget).unwrap());
        handle
            .set_scenes(ScenePair {
                version: 1,
                program: layout(program),
                preview: layout(preview),
            })
            .unwrap();
        let legacy: Legacy = noop;
        let hook = SwitcherFrameHook::new(driver, cell.into(), legacy);
        (handle, hook, drive)
    }
}

/// A DSK config on `fill` (and `key`) with test-friendly defaults.
pub(crate) fn dsk(id: &str, fill: &str, key: Option<&str>, z: i32) -> DownstreamKeyerConfig {
    DownstreamKeyerConfig {
        id: id.into(),
        program: "main".into(),
        fill: fill.into(),
        key: key.map(Into::into),
        key_type: KeyerType::Linear,
        clip: 0.0,
        gain: 1.0,
        invert: false,
        mask: None,
        fly: None,
        max_source: Some(KeyerSize { w: SIDE, h: SIDE }),
        rate: 15,
        tie: false,
        on_at_start: false,
        z,
        loss: KeyerLossPolicy::DropToOffAir,
        loss_trigger: KeyerLossTrigger::Reconnecting,
        premultiplied: false,
    }
}

pub(crate) fn cmd(op: &str, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op.into(),
        scene_version: 1,
        verb,
    }
}

/// One frame: the hook stages, the drive composes, the hook confirms.
pub(crate) fn step(
    hook: &mut Hook,
    drive: &mut CompositorDrive<Nv12Image>,
    index: u64,
) -> CompositedFrame {
    hook.before_compose(drive, tick(index));
    let frame = drive.compose(tick(index)).unwrap();
    hook.after_compose(&frame);
    frame
}

/// Luma at the canvas centre-ish probe point.
pub(crate) fn probe(frame: &CompositedFrame) -> u8 {
    frame.canvas.sample(8, 8).unwrap().0
}

/// Drain everything buffered; `Lagged` counts are summed into the second
/// value.
pub(crate) fn drain<T>(sub: &mut EventSubscription<T>) -> (Vec<Arc<T>>, u64) {
    let mut items = Vec::new();
    let mut lagged = 0;
    loop {
        match sub.try_recv() {
            Ok(item) => items.push(item.event),
            Err(TryRecvError::Lagged(n)) => lagged += n,
            Err(_) => break,
        }
    }
    (items, lagged)
}

/// The weight of keyer `index`'s draw this tick, if it drew.
pub(crate) fn weight_of(drive: &CompositorDrive<Nv12Image>, index: u8) -> Option<f32> {
    drive
        .keyer_draws()
        .iter()
        .flatten()
        .find(|d| d.keyer == index)
        .map(|d| d.weight)
}
