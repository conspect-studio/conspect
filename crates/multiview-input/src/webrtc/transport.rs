//! WebRTC ingest — the RTP-receive seam — **feature `webrtc`**.
//!
//! This module owns the *testable core* of a WebRTC receive session: the
//! application-layer **[`MediaEngine`] seam** that a concrete ICE/DTLS/SRTP engine
//! plugs into, the H.264 RTP **depacketize ->
//! access-unit** seam ([`H264Depacketizer`]), and the [`WebRtcProducer`]: an
//! honest **compressed media-event producer** that turns decrypted RTP into
//! typed [`MediaEvent`]s — keyframe-gated H.264 access units and Opus audio
//! frames — for the application layer to decode (ADR-T014: decode happens
//! there, and frame geometry/color come from the decoder's SPS/VUI, never
//! from anything declared here).
//!
//! ## No native WebRTC library here (pure / LGPL-clean default)
//!
//! `multiview-input` is `unsafe_code = forbid` and pulls in **no** native WebRTC
//! crate. The real ICE candidate gathering, DTLS handshake, and SRTP
//! depacketization need a network + crypto stack; per the module design that
//! engine is supplied by the **application layer** behind the off-by-default
//! `webrtc` feature (e.g. a sans-IO `str0m`-based driver wired at the binary
//! level). This crate defines the [`MediaEngine`] trait it drives and the pure
//! RTP -> frame adapter; the socket/crypto path is never linked here, keeping the
//! default build pure-Rust.
//!
//! ## Isolation (invariants #1 / #2 / #10)
//!
//! A WebRTC source is **sampled, never pacing**: the [`WebRtcProducer`] only ever
//! *pulls* from the engine and yields what is ready; it never blocks the output
//! clock. The depacketizers are pure state machines over injected packets with
//! **bounded** buffers that drop, never grow. A dead or lagging engine yields
//! `None`/`Pending` and is held — it cannot stall the engine.

use crate::error::Result;
use crate::normalize::WrapBits;
use crate::webrtc::route::{MediaEvent, RtpRouter};
use crate::webrtc::NegotiatedSession;

/// The RTP media clock rate WebRTC video rides on (90 kHz, RFC 8866 / RFC 6184).
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// An upper bound on the reorder window the depacketizer holds, in packets.
///
/// A WebRTC contribution ingest is typically a low-latency LAN/WAN path; a few
/// dozen packets of reorder headroom is ample. The window is **bounded**: a
/// packet beyond it forces the oldest out (drop-oldest, never grows — invariant
/// #2 / #5). This caps per-source memory regardless of the input.
pub const MAX_REORDER_PACKETS: usize = 128;

/// An upper bound on the bytes a single reassembled access unit may accumulate.
///
/// FU-A fragmentation can in principle span many packets; this cap (8 MiB)
/// rejects a pathological fragmentation that would force an unbounded allocation,
/// keeping the in-progress access unit bounded (invariant #5).
pub const MAX_ACCESS_UNIT_BYTES: usize = 8 * 1024 * 1024;

/// One decrypted RTP packet handed from a [`MediaEngine`] to the depacketizer.
///
/// This is the seam between the application-layer ICE/DTLS/SRTP engine and the
/// pure depacketize -> frame logic: the engine decrypts the SRTP, parses the RTP
/// header, and hands the fields here. `multiview-input` never sees the wire bytes
/// nor the crypto — only this typed, post-decrypt payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpFrame {
    /// The negotiated RTP payload type (matches the answer's chosen PT).
    pub payload_type: u8,
    /// The 16-bit RTP sequence number (reorder / loss detection).
    pub sequence: u16,
    /// The 32-bit RTP media timestamp (90 kHz for video).
    pub timestamp: u32,
    /// The RTP marker bit: for H.264/RFC 6184 it flags the **last packet of an
    /// access unit**.
    pub marker: bool,
    /// The decrypted RTP payload (the codec-specific bytes, e.g. an H.264 NAL or
    /// FU-A fragment).
    pub payload: Vec<u8>,
}

