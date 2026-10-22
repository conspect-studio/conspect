//! The wgpu [`GpuCompositor`]: NV12 tiles in, fixed-order pipeline on the GPU,
//! NV12 canvas out.
//!
//! Two compute passes mirror the CPU reference's two halves of the fixed color
//! order (invariant #8):
//!
//! 1. **composite** — upload each tile's NV12 (Y plane R8, interleaved UV plane
//!    Rg8) into a texture array, then per canvas pixel run range-expand ->
//!    YUV->RGB -> linearize -> primaries convert -> premultiplied-alpha blend in
//!    a linear `Rgba16Float` canvas.
//! 2. **encode** — per canvas pixel run canvas OETF -> RGB->YUV -> range
//!    compress, writing an `R8` Y plane and a half-res `Rg8` interleaved UV
//!    plane, read back as an [`Nv12Image`] (invariant #5: stays NV12).
//!
//! Tiles are scaled into their destination rect with the same nearest-neighbour
//! mapping as the CPU reference (`Tile::scaled` / scale-at-composite, RT-6 /
//! ADR-0034); a 1:1 tile (`dst` size == source size) reduces to the identity
//! placement, matching the CPU oracle in geometry. The SSIM/PSNR check covers the
//! GPU's f32 / transcendental drift.

use std::num::NonZeroU64;

use bytemuck::Zeroable;
use multiview_core::traits::{BackendKind, Compositor};

use crate::blend::LinearRgba;
use crate::error::{Error, Result};
use crate::gpu::device::GpuContext;
use crate::gpu::pool::{self, CachedBuffer, Dim2, Dim3, SurfacePool};
use crate::gpu::shader::{composite_wgsl, encode_wgsl};
use crate::gpu::uniforms::{CompositeUniforms, EncodeUniforms, TileParams};
use crate::pipeline::{CanvasColor, Nv12Image, Tile};

/// Hard cap on tiles per composite, sizing the tile texture array and storage
/// buffer. Bounded by design (data-plane memory is fixed, never per-frame).
pub const MAX_TILES: u32 = 64;

/// Layer count of the overlay image-texture array (one per resident
/// premultiplied bitmap cue). Bounded by design (data-plane memory is fixed,
/// never per-frame; ADR-E005). Clamped to
/// [`crate::overlay::gpu_image::MAX_IMAGE_LAYERS`] by the cache.
#[cfg(feature = "overlay")]
pub const OVERLAY_IMAGE_LAYERS: u32 = 16;

/// Per-layer max extent (width = height) of the persistent overlay image
/// texture-array, allocated **once** at construction. A cue larger than this in
/// either dimension is dropped (held last-good) rather than growing the
/// allocation per frame — bounded data-plane memory (ADR-E005). 1024 covers a
/// full-width caption band at common canvas sizes.
#[cfg(feature = "overlay")]
pub const OVERLAY_IMAGE_MAX_DIM: u32 = 1024;

/// A GPU-resident NV12 multiview compositor.
///
/// Owns the device, the two compute pipelines, and their static bind-group
/// layouts. The run-stable per-tick surfaces (the linear canvas, tile arrays,
/// NV12 output planes, readback + uniform/storage buffers) live in a
/// [`SurfacePool`] allocated **once** at first-frame sizing and **reused** every
/// tick — never freed and reallocated per frame (EFF-0, safety rule §5). A
/// dimension change (a rare resize) recreates the affected surface; steady
/// state allocates nothing. Construct with [`GpuCompositor::new`], which fails
/// gracefully (no panic) when there is no GPU.
#[derive(Debug)]
pub struct GpuCompositor {
    ctx: GpuContext,
    composite_pipeline: wgpu::ComputePipeline,
    composite_layout: wgpu::BindGroupLayout,
    encode_pipeline_y: wgpu::ComputePipeline,
    encode_pipeline_uv: wgpu::ComputePipeline,
    encode_layout: wgpu::BindGroupLayout,
    /// The run-stable surface pool (reused per tick, never reallocated per
    /// frame). Behind a `Mutex` for interior mutability: `composite` takes
    /// `&self` (so the trait object stays shareable), but the pool is mutated to
    /// grow/refit a surface on the rare resize. The lock is held only for the
    /// synchronous duration of one composite — never across an `.await` — so it
    /// cannot back-pressure the engine (invariant #10).
    pool: std::sync::Mutex<SurfacePool>,
    /// The overlay sub-pass + its content-keyed image-texture cache, compiled
    /// once and reused (the cache holds the upload-once bookkeeping across
    /// ticks). Present only with the `overlay` feature.
    #[cfg(feature = "overlay")]
    overlay: std::sync::Mutex<OverlayResources>,
}

