//! Safe RAII caption **decoders** over the linked libav subtitle decoders
//! (`libzvbi_teletextdec`, `dvbsub`, `cc_dec`, `webvtt`, `subrip`, `mov_text`,
//! `ass`), producing the unified [`CaptionCue`] model with timestamps rebased to
//! the internal nanosecond timeline (invariant #3).
//!
//! This module is behind the off-by-default `ffmpeg` feature; the decoders it
//! wraps are already in the linked libavcodec (no new `cargo deny`-relevant
//! dependency, see [`docs/io/captions.md`](../docs/io/captions.md) §3). The pure
//! cue model it emits lives in [`crate::caption`] and is always compiled.
//!
//! # The decode contract — "no cue right now" is normal
//!
//! `avcodec_decode_subtitle2` is **not** the send/receive pump the audio/video
//! decoders use: each call consumes one packet and reports whether a subtitle was
//! produced. Most packets produce **nothing** (a teletext page that is not the
//! selected one, a DVB-sub `clear` segment, an EIA-608 control byte that only
//! buffers state) — [`CaptionDecoder::decode`] returns an **empty** [`Vec`] for
//! those, never an error and never a panic (invariants #1, #2, #10). A stalled,
//! absent, or wrong-page caption stream simply yields no cues; it can never stall
//! the caller.
//!
//! # Timing (invariant #3)
//!
//! Each cue is anchored at the **packet's** PTS, rebased through the stream
//! time-base to nanoseconds exactly the way [`crate::decode_stream`] rebases
//! video/audio — never via float fps. libav reports the on-screen window as
//! `start_display_time`/`end_display_time` in **milliseconds relative to the
//! packet PTS**; those offsets are added in nanoseconds. When libav gives no
//! explicit end (display time `0`), a bounded default on-screen duration closes
//! the cue so an open-ended caption cannot linger forever.

// reason: libav subtitle-bitmap FFI lives here; every `unsafe` block/fn below
// carries a `// SAFETY:` comment (matches `hwframe.rs`; the crate is
// `unsafe_code = "deny"`, not `forbid`, so unsafe is permitted with justification).
#![allow(unsafe_code)]

use ffmpeg::codec::subtitle::{Rect, Subtitle};
use ffmpeg::codec::{context::Context, packet::Packet, Id, Parameters};
use ffmpeg::util::frame::side_data::Type as SideDataType;
use ffmpeg::util::frame::Video;
use ffmpeg::Dictionary;
use ffmpeg_next as ffmpeg;

use multiview_core::time::{rescale, MediaTime, Rational};

use crate::caption::{strip_ass_event, CaptionCue, CueError, CueRect};
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};

/// Nanoseconds per second, as the destination time-base denominator.
const NS_PER_SEC: Rational = Rational::new(1, 1_000_000_000);

/// Default on-screen duration (ns) for a cue libav gives no explicit end for, so
/// an open-ended caption (e.g. a 608 roll-up with no clear) cannot linger. Four
/// seconds is the conventional pop-on caption hold; the cue store further bounds
/// it by the next cue.
const DEFAULT_HOLD_NS: i64 = 4_000_000_000;

/// Which embedded closed-caption channel `cc_dec` should surface.
///
/// CEA-608 carries up to four fields (CC1–CC4); CEA-708 carries numbered
/// services. The default is CC1, the primary program caption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CcChannel {
    /// CEA-608 field 1, channel 1 (the primary program caption — the default).
    #[default]
    Cc1,
    /// CEA-608 field 1, channel 2.
    Cc2,
    /// CEA-608 field 2, channel 3.
    Cc3,
    /// CEA-608 field 2, channel 4.
    Cc4,
    /// A CEA-708 service number (1..=63).
    ///
    /// **Not decodable to text by the linked `cc_dec`.** libav's caption decoder
    /// parses the EIA-608 line-21 data in the A53 stream but discards CEA-708
    /// service blocks (DTVCC, `cc_type` 2/3), so a decoder built for a 708 service
    /// would emit no cues. [`CaptionDecoder`] therefore **refuses** this channel at
    /// construction with [`FfmpegError::UnsupportedCaptionChannel`] rather than
    /// returning a silently cue-less decoder; callers fall back to a 608 field,
    /// teletext, or a sidecar (captions.md §3 row 3, ADR-R007). The variant is kept
    /// because 708 services are a real, addressable caption form a future styled
    /// 708 decoder could surface — the refusal is a property of *this* decoder.
    Service(u8),
}

