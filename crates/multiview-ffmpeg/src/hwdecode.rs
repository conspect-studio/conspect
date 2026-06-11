//! Host-side hardware-decode planning (always compiled; pure, GPU-free).
//!
//! GPU-6 promotes the hardware backends from detection-only to real
//! device-resident decode islands. The *real* `decode -> composite -> encode`
//! runs only on a GPU-tagged self-hosted runner (no GPU/SDK on shared CI), but
//! the **decisions** that drive that path are pure host-side arithmetic and
//! belong here so they can be unit-tested on a GPU-free box:
//!
//! * [`HwDeviceKind`] — which per-vendor zero-copy island (core-engine §7); the
//!   pure enum + its libav device-name. (The FFI that *opens* a device lives in
//!   the `ffmpeg`-gated `hwframe` module — named as a plain code span because it
//!   is absent from the default, pure-Rust doc build.)
//! * [`HwInputCodec`] / [`cuvid_decoder`] — the logical input codec -> concrete
//!   `*_cuvid` NVDEC wrapper-decoder name. The `ffmpeg`-gated `select_decoder`
//!   resolves that name in the linked libav registry (graceful `None` when the
//!   build/feature does not offer it — never a panic, never a silent software
//!   substitution).
//! * [`plan_decode_resize`] — the per-backend decode-resize strategy: NVIDIA
//!   fuses the resize on the NVDEC ASIC straight to the largest consuming tile
//!   (decode-at-display-resolution, inv #6); Intel/AMD/Apple decode full-res and
//!   scale in a separate on-die pass; software decodes full-res on the CPU
//!   (efficiency §2.1). The bitstream is always entropy-decoded at *source*
//!   resolution, so the decode-engine budget is charged at the source size.
//! * [`decode_surface_pool`] — minimal, content-sized surface-pool geometry.
//!   NVDEC sizes to `dpb + slack` at the *real* content resolution (leaving the
//!   max inflated balloons a 1080p decoder to ~542 MB); VAAPI/QSV reserve the
//!   codec reference-frame set because some drivers cannot grow the pool
//!   (efficiency §1). Pool VRAM is estimated in [`HwBitDepth`] sample bytes,
//!   NV12 (1.5 B/px) or P010 (3 B/px) — never RGBA (inv #5).
//!
//! Everything here is total and panic-free: no `unwrap`/`expect`, no indexing,
//! no `as` casts, saturating/`u64` arithmetic throughout. It opens no device and
//! performs no FFI, so it compiles and runs in the default pure-Rust build.

/// A hardware backend family Multiview can target, mapped to libav's
/// `AVHWDeviceType`. Mirrors the per-vendor zero-copy islands (core-engine §7);
/// no cross-vendor on-GPU path is ever modeled.
///
/// This is the pure enum + its libav device-name. The FFI that resolves it to
/// an `AVHWDeviceType` and *opens* a device lives in the `ffmpeg`-gated
/// `hwframe` module (named as a plain code span: it is absent from the default
/// doc build).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HwDeviceKind {
    /// NVIDIA CUDA (NVDEC/NVENC island).
    Cuda,
    /// Linux VA-API (Intel/AMD island).
    Vaapi,
    /// Intel Quick Sync via oneVPL.
    Qsv,
    /// Apple `VideoToolbox`.
    VideoToolbox,
    /// Pure-CPU software decode (the universal fallback; no device).
    Software,
}

