//! Safe demuxer over `libavformat` (the `ffmpeg` feature).
//!
//! [`Demuxer`] opens a URL/file, exposes each stream's parameters as a pure
//! [`StreamParams`] snapshot, reads coded packets, and seeks. It owns one
//! `AVFormatContext` (input) and frees it in `Drop` (via `ffmpeg_next`, which
//! owns the raw FFI). The context is `Send + !Sync` by construction.
//!
//! ## Timestamps are *input* timestamps
//! [`ReadPacket::pts`]/[`ReadPacket::dts`] are raw values in the stream's
//! time-base (carried alongside as [`StreamParams::time_base`]). They are
//! **input** timestamps — the engine rebases and the output clock re-stamps
//! everything (invariants #1/#3). Nothing read here is forwarded to a muxer
//! untouched.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ffmpeg::codec::Parameters;
use ffmpeg::format::stream::Disposition;
use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;

use multiview_core::stream::{
    Bcp47, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use multiview_core::time::Rational;

use crate::convert::{from_ff_rational, MediaKind};
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};
use crate::idr::{is_idr, CodecKind, NalFraming};

/// A pure, owned snapshot of one stream's parameters.
///
/// Borrows nothing from the [`Demuxer`], so it can be stored, logged, or sent
/// across threads while the demuxer keeps reading.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamParams {
    /// Index of this stream within the container.
    pub index: usize,
    /// The media kind (video / audio / other).
    pub kind: MediaKind,
    /// The codec id name (e.g. `"h264"`, `"aac"`), as reported by libav.
    pub codec_name: String,
    /// The stream time-base (exact rational; **never** a float fps).
    pub time_base: Rational,
    /// Average frame rate, if the container declares one (video streams).
    pub avg_frame_rate: Option<Rational>,
    /// Video width in pixels (`0` for non-video / unknown).
    pub width: u32,
    /// Video height in pixels (`0` for non-video / unknown).
    pub height: u32,
    /// Audio sample rate in Hz (`0` for non-audio / unknown).
    pub sample_rate: u32,
    /// Audio channel count (`0` for non-audio / unknown).
    pub channels: u16,
    /// The stream's `language` metadata tag (BCP-47 / ISO 639 as declared by the
    /// container), if present. Used to resolve a caption rendition by language.
    pub language: Option<String>,
}

/// A read coded packet plus the index of the stream it belongs to.
///
/// The packet's `pts`/`dts` are raw stream-time-base values — input timestamps,
/// never forwarded to a muxer without rebasing/re-stamping.
pub struct ReadPacket {
    /// The stream index this packet belongs to.
    pub stream_index: usize,
    /// The underlying libav packet (ref-counted; freed on drop).
    pub packet: ffmpeg::codec::packet::Packet,
}

impl ReadPacket {
    /// Raw presentation timestamp in stream time-base ticks, if present.
    #[must_use]
    pub fn pts(&self) -> Option<i64> {
        self.packet.pts()
    }

    /// Raw decode timestamp in stream time-base ticks, if present.
    #[must_use]
    pub fn dts(&self) -> Option<i64> {
        self.packet.dts()
    }

    /// Packet payload size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.packet.size()
    }

    /// Whether the packet is flagged as a keyframe.
    ///
    /// **Not** an IDR test (GP-1, ADR-0030): `FFmpeg` sets `AV_PKT_FLAG_KEY` for
    /// HEVC CRA / open-GOP and H.264 recovery-point-SEI I-frames too, whose
    /// leading pictures reference now-absent frames. For passthrough recovery
    /// re-anchoring use [`ReadPacket::is_idr`], the strict random-access test.
    #[must_use]
    pub fn is_key(&self) -> bool {
        self.packet.is_key()
    }

    /// Whether this access unit is a **strict** IDR / clean random-access point
    /// for `(codec, framing)` — the gate for re-anchoring a guarded passthrough
    /// on recovery (GP-1, ADR-0030 boundary 2).
    ///
    /// Unlike [`ReadPacket::is_key`] this inspects the coded NAL/OBU bytes:
    /// H.264 `nal_unit_type == 5`; HEVC `IDR_W_RADL (19)` / `IDR_N_LP (20)`
    /// (rejecting CRA / BLA); AV1 a KEY frame in a temporal unit carrying a
    /// sequence header. It returns `false` for an empty packet or any input it
    /// cannot positively classify (a missed IDR only delays recovery; a false
    /// IDR would splice garbage). Derive `(codec, framing)` once per stream with
    /// [`Demuxer::stream_idr_framing`].
    #[must_use]
    pub fn is_idr(&self, codec: CodecKind, framing: NalFraming) -> bool {
        self.packet
            .data()
            .is_some_and(|bytes| is_idr(bytes, codec, framing))
    }
}

/// Sentinel meaning "no deadline is armed" in [`ReadDeadline`].
const DEADLINE_DISARMED: u64 = 0;

