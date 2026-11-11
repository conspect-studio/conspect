//! The bounded hand-off slots of a runtime load (design 3.1, 3.3, 5.1) as plain
//! types: no threads, no libav, no clock. The opener thread and the executor
//! (a later task) plug into these seams.
//!
//! Bounds per player: 1 pending request, 1 staged clip, 2 retiring clips, 1
//! shutdown clip, 1 committed-not-adopted audio deck. Slots displace or refuse;
//! none grows. The ingest-side calls (`try_take`, `retire`, `put_shutdown`,
//! [`AudioCommitSlot::put`]) use `try_lock` only and never wait. The opener-side
//! calls hold a lock only for a pointer swap and hand displaced values back so
//! the caller drops them **after** unlocking.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use multiview_framestore::SwapCell;

use super::ClipSpec;

/// Capacity of the retire queue (design 3.3).
pub const RETIRE_CAPACITY: usize = 2;

/// A load generation / audio epoch. `0` is "none"; the first load is `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u64);

impl Generation {
    /// No generation yet.
    pub const NONE: Self = Self(0);
}

/// One pending load request: pure data, tagged with its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadRequest {
    /// The generation this request serves.
    pub generation: Generation,
    /// The clip to open.
    pub spec: ClipSpec,
}

/// A staged value tagged with the generation it serves.
#[derive(Debug)]
pub struct Staged<T> {
    /// The generation this clip was opened for.
    pub generation: Generation,
    /// The staged clip (video and audio travel as one value).
    pub clip: T,
}

/// The capacity-1 audio commit slot (design 5.1): written by the ingest thread
/// at video commit, emptied only by the audio thread when it adopts. A commit
/// requires it to be empty, so a newer generation can never displace a deck the
/// audio thread has not adopted.
#[derive(Debug)]
pub struct AudioCommitSlot<A> {
    slot: Mutex<Option<(Generation, A)>>,
    occupied: AtomicBool,
    closed: AtomicBool,
    retire_requested: AtomicU64,
    retire_complete: AtomicU64,
    adopted: AtomicU64,
    runtime_owner: std::sync::OnceLock<Arc<std::fs::File>>,
}

impl<A> Default for AudioCommitSlot<A> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            occupied: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            retire_requested: AtomicU64::new(0),
            retire_complete: AtomicU64::new(0),
            adopted: AtomicU64::new(0),
            runtime_owner: std::sync::OnceLock::new(),
        }
    }
}

impl<A> AudioCommitSlot<A> {
    /// Publish only after the matching deck replaces the prior generation.
    pub fn acknowledge_adoption(&self, generation: Generation) {
        self.adopted.store(generation.0, Ordering::Release);
    }
    /// Exact-generation audio adoption, never inferred from an empty slot.
    pub fn has_adopted(&self, generation: Generation) -> bool {
        self.adopted.load(Ordering::Acquire) == generation.0
    }
    /// Retain the journal lease for the full audio-owner lifetime.
    pub fn retain_runtime_owner(&self, owner: Arc<std::fs::File>) {
        let _ = self.runtime_owner.set(owner);
    }
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the producer to discard its active deck and replace its unplayed tail.
    pub fn request_retirement(&self, generation: Generation) {
        self.retire_requested.store(generation.0, Ordering::Release);
    }
    /// Producer-side pending retirement barrier.
    pub fn retirement_requested(&self) -> Generation {
        Generation(self.retire_requested.load(Ordering::Acquire))
    }
    /// Publish only after deck destruction and silent tail publication.
    pub fn acknowledge_retirement(&self, generation: Generation) {
        self.retire_complete.store(generation.0, Ordering::Release);
    }
    /// A closed producer must already have destroyed its active owner.
    pub fn retirement_complete(&self, generation: Generation) -> bool {
        self.retire_complete.load(Ordering::Acquire) >= generation.0 || self.is_closed()
    }

    /// Whether a commit may proceed: the slot is empty, or the audio thread is
    /// gone (`close`d), in which case the player rides silence (5.5).
    #[must_use]
    pub fn ready_for_commit(&self) -> bool {
        self.closed.load(Ordering::Acquire) || !self.occupied.load(Ordering::Acquire)
    }

