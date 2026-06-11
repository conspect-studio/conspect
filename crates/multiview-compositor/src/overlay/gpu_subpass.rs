//! The GPU overlay sub-pass (feature `overlay` + `wgpu`): one batched compute
//! pass that blends overlay primitives premultiplied-source-over the linear
//! `Rgba16Float` canvas, between the composite and encode passes (ADR-0016
//! §4.1, invariants #5 + #8).
//!
//! There is **no GPU adapter at runtime** in this environment, so this code
//! must *compile* and the WGSL must *`naga`-validate* GPU-free
//! ([`crate::gpu::shader::validate_overlay_shader`]); the actual blend runs on
//! the CPU reference ([`crate::overlay::subpass::blend_overlays`]) in tests. The
//! two share the identical primitive model and `over` math (T7).
//!
//! Primitives are packed into one storage buffer (T5 batching); glyph coverage
//! is sampled from the persistent atlas; rects are evaluated analytically — no
//! per-frame bitmap upload (T1/T3).

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};

use crate::error::Result;
use crate::gpu::device::GpuContext;
use crate::gpu::shader::overlay_wgsl;
use crate::overlay::subpass::{OverlayDrawList, OverlayPrimitive};

/// Hard cap on overlay primitives per frame, sizing the storage buffer.
/// Bounded by design (data-plane memory is fixed, never per-frame; ADR-E005).
pub const MAX_OVERLAY_PRIMS: u32 = 4096;

/// Primitive kind tag, shared with `overlay.wgsl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveKind {
    /// A glyph coverage quad sampling the persistent atlas.
    Glyph = 0,
    /// An analytic (optionally rounded) filled rectangle / line.
    Rect = 1,
    /// An analytic thick, angled line segment (a capsule SDF) — clock hands.
    Stroke = 2,
    /// An analytic stroked ring / annulus (a circle-band SDF) — clock bezel.
    Ring = 3,
    /// A premultiplied-RGBA bitmap blit (DVB-sub / bitmap caption): the cue's
    /// already-premultiplied bytes are uploaded once into an
    /// [`crate::overlay::gpu_image::ImageTextureCache`] layer and sampled by the
    /// shader's `KIND_IMAGE` branch (validated SSIM/PSNR vs the CPU reference
    /// [`crate::overlay::subpass::blend_overlays`], never bit-exact).
    Image = 4,
}

impl PrimitiveKind {
    /// The numeric tag the shader switches on.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Glyph => 0,
            Self::Rect => 1,
            Self::Stroke => 2,
            Self::Ring => 3,
            Self::Image => 4,
        }
    }
}

/// One packed overlay primitive. Mirrors `OverlayPrim` in `overlay.wgsl`; every
/// field is `vec4`-aligned so the std430 storage rules hold without manual pad.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct OverlayPrimGpu {
    /// `[kind, corner_radius, atlas_x, atlas_y]`. For [`PrimitiveKind::Image`]
    /// the second slot is the texture-array layer (not a radius).
    pub kind_meta: [u32; 4],
    /// `[dest_x, dest_y, width, height]` in canvas pixels — the integer bounding
    /// box every primitive is clipped to in the shader.
    pub rect: [i32; 4],
    /// Straight LINEAR RGBA (premultiplied in-shader by coverage). For
    /// [`PrimitiveKind::Image`] only `.a` is used (the layer-opacity fade); the
    /// sampled texel supplies the (already-premultiplied) color.
    pub color: [f32; 4],
    /// Sub-pixel analytic geometry, kind-dependent (`vec4` aligned):
    /// - [`PrimitiveKind::Stroke`]: `[x0, y0, x1, y1]` segment endpoints.
    /// - [`PrimitiveKind::Ring`]: `[cx, cy, mid_radius, band_half]`.
    /// - [`PrimitiveKind::Image`]: `[src_width, src_height, 0, 0]` for the
    ///   nearest-neighbour source-texel map.
    /// - others: unused (zero).
    pub geom: [f32; 4],
}

/// Overlay sub-pass uniform block. Mirrors `OverlayUniforms` in `overlay.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct OverlayUniforms {
    /// `[canvas_w, canvas_h, primitive_count, 0]`.
    pub canvas: [u32; 4],
}

