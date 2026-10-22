//! The live WHIP-ingest endpoint driver — **feature `native`** (ADR-0048 §7,
//! ADR-T014 §4).
//!
//! One tokio task owns the single dual-stack UDP socket and every ingest
//! [`Session`]; it is the canonical sans-IO drive loop ADR-0048 §7 pins:
//! `recv → route to the session that accepts() it → drive → send outbound →
//! sleep until the earliest timer`. New publishers are registered over a
//! **bounded** command channel; each session's decrypted RTP crosses **only**
//! its drop-oldest [`RtpRing`] into the consumer's `MediaEngine` pull. The
//! engine never awaits this task and the task never blocks on a peer (UDP send
//! is non-blocking; a full ring drops oldest), so a wedged or saturated endpoint
//! loses *ingest media* — never an output tick (invariants #1 / #10).
//!
//! ## Honest scope
//!
//! The async socket loop ([`WhipEndpoint::run`]) is **live** — it binds a real
//! dual-stack UDP socket and is exercised on the hardware-gated soak, not in CI.
//! The negotiation, candidate gathering, session registration, RTP routing, GC,
//! and the bounded-ring isolation are all driven offline (the `Session` shuttle
//! tests + the [`RtpRing`] unit tests). The driver is intentionally thin glue
//! over those proven parts.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::EndpointConfig;
use crate::error::{Result, WebRtcError};
use crate::session::{SessionId, SessionRole, SessionTable};
use crate::transport::{RtpRing, Session, SessionConfig, WebRtcEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the recv loop reads (a generous MTU ceiling; WebRTC
/// packets are well under this).
const RECV_BUFFER: usize = 2048;

/// How often the driver wakes to run session GC and timeouts when otherwise
/// idle (a publisher between packets still advances ICE/DTLS timers).
const DRIVER_TICK: Duration = Duration::from_millis(50);

/// A registered ingest session plus its per-session RTP egress ring.
pub(crate) struct DrivenSession {
    id: SessionId,
    source_id: String,
    session: Session,
    ring: RtpRing,
    /// Whether ICE/DTLS has reported connected at least once (for telemetry).
    was_connected: bool,
    /// The last time a PLI was sent toward the publisher (rate-limit floor).
    last_pli: Option<Instant>,
}

/// A command sent to the running driver over the bounded channel.
pub(crate) enum Command {
    /// Register a freshly-negotiated ingest session.
    Register(Box<DrivenSession>),
    /// Tear down the session `session_id` (WHIP `DELETE`).
    Release { session_id: SessionId },
}

/// PLI rate-limit floor (ADR-T014 §7): at most one PLI per this interval per
/// session while the keyframe gate is closed.
const PLI_FLOOR: Duration = Duration::from_secs(2);

/// A handle the WHIP control provider uses to negotiate and release ingest
/// sessions against the running [`WhipEndpoint`]. Cheap to clone (an `Arc` of
/// shared state + the command sender).
#[derive(Clone)]
pub struct WhipHandle {
    inner: Arc<WhipShared>,
}

/// Shared state between the handle (negotiation) and the driver task.
pub(crate) struct WhipShared {
    /// Bounded command channel to the driver (register / release).
    commands: mpsc::Sender<Command>,
    /// Host candidate addresses gathered at bind, IPv6-first (ADR-0042).
    host_candidates: Vec<SocketAddr>,
    /// The session bookkeeping table (one-publisher-per-source / GC); guarded by
    /// a short-lived mutex the engine never touches.
    table: std::sync::Mutex<SessionTable>,
    /// Per-source live session id, so a second publisher is a `409` and a
    /// `DELETE` resolves the right session. Guarded by the same discipline.
    live_by_source: std::sync::Mutex<HashMap<String, SessionId>>,
    /// TURN **relay** transport addresses the driver's in-crate TURN client has
    /// allocated (ADR-0048 §5.1), populated as `Allocate` succeeds and offered as
    /// relay candidates on each negotiated session (the operator's NAT-traversal
    /// last resort, IPv6-first-ordered by str0m). Empty until/unless a TURN
    /// server is configured and an allocation completes.
    learned_relays: std::sync::Mutex<Vec<SocketAddr>>,
}

impl WhipShared {
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

impl std::fmt::Debug for WhipHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhipHandle")
            .field("host_candidates", &self.inner.host_candidates)
            .finish_non_exhaustive()
    }
}

/// The negotiated answer plus the RTP egress ring for one ingest session.
#[derive(Debug)]
pub struct WhipNegotiated {
    /// The minted session id (the trailing segment of the WHIP resource URL).
    pub session_id: SessionId,
    /// str0m's own complete answer SDP (BUNDLE / mid / rtcp-mux / fmtp).
    pub answer_sdp: String,
    /// The per-session drop-oldest RTP ring the ingest source drains.
    pub ring: RtpRing,
    /// The negotiated **video** RTP payload type (H.264), parsed from the answer
    /// so the consumer can route the ring's packets to the H.264 depacketizer
    /// without re-parsing SDP. `None` if no video section was negotiated.
    pub video_payload_type: Option<u8>,
    /// The negotiated **audio** RTP payload type (Opus), or `None` when audio was
    /// declined (`audio = false`) or not offered.
    pub audio_payload_type: Option<u8>,
}

/// Parse the first payload type of the `m=<kind>` section from an answer SDP.
///
/// Walks `m=video …`/`m=audio …` lines; the first numeric format token on the
/// line is the (single) negotiated dynamic payload type str0m answers with.
/// Best-effort and panic-free — `None` when the section is absent/unparseable.
fn answer_payload_type(sdp: &str, media: &str) -> Option<u8> {
    let prefix = format!("m={media} ");
    for line in sdp.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        // `m=video <port> <proto> <fmt> [<fmt> …]` — the 3rd+ tokens are PTs.
        return rest
            .split_whitespace()
            .nth(2)
            .and_then(|pt| pt.parse::<u8>().ok());
    }
    None
}