/// Shared, lock-free read-deadline state read by the `AVIOInterruptCB` callback.
///
/// The callback runs on a **foreign / libav I/O thread** while a read is blocked
/// (GP-0, ADR-0030). It must be allocation-light and must never let a Rust panic
/// unwind across the FFI boundary, so it does nothing but compare two integers:
/// an `AtomicU64` deadline (nanoseconds since `base`, or [`DEADLINE_DISARMED`])
/// against the elapsed time. The owning [`Demuxer`] keeps this `Box` alive for
/// the whole life of the context and frees it in `Drop` (no leak, unlike
/// `ffmpeg_next::format::input_with_interrupt`, which leaks the closure).
struct ReadDeadline {
    /// Monotonic base captured at construction; the deadline is relative to it.
    base: Instant,
    /// Nanoseconds-since-`base` past which a blocked read must abort, or
    /// [`DEADLINE_DISARMED`] when no deadline is armed.
    deadline_ns: AtomicU64,
}

impl ReadDeadline {
    /// A fresh, disarmed deadline anchored at `now`.
    fn new() -> Self {
        Self {
            base: Instant::now(),
            deadline_ns: AtomicU64::new(DEADLINE_DISARMED),
        }
    }

    /// Arm the deadline to `timeout` from now (saturating into the `base`
    /// timeline; clamped away from the disarmed sentinel).
    fn arm(&self, timeout: Duration) {
        let elapsed = self.base.elapsed();
        let at = elapsed.saturating_add(timeout);
        let ns = u64::try_from(at.as_nanos()).unwrap_or(u64::MAX);
        // Never store the disarmed sentinel for a real deadline.
        self.deadline_ns
            .store(ns.max(DEADLINE_DISARMED + 1), Ordering::Release);
    }

    /// Disarm the deadline (no read in progress).
    fn disarm(&self) {
        self.deadline_ns.store(DEADLINE_DISARMED, Ordering::Release);
    }

    /// Whether a blocked read should abort *now*.
    fn expired(&self) -> bool {
        let deadline = self.deadline_ns.load(Ordering::Acquire);
        let now_ns = u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX);
        should_interrupt(now_ns, deadline)
    }
}

/// Pure interrupt-decision: abort iff a deadline is armed and `now_ns` reached it.
///
/// A `deadline_ns` of [`DEADLINE_DISARMED`] means "no deadline" → never interrupt.
/// Split out so the decision is exhaustively unit-testable with no libav and no
/// clock (GP-0). The callback only ever calls this with monotonic values.
#[must_use]
fn should_interrupt(now_ns: u64, deadline_ns: u64) -> bool {
    deadline_ns != DEADLINE_DISARMED && now_ns >= deadline_ns
}

/// Options for [`Demuxer::open_with_interrupt`] (GP-0, ADR-0030).
///
/// A live passthrough opens its demuxer with a read/write timeout so a stalled
/// TCP/RTSP/SRT/UDP read aborts within `rw_timeout` (via both the libav
/// `rw_timeout` option **and** the injected `AVIOInterruptCB`) instead of
/// blocking forever, letting recovery tear the wedged demuxer down.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct DemuxOptions {
    /// The read/write timeout. `None` opens with no timeout (like
    /// [`Demuxer::open`]); the interrupt callback is still installed but stays
    /// disarmed, so behaviour matches a bare open.
    rw_timeout: Option<Duration>,
}

impl DemuxOptions {
    /// A fresh option set with no timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the read/write timeout: a blocked open or read aborts within roughly
    /// this duration.
    #[must_use]
    pub fn with_rw_timeout(mut self, timeout: Duration) -> Self {
        self.rw_timeout = Some(timeout);
        self
    }

    /// The configured read/write timeout, if any.
    #[must_use]
    pub fn rw_timeout(&self) -> Option<Duration> {
        self.rw_timeout
    }
}

/// A safe demuxer bound to one opened input container.
///
/// Not `Sync`: the wrapped `AVFormatContext` requires external synchronization
/// for shared access (CLAUDE.md §7). It is `Send`, so it may move to the decode
/// thread that drives it.
pub struct Demuxer {
    /// The libav→resource attribution registration for this demuxer's
    /// `AVFormatContext*` (ADR-0060 §3.2, mechanism B). Declared **before**
    /// `input` so Rust's field-drop order removes the map entry **before** the
    /// `AVFormatContext` memory is freed (no lookup ever observes a freed pointer
    /// mapped to a stale id). `None` when no resource was owned at open time (the
    /// thread held no [`ResourceGuard`]) — honest: no guessed id.
    ///
    /// Held purely for its `Drop` (which removes the map entry); never read after
    /// construction, hence `dead_code`-allowed with this justification.
    #[allow(dead_code)]
    registration: Option<crate::log_bridge::AvContextRegistration>,
    input: ffmpeg::format::context::Input,
    /// The read-deadline state the injected `AVIOInterruptCB` reads. Boxed so its
    /// address is stable for the raw pointer libav holds; `None` for a bare
    /// [`Demuxer::open`] (no interrupt callback installed). The box outlives the
    /// `AVFormatContext`: `input` (and so the callback) is dropped before this
    /// field in declaration order.
    deadline: Option<Box<ReadDeadline>>,
    /// The configured read timeout, replayed to arm the deadline per read.
    rw_timeout: Option<Duration>,
    /// The opened path/URL, for timeout error reporting.
    source: String,
}

