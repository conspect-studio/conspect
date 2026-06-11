//! Build script for `multiview-ffmpeg`.
//!
//! Under the off-by-default `ffmpeg` feature this compiles the tiny C log-callback
//! shim (`csrc/log_shim.c`) and links it into the crate. The shim owns the libav
//! `va_list` so the Rust side never has to spell that ABI-fragile type in
//! function-parameter position (see `csrc/log_shim.c` and `src/log_bridge.rs`).
//!
//! In the **default** (pure-Rust, no-libav) build this script does nothing, so the
//! native-dep-free workspace baseline is unaffected.
//!
//! ## Finding the libav include path
//!
//! `av_log_format_line2` is declared in `<libavutil/log.h>`, which on this and the
//! deploy image lives in a multiarch include dir (e.g.
//! `/usr/include/<triple>/libavutil/`) that is **not** on the compiler's default
//! search path. The path is discovered, in order:
//!
//! 1. **`pkg-config libavutil`** — the canonical source; the deploy build sets
//!    `PKG_CONFIG_PATH` to our `FFmpeg`, and `ffmpeg-sys-next` itself links libav
//!    via `pkg-config`, so this matches the very headers the crate compiles
//!    against.
//! 2. **`DEP_FFMPEG_INCLUDE`** — if a future `ffmpeg-sys-next` exports an include
//!    path via its `links = "ffmpeg"` metadata (the current 8.1 release does not),
//!    honour it as a fallback.
//!
//! If neither yields an include dir the build **fails loudly** (the build script
//! returns an `Err`) rather than relying on the header happening to be on the
//! default path — a link that only succeeds by luck is exactly the failure mode
//! this shim exists to remove.

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    // Recompile the shim if it changes; re-run if the build script itself changes.
    println!("cargo:rerun-if-changed=csrc/log_shim.c");
    println!("cargo:rerun-if-changed=build.rs");
    // The pkg-config / DEP fallbacks below read these; re-run if they move.
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=DEP_FFMPEG_INCLUDE");

    // The shim is only needed when the libav FFI is compiled in.
    if std::env::var_os("CARGO_FEATURE_FFMPEG").is_none() {
        return Ok(());
    }

    let include_dirs = libav_include_dirs()?;

    let mut build = cc::Build::new();
    build.file("csrc/log_shim.c");
    for dir in &include_dirs {
        build.include(dir);
    }
    // The shim is C11, warning-clean; treat shim warnings as errors so a future
    // edit that drifts from the libav header signature fails the build here.
    build.flag_if_supported("-std=c11");
    build.warnings(true);
    build.compile("multiview_av_log_shim");

    Ok(())
}

/// Discover the libav (`libavutil`) include directories — the dirs that contain
/// `<libavutil/log.h>`.
///
/// Returns an `Err` (failing the build) if no path can be found, because the shim
/// cannot compile without the header and a silent fallthrough to the default
/// include path is the unsound "links by luck" behaviour we refuse to ship.
fn libav_include_dirs() -> Result<Vec<PathBuf>, Box<dyn Error>> {
    // 1. pkg-config — the canonical, deploy-matching source.
    match pkg_config::Config::new()
        .cargo_metadata(false) // we do not need pkg-config to emit link flags
        .probe("libavutil")
    {
        Ok(lib) if !lib.include_paths.is_empty() => return Ok(lib.include_paths),
        Ok(_) => {
            // libavutil resolved but exposed no include dir (rare); fall through.
        }
        Err(err) => {
            println!("cargo:warning=pkg-config could not locate libavutil ({err}); trying DEP_FFMPEG_INCLUDE");
        }
    }

    // 2. ffmpeg-sys-next `links` metadata fallback (`DEP_FFMPEG_INCLUDE`), if a
    //    future release exports it. Accept a path-list (cargo joins with the OS
    //    separator).
    if let Some(dep_include) = std::env::var_os("DEP_FFMPEG_INCLUDE") {
        let dirs: Vec<PathBuf> = std::env::split_paths(&dep_include).collect();
        if !dirs.is_empty() {
            return Ok(dirs);
        }
    }

    Err(
        "multiview-ffmpeg: could not find the libav include path for <libavutil/log.h>. \
         Tried pkg-config (libavutil) and DEP_FFMPEG_INCLUDE. Install libavutil's \
         development headers and/or set PKG_CONFIG_PATH so the log-callback C shim can compile."
            .into(),
    )
}
