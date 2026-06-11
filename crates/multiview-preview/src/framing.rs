//! The MJPEG / JPEG **framing data model** for preview transports.
//!
//! This module owns the *pure-Rust* transport shaping the preview brief (§4)
//! mandates for the cheap, default grid/at-a-glance views:
//!
//! * [`Snapshot`] — a single low-res JPEG with its dimensions and a
//!   content-derived `ETag` for conditional GETs (the polling thumbnail path).
//! * [`MjpegStream`] — the `multipart/x-mixed-replace` boundary framing for a
//!   live thumbnail stream: it produces the `Content-Type` header value and the
//!   per-frame multipart parts, byte-for-byte.
//! * [`ThumbnailPlan`] — the clamped fps / dimension caps that keep preview
//!   "cheap by design" (the brief's thumbnail-rate model).
//! * [`JpegEncoder`] — the trait the actual NV12→JPEG encoder implements, with a
//!   dependency-free [`StubJpegEncoder`] default that emits a syntactically
//!   valid minimal JPEG so the transport layer is fully testable without a
//!   native codec. A real backend (`turbojpeg`/`zune-jpeg`) plugs in behind the
//!   same trait.
//!
//! None of this touches the protected output path; it shapes bytes the control
//! plane will hand to clients (invariant #10).
use std::fmt::Write as _;

use thiserror::Error;

/// Errors from JPEG encoding / framing.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JpegError {
    /// The supplied plane buffer is smaller than `width * height * 3 / 2`
    /// (NV12: a full luma plane plus a half-size interleaved chroma plane).
    #[error("nv12 buffer too small: have {have} bytes, need {need} for {width}x{height}")]
    BufferTooSmall {
        /// Bytes actually provided.
        have: usize,
        /// Bytes required for the declared geometry.
        need: usize,
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },

    /// NV12 requires even dimensions (2x2 chroma subsampling).
    #[error("nv12 dimensions must be even; got {width}x{height}")]
    OddDimensions {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },

    /// JPEG quality must be in `1..=100`.
    #[error("jpeg quality must be 1..=100; got {0}")]
    InvalidQuality(u8),

    /// The underlying JPEG encoder rejected otherwise-valid input (e.g. a
    /// geometry the codec cannot represent, or an internal write failure). The
    /// inputs were already geometry/quality-validated, so this is rare; it is
    /// surfaced typed rather than panicking on the preview path.
    #[error("jpeg encode failed")]
    Encode,
}

/// Validate NV12 geometry + buffer + quality common to every encoder.
///
/// # Errors
///
/// Returns [`JpegError::OddDimensions`], [`JpegError::InvalidQuality`], or
/// [`JpegError::BufferTooSmall`] as appropriate.
pub(crate) fn validate_nv12(
    plane: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<(), JpegError> {
    if width % 2 != 0 || height % 2 != 0 || width == 0 || height == 0 {
        return Err(JpegError::OddDimensions { width, height });
    }
    if quality == 0 || quality > 100 {
        return Err(JpegError::InvalidQuality(quality));
    }
    // need = w*h + 2 * (w/2 * h/2) = w*h*3/2, computed without overflow/`as`.
    let luma = usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(w, h)| w.checked_mul(h));
    let Some(luma) = luma else {
        // Geometry too large to index a host buffer; treat as "buffer too small"
        // since no real plane can satisfy it.
        return Err(JpegError::BufferTooSmall {
            have: plane.len(),
            need: usize::MAX,
            width,
            height,
        });
    };
    let chroma = luma / 2; // luma is even (w,h even) so this is exact.
    let need = luma.saturating_add(chroma);
    if plane.len() < need {
        return Err(JpegError::BufferTooSmall {
            have: plane.len(),
            need,
            width,
            height,
        });
    }
    Ok(())
}

/// Encodes an NV12 thumbnail plane into JPEG bytes for a preview transport.
///
/// The real implementation lives behind a hardware/CPU codec; this trait keeps
/// the framing/transport model independent of it (and testable via
/// [`StubJpegEncoder`]).
pub trait JpegEncoder {
    /// Encode an NV12 `plane` of `width`x`height` at `quality` (`1..=100`) to
    /// JPEG bytes.
    ///
    /// The `plane` is `width*height` bytes of luma followed by
    /// `width*height/2` bytes of interleaved `CbCr` (NV12, 1.5 B/px).
    ///
    /// # Errors
    ///
    /// Returns [`JpegError`] when the geometry is odd, the quality is out of
    /// range, or the buffer is too small for the declared geometry.
    fn encode_nv12(
        &self,
        plane: &[u8],
        width: u32,
        height: u32,
        quality: u8,
    ) -> Result<Vec<u8>, JpegError>;
}

