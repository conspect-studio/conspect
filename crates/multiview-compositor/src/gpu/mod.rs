//! The wgpu GPU compositor (feature `wgpu`).
//!
//! This is the portable GPU backend (conventions §3): it runs the **same**
//! fixed-order color pipeline as the CPU reference (invariant #8), in WGSL, on
//! NV12 tiles (invariant #5), and produces output that matches the CPU oracle
//! within an SSIM/PSNR threshold (GPU is never bit-exact; see core-engine §19).
//!
//! The whole module is gated behind the off-by-default `wgpu` feature so the
//! default build stays pure-Rust, fast, and cargo-deny clean. When no GPU is
//! present (this devcontainer, most CI runners) [`GpuCompositor::new`] returns
//! [`crate::error::Error::NoAdapter`] rather than panicking, so callers fall
//! back to the CPU path and tests skip gracefully — but the code still
//! **compiles**, and the WGSL is **statically validated** with `naga`
//! ([`shader::validate_shaders`]) without a GPU.

pub mod compositor;
pub mod device;
pub(crate) mod pool;
pub mod shader;
pub mod uniforms;

pub use compositor::{GpuCompositor, MAX_TILES};
pub use device::GpuContext;
pub use shader::{validate_module, validate_shaders};
pub use uniforms::{CompositeUniforms, EncodeUniforms, TileParams, TransferId};
