//! Live push-link state for resilient push outputs (SRT / RTMP / RIST / UDP).
//!
//! A push output's network side (connect, SRT listen/accept, write) runs on
//! its own threads and may be down for long stretches: a listener waiting for a
//! caller, a caller retrying an unreachable peer, a peer that disconnected.
//! [`PushLinkStatus`] is the lock-free shared record of that state:
//!
//! * the **sink side** (acceptor / connector / per-client writers) writes it;
//! * the **consumer side** only bumps the drop/discard counters (atomics, never
//!   a lock — invariant #10); each drop is attributed to one reason so a
//!   listener waiting for a caller is never reported as a delivery failure;
//! * the **status side** (the engine-state snapshot the control plane serves)
//!   reads a [`PushLinkSnapshot`] with atomics + a lock-free
//!   [`SwapCell`](multiview_framestore::SwapCell) load.
//!
//! [`Backoff`] is the reconnect schedule: exponential 0.5 s → 5 s with ±20 %
//! jitter, reset on a successful connect. Pure Rust, always compiled.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use multiview_framestore::SwapCell;

/// The coarse state of one push output's network link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushLinkState {
    /// Not started yet (or the run ended).
    Idle,
    /// A caller-mode push is connecting to its peer.
    Connecting,
    /// An SRT listener is bound and waiting for a caller (no caller attached).
    Listening,
    /// At least one peer is attached and receiving the program.
    Connected,
    /// The last connect/listen/write failed; backing off before retrying.
    Reconnecting,
}

impl PushLinkState {
    /// The stable wire token (`snake_case`) used by the status API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Listening => "listening",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
        }
    }

    const fn to_u8(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Connecting => 1,
            Self::Listening => 2,
            Self::Connected => 3,
            Self::Reconnecting => 4,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connecting,
            2 => Self::Listening,
            3 => Self::Connected,
            4 => Self::Reconnecting,
            _ => Self::Idle,
        }
    }
}

/// The shared, lock-free status of one push output's link.
#[derive(Debug)]
pub struct PushLinkStatus {
    /// `"srt"` / `"rtmp"` / `"rist"` / `"udp"` — for the status surface.
    transport: &'static str,
    /// `"caller"` or `"listener"`.
    mode: &'static str,
    /// The redacted target / bind URL (never carries a passphrase).
    url: String,
    /// The acceptor/connector state (Listening / Connecting / Reconnecting /
    /// Idle). `Connected` is derived from `clients > 0` at snapshot time.
    state: AtomicU8,
    /// Currently attached peers (writers with an open session).
    clients: AtomicUsize,
    /// Total sessions ever established (connects + accepts).
    sessions: AtomicU64,
    /// Packets dropped for this output, for **any** reason (no peer attached /
    /// waiting for a keyframe / slow peer / upstream shed). Monotonic; by
    /// construction the sum of the four reason counters below.
    dropped: AtomicU64,
    /// Packets discarded because **no peer was attached** (a listener waiting
    /// for a caller, or a caller between sessions). Expected and benign — it is
    /// not a delivery failure. Part of `dropped`.
    discarded_no_client: AtomicU64,
    /// Packets discarded because the session was **waiting for a video
    /// keyframe** (a fresh session, or a post-overflow resync). Expected and
    /// benign. Part of `dropped`.
    discarded_awaiting_keyframe: AtomicU64,
    /// Packets dropped because a **peer's bounded queue was full** (a slow
    /// peer). A real delivery failure. Part of `dropped`.
    dropped_slow_peer: AtomicU64,
    /// Packets dropped **upstream of the distributor** (the producer shed a
    /// packet or held its post-loss gate). Part of `dropped`.
    dropped_upstream: AtomicU64,
    /// Packets written to peers. Monotonic.
    written: AtomicU64,
    /// The consumer shed a packet before the distributor saw it (observable
    /// once through [`PushLinkStatus::take_resync`]).
    resync: AtomicBool,
    /// Producer-side gate after an upstream shed: nothing more is queued until
    /// a post-loss video keyframe is, so the loss boundary is enforced where
    /// it happens rather than against older queued packets (review SRT-04).
    /// Touched only by the one bake consumer thread.
    upstream_gated: AtomicBool,
    /// The most recent **link-level** connect/listen error (cleared by the next
    /// successful bind/connect). A peer session ending is NOT recorded here: it
    /// is history, not current link health (see `last_session_error`).
    last_error: SwapCell<Option<Arc<String>>>,
    /// How the most recent peer session ended — **historical**, never a current
    /// health alarm. Cleared when the next session attaches and ignored while a
    /// newer peer is already attached.
    last_session_error: SwapCell<Option<Arc<String>>>,
    /// Attached peer addresses, published for lock-free reads by the snapshot.
    peers: SwapCell<Arc<Vec<String>>>,
    /// Serialises the sink-side threads' read-modify-write of `peers`. Never
    /// taken by the consumer or the snapshot.
    peers_edit: Mutex<Vec<String>>,
}