impl CcChannel {
    /// The `cc_dec` `data_field`/`real_time` option value selecting this channel.
    /// `cc_dec` exposes the active channel through its `cc_data_field` /
    /// `data_field` decoder option; CC1/CC2 are field 0, CC3/CC4 are field 1.
    fn data_field_option(self) -> Option<&'static str> {
        match self {
            Self::Cc1 | Self::Cc2 => Some("0"),
            Self::Cc3 | Self::Cc4 => Some("1"),
            // 708 services are not field-addressed; let the decoder default.
            Self::Service(_) => None,
        }
    }
}

/// How a [`CaptionDecoder`] was configured — which libav decoder it drives and
/// any decoder-specific selector (teletext page, CC channel).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptionSource {
    /// DVB teletext via `libzvbi_teletextdec`. `page` selects the magazine/page
    /// (commonly 801); [`None`] follows the decoder's default (first subtitle
    /// page).
    Teletext {
        /// The teletext page to decode (e.g. `801`), or [`None`] for auto.
        page: Option<u16>,
    },
    /// DVB subtitle bitmap cues via `dvbsub`.
    DvbSubtitle,
    /// Embedded CEA-608/708 via `cc_dec`, fed the A53 caption-data bytes
    /// extracted from the video stream's SEI/user-data.
    EmbeddedCc {
        /// The CC channel/service to surface.
        channel: CcChannel,
    },
    /// `WebVTT` text cues via `webvtt` (HLS rendition or container track).
    WebVtt,
    /// `SubRip` text cues via `subrip`.
    SubRip,
    /// MP4 timed text via `mov_text`.
    MovText,
    /// ASS/SSA text via `ass` (text path; styled rasterisation is the `libass`
    /// feature, out of scope here).
    Ass,
}

impl CaptionSource {
    /// The libav decoder name this source opens.
    #[must_use]
    pub const fn decoder_name(&self) -> &'static str {
        match self {
            Self::Teletext { .. } => "libzvbi_teletextdec",
            Self::DvbSubtitle => "dvbsub",
            Self::EmbeddedCc { .. } => "cc_dec",
            Self::WebVtt => "webvtt",
            Self::SubRip => "subrip",
            Self::MovText => "mov_text",
            Self::Ass => "ass",
        }
    }

    /// The libav codec [`Id`] this source maps to, used when opening from a
    /// stream's [`Parameters`] (which already carry the id) rather than by name.
    #[must_use]
    fn codec_id(&self) -> Id {
        match self {
            Self::Teletext { .. } => Id::DVB_TELETEXT,
            Self::DvbSubtitle => Id::DVB_SUBTITLE,
            Self::EmbeddedCc { .. } => Id::EIA_608,
            Self::WebVtt => Id::WEBVTT,
            Self::SubRip => Id::SUBRIP,
            Self::MovText => Id::MOV_TEXT,
            Self::Ass => Id::ASS,
        }
    }

    /// The decoder options this source needs (teletext page, CC field). Returned
    /// as owned pairs so the caller can build a libav [`Dictionary`].
    fn decoder_options(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Teletext { page: Some(page) } => {
                // libzvbi_teletextdec selects the page via the `txt_page` option.
                vec![("txt_page", page.to_string())]
            }
            Self::EmbeddedCc { channel } => {
                // `cc_dec` has the `delay` capability: by default it buffers an
                // entire caption block and only emits the cue on a *flush* packet
                // (a null packet drained at end-of-stream). Our streaming contract
                // decodes one packet → returns its cues on the input thread and
                // never injects a flush, so the default decoder would emit nothing
                // until the stream ended. `real_time=1` makes `cc_dec` emit each
                // subtitle event as the closing control code (608 End-Of-Caption /
                // 708 service block end) is decoded — exactly the live, sampled
                // delivery the cue store expects (captions.md §4/§5).
                let mut opts = vec![("real_time", "1".to_owned())];
                if let Some(field) = channel.data_field_option() {
                    opts.push(("data_field", field.to_owned()));
                }
                opts
            }
            _ => Vec::new(),
        }
    }
}