/// The overlay sub-pass, its content-keyed image-texture cache, and the
/// **persistent** `Rgba8Unorm` image texture-array the cache maps cues into —
/// all owned by the compositor so a static caption is uploaded **once** (into a
/// layer that survives across `composite_with_overlays` calls) and reused
/// thereafter (ADR-0016 upload-once; the cache `needs_upload` flag drives the
/// `write_texture`). The texture-array is allocated once at construction
/// ([`OVERLAY_IMAGE_LAYERS`] layers of [`OVERLAY_IMAGE_MAX_DIM`] px) — never
/// per frame (ADR-E005 bounded memory).
#[cfg(feature = "overlay")]
#[derive(Debug)]
struct OverlayResources {
    subpass: crate::overlay::gpu_subpass::OverlaySubpass,
    image_cache: crate::overlay::gpu_image::ImageTextureCache,
    images: wgpu::Texture,
}

impl GpuCompositor {
    /// Build a GPU compositor, acquiring a headless device and compiling both
    /// pipelines.
    ///
    /// `target` is the load-aware admission decision (ADR-0035 Tier-1): `Some(t)`
    /// pins the compositor to the specific adapter the placement engine chose;
    /// `None` keeps the legacy `HighPerformance` adapter pick. See
    /// [`GpuContext::new`].
    ///
    /// # Errors
    ///
    /// - [`Error::NoAdapter`] / [`Error::DeviceRequest`] when no GPU is
    ///   available (the graceful-degradation path — callers fall back / skip), or
    ///   when `target` names a device no enumerated adapter matches.
    /// - [`Error::ShaderParse`] / [`Error::ShaderValidation`] if a shader is
    ///   malformed (caught by the GPU-free validator too).
    pub fn new(target: Option<&crate::backend::GpuTarget>) -> Result<Self> {
        let ctx = GpuContext::new(target)?;
        Self::with_context(ctx)
    }

