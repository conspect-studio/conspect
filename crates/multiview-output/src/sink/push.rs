//! Resilient live push (SRT caller/listener, RTMP, RIST, UDP-TS, RTSP).
//!
//! [`PacketMuxSink::run_push_resilient`] replaces the one-shot "open once, die
//! on the first error" push with three cooperating roles, none of which can
//! back-pressure the bake/encode consumer (invariants #1/#10):
//!
//! * **distributor** — the calling sink thread. It drains the sink's bounded
//!   fan-out queue as fast as packets arrive and hands each packet to every
//!   attached peer's own bounded queue with `try_send`. With no peer attached
//!   (a listener waiting for a caller, a caller reconnecting) the packet is
//!   **dropped and counted** — it never blocks, so the consumer's fan-out queue
//!   never fills and the sink is never marked wedged.
//! * **link thread** — *caller* mode opens the transport through
//!   [`Muxer::create_push`], whose `AVIOInterruptCB` makes the blocking connect
//!   abortable, keeps one session, and reconnects with back-off (0.5 s → 5 s,
//!   jittered) when it ends. *Listener* mode (SRT `mode=listener`) holds **one**
//!   native socket from `multiview-srt-sys` for the whole run, because FFmpeg's
//!   libsrt can neither bind dual-stack `[::]` nor keep listening after an
//!   accept. Each caller gets its own writer while the socket keeps accepting,
//!   so there is no re-listen gap and N callers share one encode.
//! * **writer** — one per session: writes a fresh container header, then
//!   muxes packets from its queue. The distributor gates every new (or
//!   overflowed) session on the next **video keyframe**, so a decoder always
//!   starts on an IDR and audio after it stays aligned. A write error or a
//!   write blocked past [`WRITE_DEADLINE`] ends the session (the peer is gone),
//!   which the link thread observes and replaces.
//!
//! Teardown is bounded: at end-of-program the peer queues close, writers get
//! [`TEARDOWN_GRACE`] to flush their trailer, then the shared stop flag aborts
//! any libav call still blocked (accept, connect, write) through the interrupt
//! callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, Muxer, StreamCodecParameters, StreamKind};
use multiview_srt_sys::{parse_listener_url, SrtError, SrtListener, SrtSocket, LIVE_PAYLOAD};

use super::{
    add_track_streams, apply_mux_metadata, ff, route_packet, AudioSelection, MuxStream,
    PacketMuxKind, PacketMuxSink, PacketSource, SingleRoutes, SingleTarget,
};
use crate::error::{Error, Result};
use crate::metadata::{DisplayMatrix, MuxMetadata};
use crate::push_link::{redact_target, Backoff, PushLinkState, PushLinkStatus};

/// Per-session queue depth (packets). ~3–6 s of a typical A/V program; a peer
/// slower than real time overflows it, is re-gated to the next keyframe and
/// counted as dropped instead of growing memory (inv #10).
const CLIENT_QUEUE_CAP: usize = 512;
/// A single blocked write longer than this means the peer is gone.
const WRITE_DEADLINE: Duration = Duration::from_secs(5);
/// Caller-mode connect deadline (libsrt's own `connect_timeout` is shorter).
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// How long writers may flush their trailer at end-of-program before the stop
/// flag aborts their I/O. Well inside the pipeline's 2 s sink-wedge grace.
const TEARDOWN_GRACE: Duration = Duration::from_millis(800);
/// Simultaneous callers a listener serves; further callers are refused.
const MAX_LISTENER_CLIENTS: usize = 16;
/// Handshakes libsrt may queue between accepts.
const LISTEN_BACKLOG: u16 = 16;
/// A caller session at least this long resets the reconnect back-off.
const STABLE_SESSION: Duration = Duration::from_secs(5);
/// Poll step for interruptible sleeps / joins.
const POLL: Duration = Duration::from_millis(20);