impl OverlayPrimGpu {
    /// Pack a CPU [`OverlayPrimitive`] into the GPU layout.
    ///
    /// A [`OverlayPrimitive::Glyph`] carries its **atlas** location (`atlas_x`,
    /// `atlas_y`) so the shader samples coverage from the persistent atlas
    /// rather than re-uploading the bitmap (T1). [`OverlayPrimitive::FilledRect`]
    /// and [`OverlayPrimitive::Line`] are analytic.
    #[must_use]
    pub fn pack(primitive: &OverlayPrimitive, atlas_x: u32, atlas_y: u32) -> Self {
        match primitive {
            OverlayPrimitive::Glyph {
                dest_x,
                dest_y,
                width,
                height,
                color,
                ..
            } => Self {
                kind_meta: [PrimitiveKind::Glyph.as_u32(), 0, atlas_x, atlas_y],
                rect: [
                    *dest_x,
                    *dest_y,
                    i32_from_u32(*width),
                    i32_from_u32(*height),
                ],
                color: [color.r, color.g, color.b, color.a],
                geom: [0.0; 4],
            },
            OverlayPrimitive::FilledRect {
                rect,
                corner_radius,
                color,
            } => Self {
                kind_meta: [PrimitiveKind::Rect.as_u32(), *corner_radius, 0, 0],
                rect: [
                    rect.x,
                    rect.y,
                    i32_from_u32(rect.width),
                    i32_from_u32(rect.height),
                ],
                color: [color.r, color.g, color.b, color.a],
                geom: [0.0; 4],
            },
            OverlayPrimitive::Line { rect, color } => Self {
                kind_meta: [PrimitiveKind::Rect.as_u32(), 0, 0, 0],
                rect: [
                    rect.x,
                    rect.y,
                    i32_from_u32(rect.width),
                    i32_from_u32(rect.height),
                ],
                color: [color.r, color.g, color.b, color.a],
                geom: [0.0; 4],
            },
            OverlayPrimitive::Stroke {
                x0,
                y0,
                x1,
                y1,
                half_thickness,
                color,
            } => {
                let pad = half_thickness.max(0.0) + 1.0;
                let bb = segment_bbox(*x0, *y0, *x1, *y1, pad);
                Self {
                    kind_meta: [PrimitiveKind::Stroke.as_u32(), bits(*half_thickness), 0, 0],
                    rect: bb,
                    color: [color.r, color.g, color.b, color.a],
                    geom: [*x0, *y0, *x1, *y1],
                }
            }
            OverlayPrimitive::Ring {
                cx,
                cy,
                outer_radius,
                thickness,
                color,
            } => {
                let half = thickness.max(0.0) / 2.0;
                let mid_radius = (outer_radius - half).max(0.0);
                let pad = outer_radius.max(0.0) + 1.0;
                let bb = segment_bbox(*cx, *cy, *cx, *cy, pad);
                Self {
                    kind_meta: [PrimitiveKind::Ring.as_u32(), 0, 0, 0],
                    rect: bb,
                    color: [color.r, color.g, color.b, color.a],
                    geom: [*cx, *cy, mid_radius, half],
                }
            }
            // An Image cue routes to the GPU texture (KIND_IMAGE) branch. The
            // texture-array layer is resolved by the image cache before pack, so
            // the generic entry point packs onto layer 0; a real dispatch packs
            // via `pack_image` with the resolved layer. The CPU reference
            // (subpass::blend_image) is the SSIM/PSNR oracle for the GPU blit.
            OverlayPrimitive::Image { .. } => Self::pack_image(primitive, 0),
        }
    }

