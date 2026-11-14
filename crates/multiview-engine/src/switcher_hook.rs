//! Owned frame hook for a first-slice one-cell Program scene. No shared mutex
//! crosses between the frame boundary and completion callbacks.
use crate::clock::Tick;
use crate::drive::{CompositedFrame, CompositorDrive, TransitionStyle};
use crate::runtime::FrameControl;
use crate::switcher_control::SwitcherTickDriver;
use crate::switcher_layouts::LayoutSources;
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::switcher::{Rgb8, SourceRef, TransitionDescriptor};
use multiview_framestore::{SwapCell, TileStore};
use std::collections::HashMap;
use std::sync::Arc;

/// Rebinds only one named full-canvas cell; the separate PVW monitor/encode
/// provider owns its own frame slot. Unknown source/store is rejected up front.
pub struct SwitcherFrameHook<C> {
    /// Sole M/E state owner on the output-clock task.
    pub driver: SwitcherTickDriver,
    /// The source-selected full-canvas program cell id.
    pub cell_id: String,
    /// Existing live source/layout/overlay control hook, preserved verbatim.
    pub legacy: C,
    last_program: Option<SourceRef>,
    failed_tick: Option<u64>,
    /// Last logged rebind failure, so a persistent failure logs once (#245).
    last_error: Option<String>,
    /// Off-clock PVW monitor slot; `None` until a real PVW source frame exists.
    pub preview_slot: Option<Arc<SwapCell<Option<Arc<Nv12Image>>>>>,
    /// Shared store map, reused rather than decoding the preview source twice.
    pub preview_stores: Option<HashMap<String, Arc<TileStore<Nv12Image>>>>,
    /// Wait-free PVW selection for the routed Preview feed (ADR-0108 §2.5):
    /// written only when the PVW source changes, so it allocates only then.
    pub preview_selection: Option<Arc<SwapCell<Option<Arc<str>>>>>,
    last_preview: Option<Arc<str>>,
    /// Stored layouts registered as M/E sources (#307): adopted at the frame
    /// boundary when the registry generation moves. `None` = not wired.
    pub layout_sources: Option<Arc<LayoutSources>>,
    seen_generation: u64,
    /// Keys currently adopted into the drive/driver/preview maps (≤ 4).
    adopted: Vec<Arc<str>>,
    /// A removal was deferred because the M/E still referenced the key.
    deferred: bool,
    fps_num: i64,
    fps_den: i64,
}

impl<C> SwitcherFrameHook<C> {
    /// Create the hook with a live source id in one addressable cell.
    #[must_use]
    pub fn new(driver: SwitcherTickDriver, cell_id: String, legacy: C) -> Self {
        // This hook renders the AUTO dissolve inside the Program drive
        // (ADR-0104 §3), so AUTO becomes admissible only once it exists.
        driver.advertise_auto();
        Self {
            driver,
            cell_id,
            legacy,
            last_program: None,
            failed_tick: None,
            last_error: None,
            preview_slot: None,
            preview_stores: None,
            preview_selection: None,
            last_preview: None,
            layout_sources: None,
            seen_generation: 0,
            adopted: Vec::new(),
            deferred: false,
            fps_num: 0,
            fps_den: 0,
        }
    }

    /// Keys of the layout sources adopted into the drive right now.
    #[must_use]
    pub fn adopted_layout_sources(&self) -> &[Arc<str>] {
        &self.adopted
    }

