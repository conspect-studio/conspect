//! The live WHEP-**serve** output endpoint driver — **feature `native`**
//! (ADR-0049 §5.1, ADR-0048 §7).
//!
//! A browser WHEP player `POST`s a recvonly SDP offer to
//! `/api/v1/whep/{output_id}`; the endpoint answers `201` and then **sends** the
//! program's already-encoded H.264 (+ Opus) access units to it over SRTP. This
//! module owns the *serve half*: one tokio task drives the single dual-stack UDP
//! socket and every viewer [`Session`] (the canonical sans-IO loop ADR-0048 §7
//! pins) plus the in-crate TURN client (the operator's NAT-traversal path,
//! live-in-the-driver exactly like WHIP ingest), and on each tick drains the
//! shared per-output [`EgressFeed`] and sample-writes each program AU into every
//! viewer of that output.
//!
//! ## Encode-once-mux-many (invariant #7)
//!
//! The program is encoded **once** upstream (the cli bake consumer's single
//! `ProgramEncoder`); a `webrtc` output's sink runner re-stamps each coded packet
//! into an [`EgressSample`](crate::egress::EgressSample) and pushes it onto the
//! output's [`EgressFeed`]. The per-viewer marginal cost here is **packetization
//! only** (str0m RTP + SRTP), never a re-encode. SPS/PPS are cached and prepended
//! at each IDR so a late joiner decodes from the next keyframe.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The driver never `.await`s a viewer (UDP send is non-blocking); the program
//! AUs cross only the bounded drop-oldest [`EgressFeed`], so a slow or stalled
//! viewer loses *its* media and can never grow memory, stall the fan-out, or
//! touch the output clock. New viewers register over a bounded command channel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::EndpointConfig;
use crate::egress::{EgressFeed, EgressMedia};
use crate::error::{Result, WebRtcError};
use crate::session::{SessionId, SessionRole, SessionTable};
use crate::transport::{Session, SessionConfig, WebRtcEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the driver wakes to advance ICE/DTLS timers, drain the egress feed,
/// run GC, and pump TURN when otherwise idle.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// One admitted WHEP viewer session plus its per-session SPS/PPS cache.
pub(crate) struct ViewerSession {
    id: SessionId,
    output_id: String,
    session: Session,
    /// Whether the video keyframe seam has been sent to this viewer yet — a late
    /// joiner only decodes from the first IDR, so delta AUs before the first
    /// keyframe are dropped for this viewer (the standard remuxer late-join gate).
    saw_keyframe: bool,
    /// Whether ICE/DTLS reported connected at least once (telemetry).
    was_connected: bool,
}

/// A command to the running driver over the bounded channel.
pub(crate) enum Command {
    /// Register a freshly-negotiated WHEP viewer session.
    Register(Box<ViewerSession>),
    /// Tear down the session `session_id` (WHEP `DELETE`).
    Release { session_id: SessionId },
}

/// One configured `webrtc` output: its per-output viewer cap + the shared egress
/// feed carrying the program AUs the cli's sink runner pushes.
#[derive(Clone)]
struct OutputRegistration {
    max_viewers: u32,
    feed: EgressFeed,
}

/// A handle the WHEP control provider uses to negotiate / release viewer sessions
/// against the running [`WhepServeEndpoint`]. Cheap to clone.
#[derive(Clone)]
pub struct WhepServeHandle {
    inner: Arc<WhepShared>,
}

/// Shared state between the handle (negotiation) and the driver task.
pub(crate) struct WhepShared {
    /// Bounded command channel to the driver (register / release).
    commands: mpsc::Sender<Command>,
    /// Host candidate addresses gathered at bind, IPv6-first (ADR-0042).
    host_candidates: Vec<SocketAddr>,
    /// The endpoint-global viewer-pool session table (ADR-0048 §8); guarded by a
    /// short-lived mutex the engine never touches.
    table: Mutex<SessionTable>,
    /// Per-output registration (cap + feed), keyed by output id.
    outputs: Mutex<HashMap<String, OutputRegistration>>,
    /// Per-output live viewer session ids (for the per-output `max_viewers` cap +
    /// `DELETE` resolution).
    viewers_by_output: Mutex<HashMap<String, Vec<SessionId>>>,
    /// TURN relay transport addresses the driver's in-crate TURN client allocated
    /// (ADR-0048 §5.1), offered as relay candidates on each negotiated viewer.
    learned_relays: Mutex<Vec<SocketAddr>>,
}

impl WhepShared {
    /// Publish freshly-learned TURN relays into the shared set so each future
    /// negotiation offers them as relay candidates (de-duped; a poisoned lock is a
    /// best-effort no-op). The unified driver calls this from the one TURN driver.
    pub(crate) fn push_relays(&self, relays: &[SocketAddr]) {
        if let Ok(mut learned) = self.learned_relays.lock() {
            for relay in relays {
                if !learned.contains(relay) {
                    learned.push(*relay);
                }
            }
        }
    }
}

impl std::fmt::Debug for WhepServeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepServeHandle")
            .field("host_candidates", &self.inner.host_candidates)
            .finish_non_exhaustive()
    }
}

