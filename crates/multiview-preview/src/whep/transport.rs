//! The **WHEP transport seam** — gated behind the off-by-default `webrtc`
//! feature, alongside the pure SDP/codec logic in [`super`].
//!
//! [`super::WhepSession::negotiate`] does the *pure* half of a focus session:
//! parse the offer, select the preview encode codec, and build a codec-only SDP
//! answer skeleton whose ICE/DTLS attributes are placeholders. The **transport**
//! owns the other half: it supplies the real ICE ufrag/pwd, the DTLS
//! certificate fingerprint, and the gathered candidate lines, and it drives the
//! per-session ICE/DTLS/SRTP lifecycle. This module defines that seam as a
//! trait so the negotiation-only build stays dependency-free and a native
//! (str0m) implementation, or a MediaMTX-sidecar republisher, can plug in
//! behind a *further* gate without changing the preview core.
//!
//! ## What lives here (the testable core)
//!
//! * [`WhepTransport`] — the seam a transport implements: `accept` an offer for
//!   a chosen codec + media source and return [`TransportAnswer`] attributes;
//!   `close` a session by id.
//! * [`TransportAnswer`] — the transport-supplied SDP attributes that
//!   [`super::WhepSession::build_answer`] folds into the answer, which the
//!   codec-only scaffold leaves absent.
//! * [`SessionState`] / [`SessionHandle`] — the session lifecycle
//!   (`Created → Connecting → Connected → Closed`) as an explicit, testable
//!   state machine; illegal transitions are rejected, never panicked.
//! * [`SampleFeed`] / [`SampleSink`] — the **bounded, drop-oldest** seam the
//!   preview encoder pushes encoded samples through to the transport. This is
//!   the invariant-#10 isolation boundary: pushing a sample **never blocks and
//!   never awaits**; when the consumer (the transport's egress task) falls
//!   behind, the *oldest* buffered sample is dropped. Preview is best-effort and
//!   can never back-pressure the engine or the encoder feeding it.
//!
//! ## What is NOT here (the live path)
//!
//! The real ICE/DTLS/SRTP socket path needs UDP/STUN reachability and DTLS
//! certificates, neither of which is reliable in CI. A native `str0m`-backed
//! [`WhepTransport`] therefore lands behind a further, separately-gated build
//! and is exercised by an env-gated loopback test — see the crate's
//! `tests/whep_transport.rs`. Everything in this module is socket-free: the seam,
//! the SDP glue, the lifecycle, and the bounded feed are all unit-testable with
//! an in-memory fake transport.
//!
//! ## Isolation (invariant #10)
//!
//! A transport task is a *preview* consumer. It reads encoded samples from a
//! [`SampleFeed`] (drop-oldest) and never holds a handle the engine awaits. The
//! [`SampleSink`] the encoder writes to does a single non-blocking, wait-free
//! store; a stalled or absent transport merely loses the oldest samples. Nothing
//! in this module publishes onto, or awaits, the protected output path.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::{PreviewCodec, WhepError};

/// An opaque, transport-assigned identifier for one focus session.
///
/// The transport mints this in [`WhepTransport::accept`] and the caller hands it
/// back to [`WhepTransport::close`] (and the WHEP `DELETE …/{session_id}` route
/// maps onto it). It is just a string newtype so different transports
/// (str0m's `Rtc` id, a sidecar resource path) can each choose their own form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a transport-chosen id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice (e.g. for the WHEP resource URL).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A DTLS certificate fingerprint for the SDP `a=fingerprint` line.
///
/// `algorithm` is the lower-case hash name (`"sha-256"`); `value` is the
/// colon-separated upper-case hex of the certificate digest, exactly as it
/// appears after `a=fingerprint:<algorithm> `.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtlsFingerprint {
    /// Hash algorithm name, lower-case (e.g. `"sha-256"`).
    pub algorithm: String,
    /// Colon-separated upper-case hex digest (e.g. `"AB:CD:…"`).
    pub value: String,
}