/// A safe caption decoder: takes a caption stream's codec parameters (or a
/// by-name configuration) and fed packets, and yields [`CaptionCue`]s.
///
/// Owns one libav subtitle decoder context (`Send + !Sync`, freed in `Drop` by
/// `ffmpeg_next`). It is driven on the **input** thread, never the output clock
/// (invariant #1); the cue store the caller writes is what the compositor
/// samples.
pub struct CaptionDecoder {
    decoder: ffmpeg::codec::decoder::Subtitle,
    source: CaptionSource,
    time_base: Rational,
}

impl CaptionDecoder {
    /// Build a decoder from a demuxed caption stream's [`Parameters`] and
    /// time-base, applying any source-specific selector (teletext page, CC
    /// channel).
    ///
    /// The stream parameters carry the correct codec id; the decoder is opened by
    /// the **name** the `source` names so the right implementation
    /// (`libzvbi_teletextdec` rather than a generic teletext decoder) is used.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::CodecNotFound`] — the named decoder is absent from the
    ///   linked build.
    /// * [`FfmpegError::OpenDecoder`] — the context could not be built/opened.
    pub fn from_parameters(
        source: CaptionSource,
        parameters: Parameters,
        time_base: Rational,
    ) -> Result<Self> {
        ensure_initialized()?;
        let ctx = Context::from_parameters(parameters).map_err(FfmpegError::OpenDecoder)?;
        Self::open(source, ctx, time_base)
    }

    /// Build a decoder with no demuxed stream behind it — a fresh context for the
    /// named decoder. Used for the **embedded-CC** path (the A53 bytes are side
    /// data on the *video* stream, not a stream of their own) and for feeding
    /// raw caption-data packets.
    ///
    /// # Errors
    /// As [`CaptionDecoder::from_parameters`].
    pub fn for_embedded(source: CaptionSource, time_base: Rational) -> Result<Self> {
        ensure_initialized()?;
        let name = source.decoder_name();
        let codec = ffmpeg::codec::decoder::find_by_name(name)
            .or_else(|| ffmpeg::codec::decoder::find(source.codec_id()))
            .ok_or(FfmpegError::CodecNotFound(decoder_static_name(&source)))?;
        let ctx = Context::new_with_codec(codec);
        Self::open(source, ctx, time_base)
    }

