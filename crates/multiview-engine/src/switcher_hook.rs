//! Owned frame hook for a first-slice one-cell Program scene. No shared mutex
//! crosses between the frame boundary and completion callbacks.
use crate::clock::Tick;
use crate::drive::{CompositedFrame, CompositorDrive, MixOverlay, TransitionStyle};
use crate::keyer::{KeyerDraw, MAX_DSK};
use crate::runtime::FrameControl;
use crate::switcher_control::SwitcherTickDriver;
use multiview_compositor::mask::MaskShape;
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::switcher::{Rgb8, SourceRef, TransitionDescriptor, WipePattern};
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
    fps_num: i64,
    fps_den: i64,
    /// Fixed exchange buffer for this tick's keyer draws (ADR-0113): filled
    /// by the driver, swapped into the drive, so the previous tick's `Arc`s
    /// come back here and drop on the next resolve. No per-tick allocation.
    keyer_buf: [Option<KeyerDraw>; MAX_DSK],
}

/// The compositor's mask shape for a switcher wipe pattern.
fn mask_shape(pattern: WipePattern) -> MaskShape {
    match pattern {
        WipePattern::Vertical => MaskShape::Vertical,
        WipePattern::Diagonal => MaskShape::Diagonal,
        WipePattern::Box => MaskShape::Box,
        WipePattern::Circle => MaskShape::Circle,
        _ => MaskShape::Horizontal,
    }
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
            fps_num: 0,
            fps_den: 0,
            keyer_buf: [const { None }; MAX_DSK],
        }
    }
}

impl<C: FnMut(&mut CompositorDrive<Nv12Image>)> FrameControl for SwitcherFrameHook<C> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.fps_num = drive.layout().canvas.fps_num;
        self.fps_den = drive.layout().canvas.fps_den;
        (self.legacy)(drive);
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
            TransitionDescriptor::Wipe {
                pattern,
                softness,
                border_width,
                border_color,
                position_xy,
                reverse,
                ..
            } => TransitionStyle::Wipe {
                shape: mask_shape(*pattern),
                origin: *position_xy,
                reverse: *reverse,
                softness: *softness,
                border_width: *border_width,
                border_color: *border_color,
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
            // No keyer is drawn on a rejected frame's restored state: swap
            // an empty buffer in, then drop what came back.
            for draw in &mut self.keyer_buf {
                *draw = None;
            }
            drive.set_keyer_draws(&mut self.keyer_buf);
            for draw in &mut self.keyer_buf {
                *draw = None;
            }
            // Keep the drive coherent with the restored machine (SW2-03): if
            // this tick re-pointed the cell before failing, point it back.
            let (restored, _) = self.driver.resolved_selections();
            if self.last_program.as_ref() != Some(restored)
                && drive.rebind_cell(&self.cell_id, restored.as_str()).is_ok()
            {
                self.last_program = Some(restored.clone());
            }
            return;
        }
        // Downstream keyers (ADR-0113), only on a frame that will commit:
        // tied keyers follow the same envelope the transition tiles use (MIX
        // opacity, DIP weights), then one buffer swap into the drive (the
        // previous tick's `Arc`s come back and drop on the next resolve).
        let envelope = drive.mix_overlay().map(MixOverlay::tie_envelope);
        self.driver
            .resolve_keyers(tick, envelope, &mut self.keyer_buf);
        drive.set_keyer_draws(&mut self.keyer_buf);
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
