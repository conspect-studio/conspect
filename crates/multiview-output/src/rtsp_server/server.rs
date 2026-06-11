//! The `gst-rtsp-server` serving implementation (behind the `rtsp-server`
//! feature).
//!
//! This module owns the actual GStreamer wiring for the OUT-2 in-process RTSP
//! server (ADR-0006 primary path):
//!
//! - a [`glib::MainLoop`] driven on its **own thread**, bridged to the rest of
//!   the engine only through the bounded drop-oldest queue (so the GLib loop can
//!   never back-pressure the output clock — invariants #1/#10);
//! - an `RTSPServer` bound to a host + port, with one shared
//!   [`RTSPMediaFactory`](gstreamer_rtsp_server::RTSPMediaFactory) per mount built
//!   from [`RtspCodec::launch_description`] and `set_shared(true)` so a **single**
//!   encode fans to every connected client (invariant #7);
//! - an `appsrc` `need-data` pump that, per client media, drains the shared
//!   [`BoundedPacketQueue`] and pushes the **already-encoded** NAL bytes into
//!   `appsrc` with the packet's already-tick-stamped PTS/duration (invariant #3,
//!   **no re-encode** — `h264parse`/`h265parse` only fixes up stream-format).
//!
//! Because GStreamer (and the GLib main loop) are not present in CI, building
//! this module requires the GStreamer dev libraries; it is exercised by the
//! ignored-by-default live `tests/rtsp_server_playout.rs` on a GStreamer-equipped
//! runner. The pure seam ([`super::BoundedPacketQueue`], [`super::RtspServerSink`],
//! [`super::RtspMount`], [`super::RtspCodec`]) is always compiled and CI-tested.

use std::str::FromStr;
use std::sync::Arc;
use std::thread::JoinHandle;

use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{RTSPMediaFactory, RTSPServer};

use crate::error::{Error, Result};
use crate::fanout::EncodedPacket;

use super::caps::{units_to_nanos, RtspCodec};
use super::mount::RtspMount;
use super::queue::BoundedPacketQueue;

/// Configuration for one served RTSP mount.
#[derive(Debug, Clone)]
pub struct RtspServerConfig {
    /// Bind address for the RTSP listener. IPv6-first (operator directive): use
    /// `"[::]"` (all interfaces, dual-stack) or `"[::1]"` (loopback); an IPv6
    /// literal must be bracketed so [`served_url`](RtspServerConfig::served_url)
    /// interpolates it correctly. A user-supplied IPv4 host (`"127.0.0.1"`) still
    /// works but is never the default.
    pub host: String,
    /// TCP port the RTSP listener binds (default RTSP `8554`).
    pub port: u16,
    /// The mount the program is served under.
    pub mount: RtspMount,
    /// The encoded video codec to payload.
    pub codec: RtspCodec,
    /// The packet-timestamp timebase as `(numerator, denominator)` seconds, e.g.
    /// `(1, 90000)` for a 90 kHz timebase or `(1, 30)` for whole 30 fps ticks.
    /// `appsrc` is run `format=time`, so each packet's integer `pts`/`duration`
    /// (in this timebase) is converted to nanoseconds before stamping the buffer
    /// (`ns = pts * 1_000_000_000 * num / den`). Numerator and denominator must be
    /// non-zero; a zero denominator disables stamping (the buffer carries no PTS).
    pub timebase: (u32, u32),
}

impl RtspServerConfig {
    /// The full URL clients connect to (`rtsp://host:port/mount`).
    #[must_use]
    pub fn served_url(&self) -> String {
        self.mount.served_url(&self.host, self.port)
    }
}

/// A running in-process RTSP server: the GLib main loop on its own thread plus
/// the shared media factory feeding clients from [`RtspServerHandle::queue`].
///
/// Dropping the handle stops the GLib main loop and joins the serving thread (a
/// host-side teardown that never touches the engine hot path).
pub struct RtspServerHandle {
    queue: Arc<BoundedPacketQueue>,
    // `glib::MainLoop` is `Send + Sync` and `quit()` is safe to call from any
    // thread, so the handle (on the caller's thread) can stop the loop running on
    // the serving thread.
    main_loop: glib::MainLoop,
    thread: Option<JoinHandle<()>>,
    served_url: String,
}