/// A point-in-time copy of a [`PushLinkStatus`] for the status API.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PushLinkSnapshot {
    /// Transport label (`srt` / `rtmp` / `rist` / `udp`).
    pub transport: &'static str,
    /// `caller` or `listener`.
    pub mode: &'static str,
    /// Redacted URL.
    pub url: String,
    /// Effective state (`connected` whenever a peer is attached).
    pub state: PushLinkState,
    /// Attached peers.
    pub clients: usize,
    /// Sessions established so far.
    pub sessions: u64,
    /// Packets dropped (not delivered) for this output, for any reason. The sum
    /// of the four reason counters below.
    pub dropped: u64,
    /// Packets discarded because no peer was attached (benign: a listener
    /// waiting for a caller, or a caller between sessions).
    pub discarded_no_client: u64,
    /// Packets discarded while the session waited for a video keyframe (benign:
    /// session start, or a post-overflow resync).
    pub discarded_awaiting_keyframe: u64,
    /// Packets dropped on a slow peer's full bounded queue (a real failure).
    pub dropped_slow_peer: u64,
    /// Packets dropped upstream of the distributor (producer shed / post-loss
    /// gate).
    pub dropped_upstream: u64,
    /// Packets written to peers.
    pub written: u64,
    /// The current link error, if any since the last successful connect. Never
    /// a peer session ending — see `last_session_error`.
    pub last_error: Option<String>,
    /// How the most recent peer session ended, if it ended with an error (and
    /// no newer peer is attached). Historical context, not current link health.
    pub last_session_error: Option<String>,
    /// Attached peer addresses (an SRT listener's callers; a caller's target).
    pub peers: Vec<String>,
}

impl PushLinkStatus {
    /// A fresh `Idle` status for one push output.
    #[must_use]
    pub fn new(transport: &'static str, listener: bool, url: impl Into<String>) -> Self {
        Self {
            transport,
            mode: if listener { "listener" } else { "caller" },
            url: url.into(),
            state: AtomicU8::new(PushLinkState::Idle.to_u8()),
            clients: AtomicUsize::new(0),
            sessions: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            discarded_no_client: AtomicU64::new(0),
            discarded_awaiting_keyframe: AtomicU64::new(0),
            dropped_slow_peer: AtomicU64::new(0),
            dropped_upstream: AtomicU64::new(0),
            written: AtomicU64::new(0),
            resync: AtomicBool::new(false),
            upstream_gated: AtomicBool::new(false),
            last_error: SwapCell::new(None),
            last_session_error: SwapCell::new(None),
            peers: SwapCell::new(Arc::new(Vec::new())),
            peers_edit: Mutex::new(Vec::new()),
        }
    }

    /// Whether this link is in listener mode.
    #[must_use]
    pub fn is_listener(&self) -> bool {
        self.mode == "listener"
    }

    /// Set the acceptor/connector state.
    pub fn set_state(&self, state: PushLinkState) {
        self.state.store(state.to_u8(), Ordering::Release);
    }

    /// Record a **link-level** connect/listen failure (shown until the next
    /// successful bind/connect). A peer session ending does NOT belong here:
    /// use [`set_session_error`](Self::set_session_error).
    pub fn set_error(&self, error: impl Into<String>) {
        self.last_error.store(Some(Arc::new(error.into())));
    }

    /// Clear the current link error: a successful bind (listener) or attach
    /// (caller) resolved it. Never touches the historical session error.
    pub fn clear_error(&self) {
        self.last_error.store(None);
    }

    /// Record how a peer session ended. Historical by construction: this never
    /// becomes the current link error, and it is ignored when a newer peer is
    /// already attached (that session supersedes the ending one). Cleared by
    /// the next successful attach.
    pub fn set_session_error(&self, error: impl Into<String>) {
        if self.clients() > 0 {
            return;
        }
        self.last_session_error.store(Some(Arc::new(error.into())));
    }

