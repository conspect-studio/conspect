//! The AES67 / ST 2110-30 **sender core** (ADR-0033 §1/§7): a bounded
//! drop-oldest program-audio sink that packetizes the mixed program bus to
//! continuous marker=0 RTP.
//!
//! The engine's off-hot-path bake consumer pushes baked program
//! [`AudioBlock`]s in via [`Aes67Sender::push`] (a bounded, drop-oldest copy
//! that never blocks — invariant #10); an independent send timer drains one
//! packet-time's worth of samples per tick via [`Aes67Sender::next_packet`],
//! silence-filling any underrun so the stream is **continuous** (one valid
//! marker=0 packet per tick forever — invariant #1). The two run on independent
//! clocks and the sender can never back-pressure or re-pace the engine.
//!
//! Timestamps advance by **sample-groups** (`+frames_per_packet` per packet,
//! e.g. `+48` at 1 ms / 48 kHz), never by bytes or channels; the sequence
//! advances `+1` per packet; the SSRC is a constant chosen at construction.
//! (The PTP-anchored *absolute* send timestamp — ADR-0033 §4 — is a flagged
//! hardware follow-on; this software sender uses a free-running counter, which
//! preserves the continuous cadence and never paces the engine.)

use std::sync::{Arc, Mutex};

use multiview_audio::AudioBlock;

use crate::display::audio::AudioFifo;

use super::packet::{build_rtp_header, encode_pcm_append, PcmDepth, RTP_FIXED_HEADER_LEN};

/// Default send FIFO depth in frames: 100 ms at 48 kHz. Bounds the decoupling
/// buffer between the engine bake and the send timer; on overflow the oldest
/// frames are dropped so a slow send timer never grows memory (invariant #10).
pub const DEFAULT_SEND_CAPACITY_FRAMES: usize = 4_800;

/// The maximum channel count an [`Aes67Sender`] accepts. Generous for AES67 /
/// ST 2110-30 (real streams carry 1–8 channels; this covers high-channel
/// professional layouts) while rejecting an absurd count that would balloon the
/// per-packet payload and FIFO allocation.
pub const MAX_CHANNELS: usize = 64;

/// The maximum `frames_per_packet` (ptime in sample groups) an [`Aes67Sender`]
/// accepts — 100 ms at 48 kHz, far above any real AES67 ptime (125 µs … 4 ms).
/// It bounds the RTP timestamp increment well inside `u32` and, with
/// [`MAX_PACKET_PAYLOAD_BYTES`], keeps the per-packet payload sane; the payload
/// cap is the binding guard for realistic channel counts.
pub const MAX_FRAMES_PER_PACKET: usize = 4_800;

/// The maximum send-FIFO capacity in frames an [`Aes67Sender`] accepts — 1 s at
/// 48 kHz. Bounds the up-front FIFO allocation (`capacity × channels` floats) so
/// an extreme config cannot request a multi-gigabyte buffer.
pub const MAX_CAPACITY_FRAMES: usize = 48_000;

/// The maximum RTP payload (PCM bytes, excluding the 12-byte header) an
/// [`Aes67Sender`] will emit per packet: one standard 1500-byte MTU minus the
/// IPv6 + UDP + RTP headers (40 + 8 + 12). AES67 audio packets ride a single
/// datagram and must **not** IP-fragment for real-time delivery, so a
/// `frames_per_packet × channels × depth` product above this is rejected at
/// construction (fail-closed) rather than sent oversized.
pub const MAX_PACKET_PAYLOAD_BYTES: usize = 1_440;

/// The lowest audio sample rate an [`Aes67Sender`] accepts (Hz). AES67 / ST
/// 2110-30 run at 44.1/48/96 kHz; this floor rejects `0` (which would make the
/// derived packet cadence a division by zero) and other sub-audio nonsense.
pub const MIN_SAMPLE_RATE_HZ: u32 = 8_000;

