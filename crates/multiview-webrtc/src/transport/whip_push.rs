//! The `whip_push` **output client** — **feature `native`** (ADR-0049 §5.2,
//! ADR-0048 §7).
//!
//! Multiview is the RFC 9725 **client**: it publishes the program to a remote
//! WHIP ingest. The client builds a **sendonly** offer with host candidates +
//! `a=setup:actpass` (the answerer chooses the DTLS role — RFC 5763 answerers
//! typically pick `active`, so Multiview commonly runs the DTLS **server** role),
//! `POST`s it with the configured Bearer, applies the answer, and sample-writes
//! the program AUs drained from the shared
//! [`EgressFeed`](crate::egress::EgressFeed). It is **supervised with backoff
//! reconnect** exactly like the RTMP/SRT push clients.
//!
//! ## What is pure vs live
//!
//! [`WhipPushOffer`] (the sendonly-actpass-host-candidates offer) and
//! [`PushBackoff`] (the supervised reconnect schedule) are **pure** and CI-tested
//! offline; the offer→answer→connected lifecycle and media egress are proven in
//! memory over the `Session` shuttle (`tests/egress_native.rs`). The HTTP `POST`
//! to a real WHIP origin + the live UDP socket loop ([`WhipPushClient::run`]) are
//! the live legs, hardware-gated (a real OBS/MediaMTX ingest), not run in CI.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The push client is a fan-out consumer: program AUs cross only the bounded
//! drop-oldest [`EgressFeed`], and the driver never `.await`s the remote (UDP
//! send is non-blocking). A stalled or dead WHIP target loses *its* media and
//! triggers a supervised reconnect; it can never stall the encode-once fan-out or
//! the output clock. PTS are re-stamped from the tick at encode (invariant #3).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::egress::{EgressFeed, EgressMedia};
use crate::error::{Result, WebRtcError};
use crate::transport::{MediaKind, Session, SessionConfig, WebRtcEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the push driver wakes to advance timers, drain the feed, and send.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// The maximum HTTP redirect depth the WHIP `POST` follows (ADR-0049 §5.2:
/// 307/308, https-only, depth-capped at 3).
pub const MAX_REDIRECTS: u8 = 3;

/// A `whip_push` client SDP offer: a sendonly H.264 (+ optional Opus) offer with
/// host candidates and `a=setup:actpass`, built with no socket (str0m is
/// sans-IO). The driver `POST`s [`WhipPushOffer::sdp`] to the remote WHIP origin.
#[derive(Debug)]
pub struct WhipPushOffer {
    /// The offer SDP string.
    pub sdp: String,
    /// The underlying sans-IO session awaiting the remote's answer
    /// ([`Session::accept_answer`]) — the driver owns it across the handshake.
    pub session: Session,
}

impl WhipPushOffer {
    /// Build the sendonly publish offer for `host_candidates` (IPv6-first), with
    /// an Opus `m=audio` line iff `audio`.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if a candidate is invalid or the change set
    /// produced no offer.
    pub fn create(host_candidates: &[SocketAddr], audio: bool) -> Result<Self> {
        let now = Instant::now();
        let mut session = Session::new(&SessionConfig::push(), now);
        let mut gathered = 0usize;
        for addr in host_candidates {
            if addr.ip().is_unspecified() {
                continue;
            }
            session.add_host_candidate(*addr)?;
            gathered += 1;
        }
        if gathered == 0 {
            return Err(WebRtcError::Config(
                "whip_push has no reachable host candidate: set webrtc.advertised_addresses"
                    .to_owned(),
            ));
        }
        let kinds: &[MediaKind] = if audio {
            &[MediaKind::Video, MediaKind::Audio]
        } else {
            &[MediaKind::Video]
        };
        // The push client is the offerer: sendonly + str0m's default actpass.
        let sdp = session.create_offer(kinds)?;
        Ok(Self { sdp, session })
    }
}

/// The supervised reconnect schedule for a `whip_push` client — exponential
/// backoff with a floor and a cap, reset on a successful connect (mirrors the
/// RTMP/SRT push supervision).
#[derive(Debug)]
pub struct PushBackoff {
    /// The current delay; doubles each failure, capped at [`Self::MAX_DELAY`].
    current: Duration,
}

impl PushBackoff {
    /// The initial (and floor) retry delay.
    pub const MIN_DELAY: Duration = Duration::from_millis(500);
    /// The ceiling the backoff never exceeds (a dead origin retries at most this
    /// slowly — bounded, never unbounded).
    pub const MAX_DELAY: Duration = Duration::from_secs(30);

    /// A fresh backoff at the floor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: Self::MIN_DELAY,
        }
    }

    /// The next retry delay, then double it (capped at [`Self::MAX_DELAY`]).
    #[must_use]
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current.saturating_mul(2)).min(Self::MAX_DELAY);
        delay
    }

    /// Reset to the floor after a successful connect.
    pub fn reset(&mut self) {
        self.current = Self::MIN_DELAY;
    }
}

