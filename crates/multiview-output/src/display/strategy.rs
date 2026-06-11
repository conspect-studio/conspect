//! The per-hardware **buffer-strategy** decision for the display render path
//! (DEV-B3, brief `docs/research/display-out.md` §2, [ADR-0044]).
//!
//! The decisive question per display block is *whether the display plane scans
//! out NV12 directly* and *whether the canvas reaches us as an importable
//! dmabuf*. This module answers it over **plain data** — no ioctls, no GPU, no
//! `display-kms` feature — so the path selection (NV12-direct vs the wgpu
//! NV12→XRGB pass vs the CPU NV12→XRGB fallback) is unit-tested in CI without
//! hardware. The actual ADDFB2 import (drm/gbm, [`super::kms`]) and the wgpu
//! render pass run only on hardware; *which* of them runs is decided here.
//!
//! The three strategies map onto the §2 table:
//!
//! | Display block | Strategy | Copies / passes |
//! |---|---|---|
//! | Intel Gen9+, vc4 (incl. SAND128) | [`BufferStrategy::Nv12Direct`] | 0 / 0 |
//! | AMD DCE11 (RGB-only planes) + GPU | [`BufferStrategy::WgpuXrgbPass`] | 0 / 1 |
//! | RGB-only plane, no wired GPU import | [`BufferStrategy::CpuXrgbConvert`] | 0 / 0 passes, 1 CPU convert |
//!
//! [`BufferStrategy::CpuXrgbConvert`] is the **guaranteed default**: it needs
//! neither an NV12-capable plane nor a GPU importer, only a CPU NV12 canvas and
//! an XRGB8888 plane (which every probed target in §2/§12 has). It is what
//! ships and runs today (DEV-B1); the other two are strict optimisations the
//! selector chooses *only* when their preconditions are proven by the runtime
//! probe — never assumed from a static vendor table.
//!
//! [ADR-0044]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0044.md

/// `DRM_FORMAT_MOD_LINEAR` — untiled, the default framebuffer layout.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// A DRM pixel format, identified by its 32-bit little-endian fourcc (the raw
/// value KMS planes report). A newtype so the selector never confuses a format
/// code with an arbitrary integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrmFormat(u32);

impl DrmFormat {
    /// `XR24` — XRGB8888, the universal RGB scanout format (the CPU/wgpu pass
    /// target; present on every probed primary plane, §12).
    pub const XRGB8888: Self = Self(u32::from_le_bytes(*b"XR24"));
    /// `NV12` — 8-bit 4:2:0 biplanar, the workspace-canonical pixel format
    /// (invariant #5) and the zero-copy direct-scanout format on Intel/vc4.
    pub const NV12: Self = Self(u32::from_le_bytes(*b"NV12"));
    /// `P010` — 10-bit 4:2:0 biplanar; vc4 additionally scans this out (with
    /// the SAND modifier) for HDR-capable Pi pipelines.
    pub const P010: Self = Self(u32::from_le_bytes(*b"P010"));

    /// Build a format from its four fourcc bytes (little-endian, the KMS
    /// convention).
    #[must_use]
    pub const fn from_fourcc(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }

    /// The raw 32-bit fourcc value KMS reports for this format.
    #[must_use]
    pub const fn fourcc(self) -> u32 {
        self.0
    }
}

/// One scanout plane's advertised capability: the formats it can scan out and,
/// where the driver exposes an `IN_FORMATS` blob, the modifiers it accepts.
///
/// An **empty** `modifiers` list means the driver advertised no per-format
/// modifier blob (legacy/`ADDFB`-era drivers). In that case the plane is
/// treated as linear-only: a `None`/`LINEAR` request is honoured, a non-linear
/// tiling request is refused (we never flip a tiled buffer onto a plane that
/// has not *proven* it understands that tiling — the #5727 mis-tile hazard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneFormatCaps {
    formats: Vec<DrmFormat>,
    modifiers: Vec<u64>,
}

impl PlaneFormatCaps {
    /// A plane that scans out `formats`, accepting `modifiers` (empty = no
    /// advertised modifier blob → linear-only, see the type docs).
    #[must_use]
    pub fn new(formats: Vec<DrmFormat>, modifiers: Vec<u64>) -> Self {
        Self { formats, modifiers }
    }

