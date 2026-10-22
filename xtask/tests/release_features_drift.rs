//! Anti-drift guard for `RELEASE_FEATURE_SPECS` (task #109, PR #255 Codex
//! review finding).
//!
//! `RELEASE_FEATURE_SPECS` is the list the release-feature guard actually
//! resolves. If a shipped artifact's `--features` combo is added to a canonical
//! shipping source (`.github/workflows/release.yml`, `.github/workflows/docker.yml`,
//! `deploy/Dockerfile*`) without being listed there, the guard would silently
//! skip that combo. This test DERIVES the shipped combos straight from those
//! sources and asserts `RELEASE_FEATURE_SPECS` covers every one — so the drift
//! fails CI instead of rotting silently.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::path::{Path, PathBuf};

use xtask::release_features::{
    extract_shipped_specs, uncovered_specs, RELEASE_FEATURE_SPECS, SHIPPING_SOURCES,
};

/// The workspace root: `CARGO_MANIFEST_DIR` is `<root>/xtask`, so its parent is
/// the repo root the shipping-source paths are relative to.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent (the workspace root)")
        .to_path_buf()
}

#[test]
fn release_feature_specs_cover_every_shipped_combo() {
    let root = workspace_root();

    let mut derived: Vec<String> = Vec::new();
    for source in SHIPPING_SOURCES {
        let path = root.join(source.path);
        let text = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "shipping source `{}` not found ({err}); if it moved, update \
                 SHIPPING_SOURCES in xtask/src/release_features.rs",
                source.path
            )
        });
        let specs = extract_shipped_specs(source.kind, &text);
        // Per-source non-vacuity (PR #255 re-review finding): a single global
        // `!derived.is_empty()` check lets ONE source whose parser silently
        // returns zero combos (its syntax drifted) pass, because the other
        // sources keep `derived` non-empty — that source would then be
        // silently unguarded. Assert each source independently yields at least
        // one combo, so a broken extractor fails CI instead of rotting.
        assert!(
            !specs.is_empty(),
            "shipping source `{}` yielded no feature combos — its extractor is \
             broken or the file format changed; fix it in \
             xtask/src/release_features.rs",
            source.path
        );
        derived.extend(specs);
    }

    // Backstop: if SHIPPING_SOURCES itself were emptied, the per-source loop
    // above never runs and the coverage check would vacuously pass.
    assert!(
        !derived.is_empty(),
        "no shipped feature combos were parsed from the canonical sources — \
         SHIPPING_SOURCES is empty or every extractor is broken"
    );

    let uncovered = uncovered_specs(&derived, RELEASE_FEATURE_SPECS);
    assert!(
        uncovered.is_empty(),
        "shipped feature combo(s) not covered by RELEASE_FEATURE_SPECS \
         (xtask/src/release_features.rs) — add each so the release-feature guard \
         resolves it:\n  {}",
        uncovered.join("\n  ")
    );
}