impl HwDeviceKind {
    /// The libav device-type name used to resolve the `AVHWDeviceType`.
    ///
    /// [`HwDeviceKind::Software`] has no libav hardware device, so it maps to the
    /// empty string; callers that need a hardware device must reject it before
    /// resolving (the FFI `HwDeviceContext::create` in the `ffmpeg`-gated
    /// `hwframe` module only accepts the hardware kinds).
    #[must_use]
    pub const fn libav_name(self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vaapi => "vaapi",
            Self::Qsv => "qsv",
            Self::VideoToolbox => "videotoolbox",
            Self::Software => "",
        }
    }

    /// Whether this kind fuses the resize *into* the decode (the NVDEC `-resize`
    /// ASIC lever, efficiency §2.1). Only NVIDIA does; everyone else scales in a
    /// separate pass.
    #[must_use]
    pub const fn fuses_decode_resize(self) -> bool {
        matches!(self, Self::Cuda)
    }

    /// Whether a decode on this kind stays on the device (no host round-trip).
    /// Every hardware kind keeps its decode/scale island device-resident; only
    /// software runs on the CPU.
    #[must_use]
    pub const fn is_device_resident(self) -> bool {
        !matches!(self, Self::Software)
    }
}

/// A logical *input* codec a hardware decoder can accept.
///
/// The analogue of [`crate::codec::VideoCodec`] for the decode side: callers
/// name the family (H.264, HEVC, AV1, …) and [`cuvid_decoder`] maps it to the
/// concrete NVDEC `*_cuvid` wrapper-decoder name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HwInputCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
    /// MPEG-2 video.
    Mpeg2Video,
}

impl HwInputCodec {
    /// This codec's NVDEC `*_cuvid` wrapper-decoder short-name.
    ///
    /// Every modeled input codec has a registered cuvid wrapper in the `FFmpeg`
    /// 7.1 build Multiview targets, so this never returns [`None`]; the
    /// [`Option`] mirrors [`crate::codec::VideoCodec`]'s shape and leaves room
    /// for a future codec without a cuvid wrapper.
    #[must_use]
    pub const fn cuvid_name(self) -> Option<&'static str> {
        match self {
            Self::H264 => Some("h264_cuvid"),
            Self::H265 => Some("hevc_cuvid"),
            Self::Av1 => Some("av1_cuvid"),
            Self::Vp9 => Some("vp9_cuvid"),
            Self::Mpeg2Video => Some("mpeg2_cuvid"),
        }
    }
}

/// The NVDEC `*_cuvid` wrapper-decoder short-name for `codec`, when the `cuda`
/// feature is compiled (otherwise [`None`]).
///
/// Pure: this gates the *name* by the `cuda` feature exactly as
/// [`crate::codec::VideoCodec::nvenc_encoder`] does for the encode side, so a
/// build without `cuda` can never name an NVDEC decoder. Presence in the list
/// does **not** guarantee a usable GPU — the `ffmpeg`-gated `select_decoder`
/// verifies the name resolves in the linked libav, and *opening* it still needs
/// a device.
#[must_use]
pub fn cuvid_decoder(codec: HwInputCodec) -> Option<&'static str> {
    #[cfg(feature = "cuda")]
    {
        codec.cuvid_name()
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = codec;
        None
    }
}

/// Resolve the best hardware decoder name for `codec` that is actually present
/// in the linked libav build, or [`None`] if no hardware decoder is available.
///
/// `want_hw` is the caller's request for hardware decode (false short-circuits
/// to [`None`] — use the software decoder). When true, the cuvid wrapper name is
/// looked up in the linked registry: the wrapper decoders are registered even on
/// a GPU-free box, so the name resolves there; *opening* one still needs a
/// device (the run-time gate), and a missing name falls through to [`None`] so
/// the caller degrades to software — never a crash.
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn select_decoder(codec: HwInputCodec, want_hw: bool) -> Option<&'static str> {
    if !want_hw {
        return None;
    }
    let name = cuvid_decoder(codec)?;
    if ffmpeg_next::decoder::find_by_name(name).is_some() {
        Some(name)
    } else {
        None
    }
}

