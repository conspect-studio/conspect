//! Two render buses on one output tick. Both drives must share source-store
//! handles, not spawn duplicate decoders, clocks or output encoders.

use crate::clock::Tick;
use crate::drive::CompositorDrive;
use crate::error::{Error, Result};
use crate::switcher::SwitcherProjection;
use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image, Tile};
use multiview_core::switcher::{Progress, SourceRef};
use std::sync::Arc;

/// One immutable canvas for one named bus on a real output tick.
#[derive(Debug, Clone)]
pub struct BusFrame {
    /// `main` or `preview`; never silently aliased.
    pub bus_id: &'static str,
    /// Exact shared output tick.
    pub tick: Tick,
    /// Composited NV12 image; cloned by reference for output fanout.
    pub canvas: Arc<Nv12Image>,
}

/// Both bus frames of one tick, with exactly one final PGM image.
#[derive(Debug, Clone)]
pub struct BusFrames {
    /// On-air canvas (or current transition blend).
    pub program: BusFrame,
    /// Preview canvas, never replaced by PGM if preview is unavailable.
    pub preview: BusFrame,
}

/// Owns independent scratch pools per bus and one blend compositor. All three
/// calls are synchronous, bounded and use the exact same `Tick`.
pub struct DualBusDrive {
    program: CompositorDrive<Nv12Image>,
    preview: CompositorDrive<Nv12Image>,
    blend: RunBackend,
    canvas_color: CanvasColor,
    background: LinearRgba,
    bound: Option<(SourceRef, SourceRef)>,
}

impl DualBusDrive {
    /// Accept separately constructed drives sharing the same `TileStore` Arcs.
    ///
    /// # Errors
    /// Refuses mismatched geometry or output cadence before starting a clock.
    pub fn new(
        program: CompositorDrive<Nv12Image>,
        preview: CompositorDrive<Nv12Image>,
        blend: RunBackend,
        canvas_color: CanvasColor,
        background: LinearRgba,
    ) -> Result<Self> {
        if program.layout().canvas != preview.layout().canvas {
            return Err(Error::InvalidLayout(
                "switcher buses have incompatible canvas geometry or cadence".into(),
            ));
        }
        Ok(Self {
            program,
            preview,
            blend,
            canvas_color,
            background,
            bound: None,
        })
    }

    /// Bind the selected source to each bus at the frame boundary. Each bus in
    /// the first slice has exactly one full-canvas source cell. A richer layout
    /// scene requires a separate layout resolver, never a guessed first tile.
    ///
    /// # Errors
    /// Refuses a missing cell or source store; no silent PGM fallback is possible.
    pub fn bind_sources(&mut self, program: &SourceRef, preview: &SourceRef) -> Result<()> {
        if self.program.layout().cells.len() != 1 || self.preview.layout().cells.len() != 1 {
            return Err(Error::InvalidLayout("source-select bus requires one full-canvas cell; layout-as-source needs a resolver".into()));
        }
        if self
            .bound
            .as_ref()
            .is_some_and(|(p, v)| p == program && v == preview)
        {
            return Ok(());
        }
        for (drive, source) in [(&self.program, program), (&self.preview, preview)] {
            if drive.store(source.as_str()).is_none() {
                return Err(Error::InvalidLayout(format!(
                    "switcher source {:?} has no registered store",
                    source.as_str()
                )));
            }
        }
        for (drive, source) in [(&mut self.program, program), (&mut self.preview, preview)] {
            if drive
                .layout()
                .cells
                .first()
                .and_then(|c| c.source.as_deref())
                != Some(source.as_str())
            {
                let mut layout = drive.layout().clone();
                if let Some(cell) = layout.cells.first_mut() {
                    cell.source = Some(source.as_str().to_owned());
                }
                drive.set_layout(Arc::new(layout))?;
            }
        }
        self.bound = Some((program.clone(), preview.clone()));
        Ok(())
    }

    /// Render a frame from the state machine projection, rebinding only when a
    /// completed take changes selected sources. A terminal AUTO projection is
    /// already complete: it uses the NEW PGM, and subsequent ticks keep it.
    ///
    /// # Errors
    /// Refuses a missing selected source instead of sending the wrong bus.
    pub fn compose_selected(
        &mut self,
        tick: Tick,
        program: &SourceRef,
        preview: &SourceRef,
        projection: SwitcherProjection,
    ) -> Result<BusFrames> {
        self.bind_sources(program, preview)?;
        let effective = if projection.completed {
            SwitcherProjection {
                progress: Progress::ZERO,
                ..projection
            }
        } else {
            projection
        };
        self.compose(tick, effective)
    }

    /// Render both scenes against one tick. During AUTO the full canvases are
    /// blended as two tiles, so internally overlapping layouts remain correctly
    /// precomposited and can never exhibit the flat-union through-blend defect.
    /// Call through `compose_selected` for stateful source changes: direct
    /// `compose` only renders the drives' already-bound scenes.
    /// No missing bus is substituted by the other: a genuine render error is
    /// returned to the caller to issue a typed slate/failure.
    ///
    /// # Errors
    /// Propagates a compositor failure; the output integration owns last-good
    /// and slate fallback, not an implicit PGM/PVW substitution.
    pub fn compose(&self, tick: Tick, projection: SwitcherProjection) -> Result<BusFrames> {
        let pgm = self.program.compose(tick)?;
        let pvw = self.preview.compose(tick)?;
        let pvw = Arc::new(pvw.canvas);
        let pgm = if projection.progress == Progress::ZERO {
            Arc::new(pgm.canvas)
        } else if projection.progress == Progress::COMPLETE {
            Arc::clone(&pvw)
        } else {
            let canvas = &self.program.layout().canvas;
            let opacity = multiview_core::switcher::kernel::opacity(projection.progress);
            let mixed = self
                .blend
                .composite(
                    canvas.width,
                    canvas.height,
                    self.canvas_color,
                    self.background,
                    &[
                        Tile::placed(&pgm.canvas, 0, 0, 1.0),
                        Tile::placed(&pvw, 0, 0, opacity),
                    ],
                )
                .map_err(|e| Error::Canvas(e.to_string()))?;
            Arc::new(mixed)
        };
        Ok(BusFrames {
            program: BusFrame {
                bus_id: "main",
                tick,
                canvas: pgm,
            },
            preview: BusFrame {
                bus_id: "preview",
                tick,
                canvas: pvw,
            },
        })
    }

    /// Mutably access one selected drive only between tick compositions. A
    /// frame-boundary source/layout edit is never applied mid-pair.
    pub fn drives_mut(
        &mut self,
    ) -> (
        &mut CompositorDrive<Nv12Image>,
        &mut CompositorDrive<Nv12Image>,
    ) {
        (&mut self.program, &mut self.preview)
    }
}