impl Demuxer {
    /// Open `path` as a media container, probing its streams.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    pub fn open(path: &Path) -> Result<Self> {
        ensure_initialized()?;
        let input = ffmpeg::format::input(&path).map_err(|source| FfmpegError::OpenInput {
            path: path.display().to_string(),
            source,
        })?;
        let registration = register_format_context(&input);
        Ok(Self {
            registration,
            input,
            deadline: None,
            rw_timeout: None,
            source: path.display().to_string(),
        })
    }

    /// Open `path` with an injected `AVIOInterruptCB` + `rw_timeout` (GP-0,
    /// ADR-0030 recovery teardown).
    ///
    /// Unlike [`Demuxer::open`], a blocked open or [`Demuxer::read_packet`] on a
    /// stalled live input aborts within `options.rw_timeout` instead of hanging:
    /// the timeout is handed to libav as the `rw_timeout` `AVOption` **and** an
    /// owned [`ReadDeadline`] is wired into the context's `interrupt_callback`,
    /// so a wedged read returns control and the demuxer can be torn down. With no
    /// `rw_timeout` set, this behaves like [`Demuxer::open`] (callback installed
    /// but never armed).
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::ReadTimeout`] — the open exceeded `rw_timeout` (the input
    ///   never delivered a header; the wedged context was aborted).
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    pub fn open_with_interrupt(path: &Path, options: DemuxOptions) -> Result<Self> {
        ensure_initialized()?;
        let source = path.display().to_string();
        let rw_timeout = options.rw_timeout();

        // The deadline box must outlive the AVFormatContext: libav stores a raw
        // pointer to it in `interrupt_callback`. We keep ownership here and arm
        // it for the duration of the (potentially blocking) open.
        let deadline = Box::new(ReadDeadline::new());
        if let Some(timeout) = rw_timeout {
            deadline.arm(timeout);
        }

        let input = open_input_with_interrupt(path, deadline.as_ref(), rw_timeout)?;
        deadline.disarm();

        let registration = register_format_context(&input);
        Ok(Self {
            registration,
            input,
            deadline: Some(deadline),
            rw_timeout,
            source,
        })
    }

    /// The address of this demuxer's `AVFormatContext`, as a `usize`.
    ///
    /// This is the key under which the context is registered in the libav→
    /// resource attribution map (ADR-0060 §3.2) and the address a libav line's
    /// `parent_log_context_offset` walk resolves to. Returned as a `usize` so
    /// callers (tests, telemetry) can match a logged context without holding a
    /// raw pointer or touching libav memory.
    #[must_use]
    pub fn format_ctx_ptr(&self) -> usize {
        format_context_addr(&self.input)
    }

    /// Snapshot every stream's parameters.
    #[must_use]
    pub fn streams(&self) -> Vec<StreamParams> {
        self.input
            .streams()
            .map(|stream| {
                let params = stream.parameters();
                let kind = MediaKind::from(params.medium());
                let codec_name = codec_id_name(params.id());
                let mut p = StreamParams {
                    index: stream.index(),
                    kind,
                    codec_name,
                    time_base: from_ff_rational(stream.time_base()),
                    avg_frame_rate: rate_opt(stream.avg_frame_rate()),
                    width: 0,
                    height: 0,
                    sample_rate: 0,
                    channels: 0,
                    language: stream.metadata().get("language").map(str::to_owned),
                };
                // Decode-side geometry/audio params come from the codec context
                // built from the stream parameters; build a throwaway context to
                // read them without taking ownership of a decoder here.
                if let Ok(ctx) = ffmpeg::codec::context::Context::from_parameters(params) {
                    match kind {
                        MediaKind::Video => {
                            if let Ok(v) = ctx.decoder().video() {
                                p.width = v.width();
                                p.height = v.height();
                            }
                        }
                        MediaKind::Audio => {
                            if let Ok(a) = ctx.decoder().audio() {
                                p.sample_rate = a.rate();
                                p.channels = a.channels();
                            }
                        }
                        // Subtitle streams carry no decode-side geometry/audio to
                        // read here; the caption decoder reads what it needs from
                        // the stream parameters (`stream_parameters`).
                        MediaKind::Subtitle | MediaKind::Other => {}
                    }
                }
                p
            })
            .collect()
    }

    /// Build the **full typed [`StreamInventory`]** of every elementary stream
    /// this container offers (RT-1, ADR-0034 §3).
    ///
    /// Unlike [`Demuxer::best_stream`] — which keeps only the single best stream
    /// of one kind and discards the rest — this surfaces **every** row: each
    /// audio track (with its BCP-47 language + channel layout), each subtitle
    /// track, and each non-AV data stream. The coarse libav [`MediaKind`] is
    /// lifted to the canonical [`StreamKind`]; a `kind = Other` row is refined by
    /// codec name into [`StreamKind::Data`] (SCTE-35 / KLV) or
    /// [`StreamKind::Timecode`] via `from_media_and_codec`, so a data essence is
    /// classified rather than dropped.
    ///
    /// The container `language` tag is parsed leniently onto a [`Bcp47`]:
    /// unparseable or `und` tags become `None` rather than failing the probe.
    /// Each stream gets a soft-tier [`StableStreamId`] from its
    /// `(kind, ordinal-within-kind, codec, language, title)` (the general/libav
    /// key path); TS/HLS hard keys are folded in by later RT slices.
    ///
    /// This is the **discovery half** of RT-1; it does not route anything. The
    /// owning input id is left `None` (the ingest layer that binds a configured
    /// input fills it in).
    #[must_use]
    pub fn inventory(&self) -> StreamInventory {
        // Geometry/audio params are resolved by `streams()` (it builds the
        // throwaway codec context); zip that with a parallel pass over the libav
        // streams for disposition + title, which `StreamParams` does not carry.
        // Both iterate `self.input.streams()` in container order, so they align.
        let params = self.streams();
        // Per-kind ordinal counter (ordinal-within-kind for the soft id key).
        let mut ordinals: HashMap<char, u32> = HashMap::new();

        let streams = self
            .input
            .streams()
            .zip(params)
            .map(|(stream, p)| {
                let kind = StreamKind::from_coarse_and_codec(p.kind.into(), &p.codec_name);
                let disposition = stream.disposition();
                let language = p.language.as_deref().and_then(parse_language);
                // Bind the metadata ref once: `get` borrows it, so reading two
                // keys off a temporary `DictionaryRef` would dangle.
                let metadata = stream.metadata();
                let title = metadata
                    .get("title")
                    .or_else(|| metadata.get("handler_name"))
                    .map(str::to_owned);

                // Ordinal within this kind's routing family (scope char), so two
                // same-codec/same-language tracks get distinct soft ids.
                let scope = kind_scope_char(kind);
                let ordinal = ordinals.entry(scope).or_insert(0);
                let this_ordinal = *ordinal;
                *ordinal = ordinal.saturating_add(1);

                let id = StableStreamId::from_general(
                    kind,
                    this_ordinal,
                    &p.codec_name,
                    language.as_ref(),
                    title.as_deref(),
                );

                let detail = stream_detail(kind, &p, disposition);

                StreamDescriptor::new(id, kind, p.codec_name.clone(), detail)
                    .with_language(language)
                    .with_title(title)
                    .with_default(disposition.contains(Disposition::DEFAULT))
            })
            .collect();

        StreamInventory::from_streams(streams)
    }

    /// Index of the "best" stream of `kind`, per libav's heuristic.
    #[must_use]
    pub fn best_stream(&self, kind: MediaKind) -> Option<usize> {
        let ty = match kind {
            MediaKind::Video => Type::Video,
            MediaKind::Audio => Type::Audio,
            MediaKind::Subtitle => Type::Subtitle,
            MediaKind::Other => return None,
        };
        self.input.streams().best(ty).map(|s| s.index())
    }

    /// Clone the codec [`Parameters`] of the stream at `index`, or [`None`] if
    /// there is no such stream.
    ///
    /// This is what [`crate::caption_decode::CaptionDecoder::from_parameters`]
    /// consumes: a self-contained snapshot of the stream's codec parameters
    /// (codec id, extradata, geometry) that borrows nothing from the demuxer, so
    /// the caption decoder can be built on the input thread while the demuxer
    /// keeps reading.
    #[must_use]
    pub fn stream_parameters(&self, index: usize) -> Option<Parameters> {
        self.input.stream(index).map(|s| s.parameters())
    }

    /// Derive the `(codec, framing)` pair to pass to
    /// [`ReadPacket::is_idr`] for the stream at `index` (GP-1, ADR-0030).
    ///
    /// The codec comes from the stream's codec id; the framing is read from the
    /// stream's extradata — an avcC/hvcC config record (first byte `0x01`) means
    /// length-prefixed NALs with `nal_length_size = (extradata[4] & 3) + 1`,
    /// otherwise the elementary stream is Annex-B (start codes), and AV1 is always
    /// the OBU stream. Returns [`None`] for a non-existent stream or a codec whose
    /// random-access structure this classifier does not model.
    #[must_use]
    pub fn stream_idr_framing(&self, index: usize) -> Option<(CodecKind, NalFraming)> {
        let params = self.input.stream(index)?.parameters();
        let codec = match params.id() {
            ffmpeg::codec::Id::H264 => CodecKind::H264,
            ffmpeg::codec::Id::HEVC => CodecKind::Hevc,
            ffmpeg::codec::Id::AV1 => CodecKind::Av1,
            _ => return None,
        };
        let framing = match codec {
            // AV1 is carried as the low-overhead OBU stream.
            CodecKind::Av1 => NalFraming::Obu,
            // H.264 / HEVC: avcC/hvcC extradata ⇒ length-prefixed; else Annex-B.
            CodecKind::H264 | CodecKind::Hevc => nal_framing_from_extradata(&params),
            CodecKind::Other => return None,
        };
        Some((codec, framing))
    }

    /// Read the next coded packet from any stream, or [`None`] at end-of-stream.
    ///
    /// When opened via [`Demuxer::open_with_interrupt`] with a `rw_timeout`, a
    /// read stalled on a wedged live input aborts within the timeout and returns
    /// [`FfmpegError::ReadTimeout`] (rather than blocking forever) so recovery can
    /// tear the demuxer down (GP-0, ADR-0030).
    ///
    /// # Errors
    /// * [`FfmpegError::ReadTimeout`] — the read exceeded the configured timeout.
    /// * [`FfmpegError::Decode`] — a read error other than clean EOF / timeout.
    pub fn read_packet(&mut self) -> Result<Option<ReadPacket>> {
        // Arm the per-read deadline so the interrupt callback aborts a blocked
        // read; disarm it again whatever the outcome.
        if let (Some(deadline), Some(timeout)) = (self.deadline.as_ref(), self.rw_timeout) {
            deadline.arm(timeout);
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        let outcome = packet.read(&mut self.input);
        let expired = self.deadline.as_ref().is_some_and(|d| d.expired());
        if let Some(deadline) = self.deadline.as_ref() {
            deadline.disarm();
        }

        match outcome {
            Ok(()) => {
                let stream_index = packet.stream();
                Ok(Some(ReadPacket {
                    stream_index,
                    packet,
                }))
            }
            Err(ffmpeg::Error::Eof) => Ok(None),
            // A read aborted by the interrupt callback surfaces as a libav error;
            // disambiguate it from a real decode error by the armed deadline.
            Err(other) => {
                if expired {
                    Err(FfmpegError::ReadTimeout {
                        path: self.source.clone(),
                        timeout_ms: self
                            .rw_timeout
                            .map_or(0, |t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
                    })
                } else {
                    Err(FfmpegError::Decode(other))
                }
            }
        }
    }

    /// Read the next packet belonging to `stream_index`, skipping others, or
    /// [`None`] at end-of-stream.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a read error other than clean EOF.
    pub fn read_packet_for(&mut self, stream_index: usize) -> Result<Option<ReadPacket>> {
        loop {
            match self.read_packet()? {
                Some(pkt) if pkt.stream_index == stream_index => return Ok(Some(pkt)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// Seek the container to `timestamp` (in libav `AV_TIME_BASE` units,
    /// i.e. microseconds), landing on or before the target.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] if the seek fails.
    pub fn seek(&mut self, timestamp: i64) -> Result<()> {
        // `Input::seek` takes a target plus a min/max bracket; the full range
        // lets libav pick the nearest keyframe around the target.
        self.input.seek(timestamp, ..).map_err(FfmpegError::Decode)
    }
}

/// Mark every subtitle-medium stream in `input` `AVDISCARD_ALL` so libav stops
/// fetching it (HLS: `recheck_discard_flags` drops the rendition on the first
/// read), **except** the optional routed stream named by `keep` (ADR-T011).
///
/// This is **defence-in-depth** against the HLS `WebVTT`-rendition footgun: libav
/// can fold a `TYPE=SUBTITLES` rendition into the one shared `AVFormatContext` it
/// opens for the video, so a corrupt/404/expired `.vtt` either aborts the open or
/// makes `av_read_frame` return that rendition's error for the whole context —
/// blacking out the video tile. The PRIMARY guard is the caller's **variant-pin
/// pre-open** (point the main demuxer at one VIDEO variant media playlist, which
/// carries no SUBTITLES rendition), because on `FFmpeg` **8.x** the open ABORTS while
/// loading the rendition's first `.vtt` segment — *before* this post-open discard
/// could run (8.x removed the `strict` rendition gate that 7.x used to drop it
/// pre-probe). This discard remains as a backstop on the fall-through path (the
/// master could not be fetched/parsed, or a build still folds a subtitle in): the
/// main demuxer needs nothing from a `WebVTT` rendition (the isolated
/// `read_captions` reader is the sole `WebVTT` path), so discarding it loses no
/// stream while keeping the video alive.
///
/// `keep` (a routed in-container DVB-sub stream index — the MPEG-TS DVB-sub
/// route) is preserved. **Audio renditions are never touched**: the guard keys
/// strictly on `medium() == Subtitle`, so an HLS audio rendition (also folded
/// into the shared context) keeps flowing. Returns how many streams were
/// discarded.
///
/// Must be called **before the first `read_packet`**: libav's HLS demuxer fires
/// `recheck_discard_flags` one-shot on the first packet, so a discard set later
/// would not stop the rendition fetch.
#[allow(unsafe_code)]
#[must_use]
pub fn discard_unrouted_subtitles(
    input: &mut ffmpeg::format::context::Input,
    keep: Option<usize>,
) -> usize {
    let mut discarded = 0usize;
    for mut stream in input.streams_mut() {
        if stream.parameters().medium() != Type::Subtitle {
            continue;
        }
        if Some(stream.index()) == keep {
            continue;
        }
        // SAFETY: `stream.as_mut_ptr()` is the live `AVStream` this `StreamMut`
        // wraps (owned by the `AVFormatContext` we hold `&mut`). `discard` is a
        // plain enum field; we write `AVDISCARD_ALL`. ffmpeg-next 8.1's `StreamMut`
        // exposes no `set_discard`, so the one-field write is the supported way
        // (mirrors the `avio_fetch`/`hwframe` raw-field islands in this crate).
        unsafe {
            (*stream.as_mut_ptr()).discard = ffmpeg::ffi::AVDiscard::AVDISCARD_ALL;
        }
        discarded = discarded.saturating_add(1);
    }
    discarded
}

/// The address of an opened input's `AVFormatContext`, as a `usize` map key.
///
/// libav logs against the `AVFormatContext` (and child contexts whose
/// `parent_log_context_offset` reaches it), so its address is the attribution
/// key (ADR-0060 §3.2). Returned as a `usize`: it is only ever compared as a map
/// key, never dereferenced through this value.
#[allow(unsafe_code)]
#[must_use]
fn format_context_addr(input: &ffmpeg::format::context::Input) -> usize {
    // SAFETY: `Input::as_ptr` returns the live `*const AVFormatContext` this
    // `Input` owns; we only read its address (never dereference it here). The
    // pointer is valid for the borrow of `input`.
    let ptr = unsafe { input.as_ptr() };
    // reason(allow): pointer→address cast for a map key only (never cast back /
    // dereferenced); `<*const T>::addr()` is MSRV-1.84 and this crate is 1.82, so
    // `as usize` is the only option until the MSRV moves.
    #[allow(clippy::as_conversions)]
    {
        ptr as usize
    }
}

/// Register an opened input's `AVFormatContext*` in the libav→resource map for
/// the resource the current thread owns, if any (ADR-0060 §3.2, mechanism B).
///
/// The ingest/output thread enters its [`ResourceGuard`](crate::log_bridge::ResourceGuard)
/// **before** opening the demuxer (verified in `multiview-cli`'s `pipeline.rs`:
/// `ingest_resource_scope` / the output `ResourceGuard::enter` precede
/// `Demuxer::open`), so [`current_resource`](crate::log_bridge::current_resource)
/// is set here and the format context is tagged with that resource id. A libav
/// line emitted later on a libav-owned decoder / HLS sub-demuxer thread (where
/// the thread-local is **not** set) is then attributed by walking
/// `parent_log_context_offset` up to this registered format context.
///
/// Returns [`None`] when no resource is owned at open time — honest: no guessed
/// id (mechanism C). The returned guard is held in the [`Demuxer`] and removes
/// the entry on drop, ordered before the `AVFormatContext` is freed.
#[must_use]
fn register_format_context(
    input: &ffmpeg::format::context::Input,
) -> Option<crate::log_bridge::AvContextRegistration> {
    let context = crate::log_bridge::current_resource()?;
    let addr = format_context_addr(input);
    Some(crate::log_bridge::register_av_context(addr, context))
}

/// Human-readable codec id name (e.g. `"h264"`), or `"unknown"`.
fn codec_id_name(id: ffmpeg::codec::Id) -> String {
    // `Id` implements `Debug` as the libav constant name; the canonical short
    // name comes from the codec descriptor when available.
    ffmpeg::codec::decoder::find(id).map_or_else(
        || format!("{id:?}").to_ascii_lowercase(),
        |c| c.name().to_owned(),
    )
}

/// Treat libav's `0/0` / `0/1` "no rate" sentinels as absent.
fn rate_opt(rate: ffmpeg::Rational) -> Option<Rational> {
    if rate.numerator() == 0 {
        None
    } else {
        Some(from_ff_rational(rate))
    }
}

/// Parse a raw container `language` tag onto a [`Bcp47`], **leniently**: an
/// unparseable or `und` (undetermined) tag becomes [`None`] rather than failing
/// the probe (RT-1: discovery must never reject a stream over a bad language
/// metadata value).
fn parse_language(raw: &str) -> Option<Bcp47> {
    Bcp47::parse(raw).ok()
}

/// The routing-family scope character a [`StreamKind`] belongs to, used only to
/// key the per-kind ordinal counter for the soft [`StableStreamId`].
///
/// Mirrors `multiview-core`'s kind scoping (video `v` / audio `a` / subtitle `s`
/// / data `d` / timecode `t`): two same-codec/same-language tracks of one family
/// must get distinct ordinals so their soft ids do not collide.
const fn kind_scope_char(kind: StreamKind) -> char {
    match kind {
        StreamKind::Video => 'v',
        StreamKind::Audio => 'a',
        StreamKind::Subtitle => 's',
        StreamKind::Timecode(_) => 't',
        // `StreamKind::Data(_)` plus the `#[non_exhaustive]` fallthrough: a data
        // stream — and any future kind — defaults to the generic data scope so it
        // still gets a stable per-family ordinal.
        StreamKind::Data(_) | _ => 'd',
    }
}

