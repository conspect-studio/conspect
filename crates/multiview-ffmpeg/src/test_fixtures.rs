//! Test-only **caption MPEG-TS fixture generators** (`test-fixtures` feature,
//! `#[doc(hidden)]`): a DVB-subtitle clip and an embedded-CEA-608 clip.
//!
//! The Debian/CI `FFmpeg` 7.1 CLI cannot build either fixture: it cannot
//! transcode a text subtitle into a bitmap `dvbsub` stream ("Subtitle encoding
//! currently only possible from text to text or bitmap to bitmap"; the
//! `text2graphicsub` filter is not compiled), and it has no source that attaches
//! known `AV_FRAME_DATA_A53_CC` side data to a video frame for `-a53cc` to
//! forward. So both clips are built directly through libav:
//!
//! * [`generate_dvbsub_ts`] — an in-tree LGPL `mpeg2video` program plus a
//!   `dvbsub` (codec `dvb_subtitle`) bitmap subtitle cue, muxed into MPEG-TS.
//! * [`generate_a53_cc_ts`] — an `mpeg2video` program whose frames each carry one
//!   EIA-608 word as A53 cc-data side data, encoded with `a53cc=1` so a `cc_dec`
//!   over the decoded frames recovers the caption ([`A53_CAPTION_TEXT`]).
//!
//! Both are LGPL-clean (`mpeg2video` / in-tree `dvbsub` / linked `cc_dec`, no
//! x264/x265). This lives in `multiview-ffmpeg` (the **only** crate allowed
//! `unsafe` for raw libav* FFI) so the strictly-`forbid(unsafe)` `multiview-cli`
//! test can call it across the crate boundary; the `multiview-ffmpeg` demux test
//! uses it too. It is gated behind the off-by-default `test-fixtures` feature and
//! `#[doc(hidden)]` — it is **not** part of the product API. There is no safe
//! `ffmpeg_next` constructor for an outbound `AVSubtitle` bitmap rect, so the
//! rect is poked through the raw FFI; every `unsafe` block is bounded to
//! libav-owned buffers we allocate here and carries a `// SAFETY:` note (the
//! crate is `unsafe = deny`).

// reason: this is a libav fixture generator; raw `AVSubtitle`/`AVSubtitleRect`
// FFI has no safe `ffmpeg_next` wrapper. Every `unsafe` block has a `// SAFETY:`.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// The known media instant (seconds, on the rendition's 0-based timeline) the
/// single cue in [`generate_hls_with_valid_webvtt`] becomes visible.
pub const WEBVTT_CUE_START_S: i32 = 1;
/// The media instant (seconds) the [`generate_hls_with_valid_webvtt`] cue ends.
pub const WEBVTT_CUE_END_S: i32 = 3;
/// The exact text of the cue carried by [`generate_hls_with_valid_webvtt`]; the
/// isolated `WebVTT` reader must recover this verbatim.
pub const WEBVTT_CUE_TEXT: &str = "OFFLINE WEBVTT CUE";

/// The program canvas width of the generated clip (px).
pub const WIDTH: i32 = 640;
/// The program canvas height (px).
pub const HEIGHT: i32 = 480;
/// The output frame rate (fps).
pub const FPS: i32 = 25;
/// The clip duration (seconds).
pub const DURATION_S: i32 = 6;
/// The media second the cue becomes visible.
pub const CUE_START_S: i32 = 1;

/// The subtitle bitmap rect, in 640x480 source pixels (a wide caption band near
/// the bottom of the frame — what the per-tile burn-in must reproduce).
pub const RECT_X: i32 = 120;
/// Top of the subtitle bitmap rect (source px).
pub const RECT_Y: i32 = 380;
/// Width of the subtitle bitmap rect (source px).
pub const RECT_W: i32 = 400;
/// Height of the subtitle bitmap rect (source px).
pub const RECT_H: i32 = 60;

/// Generate the DVB-sub MPEG-TS fixture at `path`: an `mpeg2video` program plus
/// one `dvbsub` cue active from [`CUE_START_S`] (held for the decoder's default
/// on-screen duration), tagged `language=eng`.
///
/// # Errors
/// Returns [`FfmpegError`] if any libav allocation/open/encode/mux step fails.
pub fn generate_dvbsub_ts(path: &Path) -> Result<()> {
    crate::decode::ensure_initialized()?;
    // SAFETY: a single-threaded libav build sequence. We own every context /
    // stream / packet / frame we allocate below and free them before returning;
    // each inner block documents the buffers it touches.
    unsafe { generate_inner(path) }
}

/// Map a libav negative return code into a typed [`FfmpegError`].
fn ff_err(code: i32) -> FfmpegError {
    FfmpegError::Mux(ffmpeg::Error::from(code))
}

/// Bail with [`FfmpegError::Mux`] carrying `ffmpeg::Error::Bug` for a libav call
/// that returned an unexpected null/short value (no errno to map).
fn bug() -> FfmpegError {
    FfmpegError::Mux(ffmpeg::Error::Bug)
}

#[allow(clippy::too_many_lines)]
unsafe fn generate_inner(path: &Path) -> Result<()> {
    let cpath = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| bug())?;
    let cfmt = CString::new("mpegts").map_err(|_| bug())?;

    let mut oc: *mut ffi::AVFormatContext = ptr::null_mut();
    // SAFETY: out-params are a fresh null pointer + valid CStrings; libav fills
    // `oc` on success (checked) and we own it until `avformat_free_context`.
    let r = ffi::avformat_alloc_output_context2(
        &raw mut oc,
        ptr::null_mut(),
        cfmt.as_ptr(),
        cpath.as_ptr(),
    );
    if r < 0 || oc.is_null() {
        return Err(ff_err(r));
    }

    // RAII-ish: ensure we free the context on every early return below.
    let result = build_all(oc, &cpath);

    // SAFETY: `oc` is the live context we allocated; freeing it releases all
    // streams it owns. Done exactly once.
    ffi::avformat_free_context(oc);
    result
}

