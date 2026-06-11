//! The Conspect **entitlement-plane wiring** for the cli (CONSPECT-2b/10,
//! ADR-0050 §3/§5/§7, the brief §13.6).
//!
//! The cli owns the single shared entitlement [`LeaseStore`] and threads its
//! computed state into the engine seams as **data**:
//!
//! * **S1 (startup gate):** the sampled [`EnforcementLevel`] the
//!   [`SoftwareEngine::build_gated`](crate::run::SoftwareEngine::build_gated)
//!   consults *before* building a NEW engine — see [`current_level`].
//! * **S3 (tile watermark):** the wait-free [`WatermarkSignal`] the overlay bake
//!   samples each frame to decide whether to stamp the corner watermark.
//! * The same store backs the control plane's `LicenceState`, so the API, the
//!   chrome, and the engine all read the **same** ladder state — there is no
//!   second opinion (ADR-0050 §2).
//!
//! # Never off air (invariant #1 / #10)
//!
//! Everything here is **data the engine samples**, never control flow on the tick
//! path. The [`WatermarkSignal`] is an `arc_swap`'d [`EnforcementLevel`] read with
//! a single wait-free load — no lock, no allocation, no `.await` — so the overlay
//! bake's per-frame watermark decision cannot pace or stall the output clock. The
//! signal is updated **off-thread** (a background poll of the lease store), never
//! from the hot loop. The startup gate runs only at build time; a running engine
//! is never re-gated (ADR-0050 §6.3).

use std::sync::Arc;

use arc_swap::ArcSwap;
use multiview_licence::verify::PinnedKey;
use multiview_licence::watcher::{LeaseDirectoryWatcher, DEFAULT_LEASE_DIR};
use multiview_licence::{EnforcementLevel, LeaseStore};

/// The env var the pinned issuer **verifying key** is read from (hex-encoded
/// Ed25519 public key, 64 hex chars / 32 bytes). When unset (or malformed), no
/// key is pinned: the machine runs **unlicensed-honest** — no lease verifies, so
/// the entitlement plane reports no installed lease and every seam fails toward
/// leniency (no watermark, no config-lock, no start block). Never embedded in the
/// binary (secret-hygiene + the honest source-build caveat, ADR-0050 §7).
pub const PUBKEY_ENV: &str = "MULTIVIEW_LICENCE_PUBKEY";

/// The env var the lease **directory** is read from (overriding
/// [`DEFAULT_LEASE_DIR`]). A dropped, signed lease file there is verified against
/// the pinned key and installed at startup; absent/unreadable is fine (no lease).
pub const LEASE_DIR_ENV: &str = "MULTIVIEW_LICENCE_DIR";

/// The wait-free **tile-watermark signal** the overlay bake samples each frame
/// (Conspect S3, ADR-0050 §5).
///
/// It holds an `arc_swap`'d [`EnforcementLevel`] — the single published ladder
/// level. The bake reads [`WatermarkSignal::watermark`] with one lock-free load
/// (no lock, no allocation), so the per-frame watermark decision is O(1) and
/// cannot stall the output clock (invariant #1). The level is updated off-thread
/// via [`WatermarkSignal::set`] (a background poll of the lease store), never from
/// the hot loop.
#[derive(Clone)]
pub struct WatermarkSignal {
    /// The published ladder level. `arc_swap` gives a wait-free latest-value read
    /// + an off-thread atomic store (ADR-I001 isolation primitive).
    level: Arc<ArcSwap<EnforcementLevel>>,
}

impl WatermarkSignal {
    /// A clean signal (no watermark): the compliant default. Equivalent to a
    /// machine with a valid lease — the canvas is unmarked.
    #[must_use]
    pub fn clean() -> Self {
        Self::at(EnforcementLevel::Active)
    }

    /// A signal pinned at `level` (tests inject a watermark rung directly to drive
    /// a deterministic golden bake).
    #[must_use]
    pub fn at(level: EnforcementLevel) -> Self {
        Self {
            level: Arc::new(ArcSwap::from_pointee(level)),
        }
    }

    /// Publish a new ladder `level` (off-thread). The next bake reads it.
    pub fn set(&self, level: EnforcementLevel) {
        self.level.store(Arc::new(level));
    }

    /// The currently-published ladder level (a wait-free load).
    #[must_use]
    pub fn level(&self) -> EnforcementLevel {
        **self.level.load()
    }

    /// Whether the bake should stamp the corner watermark this frame — a single
    /// wait-free load + the pure `EnforcementLevel::watermark()` test. Called once
    /// per baked frame, off the hot loop (the bake runs on collected output
    /// frames, ADR-0050 §5).
    #[must_use]
    pub fn watermark(&self) -> bool {
        self.level().watermark()
    }
}

impl Default for WatermarkSignal {
    fn default() -> Self {
        Self::clean()
    }
}

/// The assembled cli entitlement plane: the shared lease store, the published
/// watermark signal (S3), and the optional pinned key (so the control plane can
/// also accept a presented lease over `POST /api/v1/licence/lease`).
///
/// Everything here is **data** the engine seams sample; nothing holds an engine
/// handle (ADR-0050 §5/§6.3, invariant #1/#10). The same `store` backs the
/// control plane's `LicenceState`, so the API, the chrome, and the engine read
/// the **same** ladder state.
pub struct EntitlementPlane {
    /// The shared verified-lease store (also the control plane's `LicenceState`).
    pub store: Arc<LeaseStore>,
    /// The wait-free watermark signal the overlay bake samples (S3).
    pub signal: WatermarkSignal,
    /// The pinned issuer key, if one was configured (the install path needs it).
    pub pinned: Option<PinnedKey>,
}