/// Build the kind-specific [`StreamDetail`] for one elementary stream from its
/// probed [`StreamParams`] + libav disposition.
fn stream_detail(kind: StreamKind, p: &StreamParams, disposition: Disposition) -> StreamDetail {
    match kind {
        StreamKind::Video => StreamDetail::Video {
            width: p.width,
            height: p.height,
            frame_rate: p.avg_frame_rate,
        },
        StreamKind::Audio => StreamDetail::Audio {
            channels: p.channels,
            sample_rate: p.sample_rate,
        },
        StreamKind::Subtitle => StreamDetail::Subtitle {
            forced: disposition.contains(Disposition::FORCED),
        },
        // Data (SCTE-35 / KLV) and timecode are passthrough: no AV detail to
        // probe. `StreamKind` is `#[non_exhaustive]`; an unmodelled future kind
        // is likewise carried as a passthrough rather than dropped.
        StreamKind::Data(_) | StreamKind::Timecode(_) | _ => StreamDetail::Passthrough,
    }
}

// ---------------------------------------------------------------------------
// GP-0 FFI: interrupt callback + rw_timeout open (ADR-0030 recovery teardown).
//
// reason: combining an `AVIOInterruptCB` with a `rw_timeout` option dict on the
// open has no safe `ffmpeg_next` wrapper (`input_with_interrupt` installs no
// options and leaks the closure; `input_with_dictionary` installs no callback),
// so the open path below drives the raw libav FFI directly. The crate is
// `unsafe_code = deny`; every `unsafe` operation carries a `// SAFETY:` note and
// is bounded to libav objects we own. The interrupt callback never allocates and
// never unwinds across the FFI boundary.
#[allow(unsafe_code)]
mod ffi_open {
    use std::ffi::CString;
    use std::ptr;
    use std::time::Duration;

