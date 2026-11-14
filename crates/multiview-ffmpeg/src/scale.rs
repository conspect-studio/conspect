//! Safe `libswscale` wrapper (the `ffmpeg` feature).
//!
//! [`Scaler`] owns one `SwsContext` (freed in `Drop` by `ffmpeg_next`) and
//! converts a source [`frame::Video`](ffmpeg_next::frame::Video) into a
//! destination format/size. The headline use in the decode path is the
//! `YUV420P -> NV12` conversion that brings planar 4:2:0 software frames onto
//! the NV12-throughout timeline (invariant #5) before they enter the pipeline;
//! it equally handles arbitrary rescales.
//!
//! The context is fixed to one `(src, dst)` definition at construction; feeding
//! a frame whose geometry/format differs is a typed [`FfmpegError::FrameMismatch`]
//! error, never a silent mis-scale.

use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::{Context as Sws, Flags};
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};
use crate::frame::{alloc_video, plane_rows};

/// A `(format, width, height)` description of a scaler endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleSpec {
    /// Pixel format.
    pub format: Pixel,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ScaleSpec {
    /// Construct a spec.
    #[must_use]
    pub const fn new(format: Pixel, width: u32, height: u32) -> Self {
        Self {
            format,
            width,
            height,
        }
    }
}

/// A libswscale conversion context fixed to one source/destination definition.
///
/// `!Sync` by construction (the `SwsContext` is not safe to share across
/// threads unsynchronized); `Send` so it can live on the decode thread.
pub struct Scaler {
    sws: Sws,
    src: ScaleSpec,
    dst: ScaleSpec,
}

// `ffmpeg_next` marks its libswresample `Context` `Send` but — by omission, not
// for any soundness reason — leaves its libswscale `Context` neither `Send` nor
// `Sync` (it holds a raw `*mut SwsContext`). An `SwsContext` is a self-contained,
// owned conversion context with no thread-affine state, so moving an exclusively
// owned `Scaler` to a decode thread is sound; we assert that here so the wrapper
// satisfies the crate-wide "wrappers are `Send`" rule (CLAUDE.md §7). It is left
// `!Sync` (no `unsafe impl Sync`): per-context access must stay single-threaded.
//
// reason: a trait promise about an owned context, not raw FFI. (The checked
// allocation/copy blocks below each carry their own `allow` and SAFETY note.)
#[allow(unsafe_code)]
// SAFETY: `Sws` owns its `SwsContext` exclusively (freed once in its `Drop`),
// exposes no interior mutability, and the context carries no thread-local state;
// transferring sole ownership across threads upholds libswscale's "one thread at
// a time per context" contract because `&mut self` is required to use it.
unsafe impl Send for Scaler {}

impl Scaler {
    /// Build a scaler converting `src` frames to the `dst` format/size using
    /// high-quality Lanczos resampling (bit-exact accumulation).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Convert`] if libswscale rejects the conversion
    /// (e.g. an unsupported format pair).
    pub fn new(src: ScaleSpec, dst: ScaleSpec) -> Result<Self> {
        Self::with_flags(src, dst, Flags::LANCZOS | Flags::ACCURATE_RND)
    }

    /// Build a scaler with explicit libswscale flags.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Convert`] if libswscale rejects the conversion.
    pub fn with_flags(src: ScaleSpec, dst: ScaleSpec, flags: Flags) -> Result<Self> {
        let sws = Sws::get(
            src.format, src.width, src.height, dst.format, dst.width, dst.height, flags,
        )
        .map_err(FfmpegError::Convert)?;
        Ok(Self { sws, src, dst })
    }

    /// The source endpoint this scaler was built for.
    #[must_use]
    pub const fn source(&self) -> ScaleSpec {
        self.src
    }

    /// The destination endpoint this scaler was built for.
    #[must_use]
    pub const fn destination(&self) -> ScaleSpec {
        self.dst
    }

    /// Convert `input` into a freshly allocated destination [`Video`] frame,
    /// carrying the source PTS through unchanged.
    ///
    /// The PTS is **input** time — it is copied so the caller can rebase it; it
    /// is never assumed to be output time.
    ///
    /// **Identity requests bypass libswscale.** When the source and destination
    /// specs are equal (same format, same geometry) the conversion is a plane
    /// copy, performed here via `av_frame_copy` (the [`Video::clone`] deep
    /// copy). libswscale's no-op converter on `FFmpeg` 7/8 copies the luma plane
    /// of a semi-planar (NV12/P010) frame but leaves the interleaved chroma
    /// plane **zeroed** — Cb=Cr=0, a saturated green/magenta picture (the
    /// on-hardware live-add Defect A, 2026-06-11) — so an identity request must
    /// never reach `sws_scale`. Pinned by
    /// `tests/scale_resample.rs::identity_nv12_to_nv12_preserves_the_chroma_plane`.
    ///
    /// # Errors
    /// * [`FfmpegError::FrameMismatch`] — `input` does not match the source spec.
    /// * [`FfmpegError::Convert`] — libswscale reported a conversion error.
    pub fn run(&mut self, input: &Video) -> Result<Video> {
        if input.format() != self.src.format
            || input.width() != self.src.width
            || input.height() != self.src.height
        {
            return Err(FfmpegError::FrameMismatch(
                "input geometry/format differs from the scaler's source spec",
            ));
        }
        // The identity predicate is `ScaleSpec` equality (format + geometry) —
        // the complete set of fields this wrapper feeds into `sws_scale`. If the
        // scaler ever gains colour/range/matrix conversion, extend `ScaleSpec`'s
        // `Eq` to cover those axes, or this short-circuit could skip a real convert.
        if self.src == self.dst {
            // Identity: a deep plane copy (alloc + `av_frame_copy` +
            // `av_frame_copy_props`), never the broken libswscale no-op path.
            // libswscale's no-op converter on FFmpeg 7/8 copies luma but
            // zeroes the interleaved chroma plane of NV12/P010 — Defect A.
            //
            // `Video::clone` allocates through the unchecked `Video::new`, so
            // allocate checked and copy with the result verified instead.
            let mut output = alloc_video(self.dst.format, self.dst.width, self.dst.height)?;
            #[allow(unsafe_code)]
            // SAFETY: both pointers are live `AVFrame`s owned by `output` and
            // `input`; `output` has buffers of the same format and size as
            // `input` (checked above and by `alloc_video`), which is what
            // `av_frame_copy` requires; `av_frame_copy_props` copies metadata.
            let (copied, props) = unsafe {
                (
                    ffmpeg::ffi::av_frame_copy(output.as_mut_ptr(), input.as_ptr()),
                    ffmpeg::ffi::av_frame_copy_props(output.as_mut_ptr(), input.as_ptr()),
                )
            };
            if copied < 0 || props < 0 {
                return Err(FfmpegError::FrameMismatch("identity plane copy failed"));
            }
            return Ok(output);
        }
        // Checked allocation: `sws.run` would otherwise allocate an empty
        // output with an unchecked `av_frame_get_buffer`.
        let mut output = alloc_video(self.dst.format, self.dst.width, self.dst.height)?;
        self.sws
            .run(input, &mut output)
            .map_err(FfmpegError::Convert)?;
        output.set_pts(input.pts());
        Ok(output)
    }