    /// Build a GPU compositor on an already-acquired [`GpuContext`].
    ///
    /// # Errors
    ///
    /// Propagates shader/pipeline creation failures.
    pub fn with_context(ctx: GpuContext) -> Result<Self> {
        let device = ctx.device();

        let composite_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("multiview composite"),
            source: wgpu::ShaderSource::Wgsl(composite_wgsl().into()),
        });
        let encode_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("multiview encode"),
            source: wgpu::ShaderSource::Wgsl(encode_wgsl().into()),
        });

        let composite_layout = composite_bind_group_layout(device);
        let encode_layout = encode_bind_group_layout(device);

        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("multiview composite layout"),
                bind_group_layouts: &[Some(&composite_layout)],
                immediate_size: 0,
            });
        let encode_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("multiview encode layout"),
                bind_group_layouts: &[Some(&encode_layout)],
                immediate_size: 0,
            });

        let composite_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview composite pipeline"),
            layout: Some(&composite_pipeline_layout),
            module: &composite_module,
            entry_point: Some("composite_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_y = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview encode Y pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_y_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_uv = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview encode UV pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_uv_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        #[cfg(feature = "overlay")]
        let overlay = {
            let subpass = crate::overlay::gpu_subpass::OverlaySubpass::new(&ctx)?;
            let image_cache =
                crate::overlay::gpu_image::ImageTextureCache::new(OVERLAY_IMAGE_LAYERS);
            // Allocate the persistent image texture-array ONCE: a static caption
            // uploaded the first tick survives in its layer across frames.
            let images = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("multiview overlay image layers"),
                size: wgpu::Extent3d {
                    width: OVERLAY_IMAGE_MAX_DIM,
                    height: OVERLAY_IMAGE_MAX_DIM,
                    depth_or_array_layers: OVERLAY_IMAGE_LAYERS.max(1),
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            std::sync::Mutex::new(OverlayResources {
                subpass,
                image_cache,
                images,
            })
        };

        Ok(Self {
            ctx,
            composite_pipeline,
            composite_layout,
            encode_pipeline_y,
            encode_pipeline_uv,
            encode_layout,
            pool: std::sync::Mutex::new(SurfacePool::default()),
            #[cfg(feature = "overlay")]
            overlay,
        })
    }

    /// The number of GPU buffer/texture allocations the surface pool has made
    /// since construction.
    ///
    /// Used by the EFF-0 allocation-count gate: once the pool is warm (first
    /// frame at a given geometry), steady-state composite ticks must NOT
    /// increase this — the pool reuses its surfaces rather than reallocating per
    /// frame. A genuine resize (a rare dimension change) is the only thing that
    /// bumps it thereafter. Returns `0` if the pool lock is momentarily
    /// poisoned (never on the hot path).
    #[must_use]
    pub fn gpu_allocation_count(&self) -> u64 {
        self.pool.lock().map_or(0, |p| p.alloc_count())
    }

    /// Composite a back-to-front stack of [`Tile`]s onto a `canvas_w x
    /// canvas_h` NV12 output, running the full fixed-order pipeline on the GPU.
    ///
    /// Semantics match [`crate::pipeline::composite`] (the CPU oracle): tiles
    /// are scaled into their destination rect (scale-at-composite; 1:1 when the
    /// `dst` size equals the source size) in slice order, clipped to the canvas;
    /// uncovered pixels take `background` (a linear canvas-gamut color). The output
    /// carries the canvas [`CanvasColor::output_tag`].
    ///
    /// # Errors
    ///
    /// - [`Error::Geometry`] for non-even / zero canvas or tile dimensions.
    /// - [`Error::GpuLimit`] when `tiles.len()` exceeds [`MAX_TILES`].
    /// - The color `Unsupported*` / `UnresolvedColor` errors when an axis has no
    ///   shader implementation or is unresolved.
    /// - [`Error::GpuRuntime`] on a buffer-map / submission failure.
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        // The no-overlay path: composite straight into the canvas the encode
        // pass reads, with no overlay sub-pass between (byte-for-byte the prior
        // behaviour). The overlay dispatch is `composite_with_overlays`.
        //
        // Lock the surface pool for the synchronous duration of this composite:
        // its run-stable textures/buffers are reused in place (and grown only on
        // a resize). The lock is never held across an `.await` — these methods
        // are synchronous — so it cannot back-pressure the engine (invariant
        // #10).
        let mut pool = self
            .pool
            .lock()
            .map_err(|_| Error::GpuRuntime("surface pool lock poisoned".to_owned()))?;
        let device = self.ctx.device();
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        // Run the composite pass into the pooled linear canvas, then encode +
        // read back. The composite output texture is read in the same `pool`
        // borrow, so derive its view before the encode borrows the pool again.
        self.record_composite(
            &mut pool,
            &mut encoder,
            canvas_w,
            canvas_h,
            canvas,
            background,
            tiles,
        )?;
        let canvas_view = pool
            .canvas_lin
            .as_ref()
            .ok_or_else(|| Error::GpuRuntime("composite canvas missing".to_owned()))?
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.encode_and_read(
            &mut pool,
            &mut encoder,
            &canvas_view,
            canvas_w,
            canvas_h,
            canvas,
        )
    }

    /// Composite the `tiles`, blend an overlay `list` over the linear canvas via
    /// the overlay sub-pass, then encode to NV12 — the full
    /// composite → **overlay** → encode sequence (ADR-0016 §4.1, invariants
    /// #5 + #8).
    ///
    /// Image cues ([`crate::overlay::subpass::OverlayPrimitive::Image`]) are
    /// resolved against the compositor's content-keyed image-texture cache and
    /// uploaded **once** (a static caption uploaded the first tick it appears,
    /// reused thereafter), then sampled by the shader's `KIND_IMAGE` branch and
    /// blended premultiplied-over the linear canvas — matching the CPU reference
    /// [`crate::overlay::subpass::blend_overlays`] within SSIM/PSNR (never
    /// bit-exact). Analytic primitives (rect / line / stroke / ring) are blended
    /// in the same pass; glyphs are skipped here (they sample the persistent
    /// atlas the CLI bake owns — out of scope for this image-path dispatch).
    ///
    /// When `list` has no blendable primitive the overlay pass is skipped
    /// entirely and the result is byte-for-byte [`Self::composite`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::composite`], plus [`Error::GpuRuntime`] if the overlay
    /// resources lock is poisoned.
    #[cfg(feature = "overlay")]
    pub fn composite_with_overlays(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
        list: &crate::overlay::subpass::OverlayDrawList,
    ) -> Result<Nv12Image> {
        // Lock the surface pool for the synchronous duration of this composite
        // (consistent lock order: pool BEFORE overlay), then run the front half
        // into the pooled linear canvas. The lock is never held across an
        // `.await` (these methods are synchronous), so it cannot back-pressure
        // the engine (invariant #10).
        let mut pool = self
            .pool
            .lock()
            .map_err(|_| Error::GpuRuntime("surface pool lock poisoned".to_owned()))?;
        let device = self.ctx.device();
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        // Front half: composite the tiles into the pooled straight-alpha canvas.
        self.record_composite(
            &mut pool,
            &mut encoder,
            canvas_w,
            canvas_h,
            canvas,
            background,
            tiles,
        )?;
        // The view holds an internal (Arc) handle to the pooled texture, not a
        // Rust borrow of `pool`, so `pool` can be re-borrowed below.
        let composite_view = pool
            .canvas_lin
            .as_ref()
            .ok_or_else(|| Error::GpuRuntime("composite canvas missing".to_owned()))?
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Plan the overlay uploads (upload-once content-keyed cache) under the
        // overlay lock; the cache state must persist across frames.
        let mut guard = self
            .overlay
            .lock()
            .map_err(|_| Error::GpuRuntime("overlay resources lock poisoned".to_owned()))?;
        let resources = &mut *guard;
        let plan = crate::overlay::gpu_image::plan_image_uploads(list, &mut resources.image_cache);

        if !plan.dispatch() {
            // No blendable overlay primitive: encode the composite canvas
            // directly (the no-overlay path — byte-for-byte `composite`).
            drop(guard);
            return self.encode_and_read(
                &mut pool,
                &mut encoder,
                &composite_view,
                canvas_w,
                canvas_h,
                canvas,
            );
        }

        // Middle: record the overlay sub-pass, blending `plan` over the canvas
        // into the pooled overlaid canvas the encode pass then reads. Cues that
        // still `needs_upload` are written into the PERSISTENT image array, so a
        // static caption is uploaded once and reused.
        self.record_overlay(
            &mut pool,
            &mut encoder,
            resources,
            &composite_view,
            canvas_w,
            canvas_h,
            &plan,
        )?;
        drop(guard);
        let overlaid_view = pool
            .overlaid
            .as_ref()
            .ok_or_else(|| Error::GpuRuntime("overlaid canvas missing".to_owned()))?
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Back half: encode the overlaid canvas to NV12 and read it back.
        self.encode_and_read(
            &mut pool,
            &mut encoder,
            &overlaid_view,
            canvas_w,
            canvas_h,
            canvas,
        )
    }

    /// Record the overlay sub-pass into `encoder`: upload each image cue's
    /// premultiplied bytes that still `needs_upload` into its resolved layer of
    /// the **persistent** `images` texture-array (upload-once — a cue resident
    /// from a prior tick keeps its bytes), upload the packed primitive buffer,
    /// build the bind group (including binding 5, the image texture-array), and
    /// dispatch the sub-pass — blending the primitives premultiplied-over the
    /// `composite_view` linear canvas into a fresh `Rgba16Float` output the
    /// encode pass reads (a read-write `rgba16float` storage texture is not
    /// portable in WebGPU core, so input and output are distinct textures;
    /// invariants #5 + #8).
    ///
    /// # Errors
    ///
    /// [`Error::Geometry`] on a primitive-count or row-stride overflow.
    #[cfg(feature = "overlay")]
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    // reason: the overlay sub-pass is one linear resource-setup sequence (upload
    // the cues that changed -> pack the prim/uniform buffers -> build the bind
    // group with all six bindings -> record the dispatch); splitting it would
    // scatter the transient buffer/view lifetimes that must all outlive the
    // recorded pass. The args are the pool + encoder + overlay resources +
    // canvas geometry + plan; grouping them would just shift the same fields.
    // Kept as one readable function with section comments, mirroring
    // `record_composite`.
    fn record_overlay(
        &self,
        pool: &mut SurfacePool,
        encoder: &mut wgpu::CommandEncoder,
        resources: &OverlayResources,
        composite_view: &wgpu::TextureView,
        canvas_w: u32,
        canvas_h: u32,
        plan: &crate::overlay::gpu_image::ImageUploadPlan<'_>,
    ) -> Result<()> {
        use crate::overlay::gpu_subpass::{OverlayPrimGpu, OverlayUniforms};

        let device = self.ctx.device();
        let queue = self.ctx.queue();
        let subpass = &resources.subpass;
        let images = &resources.images;

        // Upload each cue that needs it into its resolved layer of the persistent
        // image array (upload-once: a cue already resident from a prior tick is
        // skipped, so a static caption is written exactly once and reused). A
        // cue larger than the bounded per-layer extent, or with a short/mismatched
        // buffer, is dropped rather than indexed/written past (hot-path rule:
        // hold last-good, never panic, never grow the allocation).
        for upload in plan.uploads() {
            if !upload.needs_upload {
                continue;
            }
            if upload.src_width == 0 || upload.src_height == 0 {
                continue;
            }
            if upload.src_width > OVERLAY_IMAGE_MAX_DIM || upload.src_height > OVERLAY_IMAGE_MAX_DIM
            {
                continue;
            }
            let expected = usize::try_from(upload.src_width)
                .ok()
                .and_then(|w| usize::try_from(upload.src_height).ok().map(|h| w * h))
                .and_then(|px| px.checked_mul(4));
            if expected != Some(upload.rgba.len()) {
                continue;
            }
            let bytes_per_row = upload
                .src_width
                .checked_mul(4)
                .ok_or_else(|| Error::Geometry("overlay image row overflow".to_owned()))?;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: images,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: upload.layer,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                upload.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(upload.src_height),
                },
                wgpu::Extent3d {
                    width: upload.src_width,
                    height: upload.src_height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Pack the primitive count + the packed primitives into their buffers.
        let prim_count = u32::try_from(plan.prims().len())
            .map_err(|_| Error::Geometry("overlay primitive count overflow".to_owned()))?;
        let ov_uniform = OverlayUniforms {
            canvas: [canvas_w, canvas_h, prim_count, 0],
        };
        // The storage binding must be non-empty even with zero packed prims
        // (e.g. only-skipped glyphs); pad with one zeroed prim the shader's
        // `count == 0` loop never reads.
        let mut prims = plan.prims().to_vec();
        if prims.is_empty() {
            prims.push(OverlayPrimGpu::zeroed());
        }

        // Destructure the pool so the overlaid canvas, overlay uniform, prim
        // storage, and atlas placeholder are each borrowed disjointly from the
        // shared counter.
        let SurfacePool {
            alloc_count,
            overlaid,
            ov_uniform: ov_uniform_slot,
            prim_buf: prim_buf_slot,
            atlas: atlas_slot,
            ..
        } = pool;

        // Refill the pooled overlay uniform in place.
        let ov_uniform_buf = SurfacePool::fixed_buffer(ov_uniform_slot, alloc_count, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview overlay uniforms"),
                size: pool::ov_uniform_size(),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        queue.write_buffer(ov_uniform_buf, 0, bytemuck::bytes_of(&ov_uniform));

        // Refill the pooled (MAX_OVERLAY_PRIMS-sized) prim storage buffer in
        // place; the shader reads only the `prim_count` entries written.
        let prim_buf = SurfacePool::fixed_buffer(prim_buf_slot, alloc_count, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview overlay prims"),
                size: pool::prim_buf_size(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        queue.write_buffer(prim_buf, 0, bytemuck::cast_slice(&prims));

        // The overlaid output canvas the encode pass reads (a distinct storage
        // texture — `rgba16float` read-write is not portable in WebGPU core),
        // pooled exactly like the composite canvas.
        let overlaid_view = SurfacePool::exact_texture(
            overlaid,
            alloc_count,
            Dim2 {
                width: canvas_w,
                height: canvas_h,
            },
            |d| linear_canvas_texture(device, d),
        )
        .create_view(&wgpu::TextureViewDescriptor::default());

        // A 1x1 placeholder glyph atlas (binding 2): glyphs are skipped by the
        // plan, so it is never sampled, but the bind group must be complete.
        // Allocated once and reused.
        let atlas = SurfacePool::fixed_texture(atlas_slot, alloc_count, || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("multiview overlay atlas placeholder"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        });

        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let images_view = images.create_view(&wgpu::TextureViewDescriptor::default());

        let overlay_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview overlay bind"),
            layout: subpass.bind_group_layout(),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ov_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: prim_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(composite_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&overlaid_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&images_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview overlay pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(subpass.pipeline());
            pass.set_bind_group(0, &overlay_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
        }

        Ok(())
    }

    /// Record the composite pass into `encoder`: upload the tiles and run the
    /// front half of the fixed pipeline into the pooled linear `Rgba16Float`
    /// canvas (straight alpha) the encode (or overlay) pass then reads. The
    /// surfaces come from `pool` — reused in place, grown only on a resize, never
    /// reallocated per tick (EFF-0).
    ///
    /// # Errors
    ///
    /// Same as [`Self::composite`] (geometry, tile-count, color errors).
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    // reason: a GPU composite pass is one linear sequence (size/reuse the pooled
    // surfaces -> upload tiles -> record the composite dispatch); splitting it
    // further would scatter the borrows of the pooled textures/buffers and
    // obscure the fixed pipeline order. The args are the pool (reused surfaces)
    // + encoder + the canvas/background/tiles request; grouping them would just
    // shift the same fields. Kept as one readable function with section comments.
    fn record_composite(
        &self,
        pool: &mut SurfacePool,
        encoder: &mut wgpu::CommandEncoder,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<()> {
        require_even_positive(canvas_w, canvas_h, "canvas")?;
        let tile_count = u32::try_from(tiles.len())
            .map_err(|_| Error::GpuLimit(format!("tile count {} overflows u32", tiles.len())))?;
        if tile_count > MAX_TILES {
            return Err(Error::GpuLimit(format!(
                "{tile_count} tiles exceeds MAX_TILES ({MAX_TILES})"
            )));
        }

        let device = self.ctx.device();
        let queue = self.ctx.queue();

        // Pack the per-tile params + compute the max tile extent BEFORE touching
        // the pool, so a color/geometry error short-circuits without growing it.
        let max_w = tiles.iter().map(|t| t.image.width()).max().unwrap_or(2);
        let max_h = tiles.iter().map(|t| t.image.height()).max().unwrap_or(2);
        let mut tile_params: Vec<TileParams> = Vec::with_capacity(tiles.len());
        for tile in tiles {
            let img = tile.image;
            require_even_positive(img.width(), img.height(), "tile")?;
            tile_params.push(TileParams::build(
                tile.dst_x,
                tile.dst_y,
                tile.dst_w,
                tile.dst_h,
                img.width(),
                img.height(),
                tile.opacity,
                img.color(),
                canvas,
            )?);
        }
        // The texture array must have at least one layer even with zero tiles;
        // pad the params buffer so the storage binding is non-empty (the shader
        // reads only the `tile_count` entries either way).
        if tile_params.is_empty() {
            tile_params.push(TileParams::zeroed());
        }

        // Destructure the pool once so each surface slot is borrowed disjointly
        // from the shared allocation counter (a `self.counter()` accessor would
        // alias the whole pool and clash with a `&mut slot` borrow).
        let SurfacePool {
            alloc_count,
            y_array,
            uv_array,
            canvas_lin,
            comp_uniform,
            tile_buf,
            ..
        } = pool;

        // --- size / reuse the pooled tile texture arrays ---------------------
        // Grow-only on the max tile extent; always MAX_TILES layers so the layer
        // count never forces a realloc. The shader samples each layer only over
        // that tile's freshly-written `src_w x src_h` region (textureLoad clamped
        // to src dims), so an oversized array is byte-identical to a tight one.
        let y_need = Dim3 {
            width: max_w,
            height: max_h,
            layers: MAX_TILES,
        };
        let uv_need = Dim3 {
            width: max_w / 2,
            height: max_h / 2,
            layers: MAX_TILES,
        };
        let y_tex = SurfacePool::grow_texture(y_array, alloc_count, y_need, |d| {
            tile_plane_texture(
                device,
                "multiview tile Y planes",
                d,
                wgpu::TextureFormat::R8Unorm,
            )
        });
        let y_array_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_tex = SurfacePool::grow_texture(uv_array, alloc_count, uv_need, |d| {
            tile_plane_texture(
                device,
                "multiview tile UV planes",
                d,
                wgpu::TextureFormat::Rg8Unorm,
            )
        });
        let uv_array_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());

        // --- upload each tile's NV12 planes into its layer -------------------
        for (layer, tile) in tiles.iter().enumerate() {
            let img = tile.image;
            let layer_u32 = u32::try_from(layer)
                .map_err(|_| Error::GpuLimit("tile layer overflows u32".to_owned()))?;
            write_tile_plane(
                queue,
                y_tex,
                layer_u32,
                img.width(),
                img.height(),
                1,
                img.y_plane(),
            );
            write_tile_plane(
                queue,
                uv_tex,
                layer_u32,
                img.width() / 2,
                img.height() / 2,
                2,
                img.uv_plane(),
            );
        }

        // --- size / reuse the pooled linear canvas ---------------------------
        let canvas_lin_view = SurfacePool::exact_texture(
            canvas_lin,
            alloc_count,
            Dim2 {
                width: canvas_w,
                height: canvas_h,
            },
            |d| linear_canvas_texture(device, d),
        )
        .create_view(&wgpu::TextureViewDescriptor::default());

        // --- refill the pooled uniform + tile-params buffers in place --------
        let comp_uniform_data = CompositeUniforms {
            canvas: [canvas_w, canvas_h, tile_count, 0],
            background: [background.r, background.g, background.b, background.a],
        };
        let comp_uniform_buf = SurfacePool::fixed_buffer(comp_uniform, alloc_count, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview composite uniforms"),
                size: pool::comp_uniform_size(),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        queue.write_buffer(comp_uniform_buf, 0, bytemuck::bytes_of(&comp_uniform_data));
        let tile_param_buf = SurfacePool::fixed_buffer(tile_buf, alloc_count, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview tile params"),
                size: pool::tile_buf_size(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        queue.write_buffer(tile_param_buf, 0, bytemuck::cast_slice(&tile_params));

        let composite_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview composite bind"),
            layout: &self.composite_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: comp_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: tile_param_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&y_array_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&uv_array_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&canvas_lin_view),
                },
            ],
        });

        // --- record the composite dispatch -----------------------------------
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview composite pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, &composite_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
        }

        Ok(())
    }

    /// Record the encode pass against `canvas_view` (the straight-alpha linear
    /// canvas the encode shader reads — the composite output, or the overlaid
    /// output of [`Self::record_overlay`]), submit the `encoder`, and read back
    /// the NV12 planes (invariant #5: stays NV12).
    ///
    /// # Errors
    ///
    /// - The color `Unsupported*` / `UnresolvedColor` errors when an axis has no
    ///   shader implementation or is unresolved.
    /// - [`Error::GpuRuntime`] on a buffer-map / submission failure.
    #[allow(clippy::too_many_lines)]
    // reason: the encode pass is one linear sequence (size/reuse the pooled NV12
    // output planes + encode uniform -> build the bind group -> record the two
    // dispatches -> stage + map the pooled readback buffers -> assemble the
    // Nv12Image); splitting it would scatter the pooled-resource borrows that
    // must outlive the recorded pass. Kept as one readable function with section
    // comments, mirroring `record_composite`.
    fn encode_and_read(
        &self,
        pool: &mut SurfacePool,
        encoder: &mut wgpu::CommandEncoder,
        canvas_view: &wgpu::TextureView,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
    ) -> Result<Nv12Image> {
        let device = self.ctx.device();
        let queue = self.ctx.queue();

        // Build the encode uniform first so a color error short-circuits before
        // any pool growth.
        let enc_uniform = EncodeUniforms::build(canvas_w, canvas_h, canvas)?;

        // Destructure the pool so the NV12 output planes, readback buffers, and
        // encode uniform are each borrowed disjointly from the shared counter.
        let SurfacePool {
            alloc_count,
            y_out,
            uv_out,
            y_readback,
            uv_readback,
            enc_uniform: enc_uniform_slot,
            ..
        } = pool;

        // --- size / reuse the pooled NV12 output planes ----------------------
        let y_out_tex = SurfacePool::exact_texture(
            y_out,
            alloc_count,
            Dim2 {
                width: canvas_w,
                height: canvas_h,
            },
            |d| {
                nv12_out_texture(
                    device,
                    "multiview NV12 Y out",
                    d,
                    wgpu::TextureFormat::R8Unorm,
                )
            },
        );
        let y_out_view = y_out_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_out_tex = SurfacePool::exact_texture(
            uv_out,
            alloc_count,
            Dim2 {
                width: canvas_w / 2,
                height: canvas_h / 2,
            },
            |d| {
                nv12_out_texture(
                    device,
                    "multiview NV12 UV out",
                    d,
                    wgpu::TextureFormat::Rg8Unorm,
                )
            },
        );
        let uv_out_view = uv_out_tex.create_view(&wgpu::TextureViewDescriptor::default());

        // --- refill the pooled encode uniform in place -----------------------
        let enc_uniform_buf = SurfacePool::fixed_buffer(enc_uniform_slot, alloc_count, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview encode uniforms"),
                size: pool::enc_uniform_size(),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        queue.write_buffer(enc_uniform_buf, 0, bytemuck::bytes_of(&enc_uniform));

        let encode_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview encode bind"),
            layout: &self.encode_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: enc_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(canvas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&y_out_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&uv_out_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview encode pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.encode_pipeline_y);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
            pass.set_pipeline(&self.encode_pipeline_uv);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w / 2, 8), div_ceil(canvas_h / 2, 8), 1);
        }

        // Stage each plane's copy into its pooled, padded readback buffer.
        let y_padded = self.stage_readback(
            y_readback,
            alloc_count,
            encoder,
            y_out_tex,
            canvas_w,
            canvas_h,
            1,
        )?;
        let uv_padded = self.stage_readback(
            uv_readback,
            alloc_count,
            encoder,
            uv_out_tex,
            canvas_w / 2,
            canvas_h / 2,
            2,
        )?;

        // The encoder is consumed by the submit; create a fresh local move so the
        // caller's `&mut` borrow ends cleanly (we own the submission here).
        let finished = std::mem::replace(
            encoder,
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None }),
        )
        .finish();
        queue.submit(Some(finished));

        let y_buf = pooled_buffer(y_readback.as_ref(), "Y readback")?;
        let y_bytes = self.map_read(y_buf, y_padded, canvas_w, canvas_h, 1)?;
        let uv_buf = pooled_buffer(uv_readback.as_ref(), "UV readback")?;
        let uv_bytes = self.map_read(uv_buf, uv_padded, canvas_w / 2, canvas_h / 2, 2)?;

        Nv12Image::new(canvas_w, canvas_h, y_bytes, uv_bytes, canvas.output_tag())
    }

    /// Reuse-or-resize the pooled readback `slot` for a `width x height` plane,
    /// then stage a copy of `texture` into it (rows padded to
    /// `COPY_BYTES_PER_ROW_ALIGNMENT`). Returns the padded bytes-per-row so the
    /// later map can strip the padding.
    #[allow(clippy::too_many_arguments)]
    // reason: a readback stage needs the pool slot + counter (disjoint borrows),
    // the encoder, the source texture, and the plane geometry; grouping them
    // would just shift the same fields and obscure the call site.
    fn stage_readback(
        &self,
        slot: &mut Option<CachedBuffer>,
        counter: &std::sync::atomic::AtomicU64,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        bytes_per_px: u32,
    ) -> Result<u32> {
        let unpadded = width
            .checked_mul(bytes_per_px)
            .ok_or_else(|| Error::Geometry("readback row overflow".to_owned()))?;
        let padded = align_up(unpadded, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let size = u64::from(padded)
            .checked_mul(u64::from(height))
            .ok_or_else(|| Error::Geometry("readback size overflow".to_owned()))?;
        let device = self.ctx.device();
        let buffer = SurfacePool::exact_buffer(slot, counter, size, |sz| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("multiview readback"),
                size: sz,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(padded)
    }

    /// Map a pooled readback buffer and copy out the unpadded plane bytes.
    fn map_read(
        &self,
        buffer: &wgpu::Buffer,
        padded_bytes_per_row: u32,
        width: u32,
        height: u32,
        bytes_per_px: u32,
    ) -> Result<Vec<u8>> {
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            // The receiver may be gone if the device errored; ignore send error.
            let _ = tx.send(res);
        });
        // poll until the map callback fires (headless, no surface to present).
        self.ctx
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| Error::GpuRuntime(format!("device poll failed: {e}")))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Error::GpuRuntime(format!("buffer map failed: {e}"))),
            Err(e) => return Err(Error::GpuRuntime(format!("map channel closed: {e}"))),
        }

        let row_len = usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(usize::try_from(bytes_per_px).ok()?))
            .ok_or_else(|| Error::Geometry("plane row overflow".to_owned()))?;
        let rows = usize::try_from(height)
            .map_err(|_| Error::Geometry("plane height overflow".to_owned()))?;
        let padded = usize::try_from(padded_bytes_per_row)
            .map_err(|_| Error::Geometry("padded stride overflow".to_owned()))?;

        // wgpu 30 makes `get_mapped_range` fallible (`Result<BufferView,
        // MapRangeError>`). Propagate a failed map as a typed readback error rather
        // than unwrapping — safety rule 3: no panics on the compositor readback
        // path; hold the typed error, never crash the output.
        let data = slice
            .get_mapped_range()
            .map_err(|e| Error::GpuRuntime(format!("buffer get_mapped_range failed: {e}")))?;
        let mut out = Vec::with_capacity(row_len * rows);
        for row in 0..rows {
            let start = row
                .checked_mul(padded)
                .ok_or_else(|| Error::Geometry("row offset overflow".to_owned()))?;
            let end = start
                .checked_add(row_len)
                .ok_or_else(|| Error::Geometry("row end overflow".to_owned()))?;
            let chunk = data
                .get(start..end)
                .ok_or_else(|| Error::GpuRuntime("readback short row".to_owned()))?;
            out.extend_from_slice(chunk);
        }
        drop(data);
        // Unmap so the pooled buffer can be reused (re-copied + re-mapped) next
        // tick — the whole point of pooling the readback buffer.
        buffer.unmap();
        Ok(out)
    }
}