/// The WHIP ingest lane the [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint)
/// owns: the registered ingest sessions plus the command receiver + shared
/// negotiation state. The unified driver steps it on the single shared socket.
pub(crate) struct IngestLane {
    pub(crate) commands: mpsc::Receiver<Command>,
    pub(crate) shared: Arc<WhipShared>,
    pub(crate) sessions: Vec<DrivenSession>,
}

impl WhipHandle {
    /// Build the handle + command receiver + shared state from a config and the
    /// gathered host candidates, **without binding a socket** — used by both
    /// [`WhipEndpoint::bind`] and the single-socket
    /// [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) (ADR-0048 §4).
    pub(crate) fn build(
        config: &EndpointConfig,
        host_candidates: Vec<SocketAddr>,
    ) -> (Self, mpsc::Receiver<Command>, Arc<WhipShared>) {
        let (tx, rx) = mpsc::channel(64);
        let shared = Arc::new(WhipShared {
            commands: tx,
            host_candidates,
            table: std::sync::Mutex::new(SessionTable::new(
                config.max_sessions,
                config.session_idle_timeout,
                config.tombstone_ttl,
            )),
            live_by_source: std::sync::Mutex::new(HashMap::new()),
            learned_relays: std::sync::Mutex::new(Vec::new()),
        });
        let handle = Self {
            inner: Arc::clone(&shared),
        };
        (handle, rx, shared)
    }