/// The negotiated answer for one WHEP viewer.
#[derive(Debug)]
pub struct WhepNegotiated {
    /// The minted session id (the trailing segment of the WHEP resource URL).
    pub session_id: SessionId,
    /// str0m's own complete answer SDP (BUNDLE / mid / rtcp-mux / fmtp).
    pub answer_sdp: String,
}

/// Whether the offer carries an Opus audio m-line — a lightweight scan so the
/// server only negotiates audio when the viewer offered it.
fn offer_has_audio(offer: &str) -> bool {
    offer.lines().any(|l| l.trim_start().starts_with("m=audio"))
}

/// The WHEP-serve viewer lane the [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint)
/// owns: the registered viewers plus the command receiver + shared negotiation
/// state. The unified driver steps it on the single shared socket (ADR-0048 §4).
pub(crate) struct ServeLane {
    pub(crate) commands: mpsc::Receiver<Command>,
    pub(crate) shared: Arc<WhepShared>,
    pub(crate) viewers: Vec<ViewerSession>,
}

impl WhepServeHandle {
    /// Build the handle + command receiver + shared state from a config and the
    /// gathered host candidates, **without binding a socket** — used by both
    /// [`WhepServeEndpoint::bind`] (which binds first) and the single-socket
    /// [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) (which shares one
    /// socket across roles, ADR-0048 §4).
    pub(crate) fn build(
        config: &EndpointConfig,
        host_candidates: Vec<SocketAddr>,
    ) -> (Self, mpsc::Receiver<Command>, Arc<WhepShared>) {
        let (tx, rx) = mpsc::channel(64);
        let shared = Arc::new(WhepShared {
            commands: tx,
            host_candidates,
            table: Mutex::new(SessionTable::new(
                config.max_sessions,
                config.session_idle_timeout,
                config.tombstone_ttl,
            )),
            outputs: Mutex::new(HashMap::new()),
            viewers_by_output: Mutex::new(HashMap::new()),
            learned_relays: Mutex::new(Vec::new()),
        });
        let handle = Self {
            inner: Arc::clone(&shared),
        };
        (handle, rx, shared)
    }

    /// Register a configured `webrtc` output: its per-output `max_viewers` cap and
    /// the shared [`EgressFeed`] carrying the program AUs. Called once per output
    /// at run start (the cli's sink runner owns the paired
    /// [`EgressSink`](crate::egress::EgressSink)).
    pub fn register_output(&self, output_id: &str, max_viewers: u32, feed: EgressFeed) {
        if let Ok(mut outputs) = self.inner.outputs.lock() {
            outputs.insert(
                output_id.to_owned(),
                OutputRegistration {
                    max_viewers: max_viewers.max(1),
                    feed,
                },
            );
        }
    }

