//! The **tap registry**: lazy-start, subscriber refcounting, and auto-stop.
//!
//! A *tap* is the single shared, read-only side-channel onto one pipeline entity
//! (an input slot, the program canvas, or one output rendition). Per the preview
//! brief (§3 efficiency, §2 isolation):
//!
//! * **At most one tap per entity, fanned out to many viewers.** N viewers cost
//!   the same as one: the tap produces one thumbnail stream that every viewer's
//!   [`TapLease`] subscribes to.
//! * **Lazy-start on the first subscriber.** The expensive resource (a downscale
//!   blit, a cue decoder, a per-output decode) is created only when the first
//!   [`TapRegistry::subscribe`] arrives — *cost is ~zero when nobody is
//!   watching.*
//! * **Auto-stop on the last leave.** When the final [`TapLease`] is dropped the
//!   registry runs the tap's stop callback, tearing the resource down. A later
//!   subscriber lazily starts it again.
//!
//! ## Isolation (invariant #10)
//!
//! Each [`TapLease`] holds a [`multiview_engine::isolation::EventSubscription`] — a
//! `tokio::sync::broadcast` receiver onto a bounded **drop-oldest** ring the
//! engine publishes into with a non-blocking `send`. A lease that never reads,
//! reads slowly, or is dropped **cannot** back-pressure the engine: it merely
//! lags and loses the oldest buffered frames. The registry's own bookkeeping
//! uses a short-lived `std::sync::Mutex` that the engine's publish path **never
//! touches**, so it can never stall the engine either.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use multiview_engine::isolation::{EventSubscription, RecvError, SeqEvent, TryRecvError};

pub use crate::token::{TapKey, TapScope};

/// The teardown action run when a tap's last subscriber leaves.
///
/// Boxed so heterogeneous tap kinds (blit cancel, cue-decoder SIGKILL, decode
/// session release) share one registry. `Send` so it can run from whichever
/// task drops the final lease.
type StopFn = Box<dyn FnOnce() + Send>;

/// Bookkeeping for one live tap: how many leases reference it and how to stop it.
struct TapEntry<E> {
    /// Number of live [`TapLease`]s referencing this tap.
    refcount: usize,
    /// The teardown action (taken and run when `refcount` hits zero). `Option`
    /// so it is consumed exactly once.
    stop: Option<StopFn>,
    /// The shared upstream subscription factory is not stored; instead the start
    /// closure already handed us the *first* subscription and we keep a template
    /// receiver to clone (`resubscribe`) for additional viewers. Held as the
    /// most-recent subscription so every new viewer resumes from the live head.
    template: EventSubscription<E>,
}

/// A registry of preview taps keyed by [`TapKey`], with subscriber refcounting,
/// lazy-start on first subscribe, and auto-stop on last leave.
///
/// Cheap to clone (it is an `Arc` around shared state); hand clones to the
/// per-scope endpoint handlers. The payload type `E` is the engine event/frame
/// type the underlying broadcast carries (e.g. a composited-frame snapshot).
pub struct TapRegistry<E> {
    inner: Arc<Mutex<HashMap<TapKey, TapEntry<E>>>>,
}

impl<E> Clone for TapRegistry<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E> Default for TapRegistry<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> std::fmt::Debug for TapRegistry<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.active_taps();
        f.debug_struct("TapRegistry")
            .field("active_taps", &active)
            .finish()
    }
}

impl<E> TapRegistry<E> {
    /// Build an empty registry (no taps running).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Subscribe to the tap for `key`, lazily starting it if it is the first
    /// subscriber.
    ///
    /// On the **first** subscriber for `key`, `start` is invoked exactly once to
    /// create the shared upstream resource; it returns `(subscription, stop)`
    /// where `subscription` is the engine broadcast subscription the tap reads
    /// and `stop` is the teardown action run when the last lease leaves. On
    /// **subsequent** subscribers `start` is **not** called — the existing tap's
    /// live head is `resubscribe`d so every viewer fans out from the one shared
    /// stream.
    ///
    /// Returns a [`TapLease`]: an owned subscription that decrements the
    /// refcount (and auto-stops the tap at zero) when dropped.
    ///
    /// # Errors
    ///
    /// Returns [`TapError::Poisoned`] only if the internal registry mutex was
    /// poisoned by a panic in another thread while it was held — the preview
    /// bookkeeping path, never the engine.
    pub fn subscribe<F, S>(&self, key: TapKey, start: F) -> Result<TapLease<E>, TapError>
    where
        F: FnOnce() -> (EventSubscription<E>, S),
        S: FnOnce() + Send + 'static,
    {
        let mut guard = self.inner.lock().map_err(|_| TapError::Poisoned)?;
        let subscription = if let Some(entry) = guard.get_mut(&key) {
            // Reuse the running tap: bump the refcount and fan out a fresh
            // receiver from the live head. `start` is intentionally not run.
            entry.refcount = entry.refcount.saturating_add(1);
            entry.template.resubscribe()
        } else {
            // First subscriber: lazily start the tap exactly once.
            let (subscription, stop) = start();
            let template = subscription.resubscribe();
            guard.insert(
                key.clone(),
                TapEntry {
                    refcount: 1,
                    stop: Some(Box::new(stop)),
                    template,
                },
            );
            subscription
        };
        drop(guard);
        Ok(TapLease {
            registry: self.clone(),
            key,
            subscription,
        })
    }