impl RtspServerHandle {
    /// Start an RTSP server for `config`, returning the handle whose
    /// [`queue`](Self::queue) the engine's [`RtspServerSink`](super::RtspServerSink)
    /// feeds.
    ///
    /// The `appsrc` buffer the server drains is the shared, bounded drop-oldest
    /// queue, so a slow/absent client never reaches back to the producer
    /// (invariants #1/#10). The serving GLib loop runs on its own thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Output`] if GStreamer cannot be initialized, an element
    /// in the launch pipeline is unavailable (`appsrc`, `h264parse`/`h265parse`,
    /// `rtph264pay`/`rtph265pay` not installed), or the server cannot bind the
    /// configured host/port.
    pub fn start(config: RtspServerConfig) -> Result<Self> {
        Self::start_with_queue(config, Arc::new(BoundedPacketQueue::new(DEFAULT_CAPACITY)))
    }

    /// Start the server feeding from an existing shared queue (so an already-built
    /// [`RtspServerSink`](super::RtspServerSink) and the serving side share one
    /// buffer).
    ///
    /// # Errors
    ///
    /// See [`RtspServerHandle::start`].
    pub fn start_with_queue(
        config: RtspServerConfig,
        queue: Arc<BoundedPacketQueue>,
    ) -> Result<Self> {
        gstreamer::init().map_err(|e| Error::Output(format!("gstreamer init failed: {e}")))?;

        let served_url = config.served_url();

        // The `GStreamer`/`RTSPServer` objects are constructed, attached, and run
        // on the serving thread: their sources must dispatch on the thread whose
        // default context they were attached to, and the RTSP server objects are
        // not `Send`. Only the `MainLoop` (which is `Send + Sync`) is created here
        // and cloned into the thread, so the handle can `quit()` it from the
        // caller's thread. The thread reports the bind outcome back over a sync
        // channel so `start` surfaces a port-in-use/permission failure as a typed
        // error rather than a silently-dead server.
        let main_loop = glib::MainLoop::new(None, false);
        let loop_for_thread = main_loop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let pump_queue = Arc::clone(&queue);
        let thread = std::thread::Builder::new()
            .name("mv-rtsp-server".to_owned())
            .spawn(move || {
                serve(&config, &loop_for_thread, &pump_queue, &ready_tx);
            })
            .map_err(|e| Error::Output(format!("could not spawn rtsp serving thread: {e}")))?;

        // Block only until the thread reports bind success/failure (a bounded,
        // one-shot startup handshake — never the engine hot path).
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                queue,
                main_loop,
                thread: Some(thread),
                served_url,
            }),
            Ok(Err(e)) => {
                // The serving thread already returned after the bind failure.
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::Output(
                    "rtsp serving thread exited before binding".to_owned(),
                ))
            }
        }
    }

    /// The shared bounded drop-oldest queue the engine feeds (via
    /// [`RtspServerSink`](super::RtspServerSink)). Cloning the `Arc` shares the
    /// same buffer with the serving side.
    #[must_use]
    pub fn queue(&self) -> Arc<BoundedPacketQueue> {
        Arc::clone(&self.queue)
    }

    /// The URL clients connect to (`rtsp://host:port/mount`).
    #[must_use]
    pub fn served_url(&self) -> &str {
        &self.served_url
    }

    /// Stop the GLib main loop and join the serving thread. Idempotent.
    pub fn stop(&mut self) {
        self.main_loop.quit();
        if let Some(handle) = self.thread.take() {
            // A serving thread that fails to join is a host-side teardown defect,
            // not an engine-hot-path concern; log via tracing rather than panic.
            if let Err(e) = handle.join() {
                tracing::warn!(?e, "rtsp serving thread did not join cleanly");
            }
        }
    }
}

