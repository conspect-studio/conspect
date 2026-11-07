//! Live push-link state for resilient push outputs (SRT / RTMP / RIST / UDP).
//!
//! A push output's network side (connect, SRT listen/accept, write) runs on
//! its own threads and may be down for long stretches: a listener waiting for a
//! caller, a caller retrying an unreachable peer, a peer that disconnected.
//! [`PushLinkStatus`] is the lock-free shared record of that state:
//!
//! * the **sink side** (acceptor / connector / per-client writers) writes it;
//! * the **consumer side** only bumps the dropped-packet counter (an atomic,
//!   never a lock — invariant #10);
//! * the **status side** (the engine-state snapshot the control plane serves)
//!   reads a [`PushLinkSnapshot`] with atomics + a lock-free
//!   [`SwapCell`](multiview_framestore::SwapCell) load.
//!
//! [`Backoff`] is the reconnect schedule: exponential 0.5 s → 5 s with ±20 %
//! jitter, reset on a successful connect. Pure Rust, always compiled.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
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
    /// Packets dropped for this output because no peer was attached / a peer
    /// queue was full / the sink queue was full. Monotonic.
    dropped: AtomicU64,
    /// Packets written to peers. Monotonic.
    written: AtomicU64,
    /// The consumer shed a packet before the distributor saw it, so every
    /// session must resync on the next keyframe.
    resync: AtomicBool,
    /// The most recent connect/listen/write error (cleared on connect).
    last_error: SwapCell<Option<Arc<String>>>,
    /// The attached peer's address when known (caller mode: the configured
    /// target; FFmpeg's libsrt does not expose an accepted caller's address).
    peer: SwapCell<Option<Arc<String>>>,
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
    /// Packets dropped (not delivered) for this output.
    pub dropped: u64,
    /// Packets written to peers.
    pub written: u64,
    /// Last error, if any since the last successful connect.
    pub last_error: Option<String>,
    /// Peer address when known.
    pub peer: Option<String>,
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
            written: AtomicU64::new(0),
            resync: AtomicBool::new(false),
            last_error: SwapCell::new(None),
            peer: SwapCell::new(None),
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

    /// Record a connect/listen/write failure (shown until the next connect).
    pub fn set_error(&self, error: impl Into<String>) {
        self.last_error.store(Some(Arc::new(error.into())));
    }

    /// A peer attached: count the session and clear the last error.
    pub fn client_attached(&self, peer: Option<String>) {
        self.clients.fetch_add(1, Ordering::AcqRel);
        self.sessions.fetch_add(1, Ordering::AcqRel);
        self.last_error.store(None);
        self.peer.store(peer.map(Arc::new));
    }

    /// A peer detached.
    pub fn client_detached(&self) {
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

    /// Count `n` dropped packets (wait-free; safe from the consumer thread).
    pub fn add_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// Count a packet shed upstream of the distributor and request a keyframe
    /// resync (wait-free; called from the bake consumer).
    pub fn shed_upstream(&self) {
        self.add_dropped(1);
        self.resync.store(true, Ordering::Release);
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
            written: self.written.load(Ordering::Relaxed),
            last_error: self.last_error.load().map(|e| e.as_ref().clone()),
            peer: if clients > 0 {
                self.peer.load().map(|p| p.as_ref().clone())
            } else {
                None
            },
        }
    }
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
    fn status_derives_connected_and_clears_error_on_attach() {
        let s = PushLinkStatus::new("srt", true, "srt://0.0.0.0:9000?mode=listener");
        s.set_state(PushLinkState::Listening);
        s.set_error("Input/output error");
        let snap = s.snapshot();
        assert_eq!(snap.state, PushLinkState::Listening);
        assert_eq!(snap.mode, "listener");
        assert_eq!(snap.last_error.as_deref(), Some("Input/output error"));
        s.client_attached(None);
        let snap = s.snapshot();
        assert_eq!(snap.state, PushLinkState::Connected);
        assert_eq!((snap.clients, snap.sessions), (1, 1));
        assert_eq!(snap.last_error, None);
        s.client_detached();
        s.client_detached(); // saturates, never underflows
        assert_eq!(s.snapshot().state, PushLinkState::Listening);
        s.add_dropped(3);
        assert_eq!(s.snapshot().dropped, 3);
    }
}
