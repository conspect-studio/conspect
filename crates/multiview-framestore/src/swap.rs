//! A safe, publisher-wait-free atomic-`Arc` cell.
//!
//! [`SwapCell<P>`] is the storage behind [`LatestSlot`](crate::LatestSlot) and
//! the [`TileStore`](crate::TileStore) media-time ring. It replaces
//! `arc_swap::ArcSwap{,Option}` on the invariant-#2 path because arc-swap's
//! default (hybrid) strategy can return an `Arc` that was published into a
//! **different** cell (upstream [vorner/arc-swap#210]; #133, ADR-T002
//! addendum). Its process-global debt list is keyed by raw address. When a
//! value freed from cell A has its address reused for a value stored into
//! cell B, B's writer can "pay" an in-flight debt that a reader of A holds.
//! That reader then accepts the pointer as already protected and returns B's
//! value.
//!
//! [vorner/arc-swap#210]: https://github.com/vorner/arc-swap/issues/210
//!
//! ## Design: a slot ring with a generation-tagged head
//!
//! The cell owns a growable set of slots (segments of [`SEGMENT`] slots,
//! chained through [`OnceLock`]). Each slot is a [`RwLock`] around an
//! `(generation, value)` entry. One [`AtomicU64`] `head` names the slot that
//! holds the current value and that value's generation.
//!
//! * **Publish** takes a fresh generation, then claims a slot with
//!   [`RwLock::try_write`], starting just after the head slot and skipping
//!   every slot it cannot take at once (a reader is inside it) and the head
//!   slot itself. It writes `(generation, value)` into the claimed slot,
//!   releases it, and publishes with `head.fetch_max`. If every slot is busy,
//!   it appends a new segment and claims a slot there. So **the publisher
//!   never waits on a reader**: `try_write` never blocks, and a reader that is
//!   descheduled while inside a slot (for any length of time) only takes that
//!   one slot out of rotation. The only thing a publisher can ever wait on is
//!   another *publisher* that is appending a segment at the same instant
//!   (`OnceLock::get_or_init`). The work per publish is a scan of the slots
//!   (8 unless readers have pinned more), plus at most one segment allocation,
//!   and is independent of reader scheduling.
//! * **Load** reads `head`, then [`RwLock::try_read`]s the named slot and
//!   returns a clone of its value **only if the slot's generation tag equals
//!   the head's generation**. Otherwise (a publisher lapped the ring between
//!   the two steps, or another publisher is holding the slot) it re-reads
//!   `head` and retries. A load never blocks on a lock. It retries only when
//!   a publisher made progress, so it is lock-free. With a single publisher
//!   (every production user), the head slot is never write-locked and its
//!   entry is only reclaimed two publishes later, so a load retries only if
//!   the publisher completed at least two publishes inside the load's few
//!   instructions.
//!
//! Generations come from a per-cell counter and are written together with
//! the value under the slot's lock. So a tag match means "this exact value is
//! the one `head` names", and values never cross cells.
//!
//! Memory: a slot's previous value is dropped when the slot is reused (outside
//! the lock), and the entry two publishes behind the head is cleared eagerly
//! when possible. The cell therefore holds about the current and the previous
//! value, plus one per slot that a descheduled reader is pinning. Segments are
//! never freed before the cell, and their number is bounded by the peak number
//! of simultaneously pinned slots, never by the publish count.
//!
//! ## Guarantees
//!
//! * **Never foreign.** A load returns exactly a value stored into *this*
//!   cell.
//! * **Newest-wins, never lost.** `head` only moves forward (`fetch_max`), and
//!   a publisher can claim only a slot whose generation is strictly older than
//!   the current head, so the head's entry is never overwritten or cleared. The
//!   newest value is visible as soon as its publish returns, with no follow-up
//!   publish or retry needed.
//! * **Never regresses.** A single thread's successive loads read `head`
//!   non-decreasingly (coherence of one atomic), and each returns the entry of
//!   the generation it read.
//!
//! Generations take the top 48 bits of `head` (the slot index takes the low
//! 16), which lasts 2^48 publishes per cell: 8 900 years at 1 000 publishes/s.
//!
//! A panic cannot happen while a slot lock is held (`Clone` of an `Arc` aborts
//! rather than panicking, and `mem::replace` cannot panic), so poisoning is
//! unreachable. It is still tolerated (`into_inner`) so a poisoned lock can
//! never turn into a panic on the data plane.
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock, TryLockError};

