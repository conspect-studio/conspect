//! Minimal, safe demux + video-decode path over libav* (the `ffmpeg` feature).
//!
//! This is the first vertical slice proving the chosen libav* binding
//! ([`ffmpeg_next`], which links the **system** libav* via `pkg-config` and
//! auto-detects the installed `FFmpeg` version at build time) compiles and runs
//! against the host `FFmpeg`. It opens a container, finds the best video stream,
//! constructs a software decoder, pumps packets, and yields the first decoded
//! frame's geometry / pixel format / presentation timestamp.
//!
//! ## Scope and invariants
//! * **No raw FFI here.** `ffmpeg_next` owns the `unsafe` boundary; this module
//!   is ordinary safe Rust calling its wrappers, so the crate's
//!   `unsafe_code = "deny"` is upheld with zero `unsafe` blocks on this path.
//! * **`!Sync` by construction.** [`VideoDecoder`] holds libav contexts that
//!   must not be shared across threads unsynchronized (CLAUDE.md §7); it is
//!   `Send` but deliberately not `Sync` because it owns `&mut`-style state and
//!   exposes no interior mutability.
//! * **PTS are *input* timestamps.** [`DecodedFrameInfo::pts`] is the raw,
//!   per-input presentation timestamp in stream time-base ticks. The engine's
//!   output clock re-stamps everything from the tick counter (invariant #1/#3);
//!   nothing here is ever fed to a muxer.

use std::path::Path;
use std::sync::Once;

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// Guards one-time global libav* initialization.
static INIT: Once = Once::new();

/// Run libav*'s global initialization exactly once for the process.
///
/// `ffmpeg_next::init()` registers the demuxers/decoders and is idempotent at
/// the libav level; the [`Once`] simply avoids redundant calls. The first
/// caller observes any failure; subsequent callers assume success (libav has
/// no way to re-report it, and a failed registration is fatal regardless).
///
/// On the first successful init this also installs the libav → `tracing` log
/// bridge ([`crate::log_bridge`]), replacing libav's unbounded stderr writer
/// with a rate-limited structured logger so a glitchy/corrupt input can never
/// flood the operator's logs.
///
/// # Errors
/// Returns [`FfmpegError::Init`] if libav initialization fails.
pub fn ensure_initialized() -> Result<()> {
    let mut outcome: Result<()> = Ok(());
    INIT.call_once(|| {
        if let Err(err) = ffmpeg::init() {
            outcome = Err(FfmpegError::Init(err));
            return;
        }
        // libav is up: route + rate-limit its log output before any decoder can
        // emit a flood. Idempotent and infallible.
        crate::log_bridge::install();
    });
    outcome
}

/// Geometry, pixel format, and timing of a single decoded video frame.
///
/// This is a plain owned snapshot — it borrows nothing from the decoder, so it
/// can outlive the [`VideoDecoder`] and cross thread/channel boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodedFrameInfo {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// libav pixel format of the decoded frame (e.g. `Pixel::YUV420P`).
    pub format: Pixel,
    /// Raw presentation timestamp in *stream time-base ticks*, if the frame
    /// carries one. This is an **input** timestamp — never forward it to an
    /// encoder/muxer; the output clock re-stamps from the tick counter.
    pub pts: Option<i64>,
}

/// A source of coded video packets, pulled **one at a time**.
///
/// The streaming first-frame loop ([`decode_first_via`]) reads through this
/// trait instead of materializing every packet up front, so it can return the
/// instant the first frame decodes and never buffers an entire file (or a live
/// stream that never ends). Each call yields the next packet **belonging to the
/// bound video stream** (the implementation filters out other streams), or
/// `Ok(None)` at a clean end-of-stream.
trait PacketSource {
    /// The packet type produced — generic so a test can substitute a lightweight
    /// fake without constructing a real libav packet.
    type Packet;

    /// Pull the next video packet, or `Ok(None)` at clean end-of-stream.
    ///
    /// # Errors
    /// Propagates a [`FfmpegError::Decode`] for a non-EOF read failure.
    fn next_video_packet(&mut self) -> Result<Option<Self::Packet>>;