    /// Negotiate a WHIP ingest session for `source_id` from the publisher's SDP
    /// `offer`, with `audio` controlling whether the Opus m-line is accepted.
    ///
    /// Vanilla ICE: the session gathers **all** host (+ advertised) candidates
    /// before answering, so the answer is complete and trickle is unnecessary
    /// (ADR-T014 §2's `405 PATCH`). Returns the answer SDP, the minted session
    /// id, and the per-session RTP ring; the session is registered with the
    /// driver task to receive media.
    ///
    /// # Errors
    ///
    /// * [`WebRtcError::PublisherConflict`] — a live publisher already holds the
    ///   source (`409`).
    /// * [`WebRtcError::MalformedSdp`] / [`WebRtcError::NoCompatibleCodec`] —
    ///   the offer (`400` / `406`).
    /// * [`WebRtcError::Transport`] — the driver is gone or registration failed
    ///   (`503`).
    pub fn negotiate(&self, source_id: &str, offer: &str, audio: bool) -> Result<WhipNegotiated> {
        // One publisher per source: a live session for this source is a 409.
        {
            let live = self
                .inner
                .live_by_source
                .lock()
                .map_err(|_| WebRtcError::Transport("whip source map poisoned".to_owned()))?;
            if live.contains_key(source_id) {
                return Err(WebRtcError::PublisherConflict(source_id.to_owned()));
            }
        }

        let now = Instant::now();
        let mut cfg = SessionConfig::ingest();
        cfg.enable_opus = audio;
        let mut session = Session::new(&cfg, now);
        // Gather host + advertised candidates (IPv6-first) before answering. An
        // **unspecified** bind address (`[::]` / `0.0.0.0`) is never a valid ICE
        // candidate — str0m rejects it — so it is skipped here; concrete
        // reachability comes from `advertised_addresses` (NAT 1:1 / Docker) and
        // any concrete gathered host address.
        let mut gathered = 0usize;
        for addr in &self.inner.host_candidates {
            if addr.ip().is_unspecified() {
                continue;
            }
            session.add_host_candidate(*addr)?;
            gathered += 1;
        }
        // TURN relay candidates the driver's in-crate TURN client has allocated
        // (ADR-0048 §5.1) — the operator's NAT-traversal last resort, offered
        // alongside the host candidates (str0m orders relay lowest). The relayed
        // traffic egresses the local bound socket. A learned relay does NOT count
        // toward the reachable-candidate floor below (host/advertised do): a relay
        // alone with no advertised host is a valid, if relay-only, answer.
        if let Ok(relays) = self.inner.learned_relays.lock() {
            for relay in relays.iter() {
                // `local` is the bound socket addr; the unspecified bind addr is
                // not a valid local — skip relay registration until a concrete
                // advertised host exists (the common deploy sets one).
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
            // No reachable candidate could be offered (only the unspecified bind
            // address, no advertised addresses): the publisher could never reach
            // us, so refuse rather than answer an unconnectable session. Nothing
            // has been admitted yet (admission is below), so there is nothing to
            // roll back.
            return Err(WebRtcError::Config(
                "no reachable ICE candidate: set webrtc.advertised_addresses".to_owned(),
            ));
        }
        let answer_sdp = session.accept_offer(offer)?;

        // Admit the session id outside the viewer pool (ingest is never capped by
        // the viewer pool — ADR-0048 §8) and mint a >=128-bit id.
        let session_id = {
            let mut table =
                self.inner.table.lock().map_err(|_| {
                    WebRtcError::Transport("whip session table poisoned".to_owned())
                })?;
            table.admit(SessionRole::IngestPublisher, now)?
        };
        {
            let mut live = self
                .inner
                .live_by_source
                .lock()
                .map_err(|_| WebRtcError::Transport("whip source map poisoned".to_owned()))?;
            live.insert(source_id.to_owned(), session_id.clone());
        }

        let ring = RtpRing::new();
        let driven = DrivenSession {
            id: session_id.clone(),
            source_id: source_id.to_owned(),
            session,
            ring: ring.clone(),
            was_connected: false,
            last_pli: None,
        };
        // Register with the driver; a full/closed channel means the endpoint is
        // gone — undo the bookkeeping and report unavailability (503).
        if self
            .inner
            .commands
            .try_send(Command::Register(Box::new(driven)))
            .is_err()
        {
            self.forget_source(source_id);
            return Err(WebRtcError::AtCapacity);
        }
        let video_payload_type = answer_payload_type(&answer_sdp, "video");
        let audio_payload_type = answer_payload_type(&answer_sdp, "audio");
        Ok(WhipNegotiated {
            session_id,
            answer_sdp,
            ring,
            video_payload_type,
            audio_payload_type,
        })
    }

    /// Release the session `session_id` for `source_id` (WHIP `DELETE`).
    ///
    /// Idempotent: returns `true` when a matching live session was found and a
    /// teardown was dispatched, `false` for an unknown/already-released session
    /// (the route maps that to a `404` for a never-known id, `200` within the
    /// tombstone window).
    #[must_use]
    pub fn release(&self, source_id: &str, session_id: &str) -> bool {
        let id = SessionId::from_str(session_id);
        let known = {
            match self.inner.live_by_source.lock() {
                Ok(live) => live.get(source_id) == Some(&id),
                Err(_) => false,
            }
        };
        if !known {
            return false;
        }
        self.forget_source(source_id);
        // Best-effort dispatch; the driver also GCs idle sessions, so a dropped
        // command never leaks (the session idle-times out).
        let _ = self
            .inner
            .commands
            .try_send(Command::Release { session_id: id });
        true
    }

    /// The number of currently-live ingest publishers (telemetry / tests).
    #[must_use]
    pub fn live_publisher_count(&self) -> usize {
        self.inner.live_by_source.lock().map_or(0, |m| m.len())
    }

    fn forget_source(&self, source_id: &str) {
        if let Ok(mut live) = self.inner.live_by_source.lock() {
            let _ = live.remove(source_id);
        }
    }
}

/// The WHIP-ingest endpoint: the bound socket + the registered ingest sessions.
///
/// Build with [`WhipEndpoint::bind`], then drive its [`run`](WhipEndpoint::run)
/// loop on a tokio task; negotiate/release sessions through the returned
/// [`WhipHandle`].
pub struct WhipEndpoint {
    endpoint: WebRtcEndpoint,
    commands: mpsc::Receiver<Command>,
    shared: Arc<WhipShared>,
}

impl std::fmt::Debug for WhipEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhipEndpoint")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl WhipEndpoint {
    /// Bind the dual-stack socket and create the endpoint + its control handle.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(config: EndpointConfig) -> Result<(Self, WhipHandle)> {
        let endpoint = WebRtcEndpoint::bind(config)?;
        let host_candidates = endpoint.host_candidates()?;
        let (handle, commands, shared) = WhipHandle::build(endpoint.config(), host_candidates);
        Ok((
            Self {
                endpoint,
                commands,
                shared,
            },
            handle,
        ))
    }

    /// Run the standalone driver loop until `stop` is raised, owning the socket
    /// from [`bind`](WhipEndpoint::bind). Retained for direct use/tests; the cli
    /// runs every WebRTC role on ONE socket through
    /// [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) (ADR-0048 §4). The
    /// live socket loop is hardware-gated; it never blocks the engine.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run(self, stop: Arc<AtomicBool>) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        // Build a TURN client per configured TURN server (ADR-0048 §5.1). Each is
        // sans-IO: it is driven over the same UDP socket as the media. A relay it
        // allocates is published into `shared.learned_relays` so future
        // negotiations offer it as a relay candidate (the operator's hard
        // NAT-traversal requirement, live in the driver — not just crate-level).
        let now0 = Instant::now();
        let mut turn = TurnRelayDriver::from_config(self.endpoint.config(), now0);
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

        let mut sessions: Vec<DrivenSession> = Vec::new();
        let mut commands = self.commands;
        let mut buf = vec![0u8; RECV_BUFFER];
        let mut tick = tokio::time::interval(DRIVER_TICK);

        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::select! {
                // A new register/release command.
                cmd = commands.recv() => {
                    if !Self::apply_command(&mut sessions, cmd) {
                        return Ok(()); // all handles dropped.
                    }
                }
                // An incoming datagram: a TURN-server reply, or media for a session.
                recv = socket.recv_from(&mut buf) => {
                    let now = Instant::now();
                    if let Ok((len, src)) = recv {
                        if let Some(payload) = buf.get(..len) {
                            Self::on_inbound(
                                &mut sessions, &mut turn, src, local_addr, payload, now,
                            );
                        }
                    }
                    Self::pump_turn(&socket, &mut turn, now, &self.shared).await;
                    Self::pump_outbound(&socket, &mut sessions, &mut turn, now).await;
                }
                // The idle tick: advance timers, drain RTP, drive TURN, GC.
                _ = tick.tick() => {
                    let now = Instant::now();
                    Self::tick(&mut sessions, now);
                    Self::pump_turn(&socket, &mut turn, now, &self.shared).await;
                    Self::pump_outbound(&socket, &mut sessions, &mut turn, now).await;
                    Self::reap(&mut sessions, &self.shared);
                }
            }
        }
    }

    /// Apply a register/release command to the ingest session set. Returns `false`
    /// when the command channel closed (all handles dropped) so the driver exits.
    pub(crate) fn apply_command(sessions: &mut Vec<DrivenSession>, cmd: Option<Command>) -> bool {
        match cmd {
            Some(Command::Register(driven)) => {
                sessions.push(*driven);
                true
            }
            Some(Command::Release { session_id }) => {
                for s in sessions.iter_mut().filter(|s| s.id == session_id) {
                    s.session.disconnect();
                    s.ring.close();
                }
                true
            }
            None => false,
        }
    }

    /// Classify and route one inbound datagram (relay-aware, defect C): a relayed
    /// Data indication is decapsulated to the inner media (fed as arriving on the
    /// relay addr); a TURN-server control reply feeds the relay driver; any other
    /// datagram is media for the session demux.
    pub(crate) fn on_inbound(
        sessions: &mut [DrivenSession],
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
            } => Self::route_datagram(sessions, peer, relay, &payload, now),
            crate::transport::relay_io::Inbound::TurnControl => {}
            crate::transport::relay_io::Inbound::Media => {
                Self::route_datagram(sessions, src, local_addr, payload, now);
            }
        }
    }

    /// One idle-tick step: advance each session's ICE/DTLS timers, drain its RTP
    /// ring, and send a rate-limited PLI (the outbound + GC are pumped by caller).
    pub(crate) fn tick(sessions: &mut [DrivenSession], now: Instant) {
        for s in sessions.iter_mut() {
            let _ = s.session.handle_timeout(now);
            s.was_connected |= s.session.is_connected();
            s.ring.drain_from(&mut s.session);
            Self::maybe_pli(s, now);
        }
    }

    /// Route one received datagram to the first session that accepts it (the
    /// str0m ufrag/peer demux), drain its RTP, and note connection.
    pub(crate) fn route_datagram(
        sessions: &mut [DrivenSession],
        src: SocketAddr,
        local: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        for s in sessions.iter_mut() {
            // `handle_datagram` silently ignores a datagram not for this session
            // (ufrag/peer demux miss), so trying each session is correct and
            // cheap; the accepting one consumes it.
            let before = s.session.received_rtp_count();
            let _ = s.session.handle_datagram(src, local, payload, now);
            s.was_connected |= s.session.is_connected();
            let moved = s.session.received_rtp_count() != before;
            s.ring.drain_from(&mut s.session);
            if moved {
                break;
            }
        }
    }

    /// Drain every session's outbound datagrams onto the socket, routing each
    /// through the TURN relay when str0m chose the relay candidate (source == relay
    /// addr → TURN Send indication; defect C). Non-blocking; a send error drops the
    /// datagram — never blocks the loop.
    pub(crate) async fn pump_outbound(
        socket: &tokio::net::UdpSocket,
        sessions: &mut [DrivenSession],
        turn: &mut TurnRelayDriver,
        now: Instant,
    ) {
        for s in sessions.iter_mut() {
            while let Some((source, dst, payload)) = s.session.poll_transmit(now) {
                // A send error (e.g. unreachable) is dropped: the publisher's
                // own retransmit/ICE recovers it; the loop never blocks.
                crate::transport::relay_io::send_routed(socket, turn, source, dst, &payload, now)
                    .await;
            }
        }
    }

    /// Drive the shared TURN relay driver's sans-IO output: send each queued
    /// datagram to its TURN server (allocate/refresh/retransmit) and publish any
    /// relay it harvested into `shared.learned_relays` for future negotiations.
    /// Non-blocking; a send error is dropped (the client retransmits).
    pub(crate) async fn pump_turn(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        now: Instant,
        shared: &WhipShared,
    ) {
        while let Some((destination, payload)) = turn.poll_transmit(now) {
            let _ = socket.send_to(&payload, destination).await;
        }
        // Publish any relay learned since the last pass (the driver de-dups, so
        // each relay is handed out once).
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

    /// Send a rate-limited PLI toward a session's publisher while its video
    /// keyframe gate is presumably closed (best-effort recovery, ADR-T014 §7).
    /// We PLI on connect and then at most once per [`PLI_FLOOR`]; the keyframe
    /// gate downstream holds delta frames until the IDR regardless.
    pub(crate) fn maybe_pli(s: &mut DrivenSession, now: Instant) {
        if !s.session.is_connected() {
            return;
        }
        let due = match s.last_pli {
            Some(prev) => now.saturating_duration_since(prev) >= PLI_FLOOR,
            None => true,
        };
        if due && s.session.request_video_keyframe(now) {
            s.last_pli = Some(now);
        }
    }

    /// Remove sessions whose `Rtc` has died (ICE/DTLS failed or disconnected),
    /// closing their ring and freeing the per-source slot.
    pub(crate) fn reap(sessions: &mut Vec<DrivenSession>, shared: &WhipShared) {
        sessions.retain_mut(|s| {
            if s.session.is_alive() {
                return true;
            }
            s.ring.close();
            if let Ok(mut live) = shared.live_by_source.lock() {
                if live.get(&s.source_id) == Some(&s.id) {
                    let _ = live.remove(&s.source_id);
                }
            }
            if let Ok(mut table) = shared.table.lock() {
                table.close(&s.id, Instant::now());
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Instant;

    use crate::config::{EndpointConfig, IceServer, TurnCredentials};
    use crate::turn::TurnRelayDriver;

    #[test]
    fn turn_driver_built_per_turn_server_stun_skipped() {
        // The WHIP endpoint drives its TURN clients through the shared
        // `TurnRelayDriver`: a STUN server needs no client (str0m gathers srflx
        // from bound/advertised addresses); a TURN server yields one driven
        // client.
        let config = EndpointConfig {
            ice_servers: vec![
                IceServer::stun("[2001:db8::53]:3478".parse().unwrap()),
                IceServer::turn(
                    "[2001:db8::55]:3478".parse().unwrap(),
                    TurnCredentials::long_term("u", "p"),
                ),
            ],
            ..EndpointConfig::default()
        };
        let driver = TurnRelayDriver::from_config(&config, Instant::now());
        assert_eq!(
            driver.client_count(),
            1,
            "one client for the one TURN server"
        );
    }

    #[test]
    fn no_turn_servers_yields_an_empty_driver() {
        let config = EndpointConfig {
            ice_servers: vec![IceServer::stun("[2001:db8::53]:3478".parse().unwrap())],
            ..EndpointConfig::default()
        };
        assert!(TurnRelayDriver::from_config(&config, Instant::now()).is_empty());
    }
}