    /// Whether this plane lists `format` among its scanout formats.
    #[must_use]
    pub fn has_format(&self, format: DrmFormat) -> bool {
        self.formats.contains(&format)
    }

    /// Whether this plane can scan out a buffer with `modifier` (`None` ==
    /// modifier-agnostic / linear). An empty modifier list is linear-only.
    #[must_use]
    pub fn accepts_modifier(&self, modifier: Option<u64>) -> bool {
        match modifier {
            // Linear (or modifier-agnostic): honoured by any plane, including
            // legacy planes with no advertised modifier blob.
            None | Some(DRM_FORMAT_MOD_LINEAR) => {
                self.modifiers.is_empty() || self.modifiers.contains(&DRM_FORMAT_MOD_LINEAR)
            }
            // A non-linear tiling must be explicitly advertised — never assumed.
            Some(m) => self.modifiers.contains(&m),
        }
    }
}

/// How the composited canvas reaches the sink — the other half of the
/// direct-scanout precondition. A direct flip needs an **importable NV12
/// dmabuf**; a CPU-planes canvas can only be CPU-converted (or uploaded by a
/// GPU pass).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CanvasDelivery {
    /// The canvas is CPU-resident NV12 planes (the DEV-B1 path). No dmabuf to
    /// flip directly; the CPU convert (or a GPU upload pass) is required.
    CpuPlanes,
    /// The canvas is backed by an importable dmabuf with this format/modifier
    /// (the decoder's or the compositor's GBM/Vulkan-exported buffer).
    Dmabuf {
        /// The dmabuf's pixel format.
        format: DrmFormat,
        /// The dmabuf's modifier (`None` == linear/unmodified).
        modifier: Option<u64>,
    },
}

/// Everything the strategy selector needs about one head, as plain data the
/// runtime probe fills in (plane caps from `get_plane`/`IN_FORMATS`, the canvas
/// delivery shape, and whether a wgpu importer is wired for this build/target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanoutCaps {
    /// The primary plane's advertised format/modifier capability.
    pub plane: PlaneFormatCaps,
    /// How the canvas is delivered to the sink.
    pub canvas: CanvasDelivery,
    /// Whether a wgpu NV12→XRGB import-and-render pass is wired and usable on
    /// this target (a real GPU adapter present *and* the dmabuf-import seam
    /// available — see the module-level wgpu-version verdict in [`super`]).
    pub gpu_pass_available: bool,
}

/// The chosen per-frame buffer path for one head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BufferStrategy {
    /// Import the canvas NV12 dmabuf straight to the plane via `ADDFB2` and
    /// flip it — **0 copies, 0 render passes** (Intel Gen9+, vc4). The format
    /// and modifier are exactly those proven compatible with the plane.
    Nv12Direct {
        /// The NV12 (or P010) format to scan out.
        format: DrmFormat,
        /// The buffer modifier to declare to `ADDFB2` (`None` == linear).
        modifier: Option<u64>,
    },
    /// Render the canvas NV12 → an XRGB8888 GBM scanout dmabuf in one wgpu pass
    /// and flip that — **0 copies, 1 render pass** (AMD DCE11 with a GPU).
    WgpuXrgbPass,
    /// Convert the canvas NV12 → XRGB8888 on the CPU into a mapped scanout
    /// buffer (the DEV-B1 path) and flip it. The portable, GPU-free,
    /// guaranteed-available default.
    CpuXrgbConvert,
}

/// Read a little-endian `u32` at `off` from `blob` (native byte order: KMS
/// blobs are produced by the running kernel, so native == correct).
fn read_u32(blob: &[u8], off: usize) -> Option<u32> {
    blob.get(off..off.checked_add(4)?)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_ne_bytes)
}

/// Read a native-order `u64` at `off` from `blob`.
fn read_u64(blob: &[u8], off: usize) -> Option<u64> {
    blob.get(off..off.checked_add(8)?)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
        .map(u64::from_ne_bytes)
}