/// The transport-supplied SDP attributes for a session's answer.
///
/// [`super::WhepSession::build_answer`] takes these and fills the ICE/DTLS lines
/// the codec-only scaffold leaves absent. A real transport gathers candidates
/// and a DTLS certificate; the in-memory fake used in tests supplies
/// deterministic non-placeholder values to prove the wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportAnswer {
    /// The transport's session id (also the WHEP resource id).
    pub session_id: SessionId,
    /// ICE username fragment (`a=ice-ufrag`).
    pub ice_ufrag: String,
    /// ICE password (`a=ice-pwd`).
    pub ice_pwd: String,
    /// DTLS certificate fingerprint (`a=fingerprint`).
    pub fingerprint: DtlsFingerprint,
    /// DTLS setup role for a server answer; always `Passive` here because the
    /// preview server is the answerer (the browser is the DTLS client).
    pub setup: DtlsSetup,
    /// Gathered ICE candidate lines (each the value after `a=candidate:`).
    /// May be empty for a fully trickle-ICE transport; the fake supplies one
    /// host candidate so the answer is provably non-placeholder.
    pub candidates: Vec<String>,
}

/// The DTLS `a=setup` role advertised in the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DtlsSetup {
    /// The answerer acts as the DTLS server (waits for the client `ClientHello`).
    /// This is what a WHEP egress server advertises.
    Passive,
    /// The answerer acts as the DTLS client (initiates the handshake).
    Active,
}

impl DtlsSetup {
    /// The SDP `a=setup:<role>` token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::Active => "active",
        }
    }
}

/// The lifecycle of a WHEP focus session, as an explicit state machine.
///
/// A session is born [`Created`](SessionState::Created) the moment the transport
/// accepts an offer, advances through [`Connecting`](SessionState::Connecting)
/// (ICE/DTLS in progress) to [`Connected`](SessionState::Connected) (SRTP
/// flowing), and ends [`Closed`](SessionState::Closed) on teardown, ICE failure,
/// or RTCP timeout. [`Closed`](SessionState::Closed) is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionState {
    /// The offer was accepted and an answer produced; ICE has not yet started.
    Created,
    /// ICE/DTLS negotiation is in progress.
    Connecting,
    /// DTLS-SRTP is established and preview media is flowing.
    Connected,
    /// The session is torn down (terminal). Reached from any other state.
    Closed,
}

impl SessionState {
    /// Whether `next` is a legal successor of `self`.
    ///
    /// Forward progress `Created → Connecting → Connected` is allowed; any state
    /// may go directly to [`Closed`](SessionState::Closed) (teardown / failure);
    /// [`Closed`](SessionState::Closed) is terminal. Re-entering the *same*
    /// non-terminal state is **not** a transition (it is rejected) so that
    /// `advance_to` callers cannot silently mask a logic error.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        // Grouped by source state. Forward progress is
        // Created -> Connecting -> Connected (plus the Created -> Connected fast
        // path when ICE completes before the intermediate state is observed);
        // any live state may also go straight to the terminal Closed (teardown /
        // ICE failure / RTCP timeout). Closed is terminal, so it has no arm.
        matches!(
            (self, next),
            (
                Self::Created,
                Self::Connecting | Self::Connected | Self::Closed
            ) | (Self::Connecting, Self::Connected | Self::Closed)
                | (Self::Connected, Self::Closed)
        )
    }

    /// Whether this is the terminal [`Closed`](SessionState::Closed) state.
    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// A handle to one session's mutable lifecycle state.
///
/// Cheap to clone (an `Arc` around a small `Mutex`). The transport advances it
/// as ICE/DTLS progresses; the control plane reads it to report session status
/// and to drive teardown. The mutex is short-lived preview bookkeeping the
/// engine never touches (invariant #10).
#[derive(Debug, Clone)]
pub struct SessionHandle {
    id: SessionId,
    state: Arc<Mutex<SessionState>>,
}

impl SessionHandle {
    /// Create a handle in the initial [`SessionState::Created`] state.
    #[must_use]
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            state: Arc::new(Mutex::new(SessionState::Created)),
        }
    }

    /// This session's id.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The current lifecycle state.
    ///
    /// If the state mutex was poisoned by a panic in another preview task this
    /// conservatively reports [`SessionState::Closed`] — a poisoned session is
    /// not usable and must be treated as torn down. This never reflects engine
    /// state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state.lock().map_or(SessionState::Closed, |g| *g)
    }

    /// Attempt to advance the session to `next`.
    ///
    /// # Errors
    ///
    /// Returns [`WhepError::IllegalTransition`] if `next` is not a legal
    /// successor of the current state (see
    /// [`SessionState::can_transition_to`]), or if the state lock was poisoned
    /// (treated as already closed).
    pub fn advance_to(&self, next: SessionState) -> Result<(), WhepError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| WhepError::IllegalTransition {
                from: SessionState::Closed,
                to: next,
            })?;
        if !guard.can_transition_to(next) {
            return Err(WhepError::IllegalTransition {
                from: *guard,
                to: next,
            });
        }
        *guard = next;
        Ok(())
    }

    /// Force the session to [`SessionState::Closed`] (idempotent).
    ///
    /// Teardown is always legal and must never fail, so this saturates to
    /// `Closed` regardless of the current state and ignores a poisoned lock
    /// (a poisoned session is already effectively closed).
    pub fn close(&self) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = SessionState::Closed;
        }
    }
}

