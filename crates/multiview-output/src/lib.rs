//! # multiview-output
//!
//! Output sinks/servers and packaging for the **Multiview** live video multiview
//! engine: the encode-once-mux-many fan-out model, HLS/LL-HLS playlist
//! generation, and (behind off-by-default features) the RTSP server, NDI out,
//! and RTMP/SRT push transports.
//!
//! This crate's **default build is pure Rust** with no native dependencies. The
//! two pure-Rust pillars are always available:
//!
//! - [`hls`] — HLS / LL-HLS playlist **text** generation (ADR-0007: Multiview
//!   builds the Apple Low-Latency tag layer in-house). No I/O, no `FFmpeg`.
//! - [`fanout`] — the encode-once-mux-many routing model (invariant #7): one
//!   encoded packet stream routed by reference to N transport sinks.
//! - [`tsl`] — TSL UMD (Under-Monitor Display) protocol **encoders** (v3.1 / v4.0
//!   / v5.0): pure typed label/tally messages → on-wire bytes, the egress mirror
//!   of `multiview-input::tsl`. Socket-free (a later integration) and off the engine
//!   hot path.
//! - [`rtsp`] — the OUT-1 RTSP-egress **sidecar baseline** seam (ADR-0006): the
//!   pure publish-URL builder ([`rtsp::RtspPublishTarget`]) that derives a libav
//!   RTSP ANNOUNCE/RECORD publish URL from a base + mount for the existing
//!   `PushProtocol::Rtsp` push path. No native dependency; the in-process
//!   `gst-rtsp-server` is OUT-2 (a separate feature).
//! - [`rtsp_server`] — the OUT-2 in-process RTSP **server** (ADR-0006 primary
//!   path): the always-compiled typed seam ([`rtsp_server::RtspServerSink`]
//!   [`PacketSink`](fanout::PacketSink) over a bounded drop-oldest
//!   [`rtsp_server::BoundedPacketQueue`], [`rtsp_server::RtspMount`], and
//!   [`rtsp_server::RtspCodec`] caps selection) plus, behind the off-by-default
//!   `rtsp-server` feature, the `gst-rtsp-server` serving thread that fans the
//!   already-encoded canvas to RTSP clients with no re-encode.
//!
//! The transports themselves (RTSP via `gst-rtsp-server`, the CMAF segmenter +
//! LL-HLS HTTP origin, NDI, RTMP/SRT) are feature-gated (`ffmpeg`, `ndi`, …) so
//! the GPU-free CI baseline never pulls a native library. The feature-gated
//! servers implement [`fanout::PacketSink`] and feed segment/part metadata into
//! [`hls`].
//!
//! ## Invariants this crate must uphold
//!
//! - **#7 encode-once-mux-many:** composite once, encode the canvas once per
//!   rendition, fan the *same* packets to all transports — never per-tile.
//! - **#3 timing:** packet timestamps are re-stamped from the tick counter
//!   upstream; this crate carries them as exact integers, never float fps.
//! - **#1/#10 isolation:** the fan-out and transport layer must never stall the
//!   output clock or let a slow client back-pressure the engine.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// ADR-0033 — AES67 / SMPTE ST 2110-30 PCM-audio RTP egress: the first output
/// with no encode/GPU stage. The pure send framing (`f32` → big-endian L16/L24
/// + the RTP header builder) and the bounded drop-oldest `Aes67Sender` are
/// always compiled and CI-tested; only `aes67::transport` (the UDP send loop,
/// feature `aes67`) touches a socket.
pub mod aes67;

/// DEV-B1 / ADR-0044 — local DRM/KMS display output: the raw-frame sink that
/// scans the pre-encode NV12 canvas out to HDMI/DP glass via atomic page
/// flips. The mailbox, mode policy (EDID/CVT-RB), flip state machine, and the
/// sink thread are pure Rust, always compiled, and CI-tested over a mock
/// device seam; only `display::kms` (the real ioctl backend, feature
/// `display-kms`) touches hardware.
pub mod display;

/// DEV-C1 / ADR-M010 — the shared **outbound presentation epoch** cell: the
/// per-program `WallClockRef` (output-PTS ns ↔ disciplined wall ns) the
/// engine-side sampler publishes and the HLS PDT / RTCP SR egress consumers
/// read. Pure Rust, always compiled.
pub mod epoch;
pub mod error;
pub mod fanout;
pub mod hls;

/// OUTMETA (ADR-0088 / ADR-0089) — the pure-Rust apply model for per-output
/// container metadata + presentation orientation: the validated muxer
/// dictionary entries ([`metadata::MuxMetadata`]), the tag-path display-rotation
/// matrix ([`metadata::display_matrix`]), and the pixels-path rotated rendition
/// geometry ([`metadata::rotated_geometry`]). Always compiled; the feature-gated
/// muxer wiring (`sink`) applies them. Reuses core `QuarterTurn` (no fourth
/// rotation enum); honors inv #8 (tag, never convert) + #7 (fans with the
/// rendition, no extra encode for the tag path).
pub mod metadata;

