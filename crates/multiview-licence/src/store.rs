//! The local lease **store + install service** (CONSPECT-1, brief §1 of the
//! CONSPECT-1 scope).
//!
//! The store holds the current verified active [`Entitlement`] and, on read,
//! computes the enforcement ladder via [`compute_ladder_state`] at an injected
//! instant. It is the machine-side source of truth for "what is this machine
//! entitled to". It is **thread-safe, in-memory, best-effort, and never reads a
//! system clock itself** — the clock is a [`Clock`] seam the caller (the cli)
//! supplies, so the store stays deterministic and testable (data minimisation +
//! the never-off-air invariant: it holds data and computes, nothing else).
//!
//! # Never off air (invariant #1 / #10)
//!
//! The store holds an `RwLock` over control-plane-only state. It has **no**
//! engine handle, spawns **no** task on the data plane, and is read off the hot
//! loop (the engine only ever samples the two derived booleans the cli lifts out
//! of the computed status). A wedged reader of this store cannot back-pressure
//! the engine. Installation **fails toward leniency**: an install that cannot be
//! verified is rejected and the previously-installed (or empty) state stays in
//! place — the store never installs a worse state on a transient fault.
//!
//! # The three install paths converge here
//!
//! Filesystem-drop ([`crate::watcher`]), `POST /api/v1/licence/lease`, and the
//! web-UI upload all land on [`LeaseStore::install_binding`]: verify the
//! `Ed25519` signature against the pinned key, check the fingerprint score is at/above the
//! threshold, reject a stale (older) grant, then install. One code path, one set
//! of typed rejections.

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::constants::FINGERPRINT_MATCH_THRESHOLD;
use crate::entitlement::{Entitlement, GpuLimit, HardwareClass};
use crate::error::LicenceError;
use crate::ladder::{compute_ladder_state, LadderInput, LadderState};
use crate::lease::Lease;
use crate::status::EnforcementLevel;
use crate::verify::{verify_signed_lease, PinnedKey, SignedLease};

/// A clock seam: a function the caller supplies that returns "now" in UTC.
///
/// The store never reads a system clock directly (determinism + the data-model
/// invariant). The cli installs `Arc::new(Utc::now)`; tests inject a fixed
/// instant. `Send + Sync` so the store is freely shareable across threads.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// A signed lease **binding** — the unit the three install paths carry.
///
/// It bundles the Ed25519-[`SignedLease`] with the [`Entitlement`] resource it
/// grants (tier, hardware class, GPU limit, flags) and the machine-fingerprint
/// **score** (brief §2.3) the issuer computed for this binding. The score is a
/// number (0–100), never raw identifiers (§8). `#[non_exhaustive]` (a versioned
/// wire resource) — built via [`LeaseBinding::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LeaseBinding {
    /// The Ed25519-signed lease (verified against the pinned key on install).
    pub signed: SignedLease,
    /// The entitlement this binding grants (rendered on the licence resource).
    pub entitlement: Entitlement,
    /// The salted hardware-fingerprint **score** (0–100) the issuer computed for
    /// this machine (brief §2.3). At/above [`FINGERPRINT_MATCH_THRESHOLD`] is the
    /// same machine; below is a new machine requiring re-claim.
    pub fingerprint_score: u8,
}

impl LeaseBinding {
    /// Bundle a signed lease with its entitlement + fingerprint score.
    #[must_use]
    pub fn new(signed: SignedLease, entitlement: Entitlement, fingerprint_score: u8) -> Self {
        Self {
            signed,
            entitlement,
            fingerprint_score,
        }
    }

    /// Encode the binding as CBOR (the dropped-file + WebUI-upload wire format).
    ///
    /// # Errors
    /// [`LicenceError::Cbor`] if serialisation fails.
    pub fn to_cbor(&self) -> Result<Vec<u8>, LicenceError> {
        let mut out = Vec::with_capacity(256);
        ciborium::into_writer(self, &mut out).map_err(|e| LicenceError::Cbor(e.to_string()))?;
        Ok(out)
    }

    /// Decode a binding from its canonical CBOR wire form. A typed error on
    /// garbage — never a panic (bad-inputs-are-the-purpose).
    ///
    /// # Errors
    /// [`LicenceError::Cbor`] if the bytes are not well-formed CBOR for this
    /// shape.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LicenceError> {
        ciborium::from_reader(bytes).map_err(|e| LicenceError::Cbor(e.to_string()))
    }
}