/// What a resilient push run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PushRunReport {
    /// Peer sessions established (connects + accepts).
    pub sessions: u64,
    /// Packets the distributor pulled from the fan-out queue.
    pub received: u64,
    /// Packets dropped for this output (no peer attached / peer overflow).
    pub dropped: u64,
}

/// Whether `url` is an SRT URL in listener mode (`mode=listener`, the only
/// spelling libsrt reads), i.e. it accepts callers rather than dialling a peer.
#[must_use]
pub fn is_srt_listener_url(url: &str) -> bool {
    if !url.starts_with("srt://") {
        return false;
    }
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&')
            .any(|p| p.eq_ignore_ascii_case("mode=listener"))
    })
}

/// Owned stream layout + per-output mux metadata a writer needs (cloned per
/// session; never borrowed across threads).
#[derive(Clone)]
struct WriterSpec {
    video: (StreamCodecParameters, Rational),
    audio: Option<(StreamCodecParameters, Rational)>,
    tracks: Vec<(StreamCodecParameters, Rational)>,
    selection: AudioSelection,
    metadata: MuxMetadata,
    display_matrix: Option<DisplayMatrix>,
}

/// One attached peer as seen by the distributor.
struct Client {
    tx: SyncSender<EncodedPacket>,
    /// Waiting for the next video keyframe (new session or after overflow).
    gated: bool,
}

/// State shared by the distributor, the link thread and the writers.
struct Shared {
    clients: Mutex<Vec<Client>>,
    writers: Mutex<Vec<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    link: Arc<PushLinkStatus>,
}

impl Shared {
    fn clients(&self) -> std::sync::MutexGuard<'_, Vec<Client>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn writers(&self) -> std::sync::MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.writers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

impl PacketMuxSink {
    /// The push URL and container muxer when this is a push sink.
    #[must_use]
    pub fn push_target(&self) -> Option<(&str, &'static str)> {
        match &self.kind {
            PacketMuxKind::Single(SingleTarget::Push { url, muxer_name }) => {
                Some((url.as_str(), muxer_name))
            }
            _ => None,
        }
    }