    /// Convert `input` into the caller-owned `output`, reusing its buffers.
    ///
    /// `output` is (re)allocated only when it is empty or does not match the
    /// destination spec, so a steady per-frame caller converts with no heap
    /// allocation after the first frame (the media player's alpha scale,
    /// #391). The allocation is checked ([`alloc_video`]): a failure is an
    /// error before any plane is touched or libswscale runs. An identity spec
    /// copies the planes into `output` row by row through validated row views
    /// ([`plane_rows`]), so a bottom-up (negative linesize) input is fine.
    ///
    /// # Errors
    /// As [`Self::run`], plus [`FfmpegError::FrameLayout`] /
    /// [`FfmpegError::FrameAlloc`] from the output allocation or a malformed
    /// input plane.
    pub fn run_into(&mut self, input: &Video, output: &mut Video) -> Result<()> {
        if input.format() != self.src.format
            || input.width() != self.src.width
            || input.height() != self.src.height
        {
            return Err(FfmpegError::FrameMismatch(
                "input geometry/format differs from the scaler's source spec",
            ));
        }
        if output.format() != self.dst.format
            || output.width() != self.dst.width
            || output.height() != self.dst.height
            || !has_cpu_planes(output)
        {
            *output = alloc_video(self.dst.format, self.dst.width, self.dst.height)?;
        }
        if self.src == self.dst {
            // Never the libswscale no-op path (Defect A, see `run`).
            let width = libc::c_int::try_from(self.dst.width)
                .map_err(|_| FfmpegError::FrameLayout("frame width overflows"))?;
            let format = ffmpeg::ffi::AVPixelFormat::from(self.dst.format);
            for plane in 0..output.planes() {
                let index = libc::c_int::try_from(plane)
                    .map_err(|_| FfmpegError::FrameLayout("plane index"))?;
                #[allow(unsafe_code)]
                // SAFETY: `av_image_get_linesize` only reads the static
                // pixel-format table; it returns a negative error code for
                // an invalid plane or width, which is rejected below.
                let bytes = unsafe { ffmpeg::ffi::av_image_get_linesize(format, width, index) };
                let bytes = usize::try_from(bytes)
                    .map_err(|_| FfmpegError::FrameLayout("plane row size"))?;
                let d_stride = output.stride(plane);
                let rows = plane_rows(input, plane, bytes)?;
                let dst = output.data_mut(plane);
                for (r, src) in rows.enumerate() {
                    let start = r
                        .checked_mul(d_stride)
                        .ok_or(FfmpegError::FrameLayout("plane offset overflows"))?;
                    let end = start
                        .checked_add(bytes)
                        .ok_or(FfmpegError::FrameLayout("plane offset overflows"))?;
                    dst.get_mut(start..end)
                        .ok_or(FfmpegError::FrameLayout("output plane too short"))?
                        .copy_from_slice(src);
                }
            }
            return Ok(());
        }
        self.sws.run(input, output).map_err(FfmpegError::Convert)
    }
}

/// Whether `frame` holds every plane of its format with a positive linesize,
/// so `stride`/`data_mut` describe real, forward-laid-out memory.
fn has_cpu_planes(frame: &Video) -> bool {
    let format = ffmpeg::ffi::AVPixelFormat::from(frame.format());
    #[allow(unsafe_code)]
    // SAFETY: `av_pix_fmt_count_planes` reads the static format table; the
    // frame pointer is `frame`'s own live `AVFrame`, read-only here, and
    // `planes <= AV_NUM_DATA_POINTERS`.
    unsafe {
        let planes = usize::try_from(ffmpeg::ffi::av_pix_fmt_count_planes(format)).unwrap_or(0);
        let raw = frame.as_ptr();
        planes > 0
            && planes <= 8
            && !raw.is_null()
            && (0..planes).all(|p| {
                (*raw).data.get(p).is_some_and(|d| !d.is_null())
                    && (*raw).linesize.get(p).is_some_and(|&l| l > 0)
            })
    }
}
