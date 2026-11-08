//! Owned frame hook for a first-slice one-cell Program scene. No shared mutex
//! crosses between the frame boundary and completion callbacks.
use crate::clock::Tick;
use crate::drive::{CompositedFrame, CompositorDrive};
use crate::runtime::FrameControl;
use crate::switcher_control::SwitcherTickDriver;
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::switcher::SourceRef;

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
}

impl<C> SwitcherFrameHook<C> {
    /// Create the hook with a live source id in one addressable cell.
    #[must_use]
    pub fn new(driver: SwitcherTickDriver, cell_id: String, legacy: C) -> Self {
        Self {
            driver,
            cell_id,
            legacy,
            last_program: None,
            failed_tick: None,
        }
    }
}

impl<C: FnMut(&mut CompositorDrive<Nv12Image>)> FrameControl for SwitcherFrameHook<C> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        (self.legacy)(drive);
        self.driver.drain_at_tick(tick);
        let (program, _) = self.driver.selections();
        if self.last_program.as_ref() != Some(program) {
            if let Err(error) = drive.rebind_cell(&self.cell_id, program.as_str()) {
                self.failed_tick = Some(tick.index);
                self.driver.reject_composed_tick(tick, &error.to_string());
            } else {
                self.last_program = Some(program.clone());
            }
        }
    }

    fn after_compose(&mut self, frame: &CompositedFrame) {
        // A fresh frame made it through actual composition, not a repeat.
        if self.failed_tick.take() != Some(frame.tick.index) {
            self.driver.commit_composed_tick(frame.tick);
        }
    }

    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        (self.legacy)(drive);
        self.driver.repeat_tick(tick);
    }
}
