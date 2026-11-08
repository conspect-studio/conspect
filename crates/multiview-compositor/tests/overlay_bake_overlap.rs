//! Overlapping overlay footprints must retain the CPU oracle's source-over pixels.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use multiview_compositor::overlay::subpass::{
    apply_overlays_to_nv12, apply_overlays_to_nv12_reference, OverlayColor, OverlayDrawList,
    OverlayPrimitive, OverlayRect,
};
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};

#[test]
fn coalesced_odd_edges_and_all_primitives_match_the_reference() {
    let canvas = CanvasColor::default();
    let width = 96_usize;
    let height = 64_usize;
    let y: Vec<u8> = (0..width * height)
        .map(|i| (16 + (i * 13 + i / width * 7) % 220) as u8)
        .collect();
    let uv: Vec<u8> = (0..width * height / 2)
        .map(|i| (16 + (i * 19) % 220) as u8)
        .collect();
    let input = Nv12Image::new(width as u32, height as u32, y, uv, canvas.output_tag()).unwrap();
    let mut list = OverlayDrawList::new();
    let green = OverlayColor::new(0.1, 0.9, 0.3, 0.65);
    for i in 0..8 {
        list.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(11 + i, 9 + i, 28, 21),
            corner_radius: 3,
            color: green,
        });
    }
    list.push(OverlayPrimitive::Glyph {
        dest_x: 17,
        dest_y: 13,
        width: 15,
        height: 9,
        coverage: (0..135).map(|i| (i * 41 % 256) as u8).collect(),
        color: OverlayColor::opaque(1.0, 0.2, 0.7),
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(19, 29, 21, 3),
        color: green,
    });
    list.push(OverlayPrimitive::Stroke {
        x0: 20.0,
        y0: 12.0,
        x1: 35.0,
        y1: 27.0,
        half_thickness: 2.0,
        color: green,
    });
    list.push(OverlayPrimitive::Ring {
        cx: 31.0,
        cy: 22.0,
        outer_radius: 7.0,
        thickness: 3.0,
        color: green,
    });
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(27, 27, 13, 9),
        src_width: 2,
        src_height: 2,
        rgba: vec![
            20, 140, 70, 180, 20, 40, 70, 220, 80, 30, 20, 160, 90, 20, 10, 200,
        ],
        alpha: 0.7,
    });
    // A separate clipped caption tests unchanged passthrough between regions.
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(-5, 53, 15, 14),
        src_width: 1,
        src_height: 1,
        rgba: vec![120, 60, 25, 170],
        alpha: 0.8,
    });
    let actual = apply_overlays_to_nv12(&input, &list, canvas).unwrap();
    let oracle = apply_overlays_to_nv12_reference(&input, &list, canvas).unwrap();
    for row in 0..height {
        for col in 0..width {
            let covered = (20..34).contains(&col) && (16..28).contains(&row)
                || (0..8).contains(&col) && (54..62).contains(&row);
            let outside = (64..96).contains(&col);
            let idx = row * width + col;
            if covered || outside {
                assert_eq!(
                    actual.y_plane()[idx],
                    if covered {
                        oracle.y_plane()[idx]
                    } else {
                        input.y_plane()[idx]
                    },
                    "Y ({col},{row})"
                );
            }
            if row % 2 == 0 && col % 2 == 0 {
                let uv = row / 2 * width + col;
                if covered || outside {
                    for channel in 0..2 {
                        assert_eq!(
                            actual.uv_plane()[uv + channel],
                            if covered {
                                oracle.uv_plane()[uv + channel]
                            } else {
                                input.uv_plane()[uv + channel]
                            },
                            "UV ({col},{row}) channel {channel}"
                        );
                    }
                }
            }
        }
    }
}
