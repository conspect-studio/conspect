//! # multiview-licence — the local entitlement / lease / ladder / fingerprint
//! data model (the Conspect **entitlement plane**, ADR-0050).
//!
//! This crate is the machine-side **entitlement** model: the signed
//! entitlement/lease resource ([`entitlement`], [`lease`]), the Ed25519
//! lease-verification path ([`verify`]), the enforcement ladder as **pure data**
//! ([`ladder`]), machine-identity **fingerprint scoring** ([`fingerprint`]), and
//! the published [`status::LicenceStatus`] hand-off shape.
//!
//! ## Never off air — this crate is physically incapable of touching output
//!
//! The single most important property of this crate (ADR-0050 §5/§6.3,
//! invariant #1): it **computes data and verifies signatures, and nothing
//! else.** It holds **no** `multiview-engine` dependency, **no** engine handle,
//! **no** process control, spawns **no** task, and performs **no** I/O. The
//! enforcement ladder is *data* — the hardest rung it can compute merely *asks*
//! the engine (via two booleans the cli derives and the engine reads off the hot
//! loop) to lock reconfiguration or stamp a corner watermark. There is no code
//! path here that can stop, stall, or de-pace a running program. Every computed
//! state answers `program_stays_on_air() == true` by construction
//! ([`ladder::LadderState::program_stays_on_air`]). A broadcaster's program is
//! sacred; this crate cannot violate that even by mistake.
//!
//! ## Scope of THIS crate (CONSPECT-0 + CONSPECT-1 + CONSPECT-3)
//!
//! The pure data + verification model ([`entitlement`], [`lease`], [`ladder`],
//! [`fingerprint`], [`verify`], [`status`]) **plus** the local lease lifecycle
//! (CONSPECT-1): the in-memory active-lease [`store`], the lease-directory
//! [`watcher`] (a dependency-free poll loop that picks up a dropped lease file
//! and verifies it against the pinned key), and the [`challenge`] CBOR export.
//! The pinned verifying key is a **parameter** ([`verify::PinnedKey`]); key
//! pinning/rotation policy (O2) lives in the caller.
//!
//! Under the **off-by-default `heartbeat` feature** (CONSPECT-3, ADR-0096) the
//! crate also carries the device-licensing client [`heartbeat`]: the Conspect
//! key-trust verifier (pinned-root ECDSA-P256 → attested Ed25519 intermediates),
//! the bare-Ed25519 signed-lease verifier, and the
//! [`HeartbeatClient`](heartbeat::HeartbeatClient) loop that drives the same
//! [`store::LeaseStore::install_binding`] convergence on a positively-verified
//! lease. The **live HTTP transport stays at the cli/app boundary** behind the
//! [`LicenceServer`](heartbeat::LicenceServer) seam — under this feature the leaf
//! crate gains only pure-Rust crypto + `tokio` (the loop's sleep), never a socket
//! of its own. The default build has **no** licence-server calls and stays a
//! network-free, LGPL-clean shell.
//!
//! The lifecycle pieces are still **physically incapable of touching output**:
//! the [`store`] holds an `RwLock` over control-plane-only state read off the hot
//! loop, and the [`watcher`] does control-plane filesystem I/O only — neither
//! holds an engine handle, and a malformed dropped file is logged + skipped, not
//! crashed on (bad-inputs-are-the-purpose). The heartbeat client likewise holds
//! no engine handle: it only ever **tightens** on a positively-verified signed
//! lease and keeps last-good on every failure (never off air, invariants #1/#10).
//! The only system-clock reads are off the engine (the watcher's per-install
//! wall sample and the heartbeat loop's `nextDue`/now sample); everything in the
//! pure model takes the instant as a parameter (the [`store::Clock`] seam).
//!
//! ## Data minimisation
//!
//! Machine identity is a **score over salted digests** ([`fingerprint`]) — this
//! crate is handed opaque digests and never gathers raw serials/MACs (brief §8).
//! It also never generates cryptographic keys: [`verify`] is verification-only,
//! so no RNG/entropy source enters the non-test build.
//!
//! The library target is `multiview_licence`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod challenge;
pub mod constants;
pub mod entitlement;
pub mod error;
pub mod fingerprint;
#[cfg(feature = "heartbeat")]
pub mod heartbeat;
pub mod ladder;
pub mod lease;
pub mod status;
pub mod store;
pub mod verify;
pub mod watcher;

#[doc(inline)]
pub use challenge::{ChallengeCounters, ChallengeFile};
#[doc(inline)]
pub use constants::{
    ACTIVATION_WINDOW_DAYS, CLAIM_CODE_LEN, EVALUATION_PERIOD_DAYS, EVALUATION_WATERMARK_DAY,
    FINGERPRINT_MATCH_STRONG, FINGERPRINT_MATCH_THRESHOLD, LAPSED_SOFT_MAX_DAYS, LEASE_FULL_DAYS,
    LEASE_GRACE_DAYS, LEASE_HARD_DAYS,
};
#[doc(inline)]
pub use entitlement::{Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier};
#[doc(inline)]
pub use error::LicenceError;
#[doc(inline)]
pub use ladder::{compute_ladder_state, LadderInput, LadderOutcome, LadderState};
#[doc(inline)]
pub use lease::{Lease, LeaseSource};
#[doc(inline)]
pub use status::{EnforcementLevel, LicenceStatus};
#[doc(inline)]
pub use store::{
    Clock, HardwareClassView, InstallError, LeaseBinding, LeaseStore, LicenceStatusView,
};
#[doc(inline)]
pub use watcher::{LeaseDirectoryWatcher, PollOutcome, DEFAULT_LEASE_DIR};