impl Default for PushBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// The live `whip_push` client endpoint: the bound socket + the publish session.
///
/// Build with [`WhipPushClient::bind`], then drive its [`run`](Self::run) loop on
/// a tokio task. The HTTP signalling against the remote origin is performed by a
/// [`WhipSignaller`] the cli supplies (control owns the reqwest dependency, not
/// this crate); the client drives ICE/DTLS/SRTP and the program egress.
pub struct WhipPushClient {
    endpoint: WebRtcEndpoint,
    feed: EgressFeed,
    audio: bool,
}

/// The remote answer + session resource the WHIP `POST` resolved.
#[derive(Debug, Clone)]
pub struct WhipPushAnswer {
    /// The remote's SDP answer body.
    pub answer_sdp: String,
    /// The session resource URL the client `DELETE`s to stop (resolved against
    /// the post-redirect effective URL, ADR-0049 §5.2).
    pub resource_url: Option<String>,
}

/// The HTTP signalling seam the cli implements over reqwest: `POST` the offer to
/// the remote WHIP origin (Bearer, https-only 307/308 redirects preserving method
/// + headers, depth-capped) and resolve the answer + session `Location`.
///
/// Kept as a trait so this crate never grows an HTTP-client dependency (the same
/// posture as the control `WhipProvider` seam) and the lifecycle is unit-tested
/// with an in-memory shuttle.
pub trait WhipSignaller: Send + Sync {
    /// `POST` `offer_sdp` to the configured WHIP URL with the optional Bearer
    /// `token`, returning the remote's answer + session resource.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] on a network / HTTP error, an https→http
    /// downgrade, exceeding the redirect depth, or a non-`201` status.
    fn post_offer(&self, offer_sdp: &str) -> Result<WhipPushAnswer>;

    /// `DELETE` the session resource (best-effort teardown). A failure is logged,
    /// never propagated (the remote also times the session out).
    fn delete_resource(&self, resource_url: &str);
}

/// What one `whip_push` output needs to publish on the **shared** socket via the
/// [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) (ADR-0048 §4): the program
/// egress feed, whether it carries audio, and the host candidates (the one shared
/// socket's reachable addresses, IPv6-first) the sendonly offer advertises.
#[derive(Debug, Clone)]
pub struct WhipPushSpec {
    /// The bounded drop-oldest program egress feed the client publishes.
    pub feed: EgressFeed,
    /// Whether the output carries the shared program Opus rendition.
    pub audio: bool,
    /// The host candidates the sendonly offer advertises (IPv6-first).
    pub host_candidates: Vec<SocketAddr>,
}

/// One `whip_push` output's lifecycle on the shared socket: a supervised state
/// machine the [`UnifiedEndpoint`](crate::transport::UnifiedEndpoint) steps each
/// driver tick. It mirrors the standalone [`WhipPushClient::run`] loop — build a
/// sendonly offer, `POST` it (off-thread so the one driver never blocks), apply the
/// answer, then drive ICE/DTLS/SRTP + sample-write the program AUs — but it shares
/// the one socket instead of binding its own (the defect-B fix).
pub struct PushLane {
    spec: WhipPushSpec,
    signaller: Arc<dyn WhipSignaller>,
    backoff: PushBackoff,
    state: PushState,
}

/// The supervised `whip_push` lifecycle state.
enum PushState {
    /// Not connected; the next connect attempt is due at `retry_at`.
    Idle { retry_at: Instant },
    /// A WHIP `POST` is in flight off-thread; `join` resolves to the answer.
    Connecting {
        session: Box<Session>,
        join: tokio::task::JoinHandle<Result<WhipPushAnswer>>,
    },
    /// Connected: driving ICE/DTLS/SRTP + publishing. `resource_url` is the
    /// session resource the client `DELETE`s on teardown.
    Connected {
        session: Box<Session>,
        resource_url: Option<String>,
        saw_keyframe: bool,
    },
}

impl std::fmt::Debug for PushLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = match &self.state {
            PushState::Idle { .. } => "idle",
            PushState::Connecting { .. } => "connecting",
            PushState::Connected { .. } => "connected",
        };
        f.debug_struct("PushLane")
            .field("audio", &self.spec.audio)
            .field("phase", &phase)
            .finish_non_exhaustive()
    }
}

