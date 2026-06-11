//! Safe `libswresample` wrapper (the `ffmpeg` feature).
//!
//! [`Resampler`] owns one `SwrContext` (freed in `Drop` by `ffmpeg_next`) and
//! converts an audio [`frame::Audio`](ffmpeg_next::frame::Audio) between sample
//! formats, channel layouts, and sample rates. The audio subsystem rebases each
//! source to the master clock with exactly this primitive (core-engine §11).
//!
//! `!Sync` by construction; `Send` so it can live on its decode thread.

use ffmpeg::format::Sample;
use ffmpeg::software::resampling::Context as Swr;
use ffmpeg::util::frame::Audio;
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// A `(format, channel layout, rate)` description of a resampler endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ResampleSpec {
    /// Sample format (e.g. packed `S16`, planar `FLTP`).
    pub format: Sample,
    /// Channel layout (e.g. mono, stereo).
    pub channel_layout: ChannelLayout,
    /// Sample rate in Hz.
    pub rate: u32,
}

impl ResampleSpec {
    /// Construct a spec.
    #[must_use]
    pub const fn new(format: Sample, channel_layout: ChannelLayout, rate: u32) -> Self {
        Self {
            format,
            channel_layout,
            rate,
        }
    }
}

/// A libswresample conversion context fixed to one input/output definition.
///
/// `Send` (so it can live on its decode thread): the wrapped `SwrContext` is an
/// owned, movable context (`ffmpeg_next` marks it `Send`), and this wrapper
/// stores only the output **rate** alongside it — deliberately **not** the
/// output [`ChannelLayout`], whose opaque raw pointers would otherwise make the
/// wrapper `!Send`. It is `!Sync` (no shared access). The layout is fixed inside
/// the `SwrContext` at construction, so `flush` only needs the rate.
pub struct Resampler {
    swr: Swr,
    output_rate: u32,
}

impl Resampler {
    /// Build a resampler converting `src` audio to the `dst` definition.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Convert`] if libswresample rejects the request.
    pub fn new(src: ResampleSpec, dst: ResampleSpec) -> Result<Self> {
        let swr = Swr::get(
            src.format,
            src.channel_layout,
            src.rate,
            dst.format,
            dst.channel_layout,
            dst.rate,
        )
        .map_err(FfmpegError::Convert)?;
        Ok(Self {
            swr,
            output_rate: dst.rate,
        })
    }

    /// Resample `input` into a freshly allocated output frame, carrying the
    /// source PTS through unchanged (input time — the caller rebases it).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Convert`] if libswresample reported an error.
    pub fn run(&mut self, input: &Audio) -> Result<Audio> {
        let mut output = Audio::empty();
        self.swr
            .run(input, &mut output)
            .map_err(FfmpegError::Convert)?;
        output.set_pts(input.pts());
        Ok(output)
    }

    /// Drain any samples buffered inside the resampler after the last input
    /// (for async resampling / rate conversion that introduces delay).
    ///
    /// Returns [`None`] once fully drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Convert`] if libswresample reported an error.
    pub fn flush(&mut self) -> Result<Option<Audio>> {
        let mut output = Audio::empty();
        // Allocate a small output buffer for the flush; `flush` fills it with
        // whatever delayed samples remain.
        output.set_rate(self.output_rate);
        match self.swr.flush(&mut output) {
            Ok(_) => {
                if output.samples() == 0 {
                    Ok(None)
                } else {
                    Ok(Some(output))
                }
            }
            Err(e) => Err(FfmpegError::Convert(e)),
        }
    }
}
