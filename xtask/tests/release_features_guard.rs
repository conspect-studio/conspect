//! End-to-end health check for the release-artifact feature guard (task #109).
//!
//! Runs the REAL guard — which resolves each shipped release preset with `cargo
//! tree` (feature RESOLUTION only, no compile) — and asserts that no shipped
//! preset enables an internal/test-only seam feature (e.g.
//! `multiview-control/_test-seams`). This exercises the real `cargo tree`
//! plumbing and the real `RELEASE_FEATURE_SPECS` list, complementing the pure
//! parser/orchestration unit tests in `src/release_features.rs`.
//!
//! It shells out to `cargo`; the `cargo test` runner always provides it (via the
//! `CARGO` env var the guard honours), and `--locked` resolves from the
//! committed `Cargo.lock`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use xtask::release_features::check_release_features;

#[test]
fn shipped_release_presets_enable_no_test_seam() {
    let report = check_release_features()
        .expect("`cargo tree` resolution of the release presets should succeed");

    assert!(
        !report.has_violations(),
        "a shipped release preset resolves a forbidden test-only seam feature:\n{}",
        report.render()
    );

    // Guard against a silently-emptied RELEASE_FEATURE_SPECS: the four umbrella
    // presets must actually have been resolved (an empty list would vacuously
    // "pass" has_violations()).
    for preset in ["nvidia", "apple", "linux-vaapi", "full"] {
        assert!(
            report.outcomes.iter().any(|o| o.spec == preset),
            "the guard did not resolve the `{preset}` release preset"
        );
    }
}