/// Slots per segment.
const SEGMENT: u16 = 8;
/// [`SEGMENT`] as an array length.
const SEGMENT_LEN: usize = 8;
/// Bits of `head` that hold the slot index.
const IDX_BITS: u32 = 16;
/// Mask for the slot index in `head`.
const IDX_MASK: u64 = (1 << IDX_BITS) - 1;
/// Number of segments the 16-bit slot index can address.
const MAX_SEGMENTS: u16 = u16::MAX / SEGMENT;

fn encode(generation: u64, idx: u16) -> u64 {
    (generation << IDX_BITS) | u64::from(idx)
}

fn gen_of(head: u64) -> u64 {
    head >> IDX_BITS
}

fn idx_of(head: u64) -> u16 {
    u16::try_from(head & IDX_MASK).unwrap_or(0)
}

#[derive(Debug)]
struct Entry<P> {
    /// Generation of the value (`0` = never written).
    generation: u64,
    value: Option<P>,
}

impl<P> Entry<P> {
    const fn empty() -> Self {
        Self {
            generation: 0,
            value: None,
        }
    }
}

struct Segment<P> {
    slots: [RwLock<Entry<P>>; SEGMENT_LEN],
    next: OnceLock<Box<Segment<P>>>,
}

impl<P> Segment<P> {
    fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| RwLock::new(Entry::empty())),
            next: OnceLock::new(),
        }
    }
}

fn try_read<T>(lock: &RwLock<T>) -> Option<std::sync::RwLockReadGuard<'_, T>> {
    match lock.try_read() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn try_write<T>(lock: &RwLock<T>) -> Option<std::sync::RwLockWriteGuard<'_, T>> {
    match lock.try_write() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// A thread-safe cell holding a cheaply-clonable pointer `P` (`Arc<T>` or
/// `Option<Arc<T>>`). Reads clone the pointer; publishes never wait on a
/// reader. See the [module docs](self).
///
/// Crate-private: `P::clone` runs inside a slot's read lock, so `P` must be a
/// pointer whose clone is one atomic increment (never a deep copy).
pub(crate) struct SwapCell<P> {
    /// `generation << 16 | slot index` of the current value.
    head: AtomicU64,
    /// The previous `head`, kept readable for one more publish and then
    /// cleared (memory only; never needed for correctness).
    retained: AtomicU64,
    /// Next generation to hand out.
    next_gen: AtomicU64,
    first: Segment<P>,
}

impl<P> fmt::Debug for SwapCell<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head = self.head.load(Ordering::Relaxed);
        f.debug_struct("SwapCell")
            .field("generation", &gen_of(head))
            .field("slot", &idx_of(head))
            .finish_non_exhaustive()
    }
}

impl<P> SwapCell<P> {
    /// The slot with index `idx`, if its segment exists.
    fn slot(&self, idx: u16) -> Option<&RwLock<Entry<P>>> {
        let mut segment = &self.first;
        for _ in 0..idx / SEGMENT {
            segment = segment.next.get()?;
        }
        segment.slots.get(usize::from(idx % SEGMENT))
    }

    /// The number of slots allocated so far.
    fn slot_count(&self) -> u16 {
        let mut count = SEGMENT;
        let mut segment = &self.first;
        while let Some(next) = segment.next.get() {
            count = count.saturating_add(SEGMENT);
            segment = next;
        }
        count
    }