/// Map a linked-libav video [`Id`](ffmpeg_next::codec::Id) to the logical
/// [`HwInputCodec`] family, or [`None`] for a codec with no NVDEC cuvid wrapper.
///
/// Pure: a total `match` over the codec ids Multiview's cuvid wrappers cover
/// (every other id — including audio/subtitle/data — maps to [`None`], so a
/// caller transparently keeps the software decoder). This is the bridge the run
/// path uses to turn a demuxed stream's codec id into a hardware-decode request.
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn hw_input_codec_for_id(id: ffmpeg_next::codec::Id) -> Option<HwInputCodec> {
    use ffmpeg_next::codec::Id;
    match id {
        Id::H264 => Some(HwInputCodec::H264),
        Id::HEVC => Some(HwInputCodec::H265),
        Id::AV1 => Some(HwInputCodec::Av1),
        Id::VP9 => Some(HwInputCodec::Vp9),
        Id::MPEG2VIDEO => Some(HwInputCodec::Mpeg2Video),
        _ => None,
    }
}

/// The concrete NVDEC `*_cuvid` decoder name for a demuxed stream's codec `id`
/// when hardware decode is wanted, present in the linked libav, and the build
/// compiled the `cuda` feature — otherwise [`None`] (decode in software).
///
/// This folds the codec-id → [`HwInputCodec`] mapping into [`select_decoder`],
/// so a codec without a cuvid wrapper, a build without `cuda`, or
/// `want_hw == false` all yield [`None`] and the caller decodes in software.
/// Never panics, never a silent substitution.
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn select_decoder_for_id(id: ffmpeg_next::codec::Id, want_hw: bool) -> Option<&'static str> {
    let codec = hw_input_codec_for_id(id)?;
    select_decoder(codec, want_hw)
}

/// The environment variable that disables NVDEC hardware decode at run time
/// (a per-deploy opt-out — to force the software path for A/B comparison, or to
/// dodge a known-bad driver — without a rebuild).
pub const NVDEC_DISABLE_ENV: &str = "MULTIVIEW_DISABLE_NVDEC";

/// Whether the NVDEC opt-out is engaged, given the raw `MULTIVIEW_DISABLE_NVDEC`
/// reading ([`None`] when the variable is unset).
///
/// Pure and total. An unset or empty/whitespace value, or one of the explicit
/// falsey tokens `0`/`false`/`no`/`off` (any case, surrounding whitespace
/// ignored), leaves hardware decode **enabled**; any other non-empty value
/// disables it. So `MULTIVIEW_DISABLE_NVDEC=1` forces software decode while the
/// variable being absent keeps the GPU path available.
#[must_use]
pub fn nvdec_disabled(env_value: Option<&str>) -> bool {
    match env_value {
        None => false,
        Some(raw) => {
            let v = raw.trim();
            if v.is_empty() {
                return false;
            }
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        }
    }
}

/// Whether hardware (NVDEC) decode should be attempted, given the runtime
/// opt-out reading.
///
/// The pure complement of [`nvdec_disabled`]: hardware decode is wanted unless
/// the opt-out is engaged. The compile-time `cuda` gate and the run-time
/// registry/device checks still apply downstream — this only folds in the
/// operator's explicit opt-out.
#[must_use]
pub fn want_hw_decode(env_value: Option<&str>) -> bool {
    !nvdec_disabled(env_value)
}

/// A pixel size (width x height), the unit the decode planner reasons in.
///
/// Distinct from `multiview-hal`'s `Resolution` so this crate stays leaf-free of
/// the HAL; the engine maps between them. Area uses lossless `u64` arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl TileSize {
    /// Construct a tile size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Pixel count (`width * height`) as `u64` — lossless for any `u32 * u32`.
    #[must_use]
    pub fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// This size clamped to fit within `ceiling` on both axes (component-wise
    /// minimum). Used so a fused decode resize never targets above the source.
    #[must_use]
    pub fn clamped_to(self, ceiling: Self) -> Self {
        Self {
            width: self.width.min(ceiling.width),
            height: self.height.min(ceiling.height),
        }
    }
}