    /// Whether the audio rail is unavailable; a video-only commit must avoid
    /// waiting on an adopter that cannot run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Mark the audio thread as not running (spawn failed or exited): commits
    /// no longer wait for adoption and `put` refuses.
    ///
    /// `closed` is set under the slot lock, so it is mutually ordered with
    /// [`Self::put`]'s check-and-insert: a `put` either completes before the
    /// close (and the deck is handed back here) or sees `closed` and refuses.
    /// Any unadopted deck is returned so the caller drops it (outside the
    /// lock); it is never retained in a dead slot.
    pub fn close(&self) -> Option<(Generation, A)> {
        let mut guard = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.closed.store(true, Ordering::Release);
        let leftover = guard.take();
        self.occupied.store(false, Ordering::Release);
        drop(guard);
        leftover
    }

    /// Ingest side, at commit. `try_lock` only. Refuses (hands the deck back) if
    /// the slot is occupied, closed, or contended; the caller never overwrites.
    ///
    /// # Errors
    ///
    /// Returns the deck unchanged when it was not stored.
    pub fn put(&self, generation: Generation, deck: A) -> Result<(), A> {
        let mut guard = match self.slot.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(deck),
        };
        // Checked under the lock, so it cannot interleave with `close`.
        if self.closed.load(Ordering::Acquire) || guard.is_some() {
            return Err(deck);
        }
        *guard = Some((generation, deck));
        self.occupied.store(true, Ordering::Release);
        Ok(())
    }

    /// Audio side, at adoption: take the committed deck and its epoch tag. The
    /// audio thread may block here for the few ns the ingest `put` holds it.
    pub fn take(&self) -> Option<(Generation, A)> {
        let mut guard = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken = guard.take();
        self.occupied.store(false, Ordering::Release);
        taken
    }
}

/// The request slot, `cancel_gen`, staged slot, retire queue and shutdown slot of
/// one player, generic over the staged/retired clip type `T`.
#[derive(Debug)]
pub struct LoadCoordinator<T> {
    next_gen: AtomicU64,
    /// Newest generation whose durable commit was claimed (0 = none). A claimed
    /// generation is never superseded: a later request queues behind it.
    claimed: AtomicU64,
    /// Held only for a few atomic operations (never across I/O): makes request
    /// publication and the commit claim mutually atomic.
    gate: Mutex<()>,
    quiesced: AtomicU64,
    /// Shared with the interrupt callback: the newest requested generation.
    cancel_gen: Arc<AtomicU64>,
    request: SwapCell<Option<Arc<LoadRequest>>>,
    staged: Mutex<Option<Staged<T>>>,
    retire: Mutex<VecDeque<T>>,
    retire_len: AtomicUsize,
    retire_issued: AtomicU64,
    retire_completed: AtomicU64,
    /// Capacity-one single-use nonblocking native-owner transfer at stop.
    shutdown_tx: Mutex<Option<std::sync::mpsc::SyncSender<[Option<T>; 3]>>>,
    shutdown_rx: Mutex<std::sync::mpsc::Receiver<[Option<T>; 3]>>,
    runtime_owner: std::sync::OnceLock<Arc<std::fs::File>>,
}

impl<T> Default for LoadCoordinator<T> {
    fn default() -> Self {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);
        Self {
            next_gen: AtomicU64::new(1),
            claimed: AtomicU64::new(0),
            gate: Mutex::new(()),
            quiesced: AtomicU64::new(0),
            cancel_gen: Arc::new(AtomicU64::new(0)),
            request: SwapCell::empty(),
            staged: Mutex::new(None),
            retire: Mutex::new(VecDeque::with_capacity(RETIRE_CAPACITY)),
            retire_len: AtomicUsize::new(0),
            retire_issued: AtomicU64::new(0),
            retire_completed: AtomicU64::new(0),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            shutdown_rx: Mutex::new(shutdown_rx),
            runtime_owner: std::sync::OnceLock::new(),
        }
    }
}

impl<T> LoadCoordinator<T> {
    /// Retain the journal lease through opener teardown and queued native owners.
    pub fn retain_runtime_owner(&self, owner: Arc<std::fs::File>) {
        let _ = self.runtime_owner.set(owner);
    }
    /// A fresh coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resume generation allocation above the durable boot identity.
    pub fn resume_after(&self, generation: Generation) {
        self.next_gen
            .fetch_max(generation.0.saturating_add(1), Ordering::AcqRel);
        self.cancel_gen.fetch_max(generation.0, Ordering::AcqRel);
    }

