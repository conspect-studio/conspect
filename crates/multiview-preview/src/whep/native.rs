//! The **native str0m `WhepTransport`** — gated behind the off-by-default
//! `webrtc-native` feature, the *further* gate the [`super::transport`] seam docs
//! refer to.
//!
//! [`super::transport`] defines the socket-free [`WhepTransport`] seam and proves
//! it with an in-memory fake. This module supplies a **concrete** implementation
//! backed by [`str0m`], a sans-IO WebRTC stack, giving real ICE / DTLS / SRTP:
//!
//! * [`Str0mWhepTransport::accept`] parses the browser's WHEP **SDP offer** with
//!   str0m, builds an [`str0m::Rtc`], accepts the offer (str0m mints fresh ICE
//!   credentials and a self-signed DTLS certificate), and folds the **real**
//!   ufrag/pwd + DTLS fingerprint + `a=setup:passive` + any gathered candidates
//!   into a [`TransportAnswer`] the preview core's
//!   [`super::WhepSession::build_answer`] returns. Because str0m is *sans-IO*,
//!   this entire negotiation runs **without a socket** and is unit-tested in CI.
//! * [`Str0mWhepTransport::close`] drives the session's lifecycle handle to
//!   [`SessionState::Closed`] and drops the owned `Rtc`.
//!
//! ## What still needs a socket + a peer (NOT yet complete — PRV-1c)
//!
//! The live egress path is **only partially built**, and honestly so:
//! [`Str0mWhepTransport::drive_egress_once`] implements the single
//! poll-`str0m`-and-`send`-one-datagram step, and the env-gated `#[ignore]`d
//! loopback test (`MULTIVIEW_WHEP_LOOPBACK=1`, `tests/whep_native.rs`) drives it
//! against a bound loopback UDP socket to confirm str0m **emits** ICE/DTLS
//! datagrams. What is **not** done: a full **DTLS handshake** needs a real
//! WebRTC peer (a browser / a second agent), which CI has no reliable way to
//! provide, so it is not verified here; and **SRTP media egress** —
//! packetizing the [`super::transport::SampleFeed`] into RTP and feeding str0m —
//! is **not yet wired** at all. [`Str0mWhepTransport::new`] builds a socket-free
//! negotiating transport; [`Str0mWhepTransport::bind_loopback`] binds a loopback
//! socket for the partial live test. Completing the handshake-against-a-peer +
//! the SampleFeed→SRTP feed + an ffprobe egress check is the remaining slice
//! (PRV-1c).
//!
//! ## Isolation (invariant #10)
//!
//! A str0m session is a *preview* consumer exactly like the fake: it drains the
//! media [`super::transport::SampleFeed`] (drop-oldest) and owns only its `Rtc`
//! plus a lifecycle [`SessionHandle`]. It never holds a handle the engine awaits,
//! never publishes onto the protected output path, and a stalled or absent peer
//! merely loses the oldest buffered samples.
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::{Candidate, Rtc};

use super::transport::{
    DtlsFingerprint, DtlsSetup, PreviewMediaSource, SampleFeed, SessionHandle, SessionId,
    SessionState, TransportAnswer, WhepTransport,
};
use super::{PreviewCodec, WhepError};

/// The transport-supplied SDP answer attributes parsed out of an SDP answer
/// string. This is the structured result of [`parse_answer_attributes`]; it
/// mirrors the connection/ICE/DTLS fields of [`TransportAnswer`] (minus the
/// session id, which the transport mints independently of the SDP text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerAttributes {
    /// ICE username fragment (`a=ice-ufrag`).
    pub ice_ufrag: String,
    /// ICE password (`a=ice-pwd`).
    pub ice_pwd: String,
    /// DTLS certificate fingerprint (`a=fingerprint`).
    pub fingerprint: DtlsFingerprint,
    /// DTLS `a=setup` role advertised by the answerer.
    pub setup: DtlsSetup,
    /// Gathered ICE candidate lines (each the value after `a=candidate:`).
    pub candidates: Vec<String>,
}