    /// How many video packets this source has yielded so far.
    ///
    /// Used to prove the loop stops early (it must not drain the whole stream to
    /// decode frame 0) and to surface a bounded resource-accounting figure.
    fn consumed(&self) -> usize;
}

/// A frame decoder fed packets and drained one frame at a time.
///
/// Abstracts [`ffmpeg::decoder::Video`] so the streaming loop is unit-testable
/// with a fake that decides exactly when a frame appears and never needs libav.
trait FrameDecoder {
    /// The packet type accepted — matches the paired [`PacketSource::Packet`].
    type Packet;

    /// Send one coded packet to the decoder.
    ///
    /// # Errors
    /// Propagates a [`FfmpegError::Decode`] on a libav send failure.
    fn send_packet(&mut self, packet: &Self::Packet) -> Result<()>;

    /// Signal end-of-stream so buffered frames can be flushed.
    ///
    /// # Errors
    /// Propagates a [`FfmpegError::Decode`] on a libav error.
    fn send_eof(&mut self) -> Result<()>;

    /// Try to receive exactly one frame: `Ok(Some(info))` on a frame,
    /// `Ok(None)` on the `EAGAIN`/`EOF` drain signals.
    ///
    /// # Errors
    /// Propagates a [`FfmpegError::Decode`] for any other libav failure.
    fn receive_one(&mut self) -> Result<Option<DecodedFrameInfo>>;
}

/// Demux + decode until the **first** frame appears, pulling packets lazily.
///
/// Streams packets from `source` into `decoder` one at a time, draining a frame
/// after each, and returns the instant a frame is produced — so it consumes only
/// the packets needed for frame 0, never the whole stream. Only on a clean end
/// of stream does it flush the decoder (`send_eof` + a final drain). If no frame
/// is ever produced it returns [`FfmpegError::EndOfStream`] with `kind`.
///
/// # Errors
/// * [`FfmpegError::Decode`] — a read/send/receive libav failure.
/// * [`FfmpegError::EndOfStream`] — the stream ended before any frame decoded.
fn decode_first_via<S, D>(
    source: &mut S,
    decoder: &mut D,
    kind: &'static str,
) -> Result<DecodedFrameInfo>
where
    S: PacketSource,
    D: FrameDecoder<Packet = S::Packet>,
{
    while let Some(packet) = source.next_video_packet()? {
        decoder.send_packet(&packet)?;
        if let Some(info) = decoder.receive_one()? {
            return Ok(info);
        }
    }

    // Clean end of stream: flush whatever the decoder still has buffered.
    decoder.send_eof()?;
    if let Some(info) = decoder.receive_one()? {
        return Ok(info);
    }

    Err(FfmpegError::EndOfStream(kind))
}

/// Real [`PacketSource`] over a libav input: reads packets one at a time and
/// keeps only those on the bound video stream.
///
/// Borrows the demuxer's `Input` mutably for the duration of the loop; this is
/// the disjoint-field split that lets the decoder be borrowed mutably too (the
/// two live in different fields of [`VideoDecoder`]).
struct InputPacketSource<'a> {
    input: &'a mut ffmpeg::format::context::Input,
    stream_index: usize,
    consumed: usize,
}

impl PacketSource for InputPacketSource<'_> {
    type Packet = ffmpeg::codec::packet::Packet;

    fn next_video_packet(&mut self) -> Result<Option<Self::Packet>> {
        loop {
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match packet.read(self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.consumed = self.consumed.saturating_add(1);
                        return Ok(Some(packet));
                    }
                    // A packet from another stream: skip it without counting it
                    // against the video budget and keep reading.
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(other) => return Err(FfmpegError::Decode(other)),
            }
        }
    }

    fn consumed(&self) -> usize {
        self.consumed
    }
}

/// Real [`FrameDecoder`] over a libav software video decoder.
struct LibavFrameDecoder<'a> {
    decoder: &'a mut ffmpeg::decoder::Video,
    frame: ffmpeg::frame::Video,
}