    /// Shared open path: apply the decoder options, open by the source's named
    /// decoder, and take the subtitle decoder.
    fn open(source: CaptionSource, ctx: Context, time_base: Rational) -> Result<Self> {
        // Refuse a caption channel the linked decoder cannot decode to text up
        // front, so we never hand back a decoder that silently emits no cues. The
        // linked `cc_dec` decodes EIA-608 (fields CC1–CC4) but discards CEA-708
        // service blocks (DTVCC); a 708 service selector is unsupported here.
        if let CaptionSource::EmbeddedCc {
            channel: CcChannel::Service(service),
        } = source
        {
            return Err(FfmpegError::UnsupportedCaptionChannel { service });
        }

        let name = source.decoder_name();
        let codec = ffmpeg::codec::decoder::find_by_name(name)
            .or_else(|| ffmpeg::codec::decoder::find(source.codec_id()))
            .ok_or(FfmpegError::CodecNotFound(decoder_static_name(&source)))?;

        let mut opts = Dictionary::new();
        for (k, v) in source.decoder_options() {
            opts.set(k, &v);
        }

        let opened = ctx
            .decoder()
            .open_as_with(codec, opts)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = opened.subtitle().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self {
            decoder,
            source,
            time_base,
        })
    }

    /// The source configuration this decoder was built with.
    #[must_use]
    pub fn source(&self) -> &CaptionSource {
        &self.source
    }

    /// Decode one caption packet, returning every cue it produced (usually zero
    /// or one; teletext can carry several rects).
    ///
    /// A packet that buffers decoder state without emitting a subtitle (the
    /// common case for control codes / non-selected pages / `clear` segments)
    /// yields an **empty** [`Vec`] — not an error. The packet's PTS anchors the
    /// cue window on the ns timeline.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] only for a genuine libav decode error
    /// (corrupt packet the decoder rejects), never for "no cue produced".
    pub fn decode(&mut self, packet: &Packet) -> Result<Vec<CaptionCue>> {
        let mut subtitle = Subtitle::new();
        let got = self
            .decoder
            .decode(packet, &mut subtitle)
            .map_err(FfmpegError::Decode)?;
        if !got {
            return Ok(Vec::new());
        }
        let anchor_ns = self.anchor_ns(packet, &subtitle);
        Ok(self.cues_from_subtitle(&subtitle, anchor_ns))
    }

    /// Decode raw caption-data bytes anchored at `pts` (in the configured
    /// time-base) — the **embedded-CC** convenience: wrap the A53 bytes
    /// extracted from a video frame's side data as a packet and decode them.
    ///
    /// No packet duration is set, so a text decoder that derives its on-screen
    /// window from the packet duration (`subrip`, `webvtt`, `mov_text`) falls
    /// back to the bounded [`DEFAULT_HOLD_NS`] hold. Use
    /// [`CaptionDecoder::decode_bytes_for_window`] when the source carried an
    /// explicit cue duration.
    ///
    /// # Errors
    /// As [`CaptionDecoder::decode`].
    pub fn decode_bytes(&mut self, data: &[u8], pts: Option<i64>) -> Result<Vec<CaptionCue>> {
        let mut packet = Packet::copy(data);
        packet.set_pts(pts);
        packet.set_dts(pts);
        self.decode(&packet)
    }

    /// Decode raw caption-data bytes anchored at `pts`, also carrying the
    /// packet's on-screen `duration` (both in the configured time-base) — the
    /// in-container **text** path (`subrip`, `webvtt`, `mov_text`), where the
    /// demuxed packet carries the cue body plus its duration.
    ///
    /// This differs from [`CaptionDecoder::decode_bytes`] only in that it also
    /// stamps the packet duration; a zero/negative `duration` is left unset. The
    /// on-screen window the decoder reports (`end_display_time`) is honoured when
    /// the libav decoder context carries the packet time-base — i.e. when driven
    /// from a real demuxed stream. When it does not (a standalone, demux-less
    /// context), the decoder reports no explicit end and the bounded
    /// [`DEFAULT_HOLD_NS`] fallback closes the cue.
    ///
    /// # Errors
    /// As [`CaptionDecoder::decode`].
    pub fn decode_bytes_for_window(
        &mut self,
        data: &[u8],
        pts: Option<i64>,
        duration: i64,
    ) -> Result<Vec<CaptionCue>> {
        let mut packet = Packet::copy(data);
        packet.set_pts(pts);
        packet.set_dts(pts);
        if duration > 0 {
            packet.set_duration(duration);
        }
        self.decode(&packet)
    }

    /// Decode the embedded CEA-608/708 closed captions carried on a decoded
    /// **video** frame as `AV_FRAME_DATA_A53_CC` side data — the embedded-CC
    /// path end-to-end.
    ///
    /// CEA-608/708 captions are not a stream of their own: they ride as side data
    /// on the H.264/MPEG-2 video frames the per-source decode already produces
    /// ([`docs/io/captions.md`](../docs/io/captions.md) §2/§4 "CEA special case"),
    /// so extracting them costs nothing extra. This pulls the A53 cc-data bytes
    /// off `frame` (via [`extract_a53_cc`]) and feeds them to this decoder's
    /// `cc_dec`, anchoring the cue at `pts` (in the decoder's configured
    /// time-base, i.e. the video stream's).
    ///
    /// A frame carrying **no** A53 side data — the common case, since only the
    /// frames that happen to carry caption bytes have it — yields an empty
    /// [`Vec`], never an error (invariants #2, #10). Because `cc_dec` only emits a
    /// cue when it sees a caption's closing control code, most frames that *do*
    /// carry A53 bytes still yield nothing; the cue appears on the frame whose
    /// bytes complete it.
    ///
    /// This is only meaningful for a decoder built for
    /// [`CaptionSource::EmbeddedCc`]; for any other source the extracted bytes
    /// would not be the right wire format and the call returns an empty [`Vec`]
    /// rather than feeding `cc_dec` bytes it cannot parse.
    ///
    /// # Errors
    /// As [`CaptionDecoder::decode`] — a genuine libav decode error on the A53
    /// bytes; never for "no caption on this frame".
    pub fn decode_video_frame(
        &mut self,
        frame: &Video,
        pts: Option<i64>,
    ) -> Result<Vec<CaptionCue>> {
        if !matches!(self.source, CaptionSource::EmbeddedCc { .. }) {
            return Ok(Vec::new());
        }
        match extract_a53_cc(frame) {
            Some(bytes) => self.decode_bytes(&bytes, pts),
            None => Ok(Vec::new()),
        }
    }

    /// The cue anchor (ns) for a decoded subtitle: prefer the packet PTS rebased
    /// through the stream time-base (consistent with video/audio rebasing); fall
    /// back to the subtitle's own PTS (which libav reports in microseconds).
    fn anchor_ns(&self, packet: &Packet, subtitle: &Subtitle) -> i64 {
        if let Some(ticks) = packet.pts().or_else(|| packet.dts()) {
            return rescale(ticks, self.time_base, NS_PER_SEC);
        }
        // `AVSubtitle::pts` is in AV_TIME_BASE (microseconds).
        match subtitle.pts() {
            Some(us) => us.saturating_mul(1_000),
            None => 0,
        }
    }

    /// Build cues from a decoded [`Subtitle`]'s rects. `start_display_time` /
    /// `end_display_time` are milliseconds relative to `anchor_ns`.
    fn cues_from_subtitle(&self, subtitle: &Subtitle, anchor_ns: i64) -> Vec<CaptionCue> {
        let start_ns =
            anchor_ns.saturating_add(i64::from(subtitle.start()).saturating_mul(1_000_000));
        let end_display_ms = subtitle.end();
        let end_ns = if end_display_ms == 0 {
            start_ns.saturating_add(DEFAULT_HOLD_NS)
        } else {
            anchor_ns.saturating_add(i64::from(end_display_ms).saturating_mul(1_000_000))
        };
        let start = MediaTime::from_nanos(start_ns);
        let end = MediaTime::from_nanos(end_ns);

        let mut cues = Vec::new();
        for rect in subtitle.rects() {
            // A degenerate rect (empty text after stripping, bad geometry) is
            // dropped, not fatal: captions are intermittent and a malformed rect
            // must never stall the caller (invariant #10).
            if let Ok(Some(cue)) = self.cue_from_rect(&rect, start, end) {
                cues.push(cue);
            }
        }
        cues
    }

    /// Convert one decoded rect into a cue, or [`None`] if it carries nothing
    /// displayable.
    // reason: kept as a method to group rect-handling on the decoder and to allow
    // future per-decoder state (teletext page / CC channel); this body needs no `self`.
    #[allow(clippy::unused_self)]
    fn cue_from_rect(
        &self,
        rect: &Rect<'_>,
        start: MediaTime,
        end: MediaTime,
    ) -> std::result::Result<Option<CaptionCue>, CueError> {
        match rect {
            Rect::Ass(ass) => {
                let (lines, region) = strip_ass_event(ass.get());
                if lines.is_empty() {
                    return Ok(None);
                }
                CaptionCue::try_text(start, end, lines, region).map(Some)
            }
            Rect::Text(text) => {
                let lines: Vec<String> = text
                    .get()
                    .lines()
                    .map(|l| l.trim().to_owned())
                    .filter(|l| !l.is_empty())
                    .collect();
                if lines.is_empty() {
                    return Ok(None);
                }
                CaptionCue::try_text(start, end, lines, None).map(Some)
            }
            Rect::Bitmap(bitmap) => bitmap_cue(bitmap, start, end).map(Some),
            Rect::None(_) => Ok(None),
        }
    }
}

