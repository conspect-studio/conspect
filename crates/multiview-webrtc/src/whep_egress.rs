//! The native **WHEP preview egress** transport (feature `native`, ADR-P006).
//!
//! This relocates the preview-local str0m duplication onto the crate's single
//! str0m [`Session`](crate::transport::Session) (ADR-0048 / ADR-P006 move 1):
//! WHEP preview viewers, WHEP output viewers, and WHIP ingest all ride one str0m
//! owner, one DTLS certificate, and one dual-stack UDP socket. [`WhepEgress`]
//! implements the pure `multiview_preview::whep::transport::WhepTransport` seam,
//! so the control plane stays codec/native-free and only the cli (which already
//! links this crate behind a feature) wires the live path.
//!
//! ## What it does
//!
//! * [`WhepEgress::accept_session`] — parse the browser WHEP offer with str0m
//!   (real ICE credentials + a real self-signed DTLS fingerprint), take the
//!   source's bounded **drop-oldest** video [`SampleFeed`] (and, when the offer
//!   negotiated an Opus audio m-line, its audio feed — ADR-P006), and return an
//!   [`EgressAnswer`] carrying both the preview-core [`TransportAnswer`] *and*
//!   str0m's own complete answer SDP (BUNDLE / mid / rtcp-mux / fmtp — ADR-P006
//!   move 2, never a hand-rolled rebuild).
//! * [`WhepEgress::drive_egress`] — the egress step the endpoint driver pumps:
//!   drain the drop-oldest feeds → [`Session::write_video_sample`] /
//!   [`Session::write_audio_sample`] → SRTP → the outbound datagrams the driver
//!   sends over the shared socket. The first video sample is gated on a keyframe
//!   so a late joiner decodes immediately.
//! * [`WhepEgress::handle_datagram`] / [`WhepEgress::poll_timeout`] — feed
//!   received datagrams in and report the next wake, so the endpoint driver can
//!   multiplex this session on the shared socket.
//! * [`WhepEgress::close`] — terminal lifecycle + immediate `Rtc`/feed release; a
//!   closed entry is kept as a queryable tombstone (idempotent WHEP `DELETE`).
//!
//! Because str0m is sans-IO, the whole path — handshake + SRTP egress — runs over
//! an in-memory packet shuttle with **no socket** and is unit-tested in CI
//! (`tests/whep_egress.rs`). The live browser-play leg is hardware-gated.
//!
//! ## Isolation (invariant #10 — the cardinal preview rule)
//!
//! A WHEP egress session is a *preview* consumer. It drains drop-oldest
//! [`SampleFeed`]s and owns only its `Rtc` plus a lifecycle handle; it never holds
//! a handle the engine awaits, never publishes onto the protected output path, and
//! a stalled or absent browser player merely loses the oldest buffered samples —
//! the producer pushing into the feed is never back-pressured, and other sessions
//! are unaffected. The session map is the transport's own mutex-guarded
//! bookkeeping, never an engine lock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;

use multiview_preview::whep::transport::{
    DtlsFingerprint, DtlsSetup, PreviewMediaSource, SampleFeed, SampleKind, SessionHandle,
    SessionId, SessionState, TransportAnswer, WhepTransport,
};
use multiview_preview::whep::{PreviewCodec, WhepError};

use crate::transport::{Session, SessionConfig};

/// The structured ICE/DTLS attributes parsed out of str0m's answer SDP, plus the
/// candidate lines, mirroring the preview-core [`TransportAnswer`] (minus the
/// session id, which the transport mints independently of the SDP text).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AnswerAttributes {
    ice_ufrag: String,
    ice_pwd: String,
    fingerprint: DtlsFingerprint,
    setup: DtlsSetup,
    candidates: Vec<String>,
}