/// How a backend realizes the decode-time resize (efficiency §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecodeResizeStrategy {
    /// NVDEC fused decode-time resize (`*_cuvid -resize`): the ASIC scales for
    /// free during decode, output stays NV12/P010 — decode-at-display-resolution
    /// in a single device-resident step.
    Fused,
    /// A separate on-die scale pass after a full-resolution decode
    /// (`scale_vaapi`/`scale_qsv`/`scale_vt`), still device-resident — the
    /// decoder reconstructs full-res reference frames then the media block
    /// scales.
    SeparateOnDie,
    /// A separate scale through a **full-resolution intermediate** that does not
    /// stay on the device (e.g. a non-Vulkan-Video decoder feeding a GPU
    /// compositor across the host because no stable external-texture import
    /// exists — efficiency §2.5). The decode is not device-resident with the
    /// compositor.
    SeparateFullResIntermediate,
    /// Pure-CPU software decode at full resolution.
    SoftwareFullRes,
}

/// Inputs to [`plan_decode_resize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeInputs {
    /// The backend the decode runs on.
    pub kind: HwDeviceKind,
    /// The deduplicated source's native resolution.
    pub source: TileSize,
    /// The set of tile sizes that consume this source (may be empty before any
    /// tile binds).
    pub consuming_tiles: Vec<TileSize>,
}

/// The decode-resize plan for one deduplicated source on one backend.
///
/// This is also the telemetry record the engine logs to prove inv #6 held: a
/// format-trace can assert no host `hwdownload`/`hwupload` round-trip was
/// inserted (ADR-E002 consequence) by checking [`HwDecodePlan::device_resident`]
/// and that [`HwDecodePlan::is_decode_at_display_resolution`] matches the
/// negotiated strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwDecodePlan {
    /// How the resize is realized.
    pub strategy: DecodeResizeStrategy,
    /// The resolution the decoder is driven to emit (the display/tile size on a
    /// fused backend; the source size on a separate-pass backend).
    pub decode_target: TileSize,
    /// The resolution the bitstream is entropy-decoded at (always the source):
    /// the decode-engine MP/s budget is charged here even when the output is
    /// resized down (efficiency §2.1).
    pub bitstream_resolution: TileSize,
    /// Whether the decode (and any scale) stays on the device — no host copy.
    pub device_resident: bool,
    /// The backend this plan was negotiated for.
    pub kind: HwDeviceKind,
}

impl HwDecodePlan {
    /// Whether this plan realizes decode-at-display-resolution (inv #6): the
    /// decoder emits at the consuming tile size rather than decoding full-res and
    /// scaling afterwards. True only for the [`DecodeResizeStrategy::Fused`]
    /// path.
    #[must_use]
    pub fn is_decode_at_display_resolution(self) -> bool {
        matches!(self.strategy, DecodeResizeStrategy::Fused)
            && self.decode_target.pixels() <= self.bitstream_resolution.pixels()
    }
}

/// Plan the decode-time resize for one deduplicated source on `inputs.kind`
/// (efficiency §2.1).
///
/// * NVIDIA (`Cuda`): fused NVDEC resize straight to the **largest** consuming
///   tile (then `scale_cuda` fans the smaller tiles on-die). The decode target
///   is the largest consuming tile, clamped to the source so the ASIC never
///   upscales past native; with no consuming tile yet it decodes at source.
/// * Intel/AMD (`Vaapi`/`Qsv`) and Apple (`VideoToolbox`): no fused resize — the
///   decoder reconstructs full-resolution reference frames and a separate on-die
///   VPP/SFC/`scale_vt` pass scales after, so the decode target is the source.
/// * Software: full-resolution CPU decode.
///
/// In every case the bitstream is entropy-decoded at source resolution, so
/// [`HwDecodePlan::bitstream_resolution`] is the source and the decode-engine
/// budget is charged there.
#[must_use]
pub fn plan_decode_resize(inputs: ResizeInputs) -> HwDecodePlan {
    let source = inputs.source;
    let kind = inputs.kind;

    // The largest consuming tile, clamped to the source (never upscale past
    // native). With no consuming tile yet, decode at source. Consumes the
    // `consuming_tiles` Vec (no clone).
    let largest_consuming = inputs
        .consuming_tiles
        .into_iter()
        .map(|tile| tile.clamped_to(source))
        .max_by(|a, b| {
            a.pixels()
                .cmp(&b.pixels())
                .then(a.width.cmp(&b.width))
                .then(a.height.cmp(&b.height))
        })
        .unwrap_or(source);

    let (strategy, decode_target) = match kind {
        HwDeviceKind::Cuda => (DecodeResizeStrategy::Fused, largest_consuming),
        HwDeviceKind::Vaapi | HwDeviceKind::Qsv | HwDeviceKind::VideoToolbox => {
            (DecodeResizeStrategy::SeparateOnDie, source)
        }
        HwDeviceKind::Software => (DecodeResizeStrategy::SoftwareFullRes, source),
    };

    HwDecodePlan {
        strategy,
        decode_target,
        bitstream_resolution: source,
        device_resident: kind.is_device_resident(),
        kind,
    }
}

