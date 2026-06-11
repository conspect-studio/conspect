//! Pixel formats. NV12 is the canonical working format throughout the pipeline.
//!
//! Per invariant #5 (NV12-throughout) frames stay in NV12 (1.5 B/px); RGBA is
//! never materialized per tile.
use serde::{Deserialize, Serialize};

/// Supported pixel formats. Frames stay in NV12; RGBA is avoided per-tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 8-bit 4:2:0 semi-planar (canonical working format).
    #[default]
    Nv12,
    /// 10-bit 4:2:0 semi-planar.
    P010,
    /// Packed 8-bit RGBA (do not materialize per tile).
    Rgba,
}