    /// How the most recent peer session ended, if it did (`None` once the next
    /// session attaches). Read by caller mode, where the session's death *is*
    /// the reason for the reconnect.
    #[must_use]
    pub fn session_error(&self) -> Option<Arc<String>> {
        self.last_session_error.load()
    }

    /// Move the last session-end error into the current link error — caller
    /// mode: the session died, so its error is the live reason for the
    /// reconnect (a listener instead returns to `Listening` and keeps it
    /// historical). No-op when there is none.
    pub fn promote_session_error(&self) {
        if let Some(error) = self.last_session_error.load() {
            self.last_error.store(Some(error));
            self.last_session_error.store(None);
        }
    }

    /// A peer attached: count the session, clear the current error and retire
    /// the previous session's ending error (a successful connection supersedes
    /// both).
    pub fn client_attached(&self, peer: Option<String>) {
        self.clients.fetch_add(1, Ordering::AcqRel);
        self.sessions.fetch_add(1, Ordering::AcqRel);
        self.last_error.store(None);
        self.last_session_error.store(None);
        if let Some(peer) = peer {
            self.edit_peers(|peers| peers.push(peer));
        }
    }

    fn edit_peers(&self, edit: impl FnOnce(&mut Vec<String>)) {
        let mut peers = self
            .peers_edit
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        edit(&mut peers);
        self.peers.store(Arc::new(peers.clone()));
    }

    /// A peer detached.
    pub fn client_detached(&self, peer: Option<&str>) {
        if let Some(peer) = peer {
            self.edit_peers(|peers| {
                if let Some(at) = peers.iter().position(|p| p == peer) {
                    peers.swap_remove(at);
                }
            });
        }
        let mut current = self.clients.load(Ordering::Acquire);
        while current > 0 {
            match self.clients.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// Attached peers right now (the consumer-side "is anyone listening" test).
    #[must_use]
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Acquire)
    }