    /// Pack an [`OverlayPrimitive::Image`] onto texture-array `layer`, the slot
    /// the [`crate::overlay::gpu_image::ImageTextureCache`] resolved its
    /// premultiplied bitmap into (uploaded once, reused across ticks).
    ///
    /// The layout the shader's `KIND_IMAGE` branch reads:
    /// - `kind_meta = [KIND_IMAGE, layer, 0, 0]` — the texture-array layer to
    ///   `textureLoad` from;
    /// - `rect = [dest_x, dest_y, dest_w, dest_h]` — the destination box the cue
    ///   is nearest-neighbour scaled into and clipped to;
    /// - `geom = [src_w, src_h, 0, 0]` — the source bitmap size, so the shader
    ///   maps each dest pixel to the nearest source texel
    ///   ([`crate::overlay::gpu_image::nearest_source_texel`]);
    /// - `color = [0, 0, 0, fade]` — the layer-opacity fade (the texel itself
    ///   supplies the already-premultiplied color; rgb is unused).
    ///
    /// A non-[`OverlayPrimitive::Image`] primitive is delegated to [`Self::pack`]
    /// so the function is total (the `_ => 0` arms never fabricate an image).
    #[must_use]
    pub fn pack_image(primitive: &OverlayPrimitive, layer: u32) -> Self {
        match primitive {
            OverlayPrimitive::Image {
                dest,
                src_width,
                src_height,
                alpha,
                ..
            } => Self {
                kind_meta: [PrimitiveKind::Image.as_u32(), layer, 0, 0],
                rect: [
                    dest.x,
                    dest.y,
                    i32_from_u32(dest.width),
                    i32_from_u32(dest.height),
                ],
                color: [0.0, 0.0, 0.0, alpha.clamp(0.0, 1.0)],
                geom: [u32_to_f32(*src_width), u32_to_f32(*src_height), 0.0, 0.0],
            },
            other => Self::pack(other, 0, 0),
        }
    }
}

/// The reinterpret-as-`u32` bits of an `f32` (lossless transport of a sub-pixel
/// thickness through a `u32` slot the shader bit-casts back), no `as` cast.
fn bits(value: f32) -> u32 {
    value.to_bits()
}

/// The integer bounding box `[x, y, w, h]` of the segment `(x0,y0)–(x1,y1)`
/// padded by `pad` pixels, clamped to non-negative origin (the shader clips to
/// the canvas). Used for both strokes (a real segment) and rings (a zero-length
/// "segment" at the centre padded by the outer radius).
fn segment_bbox(x0: f32, y0: f32, x1: f32, y1: f32, pad: f32) -> [i32; 4] {
    let min_x = x0.min(x1) - pad;
    let min_y = y0.min(y1) - pad;
    let max_x = x0.max(x1) + pad;
    let max_y = y0.max(y1) + pad;
    let x = floor_to_i32(min_x);
    let y = floor_to_i32(min_y);
    let w = i32_from_u32(ceil_span(min_x, max_x));
    let h = i32_from_u32(ceil_span(min_y, max_y));
    [x, y, w, h]
}

/// `floor(value)` to `i32` (saturating), no `as` cast.
fn floor_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let f = value.floor();
    if f < 0.0 {
        i32_from_u32(u32_from_f32(-f)).saturating_neg()
    } else {
        i32_from_u32(u32_from_f32(f))
    }
}

/// The pixel span `ceil(hi) - floor(lo)` as a `u32` (saturating), no `as` cast.
fn ceil_span(lo: f32, hi: f32) -> u32 {
    if !lo.is_finite() || !hi.is_finite() || hi < lo {
        return 0;
    }
    let span = hi.ceil() - lo.floor();
    u32_from_f32(span).saturating_add(1)
}