/// Build both streams, write the cue + the video program, and finalise. Split
/// out so the caller can free `oc` regardless of where this returns.
unsafe fn build_all(oc: *mut ffi::AVFormatContext, cpath: &CString) -> Result<()> {
    let (vst, vctx) = add_video_stream(oc)?;
    let sst = add_subtitle_stream(oc)?;

    // SAFETY: `oc` is live; `oformat` is set by alloc. Reading its `flags` is a
    // plain field read.
    let needs_file = ((*(*oc).oformat).flags & ffi::AVFMT_NOFILE) == 0;
    if needs_file {
        // SAFETY: `pb` out-param of the live context; `cpath` is a valid CString.
        let r = ffi::avio_open(&raw mut (*oc).pb, cpath.as_ptr(), ffi::AVIO_FLAG_WRITE);
        if r < 0 {
            return Err(ff_err(r));
        }
    }

    // SAFETY: header write on the live, fully-configured context.
    let r = ffi::avformat_write_header(oc, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }

    encode_subtitle_cue(oc, sst)?;
    encode_video(oc, vst, vctx)?;

    // SAFETY: trailer on the live context after a successful header + frames.
    let r = ffi::av_write_trailer(oc);
    if r < 0 {
        return Err(ff_err(r));
    }
    if needs_file {
        // SAFETY: closes + nulls the `pb` we opened above.
        ffi::avio_closep(&raw mut (*oc).pb);
    }
    // Free the encoder context we kept for the video stream (the subtitle stream
    // is configured directly on its codecpar, no context retained).
    let mut vctx_p = vctx;
    // SAFETY: `vctx` is the encoder context we allocated + opened; freed once.
    ffi::avcodec_free_context(&raw mut vctx_p);
    Ok(())
}

/// Add an `mpeg2video` stream to `oc`, returning `(stream, opened encoder ctx)`.
unsafe fn add_video_stream(
    oc: *mut ffi::AVFormatContext,
) -> Result<(*mut ffi::AVStream, *mut ffi::AVCodecContext)> {
    // SAFETY: encoder lookup by id; returns a static codec descriptor or null.
    let vcodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_MPEG2VIDEO);
    if vcodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure (checked).
    let vst = ffi::avformat_new_stream(oc, ptr::null());
    if vst.is_null() {
        return Err(bug());
    }
    // SAFETY: allocates an encoder context for the found codec; null on failure.
    let vctx = ffi::avcodec_alloc_context3(vcodec);
    if vctx.is_null() {
        return Err(bug());
    }
    // SAFETY: `vctx` is a fresh, exclusively-owned context; plain field writes.
    (*vctx).width = WIDTH;
    (*vctx).height = HEIGHT;
    (*vctx).time_base = ffi::AVRational { num: 1, den: FPS };
    (*vctx).framerate = ffi::AVRational { num: FPS, den: 1 };
    (*vctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*vctx).gop_size = FPS;
    (*vctx).bit_rate = 2_000_000;
    // SAFETY: `oformat` flags read on the live context.
    if ((*(*oc).oformat).flags & ffi::AVFMT_GLOBALHEADER) != 0 {
        let flag = i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).map_err(|_| bug())?;
        (*vctx).flags |= flag;
    }
    // SAFETY: opens the encoder on its own context.
    let r = ffi::avcodec_open2(vctx, vcodec, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: copies opened-encoder params into the stream's codecpar.
    let r = ffi::avcodec_parameters_from_context((*vst).codecpar, vctx);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write on the owned stream.
    (*vst).time_base = (*vctx).time_base;
    Ok((vst, vctx))
}

/// Add a `dvbsub` subtitle stream to `oc`, tagged `language=eng`, returning it.
unsafe fn add_subtitle_stream(oc: *mut ffi::AVFormatContext) -> Result<*mut ffi::AVStream> {
    // SAFETY: encoder lookup by id; static descriptor or null.
    let scodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_DVB_SUBTITLE);
    if scodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure.
    let sst = ffi::avformat_new_stream(oc, ptr::null());
    if sst.is_null() {
        return Err(bug());
    }
    // SAFETY: a fresh encoder context for the dvbsub encoder; freed below.
    let sctx = ffi::avcodec_alloc_context3(scodec);
    if sctx.is_null() {
        return Err(bug());
    }
    // SAFETY: plain field writes on the fresh, owned context.
    (*sctx).width = WIDTH;
    (*sctx).height = HEIGHT;
    (*sctx).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    // SAFETY: opens the dvbsub encoder so its codecpar (the dvbsub composition
    // descriptor) is populated, then copies it into the stream.
    let r = ffi::avcodec_open2(sctx, scodec, ptr::null_mut());
    if r < 0 {
        let mut sctx_p = sctx;
        ffi::avcodec_free_context(&raw mut sctx_p);
        return Err(ff_err(r));
    }
    let r = ffi::avcodec_parameters_from_context((*sst).codecpar, sctx);
    let mut sctx_p = sctx;
    // SAFETY: free the encoder context now its params are copied to the stream.
    ffi::avcodec_free_context(&raw mut sctx_p);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write + a metadata dict set on the owned stream.
    (*sst).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    let key = CString::new("language").map_err(|_| bug())?;
    let val = CString::new("eng").map_err(|_| bug())?;
    ffi::av_dict_set(&raw mut (*sst).metadata, key.as_ptr(), val.as_ptr(), 0);
    Ok(sst)
}