impl Drop for RtspServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Default depth of the per-server bounded drop-oldest packet buffer (a few
/// frames of slack — enough to absorb scheduling jitter, small enough that a
/// recovering client gets near-live frames, never a long backlog).
const DEFAULT_CAPACITY: usize = 8;

/// The serving-thread body: build the `RTSPServer` + shared media factory on a
/// thread-default `GLib` context, attach (bind) it, report the bind outcome over
/// `ready_tx`, then run `main_loop` until the handle quits it.
///
/// All `RTSPServer`/factory/`MainContext` objects are thread-local here (they are
/// not `Send`); only `main_loop` (and the queue) crossed the thread boundary. A
/// failure at any construction/bind step is reported once over `ready_tx` and the
/// thread returns without running the loop.
fn serve(
    config: &RtspServerConfig,
    main_loop: &glib::MainLoop,
    queue: &Arc<BoundedPacketQueue>,
    ready_tx: &std::sync::mpsc::SyncSender<Result<()>>,
) {
    // A fresh main context for this thread; the `MainLoop` was created with the
    // default `None` context, so make this one the thread-default before running.
    let context = glib::MainContext::new();
    context.push_thread_default();

    let result = build_and_attach(config, &context, queue);
    let attached = match result {
        Ok(source_id) => {
            // Signal success; keep `source_id` alive for the loop's lifetime so the
            // server stays attached (dropping it would detach the listener).
            let _ = ready_tx.send(Ok(()));
            Some(source_id)
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            None
        }
    };

    if attached.is_some() {
        main_loop.run();
    }

    // Loop has quit (or never ran): detach the source and restore the context.
    drop(attached);
    context.pop_thread_default();
}

/// Construct the `RTSPServer` + shared media factory, register the mount, and
/// attach (bind) it to `context`, returning the attach source id on success.
fn build_and_attach(
    config: &RtspServerConfig,
    context: &glib::MainContext,
    queue: &Arc<BoundedPacketQueue>,
) -> Result<glib::SourceId> {
    let server = RTSPServer::new();
    server.set_address(&config.host);
    server.set_service(&config.port.to_string());

    let factory = RTSPMediaFactory::new();
    factory.set_launch(&config.codec.launch_description());
    // One encode fans to all clients (invariant #7): every client shares the same
    // underlying media/pipeline rather than re-running the launch line.
    factory.set_shared(true);

    // When a client connects, `GStreamer` instantiates the factory's pipeline;
    // grab its `appsrc` and wire the `need-data` pump that drains the shared
    // queue. Each connected client's callback closes over a clone of the *same*
    // `Arc<BoundedPacketQueue>` — the producer side is untouched (inv #1/#10).
    let media_caps = config.codec.appsrc_caps().to_owned();
    let timebase = config.timebase;
    let pump_queue = Arc::clone(queue);
    factory.connect_media_configure(move |_factory, media| {
        configure_media(media, &media_caps, timebase, &pump_queue);
    });

    let mounts = server
        .mount_points()
        .ok_or_else(|| Error::Output("rtsp server has no mount points".to_owned()))?;
    mounts.add_factory(config.mount.path(), factory);

    // `attach` binds the listening socket and can fail (port in use / permission).
    server
        .attach(Some(context))
        .map_err(|e| Error::Output(format!("rtsp server bind failed: {e}")))
}

/// Wire one client media's `appsrc`: set its caps + live timing properties and
/// install the `need-data` pump that drains `queue` into the source.
fn configure_media(
    media: &gstreamer_rtsp_server::RTSPMedia,
    caps_str: &str,
    timebase: (u32, u32),
    queue: &Arc<BoundedPacketQueue>,
) {
    let Some(element) = media.element() else {
        tracing::warn!("rtsp media has no element; cannot configure appsrc");
        return;
    };
    let Some(bin) = element.dynamic_cast::<gstreamer::Bin>().ok() else {
        tracing::warn!("rtsp media element is not a bin; cannot find appsrc");
        return;
    };
    let Some(src_element) = bin.by_name_recurse_up("src") else {
        tracing::warn!("rtsp media bin has no `src` appsrc element");
        return;
    };
    let Ok(appsrc) = src_element.dynamic_cast::<AppSrc>() else {
        tracing::warn!("rtsp media `src` element is not an appsrc");
        return;
    };

    match gstreamer::Caps::from_str(caps_str) {
        Ok(caps) => appsrc.set_caps(Some(&caps)),
        Err(e) => tracing::warn!(%e, "invalid appsrc caps string"),
    }
    appsrc.set_is_live(true);
    appsrc.set_format(gstreamer::Format::Time);

    let pump = Arc::clone(queue);
    appsrc.set_callbacks(
        gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |src, _length| {
                push_next_packet(src, timebase, &pump);
            })
            .build(),
    );
}