/// One reassembled video access unit emitted by the depacketizer.
///
/// `data` is the elementary-stream bytes (Annex-B-free NAL bytes for H.264);
/// the 32-bit RTP `timestamp` is surfaced verbatim (the downstream
/// [`PtsNormalizer`](crate::normalize) rebases it with [`WrapBits::Rtp32`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessUnit {
    /// The RTP timestamp of the access unit (one value per frame).
    timestamp: u32,
    /// Whether this access unit is a keyframe (an H.264 IDR slice).
    keyframe: bool,
    /// Whether a sequence gap (lost packet) was observed while assembling it.
    discontinuity: bool,
    /// The reassembled elementary-stream bytes.
    data: Vec<u8>,
}

/// A keyframe-gated frame emitted by [`H264Depacketizer::push`].
///
/// Carries the reassembled **compressed** elementary-stream bytes plus the
/// metadata the ingest pipeline needs: the verbatim RTP timestamp as a
/// producer-timebase raw PTS, the keyframe flag (the gate keys on it), and a
/// discontinuity flag set when a sequence gap was observed. The
/// [`RtpRouter`](crate::webrtc::route::RtpRouter) maps this into a typed
/// [`MediaUnit`](crate::webrtc::route::MediaUnit) for the application-layer
/// decoder (ADR-T014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepacketizedFrame {
    /// The reassembled compressed NAL/access-unit bytes (codec bitstream,
    /// never pixels — decode happens at the application layer).
    pub data: Vec<u8>,
    /// The 32-bit RTP timestamp surfaced as a producer-timebase raw PTS
    /// ([`WrapBits::Rtp32`]). Always `Some` for WebRTC (every RTP packet is
    /// timestamped); typed `Option` to match [`ProducedFrame::raw_pts`].
    pub raw_pts: Option<i64>,
    /// Whether this access unit is a keyframe (an H.264 IDR slice).
    pub keyframe: bool,
    /// Whether a sequence gap (lost packet) was observed while assembling it; the
    /// pump re-anchors the normalizer on a discontinuity.
    pub discontinuity: bool,
}

/// The application-layer media engine seam.
///
/// A concrete implementation owns the ICE agent, DTLS handshake, and SRTP
/// contexts (e.g. a sans-IO `str0m` driver wired at the binary level). It is
/// driven cooperatively: each [`poll_rtp`](MediaEngine::poll_rtp) returns the next
/// decrypted RTP packet, `Ok(None)` at clean end-of-stream, or an error the
/// supervisor reacts to. It must **never block the caller** waiting on the
/// network — a source with nothing ready returns `Ok(None)` and is held
/// (invariants #1 / #10).
pub trait MediaEngine {
    /// Pull the next decrypted RTP packet.
    ///
    /// Returns `Ok(Some(frame))` for a packet, `Ok(None)` at clean end-of-stream
    /// (or when nothing is currently ready), or an error for a fault the
    /// supervisor should react to (reconnect).
    ///
    /// # Errors
    ///
    /// An [`Error`](crate::Error) when the engine faults (ICE/DTLS failure, a fatal
    /// SRTP error). The caller treats this as a connection fault and applies the
    /// supervised-reconnect backoff rather than crashing the engine.
    fn poll_rtp(&mut self) -> Result<Option<RtpFrame>>;
}

/// Tracks the RTP sequence-number watermark for forward-gap (loss) detection.
///
/// Both the H.264 and the Opus depacketizers ride this: [`SequenceTracker::note`]
/// returns `true` when the packet's sequence is *ahead* of the watermark by
/// more than one — at least one packet was lost — using the same RFC 1982
/// serial-number comparison ([`crate::st2110::rtp::seq_after`]) as the other
/// RTP ingests. A stale reordered packet is **not** a forward gap and does not
/// move the watermark backwards, so a later in-order packet still detects its
/// own gap correctly.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequenceTracker {
    /// The newest sequence number accepted (the watermark).
    last: Option<u16>,
}

impl SequenceTracker {
    /// A tracker with no sequence observed yet (the first packet never gaps).
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Note a packet's sequence number, returning `true` if a forward gap
    /// (lost packet) was detected relative to the watermark.
    pub fn note(&mut self, sequence: u16) -> bool {
        let gap = match self.last {
            Some(prev) => {
                sequence != prev.wrapping_add(1) && crate::st2110::rtp::seq_after(prev, sequence)
            }
            None => false,
        };
        // Track the newest sequence (ignore a stale reordered packet for the
        // watermark so a later in-order packet still detects its own gap).
        let stale = self
            .last
            .is_some_and(|prev| !crate::st2110::rtp::seq_after(prev, sequence));
        if !stale {
            self.last = Some(sequence);
        }
        gap
    }
}

