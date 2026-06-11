//! In-process **RTSP server** (OUT-2, ADR-0006 primary path) — the typed seam +
//! the feature-gated `gst-rtsp-server` serving thread.
//!
//! # What this is
//!
//! Multiview composites once and encodes the canvas once per rendition
//! (invariant #7). This module fans that **already-encoded** NAL stream to RTSP
//! clients with **no re-encode**, by feeding the coded packets into a `GStreamer`
//! `appsrc → h264parse → rtph264pay` pipeline hosted by `gst-rtsp-server`
//! (`appsrc → h265parse → rtph265pay` for HEVC). The serving factory is built
//! `set_shared(true)` so one encode fans to all connected clients (core-engine
//! §9.2).
//!
//! # Why it is feature-gated
//!
//! `gst-rtsp-server` pulls the `GStreamer`/`GLib` **C stack** (LGPL-2.1, dynamically
//! linked) and runs a `GLib` main loop. To keep the default Multiview build
//! pure-Rust, native-dep-free, and LGPL-clean, the whole serving path lives
//! behind the off-by-default [`rtsp-server`](../index.html) Cargo feature. The
//! default build (and GPU-free CI) compiles only the pure-Rust seam below and
//! never links `GStreamer`.
//!
//! # Layering — what is always compiled vs feature-gated
//!
//! Always compiled and CI-tested (no `GStreamer`):
//!
//! - [`BoundedPacketQueue`] — the bounded **drop-oldest** buffer that decouples a
//!   slow/absent RTSP client from the engine (invariants #1/#10): `push` never
//!   blocks and never grows past capacity; the oldest packet is shed first.
//! - [`RtspServerSink`] — the [`PacketSink`](crate::fanout::PacketSink) the
//!   engine routes packets to; `deliver` is a non-blocking hand-off into the
//!   queue, sharing the `Arc<EncodedPacket>` allocation (encode-once).
//! - [`RtspMount`] — validated mount path + `rtsp://host:port/mount` URL
//!   construction.
//! - [`RtspCodec`] — codec → `appsrc` caps + parser/payloader element selection.
//!
//! Behind the `rtsp-server` feature (pulls `GStreamer`):
//!
//! - `server` (this module, under `cfg(feature = "rtsp-server")`) — the `GLib`
//!   main loop on its own thread, the
//!   `RTSPServer`/`RTSPMountPoints` wiring, the shared media factory built from
//!   [`RtspCodec::launch_description`], and the `appsrc need-data` pump that
//!   drains the queue into `appsrc` with the packet's already-tick-stamped PTS
//!   (invariant #3). Construction is feature-checked here; the live serve is
//!   exercised by an ignored-by-default `tests/rtsp_server_playout.rs` against a
//!   `GStreamer`-equipped runner.

mod caps;
mod mount;
mod queue;
mod sink;

pub use caps::{units_to_nanos, RtspCapsError, RtspCodec};
pub use mount::{RtspMount, RtspMountError};
pub use queue::BoundedPacketQueue;
pub use sink::RtspServerSink;

/// The `GStreamer` `gst-rtsp-server` serving implementation (`GLib` main loop +
/// `appsrc → parse → pay` shared factory). Behind the off-by-default
/// `rtsp-server` feature so the default build links no `GStreamer`/`GLib`.
#[cfg(feature = "rtsp-server")]
pub mod server;