    /// Count `n` dropped packets toward the total only, with **no** reason
    /// attribution (wait-free; safe from the consumer thread). Prefer the
    /// reason-specific counters so the total stays decomposable.
    pub fn add_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// Count a packet discarded because no peer is attached (expected: a
    /// listener waiting for a caller). Wait-free.
    pub fn discard_no_client(&self) {
        self.add_dropped(1);
        self.discarded_no_client.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a packet discarded while a session waits for a video keyframe
    /// (fresh session or post-overflow resync). Wait-free.
    pub fn discard_awaiting_keyframe(&self) {
        self.add_dropped(1);
        self.discarded_awaiting_keyframe
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count a packet dropped on a slow peer's full bounded queue. Wait-free.
    pub fn drop_slow_peer(&self) {
        self.add_dropped(1);
        self.dropped_slow_peer.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a packet dropped upstream of the distributor. Wait-free.
    fn drop_upstream(&self) {
        self.add_dropped(1);
        self.dropped_upstream.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a packet shed upstream of the distributor and request a keyframe
    /// resync (wait-free; called from the bake consumer).
    pub fn shed_upstream(&self) {
        self.drop_upstream();
        self.upstream_gated.store(true, Ordering::Release);
        self.resync.store(true, Ordering::Release);
    }

    /// Whether the producer may queue a packet now. After
    /// [`shed_upstream`](Self::shed_upstream) only a video keyframe is
    /// admitted; anything else is counted as dropped. Wait-free.
    pub fn admit_upstream(&self, video_keyframe: bool) -> bool {
        if !self.upstream_gated.load(Ordering::Acquire) || video_keyframe {
            return true;
        }
        self.drop_upstream();
        false
    }

    /// The producer queued a packet: a queued video keyframe ends the
    /// post-loss gate.
    pub fn queued_upstream(&self, video_keyframe: bool) {
        if video_keyframe {
            self.upstream_gated.store(false, Ordering::Release);
        }
    }

    /// Take a pending keyframe-resync request.
    pub fn take_resync(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }

    /// Count one written packet.
    pub fn add_written(&self) {
        self.written.fetch_add(1, Ordering::Relaxed);
    }

    /// Packets dropped so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Lock-free point-in-time copy for the status API.
    #[must_use]
    pub fn snapshot(&self) -> PushLinkSnapshot {
        let clients = self.clients();
        let raw = PushLinkState::from_u8(self.state.load(Ordering::Acquire));
        let state = if clients > 0 {
            PushLinkState::Connected
        } else {
            raw
        };
        PushLinkSnapshot {
            transport: self.transport,
            mode: self.mode,
            url: self.url.clone(),
            state,
            clients,
            sessions: self.sessions.load(Ordering::Acquire),
            dropped: self.dropped(),
            discarded_no_client: self.discarded_no_client.load(Ordering::Relaxed),
            discarded_awaiting_keyframe: self.discarded_awaiting_keyframe.load(Ordering::Relaxed),
            dropped_slow_peer: self.dropped_slow_peer.load(Ordering::Relaxed),
            dropped_upstream: self.dropped_upstream.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            last_error: self.last_error.load().map(|e| e.as_ref().clone()),
            last_session_error: self.last_session_error.load().map(|e| e.as_ref().clone()),
            peers: self.peers.load().as_ref().clone(),
        }
    }
}

/// A display-safe form of a push URL (see
/// [`multiview_core::redact::redact_transport_url`]): no query (passphrase,
/// stream id), no userinfo, no non-RTMP path, and for RTMP no stream key (the
/// final path segment, even when it is the only one).
#[must_use]
pub fn redact_target(url: &str) -> String {
    multiview_core::redact::redact_transport_url(url)
}

/// Exponential reconnect back-off: `min` doubling to `max`, ±20 % jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
    rng: u64,
}

impl Backoff {
    /// The production schedule: 0.5 s → 5 s.
    #[must_use]
    pub fn standard(seed: u64) -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(5), seed)
    }

    /// A schedule from `min` to `max` with a jitter `seed`.
    #[must_use]
    pub fn new(min: Duration, max: Duration, seed: u64) -> Self {
        Self {
            min,
            max: max.max(min),
            next: min,
            rng: seed | 1,
        }
    }

    /// Reset to `min` after a successful connect.
    pub fn reset(&mut self) {
        self.next = self.min;
    }

    /// The next delay (jittered), advancing the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next.saturating_mul(2)).min(self.max);
        // xorshift64 — jitter only, not security-relevant.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        // A factor in [80, 120] percent.
        let pct = 80 + (self.rng % 41);
        let ms = u64::try_from(base.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(pct)
            / 100;
        Duration::from_millis(ms).min(self.max.saturating_mul(6) / 5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_to_cap_with_bounded_jitter_and_resets() {
        let mut b = Backoff::standard(42);
        let delays: Vec<u128> = (0..8).map(|_| b.next_delay().as_millis()).collect();
        assert!((400..=600).contains(&delays[0]), "{delays:?}");
        assert!((800..=1200).contains(&delays[1]), "{delays:?}");
        for d in &delays[4..] {
            assert!((4000..=6000).contains(d), "capped near 5 s: {delays:?}");
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(600));
    }

    #[test]
    fn redaction_removes_every_credential_shape() {
        for (url, want) in [
            (
                "rtmp://live.example/app/sk_secret",
                "rtmp://live.example/app/***",
            ),
            (
                "rtmps://u:pw@live.example:443/a/b/sk?x=1",
                "rtmps://live.example:443/a/b/***",
            ),
            (
                "srt://[::]:9000?mode=listener&passphrase=***",
                "srt://[::]:9000",
            ),
            (
                "rist://u:pw@[2001:db8::1]:8000?secret=x",
                "rist://[2001:db8::1]:8000",
            ),
            ("not a url with secret", "***"),
            // SRT-03: a single-segment RTMP path IS the stream key.
            ("rtmp://live.example/sk_secret", "rtmp://live.example/***"),
            ("rtmp://live.example/sk_secret/", "rtmp://live.example/***"),
            (
                "rtmp://live.example/app/sk_secret//",
                "rtmp://live.example/app/***",
            ),
            // Non-RTMP paths carry no meaning for the link view; never shown.
            (
                "rist://[2001:db8::1]:8000/sk_secret",
                "rist://[2001:db8::1]:8000",
            ),
            ("srt://[::1]:9000/sk_secret?streamid=x", "srt://[::1]:9000"),
        ] {
            assert_eq!(redact_target(url), want);
        }
    }

    #[test]
    fn status_derives_connected_and_clears_error_on_attach() {
        let s = PushLinkStatus::new("srt", true, "srt://0.0.0.0:9000?mode=listener");
        s.set_state(PushLinkState::Listening);
        s.set_error("Input/output error");
        let snap = s.snapshot();
        assert_eq!(snap.state, PushLinkState::Listening);
        assert_eq!(snap.mode, "listener");
        assert_eq!(snap.last_error.as_deref(), Some("Input/output error"));
        s.client_attached(Some("[::1]:5000".to_owned()));
        let snap = s.snapshot();
        assert_eq!(snap.state, PushLinkState::Connected);
        assert_eq!((snap.clients, snap.sessions), (1, 1));
        assert_eq!(snap.last_error, None);
        assert_eq!(snap.peers, vec!["[::1]:5000".to_owned()]);
        s.client_detached(Some("[::1]:5000"));
        s.client_detached(None); // saturates, never underflows
        assert_eq!(s.snapshot().peers, Vec::<String>::new());
        assert_eq!(s.snapshot().state, PushLinkState::Listening);
        s.add_dropped(3);
        assert_eq!(s.snapshot().dropped, 3);
    }

    #[test]
    fn drop_reasons_are_counted_separately_and_sum_to_the_total() {
        let s = PushLinkStatus::new("srt", true, "srt://[::]:9000");
        for _ in 0..7 {
            s.discard_no_client();
        }
        for _ in 0..3 {
            s.discard_awaiting_keyframe();
        }
        s.drop_slow_peer();
        s.shed_upstream();
        s.shed_upstream();
        assert!(
            !s.admit_upstream(false),
            "post-shed gate holds non-keyframes"
        );
        assert!(s.admit_upstream(true), "a keyframe is admitted");

        let snap = s.snapshot();
        assert_eq!(snap.discarded_no_client, 7);
        assert_eq!(snap.discarded_awaiting_keyframe, 3);
        assert_eq!(snap.dropped_slow_peer, 1);
        // Two explicit sheds plus the held post-loss gate.
        assert_eq!(snap.dropped_upstream, 3);
        assert_eq!(snap.dropped, 7 + 3 + 1 + 3, "the total stays the sum");
    }

    #[test]
    fn a_session_end_error_is_history_never_current_health() {
        let s = PushLinkStatus::new("srt", true, "srt://[::]:9000");
        s.set_state(PushLinkState::Listening);
        // The caller hung up: the session ended with an I/O error.
        s.set_session_error("Input/output error");
        let snap = s.snapshot();
        assert_eq!(snap.state, PushLinkState::Listening);
        assert_eq!(snap.last_error, None, "not a current link-health alarm");
        assert_eq!(
            snap.last_session_error.as_deref(),
            Some("Input/output error"),
            "kept as history"
        );
        // A session ending while a newer peer is already attached is superseded.
        s.client_attached(Some("[::1]:5000".to_owned()));
        assert_eq!(s.snapshot().last_session_error, None);
        assert_eq!(s.snapshot().last_error, None);
        s.set_session_error("late error from the superseded session");
        assert_eq!(
            s.snapshot().last_session_error,
            None,
            "a newer attached peer supersedes the ending one"
        );
        // The next successful connection clears everything anyway.
        s.client_detached(Some("[::1]:5000"));
        s.set_session_error("detached session error");
        assert!(s.snapshot().last_session_error.is_some());
        s.client_attached(None);
        let snap = s.snapshot();
        assert_eq!(snap.last_error, None);
        assert_eq!(snap.last_session_error, None);
        assert_eq!(snap.clients, 1);
    }

    #[test]
    fn caller_mode_promotes_a_session_end_error_to_the_current_reason() {
        let s = PushLinkStatus::new("srt", false, "srt://[::1]:9000");
        s.set_state(PushLinkState::Connecting);
        s.client_attached(None);
        s.client_detached(None);
        s.set_session_error("Broken pipe");
        assert_eq!(
            s.snapshot().last_error,
            None,
            "still history while deciding"
        );
        assert_eq!(
            s.session_error().as_deref().map(String::as_str),
            Some("Broken pipe")
        );
        s.promote_session_error();
        let snap = s.snapshot();
        assert_eq!(snap.last_error.as_deref(), Some("Broken pipe"));
        assert_eq!(snap.last_session_error, None, "promoted, not duplicated");
        // Idempotent: nothing left to promote.
        s.clear_error();
        s.promote_session_error();
        assert_eq!(s.snapshot().last_error, None);
    }

    #[test]
    fn a_successful_bind_clears_the_link_error_but_not_session_history() {
        let s = PushLinkStatus::new("srt", true, "srt://[::]:9000");
        s.set_error("transient bind failure");
        assert!(s.snapshot().last_error.is_some());
        s.set_session_error("Input/output error");
        s.clear_error();
        let snap = s.snapshot();
        assert_eq!(snap.last_error, None, "the bind recovered");
        assert_eq!(
            snap.last_session_error.as_deref(),
            Some("Input/output error"),
            "history is untouched by a link-level clear"
        );
    }
}
