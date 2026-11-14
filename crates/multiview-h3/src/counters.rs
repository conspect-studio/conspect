//! Lock-free counters for the HTTP/3 endpoint (scraped by the caller).

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counters. Every field only ever increments.
#[derive(Debug, Default)]
pub struct Counters {
    /// Connections whose handshake completed.
    pub connections_accepted: AtomicU64,
    /// Incoming connections refused by the global or per-source cap.
    pub connections_refused: AtomicU64,
    /// Incoming connections refused by [`crate::endpoint::SourceFilter`]
    /// before any cap check (a forwarder hop whose source must fail
    /// closed, design note §8).
    pub sources_refused: AtomicU64,
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
    /// Extended `CONNECTs` refused for any reason (429 included), plus
    /// admitted ones whose `200` could not be sent: every one is also
    /// logged, up to [`REFUSAL_LOGS_PER_SECOND`] lines per second.
    pub wt_refused: AtomicU64,
    /// Refusal log lines suppressed by the per-second budget (they are
    /// still counted in `wt_refused`; the next logged line reports them).
    pub wt_refusal_logs_suppressed: AtomicU64,
    /// The second (since the endpoint started) the refusal log budget
    /// belongs to, and how much of it was used.
    refusal_log_window: AtomicU64,
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

/// Refused-CONNECT log lines per second, endpoint-wide: a peer that
/// hammers CONNECT cannot flood the log (the counter still counts all).
pub const REFUSAL_LOGS_PER_SECOND: u64 = 20;

impl Counters {
    /// Counts one refused CONNECT and decides whether it may be logged now.
    /// `now_secs` is a monotonic second count. Returns the number of lines
    /// suppressed since the last logged one when logging is allowed.
    pub(crate) fn refusal(&self, now_secs: u64) -> Option<u64> {
        Self::inc(&self.wt_refused);
        // window = (second << 8) | used, used <= REFUSAL_LOGS_PER_SECOND.
        let mut window = self.refusal_log_window.load(Ordering::Relaxed);
        let allowed = loop {
            let (second, used) = (window >> 8, window & 0xff);
            // Only ever forward: a caller that sampled an older second (and
            // lost a race to a newer one) is charged to the current window.
            let next = if now_secs > second {
                (now_secs << 8) | 1
            } else if used < REFUSAL_LOGS_PER_SECOND {
                window + 1
            } else {
                break false;
            };
            match self.refusal_log_window.compare_exchange_weak(
                window,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break true,
                Err(current) => window = current,
            }
        };
        if allowed {
            Some(self.wt_refusal_logs_suppressed.swap(0, Ordering::Relaxed))
        } else {
            Self::inc(&self.wt_refusal_logs_suppressed);
            None
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_logging_is_bounded_per_second_and_counts_everything() {
        let c = Counters::default();
        let logged = (0..100).filter(|_| c.refusal(7).is_some()).count();
        assert_eq!(u64::try_from(logged).unwrap(), REFUSAL_LOGS_PER_SECOND);
        assert_eq!(Counters::get(&c.wt_refused), 100);
        assert_eq!(Counters::get(&c.wt_refusal_logs_suppressed), 80);
        // The next second logs again and reports what was suppressed.
        assert_eq!(c.refusal(8), Some(80));
        assert_eq!(Counters::get(&c.wt_refusal_logs_suppressed), 0);
        // A late caller that sampled second 7 is charged to second 8's
        // budget; it never rolls the window back and refills it.
        let late = (0..100).filter(|_| c.refusal(7).is_some()).count();
        assert_eq!(u64::try_from(late).unwrap(), REFUSAL_LOGS_PER_SECOND - 1);
        assert!(c.refusal(8).is_none(), "second 8 got a fresh budget");
    }

    #[test]
    fn refusal_logging_is_bounded_under_concurrency() {
        let c = std::sync::Arc::new(Counters::default());
        let logged = std::sync::Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let (c, logged) = (std::sync::Arc::clone(&c), std::sync::Arc::clone(&logged));
                std::thread::spawn(move || {
                    for i in 0..1000u64 {
                        // Interleaved stale and fresh seconds 0..=3.
                        let second = (i / 250 + t % 2) % 4;
                        if c.refusal(second).is_some() {
                            logged.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // At most one budget per distinct second ever opened (0..=3).
        assert!(Counters::get(&logged) <= 4 * REFUSAL_LOGS_PER_SECOND);
        assert_eq!(Counters::get(&c.wt_refused), 8000);
    }
}