/// The highest audio sample rate an [`Aes67Sender`] accepts (Hz) — above any real
/// AES67 / ST 2110-30 rate; a larger value is rejected fail-closed rather than
/// used to derive a nonsensical send cadence.
pub const MAX_SAMPLE_RATE_HZ: u32 = 192_000;

/// The maximum RTP payload type an [`Aes67Sender`] accepts: the field is 7 bits
/// (RFC 3550), so a value above this would be truncated with `& 0x7f` and diverge
/// from the advertised SDP. AES67 uses dynamic types in `96..=127`.
pub const MAX_RTP_PAYLOAD_TYPE: u8 = 127;

/// Why an [`Aes67Sender`] could not be constructed from the requested
/// configuration (panel F5): a bound was exceeded, so construction fails closed
/// rather than clamping, over-allocating, or emitting an oversized packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Aes67ConfigError {
    /// The channel count was `0` or above [`MAX_CHANNELS`].
    #[error("aes67 sender channel count {got} is outside 1..={max}")]
    Channels {
        /// The requested channel count.
        got: usize,
        /// The enforced maximum ([`MAX_CHANNELS`]).
        max: usize,
    },
    /// The sample rate was outside
    /// [`MIN_SAMPLE_RATE_HZ`]`..=`[`MAX_SAMPLE_RATE_HZ`] — it defines the RTP media
    /// clock and the derived send cadence, so an unusable rate is rejected rather
    /// than used to compute a nonsensical (or division-by-zero) packet duration.
    #[error("aes67 sender sample rate {got} Hz is outside {min}..={max}")]
    SampleRate {
        /// The requested sample rate in Hz.
        got: u32,
        /// The enforced minimum ([`MIN_SAMPLE_RATE_HZ`]).
        min: u32,
        /// The enforced maximum ([`MAX_SAMPLE_RATE_HZ`]).
        max: u32,
    },
    /// The RTP payload type was above [`MAX_RTP_PAYLOAD_TYPE`] (the 7-bit field
    /// maximum). A larger value would be silently truncated with `& 0x7f` and the
    /// transmitted PT would diverge from the advertised SDP, so it is rejected.
    #[error("aes67 sender payload type {got} exceeds the 7-bit maximum {max}")]
    PayloadType {
        /// The requested payload type.
        got: u8,
        /// The enforced maximum ([`MAX_RTP_PAYLOAD_TYPE`]).
        max: u8,
    },
    /// `frames_per_packet` (the ptime in sample groups) was `0` or above
    /// [`MAX_FRAMES_PER_PACKET`].
    #[error("aes67 sender frames-per-packet {got} is outside 1..={max}")]
    FramesPerPacket {
        /// The requested frames-per-packet.
        got: usize,
        /// The enforced maximum ([`MAX_FRAMES_PER_PACKET`]).
        max: usize,
    },
    /// The requested send-FIFO capacity exceeded [`MAX_CAPACITY_FRAMES`].
    #[error("aes67 sender capacity {got} frames exceeds the {max}-frame cap")]
    Capacity {
        /// The requested capacity in frames.
        got: usize,
        /// The enforced maximum ([`MAX_CAPACITY_FRAMES`]).
        max: usize,
    },
    /// The `frames_per_packet × channels × depth` product would exceed
    /// [`MAX_PACKET_PAYLOAD_BYTES`] — an oversized UDP datagram that would
    /// IP-fragment.
    #[error(
        "aes67 sender packet payload {got} bytes exceeds the {max}-byte cap \
         (oversized UDP datagram; would IP-fragment)"
    )]
    PayloadTooLarge {
        /// The per-packet payload the configuration would produce.
        got: usize,
        /// The enforced maximum ([`MAX_PACKET_PAYLOAD_BYTES`]).
        max: usize,
    },
}

/// The bounded drop-oldest send FIFO shared by an [`Aes67Sender`] (the serve
/// side) and its [`Aes67SenderHandle`] (the producer side).
///
/// Both halves reach it via [`try_lock`](Mutex::try_lock), never a blocking
/// `lock`: the lock is held only for the O(samples) in-memory push/drain (never
/// across a socket call), so genuine contention is nanosecond-scale, and on
/// contention the bake push sheds while the serve drain silence-fills — neither
/// half can ever block the other (panel F3, inv #1 / #10).
type SharedFifo = Arc<Mutex<AudioFifo>>;