    use ffmpeg::ffi;
    use ffmpeg_next as ffmpeg;
    use libc::{c_int, c_void};

    use super::ReadDeadline;
    use crate::error::{FfmpegError, Result};

    /// `extern "C"` interrupt callback libav polls while a read is blocked.
    ///
    /// `opaque` is the `*const ReadDeadline` we installed. The body only reads two
    /// atomics + a monotonic clock and returns `1` to abort or `0` to continue —
    /// no allocation, no `panic!`, no unwinding across FFI (every operation here
    /// is panic-free, so no `catch_unwind` is needed).
    extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
        if opaque.is_null() {
            return 0;
        }
        // SAFETY: `opaque` is the `&ReadDeadline` pointer installed in
        // `open_with_interrupt`; the owning `Box<ReadDeadline>` lives in the
        // `Demuxer` for at least as long as the `AVFormatContext` that holds this
        // callback (the context is closed in `Demuxer::drop` before the box is
        // freed, by field declaration order), so the reference is valid here.
        let deadline: &ReadDeadline = unsafe { &*opaque.cast::<ReadDeadline>() };
        c_int::from(deadline.expired())
    }

    /// Open `path` with the interrupt callback wired to `deadline` and, if set,
    /// the `rw_timeout` `AVOption`, returning a wrapped [`Input`](ffmpeg::format::context::Input).
    ///
    /// `deadline` must outlive the returned context (the caller keeps the owning
    /// `Box` alive in the `Demuxer`).
    pub(super) fn open_input_with_interrupt(
        path: &std::path::Path,
        deadline: &ReadDeadline,
        rw_timeout: Option<Duration>,
    ) -> Result<ffmpeg::format::context::Input> {
        let source = path.display().to_string();
        let curl = CString::new(source.as_str())
            .map_err(|_| open_err(&source, "path has an interior NUL"))?;

        // SAFETY: avformat_alloc_context returns an owned context or null. We
        // install the interrupt callback on it, hand it to avformat_open_input
        // (which adopts it on success and frees + nulls it on failure), and free
        // it ourselves on every early-return path before the open.
        let mut ps_io = unsafe { ffi::avformat_alloc_context() };
        if ps_io.is_null() {
            return Err(open_err(&source, "avformat_alloc_context returned null"));
        }
        // SAFETY: `ps_io` is the freshly-allocated context; `deadline` is a valid
        // reference whose owning `Box` outlives the context. We point the
        // interrupt callback's opaque at it.
        unsafe {
            (*ps_io).interrupt_callback = ffi::AVIOInterruptCB {
                callback: Some(interrupt_cb),
                opaque: ptr::from_ref(deadline).cast::<c_void>().cast_mut(),
            };
        }

        // Build the rw_timeout option dict (microseconds), if requested. On any
        // setup failure we must free the context we still own.
        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        if let Some(timeout) = rw_timeout {
            let micros = u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX);
            let set = build_timeout_dict(&mut opts, micros);
            if set.is_err() {
                // SAFETY: frees the partial dict and the allocated-but-unopened
                // context (both owned by us; close_input handles a non-opened ctx).
                unsafe {
                    ffi::av_dict_free(&raw mut opts);
                    ffi::avformat_close_input(&raw mut ps_io);
                }
                return Err(open_err(&source, "av_dict_set(rw_timeout) failed"));
            }
        }

        // SAFETY: `ps_io` is our allocated context; `curl` is a valid CString for
        // the call; `opts` is our live dict (or null). On success libav adopts
        // the context (we wrap it); on failure it frees and nulls it.
        let open = unsafe {
            ffi::avformat_open_input(&raw mut ps_io, curl.as_ptr(), ptr::null(), &raw mut opts)
        };
        // SAFETY: av_dict_free is null-safe; frees any entries open_input left.
        unsafe { ffi::av_dict_free(&raw mut opts) };
        if open < 0 {
            // avformat_open_input already freed + nulled the context on error.
            return Err(FfmpegError::OpenInput {
                path: source,
                source: ffmpeg::Error::from(open),
            });
        }

        // SAFETY: `ps_io` is the opened context; find_stream_info probes it.
        let info = unsafe { ffi::avformat_find_stream_info(ps_io, ptr::null_mut()) };
        if info < 0 {
            // SAFETY: close the opened context we still own before returning.
            unsafe { ffi::avformat_close_input(&raw mut ps_io) };
            return Err(FfmpegError::OpenInput {
                path: source,
                source: ffmpeg::Error::from(info),
            });
        }

        // SAFETY: `ps_io` is a valid, opened input context; `Input::wrap` adopts
        // it and frees it via `avformat_close_input` in its destructor.
        Ok(unsafe { ffmpeg::format::context::Input::wrap(ps_io) })
    }

    /// Set the `rw_timeout` (microseconds) `AVOption` into the `opts` dict.
    ///
    /// `opts` must point at a (possibly-null) dictionary slot owned by the caller,
    /// who frees it on every path. Returns `Err(())` if the key/value held an
    /// interior NUL or `av_dict_set` failed.
    fn build_timeout_dict(
        opts: &mut *mut ffi::AVDictionary,
        micros: u64,
    ) -> core::result::Result<(), ()> {
        let to_key = CString::new("rw_timeout").map_err(|_| ())?;
        let to_val = CString::new(micros.to_string()).map_err(|_| ())?;
        // SAFETY: `opts` is the caller's dict slot (null or live); av_dict_set
        // allocates/links the entry. The caller frees the dict on every path.
        let r = unsafe { ffi::av_dict_set(opts, to_key.as_ptr(), to_val.as_ptr(), 0) };
        if r < 0 {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Build a [`FfmpegError::OpenInput`]-shaped error for a setup failure that
    /// prevented the open from starting (interior NUL, allocation failure), as an
    /// EINVAL-wrapped libav error.
    fn open_err(source: &str, _reason: &'static str) -> FfmpegError {
        FfmpegError::OpenInput {
            path: source.to_owned(),
            source: ffmpeg::Error::from(ffi::AVERROR(libc::EINVAL)),
        }
    }
}

use ffi_open::open_input_with_interrupt;

/// Read the NAL framing of an H.264 / HEVC stream from its codec extradata.
///
/// avcC / hvcC config records begin with `configurationVersion == 1`; their
/// `lengthSizeMinusOne` lives in `extradata[4] & 0x03`, so each NAL is preceded
/// by `(that + 1)` big-endian length bytes. Anything else (no extradata, or a
/// leading `0x00 0x00 0x01` start code) is the Annex-B elementary stream.
#[allow(unsafe_code)]
fn nal_framing_from_extradata(params: &Parameters) -> NalFraming {
    // SAFETY: `params.as_ptr()` is a valid `AVCodecParameters` for the lifetime
    // of the borrow. `extradata`/`extradata_size` are plain fields; we read the
    // pointer + length and build a bounded slice (or treat absent/short as
    // Annex-B). We never write through the pointer or outlive `params`.
    let extradata: &[u8] = unsafe {
        let raw = params.as_ptr();
        let ptr = (*raw).extradata;
        let len = usize::try_from((*raw).extradata_size).unwrap_or(0);
        if ptr.is_null() || len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(ptr, len)
        }
    };
    match (extradata.first(), extradata.get(4)) {
        // avcC / hvcC: version byte 0x01, length-size in the low 2 bits of byte 4.
        (Some(&0x01), Some(&size_byte)) => NalFraming::LengthPrefixed {
            nal_length_size: (size_byte & 0x03) + 1,
        },
        _ => NalFraming::AnnexB,
    }
}