/// Parse the ICE / DTLS / candidate attributes out of an SDP **answer** string
/// (the shape [`Session::accept_offer`] produces).
///
/// Extracts `a=ice-ufrag`, `a=ice-pwd`, `a=fingerprint` (split into algorithm +
/// colon-hex value), the `a=setup` role (defaulting to `passive` — a WHEP egress
/// server is always the DTLS server), and every `a=candidate` line. ICE
/// ufrag/pwd and the DTLS fingerprint are required.
fn parse_answer_attributes(answer: &str) -> Result<AnswerAttributes, WhepError> {
    let mut ice_ufrag: Option<String> = None;
    let mut ice_pwd: Option<String> = None;
    let mut fingerprint: Option<DtlsFingerprint> = None;
    let mut setup = DtlsSetup::Passive;
    let mut candidates = Vec::new();

    for raw in answer.lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            ice_ufrag = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            ice_pwd = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("a=fingerprint:") {
            if let Some((algo, value)) = v.split_once(' ') {
                fingerprint = Some(DtlsFingerprint {
                    algorithm: algo.to_ascii_lowercase(),
                    value: value.trim().to_owned(),
                });
            }
        } else if let Some(v) = line.strip_prefix("a=setup:") {
            setup = match v.trim() {
                "active" => DtlsSetup::Active,
                _ => DtlsSetup::Passive,
            };
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            candidates.push(v.to_owned());
        }
    }

    let ice_ufrag = ice_ufrag.ok_or(WhepError::MalformedOffer {
        reason: "str0m answer has no a=ice-ufrag",
    })?;
    let ice_pwd = ice_pwd.ok_or(WhepError::MalformedOffer {
        reason: "str0m answer has no a=ice-pwd",
    })?;
    let fingerprint = fingerprint.ok_or(WhepError::MalformedOffer {
        reason: "str0m answer has no a=fingerprint",
    })?;

    Ok(AnswerAttributes {
        ice_ufrag,
        ice_pwd,
        fingerprint,
        setup,
        candidates,
    })
}

/// The result of [`WhepEgress::accept_session`]: the preview-core
/// [`TransportAnswer`] (folded into the codec scaffold by callers that want it)
/// plus **str0m's own complete answer SDP** (ADR-P006 move 2 — the WHEP HTTP
/// answer body is str0m's answer verbatim, never a hand-rolled rebuild).
#[derive(Debug, Clone)]
pub struct EgressAnswer {
    /// The transport-supplied ICE/DTLS attributes (the preview-core shape).
    pub transport: TransportAnswer,
    /// str0m's complete answer SDP — the `201 Created` body for a native session.
    pub sdp_answer: String,
}

/// One egress session: the str0m peer connection, its lifecycle handle, the
/// drop-oldest media feeds the egress pump drains, and the RTP timestamp cursors.
struct EgressSession {
    /// The sans-IO peer connection. Dropped on `close`; `None` once a tombstone.
    session: Option<Session>,
    handle: SessionHandle,
    /// The drop-oldest video tap the egress pump drains. Dropped on `close`.
    video: Option<SampleFeed>,
    /// The optional drop-oldest **Opus audio** tap (ADR-P006), present only when
    /// the offer negotiated an Opus audio m-line. Dropped on `close`.
    audio: Option<SampleFeed>,
    /// Whether ICE+DTLS have completed and a video keyframe has egressed — the
    /// first video sample is gated on a keyframe so a late joiner decodes.
    sent_video_keyframe: bool,
    /// The monotonic instant the str0m session was built. Every `now` fed to the
    /// session is clamped to be ≥ this, so a caller whose clock briefly lags the
    /// session's birth (e.g. a clock captured just before `accept_session`) cannot
    /// feed str0m a pre-birth timestamp and stall its ICE/DTLS timers. In
    /// production the live driver's `Instant::now()` is always ≥ birth, so the
    /// clamp is a no-op there; it only hardens the seam against clock skew.
    created_at: Instant,
}

/// A native WHEP egress transport backed by the crate's single str0m
/// [`Session`]. Implements `multiview_preview::whep::transport::WhepTransport`.
///
/// Construct a socket-free transport with [`Self::new`] (the CI shuttle path) or
/// one that gathers a host candidate with [`Self::with_host_candidate`]. The live
/// endpoint registers each session's host + TURN relay candidates and pumps
/// [`Self::drive_egress`] / [`Self::handle_datagram`] on the shared UDP socket.
///
/// Sessions are mutex-guarded bookkeeping the engine never touches (invariant
/// #10). `Send + Sync`.
pub struct WhepEgress {
    sessions: Mutex<HashMap<SessionId, EgressSession>>,
    /// The host candidate to gather on each session (the bound socket's reachable
    /// address). `None` for the socket-free shuttle transport.
    host_candidate: Option<SocketAddr>,
    /// Relay candidates (TURN-allocated) to register on each session, with the
    /// local socket the relayed traffic egresses from (ADR-0048 §5.1).
    ///
    /// Interior-mutable: a TURN `Allocate` completes asynchronously, *after* the
    /// transport is constructed and shared (`Arc<WhepEgress>`), so the driver's
    /// in-crate TURN client publishes each learned relay at runtime through
    /// [`Self::learn_relay`]. Every subsequently-negotiated session offers the
    /// current set as `typ relay` candidates — the operator's NAT-traversal path.
    relay_candidates: Mutex<Vec<(SocketAddr, SocketAddr)>>,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for WhepEgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.sessions.lock().map_or(0, |s| s.len());
        let relays = self.relay_candidates.lock().map_or(0, |r| r.len());
        f.debug_struct("WhepEgress")
            .field("sessions", &live)
            .field("host_candidate", &self.host_candidate)
            .field("relay_candidates", &relays)
            .finish_non_exhaustive()
    }
}

