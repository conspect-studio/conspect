//! Opt-in, per-outgoing-session group payload acknowledgement accounting.
//! No retransmission/framing bytes, no viewer aggregation, no capacity estimate.
use std::sync::{Arc, Mutex};

/// Atomic snapshot of group stream delivery. Pending is accepted payload not yet
/// proven acknowledged, NOT allocator retained memory or QUIC bytes-in-flight.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Cumulative unique application payload bytes acknowledged after stream FIN.
    pub acknowledged: u64,
    /// Cumulative payload bytes abandoned by reset, STOP or driver cancellation.
    pub abandoned: u64,
    /// Accepted payload awaiting terminal stream state.
    pub pending: u64,
    /// Live measured streams (bounded by the transport's stream admission).
    pub streams: u64,
    /// Arithmetic overflow invalidates all measurements permanently.
    pub invalid: bool,
    /// At least one measured writer had no verified native ACK semantics.
    pub unsupported: bool,
    /// Outgoing measured group writers explicitly reset (not ACK evidence).
    pub resets: u64,
}
/// Use one fresh handle for exactly one publisher-to-relay connection.
/// Do not share it among viewer connections. Disabled unless explicitly attached
/// with `stats::Session::with_delivery` before connecting.
#[derive(Clone, Default, Debug)]
pub struct Counters(Arc<Mutex<Snapshot>>);
impl Counters {
    /// Read a consistent snapshot without resetting cumulative counters.
    pub fn snapshot(&self) -> Snapshot {
        *self.0.lock().expect("delivery poisoned")
    }
    pub(crate) fn group(&self) -> Guard {
        let mut s = self.0.lock().expect("delivery poisoned");
        match s.streams.checked_add(1) {
            Some(v) => s.streams = v,
            None => s.invalid = true,
        }
        Guard {
            counters: self.clone(),
            bytes: 0,
            done: false,
        }
    }
}
pub(crate) struct Guard {
    counters: Counters,
    bytes: u64,
    done: bool,
}
impl Guard {
    pub(crate) fn unsupported(&self) { self.counters.0.lock().expect("delivery poisoned").unsupported = true; }
    pub(crate) fn accepted(&mut self, bytes: usize) {
        let mut s = self.counters.0.lock().expect("delivery poisoned");
        match (
            self.bytes.checked_add(bytes as u64),
            s.pending.checked_add(bytes as u64),
        ) {
            (Some(own), Some(pending)) => {
                self.bytes = own;
                s.pending = pending;
            }
            _ => s.invalid = true,
        }
    }
    pub(crate) fn reset(&self) {
        let mut s = self.counters.0.lock().expect("delivery poisoned");
        match s.resets.checked_add(1) { Some(v) => s.resets = v, None => s.invalid = true }
    }
    pub(crate) fn acknowledged(&mut self) {
        self.finish(true);
    }
    fn finish(&mut self, ack: bool) {
        if self.done {
            return;
        }
        self.done = true;
        let mut s = self.counters.0.lock().expect("delivery poisoned");
        s.pending -= self.bytes;
        s.streams -= 1;
        let dst = if ack {
            &mut s.acknowledged
        } else {
            &mut s.abandoned
        };
        match dst.checked_add(self.bytes) {
            Some(v) => *dst = v,
            None => s.invalid = true,
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.finish(false);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ack_and_abandon_are_disjoint_and_release_pending() {
        let c = Counters::default();
        let mut g = c.group();
        g.accepted(100);
        assert_eq!(c.snapshot().pending, 100);
        g.acknowledged();
        drop(g);
        let mut g = c.group();
        g.accepted(50);
        drop(g);
        assert_eq!(
            c.snapshot(),
            Snapshot {
                acknowledged: 100,
                abandoned: 50,
                ..Snapshot::default()
            }
        );
    }
    #[test]
    fn untagged_origin_keeps_explicit_delivery_handle() {
        let counters = Counters::default();
        let session = crate::stats::Session::default().with_delivery(counters.clone());
        let scope = session.egress(crate::Path::new("test"));
        let mut guard = scope.meter().delivery().expect("explicit delivery retained");
        guard.accepted(12);
        guard.acknowledged();
        assert_eq!(counters.snapshot().acknowledged, 12);
    }
    #[test]
    fn connections_are_isolated() {
        let a = Counters::default();
        let b = Counters::default();
        let mut g = a.group();
        g.accepted(99);
        drop(g);
        assert_eq!(b.snapshot(), Snapshot::default());
    }
}