/// Parse the ICE / DTLS / candidate attributes out of an SDP **answer** string.
///
/// This is the pure SDP-munging seam between str0m's
/// [`SdpAnswer::to_sdp_string`] output and the preview core's
/// [`TransportAnswer`]: it extracts the `a=ice-ufrag`, `a=ice-pwd`,
/// `a=fingerprint` (split into algorithm + colon-hex value), `a=setup` role, and
/// every `a=candidate` line. ICE ufrag/pwd and the DTLS fingerprint are required
/// — an answer missing any of them cannot establish a session. `a=setup`
/// defaults to `passive` (a WHEP egress server is always the DTLS server) when
/// absent. Candidates may be empty for a trickle-ICE answer (the socket-free
/// negotiating transport gathers none).
///
/// # Errors
///
/// Returns [`WhepError::MalformedOffer`] if the answer carries no ICE ufrag, no
/// ICE pwd, or no DTLS fingerprint (the three attributes without which the
/// answer is unusable).
pub fn parse_answer_attributes(answer: &str) -> Result<AnswerAttributes, WhepError> {
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
            // "<algorithm> <COLON-HEX>"
            if let Some((algo, value)) = v.split_once(' ') {
                fingerprint = Some(DtlsFingerprint {
                    algorithm: algo.to_ascii_lowercase(),
                    value: value.trim().to_owned(),
                });
            }
        } else if let Some(v) = line.strip_prefix("a=setup:") {
            setup = match v.trim() {
                "active" => DtlsSetup::Active,
                // "passive" and "actpass" (a server answer is never actpass, but
                // be lenient) both map to the server-is-DTLS-server role.
                _ => DtlsSetup::Passive,
            };
        } else if let Some(v) = line.strip_prefix("a=candidate:") {
            candidates.push(v.to_owned());
        }
    }

    let ice_ufrag = ice_ufrag.ok_or(WhepError::MalformedOffer {
        reason: "answer has no a=ice-ufrag",
    })?;
    let ice_pwd = ice_pwd.ok_or(WhepError::MalformedOffer {
        reason: "answer has no a=ice-pwd",
    })?;
    let fingerprint = fingerprint.ok_or(WhepError::MalformedOffer {
        reason: "answer has no a=fingerprint",
    })?;

    Ok(AnswerAttributes {
        ice_ufrag,
        ice_pwd,
        fingerprint,
        setup,
        candidates,
    })
}

/// One str0m session: the peer connection plus its lifecycle handle.
///
/// The `Rtc` is the sans-IO state machine; the [`SessionHandle`] is the testable
/// lifecycle the control plane reads and `close` drives terminal. The held
/// [`SampleFeed`] is the drop-oldest media tap the egress loop would drain — kept
/// here to prove the only coupling is the lossy feed, never an engine handle
/// (invariant #10).
///
/// On [`Str0mWhepTransport::close`] the `rtc` and `feed` are *released* (dropped)
/// to free the peer connection immediately, but the entry is **retained** as a
/// closed tombstone so the session id stays queryable as
/// [`SessionState::Closed`] (the WHEP `DELETE …/{id}` may be retried).
struct Str0mSession {
    /// The sans-IO peer connection. Taken (dropped) on `close`; `None` once the
    /// session is a closed tombstone.
    rtc: Option<Rtc>,
    handle: SessionHandle,
    /// The drop-oldest media tap the live egress loop drains (env-gated path
    /// only). Taken (dropped) on `close`.
    #[allow(dead_code)] // drained by the live egress loop (env-gated path only)
    feed: Option<SampleFeed>,
}

/// A native [`WhepTransport`] backed by [`str0m`]'s sans-IO ICE/DTLS/SRTP stack.
///
/// Construct a socket-free negotiating transport with [`Self::new`] (the
/// CI-runnable path: real SDP offer→answer with real ICE/DTLS attributes, no
/// socket), or a loopback-bound one with [`Self::bind_loopback`] for the
/// env-gated live DTLS-SRTP test.
///
/// Sessions are tracked in a short-lived bookkeeping map the engine never touches
/// (invariant #10). The transport is `Send + Sync`: the map is mutex-guarded.
pub struct Str0mWhepTransport {
    /// Live sessions keyed by their minted [`SessionId`]. Mutex-guarded preview
    /// bookkeeping; never an engine lock.
    sessions: Mutex<HashMap<SessionId, Str0mSession>>,
    /// If bound, the local UDP socket the egress loop sends from, plus its
    /// gathered host candidate. `None` for the socket-free negotiating transport.
    local: Option<LocalSocket>,
    /// Monotonic session-id counter so each accept mints a distinct id.
    next_id: Mutex<u64>,
}

/// A bound loopback UDP socket and the host candidate it advertises.
struct LocalSocket {
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
}