    /// Negotiate a WHEP viewer session for `output_id` from the player's SDP
    /// `offer`. `want_audio` is whether this output carries audio (the offer must
    /// also advertise it). Returns the answer SDP + minted session id.
    ///
    /// # Errors
    ///
    /// * [`WebRtcError::UnknownSession`] — `output_id` is not a configured
    ///   `webrtc` output (`404`).
    /// * [`WebRtcError::MalformedSdp`] / [`WebRtcError::NoCompatibleCodec`] — the
    ///   offer (`400` / `406`).
    /// * [`WebRtcError::AtCapacity`] — over the per-output `max_viewers` **or** the
    ///   endpoint-global viewer pool (`503` + `Retry-After`).
    /// * [`WebRtcError::Config`] — no reachable ICE candidate (set
    ///   `webrtc.advertised_addresses`).
    pub fn negotiate(
        &self,
        output_id: &str,
        offer: &str,
        want_audio: bool,
    ) -> Result<WhepNegotiated> {
        // The output must be a configured `webrtc` output.
        let registration = {
            let outputs = self
                .inner
                .outputs
                .lock()
                .map_err(|_| WebRtcError::Transport("whep output map poisoned".to_owned()))?;
            outputs
                .get(output_id)
                .cloned()
                .ok_or_else(|| WebRtcError::UnknownSession(output_id.to_owned()))?
        };

        // Per-output capacity (max_viewers) checked before the global pool so the
        // operator's per-output cap is the dominant 503 signal.
        {
            let viewers = self
                .inner
                .viewers_by_output
                .lock()
                .map_err(|_| WebRtcError::Transport("whep viewer map poisoned".to_owned()))?;
            let live = viewers.get(output_id).map_or(0, Vec::len);
            if u32::try_from(live).unwrap_or(u32::MAX) >= registration.max_viewers {
                return Err(WebRtcError::AtCapacity);
            }
        }

        let now = Instant::now();
        let audio = want_audio && offer_has_audio(offer);
        let mut session = Session::new(&SessionConfig::serve(), now);
        // Gather host + advertised candidates (IPv6-first), skipping the
        // unspecified bind addr (never a valid ICE candidate).
        let mut gathered = 0usize;
        for addr in &self.inner.host_candidates {
            if addr.ip().is_unspecified() {
                continue;
            }
            session.add_host_candidate(*addr)?;
            gathered += 1;
        }
        if let Ok(relays) = self.inner.learned_relays.lock() {
            for relay in relays.iter() {
                if let Some(local) = self
                    .inner
                    .host_candidates
                    .iter()
                    .find(|a| !a.ip().is_unspecified())
                {
                    let _ = session.add_relay_candidate(*relay, *local);
                }
            }
        }
        if gathered == 0 {
            return Err(WebRtcError::Config(
                "no reachable ICE candidate: set webrtc.advertised_addresses".to_owned(),
            ));
        }
        let _ = audio; // audio is negotiated by str0m from the offer's m-lines.
        let answer_sdp = session.accept_offer(offer)?;

        // Admit against the endpoint-global viewer pool (OutputViewer counts).
        let session_id = {
            let mut table =
                self.inner.table.lock().map_err(|_| {
                    WebRtcError::Transport("whep session table poisoned".to_owned())
                })?;
            table.admit(SessionRole::OutputViewer, now)?
        };
        {
            let mut viewers = self
                .inner
                .viewers_by_output
                .lock()
                .map_err(|_| WebRtcError::Transport("whep viewer map poisoned".to_owned()))?;
            viewers
                .entry(output_id.to_owned())
                .or_default()
                .push(session_id.clone());
        }

        let viewer = ViewerSession {
            id: session_id.clone(),
            output_id: output_id.to_owned(),
            session,
            saw_keyframe: false,
            was_connected: false,
        };
        if self
            .inner
            .commands
            .try_send(Command::Register(Box::new(viewer)))
            .is_err()
        {
            // The endpoint is gone — undo the bookkeeping and report unavailability.
            self.forget_viewer(output_id, &session_id);
            self.close_in_table(&session_id);
            return Err(WebRtcError::AtCapacity);
        }
        Ok(WhepNegotiated {
            session_id,
            answer_sdp,
        })
    }

    /// Release the viewer `session_id` for `output_id` (WHEP `DELETE`). Idempotent:
    /// `true` when a matching live viewer was found and a teardown dispatched,
    /// `false` for an unknown/already-released session.
    #[must_use]
    pub fn release(&self, output_id: &str, session_id: &str) -> bool {
        let id = SessionId::from_str(session_id);
        let known = match self.inner.viewers_by_output.lock() {
            Ok(viewers) => viewers.get(output_id).is_some_and(|ids| ids.contains(&id)),
            Err(_) => false,
        };
        if !known {
            return false;
        }
        self.forget_viewer(output_id, &id);
        let _ = self
            .inner
            .commands
            .try_send(Command::Release { session_id: id });
        true
    }

    /// The number of live viewers on `output_id` (telemetry / tests).
    #[must_use]
    pub fn live_viewer_count(&self, output_id: &str) -> usize {
        self.inner
            .viewers_by_output
            .lock()
            .map_or(0, |v| v.get(output_id).map_or(0, Vec::len))
    }

    fn forget_viewer(&self, output_id: &str, id: &SessionId) {
        if let Ok(mut viewers) = self.inner.viewers_by_output.lock() {
            if let Some(ids) = viewers.get_mut(output_id) {
                ids.retain(|x| x != id);
            }
        }
    }