/// The media kind of one [`EncodedSample`] — and therefore which RTP clock its
/// [`EncodedSample::rtp_timestamp`] is expressed in (ADR-P006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleKind {
    /// An encoded video access unit. Video timestamps ride the **90 kHz** RTP
    /// clock every video payload's SDP `rtpmap` advertises.
    Video,
    /// An encoded audio frame — **Opus by definition** on this seam (RFC 7874
    /// makes Opus WebRTC's mandatory audio codec; ADR-P006 pins it). Audio
    /// timestamps ride the **48 kHz** RTP clock RFC 7587 fixes for Opus.
    Audio,
}

impl SampleKind {
    /// The RTP clock rate (Hz) this kind's timestamps are expressed in:
    /// 90 kHz for video, 48 kHz for (Opus) audio (RFC 7587).
    #[must_use]
    pub const fn rtp_clock_hz(self) -> u32 {
        match self {
            Self::Video => 90_000,
            Self::Audio => 48_000,
        }
    }
}

/// One encoded preview media sample handed from the encoder to the transport.
///
/// Carries the encoded bytes plus its presentation timestamp (in the RTP clock
/// of its [`kind`](Self::kind)) and whether it begins a keyframe. The transport
/// packetizes this into RTP/SRTP; the preview core never inspects the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSample {
    /// Encoded bytes: a video access unit (e.g. an H.264 Annex-B / AVCC frame)
    /// or one Opus audio frame, per [`kind`](Self::kind).
    pub data: Arc<[u8]>,
    /// Presentation timestamp in RTP clock units **of this sample's kind**:
    /// video samples ride the 90 kHz RTP clock, audio (Opus) samples the
    /// 48 kHz clock RFC 7587 fixes — see [`SampleKind::rtp_clock_hz`].
    pub rtp_timestamp: u32,
    /// Whether this sample begins a keyframe (IDR) — the transport may gate the
    /// first delivered packet on a keyframe boundary. Audio frames are
    /// independently decodable, so audio producers set `false`.
    pub keyframe: bool,
    /// Which media kind this sample is — video on the 90 kHz RTP clock or Opus
    /// audio on the 48 kHz clock (ADR-P006). Determines how the transport
    /// packetizes the bytes and which negotiated m-line they ride.
    pub kind: SampleKind,
}

/// The producer end of a bounded, drop-oldest sample feed.
///
/// The preview encoder writes encoded samples here with [`SampleSink::push`],
/// which is **wait-free and non-blocking**: it never awaits and never blocks on
/// the transport. When the bounded ring is full the **oldest** buffered sample
/// is evicted to make room (drop-oldest), realizing the invariant-#10 guarantee
/// that a slow or absent transport can never back-pressure the encoder feeding
/// it. [`SampleSink::push`] returns whether an old sample was dropped, so the
/// caller can surface "preview lagging" without ever stalling.
#[derive(Debug, Clone)]
pub struct SampleSink {
    inner: Arc<Mutex<SampleRing>>,
}

/// The consumer end of a bounded, drop-oldest sample feed.
///
/// The transport's egress task drains this with [`SampleFeed::pop`]. Draining
/// slowly only causes the producer to drop the oldest samples; it can never
/// block the producer.
#[derive(Debug)]
pub struct SampleFeed {
    inner: Arc<Mutex<SampleRing>>,
}

/// Shared bounded ring backing a [`SampleSink`]/[`SampleFeed`] pair.
#[derive(Debug)]
struct SampleRing {
    queue: VecDeque<EncodedSample>,
    capacity: usize,
    /// Total samples evicted as the oldest because the ring was full.
    dropped: u64,
}

/// Build a bounded, drop-oldest sample feed of the given ring `depth`.
///
/// `depth` is clamped to at least 1 so the feed always has room for the newest
/// sample (per ADR-P001 the preview rings are shallow, depth 1–3). Returns the
/// `(SampleSink, SampleFeed)` producer/consumer pair.
#[must_use]
pub fn sample_feed(depth: usize) -> (SampleSink, SampleFeed) {
    let capacity = depth.max(1);
    let ring = Arc::new(Mutex::new(SampleRing {
        queue: VecDeque::with_capacity(capacity),
        capacity,
        dropped: 0,
    }));
    (
        SampleSink {
            inner: Arc::clone(&ring),
        },
        SampleFeed { inner: ring },
    )
}

