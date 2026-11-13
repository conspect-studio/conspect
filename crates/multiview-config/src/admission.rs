//! Start-up admission: the ONE rule deciding whether a document can be the
//! canvas the engine boots (issue #317), and the ONE function that derives the
//! Program canvas the engine drives when a switcher M/E is declared (#307).
//!
//! `MultiviewConfig::validate` checks that a document is well-formed. The engine
//! additionally refuses, at start-up, what the M/E cannot carry. Both the engine
//! (`Pipeline::build`) and the control plane call [`boot_admission`] (or its
//! canvas half, [`MultiviewConfig::program_canvas`]): a state the control plane
//! accepts is a state the engine can boot.
//!
//! With `[switcher]` declared the Program canvas IS the M/E bus: one
//! full-canvas, fully opaque, `fill` cell bound to the M/E `program_source`
//! (`docs/architecture/output-routing.md` §3). When the document's own solved
//! layout already is exactly that, it is used unchanged; otherwise (a multi-cell
//! wall, a styled or windowed cell, a cell bound to another source) the bus is
//! synthesized and the document's layout stays a stored layout, routable to
//! any output. Nothing is replaced and nothing is refused for its shape.

use multiview_core::layout::{Cell as CoreCell, FitMode, Layout as CoreLayout};

use crate::error::ConfigError;
use crate::failover::{default_failover_slate, FailoverSlate};
use crate::schema::{Cell, Overlay};
use crate::switcher::{MixEffectsConfig, SwitcherConfig};
use crate::MultiviewConfig;

/// Suffix of the reserved id of a synthesized Program cell: `<me id>.program`
/// (e.g. `me1.program`). Should an authored cell already carry that id, the
/// id is lengthened with `~` until it is unique (see [`program_canvas`]).
pub const PROGRAM_CELL_SUFFIX: &str = ".program";

/// What the declared switcher M/E cannot carry on its Program canvas.
///
/// Since #307 the shape of the document's layout is never a refusal (the
/// Program canvas is synthesized when it is not already the M/E bus); only
/// what the Program bus genuinely cannot carry remains.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SwitcherCanvasRefusal {
    /// Overlays are declared; the M/E Program bus carries none yet.
    Overlays(usize),
}

impl SwitcherCanvasRefusal {
    /// The operator-facing sentence (shown in the UI and in the problem detail).
    #[must_use]
    pub fn operator_message(&self) -> String {
        match self {
            Self::Overlays(n) => format!(
                "The switcher is on: overlays cannot be added to the Program canvas ({n} declared)."
            ),
        }
    }
}

impl std::fmt::Display for SwitcherCanvasRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the switcher M/E owns the Program canvas: {}",
            self.operator_message()
        )
    }
}

impl std::error::Error for SwitcherCanvasRefusal {}

/// The canvas the engine drives as Program, derived by [`program_canvas`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramCanvas {
    /// The layout on the Program drive.
    pub layout: CoreLayout,
    /// With a switcher: the id of the one Program cell the M/E rebinds
    /// (`None` without a switcher: the legacy multiview addresses every
    /// document cell by its own id).
    pub cell_id: Option<String>,
    /// With a switcher: the failover slate of the Program cell.
    pub slate: FailoverSlate,
    /// `true` when the Program cell was synthesized because the document's
    /// own layout is not the M/E bus (the document layout stays a stored
    /// layout, routable to outputs).
    pub synthesized: bool,
}

/// Is `solved` (from the authored `cells`) already exactly the M/E bus: one
/// unstyled, full-canvas, fully opaque cell bound to `program_source`?
#[allow(clippy::float_cmp)]
// reason: authored layout fractions compared against their exact canonical
// values; an approximate comparison would take a windowed/tiled canvas as
// "full canvas".
fn is_me_bus(me: &MixEffectsConfig, solved: &CoreLayout, cells: &[Cell]) -> bool {
    let ([core], [cell]) = (solved.cells.as_slice(), cells) else {
        return false;
    };
    let unstyled = cell.corner_radius.is_none()
        && cell.border.is_none()
        && cell.fit.as_deref().is_none_or(|fit| fit == "fill")
        && cell.visible != Some(false);
    let full = core.x == 0.0 && core.y == 0.0 && core.w == 1.0 && core.h == 1.0;
    unstyled
        && full
        && core.opacity == 1.0
        && cell.source.input_id.as_deref() == Some(me.program_source.as_str())
}