    /// Clear `idx`'s entry if it still holds `generation` and that generation
    /// is no longer the head. Never waits: a busy slot is left for reuse.
    fn clear_if_stale(&self, idx: u16, generation: u64) {
        // Never even try-lock the slot readers are being sent to: with a single
        // publisher that slot is then never write-locked at all, so a load's
        // `try_read` on it cannot fail.
        if idx == idx_of(self.head.load(Ordering::Acquire)) {
            return;
        }
        let Some(slot) = self.slot(idx) else { return };
        let Some(mut guard) = try_write(slot) else {
            return;
        };
        if guard.generation == generation && generation < gen_of(self.head.load(Ordering::Acquire))
        {
            let old = core::mem::replace(&mut *guard, Entry::empty());
            drop(guard);
            drop(old);
        }
    }

    /// After `prev` stopped being the head: keep it readable, and clear the
    /// one before it.
    fn retire(&self, prev: u64) {
        let older = self.retained.swap(prev, Ordering::AcqRel);
        if older != prev {
            self.clear_if_stale(idx_of(older), gen_of(older));
        }
    }

    /// Write `(generation, value)` into a slot that no reader is inside and
    /// that does not hold the current head, without ever waiting on a reader.
    /// Returns the slot index, or `None` (dropping `value`) only if all
    /// 65 535 addressable slots are pinned at once.
    fn claim(&self, generation: u64, value: P) -> Option<u16> {
        let head_idx = idx_of(self.head.load(Ordering::Acquire));
        let mut value = Some(value);
        let mut count = self.slot_count();
        // First pass: every allocated slot, round-robin from just after the
        // head slot, so a single publisher cycles the ring and reuses the
        // previous head last.
        let first = (1..count).filter_map(|offset| {
            u16::try_from((u32::from(head_idx) + u32::from(offset)) % u32::from(count)).ok()
        });
        if let Some(idx) = self.try_claim_any(first, generation, &mut value) {
            return Some(idx);
        }
        // Every slot is pinned by a reader (or held by another publisher):
        // append a segment and try the slots from there on.
        loop {
            if !self.grow(count) {
                return None;
            }
            let grown = self.slot_count();
            if let Some(idx) = self.try_claim_any(count..grown, generation, &mut value) {
                return Some(idx);
            }
            count = grown;
        }
    }

    /// Try to claim each of `indices` in turn (see [`SwapCell::claim`]).
    fn try_claim_any(
        &self,
        indices: impl Iterator<Item = u16>,
        generation: u64,
        value: &mut Option<P>,
    ) -> Option<u16> {
        for idx in indices {
            let Some(slot) = self.slot(idx) else { continue };
            let Some(mut guard) = try_write(slot) else {
                continue;
            };
            // Re-check under the lock: only an entry strictly older than the
            // current head may be replaced, so the head's entry (and any newer,
            // not-yet-installed one) is never overwritten, even with concurrent
            // publishers. The head only grows, so this cannot go stale.
            if guard.generation >= gen_of(self.head.load(Ordering::Acquire)) {
                continue;
            }
            let old = core::mem::replace(
                &mut *guard,
                Entry {
                    generation,
                    value: value.take(),
                },
            );
            drop(guard);
            drop(old);
            return Some(idx);
        }
        None
    }

    /// Append a segment after the last one (if `count` slots is still the
    /// total). Returns `false` once the 16-bit index space is exhausted.
    fn grow(&self, count: u16) -> bool {
        if count / SEGMENT >= MAX_SEGMENTS {
            return false;
        }
        let mut segment = &self.first;
        while let Some(next) = segment.next.get() {
            segment = next;
        }
        // Blocks only on another publisher appending at the same instant.
        segment.next.get_or_init(|| Box::new(Segment::new()));
        true
    }

    /// Read the value `head` names, if its slot still carries that generation
    /// and nobody is writing it right now.
    fn read_tagged(&self, head: u64) -> Option<P>
    where
        P: Clone,
    {
        let guard = try_read(self.slot(idx_of(head))?)?;
        if guard.generation == gen_of(head) {
            guard.value.clone()
        } else {
            None
        }
    }