/// The H.264 RTP depacketizer (RFC 6184): a pure state machine over injected
/// packets that reassembles single-NAL / STAP-A / FU-A payloads into
/// keyframe-gated [`AccessUnit`]s.
///
/// It is **keyframe-gated**: until the first IDR access unit is seen, delta
/// frames are dropped (decoding them without a reference produces corruption).
/// Reassembly of an FU-A fragmented NAL closes on the fragment with the End bit
/// (or the RTP marker). The in-progress fragment buffer is **bounded**
/// ([`MAX_ACCESS_UNIT_BYTES`]); an over-long fragmentation is dropped rather than
/// grown. It never reads a socket and never blocks (invariants #1 / #2 / #5).
#[derive(Debug)]
pub struct H264Depacketizer {
    /// Whether the keyframe gate has been opened by a first IDR.
    gate_open: bool,
    /// The in-progress FU-A reassembly: the reconstructed NAL header plus the
    /// accumulated fragment payloads, and the timestamp/keyframe of the unit.
    fragment: Option<FuAssembly>,
    /// The sequence watermark for forward-gap (loss) detection.
    sequence: SequenceTracker,
}

/// The in-progress FU-A reassembly.
#[derive(Debug)]
struct FuAssembly {
    timestamp: u32,
    /// The reconstructed NAL bytes so far (header + fragment data).
    data: Vec<u8>,
    keyframe: bool,
}