/// Encode one PAL8 bitmap DVB-sub cue (a solid white band) and mux it.
unsafe fn encode_subtitle_cue(
    oc: *mut ffi::AVFormatContext,
    sst: *mut ffi::AVStream,
) -> Result<()> {
    // The subtitle stream's encoder context was freed after configuring the
    // stream; build a fresh one to encode the cue (libav allows this — the
    // encoder is stateless per call for dvbsub).
    let scodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_DVB_SUBTITLE);
    if scodec.is_null() {
        return Err(bug());
    }
    let sctx = ffi::avcodec_alloc_context3(scodec);
    if sctx.is_null() {
        return Err(bug());
    }
    // SAFETY: fresh owned context; field writes then open.
    (*sctx).width = WIDTH;
    (*sctx).height = HEIGHT;
    (*sctx).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    let r = ffi::avcodec_open2(sctx, scodec, ptr::null_mut());
    if r < 0 {
        let mut sctx_p = sctx;
        ffi::avcodec_free_context(&raw mut sctx_p);
        return Err(ff_err(r));
    }

    let res = encode_and_write_cue(oc, sst, sctx);
    let mut sctx_p = sctx;
    // SAFETY: free the cue-encoder context exactly once.
    ffi::avcodec_free_context(&raw mut sctx_p);
    res
}

/// Build the `AVSubtitle` with one PAL8 bitmap rect, encode it on `sctx`, and
/// mux the resulting packet on `sst`.
unsafe fn encode_and_write_cue(
    oc: *mut ffi::AVFormatContext,
    sst: *mut ffi::AVStream,
    sctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let area =
        usize::try_from(RECT_W).map_err(|_| bug())? * usize::try_from(RECT_H).map_err(|_| bug())?;
    // Every pixel -> palette index 1 (opaque white); index 0 is transparent.
    let mut indices = vec![1u8; area];
    // RGB32 palette (native-endian u32; B,G,R,A on a little-endian host).
    let mut palette = vec![0u32; 256];
    if let Some(slot) = palette.get_mut(1) {
        *slot = 0xFFFF_FFFF;
    }

    let mut rect = ffi::AVSubtitleRect {
        x: RECT_X,
        y: RECT_Y,
        w: RECT_W,
        h: RECT_H,
        nb_colors: 256,
        data: [ptr::null_mut(); 4],
        linesize: [0; 4],
        flags: 0,
        type_: ffi::AVSubtitleType::SUBTITLE_BITMAP,
        text: ptr::null_mut(),
        ass: ptr::null_mut(),
    };
    // SAFETY: `indices`/`palette` outlive the encode call below; the encoder
    // reads `data[0]` as `linesize[0] * h` PAL8 bytes and `data[1]` as the CLUT.
    rect.data[0] = indices.as_mut_ptr();
    rect.linesize[0] = RECT_W;
    rect.data[1] = palette.as_mut_ptr().cast::<u8>();
    rect.linesize[1] = 256 * 4;

    let mut rect_ptr: *mut ffi::AVSubtitleRect = &raw mut rect;
    // On-screen hold (ms) the encoder records. The `dvbsub` decoder ignores this
    // and applies its own default on-screen duration, but a non-zero value keeps
    // the encoded segment well-formed. 3 s.
    let hold_ms: u32 = 3000;
    let sub = ffi::AVSubtitle {
        format: 0, // graphics/bitmap
        start_display_time: 0,
        end_display_time: hold_ms,
        num_rects: 1,
        rects: &raw mut rect_ptr,
        pts: i64::from(CUE_START_S) * 1_000_000, // AV_TIME_BASE (microseconds)
    };

    let mut buf = vec![0u8; 1 << 20];
    let cap = i32::try_from(buf.len()).map_err(|_| bug())?;
    // SAFETY: `buf` is a writable owned slice of `cap` bytes; `sub` + its rect
    // are fully populated and outlive the call.
    let n = ffi::avcodec_encode_subtitle(sctx, buf.as_mut_ptr(), cap, &raw const sub);
    if n <= 0 {
        return Err(ff_err(n));
    }

    // SAFETY: allocate a packet sized to the encoded bytes, copy them in.
    let pkt = ffi::av_packet_alloc();
    if pkt.is_null() {
        return Err(bug());
    }
    let r = ffi::av_new_packet(pkt, n);
    if r < 0 {
        let mut pkt_p = pkt;
        ffi::av_packet_free(&raw mut pkt_p);
        return Err(ff_err(r));
    }
    let len = usize::try_from(n).map_err(|_| bug())?;
    // SAFETY: `pkt.data` is `n` bytes (just allocated); `buf` holds `>= n` bytes.
    ptr::copy_nonoverlapping(buf.as_ptr(), (*pkt).data, len);
    (*pkt).stream_index = (*sst).index;
    let pts = i64::from(CUE_START_S) * 90_000;
    (*pkt).pts = pts;
    (*pkt).dts = pts;
    (*pkt).duration = i64::from(hold_ms) * 90; // ms -> 90 kHz ticks
                                               // SAFETY: interleaved write on the live container; libav takes the packet.
    let r = ffi::av_interleaved_write_frame(oc, pkt);
    let mut pkt_p = pkt;
    ffi::av_packet_free(&raw mut pkt_p);
    if r < 0 {
        return Err(ff_err(r));
    }
    Ok(())
}