/// Drain the next encoded packet (if any) from `queue` into `appsrc`, stamping
/// the buffer with the packet's already-tick-derived PTS/duration converted to
/// nanoseconds via `timebase` (invariant #3, **no re-encode**). If the queue is
/// empty the pump simply returns; `appsrc` re-asks on the next `need-data` — a
/// temporary input gap never stalls serving.
fn push_next_packet(appsrc: &AppSrc, timebase: (u32, u32), queue: &Arc<BoundedPacketQueue>) {
    let Some(packet) = queue.pop() else {
        // No packet ready: do not block, do not push EOS — wait for the next
        // `need-data`. (The engine drives the queue on the output clock.)
        return;
    };
    let buffer = build_buffer(&packet, timebase);
    if let Err(e) = appsrc.push_buffer(buffer) {
        tracing::warn!(?e, "appsrc rejected buffer; client likely gone");
    }
}

/// Build a `gst::Buffer` carrying the packet's encoded bytes (zero-copy over the
/// shared `Arc<EncodedPacket>`), stamped with its PTS/DTS/duration converted from
/// the packet's integer `timebase` units to nanoseconds (the `appsrc`
/// `format=time` contract) via the pure, CI-tested
/// [`units_to_nanos`](super::caps::units_to_nanos) helper.
fn build_buffer(packet: &Arc<EncodedPacket>, timebase: (u32, u32)) -> gstreamer::Buffer {
    let mut buffer = gstreamer::Buffer::from_slice(PacketBytes(Arc::clone(packet)));
    {
        // `get_mut` succeeds: the buffer was just constructed and is uniquely
        // owned here.
        if let Some(buffer_ref) = buffer.get_mut() {
            buffer_ref.set_pts(clock_time_from_units(packet.pts, timebase));
            buffer_ref.set_dts(clock_time_from_units(packet.dts, timebase));
            buffer_ref.set_duration(clock_time_from_units(packet.duration, timebase));
        }
    }
    buffer
}

/// A zero-copy `AsRef<[u8]>` view over an `Arc<EncodedPacket>` so the buffer
/// shares the same encoded allocation (encode-once, invariant #7) rather than
/// copying the NAL bytes into the GStreamer buffer.
struct PacketBytes(Arc<EncodedPacket>);

impl AsRef<[u8]> for PacketBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.data
    }
}

/// Convert a non-negative timebase-unit count to a GStreamer [`ClockTime`],
/// applying the `(num, den)` seconds timebase (`ns = units * 1e9 * num / den`).
///
/// Negative values (which should never reach here — output PTS are re-stamped
/// from the monotonic tick counter) and a zero denominator map to `None` rather
/// than wrapping. Delegates to the pure, CI-tested [`units_to_nanos`].
fn clock_time_from_units(units: i64, timebase: (u32, u32)) -> Option<gstreamer::ClockTime> {
    units_to_nanos(units, timebase).map(gstreamer::ClockTime::from_nseconds)
}
