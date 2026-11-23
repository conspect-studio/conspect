//! Fixed-capacity SPSC handoff for recorder frames. No operation on a live queue allocates,
//! waits, or performs a compare-and-swap. Neither endpoint can be cloned.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Slot<T>(UnsafeCell<MaybeUninit<T>>);
// SAFETY: only the producer writes an empty slot, and only the consumer reads a
// published slot. Release/acquire cursor publication transfers ownership.
#[allow(unsafe_code)]
unsafe impl<T: Send> Sync for Slot<T> {}

struct Ring<T> {
    slots: Box<[Slot<T>]>,
    capacity: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

/// Sole writer of a fixed-capacity queue. This handle is deliberately not Clone.
pub struct Producer<T> {
    ring: Arc<Ring<T>>,
}

/// Sole reader of a fixed-capacity queue. This handle is deliberately not Clone.
pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
}

/// Construct a queue with exactly `capacity` slots. Returns `None` for zero
/// capacity or when allocating the fixed slot array fails.
#[must_use]
pub fn channel<T>(capacity: usize) -> Option<(Producer<T>, Consumer<T>)> {
    if capacity == 0 || capacity > isize::MAX.cast_unsigned() {
        return None;
    }
    let mut slots = Vec::new();
    slots.try_reserve_exact(capacity).ok()?;
    slots.resize_with(capacity, || Slot(UnsafeCell::new(MaybeUninit::uninit())));
    let ring = Arc::new(Ring {
        slots: slots.into_boxed_slice(),
        capacity,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    Some((Producer { ring: ring.clone() }, Consumer { ring }))
}

impl<T> Producer<T> {
    /// Enqueue without waiting. A full queue returns ownership of `item`.
    ///
    /// # Errors
    /// Returns `item` if the consumer has not freed a slot.
    pub fn push(&mut self, item: T) -> Result<(), T> {
        let tail = self.ring.tail.load(Ordering::Relaxed);
        let head = self.ring.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.ring.capacity {
            return Err(item);
        }
        let index = tail % self.ring.capacity;
        // SAFETY: this sole producer writes the slot only after the acquire-load
        // head check proves it empty. The release-store of tail below publishes
        // the completed write; the consumer cannot observe it beforehand.
        #[allow(unsafe_code)]
        unsafe {
            (*self.ring.slots[index].0.get()).write(item);
        }
        self.ring
            .tail
            .store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Take the next item, or return `None` if the ring is empty.
    pub fn pop(&mut self) -> Option<T> {
        let head = self.ring.head.load(Ordering::Relaxed);
        let tail = self.ring.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let index = head % self.ring.capacity;
        // SAFETY: acquire-loading tail observes the producer's published slot.
        // Only this consumer reads it; release-storing head frees it for reuse.
        #[allow(unsafe_code)]
        let item = unsafe { (*self.ring.slots[index].0.get()).assume_init_read() };
        self.ring
            .head
            .store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        for offset in 0..tail.wrapping_sub(head) {
            let pos = head.wrapping_add(offset);
            // SAFETY: both endpoints have been dropped before the final Arc
            // releases Ring. Exactly these published slots remain initialized.
            #[allow(unsafe_code)]
            unsafe {
                (*self.slots[pos % self.capacity].0.get()).assume_init_drop();
            }
        }
    }
}

struct PoolStorage {
    buffers: Box<[UnsafeCell<Box<[u8]>>]>,
    leaked: AtomicUsize,
}
// SAFETY: each buffer index is owned by exactly one non-Clone Buffer or held
// in the SPSC free queue. No alias can access an index concurrently.
#[allow(unsafe_code)]
unsafe impl Sync for PoolStorage {}

/// An exclusively owned frame buffer. Return it through `BufferRecycler`;
/// dropping it instead marks that slot unavailable until pool teardown.
pub struct Buffer {
    storage: Arc<PoolStorage>,
    index: usize,
    recycled: bool,
}

impl Buffer {
    /// Mutable storage sized when the pool was constructed.
    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the non-Clone Buffer uniquely owns this index until recycled.
        #[allow(unsafe_code)]
        unsafe {
            &mut *(*self.storage.buffers[self.index].get())
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.recycled {
            self.storage.leaked.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Sole consumer of the preallocated free-list (the encoder side).
pub struct BufferSource {
    free: Consumer<usize>,
    storage: Arc<PoolStorage>,
}

/// Sole producer of the preallocated free-list (the writer side).
pub struct BufferRecycler {
    free: Producer<usize>,
    storage: Arc<PoolStorage>,
}

/// Preallocate `count` fixed-size buffers and a free-list of `count` slots.
/// Returns `None` for zero-sized inputs or a failed fixed allocation.
#[must_use]
pub fn buffer_pool(count: usize, size: usize) -> Option<(BufferSource, BufferRecycler)> {
    if size == 0 {
        return None;
    }
    let (mut free_tx, free_rx) = channel(count)?;
    let mut buffers = Vec::new();
    buffers.try_reserve_exact(count).ok()?;
    for index in 0..count {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).ok()?;
        bytes.resize(size, 0);
        buffers.push(UnsafeCell::new(bytes.into_boxed_slice()));
        let _ = free_tx.push(index);
    }
    let storage = Arc::new(PoolStorage {
        buffers: buffers.into_boxed_slice(),
        leaked: AtomicUsize::new(0),
    });
    Some((
        BufferSource {
            free: free_rx,
            storage: storage.clone(),
        },
        BufferRecycler {
            free: free_tx,
            storage,
        },
    ))
}

impl BufferSource {
    /// Take an available buffer without waiting or allocating.
    pub fn take(&mut self) -> Option<Buffer> {
        self.free.pop().map(|index| Buffer {
            storage: self.storage.clone(),
            index,
            recycled: false,
        })
    }

    /// Number of buffers permanently excluded from circulation until teardown.
    #[must_use]
    pub fn leaked_buffers(&self) -> usize {
        self.storage.leaked.load(Ordering::Relaxed)
    }
}

impl BufferRecycler {
    /// Return a buffer to the fixed free-list. If it is full, mark the buffer
    /// leaked and retain its allocation in the pool until teardown.
    ///
    /// # Errors
    /// Returns the foreign buffer untouched when it belongs to another pool.
    pub fn recycle(&mut self, mut buffer: Buffer) -> Result<(), Buffer> {
        if !Arc::ptr_eq(&self.storage, &buffer.storage) {
            return Err(buffer);
        }
        if self.free.push(buffer.index).is_ok() {
            buffer.recycled = true;
        }
        Ok(())
    }

    /// Number of buffers excluded from circulation until teardown.
    #[must_use]
    pub fn leaked_buffers(&self) -> usize {
        self.storage.leaked.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{buffer_pool, channel};

    #[test]
    fn round_trip_and_full_returns_item() {
        let (mut tx, mut rx) = channel(2).unwrap();
        assert_eq!(tx.push(10), Ok(()));
        assert_eq!(tx.push(20), Ok(()));
        assert_eq!(tx.push(30), Err(30));
        assert_eq!(rx.pop(), Some(10));
        assert_eq!(rx.pop(), Some(20));
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn wraparound_reuses_slots() {
        let (mut tx, mut rx) = channel(3).unwrap();
        for n in 0..1000 {
            assert_eq!(tx.push(n), Ok(()));
            assert_eq!(rx.pop(), Some(n));
        }
    }

    #[test]
    fn pool_recycles_and_counts_bounded_leaks() {
        let (mut source, mut recycler) = buffer_pool(2, 16).unwrap();
        let mut first = source.take().unwrap();
        let second = source.take().unwrap();
        first.bytes()[0] = 42;
        assert!(recycler.recycle(first).is_ok());
        let mut recycled = source.take().unwrap();
        assert_eq!(recycled.bytes()[0], 42);
        drop(second);
        drop(recycled);
        assert_eq!(source.leaked_buffers(), 2);
        assert_eq!(recycler.leaked_buffers(), 2);
        assert!(source.take().is_none());
    }
}
