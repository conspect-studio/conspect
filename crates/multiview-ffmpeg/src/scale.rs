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
use crate::frame::{alloc_video, check_source, plane_rows, ScratchFrame};

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
    /// * [`FfmpegError::FrameLayout`] — an `input` plane is not inside the
    ///   frame's own buffers for its geometry, or the frame has no
    ///   refcounted buffers ([`check_source`]).
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
        // The spec match says nothing about the buffers: a safe caller can
        // raise a small frame's height with `Video::set_height`. Prove every
        // source plane lies in the frame's own buffers before libav reads it.
        check_source(input)?;
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
            // `av_frame_copy` requires, and `check_source` proved every
            // plane it reads lies inside `input`'s own buffers;
            // `av_frame_copy_props` copies metadata.
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
        // `check_source` proved every row libswscale reads (the spec's
        // geometry) lies inside `input`'s buffers.
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
    /// `output` is a [`ScratchFrame`]: only [`ScratchFrame::alloc`] (the
    /// checked [`alloc_video`](crate::frame::alloc_video)) creates one and
    /// its frame is never exposed mutably, so a reused destination cannot be
    /// a null frame, an unallocated one, or a small allocation whose height
    /// was raised afterwards. It is reallocated only when it does not match
    /// the destination spec, so a steady per-frame caller converts with no
    /// heap allocation after the first frame (the media player's alpha
    /// scale, #391). Before anything is written every destination plane is
    /// re-validated against the frame's own buffers and the buffers must be
    /// writable (`av_frame_is_writable`); a failure is an error, never a
    /// write. Every source plane is validated the same way, read-only
    /// ([`check_source`]), before either path reads it. An identity spec
    /// copies the planes row by row through validated row views
    /// ([`plane_rows`]), so a bottom-up (negative linesize) input is fine.
    ///
    /// # Errors
    /// As [`Self::run`], plus [`FfmpegError::FrameLayout`] /
    /// [`FfmpegError::FrameAlloc`] from the output allocation, a destination
    /// plane that fails validation, or a malformed input plane.
    pub fn run_into(&mut self, input: &Video, output: &mut ScratchFrame) -> Result<()> {
        if input.format() != self.src.format
            || input.width() != self.src.width
            || input.height() != self.src.height
        {
            return Err(FfmpegError::FrameMismatch(
                "input geometry/format differs from the scaler's source spec",
            ));
        }
        // The spec match says nothing about the buffers: a safe caller can
        // raise a small frame's height with `Video::set_height`. Prove every
        // source plane lies in the frame's own buffers before libav reads it.
        check_source(input)?;
        if !output.matches(self.dst.format, self.dst.width, self.dst.height) {
            *output = ScratchFrame::alloc(self.dst.format, self.dst.width, self.dst.height)?;
        }
        output.check_destination()?;
        if self.src == self.dst {
            // Never the libswscale no-op path (Defect A, see `run`).
            let width = libc::c_int::try_from(self.dst.width)
                .map_err(|_| FfmpegError::FrameLayout("frame width overflows"))?;
            let format = ffmpeg::ffi::AVPixelFormat::from(self.dst.format);
            for plane in 0..output.frame().planes() {
                let index = libc::c_int::try_from(plane)
                    .map_err(|_| FfmpegError::FrameLayout("plane index"))?;
                #[allow(unsafe_code)]
                // SAFETY: `av_image_get_linesize` only reads the static
                // pixel-format table; it returns a negative error code for
                // an invalid plane or width, which is rejected below.
                let bytes = unsafe { ffmpeg::ffi::av_image_get_linesize(format, width, index) };
                let bytes = usize::try_from(bytes)
                    .map_err(|_| FfmpegError::FrameLayout("plane row size"))?;
                let rows = plane_rows(input, plane, bytes)?;
                let (dst, d_stride) = output.plane_mut(plane)?;
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
        // `check_destination` proved every plane of the (unchanged) frame
        // is writable, forward and inside its own buffers for the spec's
        // geometry, which is exactly what `sws_scale` writes; `check_source`
        // proved the same, read-only, for every input plane it reads.
        self.sws
            .run(input, output.video_mut())
            .map_err(FfmpegError::Convert)
    }
}

#[cfg(test)]
mod run_into_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    fn gray(w: u32, h: u32) -> Video {
        let mut frame = alloc_video(Pixel::GRAY8, w, h).unwrap();
        let stride = frame.stride(0);
        for (i, b) in frame.data_mut(0).iter_mut().enumerate() {
            *b = u8::try_from((i % stride) * 30 % 256).unwrap();
        }
        frame
    }

    /// A small allocation whose height was raised afterwards with the safe
    /// `Video::set_height` — the shape a caller could pass as a reused
    /// output before `run_into` took a `ScratchFrame`.
    fn undersized(w: u32, small_h: u32, claimed_h: u32) -> ScratchFrame {
        let mut frame = alloc_video(Pixel::GRAY8, w, small_h).unwrap();
        frame.set_height(claimed_h);
        ScratchFrame::from_video_unchecked(frame)
    }

    /// R3 round 2 #1: an output that matches the destination spec but whose
    /// buffers are smaller than its claimed geometry is an error before
    /// libswscale (or the identity copy) writes a byte; it used to SIGSEGV.
    #[test]
    fn an_undersized_matching_output_is_an_error_not_ub() {
        let src = gray(4, 2);
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 4, 512),
        )
        .unwrap();
        let mut out = undersized(4, 1, 512);
        let err = scale.run_into(&src, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");

        let tall = gray(4, 512);
        let mut identity = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 512),
            ScaleSpec::new(Pixel::GRAY8, 4, 512),
        )
        .unwrap();
        let mut out = undersized(4, 1, 512);
        let err = identity.run_into(&tall, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
    }

    /// The error of a `run` that must fail (`Video` is not `Debug`).
    fn run_err(scaler: &mut Scaler, input: &Video) -> FfmpegError {
        match scaler.run(input) {
            Ok(_) => panic!("run unexpectedly succeeded"),
            Err(e) => e,
        }
    }

    /// A small `GRAY8` source allocation whose height was raised afterwards
    /// with the safe `Video::set_height`, so it matches a taller source spec
    /// while its buffers hold only `small_h` rows.
    fn undersized_source(w: u32, small_h: u32, claimed_h: u32) -> Video {
        let mut frame = gray(w, small_h);
        frame.set_height(claimed_h);
        frame
    }

    /// R3 round 3: an undersized source is an error before libswscale
    /// reads it (conversion), for `run` and `run_into`.
    #[test]
    fn an_undersized_source_is_an_error_in_the_conversion_path() {
        let src = undersized_source(64, 1, 4096);
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 64, 4096),
            ScaleSpec::new(Pixel::GRAY8, 32, 16),
        )
        .unwrap();
        let err = run_err(&mut scale, &src);
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 32, 16).unwrap();
        let err = scale.run_into(&src, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
    }

    /// R3 round 3: an undersized source is an error before the identity
    /// copy reads it, for `run` (`av_frame_copy`) and `run_into`.
    #[test]
    fn an_undersized_source_is_an_error_in_the_identity_path() {
        let src = undersized_source(64, 1, 4096);
        let spec = ScaleSpec::new(Pixel::GRAY8, 64, 4096);
        let mut identity = Scaler::new(spec, spec).unwrap();
        let err = run_err(&mut identity, &src);
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 64, 4096).unwrap();
        let err = identity.run_into(&src, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
    }

    /// R3 round 3: a source with plane pointers but no refcounted buffers
    /// (nothing to bound its planes against) is refused, not read.
    #[test]
    fn a_source_without_refcounted_buffers_is_refused() {
        let backing = gray(4, 2);
        let mut borrowed = Video::empty();
        #[allow(unsafe_code)]
        // SAFETY: both frames are live and owned here; `borrowed` gets
        // `backing`'s plane pointers and geometry but no `buf` reference
        // (so its Drop frees nothing); `backing` outlives every use.
        unsafe {
            let (dst, src) = (borrowed.as_mut_ptr(), backing.as_ptr());
            (*dst).format = (*src).format;
            (*dst).width = (*src).width;
            (*dst).height = (*src).height;
            (*dst).data = (*src).data;
            (*dst).linesize = (*src).linesize;
        }
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 8, 4),
        )
        .unwrap();
        let err = run_err(&mut scale, &borrowed);
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 8, 4).unwrap();
        let err = scale.run_into(&borrowed, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
        drop(borrowed);
        drop(backing);
    }

    /// The validation accepts the frames the decode path really produces:
    /// a bottom-up (negative linesize) source still converts.
    #[test]
    fn a_bottom_up_source_still_converts() {
        let mut src = gray(4, 2);
        #[allow(unsafe_code)]
        // SAFETY: flip plane 0 in place: point `data[0]` at the last row and
        // negate the linesize; every row stays inside the frame's buffer.
        unsafe {
            let raw = src.as_mut_ptr();
            let stride = (*raw).linesize[0];
            (*raw).data[0] = (*raw).data[0].offset(isize::try_from(stride).unwrap());
            (*raw).linesize[0] = -stride;
        }
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 8, 4),
        )
        .unwrap();
        scale.run(&src).unwrap();
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 8, 4).unwrap();
        scale.run_into(&src, &mut out).unwrap();
        let spec = ScaleSpec::new(Pixel::GRAY8, 4, 2);
        let mut identity = Scaler::new(spec, spec).unwrap();
        identity.run(&src).unwrap();
    }

    /// R3 round 2 #1: a destination whose buffers are shared with another
    /// frame (not `av_frame_is_writable`) is refused, never written through.
    #[test]
    fn a_shared_output_buffer_is_refused() {
        let src = gray(4, 2);
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 8, 4),
        )
        .unwrap();
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 8, 4).unwrap();
        let mut other = Video::empty();
        #[allow(unsafe_code)]
        // SAFETY: both frames are live and owned here; `other` is empty, as
        // `av_frame_ref` requires, and takes a second reference to `out`'s
        // buffers (freed by `other`'s Drop).
        let ret = unsafe { ffmpeg::ffi::av_frame_ref(other.as_mut_ptr(), out.frame().as_ptr()) };
        assert_eq!(ret, 0);
        let before = other.data(0).to_vec();
        let err = scale.run_into(&src, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameLayout(_)), "{err}");
        assert_eq!(other.data(0), &before[..], "a shared buffer was written");
        drop(other);
        // Once the other holder lets go, the same frame is reused.
        scale.run_into(&src, &mut out).unwrap();
    }

    /// R3 round 2 #2: a failed `av_frame_alloc` (null `AVFrame`) is an
    /// allocation error; no `ScratchFrame` (hence no `run_into` output)
    /// can hold a null frame, so no accessor ever reads through one.
    #[test]
    fn a_null_frame_allocation_never_becomes_a_scratch_frame() {
        crate::frame::fail_next_frame_alloc();
        let err = ScratchFrame::alloc(Pixel::GRAY8, 8, 4).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameAlloc(_)), "{err}");
        // The seam is one-shot: the next allocation succeeds and converts.
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 8, 4).unwrap();
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 8, 4),
        )
        .unwrap();
        scale.run_into(&gray(4, 2), &mut out).unwrap();
    }

    /// A spec change reallocates through the checked path; a failure there
    /// is an error and leaves no half-built output.
    #[test]
    fn a_failed_reallocation_is_an_error() {
        let mut out = ScratchFrame::alloc(Pixel::GRAY8, 2, 2).unwrap();
        let mut scale = Scaler::new(
            ScaleSpec::new(Pixel::GRAY8, 4, 2),
            ScaleSpec::new(Pixel::GRAY8, 8, 4),
        )
        .unwrap();
        let src = gray(4, 2);
        crate::frame::fail_next_frame_alloc();
        let err = scale.run_into(&src, &mut out).unwrap_err();
        assert!(matches!(err, FfmpegError::FrameAlloc(_)), "{err}");
        assert_eq!((out.width(), out.height()), (2, 2), "output left intact");
    }
}