impl std::fmt::Debug for Str0mWhepTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.sessions.lock().map_or(0, |s| s.len());
        f.debug_struct("Str0mWhepTransport")
            .field("live_sessions", &live)
            .field("bound", &self.local.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Str0mWhepTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Str0mWhepTransport {
    /// Build a socket-free negotiating transport.
    ///
    /// str0m is sans-IO, so this performs the full SDP offer→answer negotiation
    /// (real ICE credentials + a real self-signed DTLS certificate) with **no**
    /// socket. The session stays in [`SessionState::Created`]; completing the
    /// DTLS handshake / SRTP egress needs a bound socket (see
    /// [`Self::bind_loopback`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            local: None,
            next_id: Mutex::new(0),
        }
    }

    /// Build a transport bound to a loopback UDP socket, gathering a single host
    /// candidate so the live DTLS-SRTP egress path can run.
    ///
    /// Used only by the env-gated `#[ignore]`d loopback test; the CI path uses
    /// [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns [`WhepError::MalformedOffer`] (reused as the transport's
    /// socket-failure variant) if binding the loopback socket fails.
    pub fn bind_loopback() -> Result<Self, WhepError> {
        // IPv6-first (operator directive): gather an IPv6 loopback host candidate.
        let socket = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).map_err(|_| {
            WhepError::MalformedOffer {
                reason: "failed to bind loopback UDP socket",
            }
        })?;
        let addr = socket.local_addr().map_err(|_| WhepError::MalformedOffer {
            reason: "failed to read loopback socket address",
        })?;
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            local: Some(LocalSocket {
                socket: Arc::new(socket),
                addr,
            }),
            next_id: Mutex::new(0),
        })
    }

    /// The current lifecycle state of the session with `id`, or `None` if no such
    /// session is tracked (e.g. it was never accepted, or the bookkeeping mutex
    /// was poisoned — a poisoned map is conservatively reported as absent).
    #[must_use]
    pub fn session_state(&self, id: &SessionId) -> Option<SessionState> {
        self.sessions
            .lock()
            .ok()
            .and_then(|m| m.get(id).map(|s| s.handle.state()))
    }

    /// Drive one egress step of the session with `id`: poll str0m for an outgoing
    /// transmission and, if the transport is socket-bound, send it.
    ///
    /// This is the seam of the live UDP egress loop. It is only meaningful for a
    /// [`Self::bind_loopback`] transport (a socket-free [`Self::new`] transport
    /// returns `Ok(false)` with nothing to send); the env-gated loopback test
    /// drives it to confirm str0m produces ICE/DTLS transmissions and they reach
    /// the bound socket. Returns `Ok(true)` if a datagram was sent.
    ///
    /// # Errors
    ///
    /// Returns [`WhepError::MalformedOffer`] (reused as the transport's
    /// egress-fault variant) if the session is unknown/closed, the bookkeeping
    /// mutex is poisoned, or the UDP send fails.
    pub fn drive_egress_once(&self, id: &SessionId) -> Result<bool, WhepError> {
        use str0m::Output;

        let Some(local) = &self.local else {
            // Socket-free negotiating transport: nothing to send over the wire.
            return Ok(false);
        };
        let mut map = self
            .sessions
            .lock()
            .map_err(|_| WhepError::MalformedOffer {
                reason: "preview session map poisoned",
            })?;
        let session = map.get_mut(id).ok_or(WhepError::MalformedOffer {
            reason: "no such session to drive",
        })?;
        let rtc = session.rtc.as_mut().ok_or(WhepError::MalformedOffer {
            reason: "session already closed",
        })?;

        // Poll until the next timeout (one transmit per call, at most).
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    local
                        .socket
                        .send_to(&t.contents, t.destination)
                        .map_err(|_| WhepError::MalformedOffer {
                            reason: "UDP egress send failed",
                        })?;
                    return Ok(true);
                }
                // No more queued I/O this step; the drive loop would now wait for
                // the timeout or inbound packets. Nothing more to send right now.
                Ok(Output::Timeout(_)) => return Ok(false),
                // A connection event (ICE state change, etc.): keep polling for a
                // transmission this step — the loop continues to the next
                // `poll_output` naturally.
                Ok(Output::Event(_)) => {}
                Err(_) => {
                    return Err(WhepError::MalformedOffer {
                        reason: "str0m poll_output faulted",
                    });
                }
            }
        }
    }

    /// Mint the next distinct session id.
    fn mint_id(&self) -> SessionId {
        let n = self.next_id.lock().map_or(0, |mut g| {
            *g = g.saturating_add(1);
            *g
        });
        SessionId::new(format!("str0m-{n}"))
    }

    /// Build and accept the str0m peer connection for `offer`, returning the
    /// `Rtc` and the SDP answer string str0m produced.
    fn negotiate_rtc(&self, offer: &str) -> Result<(Rtc, String), WhepError> {
        let offer = SdpOffer::from_sdp_string(offer).map_err(|_| WhepError::MalformedOffer {
            reason: "str0m could not parse the WHEP offer SDP",
        })?;

        let mut rtc = Rtc::builder().build(Instant::now());

        // Add the gathered host candidate (if bound) BEFORE accepting the offer
        // so the answer carries it; a socket-free transport gathers none (trickle).
        if let Some(local) = &self.local {
            if let Ok(cand) = Candidate::host(local.addr, "udp") {
                rtc.add_local_candidate(cand);
            }
        }

        let answer: SdpAnswer =
            rtc.sdp_api()
                .accept_offer(offer)
                .map_err(|_| WhepError::MalformedOffer {
                    reason: "str0m rejected the WHEP offer (no usable media / ICE / DTLS)",
                })?;

        Ok((rtc, answer.to_sdp_string()))
    }
}