/// Why an install was rejected. These map 1:1 to the control-plane problem codes
/// (brief §11): `SignatureInvalid` → 422 `signature_invalid`,
/// `FingerprintMismatch` → 409 `fingerprint_mismatch`, `Stale` → 409
/// `lease_stale`. Every variant leaves the store's active state untouched — a
/// rejection never degrades the machine (fail-toward-leniency).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// The Ed25519 signature did not verify against the pinned key (tampered
    /// payload, wrong signer, or a malformed signature/key).
    #[error("lease signature verification failed")]
    SignatureInvalid,

    /// The binding's fingerprint score is below
    /// [`FINGERPRINT_MATCH_THRESHOLD`] — this is treated as a *different*
    /// machine and must re-claim (brief §2.3).
    #[error("fingerprint score {score} is below the {threshold} match threshold")]
    FingerprintMismatch {
        /// The score the binding presented.
        score: u8,
        /// The threshold it failed to reach.
        threshold: u8,
    },

    /// The presented grant is **older** than the currently-installed lease — a
    /// replay / rollback. The active lease never goes backwards.
    #[error("lease granted_at {incoming} is not newer than the active {active}")]
    Stale {
        /// The active lease's grant instant.
        active: DateTime<Utc>,
        /// The (rejected) incoming grant instant.
        incoming: DateTime<Utc>,
    },
}

/// The licensed-vs-detected hardware class pair, rendered on the licence
/// resource (brief §11 endpoint 3: `hardware_class{licensed,detected}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareClassView {
    /// The class the entitlement is licensed for.
    pub licensed: HardwareClass,
    /// The class detected on the machine (a mismatch is a ladder reason).
    pub detected: HardwareClass,
}

/// The computed licence status the control plane renders at
/// `GET /api/v1/licence` (brief §11 endpoint 3). It is the [`Entitlement`]
/// resource plus the **computed** ladder `state` + `enforcement` level +
/// machine-readable `reasons`. Enforcement is **data** — this is a snapshot a
/// surface renders, never a control-flow decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LicenceStatusView {
    /// The opaque commercial tier (rendered, never computed).
    pub tier: String,
    /// The computed ladder state (the seven conditions, brief §6/§12).
    pub state: LadderState,
    /// The canonical enforcement level derived from the state (brief §6.2).
    pub enforcement: EnforcementLevel,
    /// The licensed-vs-detected hardware class.
    pub hardware_class: HardwareClassView,
    /// The GPU allowance carried by the entitlement.
    pub gpu_limit: GpuLimit,
    /// The number of GPUs currently in use (sampled by the caller; `0` when the
    /// store has no usage sample — usage informs the `over_gpu` reason only).
    pub gpus_in_use: u32,
    /// Whether the engine should deny hot-reconfiguration (derived, S2).
    pub config_locked: bool,
    /// Whether the engine should stamp a corner watermark (derived, S3).
    pub watermark: bool,
    /// Whether the startup gate should refuse a new engine instance (S1).
    pub blocks_new_instances: bool,
    /// The dated lease this status reflects.
    pub lease: Lease,
    /// Machine-readable reason codes the UI renders (brief §6.1).
    pub reasons: Vec<String>,
}

impl LicenceStatusView {
    /// The never-off-air guarantee restated at the view level: no state takes a
    /// running program off air (brief §6.3, invariant #1). Always `true`.
    #[must_use]
    pub const fn program_stays_on_air(&self) -> bool {
        true
    }
}

/// The thread-safe, in-memory active-lease store.
///
/// Holds at most one verified [`Entitlement`] and the injected [`Clock`]. Cheap
/// to `Arc`-share into the control-plane state and the directory watcher.
pub struct LeaseStore {
    /// The currently-installed verified entitlement, if any. `RwLock` because the
    /// store is read-mostly and off the engine hot loop; it can never
    /// back-pressure the engine (invariant #10).
    active: RwLock<Option<Entitlement>>,
    /// The most recent GPU-usage sample (informs the `over_gpu` reason only).
    /// `RwLock` for the same reason as `active`.
    gpus_in_use: RwLock<u32>,
    /// When the currently-active lease was **installed** (the instant passed to
    /// [`LeaseStore::install_binding`]). `None` until the first install. This is
    /// the honest "last contact" the heartbeat-status surface reports — the local
    /// instant this machine last accepted a lease, distinct from the lease's own
    /// `granted_at` (the server-side grant instant). `RwLock` for the same reason
    /// as `active`; read off the hot loop only.
    installed_at: RwLock<Option<DateTime<Utc>>>,
    /// The injected clock; the store never reads a system clock directly.
    clock: Clock,
}