/// H.264 NAL unit types this depacketizer keys on (RFC 6184 §5.2 / 5.4 / 5.8).
const NAL_TYPE_IDR: u8 = 5;
const NAL_TYPE_STAP_A: u8 = 24;
const NAL_TYPE_FU_A: u8 = 28;

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Depacketizer {
    /// Construct a depacketizer with the keyframe gate closed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gate_open: false,
            fragment: None,
            sequence: SequenceTracker::new(),
        }
    }

    /// Whether the keyframe gate has opened (a first IDR has been seen).
    #[must_use]
    pub const fn gate_open(&self) -> bool {
        self.gate_open
    }

    /// Push one decrypted RTP packet, returning a [`DepacketizedFrame`] when a
    /// complete access unit is reassembled and the keyframe gate is (or becomes)
    /// open.
    ///
    /// Returns `None` when the packet only advances an in-progress FU-A
    /// reassembly, or when a delta access unit is dropped because no keyframe has
    /// been seen yet. Never blocks and never panics: a malformed/empty payload is
    /// dropped rather than indexed out of bounds.
    pub fn push(&mut self, packet: &RtpFrame) -> Option<DepacketizedFrame> {
        let unit = self.reassemble(packet)?;
        self.gate_and_emit(unit)
    }

    /// Reassemble `packet` into a complete [`AccessUnit`], or `None` if it only
    /// advances an FU-A reassembly (or carries no usable NAL).
    fn reassemble(&mut self, packet: &RtpFrame) -> Option<AccessUnit> {
        let discontinuity = self.note_sequence(packet.sequence);
        let &first = packet.payload.first()?;
        let nal_type = first & 0x1F;
        match nal_type {
            NAL_TYPE_FU_A => self.reassemble_fu_a(packet, discontinuity),
            NAL_TYPE_STAP_A => {
                // A STAP-A aggregates several whole NALs in one packet; we emit it
                // as one access unit (it is always a complete, non-fragmented
                // unit). Keyframe iff any aggregated NAL is an IDR.
                self.drop_fragment();
                let keyframe = stap_a_has_idr(&packet.payload);
                Some(AccessUnit {
                    timestamp: packet.timestamp,
                    keyframe,
                    discontinuity,
                    data: packet.payload.clone(),
                })
            }
            // A single NAL unit packet (types 1..=23): the whole payload is the
            // NAL. Complete in one packet.
            1..=23 => {
                self.drop_fragment();
                Some(AccessUnit {
                    timestamp: packet.timestamp,
                    keyframe: nal_type == NAL_TYPE_IDR,
                    discontinuity,
                    data: packet.payload.clone(),
                })
            }
            // Other types (STAP-B/MTAP/FU-B, 0, 30, 31) are not handled — drop.
            _ => {
                self.drop_fragment();
                None
            }
        }
    }

    /// Advance an FU-A reassembly, returning the completed access unit on the End
    /// fragment (or RTP marker).
    fn reassemble_fu_a(&mut self, packet: &RtpFrame, discontinuity: bool) -> Option<AccessUnit> {
        // FU-A: [FU indicator][FU header][fragment...]. The FU header bits are
        // S(0x80) | E(0x40) | R(0x20) | original nal_unit_type (low 5 bits). The
        // FU indicator carries the original F|NRI bits in its top 3.
        let indicator = *packet.payload.first()?;
        let fu_header = *packet.payload.get(1)?;
        let start = (fu_header & 0x80) != 0;
        let end = (fu_header & 0x40) != 0 || packet.marker;
        let original_type = fu_header & 0x1F;
        let fragment_data = packet.payload.get(2..)?;

        if start {
            // Reconstruct the original NAL header: F|NRI from the indicator's top
            // bits, type from the FU header's low bits.
            let nal_header = (indicator & 0xE0) | original_type;
            let mut data = Vec::with_capacity(fragment_data.len().saturating_add(1));
            data.push(nal_header);
            data.extend_from_slice(fragment_data);
            self.fragment = Some(FuAssembly {
                timestamp: packet.timestamp,
                data,
                keyframe: original_type == NAL_TYPE_IDR,
            });
        } else {
            // Continuation/end fragment. A continuation with no in-progress start
            // (a lost start fragment) drops via `?`.
            let asm = self.fragment.as_mut()?;
            // A timestamp mismatch means the start was lost — abandon this
            // reassembly rather than splice across frames.
            if asm.timestamp != packet.timestamp {
                self.drop_fragment();
                return None;
            }
            if asm.data.len().saturating_add(fragment_data.len()) > MAX_ACCESS_UNIT_BYTES {
                // Over-long fragmentation: drop, never grow (invariant #5).
                self.drop_fragment();
                return None;
            }
            asm.data.extend_from_slice(fragment_data);
        }

        if end {
            let asm = self.fragment.take()?;
            Some(AccessUnit {
                timestamp: asm.timestamp,
                keyframe: asm.keyframe,
                discontinuity,
                data: asm.data,
            })
        } else {
            None
        }
    }

    /// Apply the keyframe gate to a complete access unit and, if admitted, map it
    /// to a [`DepacketizedFrame`].
    fn gate_and_emit(&mut self, unit: AccessUnit) -> Option<DepacketizedFrame> {
        if unit.keyframe {
            self.gate_open = true;
        }
        if !self.gate_open {
            // A delta access unit before any keyframe: sampled and dropped — never
            // stalls (invariants #1 / #2).
            return None;
        }
        Some(DepacketizedFrame {
            data: unit.data,
            raw_pts: Some(i64::from(unit.timestamp)),
            keyframe: unit.keyframe,
            discontinuity: unit.discontinuity,
        })
    }

    /// Note a packet's sequence number, returning `true` if a forward gap (lost
    /// packet) was detected relative to the last accepted sequence.
    fn note_sequence(&mut self, sequence: u16) -> bool {
        self.sequence.note(sequence)
    }

    /// Abandon any in-progress FU-A reassembly.
    fn drop_fragment(&mut self) {
        self.fragment = None;
    }
}

/// Whether a STAP-A aggregation packet contains an IDR NAL.
///
/// STAP-A layout (RFC 6184 §5.7.1): `[STAP-A NAL hdr][len][NAL][len][NAL]...`,
/// each `len` a 16-bit big-endian size. Walks the aggregated units checking each
/// NAL's type; bounded by the payload length, never indexes out of bounds.
fn stap_a_has_idr(payload: &[u8]) -> bool {
    // Skip the one-byte STAP-A header.
    let Some(mut rest) = payload.get(1..) else {
        return false;
    };
    while rest.len() >= 2 {
        let hi = usize::from(*rest.first().unwrap_or(&0));
        let lo = usize::from(*rest.get(1).unwrap_or(&0));
        let nal_len = (hi << 8) | lo;
        let Some(after_len) = rest.get(2..) else {
            break;
        };
        let Some(nal) = after_len.get(..nal_len) else {
            break;
        };
        if nal.first().is_some_and(|b| (b & 0x1F) == NAL_TYPE_IDR) {
            return true;
        }
        match after_len.get(nal_len..) {
            Some(next) => rest = next,
            None => break,
        }
    }
    false
}