/// The reserved id of a synthesized Program cell, unique against `cells`.
fn reserved_cell_id(me: &MixEffectsConfig, cells: &[Cell]) -> String {
    let mut id = format!("{}{PROGRAM_CELL_SUFFIX}", me.id);
    while cells.iter().any(|cell| cell.id == id) {
        id.push('~');
    }
    id
}

/// The Program canvas for `solved` (the document's solved layout, from the
/// authored `cells`/`overlays`) under the declared `switcher`, if any.
///
/// * No M/E declared: `solved` unchanged, every cell addressed by its own id
///   (the legacy multiview is untouched).
/// * M/E declared and `solved` already is the M/E bus: `solved` unchanged,
///   its one cell (and that cell's id and slate) is the Program cell —
///   byte-identical to the first slice.
/// * M/E declared otherwise: one synthesized full-canvas, opaque, `fill`,
///   z 0 cell bound to `program_source` on the same canvas, with a reserved
///   id ([`PROGRAM_CELL_SUFFIX`]) and the default slate.
///
/// Pure and allocation-light; callers run it on the control plane or at
/// build time, never on the output clock.
///
/// # Errors
/// [`SwitcherCanvasRefusal::Overlays`] when overlays are declared with an M/E.
pub fn program_canvas(
    switcher: Option<&SwitcherConfig>,
    solved: CoreLayout,
    cells: &[Cell],
    overlays: &[Overlay],
) -> Result<ProgramCanvas, SwitcherCanvasRefusal> {
    let Some(me) = switcher.and_then(|s| s.mix_effects.first()) else {
        return Ok(ProgramCanvas {
            layout: solved,
            cell_id: None,
            slate: default_failover_slate(),
            synthesized: false,
        });
    };
    if !overlays.is_empty() {
        return Err(SwitcherCanvasRefusal::Overlays(overlays.len()));
    }
    if is_me_bus(me, &solved, cells) {
        let (cell_id, slate) = cells.first().map_or_else(
            || (None, default_failover_slate()),
            |cell| (Some(cell.id.clone()), cell.on_loss),
        );
        return Ok(ProgramCanvas {
            layout: solved,
            cell_id,
            slate,
            synthesized: false,
        });
    }
    let layout = CoreLayout {
        name: solved.name,
        canvas: solved.canvas,
        cells: vec![CoreCell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            z: 0,
            fit: FitMode::Fill,
            source: Some(me.program_source.clone()),
            opacity: 1.0,
        }],
    };
    Ok(ProgramCanvas {
        layout,
        cell_id: Some(reserved_cell_id(me, cells)),
        slate: default_failover_slate(),
        synthesized: true,
    })
}

/// The single start-up rule for output feeds (ADR-0108 §6.1, #329). Every
/// feed is accepted: the engine starts every output on Program and then
/// issues one BOOT TAKE per output whose persisted feed is not Program,
/// through the normal take gate with a bounded retry window (the output stays
/// on Program if it never becomes takeable). A persisted stored-layout feed
/// (slice 3) starts on Program and is not re-taken at start-up yet: its body
/// lives in the layouts library, which start-up does not resolve; the
/// readback shows it as configured but not on air. It is never a reason to
/// refuse a start (#317). An explicit (non-`follow`) audio feed is re-applied
/// as an audio-only boot take (ADR-0108 slice 2 §6.1).
///
/// # Errors
/// None today; the signature keeps the one start-up rule in one place.
#[allow(clippy::unnecessary_wraps)] // The one start-up rule; it may refuse again.
pub fn outputs_carry_program(config: &MultiviewConfig) -> Result<(), String> {
    let _ = config;
    Ok(())
}

