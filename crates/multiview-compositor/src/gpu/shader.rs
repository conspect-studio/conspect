//! WGSL shader sources for the GPU compositor and a GPU-free static validator.
//!
//! The two pass shaders ([`composite_wgsl`], [`encode_wgsl`]) are each the
//! shared [`COMMON_WGSL`] prelude concatenated with the pass body
//! ([`COMPOSITE_BODY_WGSL`] / [`ENCODE_BODY_WGSL`]), so the color math lives in
//! exactly one place and both passes agree with the CPU reference.
//! [`validate_shaders`] parses and validates both with `naga`, which requires
//! **no GPU** — it is the CI gate that keeps the WGSL honest on GPU-less
//! runners.

use naga::valid::{Capabilities, ValidationFlags, Validator};

use crate::error::{Error, Result};

/// Shared color-math prelude (transfer functions, matrix apply, quantize).
pub const COMMON_WGSL: &str = include_str!("shaders/common.wgsl");

/// Composite-pass body (front half + premultiplied-alpha blend in linear).
pub const COMPOSITE_BODY_WGSL: &str = include_str!("shaders/composite.wgsl");

/// Encode-pass body (back half: OETF -> RGB->YUV -> range compress -> NV12).
pub const ENCODE_BODY_WGSL: &str = include_str!("shaders/encode.wgsl");

/// Overlay sub-pass body (batched premultiplied-linear `over` blend into the
/// existing canvas, between composite and encode — feature `overlay`).
#[cfg(feature = "overlay")]
pub const OVERLAY_BODY_WGSL: &str = include_str!("shaders/overlay.wgsl");

/// Full composite-pass WGSL source (prelude + body).
#[must_use]
pub fn composite_wgsl() -> String {
    format!("{COMMON_WGSL}\n{COMPOSITE_BODY_WGSL}")
}

/// Full encode-pass WGSL source (prelude + body).
#[must_use]
pub fn encode_wgsl() -> String {
    format!("{COMMON_WGSL}\n{ENCODE_BODY_WGSL}")
}

/// Full overlay sub-pass WGSL source (prelude + body), feature `overlay`.
#[cfg(feature = "overlay")]
#[must_use]
pub fn overlay_wgsl() -> String {
    format!("{COMMON_WGSL}\n{OVERLAY_BODY_WGSL}")
}

/// Validate the overlay sub-pass shader ([`overlay_wgsl`]) on the CPU (no GPU).
///
/// # Errors
///
/// Returns [`Error::ShaderParse`] / [`Error::ShaderValidation`] if the WGSL is
/// malformed or fails `naga` validation.
#[cfg(feature = "overlay")]
pub fn validate_overlay_shader() -> Result<()> {
    validate_module("overlay.wgsl", &overlay_wgsl())
}

/// Parse and validate one WGSL module with `naga`.
///
/// This runs entirely on the CPU (no adapter/device), so it is callable in
/// GPU-free CI to prove the shaders are well-formed and type-correct before any
/// runtime `create_shader_module`.
///
/// # Errors
///
/// Returns [`Error::ShaderParse`] if the WGSL fails to parse, or
/// [`Error::ShaderValidation`] if it parses but fails `naga` validation.
pub fn validate_module(label: &'static str, source: &str) -> Result<()> {
    let module = naga::front::wgsl::parse_str(source)
        .map_err(|e| Error::ShaderParse(format!("{label}: {e}")))?;
    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
    validator
        .validate(&module)
        .map_err(|e| Error::ShaderValidation(format!("{label}: {e}")))?;
    Ok(())
}

/// Validate both compositor shaders ([`composite_wgsl`] and [`encode_wgsl`]).
///
/// # Errors
///
/// Propagates the first [`Error::ShaderParse`] / [`Error::ShaderValidation`].
pub fn validate_shaders() -> Result<()> {
    validate_module("composite.wgsl", &composite_wgsl())?;
    validate_module("encode.wgsl", &encode_wgsl())?;
    Ok(())
}