/// Parse a KMS `IN_FORMATS` property blob (uapi `struct
/// drm_format_modifier_blob`) into a [`PlaneFormatCaps`]: the plane's scanout
/// formats and the modifiers it accepts.
///
/// Layout: a 24-byte header `{ version, flags, count_formats, formats_offset,
/// count_modifiers, modifiers_offset }`, then `count_formats` `u32` fourccs at
/// `formats_offset`, then `count_modifiers` 24-byte `drm_format_modifier`
/// records `{ formats: u64, offset: u32, pad: u32, modifier: u64 }` (the
/// `modifier` field at record byte 16) at `modifiers_offset`. Every
/// offset/length is bounds-checked; a malformed or truncated blob returns
/// [`None`] (never panics), and the caller assumes a linear-only plane.
///
/// All distinct modifiers in the blob are collected (the per-modifier
/// `formats` bitmask is which formats it applies to; for the NV12-direct gate
/// we only need the *set* of accepted modifiers — the format set is checked
/// independently by [`plane_supports_nv12`]).
#[must_use]
pub fn parse_in_formats_blob(blob: &[u8]) -> Option<PlaneFormatCaps> {
    let count_formats = usize::try_from(read_u32(blob, 8)?).ok()?;
    let formats_offset = usize::try_from(read_u32(blob, 12)?).ok()?;
    let count_modifiers = usize::try_from(read_u32(blob, 16)?).ok()?;
    let modifiers_offset = usize::try_from(read_u32(blob, 20)?).ok()?;

    let mut formats = Vec::with_capacity(count_formats);
    for i in 0..count_formats {
        let off = formats_offset.checked_add(i.checked_mul(4)?)?;
        formats.push(DrmFormat(read_u32(blob, off)?));
    }

    let mut modifiers = Vec::with_capacity(count_modifiers);
    for i in 0..count_modifiers {
        // Each drm_format_modifier is 24 bytes { formats:u64, offset:u32,
        // pad:u32, modifier:u64 }; the modifier u64 is at record byte 16.
        let rec = modifiers_offset.checked_add(i.checked_mul(24)?)?;
        let modifier = read_u64(blob, rec.checked_add(16)?)?;
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    Some(PlaneFormatCaps::new(formats, modifiers))
}

/// Does `plane` scan out NV12 (or P010) with `modifier`? Returns the matching
/// format when both the format and the modifier are advertised, else `None`.
/// This is the plane-format gate the direct-scanout decision rests on.
#[must_use]
pub fn plane_supports_nv12(plane: &PlaneFormatCaps, modifier: Option<u64>) -> Option<DrmFormat> {
    if !plane.accepts_modifier(modifier) {
        return None;
    }
    [DrmFormat::NV12, DrmFormat::P010]
        .into_iter()
        .find(|fmt| plane.has_format(*fmt))
}

/// Choose the per-frame buffer strategy for a head from its probed
/// capabilities (brief §2). Preference order, cheapest first:
///
/// 1. **NV12-direct** — the canvas is an importable NV12/P010 dmabuf *and* the
///    plane scans out that exact format+modifier (0 copies, 0 passes).
/// 2. **wgpu XRGB pass** — a GPU importer is wired (0 copies, 1 pass); the
///    fallback when the plane cannot scan out NV12 directly.
/// 3. **CPU XRGB convert** — the guaranteed default; needs only a CPU NV12
///    canvas and an XRGB plane.
///
/// A modifier mismatch (e.g. a SAND-tiled canvas onto a linear-only plane)
/// never selects direct scanout — flipping a mis-tiled buffer is the
/// rpi/linux #5727 green-screen hazard — so the selector degrades to the next
/// cheapest *correct* path instead.
#[must_use]
pub fn select_buffer_strategy(caps: &ScanoutCaps) -> BufferStrategy {
    // The wgpu pass and direct scanout both import the canvas dmabuf; a
    // CPU-planes canvas can only be CPU-converted.
    let CanvasDelivery::Dmabuf { format, modifier } = caps.canvas else {
        return BufferStrategy::CpuXrgbConvert;
    };
    // 1. Zero-copy direct scanout: only when the dmabuf's format+modifier the
    //    plane has *proven* it scans out.
    if (format == DrmFormat::NV12 || format == DrmFormat::P010)
        && plane_supports_nv12(&caps.plane, modifier) == Some(format)
    {
        return BufferStrategy::Nv12Direct { format, modifier };
    }
    // 2. One wgpu NV12→XRGB import-and-render pass when a GPU importer is
    //    wired (it imports the same dmabuf the plane could not scan out).
    if caps.gpu_pass_available {
        return BufferStrategy::WgpuXrgbPass;
    }
    // 3. The portable, always-available CPU conversion (the guaranteed default).
    BufferStrategy::CpuXrgbConvert
}