impl Compositor for GpuCompositor {
    fn kind(&self) -> BackendKind {
        BackendKind::Wgpu
    }
}

/// Upload one NV12 plane into a layer of a texture array.
fn write_tile_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    layer: u32,
    width: u32,
    height: u32,
    bytes_per_px: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * bytes_per_px),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Build a pooled tile-plane texture-array (`R8Unorm` Y or `Rg8Unorm` UV) at
/// `dim` — `TEXTURE_BINDING | COPY_DST`, [`pool::SurfacePool`]-owned.
fn tile_plane_texture(
    device: &wgpu::Device,
    label: &str,
    dim: Dim3,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: dim.width,
            height: dim.height,
            depth_or_array_layers: dim.layers.max(1),
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Build the pooled linear `Rgba16Float` composite/overlaid canvas at `dim` —
/// `STORAGE_BINDING | TEXTURE_BINDING`.
fn linear_canvas_texture(device: &wgpu::Device, dim: Dim2) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("multiview linear canvas"),
        size: wgpu::Extent3d {
            width: dim.width,
            height: dim.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// Build a pooled NV12 output plane (`R8Unorm` Y or `Rg8Unorm` UV) at `dim` —
/// `STORAGE_BINDING | COPY_SRC` (written by the encode shader, copied to the
/// readback buffer).
fn nv12_out_texture(
    device: &wgpu::Device,
    label: &str,
    dim: Dim2,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: dim.width,
            height: dim.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Borrow a pooled readback buffer the surrounding logic just ensured is
/// resident (typed error rather than panic on the impossible `None`).
fn pooled_buffer<'a>(slot: Option<&'a CachedBuffer>, what: &str) -> Result<&'a wgpu::Buffer> {
    slot.map(|c| &c.buffer)
        .ok_or_else(|| Error::GpuRuntime(format!("pooled {what} missing")))
}

/// Validate even, positive dimensions (NV12 4:2:0 requires even).
fn require_even_positive(w: u32, h: u32, what: &str) -> Result<()> {
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(Error::Geometry(format!(
            "{what} dimensions must be positive and even (got {w}x{h})"
        )));
    }
    Ok(())
}

/// Ceil-divide `n` by `d` (`d > 0`), saturating (never panics / overflows for
/// the small workgroup divisors used here).
fn div_ceil(n: u32, d: u32) -> u32 {
    n.div_ceil(d.max(1))
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: u32, align: u32) -> u32 {
    let a = align.max(1);
    value.div_ceil(a).saturating_mul(a)
}

/// `min_binding_size` for a `#[repr(C)]` uniform/storage block `T`.
///
/// `size_of::<T>()` is a small `usize` that fits a `u64` on every supported
/// target, so the `try_from` never fails; `NonZeroU64::new` then yields `None`
/// only for a zero-sized type (none of our blocks are). Using `try_from` keeps
/// the conversion within the `as_conversions` ban.
fn min_binding_size<T>() -> Option<NonZeroU64> {
    u64::try_from(core::mem::size_of::<T>())
        .ok()
        .and_then(NonZeroU64::new)
}

/// The composite-pass bind-group layout (mirrors `composite.wgsl` bindings).
fn composite_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("multiview composite bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<CompositeUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<TileParams>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
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
        ],
    })
}

/// The encode-pass bind-group layout (mirrors `encode.wgsl` bindings).
fn encode_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("multiview encode bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<EncodeUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::R8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rg8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    })
}