/// A `'static` decoder name for error reporting (the [`FfmpegError::CodecNotFound`]
/// arm holds a `&'static str`).
fn decoder_static_name(source: &CaptionSource) -> &'static str {
    source.decoder_name()
}

/// Extract the embedded CEA-608/708 caption bytes (`AV_FRAME_DATA_A53_CC` side
/// data) from a decoded video frame, or [`None`] if the frame carries none.
///
/// A53 closed captions are not a separate elementary stream: H.264/MPEG-2 video
/// carries them in SEI / user-data, which libav surfaces as the `A53_CC` side
/// data on each decoded video frame
/// ([`docs/io/captions.md`](../docs/io/captions.md) §2). The returned bytes are
/// the raw `cc_data` triplet sequence (`cc_type`/`cc_valid` marker + two 608/708
/// data bytes per entry) that [`CaptionDecoder::decode_bytes`] feeds straight to
/// `cc_dec`. They are copied out of the libav-owned side-data buffer (a few bytes
/// per frame), so the result outlives the borrow of `frame`.
///
/// Most decoded frames have no A53 side data (captions are sparse) — this returns
/// [`None`] for them, never an error: the embedded-CC path is sampled, never
/// pacing (invariants #2, #10). An empty side-data buffer also yields [`None`].
///
/// Note this must run on the **raw decoded video frame**, before any pixel-format
/// conversion: libswscale produces a *new* frame that does not carry the source
/// frame's side data.
#[must_use]
pub fn extract_a53_cc(frame: &Video) -> Option<Vec<u8>> {
    let sd = frame.side_data(SideDataType::A53CC)?;
    let bytes = sd.data();
    if bytes.is_empty() {
        None
    } else {
        Some(bytes.to_vec())
    }
}

