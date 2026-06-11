//! Runtime-load capability gate over the [`multiview_ndi_sys`] FFI leaf crate.
//!
//! This is the `forbid(unsafe_code)` bridge: the raw `dlopen`/`NDIlib_v6_load`
//! `unsafe` lives entirely in `multiview-ndi-sys`; here we only translate its
//! typed result into a capability the rest of the crate (and the UI/validator)
//! can reason about. The NDI runtime being absent is the **expected** default
//! case — it is reported, never crashed-on (the output-clock invariant is
//! untouched).
//!
//! The actual *load* is live-only (it needs a resolvable runtime dylib), so the
//! `probe`/`load` entry points are exercised live where a runtime exists; the
//! status mapping + the "absent → typed status" path are unit-tested here without
//! the SDK.

use multiview_ndi_sys::{NdiRuntime, NdiSysError};

/// The resolved-or-not status of the NDI runtime, suitable for the capability
/// report surfaced to the UI/validator.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiLoadStatus {
    /// A runtime dylib was resolved and `NDIlib_v6_load` returned a usable table.
    Available,
    /// No NDI runtime dylib was found on any search path.
    RuntimeNotFound,
    /// A dylib was found but could not be opened / lacked the entry point /
    /// returned a null table. Carries a human-readable detail.
    Unusable {
        /// Human-readable detail of why the found runtime was unusable.
        detail: String,
    },
}

impl NdiLoadStatus {
    /// Whether NDI senders may be offered (the runtime resolved successfully).
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    fn from_err(err: &NdiSysError) -> Self {
        match err {
            NdiSysError::RuntimeNotFound { .. } => Self::RuntimeNotFound,
            other => Self::Unusable {
                detail: other.to_string(),
            },
        }
    }
}

/// A successfully-loaded NDI runtime capability: owns the [`NdiRuntime`] (and thus
/// the mapped dylib + resolved function table) for as long as NDI is in use.
///
/// Constructing one is the only way to reach the live SDK function table; it is a
/// **live-only** path (needs a resolvable runtime). The capability is reported via
/// [`NdiLoadStatus`] so the UI/validator never offers NDI when the runtime is
/// absent.
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

    /// Probe the default search paths *without* committing to a live capability —
    /// returns the [`NdiLoadStatus`] the UI/validator surfaces.
    ///
    /// This still performs the real `dlopen` + symbol resolve (so it is the same
    /// answer `load` would give), but discards the loaded handle. Used by the
    /// capability report.
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
