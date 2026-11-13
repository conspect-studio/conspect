//! Start-up admission: the ONE rule deciding whether a document can be the
//! canvas the engine boots (issue #317).
//!
//! `MultiviewConfig::validate` checks that a document is well-formed. The engine
//! additionally refuses, at start-up, a canvas the first-slice M/E cannot drive
//! as a real Program bus. Before #317 that second rule lived only inside
//! `Pipeline::build`, so the control plane could accept and persist a state the
//! engine then refused to start from. Both paths now call [`boot_admission`]
//! (or its canvas half, [`switcher_canvas_admission`]): a state the control plane
//! accepts is a state the engine can boot.

use multiview_core::layout::Layout as CoreLayout;

use crate::error::ConfigError;
use crate::schema::{Cell, Overlay};
use crate::switcher::SwitcherConfig;
use crate::MultiviewConfig;

/// Why the declared switcher M/E cannot drive this canvas as its Program bus.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SwitcherCanvasRefusal {
    /// More than one cell (or none): a multiview cannot be the Program canvas.
    CellCount(usize),
    /// Overlays are declared; the first-slice Program bus carries none.
    Overlays(usize),
    /// The one cell is styled (corner radius, border, non-`fill` fit, hidden).
    StyledCell,
    /// The one cell is not exactly full-canvas and fully opaque.
    NotFullCanvas,
    /// The one cell is not bound to the M/E's `program_source`.
    NotProgramSource {
        /// The source the cell is bound to, if any.
        bound: Option<String>,
        /// The M/E's declared `program_source`.
        program_source: String,
    },
}

impl SwitcherCanvasRefusal {
    /// The operator-facing sentence (shown in the UI and in the problem detail).
    #[must_use]
    pub fn operator_message(&self) -> String {
        match self {
            Self::CellCount(n) => format!(
                "The switcher is on: a multi-cell layout cannot be the Program canvas. This layout has {n} cells; the switcher needs exactly one full-canvas cell."
            ),
            Self::Overlays(n) => format!(
                "The switcher is on: overlays cannot be added to the Program canvas ({n} declared)."
            ),
            Self::StyledCell => "The switcher is on: the Program cell cannot have a corner radius, a border, a fit other than fill, or be hidden.".to_owned(),
            Self::NotFullCanvas => "The switcher is on: the Program cell must cover the whole canvas at full opacity.".to_owned(),
            Self::NotProgramSource { bound, program_source } => format!(
                "The switcher is on: the Program cell must be bound to the M/E program source {program_source:?} (it is bound to {})." ,
                bound.as_deref().map_or_else(|| "nothing".to_owned(), |b| format!("{b:?}"))
            ),
        }
    }
}

impl std::fmt::Display for SwitcherCanvasRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The prefix is the historical start-up error text, kept verbatim so log
        // searches and runbooks still match.
        write!(
            f,
            "first-slice M/E requires one full-canvas source cell initially bound to program_source; multi-cell layouts are not replaced: {}",
            self.operator_message()
        )
    }
}

impl std::error::Error for SwitcherCanvasRefusal {}

/// Can the declared switcher (if any) drive `solved` as its Program canvas?
///
/// Exactly one full-canvas, fully opaque, unclipped, non-overlay cell bound to
/// the declared `program_source`. No M/E declared: always admissible (the
/// legacy multiview is untouched).
///
/// `cells`/`overlays` are the authored config sections the solved layout came
/// from. Pure and allocation-light; callers run it on the control plane or
/// at build time, never on the output clock.
///
/// # Errors
/// The first [`SwitcherCanvasRefusal`] the canvas violates.
#[allow(clippy::float_cmp)]
// reason: authored layout fractions compared against their exact canonical
// values; an approximate comparison would admit a windowed/tiled canvas as
// "full canvas" and change which sources the buses admit.
pub fn switcher_canvas_admission(
    switcher: Option<&SwitcherConfig>,
    solved: &CoreLayout,
    cells: &[Cell],
    overlays: &[Overlay],
) -> Result<(), SwitcherCanvasRefusal> {
    let Some(me) = switcher.and_then(|s| s.mix_effects.first()) else {
        return Ok(());
    };
    if solved.cells.len() != 1 {
        return Err(SwitcherCanvasRefusal::CellCount(solved.cells.len()));
    }
    if !overlays.is_empty() {
        return Err(SwitcherCanvasRefusal::Overlays(overlays.len()));
    }
    if cells.first().is_some_and(|cell| {
        cell.corner_radius.is_some()
            || cell.border.is_some()
            || cell.fit.as_deref().is_some_and(|fit| fit != "fill")
            || cell.visible == Some(false)
    }) {
        return Err(SwitcherCanvasRefusal::StyledCell);
    }
    if !solved.cells.first().is_some_and(|cell| {
        cell.x == 0.0 && cell.y == 0.0 && cell.w == 1.0 && cell.h == 1.0 && cell.opacity == 1.0
    }) {
        return Err(SwitcherCanvasRefusal::NotFullCanvas);
    }
    let bound = cells
        .first()
        .and_then(|cell| cell.source.input_id.as_deref());
    if bound != Some(me.program_source.as_str()) {
        return Err(SwitcherCanvasRefusal::NotProgramSource {
            bound: bound.map(str::to_owned),
            program_source: me.program_source.clone(),
        });
    }
    Ok(())
}