/// Convert a decoded DVB-sub bitmap rect (PAL8: 8-bit palette indices plus an
/// ARGB palette) into a premultiplied-RGBA [`CaptionCue::Bitmap`].
///
/// The pixel data is not exposed by `ffmpeg_next` on `FFmpeg` ≥ 5
/// (`Bitmap::picture` is gated out), so the rect's `data`/`linesize`/palette are
/// read through the raw `AVSubtitleRect` pointer in one `unsafe` block.
fn bitmap_cue(
    bitmap: &ffmpeg::codec::subtitle::Bitmap<'_>,
    start: MediaTime,
    end: MediaTime,
) -> std::result::Result<CaptionCue, CueError> {
    let width = bitmap.width();
    let height = bitmap.height();
    let nb_colors = bitmap.colors();
    let x = u32::try_from(bitmap.x()).unwrap_or(0);
    let y = u32::try_from(bitmap.y()).unwrap_or(0);

    if width == 0 || height == 0 {
        return Err(CueError::InvalidBitmap {
            width,
            height,
            len: 0,
        });
    }

    // SAFETY: `bitmap` borrows a live `AVSubtitle` for the lifetime of the
    // `Rect`, so its backing `AVSubtitleRect` pointer is valid and immutable for
    // this call. We read `data[0]` (PAL8 indices), `data[1]` (the ARGB palette,
    // `nb_colors` u32 entries), and `linesize[0]` — all populated by the libav
    // bitmap subtitle decoders. We bound every index read to `width`/`height`/
    // `nb_colors`/`linesize` and copy out into an owned `Vec`, so no raw pointer
    // escapes and no read goes past the libav-owned buffers.
    let rgba = unsafe {
        let raw = bitmap.as_ptr();
        let indices = (*raw).data[0];
        let palette = (*raw).data[1];
        let stride = (*raw).linesize[0];
        if indices.is_null() || palette.is_null() || stride <= 0 {
            return Err(CueError::InvalidBitmap {
                width,
                height,
                len: 0,
            });
        }
        let stride = usize::try_from(stride).unwrap_or(0);
        rgba_from_pal8(indices, palette, stride, width, height, nb_colors)
    };

    CaptionCue::try_bitmap(
        start,
        end,
        rgba,
        CueRect {
            x,
            y,
            width,
            height,
        },
    )
}