impl PushLane {
    /// Build a lane from its spec + the HTTP signaller (boxed so the crate keeps no
    /// HTTP dependency). The first connect attempt is due immediately.
    #[must_use]
    pub fn new(spec: WhipPushSpec, signaller: Box<dyn WhipSignaller>) -> Self {
        Self {
            spec,
            signaller: Arc::from(signaller),
            backoff: PushBackoff::new(),
            state: PushState::Idle {
                retry_at: Instant::now(),
            },
        }
    }

    /// Advance the lane one driver tick on the shared `socket` (relay-aware via
    /// `turn`). Non-blocking: a due connect dispatches the WHIP `POST` off-thread;
    /// a live session drains the feed and flushes outbound; a dead/failed session
    /// schedules a backoff reconnect. Never `.await`s the remote peer (inv #10).
    pub async fn step(
        &mut self,
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        local_addr: SocketAddr,
        now: Instant,
    ) {
        // Take the state out to transition it (placed back before returning).
        let state = std::mem::replace(
            &mut self.state,
            PushState::Idle {
                retry_at: now + PushBackoff::MAX_DELAY,
            },
        );
        self.state = match state {
            PushState::Idle { retry_at } => {
                if now < retry_at {
                    PushState::Idle { retry_at }
                } else {
                    self.begin_connect(now)
                }
            }
            PushState::Connecting { session, join } => self.poll_connect(session, join, now).await,
            PushState::Connected {
                mut session,
                resource_url,
                mut saw_keyframe,
            } => {
                let _ = local_addr;
                WhipPushClient::drain_and_send(
                    socket,
                    turn,
                    &mut session,
                    &self.spec.feed,
                    &mut saw_keyframe,
                    now,
                )
                .await;
                if session.is_alive() {
                    PushState::Connected {
                        session,
                        resource_url,
                        saw_keyframe,
                    }
                } else {
                    if let Some(url) = &resource_url {
                        self.signaller.delete_resource(url);
                    }
                    PushState::Idle {
                        retry_at: now + self.backoff.next_delay(),
                    }
                }
            }
        };
    }

    /// Feed one inbound datagram (already relay-decapsulated by the unified driver)
    /// to a connecting/connected push session — str0m ignores a datagram not for it.
    pub fn handle_inbound(
        &mut self,
        src: SocketAddr,
        dst: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        match &mut self.state {
            PushState::Connected { session, .. } | PushState::Connecting { session, .. } => {
                let _ = session.handle_datagram(src, dst, payload, now);
            }
            PushState::Idle { .. } => {}
        }
    }

    /// Build a sendonly offer and dispatch the (blocking) WHIP `POST` off-thread.
    fn begin_connect(&mut self, now: Instant) -> PushState {
        let Ok(offer) = WhipPushOffer::create(&self.spec.host_candidates, self.spec.audio) else {
            return PushState::Idle {
                retry_at: now + self.backoff.next_delay(),
            };
        };
        let signaller = Arc::clone(&self.signaller);
        let offer_sdp = offer.sdp;
        let join = tokio::task::spawn_blocking(move || signaller.post_offer(&offer_sdp));
        PushState::Connecting {
            session: Box::new(offer.session),
            join,
        }
    }

    /// Resolve the off-thread `POST` if it has finished; on success apply the
    /// answer and go Connected, on failure back off and reconnect. A still-running
    /// `POST` keeps the Connecting state (the unified driver re-steps next tick).
    /// `JoinHandle::is_finished` lets us check without awaiting (no blocking on the
    /// one driver), then a finished handle is awaited (it resolves immediately).
    async fn poll_connect(
        &mut self,
        mut session: Box<Session>,
        join: tokio::task::JoinHandle<Result<WhipPushAnswer>>,
        now: Instant,
    ) -> PushState {
        if !join.is_finished() {
            return PushState::Connecting { session, join };
        }
        // Finished: awaiting resolves immediately (no blocking the driver).
        match join.await {
            Ok(Ok(answer)) => {
                if session.accept_answer(&answer.answer_sdp).is_err() {
                    PushState::Idle {
                        retry_at: now + self.backoff.next_delay(),
                    }
                } else {
                    self.backoff.reset();
                    PushState::Connected {
                        session,
                        resource_url: answer.resource_url,
                        saw_keyframe: false,
                    }
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "whip_push POST failed; backing off");
                PushState::Idle {
                    retry_at: now + self.backoff.next_delay(),
                }
            }
            Err(join_err) => {
                tracing::warn!(error = %join_err, "whip_push POST task failed");
                PushState::Idle {
                    retry_at: now + self.backoff.next_delay(),
                }
            }
        }
    }
}

