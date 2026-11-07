//! Bounded media-sample probe (the `ffmpeg` feature): demux **and decode** a few
//! already-downloaded HLS segments and report plain-Rust facts. Used by the
//! IPTV-ORG deep probe (stage 4/5) — it proves a stream really delivers decodable
//! video/audio from this host, not merely that a playlist answers.
//!
//! Composes only this crate's safe wrappers ([`Demuxer`], [`StreamVideoDecoder`],
//! [`StreamAudioDecoder`], [`Scaler`]); no new `unsafe`. Bounded by packet and time
//! budgets so a hostile segment cannot spin the blocking worker, and the demuxer
//! carries an `rw_timeout`/interrupt so a wedged read aborts.

use std::io::Write;
use std::time::{Duration, Instant};

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

use crate::convert::MediaKind;
use crate::decode_stream::{StreamAudioDecoder, StreamVideoDecoder};
use crate::demux::{DemuxOptions, Demuxer};
use crate::error::{FfmpegError, Result};
use crate::scale::{ScaleSpec, Scaler};

/// Packets examined before giving up (hostile-input bound).
const MAX_PACKETS: usize = 2_000;
/// Max in-band data sections returned.
const MAX_DATA_SECTIONS: usize = 32;
/// Thumbnail width in pixels.
pub const THUMB_WIDTH: u32 = 320;

/// One video stream's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFacts {
    /// Codec name (`h264`).
    pub codec: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Declared average frame rate as `(num, den)`.
    pub fps: Option<(i64, i64)>,
}

/// One audio stream's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFacts {
    /// Codec name (`aac`).
    pub codec: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// `language` tag, if any.
    pub language: Option<String>,
}

/// A thumbnail as a tightly packed NV12 plane (`w*h` luma + `w*h/2` `CbCr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Thumb {
    /// Width (even).
    pub width: u32,
    /// Height (even).
    pub height: u32,
    /// Packed NV12 bytes.
    pub plane: Vec<u8>,
}

/// What the media stage observed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaSample {
    /// Video streams seen.
    pub video: Vec<VideoFacts>,
    /// Audio streams seen.
    pub audio: Vec<AudioFacts>,
    /// Video frames successfully decoded.
    pub video_frames: u32,
    /// Audio decoded, in milliseconds.
    pub audio_ms: u32,
    /// Whether any decoded video frame carried CEA-608/708 (A53) caption data.
    pub has_cea_captions: bool,
    /// Subtitle stream count (in-band).
    pub subtitle_streams: u32,
    /// Raw in-band SCTE-35 section bytes (packet payloads), capped.
    pub scte35_sections: Vec<Vec<u8>>,
    /// Timed-ID3 packets seen.
    pub id3_packets: u32,
    /// First decoded frame as a thumbnail, when NV12.
    pub thumb: Option<Nv12Thumb>,
    /// Non-fatal notes (decoder errors etc.).
    pub notes: Vec<String>,
}

fn repack_nv12(frame: &ffmpeg::util::frame::Video, w: u32, h: u32) -> Option<Vec<u8>> {
    let (w, h) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?);
    let (y, uv) = (frame.data(0), frame.data(1));
    let (ys, uvs) = (frame.stride(0), frame.stride(1));
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for row in 0..h {
        out.extend_from_slice(y.get(row * ys..row * ys + w)?);
    }
    for row in 0..h / 2 {
        out.extend_from_slice(uv.get(row * uvs..row * uvs + w)?);
    }
    Some(out)
}

fn thumbnail(frame: &ffmpeg::util::frame::Video) -> Option<Nv12Thumb> {
    if frame.format() != Pixel::NV12 || frame.width() < 2 || frame.height() < 2 {
        return None;
    }
    let (src_w, src_h) = (frame.width(), frame.height());
    let out_w = THUMB_WIDTH.min(src_w & !1).max(2);
    let out_h =
        u32::try_from(u64::from(src_h) * u64::from(out_w) / u64::from(src_w)).unwrap_or(2) & !1;
    let out_h = out_h.max(2);
    let mut scaler = Scaler::new(
        ScaleSpec::new(Pixel::NV12, src_w, src_h),
        ScaleSpec::new(Pixel::NV12, out_w, out_h),
    )
    .ok()?;
    let resized = scaler.run(frame).ok()?;
    let plane = repack_nv12(&resized, out_w, out_h)?;
    Some(Nv12Thumb {
        width: out_w,
        height: out_h,
        plane,
    })
}

fn collect_stream_facts(streams: &[crate::demux::StreamParams], sample: &mut MediaSample) {
    for s in streams {
        match s.kind {
            MediaKind::Video => sample.video.push(VideoFacts {
                codec: s.codec_name.clone(),
                width: s.width,
                height: s.height,
                fps: s.avg_frame_rate.map(|r| (r.num, r.den)),
            }),
            MediaKind::Audio => sample.audio.push(AudioFacts {
                codec: s.codec_name.clone(),
                sample_rate: s.sample_rate,
                channels: s.channels,
                language: s.language.clone(),
            }),
            MediaKind::Subtitle => sample.subtitle_streams += 1,
            MediaKind::Other => {}
        }
    }
}