    fn close_in_table(&self, id: &SessionId) {
        if let Ok(mut table) = self.inner.table.lock() {
            table.close(id, Instant::now());
        }
    }
}

/// The WHEP-serve endpoint: the bound socket + the registered viewer sessions.
pub struct WhepServeEndpoint {
    endpoint: WebRtcEndpoint,
    commands: mpsc::Receiver<Command>,
    shared: Arc<WhepShared>,
}

impl std::fmt::Debug for WhepServeEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepServeEndpoint")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl WhepServeEndpoint {
    /// Bind the dual-stack socket and create the endpoint + its control handle.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(config: EndpointConfig) -> Result<(Self, WhepServeHandle)> {
        let endpoint = WebRtcEndpoint::bind(config)?;
        let host_candidates = endpoint.host_candidates()?;
        let (handle, commands, shared) = WhepServeHandle::build(endpoint.config(), host_candidates);
        Ok((
            Self {
                endpoint,
                commands,
                shared,
            },
            handle,
        ))
    }

    /// Run the driver loop until `stop` is raised, owning the socket from
    /// [`bind`](Self::bind). Retained as the **standalone** WHEP-serve driver for
    /// direct use/tests; the cli runs every WebRTC role on ONE socket through
    /// [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) instead (ADR-0048
    /// §4). The live socket loop is hardware-gated; it never blocks the engine.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run(self, stop: Arc<AtomicBool>) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        let mut turn = TurnRelayDriver::from_config(self.endpoint.config(), Instant::now());
        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;

        let mut viewers: Vec<ViewerSession> = Vec::new();
        let mut commands = self.commands;
        let mut buf = vec![0u8; RECV_BUFFER];
        let mut tick = tokio::time::interval(DRIVER_TICK);

        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::select! {
                cmd = commands.recv() => {
                    if !Self::apply_command(&mut viewers, cmd) {
                        return Ok(());
                    }
                }
                recv = socket.recv_from(&mut buf) => {
                    let now = Instant::now();
                    if let Ok((len, src)) = recv {
                        if let Some(payload) = buf.get(..len) {
                            Self::on_inbound(
                                &mut viewers, &mut turn, src, local_addr, payload, now,
                            );
                        }
                    }
                    Self::pump_turn(&socket, &mut turn, now, &self.shared).await;
                    Self::pump_outbound(&socket, &mut viewers, &mut turn, now).await;
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    Self::tick(&mut viewers, &self.shared, now);
                    Self::pump_turn(&socket, &mut turn, now, &self.shared).await;
                    Self::pump_outbound(&socket, &mut viewers, &mut turn, now).await;
                    Self::reap(&mut viewers, &self.shared);
                }
            }
        }
    }

    /// Apply a register/release command to the viewer set. Returns `false` when the
    /// command channel has closed (all handles dropped) so the driver should exit.
    pub(crate) fn apply_command(viewers: &mut Vec<ViewerSession>, cmd: Option<Command>) -> bool {
        match cmd {
            Some(Command::Register(v)) => {
                viewers.push(*v);
                true
            }
            Some(Command::Release { session_id }) => {
                for v in viewers.iter_mut().filter(|v| v.id == session_id) {
                    v.session.disconnect();
                }
                true
            }
            None => false,
        }
    }

    /// Classify and route one inbound datagram (relay-aware, defect C): a relayed
    /// Data indication is decapsulated to the inner media; a TURN-server control
    /// reply feeds the relay driver + publishes any learned relay; any other
    /// datagram is media for the viewer demux.
    pub(crate) fn on_inbound(
        viewers: &mut [ViewerSession],
        turn: &mut TurnRelayDriver,
        src: SocketAddr,
        local_addr: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        match crate::transport::relay_io::classify_inbound(turn, src, payload, now) {
            crate::transport::relay_io::Inbound::Relayed {
                peer,
                relay,
                payload,
            } => Self::route_datagram(viewers, peer, relay, &payload, now),
            crate::transport::relay_io::Inbound::TurnControl => {}
            crate::transport::relay_io::Inbound::Media => {
                Self::route_datagram(viewers, src, local_addr, payload, now);
            }
        }
    }

    /// One idle-tick step: drain the egress feeds into the viewers and advance each
    /// viewer's ICE/DTLS timers (the outbound + GC are pumped by the caller).
    pub(crate) fn tick(viewers: &mut [ViewerSession], shared: &WhepShared, now: Instant) {
        Self::pump_egress(viewers, now, shared);
        for v in viewers.iter_mut() {
            let _ = v.session.handle_timeout(now);
            v.was_connected |= v.session.is_connected();
        }
    }

    /// Drain each output's egress feed once and sample-write every program AU
    /// into that output's viewers. The single program encode fans to N viewers
    /// (encode-once, invariant #7); a viewer that has not yet seen a keyframe
    /// skips delta AUs until the first IDR (late-join gate).
    pub(crate) fn pump_egress(viewers: &mut [ViewerSession], now: Instant, shared: &WhepShared) {
        // Snapshot the per-output feeds so the lock is not held during writes.
        let feeds: Vec<(String, EgressFeed)> = match shared.outputs.lock() {
            Ok(outputs) => outputs
                .iter()
                .map(|(id, reg)| (id.clone(), reg.feed.clone()))
                .collect(),
            Err(_) => return,
        };
        for (output_id, feed) in feeds {
            // Bounded drain: at most a small batch per tick so one output cannot
            // starve another (the feed is drop-oldest, so unread samples are lost,
            // never queued — invariant #10).
            for _ in 0..256 {
                let Some(sample) = feed.pop() else { break };
                for v in viewers.iter_mut().filter(|v| v.output_id == output_id) {
                    if !v.session.is_connected() {
                        continue;
                    }
                    match sample.media {
                        EgressMedia::Video => {
                            if sample.keyframe {
                                v.saw_keyframe = true;
                            }
                            if !v.saw_keyframe {
                                // Late joiner before its first IDR: skip delta AUs.
                                continue;
                            }
                            let _ = v.session.write_video_sample(
                                &sample.data,
                                sample.keyframe,
                                sample.rtp_timestamp,
                                now,
                            );
                        }
                        EgressMedia::Audio => {
                            let _ = v.session.write_audio_sample(
                                &sample.data,
                                sample.rtp_timestamp,
                                now,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Route one received datagram to the first viewer session that accepts it.
    pub(crate) fn route_datagram(
        viewers: &mut [ViewerSession],
        src: SocketAddr,
        local: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        for v in viewers.iter_mut() {
            let _ = v.session.handle_datagram(src, local, payload, now);
            v.was_connected |= v.session.is_connected();
        }
    }

    /// Drain every viewer's outbound datagrams onto the socket (non-blocking send;
    /// a send error drops the datagram — never blocks the loop).
    pub(crate) async fn pump_outbound(
        socket: &tokio::net::UdpSocket,
        viewers: &mut [ViewerSession],
        turn: &mut TurnRelayDriver,
        now: Instant,
    ) {
        for v in viewers.iter_mut() {
            while let Some((source, dst, payload)) = v.session.poll_transmit(now) {
                crate::transport::relay_io::send_routed(socket, turn, source, dst, &payload, now)
                    .await;
            }
        }
    }

    /// Drive the shared TURN relay driver: send each queued datagram to its TURN
    /// server (allocate/refresh/retransmit) and publish any learned relay into
    /// `shared.learned_relays` so future negotiations offer it as a relay
    /// candidate (the operator's NAT-traversal path). Non-blocking.
    pub(crate) async fn pump_turn(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        now: Instant,
        shared: &WhepShared,
    ) {
        while let Some((destination, payload)) = turn.poll_transmit(now) {
            let _ = socket.send_to(&payload, destination).await;
        }
        let new_relays = turn.take_new_relays();
        if !new_relays.is_empty() {
            if let Ok(mut relays) = shared.learned_relays.lock() {
                for relay in new_relays {
                    if !relays.contains(&relay) {
                        relays.push(relay);
                    }
                }
            }
        }
    }

    /// Remove viewer sessions whose `Rtc` has died, freeing their per-output slot
    /// and the global pool entry.
    pub(crate) fn reap(viewers: &mut Vec<ViewerSession>, shared: &WhepShared) {
        viewers.retain_mut(|v| {
            if v.session.is_alive() {
                return true;
            }
            if let Ok(mut by_output) = shared.viewers_by_output.lock() {
                if let Some(ids) = by_output.get_mut(&v.output_id) {
                    ids.retain(|x| x != &v.id);
                }
            }
            if let Ok(mut table) = shared.table.lock() {
                table.close(&v.id, Instant::now());
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::offer_has_audio;

    #[test]
    fn offer_audio_detection() {
        assert!(offer_has_audio(
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n"
        ));
        assert!(!offer_has_audio(
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n"
        ));
    }
}