impl WhepTransport for Str0mWhepTransport {
    fn accept(
        &self,
        offer: &str,
        codec: PreviewCodec,
        media: &dyn PreviewMediaSource,
    ) -> Result<TransportAnswer, WhepError> {
        // The preview core (`WhepSession::negotiate`) already selected `codec`
        // from the offer; re-confirm the offer actually advertises *that* codec so
        // an audio-only or codec-mismatched offer is rejected here too (and never
        // reaches str0m's media negotiation with nothing to send).
        if !offer_advertises(offer, codec) {
            return Err(WhepError::NoSupportedCodec);
        }

        let (rtc, answer_sdp) = self.negotiate_rtc(offer)?;
        let attrs = parse_answer_attributes(&answer_sdp)?;

        let id = self.mint_id();
        let handle = SessionHandle::new(id.clone());
        // Take the media feed exactly once (the live egress loop drains it).
        let feed = media.feed();

        let session = Str0mSession {
            rtc: Some(rtc),
            handle: handle.clone(),
            feed: Some(feed),
        };
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(id.clone(), session);
        } else {
            // Poisoned bookkeeping map: a panic in another preview task. Preview
            // is best-effort; refuse the session rather than propagate a panic.
            return Err(WhepError::MalformedOffer {
                reason: "preview session map poisoned",
            });
        }

        Ok(TransportAnswer {
            session_id: id,
            ice_ufrag: attrs.ice_ufrag,
            ice_pwd: attrs.ice_pwd,
            fingerprint: attrs.fingerprint,
            setup: attrs.setup,
            candidates: attrs.candidates,
        })
    }

    fn close(&self, id: &SessionId) -> Result<(), WhepError> {
        if let Ok(mut map) = self.sessions.lock() {
            if let Some(session) = map.get_mut(id) {
                // Drive the lifecycle handle terminal (idempotent), disconnect the
                // peer connection, and release the Rtc + media feed immediately.
                // The entry is retained as a closed tombstone so the id stays
                // queryable (the WHEP DELETE may be retried / RTCP-timeout fired).
                session.handle.close();
                if let Some(mut rtc) = session.rtc.take() {
                    rtc.disconnect();
                }
                session.feed = None;
            }
            // Absent session is not an error (idempotent DELETE).
        }
        Ok(())
    }
}

/// Whether `offer`'s video `m=` section advertises the encoding name of `codec`.
///
/// A light, allocation-free scan of the video `m=` section's `a=rtpmap` lines for
/// the caller-selected [`PreviewCodec`]'s [`PreviewCodec::rtpmap_name`]. Used to
/// reject an audio-only or codec-mismatched offer at the transport boundary
/// without re-running the full [`super::WhepSession`] parse (the caller already
/// selected the codec; this confirms the offer agrees).
fn offer_advertises(offer: &str, codec: PreviewCodec) -> bool {
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
    fn offer_video_detection_ignores_audio_only() {
        let audio = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";
        assert!(!offer_advertises(audio, PreviewCodec::H264));
        let video = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";
        assert!(offer_advertises(video, PreviewCodec::H264));
        // A VP8-only offer does not advertise H.264 — codec mismatch is rejected.
        let vp8 = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 97\r\na=rtpmap:97 VP8/90000\r\n";
        assert!(!offer_advertises(vp8, PreviewCodec::H264));
        assert!(offer_advertises(vp8, PreviewCodec::Vp8));
    }

    #[test]
    fn answer_setup_defaults_to_passive_when_absent() {
        let no_setup = "a=ice-ufrag:u\r\na=ice-pwd:p\r\na=fingerprint:sha-256 AA:BB\r\n";
        let attrs = parse_answer_attributes(no_setup).expect("parses");
        assert_eq!(attrs.setup.as_str(), "passive");
    }

    #[test]
    fn answer_setup_active_is_parsed() {
        let active =
            "a=ice-ufrag:u\r\na=ice-pwd:p\r\na=fingerprint:sha-256 AA:BB\r\na=setup:active\r\n";
        let attrs = parse_answer_attributes(active).expect("parses");
        assert_eq!(attrs.setup.as_str(), "active");
    }
}