    /// Install `value` as a new generation. With `expected = Some(h)` it is
    /// installed only if `head` is still `h` (for [`SwapCell::rcu`]).
    fn install(&self, value: P, expected: Option<u64>) -> bool {
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let Some(idx) = self.claim(generation, value) else {
            return false;
        };
        let new = encode(generation, idx);
        let prev = match expected {
            None => self.head.fetch_max(new, Ordering::AcqRel),
            Some(h) if new > h => {
                if let Ok(prev) =
                    self.head
                        .compare_exchange(h, new, Ordering::AcqRel, Ordering::Acquire)
                {
                    prev
                } else {
                    self.clear_if_stale(idx, generation);
                    return false;
                }
            }
            Some(_) => {
                self.clear_if_stale(idx, generation);
                return false;
            }
        };
        if prev < new {
            self.retire(prev);
        } else {
            // A newer value was published first: ours is already superseded.
            self.clear_if_stale(idx, generation);
        }
        true
    }
}

impl<P: Clone> SwapCell<P> {
    /// Create a cell holding `value`.
    pub(crate) fn new(value: P) -> Self {
        let first = Segment::new();
        if let Some(slot) = first.slots.first() {
            *slot.write().unwrap_or_else(PoisonError::into_inner) = Entry {
                generation: 1,
                value: Some(value),
            };
        }
        let head = encode(1, 0);
        Self {
            head: AtomicU64::new(head),
            retained: AtomicU64::new(head),
            next_gen: AtomicU64::new(2),
            first,
        }
    }

    /// The current value, cloned. Never blocks on a lock (lock-free; see the
    /// module docs). The only critical section is the pointer clone.
    pub(crate) fn load(&self) -> P {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if let Some(value) = self.read_tagged(head) {
                return value;
            }
            core::hint::spin_loop();
        }
    }

    /// Atomically replace the current value with `value` and return exactly
    /// the value it replaced. Retries (never waits) if another publisher got
    /// in first.
    pub(crate) fn swap(&self, value: P) -> P {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let Some(current) = self.read_tagged(head) else {
                core::hint::spin_loop();
                continue;
            };
            if self.install(value.clone(), Some(head)) {
                return current;
            }
        }
    }

    /// Make `value` the current value. Never waits on a reader; the replaced
    /// value is dropped outside any lock.
    pub(crate) fn store(&self, value: P) {
        // `install` only fails if every one of the 65 535 addressable slots is
        // pinned at once (more threads than the OS allows).
        let _installed = self.install(value, None);
    }
}

