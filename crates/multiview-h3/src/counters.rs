//! Lock-free counters for the HTTP/3 endpoint (scraped by the caller).

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counters. Every field only ever increments.
#[derive(Debug, Default)]
pub struct Counters {
    /// Connections whose handshake completed.
    pub connections_accepted: AtomicU64,
    /// Incoming connections refused by the global or per-source cap.
    pub connections_refused: AtomicU64,
    /// Incoming connections refused because `max_handshakes` (or the
    /// per-source handshake cap) were in progress.
    pub handshakes_refused: AtomicU64,
    /// Handshakes that failed or exceeded the handshake deadline.
    pub handshakes_failed: AtomicU64,
    /// Requests handed to the [`Site`](crate::Site).
    pub requests: AtomicU64,
    /// Request streams rejected because `max_concurrent_requests` were in
    /// flight on the connection.
    pub requests_rejected: AtomicU64,
    /// Streams failed by the header-block guard (oversized declared frame).
    pub header_guard_rejections: AtomicU64,
    /// Request streams failed by the per-request header deadline.
    pub header_deadline_rejections: AtomicU64,
    /// Connections closed by the control-stream guard or its deadlines
    /// (oversized control frame, SETTINGS late, partial frame too old,
    /// QPACK stream over its byte cap).
    pub control_stream_rejections: AtomicU64,
    /// Requests whose site did not return the response head within
    /// `H3Limits::response_head_timeout`.
    pub response_timeouts: AtomicU64,
    /// WebTransport sessions accepted (200 sent).
    pub wt_sessions: AtomicU64,
    /// Extended `CONNECTs` answered 429 (the one-session-per-connection rule).
    pub wt_429: AtomicU64,
    /// WebTransport streams reset because the session's bounded queue was
    /// full, or no session matched.
    pub wt_streams_rejected: AtomicU64,
    /// Chunks the adapter read from peer QPACK streams (drained, never
    /// decoded): bounded by the per-poll budget and the lifetime byte cap.
    pub qpack_chunks_read: AtomicU64,
    /// Accept-path polls of the QPACK drain that stopped on the per-poll
    /// chunk budget with data still buffered.
    pub qpack_budget_yields: AtomicU64,
    /// The most chunks one QPACK-drain poll read from one stream (a
    /// high-water mark, not a sum): at most the per-poll budget.
    pub qpack_max_chunks_per_poll: AtomicU64,
}

impl Counters {
    pub(crate) fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn max(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    /// Reads one counter.
    #[must_use]
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}