#[cfg(test)]
mod interrupt_logic_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::{should_interrupt, DemuxOptions, ReadDeadline, DEADLINE_DISARMED};

    #[test]
    fn disarmed_deadline_never_interrupts() {
        // The sentinel (no deadline armed) must never abort a read, no matter how
        // large the elapsed time — a healthy read must run to completion.
        assert!(!should_interrupt(0, DEADLINE_DISARMED));
        assert!(!should_interrupt(u64::MAX, DEADLINE_DISARMED));
    }

    #[test]
    fn armed_deadline_interrupts_only_at_or_past_the_deadline() {
        // Before the deadline: keep reading. At/after it: abort.
        assert!(!should_interrupt(99, 100));
        assert!(should_interrupt(100, 100));
        assert!(should_interrupt(101, 100));
    }

    #[test]
    fn arm_then_expire_via_a_zero_timeout_fires_immediately() {
        // Arming a zero (or tiny) timeout means a blocked read aborts at once.
        let d = ReadDeadline::new();
        assert!(!d.expired(), "a fresh deadline is disarmed");
        d.arm(Duration::from_nanos(0));
        // Any nonzero elapsed time is already >= a now+0 deadline.
        assert!(d.expired(), "a zero-timeout deadline expires immediately");
    }

    #[test]
    fn arm_with_a_large_timeout_does_not_expire_immediately() {
        let d = ReadDeadline::new();
        d.arm(Duration::from_secs(3600));
        assert!(
            !d.expired(),
            "a one-hour deadline must not be already expired"
        );
    }

    #[test]
    fn disarm_clears_an_armed_deadline() {
        let d = ReadDeadline::new();
        d.arm(Duration::from_nanos(0));
        assert!(d.expired());
        d.disarm();
        assert!(!d.expired(), "disarm must stop the interrupt firing");
    }

    #[test]
    fn options_round_trip_the_timeout() {
        assert_eq!(DemuxOptions::new().rw_timeout(), None);
        let opts = DemuxOptions::new().with_rw_timeout(Duration::from_millis(250));
        assert_eq!(opts.rw_timeout(), Some(Duration::from_millis(250)));
    }
}