impl FrameDecoder for LibavFrameDecoder<'_> {
    type Packet = ffmpeg::codec::packet::Packet;

    fn send_packet(&mut self, packet: &Self::Packet) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .map_err(FfmpegError::Decode)
    }

    fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    fn receive_one(&mut self) -> Result<Option<DecodedFrameInfo>> {
        VideoDecoder::drain_one(self.decoder, &mut self.frame)
    }
}

/// A software video decoder bound to one stream of an opened input container.
///
/// Constructed via [`VideoDecoder::open`]. Holds the demuxer (`input` context)
/// and the decoder context together so packets can be pumped without exposing
/// any raw libav pointer. Not `Sync`: libav contexts require external
/// synchronization for shared access (CLAUDE.md §7).
pub struct VideoDecoder {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    /// Number of video packets pulled by the most recent
    /// [`decode_first_frame`](Self::decode_first_frame) call. Stays `0` until
    /// that method runs; lets callers (and tests) confirm the first frame was
    /// decoded from only the packets it needed rather than the whole stream.
    packets_consumed: usize,
}

impl VideoDecoder {
    /// Open `path` as a media container and build a software video decoder for
    /// its best video stream.
    ///
    /// Performs one-time libav initialization, opens and probes the container,
    /// selects the best video stream, and constructs a decoder from that
    /// stream's codec parameters.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    /// * [`FfmpegError::StreamNotFound`] — no video stream is present.
    /// * [`FfmpegError::OpenDecoder`] — a decoder could not be built for the
    ///   selected stream.
    pub fn open(path: &Path) -> Result<Self> {
        ensure_initialized()?;

        let input = ffmpeg::format::input(&path).map_err(|source| FfmpegError::OpenInput {
            path: path.display().to_string(),
            source,
        })?;

        let (stream_index, parameters) = {
            let stream = input
                .streams()
                .best(Type::Video)
                .ok_or(FfmpegError::StreamNotFound("video"))?;
            (stream.index(), stream.parameters())
        };

        let codec_context = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = codec_context
            .decoder()
            .video()
            .map_err(FfmpegError::OpenDecoder)?;

        Ok(Self {
            input,
            decoder,
            stream_index,
            packets_consumed: 0,
        })
    }

    /// The index of the video stream this decoder is bound to.
    #[must_use]
    pub fn stream_index(&self) -> usize {
        self.stream_index
    }

    /// Number of video-stream packets pulled by the most recent
    /// [`decode_first_frame`](Self::decode_first_frame) call.
    ///
    /// `0` before the first call. After a successful decode this is the small
    /// number of packets the decoder actually needed for frame 0 — **not** the
    /// whole stream — which is what makes the streaming behaviour observable: a
    /// loop that buffered every packet to EOF would report the full packet
    /// count instead.
    #[must_use]
    pub fn packets_consumed(&self) -> usize {
        self.packets_consumed
    }

    /// Demux and decode until the first video frame is produced, returning its
    /// geometry / format / PTS.
    ///
    /// Pulls packets from the bound video stream **one at a time**, sending each
    /// to the decoder and draining a frame after it, and returns the instant the
    /// first frame decodes. It therefore consumes only the packets needed for
    /// frame 0 — it never buffers the whole file, and never grows without bound
    /// on a live/infinite input. Only on a clean end-of-stream does it flush the
    /// decoder (`send_eof` + a final drain). `EAGAIN` (decoder needs more input)
    /// is handled transparently. The number of packets consumed is recorded for
    /// [`packets_consumed`](Self::packets_consumed).
    ///
    /// # Errors
    /// * [`FfmpegError::Decode`] — a non-drain libav error while reading,
    ///   sending a packet, or receiving a frame.
    /// * [`FfmpegError::EndOfStream`] — the input ended before any video frame
    ///   could be decoded.
    pub fn decode_first_frame(&mut self) -> Result<DecodedFrameInfo> {
        // Split the disjoint borrows: `input` feeds the packet source while
        // `decoder` is borrowed by the frame decoder. They are different fields,
        // so the borrow checker accepts both `&mut` at once.
        let mut source = InputPacketSource {
            input: &mut self.input,
            stream_index: self.stream_index,
            consumed: 0,
        };
        let mut decoder = LibavFrameDecoder {
            decoder: &mut self.decoder,
            frame: ffmpeg::frame::Video::empty(),
        };

        let result = decode_first_via(&mut source, &mut decoder, "video");
        self.packets_consumed = source.consumed();
        result
    }

