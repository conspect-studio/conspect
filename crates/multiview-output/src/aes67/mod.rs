//! AES67 / SMPTE **ST 2110-30** PCM-audio RTP **egress** (ADR-0033, ADR-T010).
//!
//! The first output with **no encode/GPU stage**: it packetizes the mixed
//! program bus to raw L16/L24 PCM and sends it as continuous marker=0 RTP over
//! UDP multicast. Open-standard, **no SDK, no GPL escalation** — pure Rust under
//! the off-by-default `aes67` feature, `forbid(unsafe_code)`.
//!
//! - [`packet`] — the pure send-side wire framing (`f32` → big-endian L16/L24
//!   PCM + the 12-byte RTP header builder). Always compiled, default-tested, and
//!   round-tripped against `multiview-input`'s `V30Payload` decoder.
//! - [`sender`] — the [`Aes67Sender`]: a bounded drop-oldest program-audio sink
//!   that drains one packet-time per send tick, silence-filling underruns so the
//!   stream is continuous (invariants #1 / #10). Always compiled, default-tested.
//! - [`transport`] — the feature-gated (`aes67`) tokio UDP send loop.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Send is a bounded drop-oldest **sink** fed from the off-hot-path bake
//! consumer; the engine never `.await`s it and `send_to` never runs on the
//! output-clock loop. A slow or stalled send timer can only drop the oldest
//! buffered frames — it can never back-pressure or re-pace the program bus.
//!
//! ## Flagged hardware follow-on
//!
//! The PTP-anchored **absolute** send timestamp (ADR-0033 §4), the `rubato` ppm
//! boundary resampler (§5, identity when locked), and real Dante/AES67 interop
//! need a PTP-capable NIC with a PHC and are a flagged operator/hardware
//! follow-on; this software sender uses a free-running RTP counter, which keeps
//! the cadence continuous and never paces the engine.

pub mod packet;
pub mod sender;

#[cfg(feature = "aes67")]
pub mod transport;

pub use packet::{build_rtp_header, encode_pcm, PcmDepth, RTP_FIXED_HEADER_LEN, RTP_VERSION};
pub use sender::{
    Aes67ConfigError, Aes67Sender, Aes67SenderHandle, DEFAULT_SEND_CAPACITY_FRAMES,
    MAX_CAPACITY_FRAMES, MAX_CHANNELS, MAX_FRAMES_PER_PACKET, MAX_PACKET_PAYLOAD_BYTES,
};
