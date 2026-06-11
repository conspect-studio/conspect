//! Real libav ingest adapters (the `ffmpeg` feature).
//!
//! These adapt `multiview-ffmpeg`'s **safe** demux/decode wrappers to the pure-Rust
//! [`FrameProducer`] trait, so the timing /
//! resilience core in [`crate::source`] drives a genuine libav decode without
//! ever touching libav itself. `multiview-input` keeps `unsafe_code = forbid`: all
//! FFI is owned by `multiview-ffmpeg`; this module only calls its safe API.
//!
//! ## What runs here
//! * [`FileSource`] opens a container with [`Demuxer`] (real demux, real per-
//!   packet input PTS) and proves a real decode by decoding the first video
//!   frame through [`VideoDecoder`], adopting that frame's geometry / pixel
//!   format / color for the stream. It then yields one
//!   [`ProducedFrame`] per video packet, carrying
//!   the decoded geometry and the packet's **raw input** PTS — which the
//!   [`IngestPump`](crate::source::IngestPump) normalizes to a strictly-
//!   monotonic internal-timeline instant (invariant #3) before publishing into
//!   the tile store (invariant #2). RTSP/HLS/TS/SRT reuse the same [`Demuxer`]
//!   with a URL.
//! * [`TestPatternSource`] generates a tiny self-contained clip with the
//!   `ffmpeg` CLI (an **LGPL** software codec — never x264/x265, keeping the
//!   build LGPL-clean) and ingests it as a [`FileSource`].
//!
//! ## Why packet-granular timestamps
//! `multiview-ffmpeg`'s safe `Demuxer` exposes a pure `StreamParams` snapshot but
//! not the libav `codec::Parameters` that `StreamVideoDecoder::new` requires, and
//! this crate may not reach into libav to obtain them (unsafe is forbidden;
//! multiview-ffmpeg owns all FFI). The available safe surface is therefore
//! [`Demuxer`] (per-packet input PTS) + [`VideoDecoder`] (a real decoded frame).
//! For the all-intra LGPL codecs used here, each video packet is exactly one
//! coded frame in display order, so packet PTS == frame PTS and the timeline the
//! pump normalizes is the true per-frame timeline. The first frame is genuinely
//! decoded to NV12-shaped geometry; the remaining frames inherit that geometry
//! and carry their own real demuxed PTS.

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::stream::StreamInventory;
use multiview_core::time::{MediaTime, Rational};

use multiview_ffmpeg::{pixel_to_multiview, Demuxer, MediaKind, VideoDecoder};

use crate::error::{Error, Result};
use crate::normalize::WrapBits;
use crate::source::{FrameProducer, ProducedFrame};

/// The default genpts-fallback cadence (25 fps) when a container declares no
/// average frame rate. Used only to synthesize a PTS for a frame that has none;
/// the real per-frame timeline comes from the demuxed packet PTS.
const DEFAULT_CADENCE: Rational = Rational::FPS_25;

/// A file / URL ingest source over `multiview-ffmpeg`'s safe demux + decode.
///
/// Holds an open [`Demuxer`] reading the video stream's packets, plus the
/// decoded-frame geometry/format/color resolved by decoding the first frame, plus
/// the full typed [`StreamInventory`] of every elementary stream the input offers
/// (RT-1, ADR-0034). `Send`, not `Sync` (the demuxer wraps a libav context that
/// must not be shared across threads unsynchronized — it is driven from one
/// decode task).
pub struct FileSource {
    demuxer: Demuxer,
    video_index: usize,
    timebase: Rational,
    cadence: Rational,
    /// Decoded-frame geometry/format/color adopted from the first decoded frame.
    width: u32,
    height: u32,
    format: PixelFormat,
    color: ColorInfo,
    /// The full inventory of every elementary stream this input offers, probed
    /// once at open. **Additive** — the video decode path still picks best-video;
    /// this no longer throws away the audio/subtitle/data rows (RT-1).
    inventory: StreamInventory,
}

impl FileSource {
    /// Open `path` (a local file) or a URL string usable by libav (RTSP/HLS/TS/
    /// SRT/RTMP all flow through the same demuxer), selecting the best video
    /// stream and decoding its first frame to resolve geometry.
    ///
    /// # Errors
    /// Returns [`Error::Ingest`] if the container cannot be opened, has no video
    /// stream, or the first frame cannot be decoded.
    pub fn open(path: &Path) -> Result<Self> {
        // A real decode of the first frame: proves the decode path runs and
        // resolves the true geometry / pixel format / color of the stream.
        let info = {
            let mut decoder = VideoDecoder::open(path)?;
            decoder.decode_first_frame()?
        };

        let demuxer = Demuxer::open(path)?;

        // RT-1: capture the FULL typed inventory of every elementary stream the
        // input offers (audio tracks + languages, subtitles, data) — additive,
        // probed before any packet read. The video-selection below is unchanged.
        let inventory = demuxer.inventory();

        let video_index = demuxer
            .best_stream(MediaKind::Video)
            .ok_or(Error::Ingest("no video stream in input".to_owned()))?;

        let streams = demuxer.streams();
        let video = streams
            .iter()
            .find(|s| s.index == video_index)
            .ok_or(Error::Ingest(
                "video stream vanished after probe".to_owned(),
            ))?;

        let timebase = video.time_base;
        // Prefer the container's declared average frame rate for the genpts
        // fallback; fall back to 25 fps if it declares none.
        let cadence = video.avg_frame_rate.unwrap_or(DEFAULT_CADENCE);

        // Map the decoded libav pixel format onto a canonical working format;
        // software 4:2:0 (`YUV420P`) and anything unmapped is treated as NV12
        // because the pipeline's decode-to-NV12 step (multiview-ffmpeg) lands every
        // frame on the NV12-throughout timeline (invariant #5).
        let format = pixel_to_multiview(info.format).unwrap_or(PixelFormat::Nv12);

        Ok(Self {
            demuxer,
            video_index,
            timebase,
            cadence,
            width: info.width,
            height: info.height,
            format,
            color: ColorInfo::default(),
            inventory,
        })
    }