impl Default for LeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseStore {
    /// A new, empty store with the system clock (the production default).
    ///
    /// The default [`Clock`] samples [`system_now`] (the host wall clock via
    /// `std::time::SystemTime`, since `chrono`'s `clock` feature is off in this
    /// crate). Tests inject a fixed instant with [`LeaseStore::with_clock`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(system_now))
    }

    /// A new, empty store reading "now" from `clock` (tests inject a fixed
    /// instant for determinism).
    #[must_use]
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            active: RwLock::new(None),
            gpus_in_use: RwLock::new(0),
            installed_at: RwLock::new(None),
            clock,
        }
    }

    /// When the currently-active lease was installed (the `now` passed to the
    /// successful [`LeaseStore::install_binding`]), or `None` if no lease is
    /// installed. The heartbeat-status surface reports this as the honest local
    /// "last contact" instant. A poisoned lock fails toward "unknown" (`None`),
    /// never a panic.
    #[must_use]
    pub fn installed_at(&self) -> Option<DateTime<Utc>> {
        self.installed_at.read().ok().and_then(|g| *g)
    }

    /// "Now" as the store sees it, via the injected [`Clock`] seam.
    ///
    /// The control plane reads this to stamp an install (`POST /licence/lease`)
    /// at the same instant the store's reads use, so a test that injects a fixed
    /// clock drives both the install and the status read deterministically. The
    /// store never reads a system clock directly — this is the single seam.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Record the current GPU-usage sample (the caller samples placement). Used
    /// only to compute the `over_gpu` ladder reason; never gates output.
    pub fn set_gpus_in_use(&self, count: u32) {
        if let Ok(mut guard) = self.gpus_in_use.write() {
            *guard = count;
        }
    }

    /// The currently-installed entitlement, if any (a cheap clone of the held
    /// resource). A poisoned lock fails toward "no lease" — never a panic.
    #[must_use]
    pub fn current(&self) -> Option<Entitlement> {
        self.active.read().ok().and_then(|g| g.clone())
    }

    /// Install a verified binding into the store (the one path the three install
    /// surfaces converge on). Verifies the signature against `pinned`, checks the
    /// fingerprint threshold, rejects a stale grant, then installs.
    ///
    /// Returns the installed [`Lease`] (a clone) on success.
    ///
    /// # Errors
    /// [`InstallError::SignatureInvalid`], [`InstallError::FingerprintMismatch`],
    /// or [`InstallError::Stale`] — and on any rejection the store's active state
    /// is left untouched (fail toward leniency).
    pub fn install_binding(
        &self,
        binding: &LeaseBinding,
        pinned: &PinnedKey,
        now: DateTime<Utc>,
    ) -> Result<Lease, InstallError> {
        // 1. Verify the Ed25519 signature against the pinned key. A malformed
        //    signature/key is the same rejection class as a bad signature.
        let verified = verify_signed_lease(&binding.signed, pinned)
            .map_err(|_| InstallError::SignatureInvalid)?;

        // The verified lease and the entitlement's lease must be the same grant —
        // a binding that signs lease A but carries entitlement-for-lease-B is a
        // tamper of the unsigned entitlement envelope; reject it as a signature
        // failure (the covered grant does not match what would be installed).
        if verified != &binding.entitlement.lease {
            return Err(InstallError::SignatureInvalid);
        }

        // 2. Fingerprint continuity: at/above the threshold is the same machine.
        if binding.fingerprint_score < FINGERPRINT_MATCH_THRESHOLD {
            return Err(InstallError::FingerprintMismatch {
                score: binding.fingerprint_score,
                threshold: FINGERPRINT_MATCH_THRESHOLD,
            });
        }

        // 3. Staleness: never accept a grant older than the active one (replay /
        //    rollback protection). Equal grant instants are accepted (idempotent
        //    re-install of the same lease via a different path).
        if let Some(active) = self.current() {
            if verified.granted_at < active.lease.granted_at {
                return Err(InstallError::Stale {
                    active: active.lease.granted_at,
                    incoming: verified.granted_at,
                });
            }
        }
        let _ = now; // `now` reserved for a future not-yet-valid check; staleness is grant-ordered.

        // 4. Install. A poisoned lock is recovered by replacing the inner value
        //    (the store is a single-value cache; there is no invariant to lose).
        let lease = binding.entitlement.lease.clone();
        match self.active.write() {
            Ok(mut guard) => *guard = Some(binding.entitlement.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(binding.entitlement.clone()),
        }
        // Record the install instant (the honest local "last contact" the
        // heartbeat-status surface reports). Same poisoned-lock recovery as above.
        match self.installed_at.write() {
            Ok(mut guard) => *guard = Some(now),
            Err(poisoned) => *poisoned.into_inner() = Some(now),
        }
        Ok(lease)
    }

    /// The current computed status at the store's injected clock, or `None` when
    /// no lease is installed.
    #[must_use]
    pub fn status(&self) -> Option<LicenceStatusView> {
        self.status_at((self.clock)())
    }

    /// The current computed status at an explicit instant (tests use this to
    /// drive the ladder across day boundaries deterministically).
    #[must_use]
    pub fn status_at(&self, now: DateTime<Utc>) -> Option<LicenceStatusView> {
        let entitlement = self.current()?;
        let gpus_in_use = self.gpus_in_use.read().map_or(0, |g| *g);
        let input = ladder_input(&entitlement, gpus_in_use, now);
        let outcome = compute_ladder_state(&input);
        let reasons = reason_codes(outcome.state);
        Some(LicenceStatusView {
            tier: entitlement.tier.as_str().to_owned(),
            state: outcome.state,
            enforcement: EnforcementLevel::from_ladder_state(outcome.state),
            hardware_class: HardwareClassView {
                licensed: entitlement.licensed_class,
                detected: entitlement.detected_class,
            },
            gpu_limit: entitlement.gpu_limit,
            gpus_in_use,
            config_locked: outcome.config_locked(),
            watermark: outcome.watermark(),
            blocks_new_instances: outcome.blocks_new_instances(),
            lease: entitlement.lease.clone(),
            reasons,
        })
    }

    /// The ladder input the store would compute at `now`, exposed so callers (and
    /// tests) can drive [`compute_ladder_state`] directly against the store's
    /// view. `None` when no lease is installed.
    #[must_use]
    pub fn ladder_input(&self, now: DateTime<Utc>) -> Option<LadderInput> {
        let entitlement = self.current()?;
        let gpus_in_use = self.gpus_in_use.read().map_or(0, |g| *g);
        Some(ladder_input(&entitlement, gpus_in_use, now))
    }
}

