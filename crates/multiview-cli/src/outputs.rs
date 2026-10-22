//! Build-capability gating for configured outputs (DEV-B1 / ADR-0044).
//!
//! A `display` output is a raw-frame DRM/KMS sink that exists only in a
//! `display-kms` build of the `multiview` binary. A build **without** that
//! feature must FAIL a run whose config declares one — clearly, at
//! validation/build time — never silently skip it (a silently-skipped output
//! is a dead monitor nobody can explain). This module is always compiled, so
//! the default CI build exercises the rejection path and a `display-kms`
//! build exercises the acceptance path.

use multiview_config::{Output, Source, SourceKind};

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

/// Ensure every `Output::Aes67` in `outputs` is runnable in this build (#103).
///
/// AES67 / ST 2110-30 raw-PCM multicast output exists only in an `aes67` build of
/// the `multiview` binary. In an `aes67` build this always succeeds; in any other
/// build it returns a clear, actionable error naming the offending output(s) and
/// the required feature (never a silent skip — a silently-dropped output is a
/// dead stream nobody can explain; the `display-kms` precedent).
///
/// # Errors
///
/// A human-readable message naming each AES67 output and the `aes67` build
/// requirement, when this binary was built without the feature.
pub fn ensure_aes67_outputs_supported(outputs: &[Output]) -> Result<(), String> {
    let aes67: Vec<String> = outputs
        .iter()
        .filter(|o| matches!(o, Output::Aes67 { .. }))
        .map(Output::label)
        .collect();
    if aes67.is_empty() {
        return Ok(());
    }
    if cfg!(feature = "aes67") {
        return Ok(());
    }
    Err(format!(
        "output kind aes67 requires the aes67 build: this binary was built without \
         the `aes67` feature, so the configured AES67 output(s) {aes67:?} cannot run \
         (rebuild with `--features aes67`, or remove the aes67 output(s))"
    ))
}

/// Ensure every `SourceKind::Aes67` in `sources` is runnable in this build (#103).
///
/// AES67 / ST 2110-30 PCM-audio ingest exists only in an `aes67` build. In an
/// `aes67` build this always succeeds; in any other build it returns a clear,
/// actionable error naming the offending source(s) and the required feature
/// (never a silent skip — the same fail-closed contract as an AES67 output or a
/// `display` output in a non-`display-kms` build).
///
/// # Errors
///
/// A human-readable message naming each AES67 source and the `aes67` build
/// requirement, when this binary was built without the feature.
pub fn ensure_aes67_sources_supported(sources: &[Source]) -> Result<(), String> {
    let aes67: Vec<String> = sources
        .iter()
        .filter(|s| matches!(s.kind, SourceKind::Aes67 { .. }))
        .map(|s| s.id.clone())
        .collect();
    if aes67.is_empty() {
        return Ok(());
    }
    if cfg!(feature = "aes67") {
        return Ok(());
    }
    Err(format!(
        "source kind aes67 requires the aes67 build: this binary was built without \
         the `aes67` feature, so the configured AES67 source(s) {aes67:?} cannot run \
         (rebuild with `--features aes67`, or remove the aes67 source(s))"
    ))
}
