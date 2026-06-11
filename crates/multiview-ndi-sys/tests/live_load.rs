//! Live NDI runtime test (NDI-L1): load the operator-provided NDI runtime,
//! reinterpret the table as the typed `NDIlib` v6 ABI, and call a real function
//! pointer through it. This proves the runtime-load + the `bindgen`'d ABI work
//! against the real licensed SDK on hardware.
//!
//! `#[ignore]` — it needs a resolvable NDI runtime (`libndi_advanced.so.6` /
//! `libndi.so.6`) present, which CI does not have. Run on a runtime-equipped host
//! (e.g. the `x86_64` box where the Advanced SDK libndi is installed):
//!
//! ```text
//! cargo test -p multiview-ndi-sys --features bindings --test live_load -- --ignored --nocapture
//! ```
#![cfg(feature = "bindings")]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::CStr;

use multiview_ndi_sys::NdiRuntime;

#[test]
#[ignore = "requires a resolvable NDI runtime (libndi_advanced.so.6 / libndi.so.6)"]
fn live_load_reads_sdk_version() {
    let runtime = NdiRuntime::load().expect("an NDI runtime should be resolvable on this host");
    let v6 = runtime.api_table().v6();
    assert!(!v6.is_null(), "the v6 table pointer is non-null");

    // SAFETY: `v6` points at the process-lifetime NDIlib v6 function table owned
    // by the still-live `runtime` (its `Library` stays mapped for this scope). We
    // read its documented `NDIlib_version` function pointer and call it; per the
    // SDK it takes no arguments, has no preconditions, and returns a pointer to a
    // process-static NUL-terminated string.
    let version = unsafe {
        let table = &*v6;
        // The DynamicLoad table is a sequence of anonymous unions, each carrying a
        // function pointer under both its current and deprecated name; bindgen
        // names them `__bindgen_anon_N`. `version` lives in the 3rd slot. Reading a
        // union field is `unsafe` (both names alias the same pointer).
        let version_fn = table
            .__bindgen_anon_3
            .version
            .expect("the v6 table exposes the version fn");
        CStr::from_ptr(version_fn())
    };

    let text = version.to_str().expect("the version string is valid UTF-8");
    println!("NDI runtime version via the bindgen'd v6 table: {text}");
    assert!(
        text.to_ascii_uppercase().contains("NDI"),
        "the version string identifies NDI: {text}"
    );
}