/// DEV-C1 / ADR-M010 — RTCP **Sender Report** building stamped from the same
/// outbound epoch: exact integer NTP/RTP field math + the 28-byte wire form +
/// the [`rtcp::SrStamper`] seam the RTSP serving layer consumes. Pure Rust,
/// always compiled.
pub mod rtcp;

/// GP-6 — the per-stream monotonic clamp+offset packet re-stamp (ADR-0030 §4
/// "Re-stamp rule (#3 for the copy path)"). The COPY-path invariant-#3
/// primitive a guarded passthrough uses to re-stamp copied input + spliced-slate
/// packets so the muxer's interleaved write never aborts on non-monotonic DTS
/// while B-frame reorder is preserved — distinct from the encoder path's
/// `out_pts = f(tick)`. Pure `i64` arithmetic; always compiled (no `ffmpeg`).
pub mod restamp;
pub mod rtsp;
pub mod rtsp_server;
pub mod tsl;

/// Real encode-once-mux-many output sinks (file + HLS segmenter), built on
/// `multiview-ffmpeg`'s safe encoder/muxer wrappers. Behind the off-by-default
/// `ffmpeg` feature so the baseline build stays pure-Rust.
#[cfg(feature = "ffmpeg")]
pub mod sink;

/// GP-4 — the one-time pre-baked slate baker (ADR-0030 §4 "Pre-bake-once
/// slate"). Encodes **once** an IDR-led, closed-GOP, B-free loop of black /
/// SMPTE-bars video (+ optional AAC tone / silence) into a shared
/// [`slate::BakedSlate`] for a guarded passthrough to splice in on input loss,
/// then releases the encoder (no held session). Behind the off-by-default
/// `ffmpeg` feature; reuses [`sink::ProgramEncoder`] (no new FFI).
#[cfg(feature = "ffmpeg")]
pub mod slate;

/// GP-7 — the guarded-passthrough splice seam (ADR-0030 §4 "The splice seam —
/// `GuardedPacketSource`"). Assembles the GP-1/4/5/6 primitives into the live
/// copy-vs-slate failover: a [`guarded::GuardedPacketSource`] that copies the
/// live input while healthy and splices the pre-baked [`slate::BakedSlate`] on
/// loss, re-stamped monotonic across both seams, recovery gated on a true
/// strict-IDR (GP-1 `is_idr`, not `is_key`). The sole producer feeding
/// [`sink::PacketMuxSink::run_av`]. Behind the off-by-default `ffmpeg` feature;
/// reuses the GP-5 watchdog (pure-Rust `multiview-framestore`) + GP-6 restamp.
#[cfg(feature = "ffmpeg")]
pub mod guarded;

/// Proprietary NDI® output (ADR-0008): the runtime-load scaffolding (locate +
/// `dlopen` the NDI runtime via `NDIlib_v6_load`), the runtime license gate, the
/// mandatory attribution constants, and the safe `NdiOutput` sink seam over the
/// resolved API table. Behind the off-by-default, license-isolating `ndi`
/// feature; the raw FFI lives in the `multiview-ndi-sys` leaf crate so this crate
/// stays `forbid(unsafe_code)`.
#[cfg(feature = "ndi")]
pub mod ndi;

/// RIST link statistics (ADR-0095 Tier-1 / RIST-5): the bridge that drives a
/// direct-librist sender egress link, drains its stats callback, and lowers each
/// sample into the telemetry metric series + a `rist.link.stats` wire event +
/// the `rist-link-loss` health warning. Behind the off-by-default `rist-stats`
/// feature; all raw librist FFI lives in the `multiview-rist-sys` leaf crate so
/// this crate stays `forbid(unsafe_code)`. `FFmpeg`'s Tier-0 `rist://` transport
/// exposes no stats — this is the only path that observes a RIST link's health.
#[cfg(feature = "rist-stats")]
pub mod rist;

pub use epoch::SharedEpoch;
pub use error::{Error, Result};
pub use metadata::{
    display_matrix, display_matrix_degrees, rotated_geometry, DisplayMatrix, MetadataEntry,
    MetadataEntryError, MetadataScope, MuxMetadata,
};
pub use restamp::RestampAccumulator;
pub use rtcp::{rtp_timestamp_at, NtpTimestamp, SenderReport, SrStamper};
pub use rtsp::{RtspPublishError, RtspPublishTarget};
pub use rtsp_server::{
    units_to_nanos, BoundedPacketQueue, RtspCapsError, RtspCodec, RtspMount, RtspMountError,
    RtspServerSink,
};

#[cfg(feature = "ffmpeg")]
pub use guarded::{
    GuardMode, GuardedConfig, GuardedPacketSource, ManualClock, MonotonicClock, RealMonotonicClock,
};
#[cfg(feature = "ffmpeg")]
pub use sink::{
    AudioEncodeConfig, EncodeConfig, EncodeStats, FileSink, MuxStream, PacketMuxOutcome,
    PacketMuxSink, PacketSource, ProgramEncoder, PushProtocol, PushSink, SegmentResult,
    SegmentSink, VideoFrameSource,
};
