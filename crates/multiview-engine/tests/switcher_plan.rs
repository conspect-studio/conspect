#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_engine::switcher_plan::{validate_mix, ArmError};

fn scene(rects: &[(f32, f32, f32, f32)]) -> Layout {
    Layout {
        name: "me1".into(),
        canvas: Canvas {
            width: 1920,
            height: 1080,
            fps_num: 30000,
            fps_den: 1001,
        },
        cells: rects
            .iter()
            .map(|&(x, y, w, h)| Cell {
                x,
                y,
                w,
                h,
                ..Cell::default()
            })
            .collect(),
    }
}

#[test]
fn internally_overlapping_scene_is_rejected_before_arm() {
    let program = scene(&[(0.0, 0.0, 1.0, 1.0), (0.2, 0.2, 0.3, 0.3)]);
    let preview = scene(&[(0.0, 0.0, 1.0, 1.0)]);
    assert_eq!(
        validate_mix(&program, &preview).unwrap_err(),
        ArmError::OverlappingScene {
            scene: "program",
            first: 0,
            second: 1
        }
    );
}

#[test]
fn edge_adjacent_tiles_are_not_false_overlaps() {
    let program = scene(&[(0.0, 0.0, 0.5, 1.0), (0.5, 0.0, 0.5, 1.0)]);
    let preview = scene(&[(0.0, 0.0, 1.0, 1.0)]);
    validate_mix(&program, &preview).unwrap();
}

#[test]
fn scenes_overlapping_each_other_are_allowed_when_each_is_preordered() {
    let program = scene(&[(0.0, 0.0, 1.0, 1.0)]);
    let preview = scene(&[(0.0, 0.0, 1.0, 1.0)]);
    validate_mix(&program, &preview).unwrap();
}

#[test]
fn too_many_tiles_or_mismatched_canvas_are_refused() {
    let full = (0..65).map(|_| (0.0, 0.0, 0.0, 0.0)).collect::<Vec<_>>();
    assert_eq!(
        validate_mix(&scene(&full), &scene(&[])),
        Err(ArmError::TileBudgetExceeded { count: 65 })
    );
    let mut incompatible = scene(&[]);
    incompatible.canvas.fps_den = 1;
    assert_eq!(
        validate_mix(&scene(&[]), &incompatible),
        Err(ArmError::IncompatibleCanvas)
    );
}
