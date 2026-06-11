//! Integration tests for `Layout::validate`.
//!
//! Validation enforces the structural invariants a resolver/compositor relies
//! on: positive canvas geometry, positive rational cadence, and every cell's
//! normalized rectangle inside `0.0..=1.0` with positive extent. Overlap
//! between cells is explicitly allowed (`PiP` / picture-in-picture).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;

fn cell(x: f32, y: f32, w: f32, h: f32) -> Cell {
    Cell {
        x,
        y,
        w,
        h,
        z: 0,
        fit: FitMode::Contain,
        source: None,
        // Broadcast extras (crop/rotation) are additive and defaulted; spread
        // their defaults so this helper keeps exercising the original shape.
        ..Cell::default()
    }
}

fn canvas() -> Canvas {
    Canvas {
        width: 1920,
        height: 1080,
        fps_num: 60_000,
        fps_den: 1001,
    }
}

fn layout_with(cells: Vec<Cell>) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: canvas(),
        cells,
    }
}

#[test]
fn valid_full_canvas_cell_passes() {
    let l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    assert!(l.validate().is_ok());
}

#[test]
fn valid_empty_cells_passes() {
    // No cells is a valid (blank) canvas.
    let l = layout_with(vec![]);
    assert!(l.validate().is_ok());
}

#[test]
fn valid_overlapping_cells_allowed() {
    // PiP: a big cell with a small overlapping inset must validate.
    let l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0), cell(0.7, 0.7, 0.25, 0.25)]);
    assert!(l.validate().is_ok());
}

#[test]
fn invalid_zero_canvas_width_fails() {
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.width = 0;
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("width"), "message was: {msg}");
}

#[test]
fn invalid_zero_canvas_height_fails() {
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.height = 0;
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("height"), "message was: {msg}");
}

#[test]
fn invalid_zero_fps_den_fails() {
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.fps_den = 0;
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("fps_den"), "message was: {msg}");
}

#[test]
fn invalid_zero_fps_num_fails() {
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.fps_num = 0;
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("fps_num"), "message was: {msg}");
}

#[test]
fn invalid_negative_fps_num_fails() {
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.fps_num = -30;
    assert!(l.validate().is_err());
}

#[test]
fn cadence_returns_exact_rational() {
    // The seam the engine clock consumes returns the flat fields verbatim as an
    // exact rational (no float fps) — 60000/1001 must survive intact.
    let c = canvas();
    let cadence = c.cadence();
    assert_eq!(cadence, Rational::new(60_000, 1001));
    // Equal by value to the canonical NTSC 59.94 constant.
    assert_eq!(cadence, Rational::FPS_59_94);
}

#[test]
fn validate_rejects_invalid_cadence() {
    // A zero-denominator cadence is not a valid rational and must be rejected.
    let mut l = layout_with(vec![cell(0.0, 0.0, 1.0, 1.0)]);
    l.canvas.fps_den = 0;
    let err = l.validate();
    assert!(err.is_err());
    // The cadence accessor reflects the bad value and is itself invalid.
    assert!(!l.canvas.cadence().is_valid());
}

#[test]
fn invalid_cell_negative_origin_fails() {
    let l = layout_with(vec![cell(-0.1, 0.0, 0.5, 0.5)]);
    let msg = l.validate().unwrap_err().to_string();
    // Identify the offending cell index for the operator.
    assert!(msg.contains("cell 0"), "message was: {msg}");
}

#[test]
fn invalid_cell_zero_width_fails() {
    let l = layout_with(vec![cell(0.0, 0.0, 0.0, 0.5)]);
    assert!(l.validate().is_err());
}

#[test]
fn invalid_cell_zero_height_fails() {
    let l = layout_with(vec![cell(0.0, 0.0, 0.5, 0.0)]);
    assert!(l.validate().is_err());
}

#[test]
fn invalid_cell_exceeds_right_edge_fails() {
    // x + w > 1.0
    let l = layout_with(vec![cell(0.8, 0.0, 0.5, 0.5)]);
    assert!(l.validate().is_err());
}

#[test]
fn invalid_cell_exceeds_bottom_edge_fails() {
    // y + h > 1.0
    let l = layout_with(vec![cell(0.0, 0.8, 0.5, 0.5)]);
    assert!(l.validate().is_err());
}

#[test]
fn invalid_cell_nan_fails() {
    let l = layout_with(vec![cell(f32::NAN, 0.0, 0.5, 0.5)]);
    assert!(l.validate().is_err());
}

#[test]
fn second_cell_reported_by_index() {
    let l = layout_with(vec![cell(0.0, 0.0, 0.5, 0.5), cell(0.0, 0.0, 2.0, 0.5)]);
    let msg = l.validate().unwrap_err().to_string();
    assert!(msg.contains("cell 1"), "message was: {msg}");
}
