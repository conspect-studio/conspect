//! GPU-free static validation of the WGSL compositor shaders.
//!
//! `naga` parses + validates both shader modules WITHOUT any GPU/adapter, so
//! this runs on every CI runner (no `/dev/dri`, no Vulkan needed) and proves
//! the WGSL is well-formed and type-correct. It is the GPU-less gate that keeps
//! the fixed-order pipeline shaders honest.
#![cfg(feature = "wgpu")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::gpu::shader;

#[test]
fn both_shaders_parse_and_validate() {
    // Propagates a typed parse/validation error if the WGSL is malformed.
    shader::validate_shaders().expect("composite + encode WGSL must validate with naga");
}

#[test]
fn composite_shader_validates_individually() {
    shader::validate_module("composite.wgsl", &shader::composite_wgsl())
        .expect("composite.wgsl must validate");
}

#[test]
fn encode_shader_validates_individually() {
    shader::validate_module("encode.wgsl", &shader::encode_wgsl())
        .expect("encode.wgsl must validate");
}

#[test]
fn malformed_wgsl_is_rejected_not_panicked() {
    // A real validator must REJECT garbage with a typed error, never accept it
    // and never panic (guards against a tautological "always Ok" stub).
    let err = shader::validate_module("bogus", "this is not valid wgsl {{{")
        .expect_err("garbage WGSL must fail to parse");
    let msg = err.to_string();
    assert!(
        msg.contains("bogus"),
        "error should name the offending module, got: {msg}"
    );
}