    /// Flush the decoder's buffered state (`avcodec_flush_buffers`), discarding
    /// every buffered/reordered frame so decoding can resume cleanly from a new
    /// position after a container seek.
    ///
    /// A seek without a flush leaves the decoder holding pictures decoded
    /// *before* the seek; for codecs that reorder (B-frames) those stale frames
    /// would surface after the seek as out-of-order garbage. It is safe to call
    /// between packets (a no-op on a fresh/drained decoder), and the decoder
    /// stays usable afterwards.
    ///
    /// # Safety / FFI invariant
    /// The flush runs through `ffmpeg_next`'s safe `Decoder::flush` wrapper,
    /// which owns the single `unsafe` `avcodec_flush_buffers` call — this module
    /// adds no raw FFI (it is zero-`unsafe` by construction). Soundness rests on
    /// the `AVCodecContext` being **owned** by this decoder and reached only
    /// through `&mut self` (the type is `Send + !Sync`, satisfying libav's
    /// single-threaded-access rule); flushing only drops buffered frames and
    /// resets decode state, which is valid between packets.
    ///
    /// # Errors
    /// Returns [`Result::Ok`] — the underlying libav flush is infallible; the
    /// `Result` return mirrors the rest of this crate's decoder API.
    pub fn flush(&mut self) -> Result<()> {
        self.decoder.flush();
        Ok(())
    }

    /// Try to receive exactly one frame from the decoder.
    ///
    /// Returns `Ok(Some(info))` if a frame was produced, `Ok(None)` if the
    /// decoder reported `EAGAIN`/`EOF` (needs more input / fully drained), or
    /// an error for any other libav failure.
    fn drain_one(
        decoder: &mut ffmpeg::decoder::Video,
        frame: &mut ffmpeg::frame::Video,
    ) -> Result<Option<DecodedFrameInfo>> {
        match decoder.receive_frame(frame) {
            Ok(()) => Ok(Some(DecodedFrameInfo {
                width: frame.width(),
                height: frame.height(),
                format: frame.format(),
                pts: frame.pts(),
            })),
            // `EAGAIN` (more input needed) and `Eof` (fully drained) are normal
            // control-flow signals, not failures.
            Err(
                ffmpeg::Error::Other {
                    errno: ffmpeg::util::error::EAGAIN,
                }
                | ffmpeg::Error::Eof,
            ) => Ok(None),
            Err(other) => Err(FfmpegError::Decode(other)),
        }
    }
}

// `VideoDecoder` owns libav contexts that must not be shared across threads
// without synchronization; it is `Send` (it may move to a decode thread) but
// intentionally `!Sync`. `ffmpeg_next`'s context types are already `Send` and
// not `Sync`, so no manual marker impls are needed — this is asserted in tests.

#[cfg(test)]
mod tests {
    use super::{decode_first_via, DecodedFrameInfo, FrameDecoder, PacketSource, Result};
    use crate::error::FfmpegError;
    use ffmpeg::format::Pixel;
    use ffmpeg_next as ffmpeg;

    /// A bounded fake packet source: yields exactly `total` unit "packets", one
    /// per call, then `None`. It counts how many it actually handed out so a test
    /// can assert the loop stopped early instead of draining everything — the
    /// exact regression guard for the "buffer the whole file" bug.
    struct FakeSource {
        remaining: usize,
        consumed: usize,
    }

    impl FakeSource {
        fn with(total: usize) -> Self {
            Self {
                remaining: total,
                consumed: 0,
            }
        }
    }

    impl PacketSource for FakeSource {
        type Packet = ();

        fn next_video_packet(&mut self) -> Result<Option<()>> {
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            self.consumed += 1;
            Ok(Some(()))
        }

        fn consumed(&self) -> usize {
            self.consumed
        }
    }

