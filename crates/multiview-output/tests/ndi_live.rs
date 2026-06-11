//! LIVE-ONLY: actually load the proprietary NDI® runtime and create a real
//! sender. Gated `#[ignore]` because it requires (a) the NDI 6 runtime dylib
//! installed/resolvable and (b) acceptance of the proprietary SDK license — none
//! of which exist in CI or this sandbox. Run on a host with the runtime via:
//!
//! ```text
//! cargo test -p multiview-output --features ndi -- --ignored ndi_live
//! ```
//!
//! Without the runtime, `NdiCapability::probe()` reports `RuntimeNotFound` /
//! `Unusable` and the rest of the suite proves the graceful-absent path — this
//! test is the only place the *live* SDK path is exercised, and never runs
//! unattended.
#![cfg(feature = "ndi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::ndi::{NdiCapability, NdiLoadStatus};

#[test]
#[ignore = "requires a resolvable proprietary NDI runtime (NDIlib_v6_load) + license acceptance"]
fn live_runtime_resolves_and_loads() {
    // This only succeeds on a host with the NDI 6 runtime installed.
    let cap = NdiCapability::load().expect("NDI runtime must be resolvable for the live test");
    let _runtime = cap.runtime();
    // If we got here, NDIlib_v6_load returned a non-null function table.
}

#[test]
fn probe_is_typed_and_never_panics_without_runtime() {
    // This DOES run in CI: with no runtime present it must report a typed status,
    // never crash or block. (On a host that happens to have NDI it reports
    // Available — also fine.)
    let status = NdiCapability::probe();
    // The contract is: a typed status with no panic/block. In CI the runtime is
    // absent, so this is RuntimeNotFound; `is_available()` reflects the variant.
    let recognised = matches!(
        status,
        NdiLoadStatus::Available | NdiLoadStatus::RuntimeNotFound | NdiLoadStatus::Unusable { .. }
    );
    assert!(recognised, "probe must return a recognised typed status");
    // `is_available()` is true only for the Available variant.
    assert_eq!(status.is_available(), status == NdiLoadStatus::Available);
}