    /// The number of live subscribers for `key` (`0` if the tap is not running).
    #[must_use]
    pub fn subscriber_count(&self, key: &TapKey) -> usize {
        self.inner
            .lock()
            .map_or(0, |g| g.get(key).map_or(0, |e| e.refcount))
    }

    /// The number of distinct taps currently running.
    #[must_use]
    pub fn active_taps(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.len())
    }

    /// Whether the tap for `key` is currently running (has ≥1 subscriber).
    #[must_use]
    pub fn is_active(&self, key: &TapKey) -> bool {
        self.subscriber_count(key) > 0
    }

    /// Drop one reference to `key`'s tap, running its stop callback if this was
    /// the last subscriber. Internal: called from [`TapLease`]'s `Drop`.
    fn release(&self, key: &TapKey) {
        // Take the stop callback out *inside* the lock so it is consumed exactly
        // once, but RUN it *after* releasing the lock so a tap whose teardown is
        // slow (or itself touches the registry) can never deadlock the mutex.
        let stop = {
            let Ok(mut guard) = self.inner.lock() else {
                return;
            };
            let Some(entry) = guard.get_mut(key) else {
                return;
            };
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                let removed = guard.remove(key);
                removed.and_then(|mut e| e.stop.take())
            } else {
                None
            }
        };
        if let Some(stop) = stop {
            stop();
        }
    }
}

/// An owned subscription to a preview tap.
///
/// Holds the tap's broadcast subscription and a handle back to the registry.
/// Receiving via [`TapLease::recv`] / [`TapLease::try_recv`] exerts **no
/// back-pressure** on the engine (drop-oldest broadcast). Dropping the lease
/// decrements the tap's refcount and auto-stops the tap when it reaches zero.
pub struct TapLease<E> {
    registry: TapRegistry<E>,
    key: TapKey,
    subscription: EventSubscription<E>,
}

impl<E> TapLease<E> {
    /// The tap key this lease is attached to.
    #[must_use]
    pub fn key(&self) -> &TapKey {
        &self.key
    }

    /// Await the next frame/event from the tap.
    ///
    /// Cancel-safe. Returns [`RecvError::Lagged`] (with the missed count) if this
    /// viewer fell behind the drop-oldest ring, then resumes from the oldest
    /// still-buffered item; [`RecvError::Closed`] once the upstream is gone.
    ///
    /// # Errors
    ///
    /// Propagates the underlying broadcast [`RecvError`].
    pub async fn recv(&mut self) -> Result<SeqEvent<E>, RecvError> {
        self.subscription.recv().await
    }

    /// Try to receive the next buffered frame without awaiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError`] (`Empty`, `Lagged`, or `Closed`).
    pub fn try_recv(&mut self) -> Result<SeqEvent<E>, TryRecvError> {
        self.subscription.try_recv()
    }

    /// The number of frames buffered for this viewer but not yet received
    /// (bounded by the tap's drop-oldest ring depth).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.subscription.len()
    }
}

impl<E> Drop for TapLease<E> {
    fn drop(&mut self) {
        self.registry.release(&self.key);
    }
}

/// Errors from the tap registry.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TapError {
    /// The registry's internal mutex was poisoned by a panic in another thread
    /// while holding it. This only ever involves preview bookkeeping state; it
    /// can never reflect or affect the protected engine path.
    #[error("preview tap registry lock was poisoned")]
    Poisoned,
}