/// The **producer half** of an [`Aes67Sender`]: a cheap, cloneable handle the
/// engine bake consumer holds to feed baked program blocks into the shared send
/// FIFO with [`push`](Self::push).
///
/// It shares the FIFO with the serve-side [`Aes67Sender`] through an `Arc`, so
/// the bake push (`&self`) and the send/serve loop (`&mut Aes67Sender`) run
/// concurrently on their independent clocks with **no `&mut` contention** — the
/// handoff primitive the pipeline wires bake→sender with (panel F3). `push`
/// never blocks the engine (inv #10).
#[derive(Debug, Clone)]
pub struct Aes67SenderHandle {
    fifo: SharedFifo,
    channels: usize,
}

impl Aes67SenderHandle {
    /// Push one baked program block into the shared send FIFO — a bounded,
    /// drop-oldest copy that never blocks the engine bake (inv #10).
    ///
    /// A block whose channel count does not match the sender's is dropped rather
    /// than mis-interleaved (the pipeline constructs the sender with the program
    /// bus's channel count, so this is a defensive guard, not the steady path).
    /// On the rare lock contention the block is shed rather than waited on, so
    /// the bake side is never back-pressured by the send loop.
    pub fn push(&self, block: &AudioBlock) {
        if block.format().channel_count() != self.channels {
            return;
        }
        if let Ok(mut fifo) = self.fifo.try_lock() {
            fifo.push(block.interleaved());
        }
    }

    /// Current FIFO fill in frames (per channel). `0` on the rare lock
    /// contention (the caller re-reads); telemetry, never blocks.
    #[must_use]
    pub fn fill_frames(&self) -> usize {
        self.fifo.try_lock().map_or(0, |fifo| fifo.fill_frames())
    }

    /// Total frames dropped to drop-oldest overflow since construction. `0` on
    /// the rare lock contention; telemetry, never blocks.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.fifo.try_lock().map_or(0, |fifo| fifo.dropped_frames())
    }
}

/// A bounded drop-oldest AES67 / ST 2110-30 program-audio sender (the **serve
/// half**).
///
/// Construct with [`Aes67Sender::new`], take a producer [`handle`](Aes67Sender::handle)
/// for the engine bake side to feed baked program blocks through, and drain one
/// continuous RTP packet per packet-time tick with
/// [`next_packet`](Aes67Sender::next_packet). The handle and this serve half
/// share the send FIFO via an `Arc`, so the bake push and the send loop run
/// concurrently without `&mut` contention (panel F3).
#[derive(Debug)]
pub struct Aes67Sender {
    fifo: SharedFifo,
    channels: usize,
    depth: PcmDepth,
    payload_type: u8,
    ssrc: u32,
    /// The RTP media clock rate in Hz (the audio sample rate). The send cadence is
    /// derived from this and `frames_per_packet`, so the wall-clock packet
    /// interval always matches the advertised clock (panel F1).
    sample_rate: u32,
    /// Audio frames (samples per channel) emitted per packet (`ptime` worth).
    frames_per_packet: usize,
    /// The RTP timestamp increment per packet (`= frames_per_packet`, precomputed
    /// as `u32` so the hot path never casts).
    timestamp_increment: u32,
    sequence: u16,
    timestamp: u32,
    /// Reusable drain buffer (rule 22: no per-packet interleaved allocation).
    scratch: Vec<f32>,
}

