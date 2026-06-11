//! Packaging guard: every **deploy hardware preset** must ship the native
//! caption / overlay-burn-in path.
//!
//! The native HLS-WebVTT caption ingest + per-tile burn-in (the `#47` fix) is
//! entirely gated on `#[cfg(feature = "overlay")]` in `multiview-cli`
//! (`prefetch_caption_plans` / `wire_source_captions` / the `SubtitleRouter` /
//! the overlay baker). The deploy images build the `multiview` binary with a
//! single hardware preset (`nvidia` / `apple` / `linux-vaapi`); if a preset does
//! NOT pull `overlay`, the shipped image silently has the caption code compiled
//! but *inert* — no cue stores, no reader threads, nothing burned in.
//!
//! This test is a **compile-feature-aware** assertion: whenever the crate is
//! built with a deploy hardware preset enabled, the `overlay` feature MUST also
//! be active. It is the regression guard for the packaging gap where the deploy
//! `nvidia` preset shipped `ffmpeg,nvidia,web` WITHOUT `overlay`, defeating the
//! native-captions fix.
//!
//! Run it against the actual deploy feature set, e.g.
//! `cargo test -p multiview-cli --features nvidia deploy_preset_ships_captions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Whether THIS build enabled any deploy hardware preset. The presets are the
/// feature flags the deploy Dockerfiles pass as `CARGO_FEATURES` (see
/// `deploy/Dockerfile.nvidia` and `deploy/Dockerfile`). `full` is a superset of
/// `nvidia` + `linux-vaapi`, so it is covered transitively.
fn deploy_hardware_preset_enabled() -> bool {
    cfg!(feature = "nvidia") || cfg!(feature = "apple") || cfg!(feature = "linux-vaapi")
}

/// Whether the native caption / overlay-burn-in path is compiled into THIS build.
fn overlay_caption_path_enabled() -> bool {
    cfg!(feature = "overlay")
}

#[test]
fn deploy_preset_ships_captions() {
    if deploy_hardware_preset_enabled() {
        assert!(
            overlay_caption_path_enabled(),
            "a deploy hardware preset (nvidia/apple/linux-vaapi) is enabled but the `overlay` \
             feature is NOT — the native HLS-WebVTT caption burn-in path is `#[cfg(feature = \
             \"overlay\")]`-gated, so the shipped deploy image would silently have NO native \
             captions. Every deploy hardware preset MUST pull `overlay`."
        );
    }
}