/// Materialise a tight premultiplied-RGBA buffer from a PAL8 index plane plus an
/// ARGB palette.
///
/// # Safety
/// `indices` must point to at least `stride * height` readable bytes and
/// `palette` to at least `nb_colors * 4` readable bytes (libav guarantees this
/// for a decoded bitmap rect). The caller bounds `width`/`height`/`stride`.
// reason: r/g/b/a are the canonical RGBA channel names and w/h the pixel dims.
#[allow(clippy::many_single_char_names)]
unsafe fn rgba_from_pal8(
    indices: *const u8,
    palette: *const u8,
    stride: usize,
    width: u32,
    height: u32,
    nb_colors: usize,
) -> Vec<u8> {
    let w = usize::try_from(width).unwrap_or(0);
    let h = usize::try_from(height).unwrap_or(0);
    let mut out = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    // The palette is `AV_PIX_FMT_RGB32` (ARGB packed in a native-endian u32); on
    // a little-endian host the bytes are B, G, R, A. Read each component and emit
    // straight RGBA, then premultiply by alpha for the linear-light compositor.
    for row in 0..h {
        for col in 0..w {
            // SAFETY: row<h, col<w, stride>=w ⇒ in-bounds of the index plane.
            let idx = usize::from(*indices.add(row.saturating_mul(stride).saturating_add(col)));
            let (r, g, b, a) = if idx < nb_colors {
                // SAFETY: idx<nb_colors ⇒ palette entry [idx*4, idx*4+4) is valid.
                let base = idx.saturating_mul(4);
                let bch = *palette.add(base);
                let gch = *palette.add(base.saturating_add(1));
                let rch = *palette.add(base.saturating_add(2));
                let ach = *palette.add(base.saturating_add(3));
                (rch, gch, bch, ach)
            } else {
                (0, 0, 0, 0)
            };
            let oi = row.saturating_mul(w).saturating_add(col).saturating_mul(4);
            // One disjoint 4-byte slice (avoids four simultaneous &mut borrows);
            // the slice pattern matches only a full RGBA quad, no indexing.
            if let Some([o0, o1, o2, o3]) = out.get_mut(oi..oi.saturating_add(4)) {
                *o0 = premultiply(r, a);
                *o1 = premultiply(g, a);
                *o2 = premultiply(b, a);
                *o3 = a;
            }
        }
    }
    out
}

/// Premultiply one 8-bit colour channel by an 8-bit alpha (`c * a / 255`,
/// rounded), matching the compositor's premultiplied-alpha blend (ADR-C003).
fn premultiply(c: u8, a: u8) -> u8 {
    let prod = u16::from(c)
        .saturating_mul(u16::from(a))
        .saturating_add(127);
    // `(x + 127) / 255` is the standard rounded divide-by-255.
    let div = prod.saturating_add(prod / 255).wrapping_shr(8);
    u8::try_from(div.min(255)).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_names_match_each_source() {
        assert_eq!(
            CaptionSource::Teletext { page: Some(801) }.decoder_name(),
            "libzvbi_teletextdec"
        );
        assert_eq!(CaptionSource::DvbSubtitle.decoder_name(), "dvbsub");
        assert_eq!(
            CaptionSource::EmbeddedCc {
                channel: CcChannel::Cc1
            }
            .decoder_name(),
            "cc_dec"
        );
        assert_eq!(CaptionSource::WebVtt.decoder_name(), "webvtt");
        assert_eq!(CaptionSource::SubRip.decoder_name(), "subrip");
        assert_eq!(CaptionSource::MovText.decoder_name(), "mov_text");
    }

    #[test]
    fn teletext_page_becomes_a_decoder_option() {
        let opts = CaptionSource::Teletext { page: Some(801) }.decoder_options();
        assert_eq!(opts, vec![("txt_page", "801".to_owned())]);
        // Auto (no page) sets no option.
        assert!(CaptionSource::Teletext { page: None }
            .decoder_options()
            .is_empty());
    }

    #[test]
    fn cc_field_selects_the_right_data_field() {
        assert_eq!(CcChannel::Cc1.data_field_option(), Some("0"));
        assert_eq!(CcChannel::Cc3.data_field_option(), Some("1"));
        assert_eq!(CcChannel::Service(1).data_field_option(), None);
    }

    #[test]
    fn premultiply_is_rounded_and_bounded() {
        assert_eq!(premultiply(255, 255), 255);
        assert_eq!(premultiply(255, 0), 0);
        assert_eq!(premultiply(0, 255), 0);
        // 200 * 128 / 255 = 100.39 -> 100 rounded.
        assert_eq!(premultiply(200, 128), 100);
        // 255 * 128 / 255 = 128.
        assert_eq!(premultiply(255, 128), 128);
    }
}