/// A dependency-free **stub** [`JpegEncoder`] that emits a syntactically valid
/// minimal JPEG (SOI … EOI) without performing real DCT encoding.
///
/// It exists so the framing/transport layer (and the auto-stop/refcount logic)
/// can be exercised end-to-end with no native codec in the default build. It
/// still performs the same input validation a real encoder would, so callers
/// get the same typed errors. Swap in a real `turbojpeg`/`zune-jpeg` backend
/// behind the [`JpegEncoder`] trait when the compositor produces real pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubJpegEncoder;

impl JpegEncoder for StubJpegEncoder {
    fn encode_nv12(
        &self,
        plane: &[u8],
        width: u32,
        height: u32,
        quality: u8,
    ) -> Result<Vec<u8>, JpegError> {
        validate_nv12(plane, width, height, quality)?;
        // Minimal but well-formed JPEG container: SOI (FFD8) + a comment segment
        // (APP/COM) noting it is a preview stub + EOI (FFD9). A real decoder
        // accepts SOI/EOI framing; the comment makes the stub self-identifying.
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&[0xFF, 0xD8]); // SOI
                                              // COM marker (FFFE) with a short payload (length includes the 2 length
                                              // bytes). Payload: "multiview-preview-stub".
        let comment = b"multiview-preview-stub";
        let seg_len = comment.len().saturating_add(2);
        if let Ok(len16) = u16::try_from(seg_len) {
            out.extend_from_slice(&[0xFF, 0xFE]);
            out.extend_from_slice(&len16.to_be_bytes());
            out.extend_from_slice(comment);
        }
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        Ok(out)
    }
}

/// A single preview JPEG with its source dimensions and a content-derived `ETag`.
///
/// Returned by the snapshot endpoint (`/preview/snapshot.jpg`): the cheapest
/// preview surface (lazy grid cells, polling, alert thumbs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    etag: String,
}

impl Snapshot {
    /// Build a snapshot from encoded JPEG `bytes` of the given dimensions.
    ///
    /// The `ETag` is derived from the bytes (a strong, content-addressed tag), so
    /// identical content always yields an identical `ETag` for conditional `GET`s.
    #[must_use]
    pub fn new(width: u32, height: u32, bytes: Vec<u8>) -> Self {
        let etag = strong_etag(&bytes);
        Self {
            width,
            height,
            bytes,
            etag,
        }
    }

    /// The snapshot width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The snapshot height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The encoded JPEG bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The fixed MIME content type (`image/jpeg`).
    #[must_use]
    pub const fn content_type(&self) -> &'static str {
        "image/jpeg"
    }

    /// A strong, content-derived `ETag` value (quoted), suitable for an
    /// `ETag:` response header and `If-None-Match` conditional `GET`s.
    #[must_use]
    pub fn etag(&self) -> &str {
        &self.etag
    }
}

/// Compute a strong, quoted `ETag` (a hex digest) from content bytes.
///
/// Uses a stable, dependency-free FNV-1a 64-bit hash mixed with the length — it
/// is not a cryptographic digest (the token MAC handles security); it only
/// needs to be content-addressed and collision-resistant enough to gate HTTP
/// caches. Distinct content reliably yields a distinct tag.
fn strong_etag(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Mix the length so trivial prefix collisions differ.
    hash ^= u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hash = hash.wrapping_mul(FNV_PRIME);
    let mut s = String::with_capacity(18);
    s.push('"');
    let _ = write!(s, "{hash:016x}");
    s.push('"');
    s
}

/// The clamped fps / dimension plan for a thumbnail tap.
///
/// Preview is cheap-by-design (brief §3): client requests for fps and maximum
/// dimension are clamped to hard caps so a viewer can never drive the tap
/// faster or larger than the preview budget allows, and a `0` request is
/// floored to a sane minimum (never zero — that would break pacing and sizing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailPlan {
    fps: u32,
    max_dim: u32,
}

