//! NDI® output (ADR-0008) — runtime-load scaffolding + the safe sink seam.
//!
//! > NDI® is a registered trademark of **Vizrt NDI AB**. <https://ndi.video>
//!
//! This module is the `multiview-output` side of the off-by-default, **license-
//! isolating** `ndi` feature. It is gated **twice**, exactly as ADR-0008 / the
//! [NDI doc](../../../docs/io/ndi.md) require:
//!
//! 1. **Build feature** `ndi` (off by default): the default open-source build
//!    contains zero NDI code and zero proprietary obligations.
//! 2. **Runtime license gate** ([`NdiLicense`]): even in an `ndi`-enabled build,
//!    **no NDI sender is constructed** until an operator has accepted the NDI SDK
//!    license. Until then any configured NDI output is **refused with a typed
//!    status — never started, never a panic, never a block** (the output-clock
//!    invariant is untouched).
//!
//! ## Layering
//!
//! - [`loader`] wraps the [`multiview_ndi_sys`] FFI leaf crate (which owns the
//!   raw `dlopen`/`NDIlib_v6_load` `unsafe`), surfacing the runtime as either an
//!   available capability or a typed unavailable status — so this crate stays
//!   `forbid(unsafe_code)`.
//! - [`api`] defines the **API-table seam** ([`api::NdiApi`]) and the host-memory
//!   [`api::NdiVideoFrame`] descriptor an NDI sender consumes. A real
//!   SDK-function-table implementation is a live-only concern; a deterministic
//!   [`api::FakeNdiApi`] lets the sink seam be unit-tested without the SDK.
//! - [`output`] is the safe [`output::NdiOutput`] sink seam: it can only be
//!   constructed with an **accepted** [`NdiLicense`] *and* an [`api::NdiApi`].
//!
//! ## Scaffolding vs live
//!
//! Everything here is the **load + gate scaffolding** plus the sink seam, all
//! testable without the SDK (path search, graceful "runtime absent → typed
//! error", the license refusal, and the seam over [`api::FakeNdiApi`]). Actually
//! sending to a real NDI receiver requires the proprietary runtime and a live
//! NDI network and is gated behind a live-only, ignored-by-default test.

pub mod api;
pub mod convert;
pub mod license;
pub mod loader;
pub mod output;
/// The live `NdiApi` over the SDK function table (ADR-0028 §3); only compiled with
/// the `ndi-bindings` feature (build-time `bindgen` over the licensed SDK header).
#[cfg(feature = "ndi-bindings")]
pub mod sdk;

pub use api::{FakeNdiApi, NdiApi, NdiAudioFrame, NdiFourCc, NdiSendError, NdiVideoFrame};
pub use convert::{nv12_to_uyvy, Nv12Canvas};
pub use license::{NdiLicense, NdiLicenseError};
pub use loader::{NdiCapability, NdiLoadStatus};
pub use output::NdiOutput;
#[cfg(feature = "ndi-bindings")]
pub use sdk::SdkNdiApi;

/// The mandatory NDI® trademark attribution notice for the About box / NOTICE.
///
/// ADR-0008 / `docs/io/ndi.md` §7.2 make this **load-bearing**: the management
/// surface must render it whenever NDI is enabled. The propose-only `NOTICE` /
/// `README` surfaces should carry the same string.
pub const NDI_TRADEMARK_NOTICE: &str = "NDI® is a registered trademark of Vizrt NDI AB";

/// The mandatory link surfaced near NDI uses (UI/docs), per ADR-0008 §7.2.
pub const NDI_ATTRIBUTION_URL: &str = "https://ndi.video";

/// The full attribution block (notice + link) for embedding in an About panel.
#[must_use]
pub fn attribution() -> String {
    format!("{NDI_TRADEMARK_NOTICE}\n{NDI_ATTRIBUTION_URL}")
}
