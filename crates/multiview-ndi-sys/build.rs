//! Build script for `multiview-ndi-sys`.
//!
//! Under the **off-by-default `bindings` feature** (pulled by the consumers'
//! `ndi` feature) this generates the typed `NDIlib` v6 ABI — the
//! `NDIlib_v6` function table plus the frame/source/find structs — from the
//! **licensed** SDK header via `bindgen` (NDI-L1). The header is located at build
//! time and is **never vendored or committed** (ADR-0008); the runtime `.so` is
//! still `dlopen`-loaded at run time, never linked here.
//!
//! With the feature OFF this is a no-op: the crate compiles as the pure
//! runtime-loader and needs no SDK present.

// Justification (no silent suppression): a build script communicates with Cargo
// exclusively over stdout (`cargo:` directives), so `print_stdout` is inherent to
// the job here and is allowed only in this build script.
#![allow(clippy::print_stdout)]

// reason: the `Result` is load-bearing under the `bindings` feature (bindgen +
// OUT_DIR errors propagate via `?`); without the feature `main` is trivial, which
// is the build clippy sees by default — so allow the otherwise-spurious lint.
#[allow(clippy::unnecessary_wraps)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "bindings")]
    generate_ndi_bindings()?;
    Ok(())
}

/// Generate `ndi_bindings.rs` in `OUT_DIR` from the licensed `NDIlib` header.
#[cfg(feature = "bindings")]
fn generate_ndi_bindings() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    println!("cargo:rerun-if-env-changed=NDI_SDK_INCLUDE_DIR");

    let include = locate_sdk_include()?;
    let header = include.join("Processing.NDI.Lib.h");
    if !header.is_file() {
        return Err(format!(
            "the `bindings` feature requires the NDI SDK header, not found at {}. \
             Set NDI_SDK_INCLUDE_DIR, install the SDK to /opt/ndi/include, or drop it \
             in the workspace .ndi-sdk/ (ADR-0008: never vendored/committed).",
            header.display()
        )
        .into());
    }
    println!("cargo:rerun-if-changed={}", header.display());

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        // Only the NDIlib ABI surface — not the whole libc/system transitive set.
        .allowlist_type("NDIlib_.*")
        .allowlist_function("NDIlib_.*")
        .allowlist_var("NDIlib_.*")
        .allowlist_var("NDILIB_.*")
        .prepend_enum_name(false)
        .generate_comments(false)
        // The struct layouts are the load-bearing ABI; bindgen derives them from
        // the real header, so layout tests would only re-assert what bindgen
        // already computed against the same header.
        .layout_tests(false)
        .generate()?;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    bindings.write_to_file(out_dir.join("ndi_bindings.rs"))?;
    Ok(())
}

/// Locate the NDI SDK `include/` directory: an explicit override first, then the
/// canonical install prefix, then the gitignored workspace drop-spot.
#[cfg(feature = "bindings")]
fn locate_sdk_include() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    if let Ok(dir) = std::env::var("NDI_SDK_INCLUDE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let candidates = [
        PathBuf::from("/opt/ndi/include"),
        manifest.join("../../.ndi-sdk/include"),
    ];
    for candidate in candidates {
        if candidate.join("Processing.NDI.Lib.h").is_file() {
            return Ok(candidate);
        }
    }
    // None found: return the canonical path so the caller emits a clear error.
    Ok(PathBuf::from("/opt/ndi/include"))
}