/// The sample bit-depth a hardware surface pool is sized for (inv #5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HwBitDepth {
    /// 8-bit: NV12 surfaces (1.5 bytes/pixel).
    Eight,
    /// 10-bit: P010 surfaces (3 bytes/pixel — 16-bit samples, 1.5 samples/pixel).
    Ten,
}

impl HwBitDepth {
    /// Bytes per pixel for an NV12/P010 surface at this depth, scaled by 2 so the
    /// 4:2:0 1.5-sample/pixel figure stays integer: NV12 = 3 (÷2 = 1.5 B/px),
    /// P010 = 6 (÷2 = 3 B/px). Callers divide the product by 2 once at the end.
    const fn half_bytes_per_pixel(self) -> u64 {
        match self {
            Self::Eight => 3, // NV12: Y (1) + interleaved CbCr (0.5) = 1.5 B/px -> 3 half-bytes
            Self::Ten => 6,   // P010: same sample layout at 2 bytes/sample = 3 B/px -> 6 half-bytes
        }
    }
}

/// Inputs to [`decode_surface_pool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolInputs {
    /// The backend the pool is allocated on.
    pub kind: HwDeviceKind,
    /// The **real** content resolution the surfaces are sized to (NOT an
    /// inflated max — that is the ~542 MB footgun, efficiency §1).
    pub max_resolution: TileSize,
    /// The codec's decoded-picture-buffer / reference-frame depth (e.g. ~4 for
    /// H.264 high profile; +16 reserve for the VAAPI/QSV reference set).
    pub dpb_frames: u32,
}

/// The geometry of a minimal, content-sized hardware decode-surface pool.
///
/// Produced by [`decode_surface_pool`]; the engine maps it onto an
/// `HwFramesSpec` (`initial_pool_size`, `width`, `height` — in the `ffmpeg`-gated
/// `hwframe` module) when allocating the real `AVHWFramesContext` on a GPU
/// runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeSurfacePool {
    /// Number of decode (work) surfaces: DPB + a small fixed slack.
    pub decode_surfaces: u32,
    /// Number of output surfaces handed downstream (1..2).
    pub output_surfaces: u32,
    /// The resolution the surfaces are sized to (the real content size).
    pub max_resolution: TileSize,
}

impl DecodeSurfacePool {
    /// The total surface count (`decode + output`) — the `initial_pool_size` the
    /// frames context is built with.
    #[must_use]
    pub fn total_surfaces(self) -> u32 {
        self.decode_surfaces.saturating_add(self.output_surfaces)
    }

    /// Estimated VRAM the whole pool occupies, in bytes, at `depth`.
    ///
    /// `total_surfaces * width * height * bytes_per_pixel`, all in `u64` with
    /// saturating multiplication so a pathological geometry saturates rather than
    /// overflowing. NV12/P010 only (inv #5) — never an RGBA 4 B/px surface.
    #[must_use]
    pub fn estimated_vram_bytes(self, depth: HwBitDepth) -> u64 {
        let per_surface_half = self
            .max_resolution
            .pixels()
            .saturating_mul(depth.half_bytes_per_pixel());
        let per_surface = per_surface_half / 2;
        per_surface.saturating_mul(u64::from(self.total_surfaces()))
    }
}