impl Aes67Sender {
    /// Build a sender for `channels`-channel interleaved audio at `depth` and
    /// `sample_rate` Hz, emitting `frames_per_packet` frames per packet on RTP
    /// payload type `payload_type` with the constant `ssrc`, buffering up to
    /// `capacity_frames` frames (raised to at least one packet). The `sample_rate`
    /// is the RTP media clock: the send cadence
    /// ([`packet_duration`](Self::packet_duration)) is derived from it and
    /// `frames_per_packet`, so the wire interval always matches the advertised
    /// clock (panel F1).
    ///
    /// **Fallible / fail-closed (panel F5):** the configuration is validated
    /// against [`MAX_CHANNELS`], [`MIN_SAMPLE_RATE_HZ`]`..=`[`MAX_SAMPLE_RATE_HZ`],
    /// `1..=`[`MAX_FRAMES_PER_PACKET`], [`MAX_CAPACITY_FRAMES`], and the
    /// [`MAX_PACKET_PAYLOAD_BYTES`] single-MTU bound. An out-of-range value is
    /// **rejected**, never clamped or allowed to over-allocate / emit an oversized
    /// UDP packet / overflow the RTP timestamp increment / derive a nonsensical
    /// send cadence.
    ///
    /// # Errors
    ///
    /// [`Aes67ConfigError`] when the channel count, sample rate, ptime, capacity,
    /// or resulting per-packet payload size is outside the supported bounds.
    pub fn new(
        channels: usize,
        depth: PcmDepth,
        payload_type: u8,
        ssrc: u32,
        sample_rate: u32,
        frames_per_packet: usize,
        capacity_frames: usize,
    ) -> Result<Self, Aes67ConfigError> {
        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(Aes67ConfigError::Channels {
                got: channels,
                max: MAX_CHANNELS,
            });
        }
        if !(MIN_SAMPLE_RATE_HZ..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate) {
            return Err(Aes67ConfigError::SampleRate {
                got: sample_rate,
                min: MIN_SAMPLE_RATE_HZ,
                max: MAX_SAMPLE_RATE_HZ,
            });
        }
        if payload_type > MAX_RTP_PAYLOAD_TYPE {
            return Err(Aes67ConfigError::PayloadType {
                got: payload_type,
                max: MAX_RTP_PAYLOAD_TYPE,
            });
        }
        if !(1..=MAX_FRAMES_PER_PACKET).contains(&frames_per_packet) {
            return Err(Aes67ConfigError::FramesPerPacket {
                got: frames_per_packet,
                max: MAX_FRAMES_PER_PACKET,
            });
        }
        if capacity_frames > MAX_CAPACITY_FRAMES {
            return Err(Aes67ConfigError::Capacity {
                got: capacity_frames,
                max: MAX_CAPACITY_FRAMES,
            });
        }
        let payload_bytes = frames_per_packet
            .saturating_mul(channels)
            .saturating_mul(depth.bytes_per_sample());
        if payload_bytes > MAX_PACKET_PAYLOAD_BYTES {
            return Err(Aes67ConfigError::PayloadTooLarge {
                got: payload_bytes,
                max: MAX_PACKET_PAYLOAD_BYTES,
            });
        }
        let capacity_frames = capacity_frames.max(frames_per_packet);
        Ok(Self {
            fifo: Arc::new(Mutex::new(AudioFifo::new(capacity_frames, channels))),
            channels,
            depth,
            payload_type,
            ssrc,
            sample_rate,
            frames_per_packet,
            // `frames_per_packet <= MAX_FRAMES_PER_PACKET` (well inside `u32`),
            // so this widening never saturates — the defensive fallback is dead.
            timestamp_increment: u32::try_from(frames_per_packet).unwrap_or(u32::MAX),
            sequence: 0,
            timestamp: 0,
            scratch: Vec::new(),
        })
    }

    /// A cheap, cloneable producer [`handle`](Aes67SenderHandle) sharing this
    /// sender's send FIFO.
    ///
    /// The engine bake consumer holds the handle and calls
    /// [`push`](Aes67SenderHandle::push) (`&self`); the send task holds this
    /// `Aes67Sender` and drains via [`next_packet`](Self::next_packet)
    /// (`&mut self`). The two run concurrently on their independent clocks with
    /// no `&mut` contention — this is the handoff primitive the pipeline wires
    /// bake→sender with (panel F3, inv #10). Multiple handles may be taken; each
    /// clones the shared-FIFO `Arc`.
    #[must_use]
    pub fn handle(&self) -> Aes67SenderHandle {
        Aes67SenderHandle {
            fifo: Arc::clone(&self.fifo),
            channels: self.channels,
        }
    }

    /// Build the next continuous RTP packet: header + one packet-time of PCM.
    ///
    /// Drains `frames_per_packet` frames of real audio, silence-filling any
    /// underrun so the stream never gaps or stalls (invariant #1), encodes them
    /// to big-endian L16/L24, prepends the 12-byte RTP header (marker=0), and
    /// advances the sequence (`+1`) and timestamp (`+frames_per_packet`). Always
    /// returns a full packet, whatever the FIFO fill — the RTP media clock is
    /// driven by the send timer, never by the input feed.
    pub fn next_packet(&mut self) -> Vec<u8> {
        let mut packet = Vec::new();
        self.next_packet_into(&mut packet);
        packet
    }

    /// Drain the next continuous RTP packet into a caller-owned, **reused** buffer
    /// (rule 22: no per-packet allocation on the continuous send path). Identical
    /// bytes to [`next_packet`](Self::next_packet), but written into `out` in place
    /// so the send loop transmits from one buffer warmed once and reused forever
    /// — [`next_packet`](Self::next_packet) is just this over a fresh `Vec`.
    ///
    /// Drains `frames_per_packet` frames of real audio, silence-filling any
    /// underrun so the stream never gaps or stalls (invariant #1), encodes them to
    /// big-endian L16/L24, prepends the 12-byte RTP header (marker=0), and advances
    /// the sequence (`+1`) and timestamp (`+frames_per_packet`). Always writes a
    /// full packet, whatever the FIFO fill — the RTP media clock is driven by the
    /// send timer, never by the input feed.
    pub fn next_packet_into(&mut self, out: &mut Vec<u8>) {
        let want_samples = self.frames_per_packet.saturating_mul(self.channels);
        // Silence baseline first, so a skipped drain (lock contention) emits
        // silence, never stale samples from the previous packet.
        self.scratch.clear();
        self.scratch.resize(want_samples, 0.0);
        // Drain via `try_lock`, never a blocking `lock`: the send loop must never
        // wait on the engine bake side holding the shared FIFO (inv #10). On
        // contention the drain is skipped and this tick emits silence — still one
        // full, continuous marker=0 packet (inv #1). `pop_into` overwrites every
        // slot it can with real samples, leaving the rest silence.
        if let Ok(mut fifo) = self.fifo.try_lock() {
            let _real = fifo.pop_into(&mut self.scratch);
        }

        let header = build_rtp_header(self.payload_type, self.sequence, self.timestamp, self.ssrc);
        // Reuse the caller's buffer in place: clear (keeps the allocation), reserve
        // the exact packet size once, then write header + PCM — so the steady send
        // path never reallocates per packet (rule 22).
        let payload_bytes = want_samples.saturating_mul(self.depth.bytes_per_sample());
        out.clear();
        out.reserve(RTP_FIXED_HEADER_LEN.saturating_add(payload_bytes));
        out.extend_from_slice(&header);
        encode_pcm_append(out, &self.scratch, self.depth);

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.timestamp_increment);
    }

    /// Current FIFO fill in frames (per channel). `0` on the rare lock
    /// contention; telemetry, never blocks the serve loop.
    #[must_use]
    pub fn fill_frames(&self) -> usize {
        self.fifo.try_lock().map_or(0, |fifo| fifo.fill_frames())
    }

    /// Total frames dropped to drop-oldest overflow since construction
    /// (telemetry / test observability). `0` on the rare lock contention.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.fifo.try_lock().map_or(0, |fifo| fifo.dropped_frames())
    }

    /// The next RTP sequence number to be emitted.
    #[must_use]
    pub const fn sequence(&self) -> u16 {
        self.sequence
    }

    /// The next RTP timestamp to be emitted.
    #[must_use]
    pub const fn timestamp(&self) -> u32 {
        self.timestamp
    }

    /// The audio frames emitted per packet.
    #[must_use]
    pub const fn frames_per_packet(&self) -> usize {
        self.frames_per_packet
    }

    /// The RTP media clock rate in Hz this sender advertises.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The nominal packet-time (`ptime`): exactly `frames_per_packet /
    /// sample_rate`, the per-packet interval the RTP media clock advances by (panel
    /// F1). This is the *single-packet* duration, **floored** to whole nanoseconds;
    /// the send loop does **not** repeat it (that would accumulate the truncation
    /// into drift — see [`packet_deadline_offset`](Self::packet_deadline_offset)),
    /// it drives from the cumulative absolute deadline instead. Retained as the
    /// human-facing ptime and the source of the floored per-packet value.
    ///
    /// Computed in exact integer nanoseconds (never float fps — invariant #3);
    /// `sample_rate` is a validated non-zero rate, so there is no division by zero
    /// and `frames_per_packet ≤ `[`MAX_FRAMES_PER_PACKET`] keeps the product well
    /// inside `u64`.
    #[must_use]
    pub fn packet_duration(&self) -> std::time::Duration {
        let frames = u64::try_from(self.frames_per_packet).unwrap_or(u64::MAX);
        let nanos = frames.saturating_mul(1_000_000_000) / u64::from(self.sample_rate);
        std::time::Duration::from_nanos(nanos)
    }

    /// The absolute offset from the send-loop start at which packet `index`
    /// (0-based) is due — the deadline
    /// [`Aes67UdpSender::serve`](super::transport::Aes67UdpSender::serve)
    /// `sleep_until`s (panel T1).
    ///
    /// Packet `index` carries the `index × frames_per_packet`-th media frame, so it
    /// is due `index × frames_per_packet / sample_rate` seconds after the loop
    /// started. Deriving the deadline from the **cumulative** index (rather than
    /// repeating one floored [`packet_duration`](Self::packet_duration)) keeps each
    /// deadline within one nanosecond of the ideal **forever**: the single division
    /// is floored once, so the rounding never accumulates. Repeating a truncated
    /// per-packet duration instead loses the sub-nanosecond remainder every packet
    /// and drifts without bound at any rate `frames_per_packet` does not evenly
    /// divide (e.g. 48 @ 44.1 kHz) — the T1 defect (inv #1/#3, never float).
    ///
    /// Exact integer math: the `index × frames × 1e9` product is taken in `u128`
    /// (it can exceed `u64` after a few days of ticks) and the divided result — ns
    /// since start — fits `u64` for millennia; `sample_rate` is a validated
    /// non-zero rate, so there is no division by zero.
    #[must_use]
    pub fn packet_deadline_offset(&self, index: u64) -> std::time::Duration {
        // `timestamp_increment == frames_per_packet` (validated ≤ MAX_FRAMES_PER_PACKET,
        // well inside u32), so `u128::from` is exact and cast-free.
        let frames = u128::from(self.timestamp_increment);
        let nanos = u128::from(index)
            .saturating_mul(frames)
            .saturating_mul(1_000_000_000)
            / u128::from(self.sample_rate);
        std::time::Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};
    use std::sync::mpsc;
    use std::time::Duration;

    fn mono_block() -> AudioBlock {
        let fmt = AudioFormat::new(48_000, ChannelLayout::Mono);
        AudioBlock::from_interleaved(fmt, vec![0.1_f32; 48]).expect("mono block")
    }

    /// F5: construction is fallible and fail-closed — a channel count, ptime, or
    /// capacity outside the supported bounds, or a channel×ptime×depth combo that
    /// would produce an oversized (IP-fragmenting) UDP packet, is rejected rather
    /// than clamped or allowed to overflow the RTP timestamp increment.
    #[test]
    fn rejects_out_of_range_config() {
        // Zero and oversized channel count.
        assert!(matches!(
            Aes67Sender::new(0, PcmDepth::L24, 96, 1, 48_000, 48, 4_800),
            Err(Aes67ConfigError::Channels { .. })
        ));
        assert!(matches!(
            Aes67Sender::new(MAX_CHANNELS + 1, PcmDepth::L24, 96, 1, 48_000, 48, 4_800),
            Err(Aes67ConfigError::Channels { .. })
        ));
        // Zero and oversized sample rate (would derive a nonsensical / div-by-zero
        // cadence).
        assert!(matches!(
            Aes67Sender::new(2, PcmDepth::L24, 96, 1, 0, 48, 4_800),
            Err(Aes67ConfigError::SampleRate { .. })
        ));
        assert!(matches!(
            Aes67Sender::new(2, PcmDepth::L24, 96, 1, MAX_SAMPLE_RATE_HZ + 1, 48, 4_800),
            Err(Aes67ConfigError::SampleRate { .. })
        ));
        // Zero and oversized frames-per-packet (oversized ptime).
        assert!(matches!(
            Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 0, 4_800),
            Err(Aes67ConfigError::FramesPerPacket { .. })
        ));
        assert!(matches!(
            Aes67Sender::new(
                2,
                PcmDepth::L24,
                96,
                1,
                48_000,
                MAX_FRAMES_PER_PACKET + 1,
                4_800
            ),
            Err(Aes67ConfigError::FramesPerPacket { .. })
        ));
        // A channel × ptime × depth combo that would exceed one MTU (8ch × 96
        // frames × 3 bytes = 2304 > MAX_PACKET_PAYLOAD_BYTES): oversized UDP.
        assert!(matches!(
            Aes67Sender::new(8, PcmDepth::L24, 96, 1, 48_000, 96, 4_800),
            Err(Aes67ConfigError::PayloadTooLarge { .. })
        ));
        // Oversized capacity (would pre-allocate a huge FIFO).
        assert!(matches!(
            Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 48, MAX_CAPACITY_FRAMES + 1),
            Err(Aes67ConfigError::Capacity { .. })
        ));
        // A realistic config still constructs.
        assert!(Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 48, 4_800).is_ok());
    }

    /// F3: while the shared FIFO lock is held, a concurrent handle `push` (the
    /// engine bake side) must NOT block on it — it sheds the block and returns
    /// (inv #10: the engine can never be back-pressured by the send loop). A
    /// blocking lock would wait for the guard and never signal within the
    /// deadline.
    #[test]
    fn handle_push_never_blocks_on_a_held_fifo() {
        let sender =
            Aes67Sender::new(1, PcmDepth::L16, 96, 1, 48_000, 48, 480).expect("valid aes67 config");
        let handle = sender.handle();
        // Hold the shared FIFO lock on this thread (same module → private field).
        let fifo = Arc::clone(&sender.fifo);
        let guard = fifo.lock().expect("uncontended lock");
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            handle.push(&mono_block());
            let _ = tx.send(());
        });
        let received = rx.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let _ = worker.join();
        received.expect("contended handle.push must not block on the shared FIFO (inv #10)");
    }

    /// F3: while the shared FIFO lock is held, a concurrent serve `next_packet`
    /// (the send task) must NOT block — it silence-fills and returns a full
    /// packet (inv #1: one valid packet per tick, whatever the FIFO state). A
    /// blocking lock would wait for the guard.
    #[test]
    fn next_packet_never_blocks_on_a_held_fifo() {
        let mut sender =
            Aes67Sender::new(1, PcmDepth::L16, 96, 1, 48_000, 48, 480).expect("valid aes67 config");
        let fifo = Arc::clone(&sender.fifo);
        let guard = fifo.lock().expect("uncontended lock");
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(sender.next_packet());
        });
        let received = rx.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let _ = worker.join();
        let packet =
            received.expect("contended next_packet must not block on the shared FIFO (inv #1)");
        assert_eq!(
            packet.len(),
            RTP_FIXED_HEADER_LEN + 48 * 2,
            "a full mono-L16 silence packet (12-byte header + 96-byte payload)"
        );
    }
}