/// Encode `DURATION_S * FPS` grey video frames and mux them.
unsafe fn encode_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let w = u32::try_from(WIDTH).map_err(|_| bug())?;
    let h = u32::try_from(HEIGHT).map_err(|_| bug())?;
    // Use `ffmpeg_next`'s safe `frame::Video` so the pixel-format discriminant +
    // backing-buffer allocation are handled without an `as`-cast on the C enum.
    let mut video = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);

    let total = DURATION_S * FPS;
    for i in 0..i64::from(total) {
        // SAFETY: `video.as_mut_ptr()` is the live owned frame; we fill its
        // planes via the raw pointers (the safe API has no constant-fill).
        let frame = video.as_mut_ptr();
        let r = ffi::av_frame_make_writable(frame);
        if r < 0 {
            return Err(ff_err(r));
        }
        // A dim grey field: Y ~ 40, neutral chroma 128.
        fill_plane((*frame).data[0], (*frame).linesize[0], WIDTH, HEIGHT, 40);
        fill_plane(
            (*frame).data[1],
            (*frame).linesize[1],
            WIDTH / 2,
            HEIGHT / 2,
            128,
        );
        fill_plane(
            (*frame).data[2],
            (*frame).linesize[2],
            WIDTH / 2,
            HEIGHT / 2,
            128,
        );
        (*frame).pts = i;
        drain_video(oc, vst, vctx, frame)?;
    }
    // Flush.
    let r = ffi::avcodec_send_frame(vctx, ptr::null());
    if r < 0 {
        return Err(ff_err(r));
    }
    drain_video(oc, vst, vctx, ptr::null_mut())
}

/// Send `frame` (or null to flush) and mux every packet it yields.
unsafe fn drain_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
) -> Result<()> {
    if !frame.is_null() {
        // SAFETY: send a valid frame to the opened encoder.
        let r = ffi::avcodec_send_frame(vctx, frame);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    let pkt = ffi::av_packet_alloc();
    if pkt.is_null() {
        return Err(bug());
    }
    let result = (|| -> Result<()> {
        loop {
            // SAFETY: drains buffered packets from the encoder into `pkt`.
            let r = ffi::avcodec_receive_packet(vctx, pkt);
            if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                return Ok(());
            }
            if r < 0 {
                return Err(ff_err(r));
            }
            (*pkt).stream_index = (*vst).index;
            ffi::av_packet_rescale_ts(pkt, (*vctx).time_base, (*vst).time_base);
            let r = ffi::av_interleaved_write_frame(oc, pkt);
            if r < 0 {
                return Err(ff_err(r));
            }
        }
    })();
    let mut pkt_p = pkt;
    // SAFETY: free the drain packet exactly once.
    ffi::av_packet_free(&raw mut pkt_p);
    result
}

/// Fill a single plane with a constant byte (`linesize` row stride).
unsafe fn fill_plane(data: *mut u8, linesize: i32, w: i32, h: i32, val: u8) {
    if data.is_null() || w <= 0 || h <= 0 {
        return;
    }
    let Ok(width) = usize::try_from(w) else {
        return;
    };
    for row in 0..h {
        let Ok(stride) = isize::try_from(i64::from(row) * i64::from(linesize)) else {
            return;
        };
        // SAFETY: row<h and `stride` is within the plane allocated by libav at
        // `linesize * h`; we write `width <= linesize` bytes per row.
        let line = data.offset(stride);
        ptr::write_bytes(line, val, width);
    }
}

// ---------------------------------------------------------------------------
// Embedded CEA-608 (A53_CC) MPEG-TS fixture
// ---------------------------------------------------------------------------
//
// The FFmpeg CLI cannot inject a known EIA-608 caption into a *video* bitstream
// as `AV_FRAME_DATA_A53_CC` side data: `-a53cc 1` only *forwards* A53 side data
// that is already on the encoder's input frames, and the CLI has no source that
// attaches it. So, like the DVB-sub fixture above, this builds the clip directly
// through libav: a small `mpeg2video` program whose input frames each carry one
// EIA-608 control/character word as an A53 cc-data triplet, encoded with
// `a53cc=1` so the words land in the bitstream and a `cc_dec` over the decoded
// frames recovers them. `mpeg2video` is LGPL — no x264/x265 (captions.md §9).

/// The caption text the [`generate_a53_cc_ts`] fixture burns into the video as
/// EIA-608 closed captions; a `cc_dec` over the decoded A53 side data recovers it.
pub const A53_CAPTION_TEXT: &str = "HELLO WORLD";

/// The output frame rate of the A53 fixture (fps).
pub const A53_FPS: i32 = 25;

/// Build the EIA-608 control/character word sequence for a pop-on caption of
/// [`A53_CAPTION_TEXT`]: Resume-Caption-Loading, Erase-Non-displayed-Memory, a
/// preamble address code (row 15), the odd-parity character pairs, then
/// End-Of-Caption (which is what makes `cc_dec` emit the cue). Each entry is a
/// two-byte 608 word; both bytes carry odd parity.
fn eia608_words() -> Vec<(u8, u8)> {
    // Two-byte control codes (channel 1, field 1).
    const RCL: (u8, u8) = (0x14, 0x20); // Resume Caption Loading (pop-on)
    const ENM: (u8, u8) = (0x14, 0x2e); // Erase Non-displayed Memory
    const PAC15: (u8, u8) = (0x14, 0x70); // Preamble Address Code, row 15
    const EOC: (u8, u8) = (0x14, 0x2f); // End Of Caption (swap to display)

    let mut words = vec![RCL, ENM, PAC15];
    let mut chars: Vec<u8> = A53_CAPTION_TEXT.bytes().collect();
    if chars.len() % 2 == 1 {
        chars.push(0x00); // pad to whole 608 words with a null
    }
    for pair in chars.chunks_exact(2) {
        if let [a, b] = *pair {
            words.push((a, b));
        }
    }
    words.push(EOC);
    words
}