/// Round a non-negative `f32` to `u32` (saturating), no `as` cast.
fn u32_from_f32(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let target = value.round();
    let mut lo = 0_u32;
    let mut hi = u32::MAX;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if u32_to_f32(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Exact small-`u32` → `f32`, no `as`.
fn u32_to_f32(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// The compiled overlay sub-pass: the compute pipeline + its bind-group layout.
///
/// Construct with [`OverlaySubpass::new`], which compiles + (implicitly) the
/// WGSL is naga-validated by [`crate::gpu::shader::validate_overlay_shader`].
#[derive(Debug)]
pub struct OverlaySubpass {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl OverlaySubpass {
    /// Compile the overlay sub-pass pipeline on an existing [`GpuContext`].
    ///
    /// # Errors
    ///
    /// Propagates a shader compile failure surfaced by the device.
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("multiview overlay subpass"),
            source: wgpu::ShaderSource::Wgsl(overlay_wgsl().into()),
        });
        let layout = bind_group_layout(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("multiview overlay layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview overlay pipeline"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("overlay_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        Ok(Self { pipeline, layout })
    }

    /// The compute pipeline (for the compositor to bind into its encoder).
    #[must_use]
    pub fn pipeline(&self) -> &wgpu::ComputePipeline {
        &self.pipeline
    }

    /// The bind-group layout (canvas-in texture, canvas-out storage, atlas,
    /// uniforms, packed primitive buffer).
    #[must_use]
    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    /// Pack a whole [`OverlayDrawList`] into GPU primitives, resolving each
    /// glyph's atlas slot via `atlas_slot` (returns `(x, y)` texels). Caps at
    /// [`MAX_OVERLAY_PRIMS`].
    ///
    /// Glyph primitives whose `atlas_slot` resolves to `None` (not resident) are
    /// skipped — the layer holds last-good rather than crashing (hot-path rule).
    #[must_use]
    pub fn pack_list(
        list: &OverlayDrawList,
        mut atlas_slot: impl FnMut(usize) -> Option<(u32, u32)>,
    ) -> Vec<OverlayPrimGpu> {
        let cap = usize::try_from(MAX_OVERLAY_PRIMS).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(list.primitives.len().min(cap));
        for (i, primitive) in list.primitives.iter().enumerate() {
            if out.len() >= cap {
                break;
            }
            match primitive {
                OverlayPrimitive::Glyph { .. } => {
                    let Some((ax, ay)) = atlas_slot(i) else {
                        continue;
                    };
                    out.push(OverlayPrimGpu::pack(primitive, ax, ay));
                }
                _ => out.push(OverlayPrimGpu::pack(primitive, 0, 0)),
            }
        }
        out
    }

    /// Pack a whole [`OverlayDrawList`], resolving glyph atlas slots via
    /// `atlas_slot` **and** image-cue texture-array layers via `image_layer`
    /// (returns the layer the cue's premultiplied bitmap was uploaded into;
    /// `None` skips a not-yet-resident image, holding last-good). Caps at
    /// [`MAX_OVERLAY_PRIMS`].
    ///
    /// This is the dispatch-path packer: the caller drives the
    /// [`crate::overlay::gpu_image::ImageTextureCache`] to assign each Image cue
    /// a layer (uploading only on a cache miss) and threads that layer here, so
    /// the shader's `KIND_IMAGE` branch samples the right array layer.
    #[must_use]
    pub fn pack_list_with_images(
        list: &OverlayDrawList,
        mut atlas_slot: impl FnMut(usize) -> Option<(u32, u32)>,
        mut image_layer: impl FnMut(usize) -> Option<u32>,
    ) -> Vec<OverlayPrimGpu> {
        let cap = usize::try_from(MAX_OVERLAY_PRIMS).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(list.primitives.len().min(cap));
        for (i, primitive) in list.primitives.iter().enumerate() {
            if out.len() >= cap {
                break;
            }
            match primitive {
                OverlayPrimitive::Glyph { .. } => {
                    let Some((ax, ay)) = atlas_slot(i) else {
                        continue;
                    };
                    out.push(OverlayPrimGpu::pack(primitive, ax, ay));
                }
                OverlayPrimitive::Image { .. } => {
                    let Some(layer) = image_layer(i) else {
                        continue;
                    };
                    out.push(OverlayPrimGpu::pack_image(primitive, layer));
                }
                _ => out.push(OverlayPrimGpu::pack(primitive, 0, 0)),
            }
        }
        out
    }
}

/// The overlay sub-pass bind-group layout (mirrors `overlay.wgsl` bindings).
fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("multiview overlay bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<OverlayUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<OverlayPrimGpu>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba16Float,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            // The image-cue texture-array: one `Rgba8Unorm` layer per resident
            // premultiplied bitmap (DVB-sub / libass), uploaded once by the
            // image cache and sampled by the `KIND_IMAGE` branch with
            // `textureLoad` (non-filterable; the shader nearest-maps itself).
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
        ],
    })
}

/// `min_binding_size` for a `#[repr(C)]` uniform/storage block `T`
/// (mirrors `gpu::compositor::min_binding_size`; no `as` cast).
fn min_binding_size<T>() -> Option<NonZeroU64> {
    u64::try_from(core::mem::size_of::<T>())
        .ok()
        .and_then(NonZeroU64::new)
}

/// Saturating `u32 -> i32` (overlay sizes are small), no `as` cast.
fn i32_from_u32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