/// An honest compressed **media-event producer** over a [`MediaEngine`]: pulls
/// decrypted RTP, routes it by negotiated payload type
/// ([`RtpRouter`](crate::webrtc::route::RtpRouter)), and yields typed
/// [`MediaEvent`]s — keyframe-gated H.264 video access units and Opus audio
/// frames — for the application layer to decode.
///
/// This seam is deliberately **compressed-only** (ADR-T014): the bytes it
/// yields are codec bitstream — never pixels — and it declares no geometry.
/// Frame geometry comes from the decoder's SPS and color from the H.264 VUI at
/// the application layer (the `multiview-ffmpeg` packet decoders), so a
/// publisher that changes resolution mid-session simply yields new metadata
/// downstream. It does **non-blocking pulls only** from the engine and never
/// paces the output clock; the depacketizers' bounded buffers drop, never grow
/// (invariants #1 / #2 / #5).
///
/// ## Timing (invariant #3)
///
/// Every [`MediaUnit`](crate::webrtc::route::MediaUnit) surfaces its 32-bit
/// RTP timestamp **verbatim** as `raw_pts`: video units tick the 90 kHz clock
/// ([`VIDEO_CLOCK_RATE`]), audio units the 48 kHz Opus clock
/// ([`AUDIO_CLOCK_RATE`](crate::webrtc::opus::AUDIO_CLOCK_RATE)) — the
/// per-unit [`timebase`](crate::webrtc::route::MediaUnit::timebase) carries
/// the distinction. **Both** clocks are 32-bit RTP clocks, so downstream
/// normalizers unwrap either with [`WebRtcProducer::WRAP_BITS`]
/// (= [`WrapBits::Rtp32`]): the video path through the
/// [`PtsNormalizer`](crate::normalize::PtsNormalizer) (ADR-T003), the audio
/// path through the shared RTP-audio rebase seam (ADR-T013).
pub struct WebRtcProducer {
    engine: Box<dyn MediaEngine + Send>,
    router: RtpRouter,
}

impl WebRtcProducer {
    /// The timestamp wrap width of **both** negotiated RTP clocks — the 90 kHz
    /// video clock and the 48 kHz audio clock are each 32-bit
    /// ([`WrapBits::Rtp32`]). Hand this to the
    /// [`PtsNormalizer`](crate::normalize::PtsNormalizer) (or the ADR-T013
    /// audio rebase) for either stream.
    pub const WRAP_BITS: WrapBits = WrapBits::Rtp32;

    /// Build a producer around an application-supplied [`MediaEngine`],
    /// routing by the session's negotiated payload types (H.264 video + Opus
    /// audio — the only codecs Multiview answers; any other payload type is
    /// counted and dropped, never an error).
    #[must_use]
    pub fn new(engine: Box<dyn MediaEngine + Send>, negotiated: &NegotiatedSession) -> Self {
        Self {
            engine,
            router: RtpRouter::new(negotiated),
        }
    }

    /// Pull the next typed media event.
    ///
    /// Pulls decrypted RTP from the engine until a complete, gate-admitted
    /// unit emerges, returning `Ok(None)` when the engine has nothing ready
    /// (or signals clean end-of-stream) — the caller re-polls later; this
    /// never blocks and never spins on a silent engine (invariants #1 / #10).
    ///
    /// # Errors
    ///
    /// Propagates an engine fault ([`MediaEngine::poll_rtp`]); the supervisor
    /// treats it as a connection fault and reacts, rather than crashing.
    pub fn next_event(&mut self) -> Result<Option<MediaEvent>> {
        loop {
            let Some(packet) = self.engine.poll_rtp()? else {
                return Ok(None);
            };
            if let Some(event) = self.router.route(&packet) {
                return Ok(Some(event));
            }
        }
    }

    /// The payload-type router: exposes the video keyframe-gate state and the
    /// unknown-payload-type drop counter (telemetry).
    #[must_use]
    pub fn router(&self) -> &RtpRouter {
        &self.router
    }
}

impl core::fmt::Debug for WebRtcProducer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WebRtcProducer")
            .field("router", &self.router)
            .finish_non_exhaustive()
    }
}