impl SampleSink {
    /// Push an encoded sample, dropping the oldest if the ring is full.
    ///
    /// Wait-free with respect to the transport: this only takes the feed's own
    /// short-lived bookkeeping mutex, never a lock the engine or encoder hot
    /// path awaits, and never blocks on the consumer. Returns `true` if an older
    /// sample was evicted to make room (the feed is lagging), `false` otherwise.
    /// If the bookkeeping mutex is poisoned the sample is silently dropped and
    /// `false` returned — preview is best-effort and must never propagate a
    /// failure outward.
    ///
    /// `#[must_use]`: the returned lag flag is the only synchronous "preview
    /// falling behind" signal at the push site; a caller that genuinely does not
    /// care can bind it to `_`.
    #[must_use]
    pub fn push(&self, sample: EncodedSample) -> bool {
        let Ok(mut ring) = self.inner.lock() else {
            return false;
        };
        let mut evicted = false;
        while ring.queue.len() >= ring.capacity {
            if ring.queue.pop_front().is_some() {
                ring.dropped = ring.dropped.saturating_add(1);
                evicted = true;
            } else {
                break;
            }
        }
        ring.queue.push_back(sample);
        evicted
    }

    /// The number of samples currently buffered (bounded by the ring depth).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.inner.lock().map_or(0, |r| r.queue.len())
    }
}

impl SampleFeed {
    /// Remove and return the oldest buffered sample, or `None` if empty.
    ///
    /// Non-blocking. Draining slowly only causes the producer to drop the oldest
    /// samples on its next [`SampleSink::push`]; it never back-pressures the
    /// producer.
    #[must_use]
    pub fn pop(&self) -> Option<EncodedSample> {
        self.inner.lock().ok().and_then(|mut r| r.queue.pop_front())
    }

    /// The number of samples currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.inner.lock().map_or(0, |r| r.queue.len())
    }

    /// The total number of samples dropped (evicted as oldest) since creation.
    ///
    /// A non-zero, growing count is the operator-visible "preview lagging"
    /// signal; it never indicates engine trouble.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.lock().map_or(0, |r| r.dropped)
    }
}

/// The source of preview media a transport pulls encoded samples from.
///
/// A real implementation wraps a [`crate::TapLease`] (drop-oldest engine tap) →
/// preview H.264 encode → [`SampleSink`]; the transport drains the paired
/// [`SampleFeed`]. Modeling it as a trait keeps the transport seam testable with
/// an in-memory fake and keeps the encoder wiring out of the negotiation core.
pub trait PreviewMediaSource: Send + Sync {
    /// The codec the samples from [`Self::feed`] are encoded with.
    fn codec(&self) -> PreviewCodec;

    /// The drop-oldest feed the transport drains encoded video samples from.
    ///
    /// Called once when a session is accepted; the transport owns the returned
    /// [`SampleFeed`] for the life of the session.
    fn feed(&self) -> SampleFeed;

    /// The **optional** drop-oldest audio feed (ADR-P006).
    ///
    /// Audio on this seam is **Opus by definition** (RFC 7874's mandatory
    /// WebRTC audio codec; ADR-P006 pins 48 kHz / 20 ms frames): the samples
    /// it yields are [`SampleKind::Audio`] on the 48 kHz RTP clock. Like
    /// [`Self::feed`], this is called at most once per session, when the
    /// transport accepts it — and only when the session's offer negotiated an
    /// audio m-line.
    ///
    /// The default is `None`: sessions whose offer carries no audio m-line,
    /// and scopes with no audio source, simply leave audio absent (ADR-P006),
    /// so video-only sources need no override.
    fn audio_feed(&self) -> Option<SampleFeed> {
        None
    }
}