impl ThumbnailPlan {
    /// The hard upper bound on thumbnail frame rate (the brief's 1–5 fps grid
    /// model; we allow a little headroom for a briefly-focused source).
    pub const MAX_FPS: u32 = 10;
    /// The hard upper bound on a thumbnail's longest edge (≈720p focus, well
    /// under a full canvas).
    pub const MAX_DIM: u32 = 1280;
    /// The minimum frame rate (one frame per second).
    pub const MIN_FPS: u32 = 1;
    /// The minimum longest edge.
    pub const MIN_DIM: u32 = 16;

    /// Build a plan, clamping `fps` into `[MIN_FPS, MAX_FPS]` and `max_dim` into
    /// `[MIN_DIM, MAX_DIM]`.
    #[must_use]
    pub const fn clamped(fps: u32, max_dim: u32) -> Self {
        Self {
            fps: clamp_u32(fps, Self::MIN_FPS, Self::MAX_FPS),
            max_dim: clamp_u32(max_dim, Self::MIN_DIM, Self::MAX_DIM),
        }
    }

    /// The clamped frame rate (frames per second).
    #[must_use]
    pub const fn fps(&self) -> u32 {
        self.fps
    }

    /// The clamped maximum longest-edge dimension (pixels).
    #[must_use]
    pub const fn max_dim(&self) -> u32 {
        self.max_dim
    }
}

/// `const`-friendly `u32` clamp (avoids `Ord::clamp`, which is not `const` and
/// would also require `lo <= hi` to hold at runtime).
const fn clamp_u32(v: u32, lo: u32, hi: u32) -> u32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// The `multipart/x-mixed-replace` MJPEG framing model.
///
/// Holds the multipart boundary token and produces (a) the `Content-Type`
/// response header value and (b) the exact byte framing of each JPEG part. It
/// carries no transport/IO itself — the control plane writes the parts to the
/// response body — so this stays a pure, testable data model.
#[derive(Debug, Clone)]
pub struct MjpegStream {
    boundary: String,
}

impl Default for MjpegStream {
    fn default() -> Self {
        Self::new()
    }
}

impl MjpegStream {
    /// The default boundary token. Fixed and self-delimiting (it does not appear
    /// in JPEG payloads, which are binary and start with the SOI marker).
    const DEFAULT_BOUNDARY: &'static str = "multiviewpreviewframe";

    /// Build a stream with the default boundary token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            boundary: Self::DEFAULT_BOUNDARY.to_owned(),
        }
    }

    /// Build a stream with a caller-supplied boundary token (used when an id
    /// must be embedded for debugging). Empty input falls back to the default.
    #[must_use]
    pub fn with_boundary(boundary: impl Into<String>) -> Self {
        let boundary = boundary.into();
        if boundary.is_empty() {
            return Self::new();
        }
        Self { boundary }
    }

    /// The multipart boundary token (without the leading `--`).
    #[must_use]
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// The `Content-Type` response header value for the MJPEG stream, declaring
    /// the boundary the parts use.
    #[must_use]
    pub fn content_type(&self) -> String {
        format!("multipart/x-mixed-replace; boundary={}", self.boundary)
    }

    /// Frame one JPEG image as a multipart part: the boundary delimiter, the
    /// per-part headers (`Content-Type` + `Content-Length`), the header/body
    /// separator, the raw JPEG bytes, and a trailing CRLF.
    ///
    /// The bytes are returned ready to write directly to the response body.
    #[must_use]
    pub fn frame_part(&self, jpeg: &[u8]) -> Vec<u8> {
        let header = format!(
            "\r\n--{}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            self.boundary,
            jpeg.len(),
        );
        let mut part =
            Vec::with_capacity(header.len().saturating_add(jpeg.len()).saturating_add(2));
        part.extend_from_slice(header.as_bytes());
        part.extend_from_slice(jpeg);
        part.extend_from_slice(b"\r\n");
        part
    }

    /// The closing delimiter that terminates the stream (`\r\n--boundary--\r\n`).
    #[must_use]
    pub fn closing_delimiter(&self) -> Vec<u8> {
        format!("\r\n--{}--\r\n", self.boundary).into_bytes()
    }
}
