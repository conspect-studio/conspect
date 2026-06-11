//! NDI® **ingest** (ADR-0008, IN-3) — runtime-load scaffolding + the safe
//! receive→NV12 producer seam.
//!
//! > NDI® is a registered trademark of **Vizrt NDI AB**. <https://ndi.video>
//!
//! This module is the `multiview-input` side of the off-by-default, **license-
//! isolating** `ndi` feature — the receive counterpart of `multiview-output`'s NDI
//! sender (OUT-4). It is gated **twice**, exactly as ADR-0008 / the
//! [NDI doc](../../../docs/io/ndi.md) require:
//!
//! 1. **Build feature** `ndi` (off by default): the default open-source build
//!    contains zero NDI code and zero proprietary obligations. The proprietary
//!    runtime-load dependency ([`multiview_ndi_sys`]) is pulled in only by this
//!    feature, so the default `cargo deny check` (`all-features = false`) never
//!    scans it.
//! 2. **Runtime license gate** ([`license::NdiLicense`]): even in an `ndi`-enabled
//!    build, **no NDI source starts receiving** until an operator has accepted the
//!    NDI SDK license. Until then a configured NDI source is **refused with a typed
//!    status — never started, never a panic, never a block** (the output-clock
//!    invariant is untouched; the tile degrades).
//!
//! ## Layering
//!
//! - [`loader`] wraps the [`multiview_ndi_sys`] FFI leaf crate (which owns the raw
//!   `dlopen` / `NDIlib_v6_load` `unsafe`), surfacing the runtime as either an
//!   available capability or a typed unavailable status — so `multiview-input`
//!   stays `forbid(unsafe_code)`.
//! - [`receiver`] defines the **receive seam** ([`receiver::NdiReceiver`]) the
//!   producer samples, plus a deterministic
//!   [`receiver::FakeNdiReceiver`] for unit-testing the ingest path without the
//!   SDK. A real SDK-function-table receiver (`FrameSync`-wrapped) is a live-only
//!   concern.
//! - [`convert`] is the pure UYVY/BGRA → NV12 host conversion (checked indexing,
//!   panic-free), the correctness load — fully unit-tested without the SDK.
//! - [`NdiProducer`] adapts the receive seam to the IN-2
//!   [`FrameProducer`](crate::source::FrameProducer) shape: sample → convert →
//!   `ProducedFrame`, sampled and never pacing (invariants #1 / #2 / #10).
//!
//! ## Scaffolding vs live (hardware honesty)
//!
//! Everything here is the **load + gate scaffolding** plus the receive→NV12
//! producer seam, all testable without the SDK (path search → typed "runtime
//! absent" via [`multiview_ndi_sys`], the license refusal, the conversion, and the
//! producer over [`receiver::FakeNdiReceiver`]). Actually receiving from a real NDI
//! sender requires the proprietary runtime and a live NDI network and is gated
//! behind a live-only, ignored-by-default test (`ndi_live.rs`).

pub mod convert;
pub mod license;
pub mod loader;
pub mod producer;
pub mod receiver;
/// The live `NdiReceiver` over the SDK function table (ADR-0028 §3); only compiled
/// with the `ndi-bindings` feature (build-time `bindgen` over the licensed header).
#[cfg(feature = "ndi-bindings")]
pub mod sdk;

pub use convert::{bgra_to_nv12, uyvy_to_nv12, HostNv12, NdiConvertError, ReceivedVideoFrame};
pub use license::{LicenseAcceptance, NdiLicense, NdiLicenseError};
pub use loader::{NdiCapability, NdiLoadStatus};
pub use producer::NdiProducer;
pub use receiver::{FakeNdiReceiver, NdiReceiver, NdiRecvError, NdiRecvFourCc, ReceivedFrame};
#[cfg(feature = "ndi-bindings")]
pub use sdk::SdkNdiReceiver;

/// The mandatory NDI® trademark attribution notice for the About box / NOTICE.
///
/// ADR-0008 / `docs/io/ndi.md` §7.2 make this **load-bearing**: the management
/// surface must render it whenever NDI is enabled (the same string the OUT-3/OUT-4
/// output side surfaces). Re-stated here so an ingest-only NDI build still carries
/// the attribution.
pub const NDI_TRADEMARK_NOTICE: &str = "NDI® is a registered trademark of Vizrt NDI AB";

/// The mandatory link surfaced near NDI uses (UI/docs), per ADR-0008 §7.2.
pub const NDI_ATTRIBUTION_URL: &str = "https://ndi.video";

/// The full attribution block (notice + link) for embedding in an About panel.
#[must_use]
pub fn attribution() -> String {
    format!("{NDI_TRADEMARK_NOTICE}\n{NDI_ATTRIBUTION_URL}")
}