/// Set the high bit of a 7-bit EIA-608 byte so the total number of set bits is
/// odd (the line-21 parity rule the decoder validates).
fn odd_parity(byte: u8) -> u8 {
    let low = byte & 0x7f;
    if low.count_ones() % 2 == 0 {
        low | 0x80
    } else {
        low
    }
}

/// Wrap one EIA-608 word as a single A53 cc-data triplet: a `0xFC` marker
/// (`cc_valid = 1`, `cc_type = 0` → field-1 line-21) followed by the two
/// odd-parity data bytes. This is exactly the `AV_FRAME_DATA_A53_CC` payload an
/// H.264/MPEG-2 decoder reproduces.
fn a53_triplet(word: (u8, u8)) -> [u8; 3] {
    [0xFC, odd_parity(word.0), odd_parity(word.1)]
}

/// Generate an MPEG-TS clip at `path` carrying [`A53_CAPTION_TEXT`] as embedded
/// EIA-608 closed captions in the `mpeg2video` bitstream (one 608 word per frame
/// as `AV_FRAME_DATA_A53_CC` side data, encoded with `a53cc=1`). Decoding the
/// video and feeding the recovered A53 side data to `cc_dec` reproduces the text.
///
/// The clip is `A53_FPS`-cadence, 64×64, and runs a few frames past the last
/// caption word so the encoder fully flushes.
///
/// # Errors
/// Returns [`FfmpegError`] if any libav allocation/open/encode/mux step fails.
pub fn generate_a53_cc_ts(path: &Path) -> Result<()> {
    crate::decode::ensure_initialized()?;
    // SAFETY: a single-threaded libav build sequence. We own every context /
    // stream / packet / frame allocated below and free them before returning;
    // each inner block documents the buffers it touches.
    unsafe { generate_a53_inner(path) }
}

unsafe fn generate_a53_inner(path: &Path) -> Result<()> {
    let cpath = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| bug())?;
    let cfmt = CString::new("mpegts").map_err(|_| bug())?;

    let mut oc: *mut ffi::AVFormatContext = ptr::null_mut();
    // SAFETY: fresh null out-param + valid CStrings; libav fills `oc` on success.
    let r = ffi::avformat_alloc_output_context2(
        &raw mut oc,
        ptr::null_mut(),
        cfmt.as_ptr(),
        cpath.as_ptr(),
    );
    if r < 0 || oc.is_null() {
        return Err(ff_err(r));
    }
    let result = build_a53_all(oc, &cpath);
    // SAFETY: free the context we allocated exactly once (releases its streams).
    ffi::avformat_free_context(oc);
    result
}

/// Add an `mpeg2video` stream with `a53cc=1`, encode the caption-bearing frames,
/// and finalise the container.
unsafe fn build_a53_all(oc: *mut ffi::AVFormatContext, cpath: &CString) -> Result<()> {
    let (vst, vctx) = add_a53_video_stream(oc)?;

    // SAFETY: `oformat` flags read on the live context.
    let needs_file = ((*(*oc).oformat).flags & ffi::AVFMT_NOFILE) == 0;
    if needs_file {
        // SAFETY: `pb` out-param of the live context; `cpath` is a valid CString.
        let r = ffi::avio_open(&raw mut (*oc).pb, cpath.as_ptr(), ffi::AVIO_FLAG_WRITE);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    // SAFETY: header write on the live, fully-configured context.
    let r = ffi::avformat_write_header(oc, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }

    let res = encode_a53_video(oc, vst, vctx);

    if res.is_ok() {
        // SAFETY: trailer on the live context after a successful header + frames.
        let r = ffi::av_write_trailer(oc);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    if needs_file {
        // SAFETY: closes + nulls the `pb` we opened above.
        ffi::avio_closep(&raw mut (*oc).pb);
    }
    let mut vctx_p = vctx;
    // SAFETY: free the encoder context we allocated + opened, exactly once.
    ffi::avcodec_free_context(&raw mut vctx_p);
    res
}

/// Add an `mpeg2video` stream with the `a53cc` private option enabled, returning
/// `(stream, opened encoder ctx)`.
unsafe fn add_a53_video_stream(
    oc: *mut ffi::AVFormatContext,
) -> Result<(*mut ffi::AVStream, *mut ffi::AVCodecContext)> {
    // SAFETY: encoder lookup by id; static codec descriptor or null.
    let vcodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_MPEG2VIDEO);
    if vcodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure.
    let vst = ffi::avformat_new_stream(oc, ptr::null());
    if vst.is_null() {
        return Err(bug());
    }
    // SAFETY: allocates an encoder context for the found codec; null on failure.
    let vctx = ffi::avcodec_alloc_context3(vcodec);
    if vctx.is_null() {
        return Err(bug());
    }
    // SAFETY: `vctx` is a fresh, exclusively-owned context; plain field writes.
    (*vctx).width = 64;
    (*vctx).height = 64;
    (*vctx).time_base = ffi::AVRational {
        num: 1,
        den: A53_FPS,
    };
    (*vctx).framerate = ffi::AVRational {
        num: A53_FPS,
        den: 1,
    };
    (*vctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*vctx).gop_size = A53_FPS;
    (*vctx).bit_rate = 1_000_000;
    // Enable A53 closed-caption forwarding so the per-frame A53 side data is
    // written into the bitstream.
    let key = CString::new("a53cc").map_err(|_| bug())?;
    let val = CString::new("1").map_err(|_| bug())?;
    // SAFETY: `priv_data` is the mpeg2video encoder's private AVOptions block,
    // valid on the freshly allocated (not-yet-opened) context; `key`/`val` are
    // valid CStrings living for the call.
    ffi::av_opt_set((*vctx).priv_data, key.as_ptr(), val.as_ptr(), 0);

    // SAFETY: `oformat` flags read on the live context.
    if ((*(*oc).oformat).flags & ffi::AVFMT_GLOBALHEADER) != 0 {
        let flag = i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).map_err(|_| bug())?;
        (*vctx).flags |= flag;
    }
    // SAFETY: opens the encoder on its own context.
    let r = ffi::avcodec_open2(vctx, vcodec, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: copies opened-encoder params into the stream's codecpar.
    let r = ffi::avcodec_parameters_from_context((*vst).codecpar, vctx);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write on the owned stream.
    (*vst).time_base = (*vctx).time_base;
    Ok((vst, vctx))
}

/// Encode the caption-bearing frames: one 608 word per frame as A53 side data
/// (followed by a few caption-free frames so the encoder flushes cleanly).
unsafe fn encode_a53_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let words = eia608_words();
    let total = i64::try_from(words.len()).map_err(|_| bug())? + 6;

    for i in 0..total {
        // Use the safe `frame::Video` so the pixel-format discriminant + backing
        // allocation are handled without an `as`-cast on the C enum.
        let mut video = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, 64, 64);
        // SAFETY: `video` is the live owned frame; fill its planes via raw pointers
        // (the safe API has no constant-fill).
        {
            let frame = video.as_mut_ptr();
            let r = ffi::av_frame_make_writable(frame);
            if r < 0 {
                return Err(ff_err(r));
            }
            fill_plane((*frame).data[0], (*frame).linesize[0], 64, 64, 40);
            fill_plane((*frame).data[1], (*frame).linesize[1], 32, 32, 128);
            fill_plane((*frame).data[2], (*frame).linesize[2], 32, 32, 128);
            (*frame).pts = i;
        }
        // Attach this frame's 608 word as one A53 cc-data triplet.
        if let Some(word) = words.get(usize::try_from(i).unwrap_or(usize::MAX)) {
            let trip = a53_triplet(*word);
            attach_a53_side_data(&mut video, &trip)?;
        }
        // SAFETY: `video.as_ptr()` is the live owned frame, fully populated.
        let frame = video.as_ptr();
        drain_a53_video(oc, vst, vctx, frame)?;
    }
    // SAFETY: flush the encoder with a null frame, then drain.
    let r = ffi::avcodec_send_frame(vctx, ptr::null());
    if r < 0 {
        return Err(ff_err(r));
    }
    drain_a53_video(oc, vst, vctx, ptr::null())
}