    /// Adopt the published layout-source set at this frame boundary (#307).
    /// One Acquire load in the steady state; a reconcile (and its allocation)
    /// runs only when the generation moved. A removal of a key the M/E still
    /// references is deferred and re-checked on later ticks (O(adopted) ≤ 4,
    /// no allocation) until it is released.
    fn adopt_layout_sources(&mut self, drive: &mut CompositorDrive<Nv12Image>) {
        let Some(sources) = self.layout_sources.as_ref() else {
            return;
        };
        let generation = sources.generation();
        if generation != self.seen_generation {
            self.seen_generation = generation;
            let set = sources.load();
            for (key, store) in &set.entries {
                let current = drive.store(key);
                if current.is_some_and(|existing| Arc::ptr_eq(existing, store)) {
                    continue;
                }
                drive.insert_store(key.as_ref(), Arc::clone(store));
                self.driver.insert_prime_store(key, Arc::clone(store));
                if let Some(stores) = self.preview_stores.as_mut() {
                    stores.insert(key.to_string(), Arc::clone(store));
                }
                if !self.adopted.iter().any(|k| k == key) {
                    self.adopted.push(Arc::clone(key));
                }
            }
            self.deferred = self.adopted.iter().any(|key| set.get(key).is_none());
            // Ack: every key of this generation is in the maps now, so a
            // command naming one, drained after this, finds its store.
            sources.mark_adopted(generation);
        }
        if !self.deferred {
            return;
        }
        let set = sources.load();
        let mut index = 0;
        let mut still_deferred = false;
        while let Some(key) = self.adopted.get(index) {
            if set.get(key).is_some() {
                index += 1;
                continue;
            }
            if self.driver.references_source(key) {
                still_deferred = true;
                index += 1;
                continue;
            }
            drive.remove_store(key);
            self.driver.remove_prime_store(key);
            if let Some(stores) = self.preview_stores.as_mut() {
                stores.remove(key.as_ref());
            }
            self.adopted.swap_remove(index);
        }
        self.deferred = still_deferred;
    }
}

