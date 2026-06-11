//! The **audio-routing singleton** store: the document-level `[audio]` block
//! (`multiview_config::AudioRouting`) the control plane manages over
//! `GET`/`PUT /api/v1/audio-routing`.
//!
//! Unlike the sources/outputs/overlays collections this is **one document, one
//! address** — there is exactly one routing block per configuration — so it is
//! not a [`ResourceRepository`](crate::resource_store::ResourceRepository).
//! The store pairs the optional typed document with a monotonic
//! [`Version`](crate::concurrency::Version) so the routes can render an `ETag`
//! and enforce `If-Match` optimistic concurrency exactly like every other
//! mutable resource (ADR-W006). An **unconfigured** store still carries
//! [`Version::INITIAL`]: the singleton conceptually always exists (the GET is
//! 404-free), so the very first PUT is already conditional.
//!
//! Isolation (invariant #10): plain control-plane state behind a `Mutex`,
//! read/written only by HTTP handlers and the bind-time seeding — never on the
//! engine's data plane.
use std::sync::Mutex;

use multiview_config::AudioRouting;

use crate::concurrency::Version;

/// The object kind used for concurrency errors and audit records.
pub const AUDIO_ROUTING_KIND: &str = "audio-routing";

/// The audited object id of the singleton (one document, one address).
pub const AUDIO_ROUTING_ID: &str = "audio-routing";

/// The stored document + its version, taken under one lock acquisition.
#[derive(Debug, Clone)]
struct Slot {
    /// The typed routing document; `None` until an operator (or the loaded
    /// config) provides one.
    routing: Option<AudioRouting>,
    /// The monotonic document version (the `ETag` source).
    version: Version,
}

/// The in-memory audio-routing singleton store.
#[derive(Debug)]
pub struct AudioRoutingStore {
    /// The single slot. A poisoned lock is recovered (control-only state — a
    /// prior panic in another request must not wedge the control plane).
    inner: Mutex<Slot>,
}

impl Default for AudioRoutingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRoutingStore {
    /// A fresh, **unconfigured** store at [`Version::INITIAL`].
    #[must_use]
    pub fn new() -> Self {
        Self::seeded(None)
    }

    /// A store seeded from a loaded configuration's optional `[audio]` block
    /// (still [`Version::INITIAL`] — seeding is the document's first state, not
    /// a mutation).
    #[must_use]
    pub fn seeded(routing: Option<AudioRouting>) -> Self {
        Self {
            inner: Mutex::new(Slot {
                routing,
                version: Version::INITIAL,
            }),
        }
    }

    /// Take the slot, recovering a poisoned lock (the slot is plain data; the
    /// last consistent write always wins).
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The current document (cloned) and its version, read atomically.
    #[must_use]
    pub fn snapshot(&self) -> (Option<AudioRouting>, Version) {
        let slot = self.slot();
        (slot.routing.clone(), slot.version)
    }

    /// The current document version (for precondition checks).
    #[must_use]
    pub fn version(&self) -> Version {
        self.slot().version
    }

    /// Replace the document, but **only** if the caller still holds the
    /// current version (the compare runs under the same lock as the swap, so
    /// two racing PUTs cannot both win).
    ///
    /// # Errors
    ///
    /// Returns the actual current version when `expected` is stale; the caller
    /// maps it to `412 Precondition Failed`.
    pub fn replace_if(&self, expected: Version, routing: AudioRouting) -> Result<Version, Version> {
        let mut slot = self.slot();
        if slot.version != expected {
            return Err(slot.version);
        }
        slot.routing = Some(routing);
        slot.version = slot.version.next();
        Ok(slot.version)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use multiview_config::AudioRouting;

    use super::AudioRoutingStore;
    use crate::concurrency::Version;

    fn routing(rate: u32) -> AudioRouting {
        serde_json::from_value(serde_json::json!({
            "sample_rate_hz": rate,
            "routes": []
        }))
        .unwrap()
    }

    #[test]
    fn an_unconfigured_store_is_version_initial_with_no_document() {
        let store = AudioRoutingStore::new();
        let (doc, version) = store.snapshot();
        assert!(doc.is_none());
        assert_eq!(version, Version::INITIAL);
    }

    #[test]
    fn replace_if_swaps_and_bumps_only_on_the_current_version() {
        let store = AudioRoutingStore::new();
        let v2 = store.replace_if(Version::INITIAL, routing(48_000)).unwrap();
        assert_eq!(v2, Version::new(2));

        // A stale replace loses, reporting the live version.
        let err = store
            .replace_if(Version::INITIAL, routing(44_100))
            .unwrap_err();
        assert_eq!(err, Version::new(2));

        let (doc, version) = store.snapshot();
        assert_eq!(doc.unwrap().sample_rate_hz, 48_000);
        assert_eq!(version, Version::new(2));
    }

    #[test]
    fn a_seeded_store_carries_the_document_at_version_initial() {
        let store = AudioRoutingStore::seeded(Some(routing(96_000)));
        let (doc, version) = store.snapshot();
        assert_eq!(doc.unwrap().sample_rate_hz, 96_000);
        assert_eq!(version, Version::INITIAL);
    }
}
