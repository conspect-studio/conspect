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
// reason: this single marker assertion is the only `unsafe` outside `hwframe`;
// it is a trait promise about an owned context, not raw FFI.
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
        let mut output = Video::empty();
        self.sws
            .run(input, &mut output)
            .map_err(FfmpegError::Convert)?;
        output.set_pts(input.pts());
        Ok(output)
    }
}