    /// A fake decoder that emits a frame after exactly `emit_after` packets have
    /// been sent (`None` => never emit a frame, modelling a video stream that
    /// decodes nothing). It also records whether EOF was flushed.
    struct FakeDecoder {
        sent: usize,
        emit_after: Option<usize>,
        eof_sent: bool,
    }

    impl FakeDecoder {
        fn emitting_after(n: usize) -> Self {
            Self {
                sent: 0,
                emit_after: Some(n),
                eof_sent: false,
            }
        }

        fn never_emitting() -> Self {
            Self {
                sent: 0,
                emit_after: None,
                eof_sent: false,
            }
        }
    }

    impl FrameDecoder for FakeDecoder {
        type Packet = ();

        fn send_packet(&mut self, _packet: &()) -> Result<()> {
            self.sent += 1;
            Ok(())
        }

        fn send_eof(&mut self) -> Result<()> {
            self.eof_sent = true;
            Ok(())
        }

        fn receive_one(&mut self) -> Result<Option<DecodedFrameInfo>> {
            match self.emit_after {
                Some(after) if self.sent >= after => Ok(Some(DecodedFrameInfo {
                    width: 320,
                    height: 240,
                    format: Pixel::YUV420P,
                    pts: Some(0),
                })),
                _ => Ok(None),
            }
        }
    }

    #[test]
    fn returns_after_consuming_only_the_packets_frame_zero_needs() {
        // A stream offering 1000 packets, but frame 0 is ready after the first.
        // A correct streaming loop must return having pulled exactly ONE packet;
        // the old "collect every packet into a Vec" loop would consume all 1000.
        let mut source = FakeSource::with(1000);
        let mut decoder = FakeDecoder::emitting_after(1);

        let info = decode_first_via(&mut source, &mut decoder, "video")
            .expect("first frame should decode from the first packet");

        assert_eq!(info.width, 320, "decoded width");
        assert_eq!(
            source.consumed(),
            1,
            "must stop after the one packet frame 0 needed, not drain the stream"
        );
        assert!(
            !decoder.eof_sent,
            "must not flush EOF when a frame decoded mid-stream"
        );
    }

    #[test]
    fn returns_after_decoder_latency_without_draining_the_rest() {
        // The decoder needs three packets of priming before frame 0 appears.
        // The loop must consume exactly three of the 1000 available, no more.
        let mut source = FakeSource::with(1000);
        let mut decoder = FakeDecoder::emitting_after(3);

        decode_first_via(&mut source, &mut decoder, "video").expect("frame after 3 packets");

        assert_eq!(
            source.consumed(),
            3,
            "consume exactly the priming packets frame 0 needed"
        );
    }

    #[test]
    fn stream_with_no_video_frame_returns_end_of_stream() {
        // Packets exist but none ever decode to a frame; after the source is
        // exhausted the loop must flush EOF and then surface EndOfStream — never
        // an Ok, never a panic, and the `kind` label must be preserved.
        let mut source = FakeSource::with(5);
        let mut decoder = FakeDecoder::never_emitting();

        let kind = match decode_first_via(&mut source, &mut decoder, "video") {
            Ok(_) => panic!("a stream that decodes no frame must not return Ok"),
            Err(FfmpegError::EndOfStream(kind)) => kind,
            Err(other) => panic!("expected EndOfStream, got {other}"),
        };

        assert_eq!(kind, "video", "EndOfStream must carry the stream kind");
        assert_eq!(source.consumed(), 5, "every available packet was tried");
        assert!(decoder.eof_sent, "EOF must be flushed before giving up");
    }

    #[test]
    fn empty_stream_returns_end_of_stream() {
        // A source that yields nothing at all (an immediately-empty stream).
        let mut source = FakeSource::with(0);
        let mut decoder = FakeDecoder::never_emitting();

        match decode_first_via(&mut source, &mut decoder, "video") {
            Err(FfmpegError::EndOfStream("video")) => {}
            Ok(_) => panic!("an empty stream must not yield a frame"),
            Err(other) => panic!("expected EndOfStream(\"video\"), got {other}"),
        }
        assert_eq!(source.consumed(), 0, "nothing to consume");
        assert!(decoder.eof_sent, "EOF flushed even for an empty stream");
    }
}
