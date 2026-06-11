//! Frame metadata (pixel storage is backend-specific and lives in other crates).
use crate::{color::ColorInfo, pixel::PixelFormat, time::MediaTime};

/// Metadata describing one video frame on the internal timeline.
///
/// The actual pixel storage (host buffer, CUDA surface, `IOSurface`, wgpu
/// texture, …) is backend-specific and lives in the feature-gated crates; this
/// type carries only the pure-Rust description that travels alongside it.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    /// Presentation time on the internal monotonic timeline.
    pub pts: MediaTime,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: PixelFormat,
    /// Color description (four axes).
    pub color: ColorInfo,
}