impl Default for WhepEgress {
    fn default() -> Self {
        Self::new()
    }
}

impl WhepEgress {
    /// Build a socket-free egress transport (the CI shuttle path: real SDP
    /// offer→answer with real ICE/DTLS, no host candidate).
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            host_candidate: None,
            relay_candidates: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
        }
    }

    /// Build an egress transport that gathers `host` as the host candidate on each
    /// accepted session (the bound socket's reachable address).
    #[must_use]
    pub fn with_host_candidate(host: SocketAddr) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            host_candidate: Some(host),
            relay_candidates: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
        }
    }

    /// Build an egress transport with a host candidate **and** seeded TURN relay
    /// candidates (the operator's NAT-traversal path, ADR-0048 §5.1). Each
    /// `(relayed, local)` pair is the address a TURN Allocate yielded and the
    /// local socket the relayed traffic egresses from. Further relays the driver
    /// learns at runtime are added with [`Self::learn_relay`].
    #[must_use]
    pub fn with_candidates(
        host: SocketAddr,
        relay_candidates: Vec<(SocketAddr, SocketAddr)>,
    ) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            host_candidate: Some(host),
            relay_candidates: Mutex::new(relay_candidates),
            next_id: Mutex::new(0),
        }
    }

    /// Publish a TURN **relay** candidate the egress driver's in-crate TURN client
    /// allocated at runtime (ADR-0048 §5.1): `relayed` is the address the TURN
    /// server allocated, `local` the socket the relayed traffic egresses from.
    ///
    /// Idempotent — a relay already known is not added twice (a re-allocation or a
    /// `poll_output`/`handle_input` both surfacing the same relay is harmless).
    /// Every subsequently-negotiated session offers the current set as `typ relay`
    /// candidates, so a browser behind NAT can WHEP-play via the operator's TURN
    /// relay. A poisoned lock is ignored (best-effort; the relay is simply not
    /// added — the session still answers with its host candidate).
    pub fn learn_relay(&self, relayed: SocketAddr, local: SocketAddr) {
        if let Ok(mut relays) = self.relay_candidates.lock() {
            if !relays.iter().any(|(r, l)| *r == relayed && *l == local) {
                relays.push((relayed, local));
            }
        }
    }

    /// The current lifecycle state of session `id`, or `None` if untracked.
    #[must_use]
    pub fn session_state(&self, id: &SessionId) -> Option<SessionState> {
        self.sessions
            .lock()
            .ok()
            .and_then(|m| m.get(id).map(|s| s.handle.state()))
    }

    /// Accept a WHEP `offer` for `codec`, wiring `media`'s drop-oldest feeds, and
    /// return the [`EgressAnswer`] (the transport attributes + str0m's answer SDP).
    ///
    /// # Errors
    ///
    /// [`WhepError::NoSupportedCodec`] if the offer's video m-line does not
    /// advertise `codec`; [`WhepError::MalformedOffer`] if str0m cannot parse /
    /// answer the offer or its answer lacks ICE/DTLS attributes.
    pub fn accept_session(
        &self,
        offer: &str,
        codec: PreviewCodec,
        media: &dyn PreviewMediaSource,
    ) -> Result<EgressAnswer, WhepError> {
        // Re-confirm the offer advertises the caller-selected video codec so an
        // audio-only or codec-mismatched offer is rejected before str0m media
        // negotiation (the preview core already selected `codec`).
        if !offer_advertises_video(offer, codec) {
            return Err(WhepError::NoSupportedCodec);
        }
        let opus = offer_advertises_opus(offer);

        let created_at = Instant::now();
        let mut session = Session::new(&SessionConfig::default(), created_at);
        if let Some(host) = self.host_candidate {
            session
                .add_host_candidate(host)
                .map_err(|_| WhepError::MalformedOffer {
                    reason: "egress could not gather a host candidate",
                })?;
        }
        // Offer every TURN relay the driver has learned so far (a snapshot taken
        // under a short-lived lock; the driver may add more between sessions).
        let relays = self
            .relay_candidates
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default();
        for (relayed, local) in relays {
            // A relay candidate that str0m rejects is non-fatal: it only loses
            // that reachability path, never the session.
            let _ = session.add_relay_candidate(relayed, local);
        }
        let sdp_answer = session
            .accept_offer(offer)
            .map_err(|_| WhepError::MalformedOffer {
                reason: "str0m rejected the WHEP offer (no usable media / ICE / DTLS)",
            })?;
        let attrs = parse_answer_attributes(&sdp_answer)?;

        let id = self.mint_id();
        let handle = SessionHandle::new(id.clone());
        // Take the media feeds exactly once. Audio is taken only when the offer
        // negotiated an Opus m-line — a feed that can never be sent is never held
        // (ADR-P006).
        let video = Some(media.feed());
        let audio = if opus { media.audio_feed() } else { None };

        let entry = EgressSession {
            session: Some(session),
            handle,
            video,
            audio,
            sent_video_keyframe: false,
            created_at,
        };
        match self.sessions.lock() {
            Ok(mut map) => {
                map.insert(id.clone(), entry);
            }
            Err(_) => {
                return Err(WhepError::MalformedOffer {
                    reason: "preview session map poisoned",
                });
            }
        }

        Ok(EgressAnswer {
            transport: TransportAnswer {
                session_id: id,
                ice_ufrag: attrs.ice_ufrag,
                ice_pwd: attrs.ice_pwd,
                fingerprint: attrs.fingerprint,
                setup: attrs.setup,
                candidates: attrs.candidates,
            },
            sdp_answer,
        })
    }

    /// Drive one egress step for session `id`: drain the drop-oldest media feeds
    /// into the str0m session (re-stamping each sample on its RTP clock), advance
    /// the session's timers, and return the outbound datagrams the driver sends.
    ///
    /// The first video sample is gated on a keyframe so a late joiner decodes
    /// immediately; audio frames are independently decodable and flow at once.
    /// A stalled consumer that never calls this only causes the feeds to drop the
    /// oldest samples — it never back-pressures the producer (invariant #10).
    ///
    /// # Errors
    ///
    /// [`WhepError::MalformedOffer`] if the bookkeeping mutex is poisoned. A
    /// closed/unknown session is **not** an error: it simply emits no datagrams.
    pub fn drive_egress(
        &self,
        id: &SessionId,
        now: Instant,
    ) -> Result<Vec<(SocketAddr, SocketAddr, Vec<u8>)>, WhepError> {
        let mut map = self
            .sessions
            .lock()
            .map_err(|_| WhepError::MalformedOffer {
                reason: "preview session map poisoned",
            })?;
        let Some(entry) = map.get_mut(id) else {
            return Ok(Vec::new());
        };
        // Clamp the driver clock to the session's birth so a caller whose clock
        // lags the str0m session's creation cannot feed a pre-birth timestamp and
        // stall ICE/DTLS (a no-op for the live driver's monotonic `Instant::now()`).
        let now = now.max(entry.created_at);
        let Some(session) = entry.session.as_mut() else {
            // A closed tombstone: nothing to send.
            return Ok(Vec::new());
        };

        // Only feed media once connected; before that, just advance timers (the
        // ICE/DTLS handshake). Writing before Connected drops the sample anyway.
        if session.is_connected() {
            // A keyframe-request from the peer (PLI/FIR) re-arms the keyframe gate
            // so the next sample re-syncs the decoder (rate-limiting is owned by
            // the producing encoder; here we only re-arm the gate).
            if session.take_keyframe_request() {
                entry.sent_video_keyframe = false;
            }
            // Drain video: gate the first delivered sample on a keyframe.
            if let Some(feed) = &entry.video {
                while let Some(sample) = feed.pop() {
                    debug_assert_eq!(sample.kind, SampleKind::Video);
                    if !entry.sent_video_keyframe {
                        if !sample.keyframe {
                            // Skip pre-keyframe samples so the decoder gets a sync
                            // point first (the feed is drop-oldest, so skipping is
                            // free and never grows memory).
                            continue;
                        }
                        entry.sent_video_keyframe = true;
                    }
                    // A write fault disconnects this session only, never the
                    // engine; preview is best-effort.
                    if session
                        .write_video_sample(
                            &sample.data,
                            sample.keyframe,
                            sample.rtp_timestamp,
                            now,
                        )
                        .is_err()
                    {
                        break;
                    }
                }
            }
            // Drain audio: independently decodable, so no keyframe gate.
            if let Some(feed) = &entry.audio {
                while let Some(sample) = feed.pop() {
                    debug_assert_eq!(sample.kind, SampleKind::Audio);
                    if session
                        .write_audio_sample(&sample.data, sample.rtp_timestamp, now)
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }

        // Advance the session's timers and collect everything it wants to send.
        if session.handle_timeout(now).is_err() {
            // A timer fault disconnects this session; report no datagrams.
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while let Some(dg) = session.poll_transmit(now) {
            out.push(dg);
        }
        // Reflect the str0m connection state onto the lifecycle handle (best
        // effort; an illegal transition is ignored — preview is best-effort).
        if session.is_connected() {
            let _ = entry.handle.advance_to(SessionState::Connected);
        }
        Ok(out)
    }

    /// Feed a received datagram into session `id`. `source` is the peer's remote
    /// address, `destination` the local socket address it arrived on (the shared
    /// dual-stack socket's local addr).
    ///
    /// # Errors
    ///
    /// [`WhepError::MalformedOffer`] if the bookkeeping mutex is poisoned. An
    /// unknown/closed session silently ignores the datagram.
    pub fn handle_datagram(
        &self,
        id: &SessionId,
        source: SocketAddr,
        destination: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Result<(), WhepError> {
        let mut map = self
            .sessions
            .lock()
            .map_err(|_| WhepError::MalformedOffer {
                reason: "preview session map poisoned",
            })?;
        let Some(entry) = map.get_mut(id) else {
            return Ok(());
        };
        // Clamp the driver clock to the session's birth (see `drive_egress`).
        let now = now.max(entry.created_at);
        let Some(session) = entry.session.as_mut() else {
            return Ok(());
        };
        // A datagram str0m cannot ingest disconnects nothing — it is silently
        // ignored (preview is best-effort).
        let _ = session.handle_datagram(source, destination, payload, now);
        Ok(())
    }

    /// The next instant session `id` wants to be polled, or `now` if untracked.
    #[must_use]
    pub fn poll_timeout(&self, id: &SessionId, now: Instant) -> Instant {
        self.sessions
            .lock()
            .ok()
            .and_then(|mut m| {
                m.get_mut(id)
                    .and_then(|e| e.session.as_mut().map(|s| s.poll_timeout(now)))
            })
            .unwrap_or(now)
    }

    /// The ids of every tracked session (live + closed tombstones), for the
    /// endpoint driver to iterate and pump.
    #[must_use]
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.sessions
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Feed one received datagram to **every** live session: str0m demuxes by ICE
    /// ufrag and silently ignores a datagram not addressed to it, so the endpoint
    /// driver (which owns the single shared socket) can fan an inbound datagram to
    /// all sessions without learning the per-session peer mapping itself. The
    /// earliest wake instant any session now wants is the driver's next park hint
    /// (returned), so the driver never busy-spins.
    ///
    /// # Errors
    ///
    /// [`WhepError::MalformedOffer`] only if the bookkeeping mutex is poisoned.
    pub fn handle_datagram_broadcast(
        &self,
        source: SocketAddr,
        destination: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Result<(), WhepError> {
        for id in self.session_ids() {
            self.handle_datagram(&id, source, destination, payload, now)?;
        }
        Ok(())
    }

    /// Drive every session's egress one step and return all outbound datagrams
    /// (paired with the destination the driver sends each to). The endpoint driver
    /// calls this on a tick / after a recv; a stalled session merely produces
    /// nothing (invariant #10 — never blocks the others or the engine).
    ///
    /// # Errors
    ///
    /// [`WhepError::MalformedOffer`] only if the bookkeeping mutex is poisoned.
    pub fn drive_all(
        &self,
        now: Instant,
    ) -> Result<Vec<(SocketAddr, SocketAddr, Vec<u8>)>, WhepError> {
        let mut out = Vec::new();
        for id in self.session_ids() {
            out.extend(self.drive_egress(&id, now)?);
        }
        Ok(out)
    }

    /// The earliest instant any tracked session wants to be polled (the driver's
    /// park horizon). Returns `now + 1s` when there are no sessions.
    #[must_use]
    pub fn next_wake(&self, now: Instant) -> Instant {
        self.session_ids()
            .into_iter()
            .map(|id| self.poll_timeout(&id, now))
            .min()
            .unwrap_or_else(|| now + std::time::Duration::from_secs(1))
    }

    fn mint_id(&self) -> SessionId {
        let n = self.next_id.lock().map_or(0, |mut g| {
            *g = g.saturating_add(1);
            *g
        });
        SessionId::new(format!("whep-egress-{n}"))
    }
}

impl WhepTransport for WhepEgress {
    fn accept(
        &self,
        offer: &str,
        codec: PreviewCodec,
        media: &dyn PreviewMediaSource,
    ) -> Result<TransportAnswer, WhepError> {
        self.accept_session(offer, codec, media)
            .map(|a| a.transport)
    }

    fn close(&self, id: &SessionId) -> Result<(), WhepError> {
        if let Ok(mut map) = self.sessions.lock() {
            if let Some(entry) = map.get_mut(id) {
                entry.handle.close();
                if let Some(mut session) = entry.session.take() {
                    session.disconnect();
                }
                entry.video = None;
                entry.audio = None;
            }
            // Absent session is not an error (idempotent DELETE).
        }
        Ok(())
    }
}

/// Whether `offer`'s video `m=` section advertises `codec`'s encoding name.
fn offer_advertises_video(offer: &str, codec: PreviewCodec) -> bool {
    let want = codec.rtpmap_name();
    let mut in_video = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_video = rest.starts_with("video");
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(mapping) = rtpmap.split_whitespace().nth(1) {
                if let Some(name) = mapping.split('/').next() {
                    if name.eq_ignore_ascii_case(want) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Whether `offer`'s audio `m=` section advertises an Opus rtpmap at 48 kHz.
fn offer_advertises_opus(offer: &str) -> bool {
    let mut in_audio = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_audio = rest.starts_with("audio");
            continue;
        }
        if !in_audio {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(mapping) = rtpmap.split_whitespace().nth(1) {
                let mut fields = mapping.split('/');
                let name = fields.next().unwrap_or("");
                let clock = fields.next().unwrap_or("");
                if name.eq_ignore_ascii_case("opus") && clock == "48000" {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    #[test]
    fn offer_video_detection() {
        assert!(offer_advertises_video(
            "m=video 9 X 96\r\na=rtpmap:96 H264/90000\r\n",
            PreviewCodec::H264
        ));
        assert!(!offer_advertises_video(
            "m=audio 9 X 111\r\na=rtpmap:111 opus/48000/2\r\n",
            PreviewCodec::H264
        ));
        assert!(offer_advertises_video(
            "m=video 9 X 97\r\na=rtpmap:97 VP8/90000\r\n",
            PreviewCodec::Vp8
        ));
    }

    #[test]
    fn opus_detection_requires_48k_in_audio_section() {
        assert!(offer_advertises_opus(
            "m=audio 9 X 111\r\na=rtpmap:111 opus/48000/2\r\n"
        ));
        assert!(!offer_advertises_opus(
            "m=audio 9 X 0\r\na=rtpmap:0 PCMU/8000\r\n"
        ));
        // An opus rtpmap in the video section is not an audio m-line.
        assert!(!offer_advertises_opus(
            "m=video 9 X 96\r\na=rtpmap:96 H264/90000\r\n"
        ));
    }

    #[test]
    fn parse_answer_attributes_extracts_lines() {
        const ANSWER: &str = "v=0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=ice-ufrag:Sv3R\r\n\
a=ice-pwd:serverPasswordValue0123456789ab\r\n\
a=fingerprint:sha-256 AA:BB:CC:DD\r\n\
a=setup:passive\r\n\
a=candidate:1 1 udp 2122260223 ::1 50000 typ host\r\n";
        let attrs = parse_answer_attributes(ANSWER).expect("parses");
        assert_eq!(attrs.ice_ufrag, "Sv3R");
        assert_eq!(attrs.setup.as_str(), "passive");
        assert_eq!(attrs.candidates.len(), 1);
    }

    #[test]
    fn parse_answer_attributes_rejects_missing_ice() {
        assert!(parse_answer_attributes("v=0\r\na=fingerprint:sha-256 AA\r\n").is_err());
    }
}
