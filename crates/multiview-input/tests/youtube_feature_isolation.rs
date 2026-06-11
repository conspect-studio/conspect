//! Feature-isolation guard for the `youtube` resolver (ADR-0015 / IN-7).
//!
//! The `youtube` feature is the **pure-Rust** `yt-dlp` resolver (info-dict parse,
//! live-status classification, `expire`-deadline parse, the `tokio::process` spawn
//! shell, and the off-data-plane re-resolution loop). None of that touches libav:
//! the resolved `*.googlevideo.com` HLS master is handed *verbatim* to the
//! `ffmpeg`-gated HLS ingest path, which the CLI enables via its **own** `ffmpeg`
//! feature — the resolver never links or compiles against libav.
//!
//! So `cargo clippy -p multiview-input --features youtube` must lint on a
//! libav-free runner (the `feature-gated clippy (multiview-input, youtube)` CI
//! leg). If the `youtube` feature ever re-acquires a transitive `ffmpeg` edge,
//! `ffmpeg-sys-next`'s build script panics there (no `libavutil.pc`) and that CI
//! leg goes red.
//!
//! This file pins the boundary two ways:
//!
//! * a **compile-time** guard ([`compile_error!`] below) makes the coupling a hard
//!   build failure on the exact feature set CI lints — `youtube` together with
//!   `ffmpeg` cannot compile, so a re-coupling in `Cargo.toml` is caught at build
//!   time, not just at the eventual libav-less panic; and
//! * a **behavioural** test that drives the pure resolver end to end (parse a
//!   recorded `yt-dlp -J` info-dict into a usable HLS master URL + `expire`
//!   deadline) with no network, no subprocess, and no libav — proving the
//!   resolver's correctness load is reachable on a libav-free build.
#![cfg(feature = "youtube")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// The `youtube` feature must stay libav-free: it must NOT enable `multiview-input`'s
// `ffmpeg` feature. If a Cargo.toml edge ever re-couples them, this build fails on
// the exact feature set the `feature-gated clippy (multiview-input, youtube)` CI leg
// uses — long before ffmpeg-sys-next's build script would panic with no
// libavutil.pc on the libav-free runner.
#[cfg(all(feature = "youtube", feature = "ffmpeg"))]
compile_error!(
    "the `youtube` feature must stay libav-free: it must NOT transitively enable \
     multiview-input's `ffmpeg` feature (the libav-free `feature-gated clippy \
     (multiview-input, youtube)` CI leg builds with exactly `--features youtube` \
     and has no libavutil.pc). Enable `ffmpeg` on the consuming crate's own \
     feature instead (e.g. multiview-cli's `youtube = [\"ffmpeg\", …]`)."
);

use multiview_input::youtube::{parse_info_dict, LiveStatus};

/// A trimmed live `yt-dlp -J` info-dict carrying an HLS master with an `expire`
/// query param. Mirrors the shape the pure resolver classifies as live.
const LIVE_INFO_JSON: &str = r#"{
  "id": "abcdEFGH123",
  "live_status": "is_live",
  "formats": [
    {
      "manifest_url": "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8",
      "protocol": "m3u8_native"
    }
  ]
}"#;

/// The pure resolver core resolves a live info-dict to a usable HLS master URL +
/// `expire` deadline with no libav, no network, and no subprocess.
///
/// This binary is built with exactly `--features youtube` on the libav-free CI leg;
/// that it compiles and this parse succeeds proves the resolver's correctness load
/// is reachable without pulling `ffmpeg`. (The compile-time guard above pins that
/// the feature set itself stays libav-free.)
#[test]
fn youtube_resolver_works_without_libav() {
    let resolved = parse_info_dict(LIVE_INFO_JSON).expect("a live info-dict resolves");

    assert_eq!(resolved.live_status, LiveStatus::Live);
    assert_eq!(
        resolved.manifest_url,
        "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8",
    );
    // The `expire` deadline is parsed off the resolved URL (path form here), so the
    // re-resolution loop has a deadline to refresh ahead of — all libav-free.
    assert_eq!(resolved.expire_unix, Some(1_893_456_000));
}
