//! Build-capability gating for configured outputs (DEV-B1 / ADR-0044).
//!
//! A `display` output is a raw-frame DRM/KMS sink that exists only in a
//! `display-kms` build of the `multiview` binary. A build **without** that
//! feature must FAIL a run whose config declares one — clearly, at
//! validation/build time — never silently skip it (a silently-skipped output
//! is a dead monitor nobody can explain). This module is always compiled, so
//! the default CI build exercises the rejection path and a `display-kms`
//! build exercises the acceptance path.

use multiview_config::Output;

/// Ensure every `Output::Display` in `outputs` is runnable in this build.
///
/// In a `display-kms` build this always succeeds. In any other build it
/// returns a clear, actionable error naming the offending output(s) and the
/// required feature.
///
/// # Errors
///
/// A human-readable message naming each display output and the `display-kms`
/// build requirement, when this binary was built without the feature.
pub fn ensure_display_outputs_supported(outputs: &[Output]) -> Result<(), String> {
    let displays: Vec<String> = outputs
        .iter()
        .filter(|o| matches!(o, Output::Display { .. }))
        .map(Output::label)
        .collect();
    if displays.is_empty() {
        return Ok(());
    }
    if cfg!(feature = "display-kms") {
        return Ok(());
    }
    Err(format!(
        "output kind display requires the display-kms build: this binary was built \
         without the `display-kms` feature, so the configured display output(s) \
         {displays:?} cannot run (rebuild with `--features display-kms`, or remove \
         the display output(s))"
    ))
}