/// The transport seam: ICE/DTLS/SRTP for one or more WHEP focus sessions.
///
/// Implemented by a native (str0m) in-process transport behind a further gate,
/// by a MediaMTX-sidecar republisher, or by the in-memory fake used in tests.
/// The preview core depends only on this trait, so neither the negotiation logic
/// nor the control plane links a native WebRTC stack.
///
/// ## Isolation (invariant #10)
///
/// An implementation is a preview consumer: it drains a [`SampleFeed`]
/// (drop-oldest) and must never hold a handle the engine awaits, never publish
/// onto the protected output path, and never block the encoder feeding it.
pub trait WhepTransport: Send + Sync {
    /// Accept a WHEP `offer` for the chosen `codec`, sourcing media from
    /// `media`, and return the transport-supplied SDP answer attributes.
    ///
    /// The caller ([`super::WhepSession`]) has already selected `codec` from the
    /// offer; the transport gathers ICE candidates + a DTLS fingerprint, mints a
    /// [`SessionId`], wires `media`'s [`SampleFeed`] (and, when the offer
    /// negotiated an Opus audio m-line, its optional
    /// [`PreviewMediaSource::audio_feed`]) to its egress, and returns the
    /// [`TransportAnswer`] the caller folds into the SDP answer.
    ///
    /// # Errors
    ///
    /// Returns a [`WhepError`] if the transport cannot establish a session
    /// (e.g. no encode session available, ICE gathering failed). The error is
    /// surfaced to the operator and never affects the engine.
    fn accept(
        &self,
        offer: &str,
        codec: PreviewCodec,
        media: &dyn PreviewMediaSource,
    ) -> Result<TransportAnswer, WhepError>;

    /// Tear down the session with `id`, releasing its encode session and peer
    /// connection immediately. Idempotent: closing an unknown or already-closed
    /// session is not an error.
    ///
    /// # Errors
    ///
    /// Returns a [`WhepError`] only if the transport hit an internal fault while
    /// closing; the absence of the session is **not** an error.
    fn close(&self, id: &SessionId) -> Result<(), WhepError>;
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
    fn lifecycle_forward_progress_is_legal() {
        let h = SessionHandle::new(SessionId::new("s1"));
        assert_eq!(h.state(), SessionState::Created);
        h.advance_to(SessionState::Connecting).unwrap();
        assert_eq!(h.state(), SessionState::Connecting);
        h.advance_to(SessionState::Connected).unwrap();
        assert_eq!(h.state(), SessionState::Connected);
    }

    #[test]
    fn lifecycle_rejects_backwards_transition() {
        let h = SessionHandle::new(SessionId::new("s2"));
        h.advance_to(SessionState::Connected).unwrap();
        let err = h.advance_to(SessionState::Connecting).unwrap_err();
        assert!(matches!(err, WhepError::IllegalTransition { .. }));
        // State is unchanged by a rejected transition.
        assert_eq!(h.state(), SessionState::Connected);
    }

    #[test]
    fn close_is_terminal_and_idempotent() {
        let h = SessionHandle::new(SessionId::new("s3"));
        h.advance_to(SessionState::Connecting).unwrap();
        h.close();
        assert_eq!(h.state(), SessionState::Closed);
        // No transition leaves Closed.
        assert!(h.advance_to(SessionState::Connected).is_err());
        // Closing again is a no-op, not a panic.
        h.close();
        assert_eq!(h.state(), SessionState::Closed);
    }

    #[test]
    fn sample_feed_is_drop_oldest_and_bounded() {
        let (sink, feed) = sample_feed(2);
        let mk = |ts: u32| EncodedSample {
            data: Arc::from(ts.to_le_bytes().as_slice()),
            rtp_timestamp: ts,
            keyframe: ts == 0,
            kind: SampleKind::Video,
        };
        assert!(!sink.push(mk(0)));
        assert!(!sink.push(mk(1)));
        // Ring is full (depth 2); pushing a third evicts the oldest (ts=0).
        assert!(sink.push(mk(2)));
        assert_eq!(feed.buffered(), 2);
        assert_eq!(feed.dropped(), 1);
        assert_eq!(feed.pop().unwrap().rtp_timestamp, 1);
        assert_eq!(feed.pop().unwrap().rtp_timestamp, 2);
        assert!(feed.pop().is_none());
    }

    #[test]
    fn sample_feed_depth_is_clamped_to_one() {
        let (sink, feed) = sample_feed(0);
        let s = EncodedSample {
            data: Arc::from([1u8].as_slice()),
            rtp_timestamp: 7,
            keyframe: true,
            kind: SampleKind::Audio,
        };
        assert!(!sink.push(s.clone()));
        assert_eq!(feed.buffered(), 1);
        // A second push evicts the first — capacity floored at 1, never 0.
        assert!(sink.push(s));
        assert_eq!(feed.buffered(), 1);
    }
}