/// Demux + decode `segments` (each already a complete container chunk; for fMP4
/// pass `init + media` concatenated), concatenated into one bounded temp file.
///
/// Returns the observed [`MediaSample`]; stream-level problems are reported in
/// `notes` and as zero counts rather than errors, so the caller can classify
/// `no_media` honestly. Blocking: run on `spawn_blocking`.
///
/// # Errors
///
/// [`FfmpegError`] only when the temp file cannot be written or libav cannot be
/// initialised at all.
#[allow(clippy::too_many_lines)]
pub fn probe_segments(segments: &[Vec<u8>], budget: Duration) -> Result<MediaSample> {
    let started = Instant::now();
    let mut file = tempfile::Builder::new()
        .prefix("mv-probe-")
        .suffix(".ts")
        .tempfile()
        .map_err(|e| FfmpegError::Fetch {
            url: "probe-sample".to_owned(),
            reason: e.to_string(),
        })?;
    for s in segments {
        file.write_all(s).map_err(|e| FfmpegError::Fetch {
            url: "probe-sample".to_owned(),
            reason: e.to_string(),
        })?;
    }
    file.flush().map_err(|e| FfmpegError::Fetch {
        url: "probe-sample".to_owned(),
        reason: e.to_string(),
    })?;

    let mut sample = MediaSample::default();
    let opts = DemuxOptions::new().with_rw_timeout(Duration::from_secs(5));
    let mut demux = match Demuxer::open_with_interrupt(file.path(), opts) {
        Ok(d) => d,
        Err(e) => {
            sample.notes.push(format!("open: {e}"));
            return Ok(sample);
        }
    };
    let streams = demux.streams();
    collect_stream_facts(&streams, &mut sample);
    let scte_idx: Vec<usize> = streams
        .iter()
        .filter(|s| s.kind == MediaKind::Other && s.codec_name.contains("scte"))
        .map(|s| s.index)
        .collect();
    let id3_idx: Vec<usize> = streams
        .iter()
        .filter(|s| s.kind == MediaKind::Other && s.codec_name.contains("id3"))
        .map(|s| s.index)
        .collect();

    let vidx = demux.best_stream(MediaKind::Video);
    let aidx = demux.best_stream(MediaKind::Audio);
    let mut vdec = vidx.and_then(|i| {
        let p = demux.stream_parameters(i)?;
        let tb = streams.iter().find(|s| s.index == i)?.time_base;
        match StreamVideoDecoder::new(p, tb) {
            Ok(d) => Some(d),
            Err(e) => {
                sample.notes.push(format!("video decoder: {e}"));
                None
            }
        }
    });
    let mut adec = aidx.and_then(|i| {
        let p = demux.stream_parameters(i)?;
        let tb = streams.iter().find(|s| s.index == i)?.time_base;
        match StreamAudioDecoder::new(p, tb) {
            Ok(d) => Some(d),
            Err(e) => {
                sample.notes.push(format!("audio decoder: {e}"));
                None
            }
        }
    });

    let want_video = vdec.is_some();
    let want_audio = adec.is_some();
    let mut packets = 0usize;
    let mut audio_samples: u64 = 0;
    let audio_rate = adec.as_ref().map_or(0, StreamAudioDecoder::sample_rate);
    loop {
        let video_done = !want_video || sample.video_frames >= 2;
        let audio_done = !want_audio || sample.audio_ms >= 100;
        if (video_done && audio_done && scte_idx.is_empty() && id3_idx.is_empty())
            || packets >= MAX_PACKETS
            || started.elapsed() > budget
        {
            break;
        }
        let pkt = match demux.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                sample.notes.push(format!("read: {e}"));
                break;
            }
        };
        packets += 1;
        let idx = pkt.stream_index;
        if scte_idx.contains(&idx) {
            if sample.scte35_sections.len() < MAX_DATA_SECTIONS {
                if let Some(d) = pkt.packet.data() {
                    sample.scte35_sections.push(d.to_vec());
                }
            }
            continue;
        }
        if id3_idx.contains(&idx) {
            sample.id3_packets += 1;
            continue;
        }
        if Some(idx) == vidx && !video_done {
            if let Some(dec) = vdec.as_mut() {
                if let Err(e) = dec.send_packet(&pkt.packet) {
                    sample.notes.push(format!("video send: {e}"));
                    continue;
                }
                while let Ok(Some(frame)) = dec.receive_frame() {
                    sample.video_frames += 1;
                    if sample.thumb.is_none() {
                        sample.thumb = thumbnail(&frame.frame);
                    }
                    if frame.a53_cc.is_some() {
                        sample.has_cea_captions = true;
                    }
                }
            }
        } else if Some(idx) == aidx && !audio_done {
            if let Some(dec) = adec.as_mut() {
                if let Err(e) = dec.send_packet(&pkt.packet) {
                    sample.notes.push(format!("audio send: {e}"));
                    continue;
                }
                while let Ok(Some(frame)) = dec.receive_frame() {
                    audio_samples += u64::try_from(frame.frame.samples()).unwrap_or(0);
                }
                if audio_rate > 0 {
                    sample.audio_ms = u32::try_from(audio_samples * 1000 / u64::from(audio_rate))
                        .unwrap_or(u32::MAX);
                }
            }
        }
    }
    Ok(sample)
}
