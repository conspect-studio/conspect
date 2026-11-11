//! Separate bounded, non-dropping compare-and-detach admission. Off-clock callers
//! retain a ticket; duplicate operation IDs return that same ticket.
use super::Generation;
use multiview_framestore::SwapCell;
use std::sync::{Arc, Mutex};

/// Idempotent detach request. Operation IDs are scoped to a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetachRequest {
    /// Caller-assigned operation identity.
    pub operation: u64,
    /// Committed generation that the caller observed.
    pub expected: Generation,
}
/// Terminal result. Retired means both native and audio ownership ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachAck {
    /// Durable precommit failed; the live binding is unchanged.
    JournalFailed,
    /// No current binding matched; nothing changed.
    Stale(Generation),
    /// Ingest exited before it could prove retirement. No retirement is implied.
    Stopped(Generation),
    /// The matching binding and both rails retired.
    Retired(Generation),
}
/// Bounded admission failure; the caller must retry, not assume acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachAdmissionError {
    /// No executor remains to service a new operation.
    Stopped,
    /// Another operation has not completed.
    Busy,
    /// The same identity names a different request.
    ConflictingOperation,
    /// Operation IDs must increase after the retained result.
    ExpiredOperation,
}
/// Durable-in-process identity and terminal acknowledgement for one operation.
pub struct DetachTicket {
    /// Exact request accepted by the player.
    pub request: DetachRequest,
    result: SwapCell<Option<Arc<DetachAck>>>,
    /// A boot intent is not a retirement acknowledgement.
    pub(crate) recovery_generation: Option<Generation>,
}
impl DetachTicket {
    /// Poll without a lock or a wait.
    pub fn acknowledgement(&self) -> Option<DetachAck> {
        self.result.load().as_ref().map(|a| **a)
    }
}
#[derive(Default)]
struct State {
    ticket: Option<Arc<DetachTicket>>,
    taken: bool,
    stopped: bool,
}
/// Capacity-one admission, independent of the transport drop-oldest queue.
#[derive(Default)]
pub struct DetachPort {
    state: Mutex<State>,
}
impl DetachPort {
    /// Restore pending intent only. The runtime must prove boot quiescence.
    pub fn recovered(request: DetachRequest, generation: Generation) -> Self {
        let ticket = Arc::new(DetachTicket {
            request,
            result: SwapCell::empty(),
            recovery_generation: Some(generation),
        });
        Self {
            state: Mutex::new(State {
                ticket: Some(ticket),
                taken: false,
                stopped: false,
            }),
        }
    }
    /// Admit exactly one operation; retain the last result for duplicate retry.
    pub fn submit(
        &self,
        request: DetachRequest,
    ) -> Result<Arc<DetachTicket>, DetachAdmissionError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ticket) = &state.ticket {
            if ticket.request.operation == request.operation {
                return if ticket.request == request {
                    Ok(Arc::clone(ticket))
                } else {
                    Err(DetachAdmissionError::ConflictingOperation)
                };
            }
            if request.operation < ticket.request.operation {
                return Err(DetachAdmissionError::ExpiredOperation);
            }
            if ticket.acknowledgement().is_none() {
                return Err(DetachAdmissionError::Busy);
            }
        }
        if state.stopped {
            return Err(DetachAdmissionError::Stopped);
        }
        let ticket = Arc::new(DetachTicket {
            request,
            result: SwapCell::empty(),
            recovery_generation: None,
        });
        state.ticket = Some(Arc::clone(&ticket));
        state.taken = false;
        Ok(ticket)
    }
    /// Ingest-side nonblocking take. No native owner crosses this seam.
    pub(crate) fn take(&self) -> Option<Arc<DetachTicket>> {
        let mut state = self.state.try_lock().ok()?;
        if state.taken {
            return None;
        }
        state.taken = true;
        state.ticket.clone()
    }
    /// Terminalize any accepted request on executor exit, including a taken ticket.
    pub(crate) fn stopped(&self, generation: Generation) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ticket) = &state.ticket {
            if ticket.acknowledgement().is_none() {
                Self::finish(ticket, DetachAck::Stopped(generation));
            }
        }
        state.taken = true;
        state.stopped = true;
    }
    pub(crate) fn finish(ticket: &DetachTicket, ack: DetachAck) {
        ticket.result.store(Some(Arc::new(ack)));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_executor_terminalizes_taken_ticket_without_retirement_claim() {
        let port = DetachPort::default();
        let request = DetachRequest {
            operation: 1,
            expected: Generation(3),
        };
        let ticket = port.submit(request).unwrap();
        assert!(port.take().is_some());
        port.stopped(Generation(4));
        assert_eq!(
            ticket.acknowledgement(),
            Some(DetachAck::Stopped(Generation(4)))
        );
        assert_eq!(
            port.submit(request).unwrap().acknowledgement(),
            ticket.acknowledgement()
        );
        assert!(matches!(
            port.submit(DetachRequest {
                operation: 2,
                ..request
            }),
            Err(DetachAdmissionError::Stopped)
        ));
        port.stopped(Generation(5));
        assert_eq!(
            ticket.acknowledgement(),
            Some(DetachAck::Stopped(Generation(4)))
        );
    }

    #[test]
    fn bounded_admission_retains_duplicate_ack_and_refuses_conflicts() {
        let port = DetachPort::default();
        let request = DetachRequest {
            operation: 4,
            expected: Generation(3),
        };
        let ticket = port.submit(request).unwrap();
        assert!(Arc::ptr_eq(&ticket, &port.submit(request).unwrap()));
        assert!(matches!(
            port.submit(DetachRequest {
                operation: 5,
                ..request
            }),
            Err(DetachAdmissionError::Busy)
        ));
        assert!(matches!(
            port.submit(DetachRequest {
                expected: Generation(2),
                ..request
            }),
            Err(DetachAdmissionError::ConflictingOperation)
        ));
        assert!(port.take().is_some());
        assert!(port.take().is_none());
        DetachPort::finish(&ticket, DetachAck::Retired(Generation(4)));
        assert_eq!(
            port.submit(request).unwrap().acknowledgement(),
            Some(DetachAck::Retired(Generation(4)))
        );
    }
}
