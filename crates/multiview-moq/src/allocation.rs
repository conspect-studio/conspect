//! Family-scoped retained allocation reservations, independent of network ACK counters.
//! Attach the returned reservation to the actual serialized Bytes::from_owner owner;
//! never drop it at enqueue/FIN/ACK while payload clones or slices still retain memory.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Clone)]
pub struct AllocationBudget(Arc<Account>);
#[derive(Debug)]
struct Account {
    used: AtomicUsize,
    limit: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocationLimit;
#[derive(Debug)]
pub struct Reservation {
    account: Arc<Account>,
    bytes: usize,
}
impl AllocationBudget {
    pub fn new(limit: usize) -> Result<Self, AllocationLimit> {
        if limit == 0 {
            return Err(AllocationLimit);
        }
        Ok(Self(Arc::new(Account {
            used: AtomicUsize::new(0),
            limit,
        })))
    }
    pub fn limit(&self) -> usize {
        self.0.limit
    }
    pub fn retained_bytes(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
    /// Reserve the complete allocation capacity BEFORE allocating. On failure no
    /// account mutation occurs; other admission reservations unwind by RAII.
    pub fn reserve(&self, bytes: usize) -> Result<Reservation, AllocationLimit> {
        self.0
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|sum| *sum <= self.0.limit)
            })
            .map_err(|_| AllocationLimit)?;
        Ok(Reservation {
            account: self.0.clone(),
            bytes,
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.account.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Owner {
        data: Vec<u8>,
        _reservation: Reservation,
    }
    impl AsRef<[u8]> for Owner {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    #[test]
    fn last_payload_clone_and_slice_own_reservation() {
        let budget = AllocationBudget::new(100).unwrap();
        let reservation = budget.reserve(100).unwrap();
        let payload = bytes::Bytes::from_owner(Owner {
            data: vec![1; 100],
            _reservation: reservation,
        });
        let clone = payload.clone();
        let slice = clone.slice(2..3);
        drop(payload);
        drop(clone);
        assert_eq!(budget.retained_bytes(), 100);
        assert!(budget.reserve(1).is_err());
        drop(slice);
        assert_eq!(budget.retained_bytes(), 0);
    }
    #[test]
    fn rejected_reservations_do_not_leak_or_wrap() {
        let budget = AllocationBudget::new(10).unwrap();
        assert!(budget.reserve(usize::MAX).is_err());
        let charge = budget.reserve(8).unwrap();
        assert!(budget.reserve(3).is_err());
        assert_eq!(budget.retained_bytes(), 8);
        drop(charge);
        assert_eq!(budget.retained_bytes(), 0);
    }
    #[test]
    fn family_accounts_are_independent_and_shared_clones_are_one_account() {
        let a = AllocationBudget::new(10).unwrap();
        let b = AllocationBudget::new(10).unwrap();
        let charge = a.clone().reserve(7).unwrap();
        assert_eq!(a.retained_bytes(), 7);
        assert_eq!(b.retained_bytes(), 0);
        drop(charge);
        assert_eq!(a.retained_bytes(), 0);
    }
}