/// Attach `bytes` to `video` as `AV_FRAME_DATA_A53_CC` side data via the safe
/// `ffmpeg_next` allocator, copying the bytes into the libav-owned buffer.
unsafe fn attach_a53_side_data(video: &mut ffmpeg::frame::Video, bytes: &[u8]) -> Result<()> {
    use ffmpeg::util::frame::side_data::Type as SideDataType;
    let mut sd = video
        .new_side_data(SideDataType::A53CC, bytes.len())
        .ok_or_else(bug)?;
    // SAFETY: `new_side_data` allocated a `bytes.len()`-byte buffer on the frame;
    // `sd.as_mut_ptr()` points at the live `AVFrameSideData` whose `data` field
    // is that buffer. We write exactly `bytes.len()` bytes into it.
    let dst = std::slice::from_raw_parts_mut((*sd.as_mut_ptr()).data, bytes.len());
    dst.copy_from_slice(bytes);
    Ok(())
}

/// Send `frame` (or null to flush) to the encoder and mux every packet it yields.
unsafe fn drain_a53_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
    frame: *const ffi::AVFrame,
) -> Result<()> {
    if !frame.is_null() {
        // SAFETY: send a valid, fully-populated frame to the opened encoder.
        let r = ffi::avcodec_send_frame(vctx, frame);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    let pkt = ffi::av_packet_alloc();
    if pkt.is_null() {
        return Err(bug());
    }
    let result = (|| -> Result<()> {
        loop {
            // SAFETY: drains buffered packets from the encoder into `pkt`.
            let r = ffi::avcodec_receive_packet(vctx, pkt);
            if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                return Ok(());
            }
            if r < 0 {
                return Err(ff_err(r));
            }
            (*pkt).stream_index = (*vst).index;
            ffi::av_packet_rescale_ts(pkt, (*vctx).time_base, (*vst).time_base);
            let r = ffi::av_interleaved_write_frame(oc, pkt);
            if r < 0 {
                return Err(ff_err(r));
            }
        }
    })();
    let mut pkt_p = pkt;
    // SAFETY: free the drain packet exactly once.
    ffi::av_packet_free(&raw mut pkt_p);
    result
}

// ---------------------------------------------------------------------------
// HLS master + WebVTT-subtitle-rendition fixtures (fully offline, `file://`)
// ---------------------------------------------------------------------------
//
// These reproduce the ABC-News-AU class of master playlist: an
// `EXT-X-MEDIA:TYPE=SUBTITLES` WebVTT rendition alongside a video variant. libav
// folds the rendition into the *one* shared `AVFormatContext` it opens for the
// video, so a corrupt/expired `.vtt` segment either aborts `avformat_open_input`
// or makes `av_read_frame` return that rendition's error for the whole context —
// killing the video tile. The fix (ADR-T011) discards the unrouted subtitle
// stream in the main demuxer; the isolated `read_captions` reader is the sole
// WebVTT path. Both fixtures are written entirely on disk (a short LGPL
// `mpeg2video` TS segment + hand-written playlists + a `.vtt`), referenced by
// `file://` URLs, so the end-to-end tests need no network.
//
// The TS segment is built with the same LGPL `mpeg2video` encoder as the
// captions fixtures above (no x264/x265). The playlists are plain text.

