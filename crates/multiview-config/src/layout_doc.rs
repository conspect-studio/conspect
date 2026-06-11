//! The stored **named-layout document** — the `{ canvas, layout, cells }` body
//! the control plane's layouts repository holds and the `WebUI` layout editor
//! saves (ADR-W019).
//!
//! This is the typed, solvable view of one stored layout: the same `layout` +
//! `cells` schema a full [`crate::MultiviewConfig`] carries, with a **minimal
//! canvas** ([`LayoutCanvas`]: `width`/`height`/`fps` only). The editor saves
//! exactly that minimal canvas; the seeded working-layout body carries the full
//! authored [`crate::Canvas`] — both deserialize here (unknown canvas fields
//! such as `pixel_format`/`background`/`color` are ignored: they are pinned at
//! session start and cannot change on a live apply, ADR-R004).
//!
//! The control plane parses + solves a stored body **at the apply-layout route**
//! (off the engine hot path) via [`LayoutDocument::solve_named`]; the engine's
//! frame-boundary drain then only swaps the already-solved
//! [`multiview_core::layout::Layout`] (invariants #1/#10).

use multiview_core::layout::Layout as CoreLayout;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::failover::FailoverSlate;
use crate::schema::{Cell, Fps, Layout};
use crate::{grid, MultiviewConfig};

/// The minimal canvas a stored layout body must declare: geometry + cadence.
///
/// `width`/`height`/`fps` are exactly the axes that are **pinned** for the life
/// of an output session (ADR-R004) — they are required so a live apply can be
/// classified (Class-1 iff they match the running canvas). Other authored
/// canvas fields (`pixel_format`, `background`, `color`) are accepted and
/// ignored: they cannot change on a live apply either, and the editor never
/// writes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LayoutCanvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Output cadence as an exact rational (parsed from a `"num/den"` string —
    /// never a float, invariant #3).
    pub fps: Fps,
}

impl LayoutCanvas {
    /// Build a pinned-canvas value (the type is `#[non_exhaustive]`, so
    /// downstream crates construct it here — e.g. the control plane capturing
    /// the running session's canvas snapshot at seed time, ADR-W019).
    #[must_use]
    pub const fn new(width: u32, height: u32, fps: Fps) -> Self {
        Self { width, height, fps }
    }
}

// NOTE (ADR-W019 MINOR-3): `LayoutCanvas`'s derived `PartialEq` is already a
// same-signal comparison BY VALUE — `Fps` delegates to
// [`multiview_core::time::Rational`]'s `PartialEq`, which cross-multiplies in
// `i128` — so a non-reduced `50/2` equals `25/1` and the Class-1 gate never
// falsely refuses an equivalent cadence (pinned by the
// `cadence_equality_is_by_value` test below).

/// The serde default for an omitted `layout` strategy: absolute placement
/// (per-cell `rect`), the only kind the `WebUI` editor writes.
fn default_strategy() -> Layout {
    Layout::Absolute
}

/// One stored named-layout document: `{ canvas, layout, cells }`.
///
/// Deserializes both the seeded working-layout body (full authored canvas, any
/// layout strategy) and the `WebUI` editor's minimal absolute body. Solve it with
/// [`LayoutDocument::solve_named`]; read the parallel per-cell extras the
/// engine drive needs with [`LayoutDocument::cell_ids`] /
/// [`LayoutDocument::cell_slates`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LayoutDocument {
    /// The canvas the layout was authored for (pinned axes only).
    pub canvas: LayoutCanvas,
    /// The placement strategy (grid / absolute / preset). Defaults to absolute
    /// when omitted (the editor's shape always writes it; defensive default).
    #[serde(default = "default_strategy")]
    pub layout: Layout,
    /// Cells (tiles) and their source bindings, in declaration order.
    #[serde(default)]
    pub cells: Vec<Cell>,
}

impl LayoutDocument {
    /// Parse a stored layout body (the repository's opaque JSON document).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::JsonParse`] naming the offending path when the
    /// body is not a `{ canvas, layout, cells }` document.
    pub fn from_body(body: &serde_json::Value) -> Result<Self, ConfigError> {
        serde_json::from_value(body.clone()).map_err(|e| ConfigError::JsonParse(e.to_string()))
    }

