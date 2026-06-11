//! Integration tests for per-cell [`opacity`](multiview_core::layout::Cell::opacity).
//!
//! Per-tile opacity is the seam that unlocks the compositor's premultiplied
//! linear-light `over` blend for overlapping/z-stacked tiles (cross-fade /
//! PiP-ghost). It is **additive and defaulted** to a fully opaque `1.0`: a cell
//! built without it (or deserialized from a document predating it) behaves
//! exactly as before (a hard-cover). [`Layout::validate`] constrains it to the
//! straight-alpha unit interval `[0.0, 1.0]`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_core::layout::{Canvas, Cell, FitMode, Layout};

fn canvas() -> Canvas {
    Canvas {
        width: 1920,
        height: 1080,
        fps_num: 60_000,
        fps_den: 1001,
    }
}

/// A full-canvas opaque cell with an explicit opacity, spreading defaults for
/// the broadcast extras.
fn cell_with_opacity(opacity: f32) -> Cell {
    Cell {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
        z: 0,
        fit: FitMode::Contain,
        source: None,
        opacity,
        ..Cell::default()
    }
}

fn layout_with(cells: Vec<Cell>) -> Layout {
    Layout {
        name: "opacity".to_owned(),
        canvas: canvas(),
        cells,
    }
}

#[test]
fn default_cell_is_fully_opaque() {
    // The default must be a fully opaque hard-cover so existing behaviour is
    // unchanged: a cell built without opacity composites at 1.0.
    let c = Cell::default();
    assert_eq!(c.opacity, 1.0);
}

#[test]
fn opacity_defaults_to_one_when_absent_in_document() {
    // A JSON document predating the field must deserialize with opacity == 1.0
    // (the `#[serde(default)]` seam), and validate.
    let json = r#"{"x":0.0,"y":0.0,"w":0.5,"h":0.5,"z":0,"fit":"Contain","source":null}"#;
    let c: Cell = serde_json::from_str(json).unwrap();
    assert_eq!(c.opacity, 1.0);
    let l = layout_with(vec![c]);
    assert!(l.validate().is_ok());
}

#[test]
fn opacity_round_trips_when_present() {
    // A document that sets opacity must preserve the exact value across a
    // serialize -> deserialize round-trip.
    let original = cell_with_opacity(0.25);
    let json = serde_json::to_string(&original).unwrap();
    let back: Cell = serde_json::from_str(&json).unwrap();
    assert_eq!(back.opacity, 0.25);
    // And the serialized form actually carries the value (not lost).
    assert!(
        json.contains("0.25"),
        "serialized cell must carry the opacity value, got {json}"
    );
}

#[test]
fn validate_accepts_opacity_boundaries() {
    // The closed interval endpoints are both legal: 0.0 (fully transparent) and
    // 1.0 (fully opaque).
    assert!(layout_with(vec![cell_with_opacity(0.0)]).validate().is_ok());
    assert!(layout_with(vec![cell_with_opacity(1.0)]).validate().is_ok());
    assert!(layout_with(vec![cell_with_opacity(0.5)]).validate().is_ok());
}

#[test]
fn validate_rejects_opacity_above_one() {
    let err = layout_with(vec![cell_with_opacity(1.5)]).validate();
    assert!(
        err.is_err(),
        "opacity above 1.0 must be rejected, got {err:?}"
    );
}

#[test]
fn validate_rejects_opacity_below_zero() {
    let err = layout_with(vec![cell_with_opacity(-0.1)]).validate();
    assert!(
        err.is_err(),
        "opacity below 0.0 must be rejected, got {err:?}"
    );
}

#[test]
fn validate_rejects_non_finite_opacity() {
    assert!(
        layout_with(vec![cell_with_opacity(f32::NAN)])
            .validate()
            .is_err(),
        "NaN opacity must be rejected"
    );
    assert!(
        layout_with(vec![cell_with_opacity(f32::INFINITY)])
            .validate()
            .is_err(),
        "infinite opacity must be rejected"
    );
}