/// The duration (seconds) of the single media segment the HLS fixtures carry.
const HLS_SEGMENT_S: i32 = 2;
/// The frame rate of the HLS fixtures' video segment (fps).
const HLS_FPS: i32 = 25;
/// The pixel width of the HLS fixtures' video segment (small for a fast decode
/// in CI).
const HLS_W: i32 = 160;
/// The HLS fixtures' video segment height.
const HLS_H: i32 = 120;

/// Generate, under `dir`, an HLS master playlist whose `TYPE=SUBTITLES` `WebVTT`
/// rendition's first `.vtt` segment is **deliberately corrupt** (garbage bytes,
/// no `WEBVTT` header) — the ABC-News-AU failure shape. Writes:
///
/// * `master.m3u8` — an `EXT-X-MEDIA:TYPE=SUBTITLES` group plus a video variant,
/// * `video.m3u8` + `seg0.ts` — a short LGPL `mpeg2video` MPEG-TS segment,
/// * `subs.m3u8` + `subs0.vtt` — the corrupt subtitle rendition.
///
/// The main demuxer opened on `master.m3u8` must keep decoding the video despite
/// the broken `WebVTT` rendition (the fix); the isolated reader is the only path
/// that would touch the `.vtt`.
///
/// # Errors
/// Returns [`FfmpegError`] if any libav encode/mux step or file write fails.
pub fn generate_hls_with_broken_webvtt(dir: &Path) -> Result<()> {
    crate::decode::ensure_initialized()?;
    write_ts_segment(&dir.join("seg0.ts"))?;
    write_text(&dir.join("video.m3u8"), &media_playlist_for("seg0.ts"))?;
    // A corrupt first segment: control/high bytes with NO `WEBVTT` signature, so
    // the WebVTT demuxer errors on "loading first segment".
    write_text(
        &dir.join("subs0.vtt"),
        "\u{0}\u{1}\u{2}NOT-A-WEBVTT-FILE\u{7f}garbage cue payload\n",
    )?;
    write_text(&dir.join("subs.m3u8"), &media_playlist_for("subs0.vtt"))?;
    write_text(&dir.join("master.m3u8"), &master_playlist(dir))?;
    Ok(())
}

/// Generate, under `dir`, an HLS master playlist whose `TYPE=SUBTITLES` `WebVTT`
/// rendition carries **one valid cue** ([`WEBVTT_CUE_TEXT`], on screen
/// [`WEBVTT_CUE_START_S`]–[`WEBVTT_CUE_END_S`]). Same file layout as
/// [`generate_hls_with_broken_webvtt`] but `subs0.vtt` is a well-formed `WebVTT`
/// segment, so the isolated reader recovers the cue.
///
/// # Errors
/// Returns [`FfmpegError`] if any libav encode/mux step or file write fails.
pub fn generate_hls_with_valid_webvtt(dir: &Path) -> Result<()> {
    crate::decode::ensure_initialized()?;
    write_ts_segment(&dir.join("seg0.ts"))?;
    write_text(&dir.join("video.m3u8"), &media_playlist_for("seg0.ts"))?;
    write_text(&dir.join("subs0.vtt"), &valid_webvtt_segment())?;
    write_text(&dir.join("subs.m3u8"), &media_playlist_for("subs0.vtt"))?;
    write_text(&dir.join("master.m3u8"), &master_playlist(dir))?;
    Ok(())
}

/// Write `contents` to `path`, mapping an I/O error into a typed
/// [`FfmpegError::OpenInput`] (EIO) while logging the underlying io detail.
fn write_text(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| {
        tracing::debug!(path = %path.display(), error = %e, "fixture file write failed");
        FfmpegError::OpenInput {
            path: path.display().to_string(),
            source: ffmpeg::Error::from(ffi::AVERROR(libc::EIO)),
        }
    })
}

/// A `VOD` media playlist referencing one segment file (`seg`) for two seconds.
fn media_playlist_for(seg: &str) -> String {
    let dur = HLS_SEGMENT_S;
    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:{dur}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXTINF:{dur}.000,\n\
         {seg}\n\
         #EXT-X-ENDLIST\n",
    )
}

/// The master playlist binding the video variant to the `WebVTT` subtitle
/// rendition, with absolute `file://` URIs resolved against `dir`.
fn master_playlist(dir: &Path) -> String {
    let base = format!("file://{}", dir.display());
    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",\
         LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{base}/subs.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=300000,SUBTITLES=\"subs\"\n\
         {base}/video.m3u8\n",
    )
}

/// A well-formed single-cue `WebVTT` segment carrying [`WEBVTT_CUE_TEXT`].
fn valid_webvtt_segment() -> String {
    let start = WEBVTT_CUE_START_S;
    let end = WEBVTT_CUE_END_S;
    let text = WEBVTT_CUE_TEXT;
    format!(
        "WEBVTT\n\
         X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\
         \n\
         00:00:0{start}.000 --> 00:00:0{end}.000\n\
         {text}\n",
    )
}

/// Encode a short LGPL `mpeg2video` clip into the MPEG-TS file at `path`.
fn write_ts_segment(path: &Path) -> Result<()> {
    // SAFETY: a single-threaded libav build sequence; we own every context /
    // stream / packet / frame allocated below and free them before returning.
    unsafe { write_ts_segment_inner(path) }
}