impl<T> SwapCell<Arc<T>> {
    /// Read-copy-update: compute `f(&current)` **outside** any lock, then
    /// install it only if `current` is still the head. Retry if another
    /// publisher got in first. With a single publisher (the
    /// [`TileStore`](crate::TileStore) contract) this always succeeds on the
    /// first pass. Returns the value that was replaced.
    pub(crate) fn rcu<F>(&self, mut f: F) -> Arc<T>
    where
        F: FnMut(&Arc<T>) -> T,
    {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let Some(current) = self.read_tagged(head) else {
                core::hint::spin_loop();
                continue;
            };
            if self.install(Arc::new(f(&current)), Some(head)) {
                return current;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
impl<P> SwapCell<P> {
    /// Test seam: take and hold a reader's critical section on the value
    /// readers currently load, exactly as a reader that the OS deschedules
    /// in the middle of `load` would hold it.
    fn hold_reader_for_test(&self) -> impl Drop + '_ {
        let head = self.head.load(Ordering::Acquire);
        self.slot(idx_of(head))
            .expect("the head slot exists")
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod chaos {
    //! Round-2 #133 chaos test (invariants #1 / #10): a reader that is
    //! descheduled while inside its read critical section must not delay the
    //! publisher by even one tick.
    use super::{gen_of, SwapCell};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    const PUBLISHES: u64 = 1000;

    #[test]
    fn segment_constants_agree() {
        assert_eq!(usize::from(super::SEGMENT), super::SEGMENT_LEN);
    }
    /// Far below the shortest output tick (16.7 ms at 60 fps).
    const PER_PUBLISH_BUDGET: Duration = Duration::from_millis(1);

    #[test]
    fn publisher_never_waits_on_a_descheduled_reader() {
        let cell: Arc<SwapCell<Option<Arc<u64>>>> = Arc::new(SwapCell::new(Some(Arc::new(0))));
        let held = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        // A reader inside its read critical section, then "descheduled" (parked
        // on a barrier) for 500 ms.
        let reader = {
            let cell = Arc::clone(&cell);
            let held = Arc::clone(&held);
            let release = Arc::clone(&release);
            thread::spawn(move || {
                let guard = cell.hold_reader_for_test();
                held.wait();
                release.wait();
                drop(guard);
                cell.load()
            })
        };
        held.wait();

        // The output clock publishes 1000 times while that reader is parked.
        let done = Arc::new(AtomicU64::new(0));
        let publisher = {
            let cell = Arc::clone(&cell);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut worst = Duration::ZERO;
                for v in 1..=PUBLISHES {
                    let t = Instant::now();
                    cell.store(Some(Arc::new(v)));
                    worst = worst.max(t.elapsed());
                    done.store(v, Ordering::Release);
                }
                worst
            })
        };

        thread::sleep(Duration::from_millis(500));
        let completed = done.load(Ordering::Acquire);
        assert_eq!(
            completed, PUBLISHES,
            "publisher stalled behind a descheduled reader: {completed}/{PUBLISHES} publishes in 500 ms"
        );
        let worst = publisher.join().expect("publisher panicked");
        assert!(
            worst < PER_PUBLISH_BUDGET,
            "a publish took {worst:?} (budget {PER_PUBLISH_BUDGET:?})"
        );

        release.wait();
        let seen = reader.join().expect("reader panicked");
        assert_eq!(
            seen.as_deref(),
            Some(&PUBLISHES),
            "reader must see the newest value"
        );
        assert_eq!(cell.load().as_deref(), Some(&PUBLISHES));
    }

    /// Every slot pinned by a parked reader: publishes keep returning at once
    /// (the ring grows), and the newest value is visible without any further
    /// publish.
    #[test]
    fn publish_grows_past_pinned_slots_and_never_loses_the_newest() {
        let cell: Arc<SwapCell<Option<Arc<u64>>>> = Arc::new(SwapCell::new(None));
        let mut guards = Vec::new();
        for v in 1..=40u64 {
            cell.store(Some(Arc::new(v)));
            assert_eq!(cell.load().as_deref(), Some(&v));
            guards.push(cell.hold_reader_for_test());
        }
        assert!(cell.slot_count() >= 40, "pinned slots must not be reused");
        drop(guards);
        for v in 41..=1000u64 {
            cell.store(Some(Arc::new(v)));
        }
        assert_eq!(cell.load().as_deref(), Some(&1000));
        assert_eq!(cell.swap(None).as_deref(), Some(&1000));
        assert!(cell.load().is_none());
    }

    /// Racing publishers and readers: the final value is the one with the
    /// highest generation, no load ever returns a value that was never stored,
    /// and each reader's view never goes backwards per publisher.
    #[test]
    fn racing_publishers_converge_on_the_newest() {
        const PER: u64 = 20_000;
        let cell: Arc<SwapCell<Arc<(u64, u64)>>> = Arc::new(SwapCell::new(Arc::new((9, 0))));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let cell = Arc::clone(&cell);
                thread::spawn(move || {
                    let mut last = [0u64; 4];
                    for _ in 0..200_000 {
                        let v = cell.load();
                        let (who, n) = *v;
                        if who == 9 {
                            continue;
                        }
                        let who = usize::try_from(who).unwrap();
                        assert!(
                            n >= last[who],
                            "publisher {who} regressed: {n} < {}",
                            last[who]
                        );
                        last[who] = n;
                    }
                })
            })
            .collect();
        let writers: Vec<_> = (0..4u64)
            .map(|who| {
                let cell = Arc::clone(&cell);
                thread::spawn(move || {
                    for n in 1..=PER {
                        cell.store(Arc::new((who, n)));
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        for r in readers {
            r.join().unwrap();
        }
        let head = cell.head.load(Ordering::Acquire);
        let (_, n) = *cell.load();
        assert_eq!(
            n, PER,
            "the last-generation publish is some publisher's final value"
        );
        assert_eq!(gen_of(head) + 1, cell.next_gen.load(Ordering::Acquire));
    }
}