impl WhipPushClient {
    /// Bind the dual-stack socket and create the client over the program egress
    /// `feed`. `audio` is whether the output carries the program Opus rendition.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(
        config: crate::config::EndpointConfig,
        feed: EgressFeed,
        audio: bool,
    ) -> Result<Self> {
        let endpoint = WebRtcEndpoint::bind(config)?;
        Ok(Self {
            endpoint,
            feed,
            audio,
        })
    }

    /// The host candidates the publish offer advertises (IPv6-first).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        self.endpoint.host_candidates()
    }

    /// Run the supervised publish loop until `stop`: build a sendonly offer, hand
    /// it to `signaller` for the WHIP `POST`, apply the answer, then drive
    /// ICE/DTLS/SRTP and sample-write the program AUs drained from the feed. On a
    /// drop or signalling failure, back off (bounded) and reconnect. The live
    /// socket loop is hardware-gated; it never blocks the engine.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run<S: WhipSignaller>(self, signaller: S, stop: Arc<AtomicBool>) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        let host_candidates = self.endpoint.host_candidates()?;
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

        let mut backoff = PushBackoff::new();
        let mut buf = vec![0u8; RECV_BUFFER];

        while !stop.load(Ordering::Acquire) {
            // Build a fresh sendonly offer + negotiate with the remote origin.
            let Ok(offer) = WhipPushOffer::create(&host_candidates, self.audio) else {
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            };
            let mut session = offer.session;
            let answer = match signaller.post_offer(&offer.sdp) {
                Ok(answer) => answer,
                Err(err) => {
                    tracing::warn!(error = %err, "whip_push POST failed; backing off");
                    tokio::time::sleep(backoff.next_delay()).await;
                    continue;
                }
            };
            if session.accept_answer(&answer.answer_sdp).is_err() {
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
            backoff.reset();
            let mut saw_keyframe = false;
            let mut tick = tokio::time::interval(DRIVER_TICK);

            // Drive this session until ICE/DTLS dies or stop is raised.
            loop {
                if stop.load(Ordering::Acquire) {
                    if let Some(url) = &answer.resource_url {
                        signaller.delete_resource(url);
                    }
                    return Ok(());
                }
                tokio::select! {
                    recv = socket.recv_from(&mut buf) => {
                        let now = Instant::now();
                        if let Ok((len, src)) = recv {
                            if let Some(payload) = buf.get(..len) {
                                let _ = session.handle_datagram(src, local_addr, payload, now);
                            }
                        }
                        Self::drain_and_send(&socket, &mut turn, &mut session, &self.feed, &mut saw_keyframe, now).await;
                    }
                    _ = tick.tick() => {
                        let now = Instant::now();
                        let _ = session.handle_timeout(now);
                        Self::drain_and_send(&socket, &mut turn, &mut session, &self.feed, &mut saw_keyframe, now).await;
                    }
                }
                if !session.is_alive() {
                    // The remote dropped: tear the resource down (best-effort) and
                    // reconnect with backoff (supervised, like RTMP/SRT push).
                    if let Some(url) = &answer.resource_url {
                        signaller.delete_resource(url);
                    }
                    break;
                }
            }
            tokio::time::sleep(backoff.next_delay()).await;
        }
        Ok(())
    }

    /// Drain the egress feed and sample-write each program AU into the session,
    /// then flush its outbound datagrams onto the socket, routing through the TURN
    /// relay when str0m chose the relay candidate (defect C). Non-blocking.
    async fn drain_and_send(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        session: &mut Session,
        feed: &EgressFeed,
        saw_keyframe: &mut bool,
        now: Instant,
    ) {
        if session.is_connected() {
            for _ in 0..256 {
                let Some(sample) = feed.pop() else { break };
                match sample.media {
                    EgressMedia::Video => {
                        if sample.keyframe {
                            *saw_keyframe = true;
                        }
                        if !*saw_keyframe {
                            continue;
                        }
                        let _ = session.write_video_sample(
                            &sample.data,
                            sample.keyframe,
                            sample.rtp_timestamp,
                            now,
                        );
                    }
                    EgressMedia::Audio => {
                        let _ = session.write_audio_sample(&sample.data, sample.rtp_timestamp, now);
                    }
                }
            }
        }
        while let Some((source, dst, payload)) = session.poll_transmit(now) {
            crate::transport::relay_io::send_routed(socket, turn, source, dst, &payload, now).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn backoff_caps_and_resets() {
        let mut b = PushBackoff::new();
        assert_eq!(b.next_delay(), PushBackoff::MIN_DELAY);
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= PushBackoff::MAX_DELAY);
        }
        b.reset();
        assert_eq!(b.next_delay(), PushBackoff::MIN_DELAY);
    }
}