/// Why a document cannot be booted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BootAdmissionError {
    /// The document is not a valid configuration.
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    /// The document is valid but the declared switcher cannot carry it.
    #[error(transparent)]
    SwitcherCanvas(#[from] SwitcherCanvasRefusal),
    /// An output is routed to something this engine cannot carry.
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
/// [`BootAdmissionError::SwitcherCanvas`] for what the M/E cannot carry.
pub fn boot_admission(config: &MultiviewConfig) -> Result<(), BootAdmissionError> {
    config.validate()?;
    let solved = config.solve_layout()?;
    config.program_canvas(solved)?;
    outputs_carry_program(config).map_err(BootAdmissionError::OutputRouting)?;
    Ok(())
}

impl MultiviewConfig {
    /// [`program_canvas`] for this document's own switcher, cells and overlays
    /// against its already-solved layout.
    ///
    /// # Errors
    /// [`SwitcherCanvasRefusal`] for what the M/E cannot carry.
    pub fn program_canvas(
        &self,
        solved: CoreLayout,
    ) -> Result<ProgramCanvas, SwitcherCanvasRefusal> {
        program_canvas(self.switcher.as_ref(), solved, &self.cells, &self.overlays)
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
    fn multi_cell_switcher_program_is_the_synthesized_one_cell_me_bus() {
        let config = doc(TWO, true);
        let solved = config.solve_layout().unwrap();
        let program = config.program_canvas(solved.clone()).unwrap();
        assert!(program.synthesized);
        assert_eq!(program.cell_id.as_deref(), Some("me1.program"));
        assert_eq!(program.layout.canvas, solved.canvas, "same canvas");
        assert_eq!(program.layout.name, solved.name);
        assert_eq!(
            program.layout.cells,
            vec![CoreCell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Fill,
                source: Some("a".to_owned()),
                opacity: 1.0,
            }]
        );
        program.layout.validate().unwrap();
    }

    #[test]
    fn an_admissible_one_cell_canvas_is_used_unchanged() {
        let config = doc(ONE, true);
        let solved = config.solve_layout().unwrap();
        let program = config.program_canvas(solved.clone()).unwrap();
        assert!(!program.synthesized);
        assert_eq!(program.layout, solved, "byte-identical first-slice canvas");
        assert_eq!(program.cell_id.as_deref(), Some("c1"), "same cell id");
    }

    #[test]
    fn without_a_switcher_the_document_layout_is_the_program_canvas() {
        let config = doc(TWO, false);
        let solved = config.solve_layout().unwrap();
        let program = config.program_canvas(solved.clone()).unwrap();
        assert_eq!(program.layout, solved);
        assert_eq!(program.cell_id, None);
        assert!(!program.synthesized);
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
    
    #[test]
    fn styled_or_windowed_one_cell_canvases_synthesize_the_program_cell() {
        for cells in [
            ONE.replace("[cells.source]", "fit = \"contain\"\n[cells.source]"),
            ONE.replace("w = 1.0", "w = 0.5"),
            ONE.replace("[cells.source]", "opacity = 0.5\n[cells.source]"),
        ] {
            let config = doc(&cells, true);
            let program = config
                .program_canvas(config.solve_layout().unwrap())
                .unwrap();
            assert!(program.synthesized, "{cells}");
            assert_eq!(program.layout.cells[0].fit, FitMode::Fill);
            assert!((program.layout.cells[0].opacity - 1.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn the_reserved_cell_id_never_collides_with_an_authored_cell() {
        let cells = TWO.replace("id = \"c2\"", "id = \"me1.program\"");
        let config = doc(&cells, true);
        let program = config
            .program_canvas(config.solve_layout().unwrap())
            .unwrap();
        let id = program.cell_id.unwrap();
        assert!(config.cells.iter().all(|cell| cell.id != id), "{id}");
        assert!(id.starts_with("me1.program"));
    }

    #[test]
    fn overlays_with_a_switcher_are_still_refused() {
        let mut config = doc(TWO, true);
        let overlay: Overlay =
            toml::from_str("id = \"o\"\nkind = \"clock\"\ntarget = \"canvas\"\n")
                .expect("overlay fixture");
        config.overlays.push(overlay);
        let refusal = config
            .program_canvas(config.solve_layout().unwrap())
            .unwrap_err();
        assert_eq!(refusal, SwitcherCanvasRefusal::Overlays(1));
        assert!(refusal
            .operator_message()
            .starts_with("The switcher is on: overlays cannot be added"));
    }
}
