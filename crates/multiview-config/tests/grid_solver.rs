//! Unit tests for the pure CSS-grid solver (fr / px / % tracks + gaps + areas).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::grid::{self, GridLayout, Track};

/// Look up the solved rect for a named area.
fn rect_for<'a>(rects: &'a [grid::AreaRect], name: &str) -> &'a grid::AreaRect {
    rects
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no rect for area {name}"))
}

fn approx(a: f32, b: f32) {
    assert!((a - b).abs() < 1e-5, "expected {b}, got {a}");
}

#[test]
fn two_equal_columns_no_gap() {
    let g = GridLayout {
        columns: vec![Track::Fr(1.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec!["a b".to_owned()],
    };
    let rects = grid::solve(&g, 1000, 1000).unwrap();
    let a = rect_for(&rects, "a");
    approx(a.x, 0.0);
    approx(a.y, 0.0);
    approx(a.w, 0.5);
    approx(a.h, 1.0);
    let b = rect_for(&rects, "b");
    approx(b.x, 0.5);
    approx(b.y, 0.0);
    approx(b.w, 0.5);
    approx(b.h, 1.0);
}

#[test]
fn two_equal_columns_with_gap() {
    // 1000px wide, two 1fr columns, 100px gap.
    // available = 900, each cell = 450px.
    // col0: x=0   w=450  -> 0.0  .. 0.45
    // col1: x=550 w=450  -> 0.55 .. 0.45
    let g = GridLayout {
        columns: vec![Track::Fr(1.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0)],
        gap: 100,
        row_gap: None,
        column_gap: None,
        areas: vec!["a b".to_owned()],
    };
    let rects = grid::solve(&g, 1000, 1000).unwrap();
    let a = rect_for(&rects, "a");
    approx(a.x, 0.0);
    approx(a.w, 0.45);
    let b = rect_for(&rects, "b");
    approx(b.x, 0.55);
    approx(b.w, 0.45);
}

#[test]
fn fixed_px_and_fr_mix() {
    // 1000px wide: [200px, 1fr], no gap. fr cell takes 800px.
    let g = GridLayout {
        columns: vec![Track::Px(200.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec!["a b".to_owned()],
    };
    let rects = grid::solve(&g, 1000, 1000).unwrap();
    let a = rect_for(&rects, "a");
    approx(a.x, 0.0);
    approx(a.w, 0.2);
    let b = rect_for(&rects, "b");
    approx(b.x, 0.2);
    approx(b.w, 0.8);
}

#[test]
fn spanning_area_covers_its_tracks_plus_interior_gap() {
    // 3fr/1fr columns, 3 equal rows, gap 8, big spans rows 0-1, cols 0-1
    // (the 1+5 layout). Confirm the spanning area is a single contiguous rect.
    let g = GridLayout {
        columns: vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec![
            "big big s1".to_owned(),
            "big big s2".to_owned(),
            "s3  s4  s5".to_owned(),
        ],
    };
    let rects = grid::solve(&g, 900, 900).unwrap();
    let big = rect_for(&rects, "big");
    // big spans 2 of 3 cols and 2 of 3 rows with no gap.
    approx(big.x, 0.0);
    approx(big.y, 0.0);
    approx(big.w, 2.0 / 3.0);
    approx(big.h, 2.0 / 3.0);
    let s1 = rect_for(&rects, "s1");
    approx(s1.x, 2.0 / 3.0);
    approx(s1.w, 1.0 / 3.0);
}

#[test]
fn percent_track_consumes_fraction_of_canvas() {
    // [25%, 1fr] of 1000px -> 250px, 750px.
    let g = GridLayout {
        columns: vec![Track::Percent(25.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec!["a b".to_owned()],
    };
    let rects = grid::solve(&g, 1000, 1000).unwrap();
    approx(rect_for(&rects, "a").w, 0.25);
    approx(rect_for(&rects, "b").w, 0.75);
}

#[test]
fn track_parsing_accepts_fr_px_percent() {
    assert_eq!("3fr".parse::<Track>().unwrap(), Track::Fr(3.0));
    assert_eq!("200px".parse::<Track>().unwrap(), Track::Px(200.0));
    assert_eq!("25%".parse::<Track>().unwrap(), Track::Percent(25.0));
    assert!("garbage".parse::<Track>().is_err());
}

#[test]
fn non_rectangular_area_is_rejected() {
    // "a" forms an L-shape -> not a rectangle -> error.
    let g = GridLayout {
        columns: vec![Track::Fr(1.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0), Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec!["a a".to_owned(), "a b".to_owned()],
    };
    assert!(grid::solve(&g, 1000, 1000).is_err());
}

#[test]
fn ragged_area_rows_are_rejected() {
    // Row token counts differ from the column count.
    let g = GridLayout {
        columns: vec![Track::Fr(1.0), Track::Fr(1.0)],
        rows: vec![Track::Fr(1.0)],
        gap: 0,
        row_gap: None,
        column_gap: None,
        areas: vec!["a b c".to_owned()],
    };
    assert!(grid::solve(&g, 1000, 1000).is_err());
}