    /// The shared cancellation cell (the interrupt callback compares it with the
    /// generation an open serves).
    #[must_use]
    pub fn cancel_gen(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.cancel_gen)
    }

    /// The newest requested generation.
    #[must_use]
    pub fn latest(&self) -> Generation {
        Generation(self.cancel_gen.load(Ordering::Acquire))
    }

    /// Ingest side: request `spec`. Supersedes any pending request, bumps
    /// `cancel_gen` (which cancels an in-flight open), and returns the new
    /// generation. Wait-free; the caller then wakes the opener (`unpark`).
    ///
    /// Single-producer: only the ingest thread calls this. The order is one
    /// step: assign the generation, publish it to `cancel_gen` (`fetch_max`,
    /// Release), and only then store the request. An opener that takes a request
    /// therefore always observes `latest() >= request.generation`, so it never
    /// sees its own newest request as stale (see [`Self::is_superseded`]).
    pub fn submit(&self, spec: ClipSpec) -> Generation {
        self.submit_with(spec, || {})
    }

    /// [`Self::submit`] with a hook run between publishing the generation and
    /// storing the request (the window an opener can race into). Test seam.
    fn submit_with(&self, spec: ClipSpec, between: impl FnOnce()) -> Generation {
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = Generation(self.next_gen.fetch_add(1, Ordering::Relaxed));
        self.cancel_gen.fetch_max(generation.0, Ordering::AcqRel);
        between();
        self.request
            .store(Some(Arc::new(LoadRequest { generation, spec })));
        generation
    }

    /// Ingest side: atomically claim the right to commit `generation`. Succeeds
    /// iff no newer request was published first; afterwards a newer request
    /// never supersedes this generation (it commits, then the newer one loads).
    /// The gate is held for a few atomics only, never across I/O.
    pub fn claim_commit(&self, generation: Generation) -> bool {
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.claimed.load(Ordering::Acquire) == generation.0 {
            return true;
        }
        if generation.0 < self.cancel_gen.load(Ordering::Acquire) {
            return false;
        }
        self.claimed.store(generation.0, Ordering::Release);
        true
    }

    /// Opener side: whether work for `generation` is stale, decided against
    /// `latest()` at this moment. A generation equal to `latest()` is never
    /// stale.
    #[must_use]
    pub fn is_superseded(&self, generation: Generation) -> bool {
        generation < self.latest() && self.claimed.load(Ordering::Acquire) != generation.0
    }

    /// Opener side: take the pending request, if any.
    pub fn take_request(&self) -> Option<Arc<LoadRequest>> {
        self.request.swap(None)
    }

    /// Opener side: stage `clip` for `generation`. A clip already staged and not
    /// yet taken is displaced and **returned** so the caller drops it after this
    /// call (outside the lock).
    #[must_use = "the displaced clip must be dropped by the caller, on the opener"]
    pub fn stage(&self, generation: Generation, clip: T) -> Option<T> {
        let mut guard = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let displaced = guard.replace(Staged { generation, clip });
        drop(guard);
        displaced.map(|s| s.clip)
    }

    /// Opener side: remove and return a staged clip whose generation is no
    /// longer the latest, for the opener to drop.
    #[must_use = "the stale clip must be dropped by the caller, on the opener"]
    pub fn sweep_stale(&self) -> Option<T> {
        let latest = self.latest();
        let mut guard = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = guard
            .as_ref()
            .is_some_and(|s| {
                s.generation != latest && self.claimed.load(Ordering::Acquire) != s.generation.0
            })
            .then(|| guard.take())
            .flatten();
        drop(guard);
        stale.map(|s| s.clip)
    }

    /// Number of queued clips waiting for opener-thread teardown.
    #[must_use]
    pub fn retired_len(&self) -> usize {
        self.retire_len.load(Ordering::Acquire)
    }

    /// Whether the retire queue has a free slot (the ingest side is its only
    /// producer, so room observed stays).
    #[must_use]
    pub fn retire_has_room(&self) -> bool {
        self.retire_len.load(Ordering::Acquire) < RETIRE_CAPACITY
    }

    /// Ingest side: take the staged clip **iff** it is the latest generation,
    /// the retire queue has room, and the audio commit slot is ready. Otherwise
    /// returns `None` and leaves the clip staged (the tile keeps playing the
    /// current clip). `try_lock` only: never waits, never drops a clip.
    pub fn try_take(&self, audio: &AudioCommitSlot<impl Sized>) -> Option<Staged<T>> {
        if !self.retire_has_room() || !audio.ready_for_commit() {
            return None;
        }
        let mut guard = match self.staged.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let generation = guard.as_ref()?.generation;
        if generation != self.latest() && self.claimed.load(Ordering::Acquire) != generation.0 {
            return None;
        }
        guard.take()
    }

    /// Ingest side, at commit: push the old clip into the retire queue.
    /// `try_lock` only. After a successful [`Self::try_take`] there is room, so
    /// this fails only if the opener holds the lock for its pop (nanoseconds);
    /// the caller then keeps the old clip in a local and retries next frame.
    ///
    /// # Errors
    ///
    /// Returns the clip unchanged when it was not queued.
    pub fn retire(&self, old: T) -> Result<(), T> {
        let mut guard = match self.retire.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(old),
        };
        if guard.len() >= RETIRE_CAPACITY {
            return Err(old);
        }
        self.retire_issued.fetch_add(1, Ordering::Release);
        guard.push_back(old);
        self.retire_len.store(guard.len(), Ordering::Release);
        Ok(())
    }

    /// Opener confirms cancellation after it destroyed stale staging and open work.
    pub fn acknowledge_quiescence(&self, observed: Generation) {
        self.quiesced.store(observed.0, Ordering::Release);
    }
    /// Whether the opener has observed the unload cancellation.
    pub fn is_quiescent(&self, generation: Generation) -> bool {
        self.quiesced.load(Ordering::Acquire) >= generation.0
    }

    /// Capture the FIFO native destruction barrier after all owners were queued.
    pub fn retirement_barrier(&self) -> u64 {
        self.retire_issued.load(Ordering::Acquire)
    }
    /// Opener-side completion, strictly after dropping the popped owner.
    pub fn acknowledge_retired(&self) {
        self.retire_completed.fetch_add(1, Ordering::Release);
    }
    /// Whether all owners through the captured barrier were destroyed.
    pub fn retirement_complete(&self, barrier: u64) -> bool {
        self.retire_completed.load(Ordering::Acquire) >= barrier
    }
    /// Cancel pending/opening loads and reserve an unloaded binding generation.
    pub fn cancel_pending(&self) -> Generation {
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = Generation(self.next_gen.fetch_add(1, Ordering::Relaxed));
        self.cancel_gen.store(generation.0, Ordering::Release);
        self.claimed.store(0, Ordering::Release);
        self.request.store(None);
        generation
    }

    /// Opener side: pop one retired clip for the opener to drop after unlocking.
    pub fn pop_retired(&self) -> Option<T> {
        let mut guard = self
            .retire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let item = guard.pop_front();
        self.retire_len.store(guard.len(), Ordering::Release);
        item
    }

    /// Ingest side, once, on stop: hand all three possible local native owners
    /// to the opener in a preallocated one-item bounded channel. `try_send` does
    /// not wait on the receiver even if the opener is blocked in a kernel call.
    ///
    /// # Errors
    /// Returns the entire bundle unchanged on an unexpected duplicate send or
    /// a disconnected opener. Caller must keep ownership off-ingest.
    pub fn put_shutdown_bundle(&self, clips: [Option<T>; 3]) -> Result<(), [Option<T>; 3]> {
        let mut sender = match self.shutdown_tx.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(clips),
        };
        let Some(sender) = sender.take() else {
            return Err(clips);
        };
        sender.try_send(clips).map_err(|error| match error {
            std::sync::mpsc::TrySendError::Full(bundle)
            | std::sync::mpsc::TrySendError::Disconnected(bundle) => bundle,
        })
    }

    /// Compatibility single-clip handoff.
    ///
    /// # Errors
    /// Returns the clip unchanged if the bounded slot is already full.
    pub fn put_shutdown(&self, clip: T) -> Result<(), T> {
        let mut sender = match self.shutdown_tx.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(clip),
        };
        let Some(sender) = sender.take() else {
            return Err(clip);
        };
        match sender.try_send([Some(clip), None, None]) {
            Ok(()) => Ok(()),
            Err(error) => {
                let bundle = match error {
                    std::sync::mpsc::TrySendError::Full(bundle)
                    | std::sync::mpsc::TrySendError::Disconnected(bundle) => bundle,
                };
                let [Some(clip), _, _] = bundle else {
                    return Ok(());
                };
                Err(clip)
            }
        }
    }

    /// Opener side: nonblocking take of all final native owners.
    pub fn take_shutdown_bundle(&self) -> Option<[Option<T>; 3]> {
        self.shutdown_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_recv()
            .ok()
    }

    /// Compatibility single-clip reader for earlier unit tests.
    pub fn take_shutdown(&self) -> Option<T> {
        self.take_shutdown_bundle()
            .and_then(|mut bundle| bundle[0].take())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn claimed_generation_commits_and_newer_request_queues_behind_it() {
        let c: LoadCoordinator<u8> = LoadCoordinator::new();
        let a = c.submit(spec("p"));
        assert!(c.claim_commit(a));
        let b = c.submit(spec("p"));
        assert!(b > a);
        assert!(!c.is_superseded(a), "claimed A is never superseded");
        assert!(c.is_superseded(Generation(a.0 - 1)) || a.0 == 1);
    }
    #[test]
    fn newer_request_before_claim_denies_commit() {
        let c: LoadCoordinator<u8> = LoadCoordinator::new();
        let a = c.submit(spec("p"));
        let _b = c.submit(spec("p"));
        assert!(!c.claim_commit(a));
        assert!(c.is_superseded(a));
    }

    use super::*;
    use crate::player::LoadThen;

    fn spec(p: &str) -> ClipSpec {
        ClipSpec::new(p, LoadThen::Cue)
    }

    #[test]
    fn newer_request_supersedes_the_pending_one_and_bumps_cancel_gen() {
        let c = LoadCoordinator::<u32>::new();
        let cancel = c.cancel_gen();
        let g1 = c.submit(spec("/a"));
        assert_eq!(cancel.load(Ordering::Acquire), g1.0);
        let g2 = c.submit(spec("/b"));
        assert!(g2 > g1);
        assert_eq!(
            cancel.load(Ordering::Acquire),
            g2.0,
            "in-flight open cancelled"
        );
        let r = c.take_request().unwrap();
        assert_eq!((r.generation, r.spec.path.to_str()), (g2, Some("/b")));
        assert!(c.take_request().is_none(), "only the newest was held");
    }

    #[test]
    fn opener_racing_between_publish_and_store_never_aborts_the_newest() {
        let c = LoadCoordinator::<u32>::new();
        let g1 = c.submit(spec("/a"));
        assert_eq!(c.take_request().unwrap().generation, g1);
        // The opener is mid-open for g1 when the newest request is submitted;
        // it runs inside the publish..store window.
        let g2 = c.submit_with(spec("/b"), || {
            assert_eq!(c.latest(), Generation(2), "published before the store");
            assert!(c.take_request().is_none(), "request not stored yet");
            assert!(c.is_superseded(g1), "the older open is cancelled");
        });
        let r = c.take_request().expect("newest request is never lost");
        assert_eq!(r.generation, g2);
        assert!(!c.is_superseded(r.generation), "newest is never stale");
        let audio = AudioCommitSlot::<u8>::new();
        assert!(c.stage(r.generation, 7).is_none());
        assert_eq!(c.try_take(&audio).unwrap().clip, 7, "newest stages");
    }

    #[test]
    fn close_and_put_are_ordered_and_close_hands_back_the_deck() {
        let audio = AudioCommitSlot::<&str>::new();
        assert!(audio.put(Generation(1), "d1").is_ok());
        assert_eq!(audio.close(), Some((Generation(1), "d1")), "handed back");
        assert_eq!(audio.put(Generation(2), "d2"), Err("d2"), "dead slot");
        assert!(audio.take().is_none(), "nothing retained");
        assert!(audio.ready_for_commit());
        // close while put holds the lock: put's closed check is under the lock.
        let a2 = AudioCommitSlot::<&str>::new();
        {
            let _held = a2.slot.lock().unwrap();
            assert_eq!(a2.put(Generation(1), "x"), Err("x"), "contended refuses");
        }
        assert!(a2.close().is_none());
        assert_eq!(a2.put(Generation(1), "y"), Err("y"));
    }

    #[test]
    fn stale_generation_clip_is_never_taken_and_is_left_for_the_opener() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<u8>::new();
        let g1 = c.submit(spec("/a"));
        assert!(c.stage(g1, 1).is_none());
        let _g2 = c.submit(spec("/b")); // supersedes g1 after it staged
        assert!(c.try_take(&audio).is_none(), "stale clip is left in place");
        assert_eq!(c.sweep_stale(), Some(1), "the opener sweeps and drops it");
        assert!(c.sweep_stale().is_none());
    }

    #[test]
    fn staging_displaces_an_untaken_clip_and_returns_it_for_drop() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<u8>::new();
        let g1 = c.submit(spec("/a"));
        assert_eq!(c.stage(g1, 10), None);
        let g2 = c.submit(spec("/b"));
        assert_eq!(c.stage(g2, 20), Some(10), "older clip returned for drop");
        let taken = c.try_take(&audio).unwrap();
        assert_eq!((taken.generation, taken.clip), (g2, 20));
        assert!(c.try_take(&audio).is_none(), "capacity 1: nothing left");
    }

    #[test]
    fn try_take_returns_none_while_the_opener_holds_the_lock() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<u8>::new();
        let g = c.submit(spec("/a"));
        assert!(c.stage(g, 1).is_none());
        {
            let _held = c.staged.lock().unwrap();
            assert!(c.try_take(&audio).is_none(), "contended: no wait, no take");
        }
        assert_eq!(c.try_take(&audio).unwrap().clip, 1, "retried next frame");
    }

    #[test]
    fn try_take_returns_none_while_the_retire_queue_is_full() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<u8>::new();
        let g = c.submit(spec("/a"));
        assert!(c.stage(g, 1).is_none());
        assert!(c.retire(100).is_ok());
        assert!(c.retire(101).is_ok());
        assert_eq!(c.retire(102), Err(102), "capacity 2");
        assert!(c.try_take(&audio).is_none(), "no commit; clip stays staged");
        assert_eq!(c.pop_retired(), Some(100));
        assert_eq!(
            c.try_take(&audio).unwrap().clip,
            1,
            "commits once a slot frees"
        );
    }

    #[test]
    fn try_take_returns_none_while_the_audio_commit_slot_is_occupied() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<&str>::new();
        let g1 = c.submit(spec("/a"));
        assert!(c.stage(g1, 1).is_none());
        let taken = c.try_take(&audio).unwrap();
        assert!(audio.put(taken.generation, "deck1").is_ok());
        // A newer generation stages; it cannot commit, and cannot displace the
        // unadopted deck.
        let g2 = c.submit(spec("/b"));
        assert!(c.stage(g2, 2).is_none());
        assert!(c.try_take(&audio).is_none());
        assert_eq!(audio.put(g2, "deck2"), Err("deck2"));
        assert_eq!(audio.take(), Some((g1, "deck1")), "adoption empties it");
        assert_eq!(c.try_take(&audio).unwrap().clip, 2);
    }

    #[test]
    fn a_closed_audio_slot_does_not_block_commits() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<&str>::new();
        assert!(audio.put(Generation(1), "d").is_ok());
        assert!(audio.close().is_some());
        let g = c.submit(spec("/a"));
        assert!(c.stage(g, 1).is_none());
        assert!(
            c.try_take(&audio).is_some(),
            "audio thread gone: ride silence"
        );
        assert_eq!(audio.put(g, "x"), Err("x"));
    }

    #[test]
    fn shutdown_slot_always_accepts_the_final_clip() {
        let c = LoadCoordinator::<u32>::new();
        // Even with a full retire queue and an occupied staged slot.
        assert!(c.retire(1).is_ok());
        assert!(c.retire(2).is_ok());
        let g = c.submit(spec("/a"));
        assert!(c.stage(g, 3).is_none());
        assert!(c.put_shutdown(9).is_ok());
        assert_eq!(c.put_shutdown(10), Err(10), "single use");
        assert_eq!(c.take_shutdown(), Some(9));
    }

    #[test]
    fn storm_of_10k_requests_holds_at_most_one_request_and_one_staged() {
        let c = LoadCoordinator::<u32>::new();
        let audio = AudioCommitSlot::<u8>::new();
        let mut displaced = 0u32;
        let mut last = Generation::NONE;
        for i in 0..10_000u32 {
            last = c.submit(spec("/x"));
            if c.stage(last, i).is_some() {
                displaced += 1;
            }
        }
        assert_eq!(displaced, 9_999, "each stage displaced exactly one");
        assert!(c.take_request().is_some());
        assert!(c.take_request().is_none(), "one request slot");
        let taken = c.try_take(&audio).unwrap();
        assert_eq!((taken.generation, taken.clip), (last, 9_999));
        assert!(c.try_take(&audio).is_none());
    }
}