    /// The full typed [`StreamInventory`] of every elementary stream this input
    /// offers — every audio track (with BCP-47 language + channel layout), every
    /// subtitle track, and every non-AV data stream (RT-1, ADR-0034 §3).
    ///
    /// This is **additive**: opening a `FileSource` still selects the best video
    /// stream for decode exactly as before, but the previously-discarded rows now
    /// survive here so they can be routed by later RT slices.
    #[must_use]
    pub const fn inventory(&self) -> &StreamInventory {
        &self.inventory
    }

    /// The resolved decoded-frame width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The resolved decoded-frame height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The resolved canonical pixel format.
    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }
}

impl FrameProducer for FileSource {
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>> {
        match self.demuxer.read_packet_for(self.video_index)? {
            None => Ok(None),
            Some(packet) => {
                let meta = FrameMeta {
                    // Replaced by the pump with the normalized instant; the raw
                    // input PTS rides in `raw_pts`.
                    pts: MediaTime::ZERO,
                    width: self.width,
                    height: self.height,
                    format: self.format,
                    color: self.color,
                };
                Ok(Some(ProducedFrame {
                    pixels: Vec::new(),
                    raw_pts: packet.pts(),
                    discontinuity: false,
                    meta,
                }))
            }
        }
    }

    fn timebase(&self) -> Rational {
        self.timebase
    }

    fn cadence(&self) -> Rational {
        self.cadence
    }

    fn wrap_bits(&self) -> WrapBits {
        // A file container exposes a monotonic, non-wrapping timeline; live TS
        // sources would select `WrapBits::Mpeg33`. Files do not wrap.
        WrapBits::None
    }
}

/// A synthetic test-pattern ingest source.
///
/// Generates a tiny self-contained clip with the `ffmpeg` CLI using an **LGPL**
/// software codec (`ffv1`, all-intra — never x264/x265, so the build stays
/// LGPL-clean), then ingests it as a [`FileSource`]. The clip lives in a
/// caller-owned directory (typically a tempdir) for the lifetime of ingest.
pub struct TestPatternSource {
    inner: FileSource,
}

/// Parameters for the generated test pattern.
#[derive(Debug, Clone, Copy)]
pub struct TestPatternSpec {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frame rate (integer fps for the CLI `testsrc` source).
    pub rate: u32,
    /// Clip duration in seconds.
    pub seconds: u32,
}

impl Default for TestPatternSpec {
    fn default() -> Self {
        Self {
            width: 320,
            height: 240,
            rate: 25,
            seconds: 1,
        }
    }
}

impl TestPatternSource {
    /// Generate a test-pattern clip into `dir` per `spec` and open it for ingest.
    ///
    /// The clip is encoded with the LGPL `ffv1` codec (all-intra; every packet a
    /// keyframe in display order) so the per-packet timeline the pump normalizes
    /// is the true per-frame timeline, and the build stays LGPL-clean.
    ///
    /// # Errors
    /// Returns [`Error::Ingest`] if the `ffmpeg` CLI is unavailable, generation
    /// fails, or the produced clip cannot be opened/decoded.
    pub fn generate(dir: &Path, spec: TestPatternSpec) -> Result<Self> {
        let clip = Self::generate_clip(dir, spec)?;
        let inner = FileSource::open(&clip)?;
        Ok(Self { inner })
    }

    /// Generate the clip with the `ffmpeg` CLI, returning its path.
    fn generate_clip(dir: &Path, spec: TestPatternSpec) -> Result<PathBuf> {
        let out = dir.join("multiview-testpattern.mkv");
        let size = format!("{}x{}", spec.width, spec.height);
        let lavfi = format!("testsrc=size={size}:rate={}", spec.rate);
        let duration = spec.seconds.to_string();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &lavfi,
                "-t",
                &duration,
                // LGPL, in-tree software codec — NOT x264/x265 (GPL).
                "-c:v",
                "ffv1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&out)
            .status()
            .map_err(|e| Error::Ingest(format!("failed to spawn the ffmpeg CLI: {e}")))?;
        if !status.success() {
            return Err(Error::Ingest(
                "ffmpeg CLI exited with failure generating the test pattern".to_owned(),
            ));
        }
        if !out.exists() {
            return Err(Error::Ingest(
                "ffmpeg CLI produced no test-pattern file".to_owned(),
            ));
        }
        Ok(out)
    }

    /// The resolved decoded width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.inner.width()
    }

    /// The resolved decoded height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.inner.height()
    }
}

impl FrameProducer for TestPatternSource {
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>> {
        self.inner.next_frame()
    }

    fn timebase(&self) -> Rational {
        self.inner.timebase()
    }

    fn cadence(&self) -> Rational {
        self.inner.cadence()
    }

    fn wrap_bits(&self) -> WrapBits {
        self.inner.wrap_bits()
    }
}