unsafe fn write_ts_segment_inner(path: &Path) -> Result<()> {
    let cpath = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| bug())?;
    let cfmt = CString::new("mpegts").map_err(|_| bug())?;

    let mut oc: *mut ffi::AVFormatContext = ptr::null_mut();
    // SAFETY: fresh null out-param + valid CStrings; libav fills `oc` on success.
    let r = ffi::avformat_alloc_output_context2(
        &raw mut oc,
        ptr::null_mut(),
        cfmt.as_ptr(),
        cpath.as_ptr(),
    );
    if r < 0 || oc.is_null() {
        return Err(ff_err(r));
    }
    let result = write_ts_segment_all(oc, &cpath);
    // SAFETY: free the context we allocated exactly once (releases its streams).
    ffi::avformat_free_context(oc);
    result
}

/// Add one `mpeg2video` stream, encode `HLS_SEGMENT_S * HLS_FPS` grey frames,
/// and finalise the MPEG-TS container.
unsafe fn write_ts_segment_all(oc: *mut ffi::AVFormatContext, cpath: &CString) -> Result<()> {
    let (vst, vctx) = add_hls_video_stream(oc)?;

    // SAFETY: `oformat` flags read on the live context.
    let needs_file = ((*(*oc).oformat).flags & ffi::AVFMT_NOFILE) == 0;
    if needs_file {
        // SAFETY: `pb` out-param of the live context; `cpath` is a valid CString.
        let r = ffi::avio_open(&raw mut (*oc).pb, cpath.as_ptr(), ffi::AVIO_FLAG_WRITE);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    // SAFETY: header write on the live, fully-configured context.
    let r = ffi::avformat_write_header(oc, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }

    let res = encode_hls_video(oc, vst, vctx);
    if res.is_ok() {
        // SAFETY: trailer on the live context after a successful header + frames.
        let r = ffi::av_write_trailer(oc);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    if needs_file {
        // SAFETY: closes + nulls the `pb` we opened above.
        ffi::avio_closep(&raw mut (*oc).pb);
    }
    let mut vctx_p = vctx;
    // SAFETY: free the encoder context we allocated + opened, exactly once.
    ffi::avcodec_free_context(&raw mut vctx_p);
    res
}

/// Add an `mpeg2video` stream sized for the HLS fixtures, returning
/// `(stream, opened encoder ctx)`.
unsafe fn add_hls_video_stream(
    oc: *mut ffi::AVFormatContext,
) -> Result<(*mut ffi::AVStream, *mut ffi::AVCodecContext)> {
    // SAFETY: encoder lookup by id; static codec descriptor or null.
    let vcodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_MPEG2VIDEO);
    if vcodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure.
    let vst = ffi::avformat_new_stream(oc, ptr::null());
    if vst.is_null() {
        return Err(bug());
    }
    // SAFETY: allocates an encoder context for the found codec; null on failure.
    let vctx = ffi::avcodec_alloc_context3(vcodec);
    if vctx.is_null() {
        return Err(bug());
    }
    // SAFETY: `vctx` is a fresh, exclusively-owned context; plain field writes.
    (*vctx).width = HLS_W;
    (*vctx).height = HLS_H;
    (*vctx).time_base = ffi::AVRational {
        num: 1,
        den: HLS_FPS,
    };
    (*vctx).framerate = ffi::AVRational {
        num: HLS_FPS,
        den: 1,
    };
    (*vctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*vctx).gop_size = HLS_FPS;
    (*vctx).bit_rate = 1_000_000;
    // SAFETY: `oformat` flags read on the live context.
    if ((*(*oc).oformat).flags & ffi::AVFMT_GLOBALHEADER) != 0 {
        let flag = i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).map_err(|_| bug())?;
        (*vctx).flags |= flag;
    }
    // SAFETY: opens the encoder on its own context.
    let r = ffi::avcodec_open2(vctx, vcodec, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: copies opened-encoder params into the stream's codecpar.
    let r = ffi::avcodec_parameters_from_context((*vst).codecpar, vctx);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write on the owned stream.
    (*vst).time_base = (*vctx).time_base;
    Ok((vst, vctx))
}

/// Encode `HLS_SEGMENT_S * HLS_FPS` grey frames and mux them, then flush.
unsafe fn encode_hls_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let w = u32::try_from(HLS_W).map_err(|_| bug())?;
    let h = u32::try_from(HLS_H).map_err(|_| bug())?;
    let total = HLS_SEGMENT_S * HLS_FPS;
    for i in 0..i64::from(total) {
        let mut video = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);
        // SAFETY: `video.as_mut_ptr()` is the live owned frame; fill its planes via
        // raw pointers (the safe API has no constant-fill).
        let frame = video.as_mut_ptr();
        let r = ffi::av_frame_make_writable(frame);
        if r < 0 {
            return Err(ff_err(r));
        }
        fill_plane((*frame).data[0], (*frame).linesize[0], HLS_W, HLS_H, 80);
        fill_plane(
            (*frame).data[1],
            (*frame).linesize[1],
            HLS_W / 2,
            HLS_H / 2,
            128,
        );
        fill_plane(
            (*frame).data[2],
            (*frame).linesize[2],
            HLS_W / 2,
            HLS_H / 2,
            128,
        );
        (*frame).pts = i;
        drain_video(oc, vst, vctx, frame)?;
    }
    // SAFETY: flush the encoder with a null frame, then drain.
    let r = ffi::avcodec_send_frame(vctx, ptr::null());
    if r < 0 {
        return Err(ff_err(r));
    }
    drain_video(oc, vst, vctx, ptr::null_mut())
}