    /// Run a **resilient** push over `source` until end-of-program (see the
    /// module docs): never blocks the producer, reconnects / re-listens with
    /// back-off, gates each session on a keyframe, and publishes its link state
    /// into `link`.
    ///
    /// # Errors
    /// [`Error::Output`] only when this sink is not a push sink, or the
    /// `source` itself fails. Network failures are never errors here — they are
    /// retried and reported through `link`.
    pub fn run_push_resilient<P: PacketSource>(
        &self,
        source: &mut P,
        video: MuxStream<'_>,
        audio: Option<MuxStream<'_>>,
        track_streams: &[MuxStream<'_>],
        link: &Arc<PushLinkStatus>,
    ) -> Result<PushRunReport> {
        let Some((url, muxer_name)) = self.push_target() else {
            return Err(Error::Output(
                "run_push_resilient called on a non-push sink".to_owned(),
            ));
        };
        let audio = if self.audio_selection.program {
            audio
        } else {
            None
        };
        let spec = WriterSpec {
            video: (video.params.clone(), video.time_base),
            audio: audio.map(|a| (a.params.clone(), a.time_base)),
            tracks: track_streams
                .iter()
                .map(|t| (t.params.clone(), t.time_base))
                .collect(),
            selection: self.audio_selection.clone(),
            metadata: self.metadata.clone(),
            display_matrix: self.display_matrix,
        };
        let shared = Arc::new(Shared {
            clients: Mutex::new(Vec::new()),
            writers: Mutex::new(Vec::new()),
            stop: Arc::new(AtomicBool::new(false)),
            link: Arc::clone(link),
        });
        let listener = is_srt_listener_url(url);
        let link_thread = {
            let shared = Arc::clone(&shared);
            let url = url.to_owned();
            std::thread::Builder::new()
                .name("multiview-push-link".to_owned())
                .spawn(move || {
                    if listener {
                        listener_main(&shared, &url, muxer_name, &spec);
                    } else {
                        caller_main(&shared, &url, muxer_name, &spec);
                    }
                })
                .map_err(|e| Error::Output(format!("spawn push link thread: {e}")))?
        };

        let mut report = PushRunReport::default();
        let pulled = distribute(source, &shared, &mut report);
        teardown(&shared, link_thread);
        report.sessions = link.snapshot().sessions;
        report.dropped = link.dropped();
        pulled.map(|()| report)
    }
}

/// The distributor loop: never blocks on a peer; drops + counts when no peer
/// is attached or a peer's queue is full.
fn distribute<P: PacketSource>(
    source: &mut P,
    shared: &Shared,
    report: &mut PushRunReport,
) -> Result<()> {
    while let Some(packet) = source.next_packet()? {
        report.received = report.received.saturating_add(1);
        let mut clients = shared.clients();
        if shared.link.take_resync() {
            for client in clients.iter_mut() {
                client.gated = true;
            }
        }
        if clients.is_empty() {
            shared.link.add_dropped(1);
            continue;
        }
        clients.retain_mut(|client| deliver(client, &packet, &shared.link));
    }
    Ok(())
}

/// Hand `packet` to one peer without blocking. Returns `false` when the peer's
/// writer has gone (remove it).
fn deliver(client: &mut Client, packet: &EncodedPacket, link: &PushLinkStatus) -> bool {
    if client.gated && !(matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe()) {
        link.add_dropped(1);
        return true;
    }
    match client.tx.try_send(packet.clone()) {
        Ok(()) => {
            client.gated = false;
            true
        }
        Err(TrySendError::Full(_)) => {
            // Slow peer: shed and resync on the next keyframe rather than
            // splice a hole into the middle of a GOP.
            client.gated = true;
            link.add_dropped(1);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Bounded teardown: close peer queues, give writers a grace to flush, then
/// abort any blocked libav I/O via the stop flag and detach stragglers.
fn teardown(shared: &Shared, link_thread: JoinHandle<()>) {
    shared.clients().clear();
    let deadline = Instant::now() + TEARDOWN_GRACE;
    while Instant::now() < deadline && shared.writers().iter().any(|h| !h.is_finished()) {
        std::thread::sleep(POLL);
    }
    shared.stop.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline
        && (!link_thread.is_finished() || shared.writers().iter().any(|h| !h.is_finished()))
    {
        std::thread::sleep(POLL);
    }
    if link_thread.is_finished() {
        let _ = link_thread.join();
    } else {
        tracing::warn!("push link thread still blocked at teardown; detaching");
    }
    for handle in shared.writers().drain(..) {
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    shared.link.set_state(PushLinkState::Idle);
}

/// Sleep `total` in short steps, returning early when stopped.
fn sleep_unless_stopped(shared: &Shared, total: Duration) {
    let deadline = Instant::now() + total;
    while !shared.stopped() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

fn backoff_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::from(std::process::id()) ^ u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0)
}

/// Report a link failure and wait out the next back-off step.
fn back_off(shared: &Shared, backoff: &mut Backoff, url: &str, error: &str) {
    let delay = backoff.next_delay();
    tracing::warn!(
        url = %redact_target(url),
        error,
        retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        "push link failed; retrying"
    );
    shared.link.set_error(error);
    shared.link.set_state(PushLinkState::Reconnecting);
    sleep_unless_stopped(shared, delay);
}

/// Caller mode: one session at a time, reconnecting until stopped.
fn caller_main(shared: &Shared, url: &str, muxer_name: &'static str, spec: &WriterSpec) {
    let mut backoff = Backoff::standard(backoff_seed());
    while !shared.stopped() {
        shared.link.set_state(PushLinkState::Connecting);
        let opened = Muxer::create_push(
            url,
            muxer_name,
            Arc::clone(&shared.stop),
            Some(CONNECT_DEADLINE),
        );
        let muxer = match opened {
            Ok(muxer) => muxer,
            Err(e) => {
                if !shared.stopped() {
                    back_off(shared, &mut backoff, url, &e.to_string());
                }
                continue;
            }
        };
        let started = Instant::now();
        let Some(writer) = start_session(shared, muxer, spec, Some(redact_target(url))) else {
            sleep_unless_stopped(shared, backoff.next_delay());
            continue;
        };
        while !shared.stopped() && !writer.is_finished() {
            std::thread::sleep(POLL);
        }
        shared.writers().push(writer);
        reap_finished(shared);
        if shared.stopped() {
            break;
        }
        // Only a session that actually lived resets the schedule: a peer that
        // accepts then fails the header must not hot-loop.
        if started.elapsed() >= STABLE_SESSION {
            backoff.reset();
        }
        shared.link.set_state(PushLinkState::Reconnecting);
        sleep_unless_stopped(shared, backoff.next_delay());
    }
}

/// Listener mode: hold one socket, give each caller its own writer. A bind or
/// socket failure rebinds with back-off; a bad URL keeps reporting its error.
fn listener_main(shared: &Shared, url: &str, muxer_name: &'static str, spec: &WriterSpec) {
    let mut backoff = Backoff::standard(backoff_seed());
    while !shared.stopped() {
        let bound = parse_listener_url(url, true)
            .and_then(|listen| SrtListener::bind(listen.addr, &listen.options, LISTEN_BACKLOG));
        let listener = match bound {
            Ok(listener) => listener,
            Err(e) => {
                back_off(shared, &mut backoff, url, &e.to_string());
                continue;
            }
        };
        backoff.reset();
        shared.link.set_state(PushLinkState::Listening);
        tracing::info!(addr = %listener.local_addr(), "srt listener ready");
        let failure = accept_callers(shared, &listener, muxer_name, spec);
        drop(listener);
        if let Some(e) = failure {
            back_off(shared, &mut backoff, url, &e.to_string());
        }
    }
}

/// Accept until stopped (`None`) or the socket fails (`Some`).
fn accept_callers(
    shared: &Shared,
    listener: &SrtListener,
    muxer_name: &'static str,
    spec: &WriterSpec,
) -> Option<SrtError> {
    loop {
        let stop = Arc::clone(&shared.stop);
        let accepted = listener.accept(&move || stop.load(Ordering::Acquire));
        let (socket, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(SrtError::Interrupted) => return None,
            Err(e) => return Some(e),
        };
        reap_finished(shared);
        if shared.link.clients() >= MAX_LISTENER_CLIENTS {
            tracing::warn!(
                limit = MAX_LISTENER_CLIENTS,
                "srt listener full; refusing caller"
            );
            continue;
        }
        let writer = socket_writer(socket, Arc::clone(&shared.stop));
        match Muxer::create_with_writer(muxer_name, LIVE_PAYLOAD, writer) {
            Ok(muxer) => {
                if let Some(handle) =
                    start_session(shared, muxer, spec, peer.map(|p| p.to_string()))
                {
                    shared.writers().push(handle);
                }
            }
            Err(e) => shared.link.set_error(e.to_string()),
        }
    }
}

/// The muxer byte sink for one accepted caller. The socket closes with the muxer.
fn socket_writer(socket: SrtSocket, stop: Arc<AtomicBool>) -> multiview_ffmpeg::PacketWriter {
    Box::new(move |data: &[u8]| {
        let stop = &stop;
        socket
            .send(data, WRITE_DEADLINE, &|| stop.load(Ordering::Acquire))
            .is_ok()
    })
}

/// Join and drop writer handles that already finished (bounded bookkeeping).
fn reap_finished(shared: &Shared) {
    let mut writers = shared.writers();
    let mut i = 0;
    while i < writers.len() {
        if writers.get(i).is_some_and(JoinHandle::is_finished) {
            let handle = writers.swap_remove(i);
            let _ = handle.join();
        } else {
            i += 1;
        }
    }
}

/// Register a new peer session: its bounded queue (gated on a keyframe) and
/// its writer thread. `None` if the writer thread cannot be spawned.
fn start_session(
    shared: &Shared,
    muxer: Muxer,
    spec: &WriterSpec,
    peer: Option<String>,
) -> Option<JoinHandle<()>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(CLIENT_QUEUE_CAP);
    let spec = spec.clone();
    let link = Arc::clone(&shared.link);
    link.client_attached(peer.clone());
    let writer_peer = peer.clone();
    let spawned = std::thread::Builder::new()
        .name("multiview-push-writer".to_owned())
        .spawn(move || writer_main(muxer, &rx, &spec, &link, writer_peer.as_deref()));
    match spawned {
        Ok(handle) => {
            shared.clients().push(Client { tx, gated: true });
            tracing::info!("push peer session started");
            Some(handle)
        }
        Err(e) => {
            shared.link.client_detached(peer.as_deref());
            shared.link.set_error(format!("spawn push writer: {e}"));
            None
        }
    }
}

/// One peer session: header, then packets until the queue closes or a write
/// fails. Always detaches from the link status on exit.
fn writer_main(
    mut muxer: Muxer,
    rx: &Receiver<EncodedPacket>,
    spec: &WriterSpec,
    link: &PushLinkStatus,
    peer: Option<&str>,
) {
    let result = write_session(&mut muxer, rx, spec, link);
    // Best-effort trailer under a short deadline; the peer may be gone.
    muxer.arm_io_deadline(Some(Duration::from_millis(500)));
    match muxer.finish() {
        Ok(()) | Err(_) => {}
    }
    link.client_detached(peer);
    if let Err(e) = result {
        tracing::warn!(error = %e, "push peer session ended");
        link.set_error(e.to_string());
    } else {
        tracing::info!("push peer session closed at end-of-program");
    }
}

fn write_session(
    muxer: &mut Muxer,
    rx: &Receiver<EncodedPacket>,
    spec: &WriterSpec,
    link: &PushLinkStatus,
) -> Result<()> {
    let video_index = muxer
        .add_stream_from_parameters(&spec.video.0, spec.video.1)
        .map_err(ff)?;
    let audio_index = match &spec.audio {
        Some((params, tb)) => Some(muxer.add_stream_from_parameters(params, *tb).map_err(ff)?),
        None => None,
    };
    let track_mux: Vec<MuxStream<'_>> = spec
        .tracks
        .iter()
        .map(|(p, tb)| MuxStream::new(p, *tb))
        .collect();
    let tracks = add_track_streams(muxer, &spec.selection, &track_mux)?;
    apply_mux_metadata(muxer, &spec.metadata, spec.display_matrix, video_index)?;
    muxer.arm_io_deadline(Some(WRITE_DEADLINE));
    muxer.write_header().map_err(ff)?;
    let routes = SingleRoutes {
        video: video_index,
        audio: audio_index,
        drop_program_audio: !spec.selection.program,
        tracks: &tracks,
    };
    while let Ok(packet) = rx.recv() {
        let Some(index) = route_packet(&routes, &packet)? else {
            continue;
        };
        muxer.arm_io_deadline(Some(WRITE_DEADLINE));
        muxer
            .write_packet(index, packet.into_owned_packet())
            .map_err(ff)?;
        link.add_written();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_srt_listener_url;

    #[test]
    fn listener_detection() {
        assert!(is_srt_listener_url("srt://0.0.0.0:9000?mode=listener"));
        assert!(is_srt_listener_url(
            "srt://0.0.0.0:9000?latency=1&mode=listener"
        ));
        assert!(!is_srt_listener_url("srt://[::1]:9000?mode=caller"));
        assert!(!is_srt_listener_url("srt://[::1]:9000"));
        assert!(!is_srt_listener_url("rtmp://x/app?mode=listener"));
    }
}