/// Fixed slack of decode surfaces beyond the DPB (efficiency §1: NVDEC
/// `min + 3..4`; we hold the upper bound, 4, so a brief reorder never stalls).
const DECODE_SURFACE_SLACK: u32 = 4;

/// Output surfaces held downstream (efficiency §1: NVDEC `1..2`; we hold 2 to
/// keep the compositor fed without ballooning the pool).
const OUTPUT_SURFACES: u32 = 2;

/// Size a minimal, content-sized decode-surface pool for `inputs` (efficiency
/// §1).
///
/// The pool is `dpb_frames + slack` decode surfaces plus a small output set, all
/// sized to the **real** content resolution — never an inflated 8K max, which is
/// the footgun that balloons a 1080p decoder from tens of MB to ~542 MB. NVDEC
/// and VAAPI/QSV use the same shape here; the difference is that VAAPI/QSV pass a
/// larger `dpb_frames` (the +16 reference reserve) because some drivers cannot
/// grow the pool after init, making it a hard maximum.
#[must_use]
pub fn decode_surface_pool(inputs: PoolInputs) -> DecodeSurfacePool {
    let decode_surfaces = inputs.dpb_frames.saturating_add(DECODE_SURFACE_SLACK);
    DecodeSurfacePool {
        decode_surfaces,
        output_surfaces: OUTPUT_SURFACES,
        max_resolution: inputs.max_resolution,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn software_has_no_libav_device_name_and_is_not_resident() {
        assert_eq!(HwDeviceKind::Software.libav_name(), "");
        assert!(!HwDeviceKind::Software.is_device_resident());
        assert!(!HwDeviceKind::Software.fuses_decode_resize());
    }

    #[test]
    fn only_nvidia_fuses_the_decode_resize() {
        assert!(HwDeviceKind::Cuda.fuses_decode_resize());
        for kind in [
            HwDeviceKind::Vaapi,
            HwDeviceKind::Qsv,
            HwDeviceKind::VideoToolbox,
            HwDeviceKind::Software,
        ] {
            assert!(!kind.fuses_decode_resize(), "{kind:?} must not fuse");
        }
    }

    #[test]
    fn nvdec_opt_out_defaults_enabled_and_only_affirmative_disables() {
        // Unset / empty / explicit falsey -> NVDEC stays enabled (default-on).
        assert!(!nvdec_disabled(None));
        assert!(!nvdec_disabled(Some("")));
        assert!(!nvdec_disabled(Some("  \t ")));
        for off in ["0", "false", "FALSE", "No", "off", "  Off  "] {
            assert!(!nvdec_disabled(Some(off)), "{off:?} must keep NVDEC on");
        }
        // Any affirmative value disables hardware decode.
        for on in ["1", "true", "yes", "on", "x"] {
            assert!(nvdec_disabled(Some(on)), "{on:?} must disable NVDEC");
        }
        // want_hw_decode is the exact complement of nvdec_disabled.
        assert!(want_hw_decode(None));
        assert!(!want_hw_decode(Some("1")));
        assert!(want_hw_decode(Some("off")));
        // The opt-out env var name is the canonical MULTIVIEW_* spelling.
        assert_eq!(NVDEC_DISABLE_ENV, "MULTIVIEW_DISABLE_NVDEC");
    }

    #[test]
    fn clamp_takes_the_component_wise_minimum() {
        let s = TileSize::new(1280, 720);
        assert_eq!(
            TileSize::new(1920, 1080).clamped_to(s),
            TileSize::new(1280, 720)
        );
        assert_eq!(
            TileSize::new(640, 2000).clamped_to(s),
            TileSize::new(640, 720)
        );
    }
}