/// ADR-0108 slice 1, step 1: an output fed directly by a source (or a switcher
/// bus other than Program) is validated and read back, but the engine does not
/// carry it yet, so start-up refuses it rather than silently putting Program on
/// that output. A `bus = "preview"` output keeps its existing behaviour. An
/// explicit (non-`follow`) audio feed is accepted: start-up re-applies it as
/// an audio-only boot take (ADR-0108 slice 2 §6.1).
///
/// # Errors
/// The start-up refusal text naming the routed output.
pub fn outputs_carry_program(config: &MultiviewConfig) -> Result<(), String> {
    if let Some(routed) = config
        .output_feeds()
        .into_iter()
        .find(|xp| matches!(xp.effective_video(), crate::VideoFeed::Source(_)))
    {
        return Err(format!(
            "output {:?} is routed to {:?}; this engine carries only Program on outputs",
            routed.output,
            routed.effective_video().picture_bus()
        ));
    }
    // ADR-0108 slice 2: an explicit (non-`follow`) audio feed is accepted:
    // the run starts the output on follow and re-applies the persisted feed
    // as an audio-only boot take once its routing handle is published.
    Ok(())
}

/// Why a document cannot be booted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BootAdmissionError {
    /// The document is not a valid configuration.
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    /// The document is valid but the declared switcher cannot drive its canvas.
    #[error(transparent)]
    SwitcherCanvas(#[from] SwitcherCanvasRefusal),
    /// An output is routed to something this engine does not carry yet.
    #[error("{0}")]
    OutputRouting(String),
}

/// The start-up admission validator: everything a document must satisfy, beyond
/// I/O and device availability, for `Pipeline::build` to accept it. The
/// control plane calls this on every prospective accepted state before it is
/// persisted; `multiview validate` calls it on the accepted state.
///
/// # Errors
/// [`BootAdmissionError::Invalid`] for a malformed document,
/// [`BootAdmissionError::SwitcherCanvas`] for a canvas the M/E cannot drive.
pub fn boot_admission(config: &MultiviewConfig) -> Result<(), BootAdmissionError> {
    config.validate()?;
    let solved = config.solve_layout()?;
    config.switcher_canvas_admission(&solved)?;
    outputs_carry_program(config).map_err(BootAdmissionError::OutputRouting)?;
    Ok(())
}

impl MultiviewConfig {
    /// [`switcher_canvas_admission`] for this document's own switcher, cells and
    /// overlays against its already-solved layout.
    ///
    /// # Errors
    /// The first [`SwitcherCanvasRefusal`] the canvas violates.
    pub fn switcher_canvas_admission(
        &self,
        solved: &CoreLayout,
    ) -> Result<(), SwitcherCanvasRefusal> {
        switcher_canvas_admission(self.switcher.as_ref(), solved, &self.cells, &self.overlays)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn doc(cells: &str, switcher: bool) -> MultiviewConfig {
        let sw = if switcher {
            "[switcher]\n[[switcher.mix_effects]]\nid = \"me1\"\nprogram = \"main\"\nprogram_source = \"a\"\npreview_source = \"b\"\n"
        } else {
            ""
        };
        let text = format!(
            "schema_version = 1\n[canvas]\nwidth = 64\nheight = 64\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#101014\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"absolute\"\n[[sources]]\nid = \"a\"\nkind = \"bars\"\n[[sources]]\nid = \"b\"\nkind = \"bars\"\n{cells}[[outputs]]\nkind = \"hls\"\npath = \"/tmp/x.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n{sw}"
        );
        MultiviewConfig::load_from_toml(&text).expect("fixture parses")
    }

    const ONE: &str = "[[cells]]\nid = \"c1\"\nrect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }\n[cells.source]\ninput_id = \"a\"\n";
    const TWO: &str = "[[cells]]\nid = \"c1\"\nrect = { x = 0.0, y = 0.0, w = 0.5, h = 1.0 }\n[cells.source]\ninput_id = \"a\"\n[[cells]]\nid = \"c2\"\nrect = { x = 0.5, y = 0.0, w = 0.5, h = 1.0 }\n[cells.source]\ninput_id = \"b\"\n";

    #[test]
    fn table_of_canvas_and_switcher_combinations() {
        for (cells, switcher, ok) in [
            (ONE, false, true),
            (ONE, true, true),
            (TWO, false, true),
            (TWO, true, false),
        ] {
            let config = doc(cells, switcher);
            let result = boot_admission(&config);
            assert_eq!(
                result.is_ok(),
                ok,
                "cells={cells:?} switcher={switcher}: {result:?}"
            );
        }
    }

    #[test]
    fn multi_cell_refusal_names_the_operator_reason_and_keeps_the_log_text() {
        let err = boot_admission(&doc(TWO, true)).unwrap_err();
        let BootAdmissionError::SwitcherCanvas(refusal) = &err else {
            panic!("expected a switcher refusal, got {err:?}");
        };
        assert_eq!(refusal, &SwitcherCanvasRefusal::CellCount(2));
        assert!(refusal
            .operator_message()
            .starts_with("The switcher is on: a multi-cell layout cannot be the Program canvas."));
        assert!(err
            .to_string()
            .starts_with("first-slice M/E requires one full-canvas source cell initially bound to program_source; multi-cell layouts are not replaced"));
    }

    #[test]
    fn one_cell_bound_to_another_source_is_refused() {
        let config = doc(&ONE.replace("input_id = \"a\"", "input_id = \"b\""), true);
        assert!(matches!(
            boot_admission(&config),
            Err(BootAdmissionError::SwitcherCanvas(
                SwitcherCanvasRefusal::NotProgramSource { .. }
            ))
        ));
    }
}