/// The host wall clock as a [`DateTime<Utc>`], the default [`Clock`] seam.
///
/// `chrono`'s `clock` feature is off in this crate (the data model takes its
/// instant as a parameter), so "now" is read from `std::time::SystemTime` and
/// converted. A clock before the Unix epoch (or a conversion failure) saturates
/// to the epoch rather than panicking — the ladder then reads as lapsed, the
/// fail-safe direction (never toward unwarranted leniency on a clock anomaly,
/// and never a crash).
#[must_use]
pub fn system_now() -> DateTime<Utc> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    DateTime::from_timestamp(secs, 0).unwrap_or(DateTime::UNIX_EPOCH)
}

/// Build the ladder input from an installed entitlement + a usage sample.
///
/// The GPU limit maps to a count: `Unlimited` becomes `u32::MAX` so the
/// `over_gpu` reason can never fire (mirrors [`GpuLimit::is_over`]); a `Limited`
/// cap passes through.
fn ladder_input(entitlement: &Entitlement, gpus_in_use: u32, now: DateTime<Utc>) -> LadderInput {
    let gpu_limit = match entitlement.gpu_limit {
        GpuLimit::Unlimited => u32::MAX,
        GpuLimit::Limited(count) => count,
    };
    LadderInput {
        lease: entitlement.lease.clone(),
        now,
        licensed_class: entitlement.licensed_class,
        detected_class: entitlement.detected_class,
        gpu_limit,
        gpu_in_use: gpus_in_use,
        evaluation_started_at: None,
    }
}

/// The machine-readable reason codes for a computed state (brief §6.1 — the UI
/// renders all of them). Stable `snake_case` slugs.
fn reason_codes(state: LadderState) -> Vec<String> {
    let code = match state {
        LadderState::Compliant => "lease_valid",
        LadderState::Grace => "lease_in_grace",
        LadderState::LapsedSoft => "lease_lapsed_soft",
        LadderState::LapsedHard => "lease_lapsed_hard",
        LadderState::Evaluation => "evaluation",
        LadderState::ClassMismatch => "hardware_class_mismatch",
        LadderState::OverGpu => "gpu_over_limit",
    };
    vec![code.to_owned()]
}
