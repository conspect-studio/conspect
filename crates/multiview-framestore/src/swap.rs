//! A safe atomic-`Arc` cell with pointer-swap-sized critical sections.
//!
//! [`SwapCell<T>`] is the storage behind [`LatestSlot`](crate::LatestSlot) and
//! the [`TileStore`](crate::TileStore) media-time ring. It replaces
//! `arc_swap::ArcSwap{,Option}` on the invariant-#2 path because arc-swap's
//! default (hybrid) strategy can return an `Arc` that was published into a
//! **different** cell (upstream [vorner/arc-swap#210]; #133, ADR-T002
//! addendum). Its process-global debt list is keyed by raw address. When a
//! value freed from cell A has its address reused for a value stored into
//! cell B, B's writer can "pay" an in-flight debt that a reader of A holds.
//! That reader then accepts the pointer as already protected and returns B's
//! value. Under a single writer this looked like a sequence regression
//! (`6613 < 7357`), but it is a foreign read, and with different payload types
//! it would be type confusion.
//!
//! [vorner/arc-swap#210]: https://github.com/vorner/arc-swap/issues/210
//!
//! ## Bound (invariants #1 / #10)
//!
//! The cell is a [`std::sync::RwLock`] around an `Arc`. Every critical section
//! is a constant-time pointer operation:
//!
//! * a **reader** holds the shared lock only for one `Arc::clone` (one atomic
//!   increment);
//! * a **writer** holds the exclusive lock only for one `mem::replace` of the
//!   pointer (plus, for [`SwapCell::rcu`], one pointer comparison).
//!
//! No critical section allocates, frees, runs a destructor, does I/O, or calls
//! user code. The value being replaced is dropped **after** the guard is
//! released, and `rcu` computes the new value **outside** the lock. So neither
//! side can wait on the other for longer than a few-instruction critical
//! section. The only unbounded case is the OS preempting the lock holder inside
//! that window, which costs one scheduler slice and only when it hits those few
//! nanoseconds. The cell never waits on another party's progress: a slow,
//! wedged or gone reader cannot hold the lock, because it never holds the lock
//! outside the clone.
//!
//! ## Ordering
//!
//! Lock acquire/release gives every load and store a single total order (the
//! lock's), and a thread's later `load` happens-after its earlier one. So a
//! single reader looping on [`SwapCell::load`] can never get a value older than
//! one it has already seen. Each load returns exactly the `Arc` most recently
//! stored into **this** cell; there is no shared reclamation state between
//! cells.
//!
//! A panic cannot happen while the lock is held (`Arc::clone` aborts rather
//! than panicking on overflow, and `mem::replace` cannot panic), so poisoning is unreachable. It is still tolerated
//! (`into_inner`) so a poisoned lock can never turn into a panic on the
//! data plane.
use std::sync::{Arc, PoisonError, RwLock};

/// A thread-safe cell holding a cheaply-clonable pointer `P` (`Arc<T>` or
/// `Option<Arc<T>>`). Reads clone the pointer; writes swap it. See the
/// [module docs](self) for the blocking bound.
///
/// Crate-private: `P::clone` runs inside the read lock, so `P` must be a
/// pointer whose clone is one atomic increment (never a deep copy).
#[derive(Debug, Default)]
pub(crate) struct SwapCell<P> {
    inner: RwLock<P>,
}

impl<P: Clone> SwapCell<P> {
    /// Create a cell holding `value`.
    pub(crate) fn new(value: P) -> Self {
        Self {
            inner: RwLock::new(value),
        }
    }

    /// The current value, cloned. The critical section is a single pointer
    /// clone (one atomic increment).
    pub(crate) fn load(&self) -> P {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the current value with `value` and return the previous one. The
    /// critical section is one pointer swap. The previous value is returned,
    /// and dropped by the caller, **outside** the lock.
    pub(crate) fn swap(&self, value: P) -> P {
        let mut guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        core::mem::replace(&mut *guard, value)
    }

    /// Replace the current value with `value`. The previous value is dropped
    /// after the lock is released, so its destructor never runs inside the
    /// critical section.
    pub(crate) fn store(&self, value: P) {
        drop(self.swap(value));
    }
}

impl<T> SwapCell<Arc<T>> {
    /// Read-copy-update: compute `f(&current)` **outside** the lock, then
    /// install it only if `current` is still the stored value. Retry if
    /// another writer got in first. With a single writer (the
    /// [`TileStore`](crate::TileStore) contract) this always succeeds on the
    /// first pass. Returns the value that was replaced.
    pub(crate) fn rcu<F>(&self, mut f: F) -> Arc<T>
    where
        F: FnMut(&Arc<T>) -> T,
    {
        let mut current = self.load();
        loop {
            let next = Arc::new(f(&current));
            let mut guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
            if Arc::ptr_eq(&guard, &current) {
                return core::mem::replace(&mut *guard, next);
            }
            let latest = Arc::clone(&guard);
            drop(guard);
            // `next` is dropped here, outside the lock.
            current = latest;
        }
    }
}