impl<C: FnMut(&mut CompositorDrive<Nv12Image>)> FrameControl for SwitcherFrameHook<C> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.fps_num = drive.layout().canvas.fps_num;
        self.fps_den = drive.layout().canvas.fps_den;
        (self.legacy)(drive);
        self.adopt_layout_sources(drive);
        self.driver.drain_at_tick(tick);
        // Resolved SOURCES: a re-patch of the PGM input changes `program`
        // without a machine change, and rebinds the cell on this same tick.
        let (program, preview) = self.driver.resolved_selections();
        let mut failure = None;
        if self.last_program.as_ref() != Some(program) {
            match drive.rebind_cell(&self.cell_id, program.as_str()) {
                Ok(()) => self.last_program = Some(program.clone()),
                Err(error) => failure = Some(error.to_string()),
            }
        }
        // AUTO dissolve: the outgoing source stays in the Program cell, the
        // incoming one is drawn above it at elapsed/duration in this same pass.
        // At the terminal tick the machine has swapped and the window is gone.
        let mix = self.driver.auto_window().map(|(start, rate)| {
            let elapsed = (i128::from(tick.index) - start).clamp(0, i128::from(rate.get()));
            (
                u32::try_from(elapsed).unwrap_or(rate.get()),
                rate.get(),
                start,
            )
        });
        // The style follows the machine's (frozen in-flight) descriptor; a
        // change re-codes the DIP colour once, never per tick.
        let style = match self.driver.transition() {
            TransitionDescriptor::Dip {
                color,
                switch_point,
                ..
            } => TransitionStyle::Dip {
                color: color.unwrap_or(Rgb8::BLACK),
                switch_point: *switch_point,
            },
            _ => TransitionStyle::Mix,
        };
        if mix.is_some() && drive.transition_style() != style {
            if let Err(error) = drive.set_transition_style(style) {
                failure = Some(error.to_string());
            }
        }
        let result = match mix {
            Some((elapsed, duration, start)) => {
                drive.set_mix_overlay(Some((preview.as_str(), elapsed, duration, start)))
            }
            None if drive.mix_overlay().is_some() => drive.set_mix_overlay(None),
            None => Ok(()),
        };
        if let (None, Err(error)) = (&failure, result) {
            failure = Some(error.to_string());
        }
        if let Some(error) = failure {
            if self.last_error.as_deref() != Some(error.as_str()) {
                tracing::warn!(cell = %self.cell_id, %error, "switcher frame rejected: Program cell could not be bound");
                self.last_error = Some(error.clone());
            }
            self.failed_tick = Some(tick.index);
            self.driver.reject_composed_tick(tick, &error);
            let _ = drive.set_mix_overlay(None);
            // Keep the drive coherent with the restored machine (SW2-03): if
            // this tick re-pointed the cell before failing, point it back.
            let (restored, _) = self.driver.resolved_selections();
            if self.last_program.as_ref() != Some(restored)
                && drive.rebind_cell(&self.cell_id, restored.as_str()).is_ok()
            {
                self.last_program = Some(restored.clone());
            }
        }
    }

    fn after_compose(&mut self, frame: &CompositedFrame) {
        if let Some(selection) = &self.preview_selection {
            let (_, preview) = self.driver.resolved_selections();
            if self.last_preview.as_deref() != Some(preview.as_str()) {
                let id: Arc<str> = Arc::from(preview.as_str());
                selection.store(Some(Arc::clone(&id)));
                self.last_preview = Some(id);
            }
        }
        // A cheap, wait-free source sample only (no lock, no allocation): the
        // selected PVW source's newest frame becomes the monitor picture. An
        // actual configured full-rate PVW output must instead own its own
        // admitted compositor/rendition — this tap never composes or encodes.
        if let (Some(slot), Some(stores)) = (&self.preview_slot, &self.preview_stores) {
            let (_, preview) = self.driver.resolved_selections();
            let picture = stores
                .get(preview.as_str())
                .and_then(|store| store.read_at(frame.tick.pts).frame().cloned());
            // A missing PVW frame stays an honest "unavailable" monitor; it is
            // never replaced with the Program canvas.
            slot.store(picture);
        }
        // A rejected take must not commit its (rolled-back) state, and a repeat
        // tick never reaches here. Everything below describes a fresh frame.
        if self.failed_tick.take() == Some(frame.tick.index) {
            return;
        }
        self.driver.commit_composed_tick(frame.tick);
        // Capability is advertised only once BOTH real buses have a picture and
        // the authored scene generation is known — never a fabricated version,
        // so the control plane cannot 202 an unwired or version-mismatched
        // provider.
        let preview_ready = self
            .preview_slot
            .as_ref()
            .is_some_and(|slot| slot.load().is_some());
        if preview_ready {
            if let Some(pair) = self.driver.scene_pair() {
                self.driver
                    .publish_provider_status(crate::switcher_control::ProviderStatus {
                        scene_version: pair.version,
                        last_tick: frame.tick.index,
                        width: frame.canvas.width(),
                        height: frame.canvas.height(),
                        fps_num: self.fps_num,
                        fps_den: self.fps_den,
                    });
            }
        }
    }

    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        (self.legacy)(drive);
        self.driver.repeat_tick(tick);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use crate::switcher::SwitcherState;
    use crate::switcher_control::{
        PatchSet, ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
    };
    use crate::switcher_inputs::MixerInputs;
    use crate::switcher_layouts::LayoutSourceSet;
    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_core::color::ColorInfo;
    use multiview_core::layout::{Canvas, Cell, Layout};
    use multiview_core::time::MediaTime;

    const KEY: &str = "layout:x";

    fn layout(id: &str) -> Arc<Layout> {
        Arc::new(Layout {
            name: "program".into(),
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

    fn solid(luma: u8) -> Nv12Image {
        let color = ColorInfo::default().resolve_defaults(64, 64);
        Nv12Image::solid(64, 64, luma, 128, 128, color).unwrap()
    }

    fn tick(index: u64) -> Tick {
        Tick {
            index,
            pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * 40_000_000),
        }
    }

    type Hook = SwitcherFrameHook<fn(&mut CompositorDrive<Nv12Image>)>;

    struct Rig {
        handle: SwitcherControl,
        hook: Hook,
        drive: CompositorDrive<Nv12Image>,
        sources: Arc<LayoutSources>,
        store: Arc<TileStore<Nv12Image>>,
        stores: Vec<Arc<TileStore<Nv12Image>>>,
        next: u64,
    }

    fn noop(_: &mut CompositorDrive<Nv12Image>) {}

    fn rig(inputs: bool) -> Rig {
        let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
        let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
        let stores: HashMap<String, Arc<TileStore<Nv12Image>>> =
            HashMap::from([("a".into(), Arc::clone(&a)), ("b".into(), Arc::clone(&b))]);
        let drive = CompositorDrive::new(
            layout("a"),
            stores.clone(),
            solid(16),
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
        )
        .unwrap()
        .with_cell_ids(vec![Some("pgm".into())]);
        let (program, preview) = if inputs {
            (SourceRef::input(1), SourceRef::input(2))
        } else {
            (SourceRef::new("a"), SourceRef::new("b"))
        };
        let (handle, mut driver) =
            SwitcherControl::new(SwitcherState::new(program, preview, vec![]));
        driver.set_prime_stores(stores.clone());
        if inputs {
            driver.set_inputs(
                MixerInputs::new([
                    (1, None, Some(SourceRef::new("a"))),
                    (2, None, Some(SourceRef::new("b"))),
                    (3, None, None),
                ])
                .unwrap(),
            );
        }
        handle
            .set_scenes(ScenePair {
                version: 1,
                program: layout("a"),
                preview: layout("b"),
            })
            .unwrap();
        let mut hook: Hook = SwitcherFrameHook::new(driver, "pgm".into(), noop);
        let sources = Arc::new(LayoutSources::new());
        hook.layout_sources = Some(Arc::clone(&sources));
        hook.preview_stores = Some(stores);
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults(KEY));
        Rig {
            handle,
            hook,
            drive,
            sources,
            store,
            stores: vec![a, b],
            next: 0,
        }
    }

    impl Rig {
        fn register(&self) {
            self.sources.publish(LayoutSourceSet {
                entries: vec![(Arc::from(KEY), Arc::clone(&self.store))],
            });
        }

        fn unregister(&self) {
            self.sources.publish(LayoutSourceSet::default());
        }

        /// One fresh tick: every store (and the layout store, if `fresh`)
        /// gets a frame at the tick pts first.
        fn step(&mut self, fresh_layout: bool) {
            let tick = tick(self.next);
            self.next += 1;
            for store in &self.stores {
                store.publish(solid(200), tick.pts);
            }
            if fresh_layout {
                self.store.publish(solid(90), tick.pts);
            }
            self.hook.before_compose(&mut self.drive, tick);
            let frame = self.drive.compose(tick).unwrap();
            self.hook.after_compose(&frame);
        }

        fn send(&self, verb: SwitcherVerb) {
            self.handle
                .try_send(SwitcherCommand {
                    operation_id: format!("op-{}", self.next),
                    scene_version: 1,
                    verb,
                })
                .unwrap();
        }

        fn present(&self) -> bool {
            let drive = self.drive.store(KEY).is_some();
            let preview = self
                .hook
                .preview_stores
                .as_ref()
                .is_some_and(|s| s.contains_key(KEY));
            assert_eq!(drive, preview, "drive and preview maps agree");
            drive
        }
    }

    #[test]
    fn a_generation_change_adopts_the_store_everywhere() {
        let mut rig = rig(false);
        rig.step(true);
        assert!(!rig.present());
        rig.register();
        rig.step(true);
        assert!(rig.present());
        assert_eq!(rig.sources.adopted_generation(), rig.sources.generation());
        assert!(rig.hook.driver.references_source("a"));
        assert_eq!(rig.hook.adopted_layout_sources().len(), 1);
        // The driver knows the store: a select of the layout is admitted.
        rig.send(SwitcherVerb::SelectProgram(SourceRef::new(KEY)));
        rig.step(true);
        assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), KEY);
        // A body swap (same key, new store) replaces the Arc in place.
        let swapped = Arc::new(TileStore::<Nv12Image>::with_defaults(KEY));
        rig.sources.publish(LayoutSourceSet {
            entries: vec![(Arc::from(KEY), Arc::clone(&swapped))],
        });
        rig.step(false);
        assert!(Arc::ptr_eq(rig.drive.store(KEY).unwrap(), &swapped));
        assert_eq!(rig.hook.adopted_layout_sources().len(), 1);
    }

    #[test]
    fn an_unchanged_generation_does_no_work() {
        let mut rig = rig(false);
        rig.register();
        rig.step(true);
        assert!(rig.present());
        // Removed behind the hook's back: with the generation unchanged the
        // hook never looks at the set again, so it does not re-insert.
        rig.drive.remove_store(KEY);
        rig.step(true);
        assert!(rig.drive.store(KEY).is_none());
        assert_eq!(rig.hook.seen_generation, rig.sources.generation());
        assert!(!rig.hook.deferred);
    }

    #[test]
    fn removal_waits_while_on_program_then_happens_once_released() {
        let mut rig = rig(false);
        rig.register();
        rig.step(true);
        rig.send(SwitcherVerb::SelectProgram(SourceRef::new(KEY)));
        rig.step(true);
        rig.unregister();
        for _ in 0..5 {
            rig.step(true);
            assert!(rig.present(), "never removed while on Program");
            assert!(rig.hook.deferred);
        }
        rig.send(SwitcherVerb::SelectProgram(SourceRef::new("a")));
        rig.step(true);
        // Adoption runs before the drain: the release is seen at the next
        // frame boundary.
        assert!(rig.present());
        rig.step(true);
        assert!(!rig.present(), "removed at the boundary after the release");
        assert!(!rig.hook.deferred);
        assert_eq!(rig.hook.adopted_layout_sources(), []);
        assert!(!rig.hook.driver.references_source(KEY));
    }

    #[test]
    fn removal_waits_while_on_preview() {
        let mut rig = rig(false);
        rig.register();
        rig.step(true);
        rig.send(SwitcherVerb::SelectPreview(SourceRef::new(KEY)));
        rig.step(true);
        assert_eq!(rig.handle.snapshot().unwrap().preview.as_str(), KEY);
        rig.unregister();
        rig.step(true);
        assert!(rig.present(), "never removed while on Preview");
        rig.send(SwitcherVerb::SelectPreview(SourceRef::new("b")));
        rig.step(true);
        rig.step(true);
        assert!(!rig.present());
    }

    #[test]
    fn removal_waits_while_patched_to_an_input() {
        let mut rig = rig(true);
        rig.register();
        rig.step(true);
        rig.send(SwitcherVerb::Patch(PatchSet {
            members: vec![(3, Some(SourceRef::new(KEY)))],
            allow_on_air: false,
        }));
        rig.step(true);
        assert_eq!(rig.handle.inputs().unwrap().version, 1);
        rig.unregister();
        rig.step(true);
        assert!(rig.present(), "never removed while patched");
        rig.send(SwitcherVerb::Patch(PatchSet {
            members: vec![(3, None)],
            allow_on_air: false,
        }));
        rig.step(true);
        rig.step(true);
        assert!(!rig.present());
    }

    #[test]
    fn removal_waits_while_a_held_take_names_it() {
        let mut rig = rig(false);
        rig.register();
        rig.step(false);
        // The layout store has no frame yet: the punch is held for prime.
        rig.send(SwitcherVerb::SelectProgram(SourceRef::new(KEY)));
        rig.step(false);
        assert!(rig.hook.driver.references_source(KEY));
        rig.unregister();
        rig.step(false);
        assert!(rig.present(), "never removed while a held take names it");
        // The held take expires at the prime deadline, then it is released.
        for _ in 0..crate::switcher_control::TAKE_PRIME_DEADLINE_TICKS {
            rig.step(false);
        }
        assert!(!rig.hook.driver.references_source(KEY));
        rig.step(false);
        assert!(!rig.present());
    }
}
