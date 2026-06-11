//! # multiview-input
//!
//! Ingest sources (RTSP/HLS/TS/SRT/RTMP/NDI/file/test), the custom input pacer,
//! jitter buffers, timestamp normalization, and supervised reconnect for the
//! **Multiview** live video multiview engine.
//!
//! **Inputs are sampled, never pacing.** They feed last-good-frame stores and
//! must never block or back-pressure the engine. This crate's default build is
//! pure-Rust with no native dependencies: it owns the *timing and resilience
//! logic* that underpins invariants #3 (unified timing) and #4 (HLS ingest
//! pacing). The actual demuxers/decoders (libav, NDI) live behind the
//! off-by-default `ffmpeg` / `ndi` features and are not part of the pure-Rust
//! baseline.
//!
//! ## Modules
//!
//! * [`normalize`] — per-input PTS normalization: 33-bit/32-bit wrap unwrap,
//!   genpts fallback, monotonic guard, rebase onto the internal ns timeline, and
//!   discontinuity re-anchor (invariant #3, [ADR-T003]).
//! * [`jitter`] — a bounded reorder buffer that sorts by PTS within a window,
//!   drops late packets, and drops-oldest at capacity (never grows).
//! * [`pacer`] — the HLS / wall-clock input pacer with an injected clock
//!   (invariant #4, [ADR-T004]).
//! * [`param_probe`] — per-AU coded **parameter-set** probe + drift detection
//!   (GP-2, ADR-0030 §4): snapshot the active SPS/PPS/VPS (H.264/HEVC) or
//!   `sequence_header` OBU (AV1) from extradata or the first GOP, then report
//!   when a later access unit's in-band parameter-set bytes change — the signal a
//!   guarded passthrough uses to invalidate + re-bake its param-matched slate. A
//!   pure, libav-free byte parser reusing the GP-1 codec/framing types.
//! * [`reconnect`] — capped exponential backoff with full jitter for supervised
//!   reconnect, with an injected jitter source.
//! * [`source`] — the producer-agnostic ingest pipeline: an [`source::IngestPump`]
//!   wires decode → normalize → jitter → last-good-frame store. Pure-Rust and
//!   always built; tests drive it with a synthetic producer.
//! * [`tsl`] — TSL UMD (Under-Monitor Display) protocol **decoders** (v3.1 / v4.0
//!   / v5.0): pure byte-slice → typed label/tally messages, mapping wire tally
//!   onto [`multiview_core::tally`]. The sockets that carry them are a later
//!   integration; the codecs are socket-free and off the engine hot path.
//! * [`st2110`] — SMPTE **ST 2110** RTP depacketizers (RFC 3550 header, -20
//!   video, -30 AES67 audio, -40 ANC): pure, golden-vector + property tested.
//!   The UDP receive sockets live behind the off-by-default `st2110` feature and
//!   are compile-verified only (no NIC here).
//! * [`st2022_7`] — SMPTE **ST 2022-7** hitless dual-path reconstruction: a pure,
//!   property-tested merge of two lossy RTP sequence streams into one
//!   gap-minimized in-order stream over a bounded reorder window.
//! * [`st2022_6`] — SMPTE **ST 2022-6** HBRMT (SDI-over-IP) framing parser
//!   (pure).
//! * [`mpegts`] — MPEG-2 Transport Stream **PSI/SI** section parsers (PAT, PMT,
//!   NIT, SDT, CAT, TDT, TOT) with CRC-32/MPEG-2 validation, plus an MPTS
//!   program-selection model, typed ES-descriptor decoders (ISO-639 / subtitling
//!   / teletext / AC-3), and a PMT → [`multiview_core::stream::StreamInventory`]
//!   fold-in with SCTE-35 reconciliation (RT-2). Pure, golden-vector + property
//!   tested.
//! * [`inventory`] — the container-agnostic [`multiview_core::stream::StreamInventory`]
//!   **merge** (RT-2, ADR-0034 §3): overlay the MPEG-TS PMT's language/role +
//!   SCTE-35 PIDs, or fold an HLS master's AUDIO/SUBTITLES renditions, onto the
//!   general-demux base so a consumer gets **one** unified inventory regardless of
//!   container.
//! * [`scte`] — **SCTE-35** `splice_info_section` and **SCTE-104 / ST 2010**
//!   parsers emitting typed splice/cue events (pure, golden-vector tested).
//! * [`dash`] — **MPEG-DASH** (ISO/IEC 23009-1) MPD manifest model + a pure
//!   segment-selection / ABR-ladder-awareness model; the HTTP fetch reuses the
//!   existing libav ingest.
//! * [`webrtc`] — **WebRTC** ingest: a pure, testable SDP offer/answer model;
//!   the ICE/DTLS/SRTP transport lives behind the off-by-default `webrtc`
//!   feature and is compile-verified only.
//! * [`srt`] — **SRT** caller/listener/rendezvous + AES-encryption + stream-id
//!   connection model and option parsing (pure); the socket lives behind the
//!   `ffmpeg`/`srt` gating.
//! * `libav` *(feature `ffmpeg`)* — real demux/decode adapters (`FileSource`,
//!   `TestPatternSource`) over `multiview-ffmpeg`'s **safe** wrappers, implementing
//!   [`source::FrameProducer`]. All FFI is owned by `multiview-ffmpeg`; this crate
//!   stays `unsafe_code = forbid`.
//! * `ndi` *(feature `ndi`)* — **NDI®** ingest (ADR-0008, IN-3): the pure
//!   UYVY/BGRA → NV12 host conversion plus an [`ndi::NdiProducer`] that samples a
//!   receive seam ([`ndi::NdiReceiver`]) into the IN-2 ingest pump. Sampled, never
//!   pacing (invariants #1/#2/#10). The runtime is dynamically loaded via the
//!   `multiview-ndi-sys` FFI leaf crate (which owns the `unsafe` `dlopen`), so this
//!   crate stays `unsafe_code = forbid`; the real receive needs the proprietary
//!   runtime + a live NDI network and is gated behind an `#[ignore]`d test. NDI® is
//!   a registered trademark of Vizrt NDI AB.
//! * `youtube` *(feature `youtube`)* — `YouTube` live resolver (ADR-0015): a
//!   pure `yt-dlp -J` info-dict parser (manifest extraction, live-status
//!   classification, `expire`-deadline parsing — no network/subprocess) plus a
//!   thin `tokio::process` spawn shell around a runtime-discovered `yt-dlp`. The
//!   resolved HLS master feeds the standard HLS ingest path; `yt-dlp` is never
//!   vendored or linked, keeping the default build LGPL-clean.
//!
//! All timing math is exact (i64 nanoseconds / i128 intermediates / exact
//! rationals via [`multiview_core::time`]) — **never** float fps.
//!
//! [ADR-T003]: per-input timestamp normalization.
//! [ADR-T004]: HLS ingest pacing.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod caption_store;
pub mod dash;
pub mod error;
pub mod hls;
pub mod inventory;
pub mod jitter;
pub mod mpegts;
pub mod normalize;
pub mod pacer;
pub mod param_probe;
pub mod reconnect;
pub mod scte;
pub mod source;
pub mod srt;
pub mod st2022_6;
pub mod st2022_7;
pub mod st2110;
pub mod tsl;
pub mod webrtc;

#[cfg(feature = "ffmpeg")]
pub mod libav;

#[cfg(feature = "ndi")]
pub mod ndi;

#[cfg(feature = "youtube")]
pub mod youtube;

pub use error::{Error, Result};