impl EntitlementPlane {
    /// Assemble the entitlement plane from the process environment: read the
    /// pinned key from [`PUBKEY_ENV`] (hex), build the shared store, and — when a
    /// key is pinned — load any dropped lease file from [`LEASE_DIR_ENV`] (or
    /// [`DEFAULT_LEASE_DIR`]) once at startup. The published [`WatermarkSignal`] is
    /// initialised from the resulting state.
    ///
    /// All-data, fail-toward-leniency: a missing/malformed key, a missing lease
    /// dir, or a garbage lease file leaves the machine unlicensed-honest (no
    /// watermark, no lock, no start block) — never a crash, never off air.
    #[must_use]
    pub fn from_env() -> Self {
        let store = Arc::new(LeaseStore::new());
        let pinned = pinned_key_from_env();
        if let Some(pinned) = pinned.as_ref() {
            let dir = std::env::var(LEASE_DIR_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_LEASE_DIR.to_owned());
            // One-shot load of any dropped, signed lease file (the directory
            // watcher's poll step). A missing dir / garbage file is logged + skipped
            // inside `poll_once` (never off air).
            let watcher = LeaseDirectoryWatcher::new(dir, pinned.clone(), Arc::clone(&store));
            let _ = watcher.poll_once(multiview_licence::store::system_now());
        } else {
            tracing::info!(
                "no {PUBKEY_ENV} pinned — running unlicensed-honest (no lease verifies; \
                 enforcement seams fail toward leniency)"
            );
        }
        let signal = WatermarkSignal::clean();
        refresh_signal(&store, &signal);
        Self {
            store,
            signal,
            pinned,
        }
    }

    /// The current sampled enforcement level for the S1 startup gate.
    #[must_use]
    pub fn level(&self) -> Option<EnforcementLevel> {
        current_level(&self.store)
    }
}

/// Read + parse the pinned issuer verifying key from [`PUBKEY_ENV`] (hex). Returns
/// `None` when unset, empty, or not a valid 32-byte Ed25519 key (logged) — the
/// unlicensed-honest default.
#[must_use]
fn pinned_key_from_env() -> Option<PinnedKey> {
    let hex = std::env::var(PUBKEY_ENV).ok().filter(|s| !s.is_empty())?;
    let bytes = decode_hex(hex.trim())?;
    if let Ok(key) = PinnedKey::from_slice(&bytes) {
        Some(key)
    } else {
        tracing::warn!(
            "{PUBKEY_ENV} is not a valid 32-byte Ed25519 public key — ignoring \
             (running unlicensed-honest)"
        );
        None
    }
}

/// Decode a hex string to bytes, or `None` on any non-hex / odd-length input
/// (no `as` casts; total + panic-free).
#[must_use]
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i + 1 < s.len() {
        let hi = nibble(*bytes.get(i)?)?;
        let lo = nibble(*bytes.get(i + 1)?)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// Sample the current entitlement [`EnforcementLevel`] from the lease `store`, or
/// `None` when no lease is installed.
///
/// This is what the startup gate (S1) consults *before* building a new engine
/// (`build_gated`). Reading off the store is wait-free and off the hot loop; the
/// gate fails toward leniency on `None` (ADR-0050 §6.3).
#[must_use]
pub fn current_level(store: &LeaseStore) -> Option<EnforcementLevel> {
    store.status().map(|s| s.enforcement)
}

/// Publish the lease `store`'s current level into `signal` (off-thread refresh).
///
/// Called by the cli's background entitlement poll so the wait-free watermark
/// signal tracks the lease as it ages across day boundaries / is renewed —
/// without ever reading the store on the hot loop. A store with no lease publishes
/// the compliant `Active` level (fail-toward-leniency: no positive lapse evidence
/// → no watermark).
pub fn refresh_signal(store: &LeaseStore, signal: &WatermarkSignal) {
    let level = current_level(store).unwrap_or(EnforcementLevel::Active);
    signal.set(level);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_signal_does_not_watermark() {
        assert!(!WatermarkSignal::clean().watermark());
        assert!(!WatermarkSignal::default().watermark());
    }

    #[test]
    fn a_watermark_rung_signals_a_watermark() {
        assert!(WatermarkSignal::at(EnforcementLevel::Watermark).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::BlockNewInstance).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::UnlicensedBuild).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::ConfigLocked).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::Warning).watermark());
    }

    #[test]
    fn set_publishes_a_new_level_read_wait_free() {
        let signal = WatermarkSignal::clean();
        assert!(!signal.watermark());
        signal.set(EnforcementLevel::Watermark);
        assert!(
            signal.watermark(),
            "an off-thread set is visible to the next read"
        );
        assert_eq!(signal.level(), EnforcementLevel::Watermark);
    }

    #[test]
    fn refresh_from_an_empty_store_is_clean() {
        let store = LeaseStore::new();
        let signal = WatermarkSignal::at(EnforcementLevel::Watermark);
        refresh_signal(&store, &signal);
        assert!(
            !signal.watermark(),
            "an empty store fails toward leniency: no lapse evidence → clean"
        );
        assert_eq!(current_level(&store), None);
    }

    #[test]
    fn decode_hex_round_trips_and_rejects_garbage() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("ABcd"), Some(vec![0xab, 0xcd]));
        assert_eq!(decode_hex(""), Some(vec![]));
        // Odd length and non-hex are rejected (never a panic).
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("0x10"), None);
    }
}
