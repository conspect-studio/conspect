//! Checked first-slice transition geometry. Mix of overlapping scenes must
//! never flatten both tile lists: that would expose the scene behind a foreground
//! tile. This control-side validator rejects it before enqueueing an AUTO.

use multiview_core::layout::{Cell, Layout};
use thiserror::Error;

/// Maximum total input tiles one transition may admit (GPU tile-array bound).
pub const MAX_SWITCHER_TILES: usize = 64;

/// Reason a scene pair cannot be armed for a flat-list first-slice mix.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArmError {
    /// The two scene tile sets exceed one compositor's GPU tile budget.
    #[error("mix has {count} tiles, exceeding the bound of {MAX_SWITCHER_TILES}")]
    TileBudgetExceeded {
        /// Total number of cells in the two scenes.
        count: usize,
    },
    /// One scene has overlapping foreground and background tiles; it must be
    /// precomposited before mixing, which is not yet available.
    #[error(
        "scene {scene} has overlapping cells {first} and {second}; mix requires precomposition"
    )]
    OverlappingScene {
        /// Which named bus had the internal overlap.
        scene: &'static str,
        /// First overlapping cell index.
        first: usize,
        /// Second overlapping cell index.
        second: usize,
    },
    /// PGM and PVW must represent the same clock signal.
    #[error("PGM and PVW canvases must have identical geometry and cadence")]
    IncompatibleCanvas,
}

/// Validate the first-slice AUTO mix at arm time, before enqueue. A Cut does
/// not flatten two scenes and therefore does not require this restriction.
///
/// # Errors
/// Returns a typed geometry, tile-limit or cadence refusal; the current PGM
/// and audio state are untouched.
pub fn validate_mix(program: &Layout, preview: &Layout) -> Result<(), ArmError> {
    if program.canvas != preview.canvas {
        return Err(ArmError::IncompatibleCanvas);
    }
    let count = program.cells.len().saturating_add(preview.cells.len());
    if count > MAX_SWITCHER_TILES {
        return Err(ArmError::TileBudgetExceeded { count });
    }
    for (scene, layout) in [("program", program), ("preview", preview)] {
        for (i, first) in layout.cells.iter().enumerate() {
            for (j, second) in layout.cells.iter().enumerate().skip(i + 1) {
                if rects_overlap(first, second, layout.canvas.width, layout.canvas.height) {
                    return Err(ArmError::OverlappingScene {
                        scene,
                        first: i,
                        second: j,
                    });
                }
            }
        }
    }
    Ok(())
}

fn rects_overlap(a: &Cell, b: &Cell, width: u32, height: u32) -> bool {
    fn edge(fraction: f32, extent: u32) -> u32 {
        if !fraction.is_finite() || fraction <= 0.0 {
            return 0;
        }
        // Same integer-pixel floor convention as the compositor drive; its
        // validated [0,1] coordinates cannot exceed `extent`.
        let value = f64::from(fraction) * f64::from(extent);
        let mut lo = 0;
        let mut hi = extent;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if f64::from(mid) <= value {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }
    let ax = edge(a.x, width);
    let ay = edge(a.y, height);
    let ar = edge(a.x + a.w, width);
    let ab = edge(a.y + a.h, height);
    let bx = edge(b.x, width);
    let by = edge(b.y, height);
    let br = edge(b.x + b.w, width);
    let bb = edge(b.y + b.h, height);
    ax < br && bx < ar && ay < bb && by < ab
}