    /// Solve this document into a validated [`CoreLayout`] named `name` (the
    /// stored layout id, so the active layout is attributable).
    ///
    /// Grid cells are placed by solving the CSS grid at the document's canvas
    /// size; absolute cells use their declared `rect`. On top of the structural
    /// solve this enforces what the engine drive needs from a *stored* layout:
    /// unique, non-empty cell ids (the O(1) re-point address space) and the core
    /// geometry invariants ([`CoreLayout::validate`] — rects in the unit square,
    /// usable cadence, opacity in range).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Grid`] for an unsolvable grid, or
    /// [`ConfigError::Validation`] for a duplicate/empty cell id, a cell without
    /// a placement, an unknown grid area, or invalid solved geometry.
    pub fn solve_named(&self, name: &str) -> Result<CoreLayout, ConfigError> {
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::with_capacity(self.cells.len());
        for cell in &self.cells {
            if cell.id.is_empty() {
                return Err(ConfigError::Validation(
                    "a stored-layout cell has an empty id".to_owned(),
                ));
            }
            if !seen.insert(cell.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate cell id {:?} in stored layout",
                    cell.id
                )));
            }
        }

        let grid_rects = match self.layout.as_grid_layout()? {
            Some(grid) => Some(grid::solve(&grid, self.canvas.width, self.canvas.height)?),
            None => None,
        };

        let mut cells = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            cells.push(MultiviewConfig::solve_cell(cell, grid_rects.as_deref())?);
        }

        let cadence = self.canvas.fps.rational();
        let solved = CoreLayout {
            name: name.to_owned(),
            canvas: multiview_core::layout::Canvas {
                width: self.canvas.width,
                height: self.canvas.height,
                fps_num: cadence.num,
                fps_den: cadence.den,
            },
            cells,
        };
        solved.validate().map_err(|e| match e {
            multiview_core::Error::Config(msg) => ConfigError::Validation(msg),
            other => ConfigError::Validation(other.to_string()),
        })?;
        Ok(solved)
    }

    /// The cell ids in declaration order — exactly `solve_named`'s cell order —
    /// the parallel id list the engine drive's `set_cell_ids` consumes for O(1)
    /// re-point addressing.
    #[must_use]
    pub fn cell_ids(&self) -> Vec<Option<String>> {
        self.cells.iter().map(|c| Some(c.id.clone())).collect()
    }

    /// The per-cell `on_loss` failover-slate policy in declaration order
    /// (parallel to [`LayoutDocument::cell_ids`]).
    #[must_use]
    pub fn cell_slates(&self) -> Vec<FailoverSlate> {
        self.cells.iter().map(|c| c.on_loss).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The minimal absolute body the `WebUI` editor saves.
    fn editor_body() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "canvas": { "width": 320, "height": 240, "fps": "25/1" },
            "layout": { "kind": "absolute" },
            "cells": [
                {
                    "id": "full",
                    "label": "Full",
                    "rect": { "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0 },
                    "z": 0,
                    "rotation": 0,
                    "source": { "input_id": "in_a" }
                }
            ]
        })
    }

    #[test]
    fn editor_minimal_body_parses_and_solves() {
        let doc = LayoutDocument::from_body(&editor_body()).expect("editor body parses");
        let solved = doc.solve_named("wall-a").expect("editor body solves");
        assert_eq!(solved.name, "wall-a");
        assert_eq!(solved.canvas.width, 320);
        assert_eq!(solved.canvas.fps_num, 25);
        assert_eq!(solved.cells.len(), 1);
        assert_eq!(
            solved.cells.first().unwrap().source.as_deref(),
            Some("in_a")
        );
        assert_eq!(doc.cell_ids(), vec![Some("full".to_owned())]);
        assert_eq!(doc.cell_slates(), vec![FailoverSlate::Bars]);
    }

    #[test]
    fn seeded_grid_body_parses_and_solves() {
        // The seeded working-layout shape: full authored canvas + grid strategy.
        let body = serde_json::json!({
            "canvas": {
                "width": 640, "height": 480, "fps": "30000/1001",
                "pixel_format": "nv12", "background": "#101014",
                "color": { "profile": "sdr-bt709-limited" }
            },
            "layout": {
                "kind": "grid",
                "columns": ["1fr", "1fr"], "rows": ["1fr"], "areas": ["a b"]
            },
            "cells": [
                { "id": "l", "area": "a", "source": { "input_id": "x" } },
                { "id": "r", "area": "b", "on_loss": { "slate": "black" }, "source": {} }
            ]
        });
        let doc = LayoutDocument::from_body(&body).expect("seeded body parses");
        let solved = doc.solve_named("schema_v1").expect("grid solves");
        assert_eq!(solved.cells.len(), 2);
        let left = solved.cells.first().unwrap();
        assert!((left.w - 0.5).abs() < 1e-6, "grid splits the row in half");
        assert_eq!(
            doc.cell_slates(),
            vec![FailoverSlate::Bars, FailoverSlate::Black]
        );
    }

    #[test]
    fn duplicate_cell_ids_are_rejected() {
        let mut body = editor_body();
        let cell = body["cells"][0].clone();
        body["cells"].as_array_mut().unwrap().push(cell);
        let doc = LayoutDocument::from_body(&body).expect("parses");
        let err = doc.solve_named("dup").expect_err("duplicate ids must fail");
        assert!(err.to_string().contains("duplicate cell id"));
    }

    #[test]
    fn unknown_grid_area_is_rejected() {
        let body = serde_json::json!({
            "canvas": { "width": 320, "height": 240, "fps": "25/1" },
            "layout": { "kind": "grid", "columns": ["1fr"], "rows": ["1fr"], "areas": ["a"] },
            "cells": [ { "id": "x", "area": "nope", "source": {} } ]
        });
        let doc = LayoutDocument::from_body(&body).expect("parses");
        assert!(doc.solve_named("bad").is_err(), "unknown area must fail");
    }

    #[test]
    fn cadence_equality_is_by_value() {
        // ADR-W019 MINOR-3: a non-reduced cadence is the SAME pinned signal.
        let a = LayoutCanvas::new(320, 240, "25/1".parse().expect("fps"));
        let b = LayoutCanvas::new(320, 240, "50/2".parse().expect("fps"));
        let c = LayoutCanvas::new(320, 240, "30000/1001".parse().expect("fps"));
        assert_eq!(a, b, "50/2 == 25/1 by value (cross-multiplied)");
        assert_ne!(a, c, "30000/1001 is a genuinely different cadence");
    }

    #[test]
    fn unparseable_body_is_an_error() {
        let body = serde_json::json!({ "canvas": { "width": "wide" }, "cells": [] });
        assert!(LayoutDocument::from_body(&body).is_err());
    }
}
