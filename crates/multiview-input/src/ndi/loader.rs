//! Runtime-load capability gate over the [`multiview_ndi_sys`] FFI leaf crate, for
//! the **ingest** side.
//!
//! This is the `forbid(unsafe_code)` bridge: the raw `dlopen` / `NDIlib_v6_load`
//! `unsafe` lives entirely in `multiview-ndi-sys` (the OUT-3 seam); here we only
//! translate its typed result into a capability the rest of the crate (and the
//! UI/validator) can reason about. The NDI runtime being absent is the
//! **expected** default case — it is reported, never crashed-on, and a missing
//! runtime never extends the engine's prime-wait (the output-clock invariant is
//! untouched).
//!
//! The actual *load* is live-only (it needs a resolvable runtime dylib), so the
//! `probe` entry point is exercised live where a runtime exists; the status
//! mapping + the "absent → typed status" path are unit-tested here without the
//! SDK.

use multiview_ndi_sys::{NdiRuntime, NdiSysError};

/// The resolved-or-not status of the NDI runtime, suitable for the capability
/// report surfaced to the UI/validator and used to decide whether an NDI source
/// can start.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiLoadStatus {
    /// A runtime dylib was resolved and `NDIlib_v6_load` returned a usable table.
    Available,
    /// No NDI runtime dylib was found on any search path. NDI ingest is simply
    /// unavailable; the source's tile degrades, nothing hangs.
    RuntimeNotFound,
    /// A dylib was found but could not be opened / lacked the entry point /
    /// returned a null table. Carries a human-readable detail.
    Unusable {
        /// Human-readable detail of why the found runtime was unusable.
        detail: String,
    },
    /// The operator has not accepted the NDI SDK license (ADR-0008 §7.5): the NDI
    /// source/output is refused with the `ndi_unlicensed` status and never
    /// started. Distinct from a missing runtime — the operator must accept the
    /// license, not install a dylib. The license axis is checked **before** the
    /// runtime is probed, so an unaccepted source never touches the SDK.
    Unlicensed,
}

impl NdiLoadStatus {
    /// Whether NDI sources may be offered (the runtime resolved successfully).
    /// Always `false` for [`Self::Unlicensed`] — an unaccepted license is never
    /// "available", regardless of whether a runtime could load.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The stable status token surfaced to telemetry / the UI / the validator,
    /// matching the names ADR-0008 §7.5 and `docs/io/ndi.md` use verbatim
    /// (`ndi_unlicensed` for an unaccepted license).
    #[must_use]
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::RuntimeNotFound => "ndi_runtime_not_found",
            Self::Unusable { .. } => "ndi_runtime_unusable",
            Self::Unlicensed => "ndi_unlicensed",
        }
    }

    /// Map a [`multiview_ndi_sys`] load error to a reported status.
    #[must_use]
    pub fn from_err(err: &NdiSysError) -> Self {
        match err {
            NdiSysError::RuntimeNotFound { .. } => Self::RuntimeNotFound,
            other => Self::Unusable {
                detail: other.to_string(),
            },
        }
    }
}

/// A successfully-loaded NDI runtime capability: owns the [`NdiRuntime`] (and thus
/// the mapped dylib + resolved function table) for as long as NDI ingest is in
/// use.
///
/// Constructing one is the only way to reach the live SDK function table; it is a
/// **live-only** path (needs a resolvable runtime). The real
/// [`super::receiver::NdiReceiver`] binding consumes this capability's runtime.
#[derive(Debug)]
pub struct NdiCapability {
    runtime: NdiRuntime,
}

impl NdiCapability {
    /// Attempt to resolve + load the NDI runtime from the default search paths.
    ///
    /// On success returns the live capability; on failure returns the typed
    /// [`NdiLoadStatus`] (never a panic, never a block) so the caller can report
    /// the runtime unavailable and continue.
    ///
    /// # Errors
    /// [`NdiLoadStatus`] describing why the runtime could not be loaded.
    pub fn load() -> Result<Self, NdiLoadStatus> {
        match NdiRuntime::load() {
            Ok(runtime) => Ok(Self { runtime }),
            Err(err) => Err(NdiLoadStatus::from_err(&err)),
        }
    }

    /// Probe the default search paths, returning the [`NdiLoadStatus`] the
    /// UI/validator surfaces. Never a panic, never a block — a missing runtime is
    /// the expected default.
    #[must_use]
    pub fn probe() -> NdiLoadStatus {
        match NdiRuntime::load() {
            Ok(_) => NdiLoadStatus::Available,
            Err(err) => NdiLoadStatus::from_err(&err),
        }
    }

    /// Borrow the underlying loaded runtime (for the live SDK-table binding).
    #[must_use]
    pub fn runtime(&self) -> &NdiRuntime {
        &self.runtime
    }
}
