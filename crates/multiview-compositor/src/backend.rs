//! The compositor **backend-selection seam** (invariant #1: degradation-safe).
//!
//! The run path composites once per output tick. By default it uses the
//! pure-Rust CPU reference ([`crate::pipeline::composite`]); under the off-by-default
//! `wgpu` feature it may *prefer* the portable GPU compositor (`gpu::GpuCompositor`,
//! a plain code span: the type is absent from the default no-`wgpu` doc build).
//! This module is the single seam where that choice is made.
//!
//! **The selection is degradation-safe.** A missing or failed GPU must never
//! stall or crash the run (invariant #1, safety rule §3): [`RunBackend::select`]
//! tries the GPU only when a target is *supplied* AND the `wgpu` feature is
//! compiled, and on **any** GPU initialization error (no adapter, the chosen
//! adapter not found, device request failure, shader build failure) it **falls
//! back to the CPU reference** rather than propagating the error or panicking.
//! The CPU path is the default for both `select(None)` and a build without the
//! feature. A `Some(GpuTarget)` carries the load-aware admission decision — the
//! specific least-contended GPU the placement engine chose (ADR-0035 Tier-1), or
//! `GpuTarget::none()` for "prefer the GPU at the default adapter".
//!
//! Once constructed, [`RunBackend::composite`] dispatches to the chosen
//! path with the exact signature/semantics of [`crate::pipeline::composite`]: tiles
//! placed 1:1 in slice order, clipped to the canvas; uncovered pixels take
//! `background`; the output carries the canvas tag. The CPU variant is a thin,
//! byte-for-byte dispatch to the free function (no re-encode, no reorder).
//!
//! Neither variant blocks on an input or a client. On the output clock
//! ([`RunBackend::composite_job`]) a GPU backend composites on its own
//! prewarmed worker thread and the clock waits for it at most a bounded time
//! ([`crate::offclock`], issue #415). The readback inside the job is bounded
//! too ([`crate::readback`], issue #395). A frame that is not ready in time
//! returns [`crate::error::Error::GpuDeadline`] (the engine repeats last-good
//! under the tick, counted). A GPU stalled past
//! [`crate::readback::STALL_HOLD`], or one that hit a genuine runtime failure,
//! gets its frames composited on the CPU reference, also off the clock. A lost
//! device (callback, uncaptured error, or a wgpu panic) goes to the CPU for
//! good. A hung or lost GPU therefore never stalls, ends or panics the output
//! clock, and the GPU compositor is never torn down on the clock thread.

use crate::blend::LinearRgba;
use crate::error::Result;
use crate::pipeline::{
    auto_thread_count, composite_core, CanvasColor, LutCache, Nv12Image, ScratchPool, Tile,
};

/// A **wgpu-free** description of the one GPU a load-aware admission decision
/// pinned the whole pipeline island to (ADR-0035 Tier-1 / ADR-0018).
///
/// The placement engine ([`multiview_hal::select_device`]) chooses a single
/// device; the cli turns that device's identity into a `GpuTarget` and threads
/// it — unchanged — into every hardware site (wgpu compositor here; NVDEC/NVENC
/// when those hardware paths are wired). One `GpuTarget` per island is what makes
/// affinity structural: there is exactly one value, so decode→composite→encode
/// cannot land on different GPUs (the GPU-placement principle: *load informs
/// placement, never fragments a pipeline*).
///
/// Matching against a concrete adapter is done by [`GpuTarget::matches`] against
/// the (wgpu-free) [`AdapterMatchInfo`], so the match logic is unit-testable with
/// no GPU and no `wgpu` dependency. The discriminators are tried most-robust
/// first: the **PCI bus id** (the key NVML and wgpu both expose), then the
/// `(vendor_id, device_id)` PCI pair, then the adapter name. A `GpuTarget` with
/// every field absent matches **nothing** (the caller keeps its default
/// `HighPerformance` path instead).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuTarget {
    /// The PCI bus id of the chosen device (NVML `PciInfo.bus_id`), e.g.
    /// `00000000:01:00.0`. The most robust discriminator: matched against a
    /// concrete adapter's `device_pci_bus_id` after both are normalised (the
    /// NVML 8-hex-digit domain vs the Vulkan 4-hex-digit domain are reconciled by
    /// [`normalize_pci_bus_id`]).
    pub pci_bus_id: Option<String>,
    /// The PCI vendor id of the chosen device (e.g. `0x10de` for NVIDIA), if
    /// known — the fallback `(vendor, device)` pair when no PCI bus id matched.
    pub vendor_id: Option<u32>,
    /// The PCI device id of the chosen device, if known.
    pub device_id: Option<u32>,
    /// The human-readable adapter name of the chosen device (e.g.
    /// `NVIDIA GeForce RTX 4060`), the last-resort discriminator.
    pub name: Option<String>,
}

/// The **wgpu-free** subset of a concrete adapter's identity a [`GpuTarget`] is
/// matched against.
///
/// The `wgpu`-gated adapter-pick site (`gpu::device`) maps each
/// `wgpu::AdapterInfo` into this plain struct so the match decision lives in pure
/// code: testable with synthetic adapters on a GPU-free machine, exactly the
/// discipline the rest of the placement seam holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterMatchInfo {
    /// The adapter's PCI bus id (`wgpu::AdapterInfo::device_pci_bus_id`), e.g.
    /// `0000:01:00.0`; empty when the backend does not expose one.
    pub pci_bus_id: String,
    /// The adapter's `(Backend-specific)` PCI vendor id.
    pub vendor_id: u32,
    /// The adapter's `(Backend-specific)` PCI device id.
    pub device_id: u32,
    /// The adapter's human-readable name.
    pub name: String,
}

impl GpuTarget {
    /// Construct an empty target (matches nothing — the caller keeps its default
    /// adapter path).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether *any* discriminator is present (an empty target is inert).
    #[must_use]
    pub fn is_some(&self) -> bool {
        self.pci_bus_id.is_some()
            || (self.vendor_id.is_some() && self.device_id.is_some())
            || self.name.is_some()
    }

    /// Whether `adapter` is the device this target names.
    ///
    /// Tries the discriminators most-robust first: PCI bus id (normalised on
    /// both sides), then the `(vendor, device)` PCI pair, then an exact name
    /// match. A target with no usable discriminator never matches (returns
    /// `false`), so a caller folds back to its default adapter selection.
    #[must_use]
    pub fn matches(&self, adapter: &AdapterMatchInfo) -> bool {
        // 1. PCI bus id — the key NVML (`PciInfo.bus_id`) and wgpu
        //    (`device_pci_bus_id`) both expose. Normalise both: NVML reports an
        //    8-hex-digit domain (`00000000:01:00.0`) while Vulkan reports a
        //    4-hex-digit domain (`0000:01:00.0`); reducing the domain to its
        //    trailing 4 hex digits + lowercasing reconciles them.
        if let Some(want) = self.pci_bus_id.as_deref() {
            if !adapter.pci_bus_id.is_empty()
                && normalize_pci_bus_id(want) == normalize_pci_bus_id(&adapter.pci_bus_id)
            {
                return true;
            }
        }
        // 2. The PCI (vendor, device) pair — robust when bus ids are absent or
        //    formatted differently across backends, but coarser (it cannot tell
        //    two identical cards apart, so it is below the bus id).
        if let (Some(vendor), Some(device)) = (self.vendor_id, self.device_id) {
            if vendor == adapter.vendor_id && device == adapter.device_id {
                return true;
            }
        }
        // 3. The adapter name — last resort (identical SKUs share a name, so it
        //    is the weakest, but still better than blindly grabbing GPU0).
        if let Some(name) = self.name.as_deref() {
            if !name.is_empty() && name == adapter.name {
                return true;
            }
        }
        false
    }
}

/// Normalise a PCI bus id for cross-source comparison.
///
/// NVML reports `00000000:01:00.0` (8-hex-digit domain); wgpu/Vulkan reports
/// `0000:01:00.0` (4-hex-digit domain). Lower-casing and reducing the domain
/// segment to its trailing 4 hex digits maps both onto the same canonical
/// `0000:01:00.0` form so a string compare is sound. A string with no `:` is
/// returned lower-cased unchanged (it still compares equal to itself).
#[must_use]
pub fn normalize_pci_bus_id(bus_id: &str) -> String {
    let lower = bus_id.trim().to_ascii_lowercase();
    match lower.split_once(':') {
        Some((domain, rest)) => {
            // Keep the trailing 4 chars of the domain (Vulkan width); pad on the
            // left with `0` if the source domain was shorter than 4.
            let tail: String = domain
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let padded = format!("{tail:0>4}");
            format!("{padded}:{rest}")
        }
        None => lower,
    }
}

/// Which compositor backend a [`RunBackend`] resolved to.
///
/// `Cpu` is always available (the pure-Rust reference). `Gpu` exists only when
/// the crate is built with the `wgpu` feature *and* an adapter initialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunBackendKind {
    /// The pure-Rust CPU reference compositor ([`crate::pipeline::composite`]).
    Cpu,
    /// The portable wgpu GPU compositor ([`crate::gpu::GpuCompositor`]).
    #[cfg(feature = "wgpu")]
    Gpu,
    /// The simulated GPU chaos-test seam ([`RunBackend::simulated_gpu`]).
    #[doc(hidden)]
    SimulatedGpu,
}

/// The persistent CPU-reference backend: owns the transfer-LUT cache and the
/// reusable per-band composite scratch for the whole run (efficiency findings #5
/// and #6). Constructed by [`RunBackend::cpu`]; callers normally interact through
/// [`RunBackend::composite`].
///
/// The scratch is behind a [`std::sync::Mutex`] because `composite` takes `&self`
/// (the same API as the GPU backend), while the per-run caches are mutated in
/// place. The output clock has one compositor thread, so this is uncontended in
/// production; the lock is held only for the synchronous composite and never
/// across an `.await`. A poisoned lock is recovered rather than crashing the
/// protected output clock — the next composite advances every coverage sentinel
/// before reading pooled scratch, so stale partial state is not observable.
#[derive(Debug, Default)]
pub struct CpuBackend {
    scratch: std::sync::Mutex<CpuScratch>,
}

/// The two reusable CPU resources owned together so `CpuBackend::composite` can
/// split-borrow them: the memoized transfer LUTs and the per-band scratch pool.
#[derive(Debug, Default)]
struct CpuScratch {
    luts: LutCache,
    pool: ScratchPool,
}

impl CpuBackend {
    /// Lock the per-run scratch, recovering the inner value after a poison rather
    /// than propagating a panic/failure into the output clock (safety rule §3).
    fn lock_scratch(&self) -> std::sync::MutexGuard<'_, CpuScratch> {
        match self.scratch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Composite one CPU frame with memoized transfer LUTs + reused band scratch,
    /// fanning across [`auto_thread_count`] worker threads (the production path).
    fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        self.composite_with_thread_count(
            canvas_w,
            canvas_h,
            canvas,
            background,
            tiles,
            auto_thread_count(),
        )
    }

    /// As [`CpuBackend::composite`] but with an explicit worker-thread count — the
    /// deterministic seam that lets a test force the multi-band parallel branch
    /// (or the serial branch) regardless of host CPU count. The output is
    /// byte-identical for any `n_threads` (the band split only partitions work;
    /// [`crate::pipeline::composite_with_threads`] documents the invariant), so
    /// this changes only *which* code path runs, never the pixels.
    fn composite_with_thread_count(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
        n_threads: usize,
    ) -> Result<Nv12Image> {
        let mut scratch = self.lock_scratch();
        let CpuScratch { luts, pool } = &mut *scratch;
        let luts = luts.ensure(canvas, tiles);
        composite_core(
            canvas_w,
            canvas_h,
            canvas,
            background,
            tiles,
            Some(luts),
            pool,
            n_threads,
        )
    }

    /// [`CpuBackend::composite`] for the off-clock CPU fallback worker.
    pub(crate) fn composite_persistent(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        self.composite(canvas_w, canvas_h, canvas, background, tiles)
    }

    /// Number of transfer-LUT set builds since this backend was constructed.
    fn lut_build_count(&self) -> u64 {
        self.lock_scratch().luts.build_count()
    }

    /// Number of per-band scratch allocations since this backend was constructed.
    fn scratch_alloc_count(&self) -> u64 {
        self.lock_scratch().pool.alloc_count()
    }

    /// Number of per-band scratch slots the reused pool holds (grow-only): `1`
    /// after a serial composite, the clamped worker count after a parallel one.
    fn band_count(&self) -> usize {
        self.lock_scratch().pool.band_count()
    }
}

/// A constructed compositor backend: the CPU reference, or (under `wgpu` with a
/// live adapter) the GPU compositor.
///
/// Build one with [`RunBackend::cpu`] (always CPU) or
/// [`RunBackend::select`] (GPU-preferred with CPU fallback). Dispatch a
/// per-tick composite through [`RunBackend::composite`].
#[derive(Debug)]
#[non_exhaustive]
pub enum RunBackend {
    /// The pure-Rust CPU reference compositor. Always available; owns its
    /// run-persistent transfer-LUT cache + reusable band scratch.
    Cpu(CpuBackend),
    /// The wgpu GPU compositor, driven off the clock thread by its own
    /// prewarmed worker with a CPU fallback worker ([`crate::offclock`]).
    #[cfg(feature = "wgpu")]
    Gpu(Box<crate::offclock::OffClockGpu<crate::gpu::GpuCompositor>>),
    /// **Test seam only** (never constructed in production): CPU-reference
    /// pixels routed through the GPU readback deadline/stall state machine with
    /// a fake device ([`RunBackend::simulated_gpu`]), driven off the clock
    /// thread exactly like the wgpu variant.
    #[doc(hidden)]
    SimulatedGpu(Box<crate::offclock::OffClockGpu<crate::readback::SimulatedGpu>>),
}

impl RunBackend {
    /// The pure-Rust CPU reference backend. Always available, never fails.
    #[must_use]
    pub fn cpu() -> Self {
        Self::Cpu(CpuBackend::default())
    }

    /// The GPU-free chaos seam: a backend that composites on the CPU reference
    /// but routes every frame through the SAME readback deadline/stall state
    /// machine as the GPU compositor, over a fake device whose readback
    /// completes `latency` after submit (or never while hung). The returned
    /// [`crate::readback::SimulatedGpuControl`] changes latency, hangs and
    /// releases the device. Production never constructs this. It is
    /// admitted against its own unbounded pool (tests build many).
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::GpuRuntime`] when a worker thread cannot be
    /// spawned.
    #[doc(hidden)]
    pub fn simulated_gpu(
        latency: std::time::Duration,
    ) -> Result<(Self, crate::readback::SimulatedGpuControl)> {
        static SIM_POOL: std::sync::OnceLock<std::sync::Arc<crate::offclock::Admission>> =
            std::sync::OnceLock::new();
        let pool =
            SIM_POOL.get_or_init(|| std::sync::Arc::new(crate::offclock::Admission::new(u64::MAX)));
        let sim = crate::readback::SimulatedGpu::new(latency);
        let control = crate::readback::SimulatedGpuControl::new(&sim);
        let backend = crate::offclock::OffClockGpu::new_in(pool, sim)?;
        Ok((Self::SimulatedGpu(Box::new(backend)), control))
    }

    /// Bound the GPU readback wait by the output frame `period`:
    /// `min(10 ms, period / 2)` ([`crate::readback::readback_deadline`]). A
    /// no-op on the CPU backend. The engine calls it whenever the canvas
    /// cadence is set or changes.
    pub fn set_frame_period(&self, period: std::time::Duration) {
        match self {
            Self::Cpu(_) => {}
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.set_frame_period(period),
            Self::SimulatedGpu(sim) => sim.set_frame_period(period),
        }
    }

    /// Frames whose GPU readback missed its deadline or failed fast while an
    /// earlier readback was outstanding ([`crate::error::Error::GpuDeadline`]).
    /// `0` for the CPU backend.
    #[must_use]
    pub fn gpu_deadline_misses(&self) -> u64 {
        match self {
            Self::Cpu(_) => 0,
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.deadline_misses(),
            Self::SimulatedGpu(sim) => sim.deadline_misses(),
        }
    }

    /// Whether the GPU device was lost (issue #415): every frame since is
    /// composited on the CPU reference. `false` for the CPU backend.
    #[must_use]
    pub fn gpu_lost(&self) -> bool {
        match self {
            Self::Cpu(_) => false,
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.health().is_lost(),
            Self::SimulatedGpu(sim) => sim.health().is_lost(),
        }
    }

    /// **Test seam**: lose the GPU device (wgpu `Device::destroy`). A no-op
    /// on the CPU and simulated backends.
    #[doc(hidden)]
    pub fn destroy_gpu_device_for_test(&self) {
        #[cfg(feature = "wgpu")]
        if let Self::Gpu(gpu) = self {
            gpu.gpu().destroy_device_for_test();
        }
    }

    /// The output-clock composite (issue #415): the same semantics as
    /// [`RunBackend::composite`], but a GPU backend composites the frame on
    /// its own worker thread and the clock waits for it at most a bounded
    /// time ([`crate::offclock::frame_budget`]). A frame not ready in time is
    /// [`crate::error::Error::GpuDeadline`] (the engine repeats last-good);
    /// a lost device composites on the CPU fallback worker. The CPU backend
    /// composites inline, exactly as [`RunBackend::composite`].
    ///
    /// # Errors
    ///
    /// As [`RunBackend::composite`].
    pub fn composite_job(
        &self,
        job: &std::sync::Arc<crate::offclock::FrameJob>,
    ) -> Result<Nv12Image> {
        match self {
            Self::Cpu(cpu) => {
                let tiles = job.borrowed_tiles();
                cpu.composite(
                    job.canvas_w,
                    job.canvas_h,
                    job.canvas,
                    job.background,
                    &tiles,
                )
            }
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.composite_job(job),
            Self::SimulatedGpu(sim) => sim.composite_job(job),
        }
    }

    /// Select a backend, GPU-preferred-with-CPU-fallback, pinned to `target`.
    ///
    /// `target` is the load-aware admission decision (ADR-0035 Tier-1): `Some(t)`
    /// when the run wants the GPU compositor on a **specific** device (the one
    /// [`multiview_hal::select_device`] chose as least-contended); `None` keeps
    /// the legacy behaviour — the CPU reference, *unless* the caller specifically
    /// wants the GPU at the default `HighPerformance` adapter, for which it passes
    /// `Some(GpuTarget::none())` (an inert target that pins nothing, so the
    /// adapter pick keeps its `HighPerformance` path).
    ///
    /// When `target.is_some()` **and** the `wgpu` feature is compiled, this
    /// acquires the GPU compositor on the matching adapter; on success it returns
    /// the GPU backend, and on **any** initialization error (no adapter, the
    /// chosen adapter not found, device-request failure, shader build failure) it
    /// logs the reason at `info` and **falls back to the CPU reference** — the
    /// degradation-safe path (invariant #1: a missing/failed GPU never stalls or
    /// crashes the run). A `target` of `None`, or a build without the feature,
    /// returns the CPU backend directly.
    ///
    /// This function never panics and never returns an error: the result is
    /// always a usable backend.
    #[must_use]
    pub fn select(target: Option<GpuTarget>) -> Self {
        if let Some(target) = target {
            #[cfg(feature = "wgpu")]
            {
                // Admission before the device exists (issue #415): a process
                // that already pins the cap of live GPU workers (some may be
                // wedged in a hung driver) builds no further device.
                let admission = crate::offclock::gpu_admission();
                let gpu = match admission.try_acquire() {
                    None => Err(crate::error::Error::GpuRuntime(format!(
                        "GPU worker admission cap reached ({} live)",
                        admission.live()
                    ))),
                    Some(permit) => {
                        crate::gpu::GpuCompositor::new(target.is_some().then_some(&target))
                            .and_then(|gpu| {
                                crate::offclock::OffClockGpu::with_permit(
                                    permit,
                                    std::sync::Arc::new(gpu),
                                )
                            })
                    }
                };
                match gpu {
                    Ok(gpu) => {
                        tracing::info!("compositor backend: GPU (wgpu) selected");
                        return Self::Gpu(Box::new(gpu));
                    }
                    Err(e) => {
                        tracing::info!(
                            error = %e,
                            "GPU compositor unavailable; falling back to CPU reference"
                        );
                    }
                }
            }
            #[cfg(not(feature = "wgpu"))]
            {
                let _ = target;
                tracing::debug!(
                    "GPU compositor requested but the `wgpu` feature is not compiled; using CPU"
                );
            }
        }
        Self::cpu()
    }

    /// The backend this resolved to (for telemetry / introspection).
    #[must_use]
    pub const fn kind(&self) -> RunBackendKind {
        match self {
            Self::Cpu(_) => RunBackendKind::Cpu,
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => RunBackendKind::Gpu,
            Self::SimulatedGpu(_) => RunBackendKind::SimulatedGpu,
        }
    }

    /// `true` if this backend runs the composite on the GPU.
    #[must_use]
    pub const fn is_gpu(&self) -> bool {
        match self {
            Self::Cpu(_) => false,
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => true,
            Self::SimulatedGpu(_) => false,
        }
    }

    /// The number of transfer-LUT set builds the persistent CPU backend has
    /// performed since construction. A stable transfer set builds once across
    /// any number of ticks (efficiency finding #5). Returns `0` for the GPU
    /// backend, whose shader transfer functions do not use CPU LUTs.
    ///
    /// This counter is an observability/test seam, not part of the composite
    /// algorithm; reading it does not mutate or rebuild the cache.
    #[doc(hidden)]
    #[must_use]
    pub fn lut_build_count(&self) -> u64 {
        match self {
            Self::Cpu(cpu) => cpu.lut_build_count(),
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => 0,
            Self::SimulatedGpu(_) => 0,
        }
    }

    /// The number of reusable CPU band-scratch allocations since construction.
    /// The count may grow during warmup/resizes but stays constant across steady-
    /// state ticks (efficiency finding #6). Returns `0` for the GPU backend,
    /// which exposes its own surface-pool counter.
    #[doc(hidden)]
    #[must_use]
    pub fn scratch_alloc_count(&self) -> u64 {
        match self {
            Self::Cpu(cpu) => cpu.scratch_alloc_count(),
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => 0,
            Self::SimulatedGpu(_) => 0,
        }
    }

    /// The number of per-band scratch slots the CPU backend's reused pool holds
    /// (grow-only high-water): `1` after a serial composite, the clamped worker
    /// count after a parallel one. `>= 2` proves a composite actually fanned
    /// across the multi-band parallel path rather than falling back to serial —
    /// the deterministic signal a test needs when it forces the worker count via
    /// [`RunBackend::composite_with_thread_count`]. Returns `0` for the GPU
    /// backend (it has no CPU band scratch).
    ///
    /// An observability/test seam, not part of the composite algorithm; reading it
    /// does not mutate the pool.
    #[doc(hidden)]
    #[must_use]
    pub fn band_count(&self) -> usize {
        match self {
            Self::Cpu(cpu) => cpu.band_count(),
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => 0,
            Self::SimulatedGpu(_) => 0,
        }
    }

    /// Composite one back-to-front stack of [`Tile`]s onto a `canvas_w x
    /// canvas_h` NV12 output, dispatching to the chosen backend.
    ///
    /// Semantics are identical to [`crate::pipeline::composite`] on both paths: tiles
    /// placed 1:1 in slice order, clipped to the canvas; uncovered pixels take
    /// `background` (a linear canvas-gamut color); the output carries the canvas
    /// tag. The CPU variant is byte-for-byte the free function; the GPU variant
    /// matches it within an SSIM/PSNR threshold (GPU is never bit-exact).
    ///
    /// This call is synchronous and **never blocks on an input or a client**.
    /// On the GPU path the readback wait is bounded by the per-frame deadline
    /// ([`RunBackend::set_frame_period`]): a miss returns
    /// [`crate::error::Error::GpuDeadline`] (not re-composited on the CPU — the
    /// budget is spent; the engine repeats last-good under the tick), and a
    /// genuine GPU failure re-composites the frame on the CPU reference; a
    /// panic out of a GPU call is caught and marks the device lost (CPU for
    /// good). This synchronous path runs the GPU on the caller's thread: the
    /// output clock uses [`RunBackend::composite_job`] instead (issue #415).
    ///
    /// # Errors
    ///
    /// Propagates the compositor [`crate::error::Error`] for a structurally
    /// invalid request (odd/zero canvas, unresolved/unsupported color axis) or,
    /// on a GPU path, [`crate::error::Error::GpuDeadline`].
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        match self {
            Self::Cpu(cpu) => cpu.composite(canvas_w, canvas_h, canvas, background, tiles),
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.composite_inline(canvas_w, canvas_h, canvas, background, tiles),
            Self::SimulatedGpu(sim) => {
                sim.composite_inline(canvas_w, canvas_h, canvas, background, tiles)
            }
        }
    }

    /// Frames the GPU backend re-composited on the CPU after a genuine GPU
    /// failure since construction ([`RunBackend::composite`]). `0` for the CPU
    /// backend. Alpha tiles never count: the GPU composites them itself.
    #[must_use]
    pub fn gpu_cpu_fallback_frames(&self) -> u64 {
        match self {
            Self::Cpu(_) => 0,
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.cpu_fallback_frames(),
            Self::SimulatedGpu(sim) => sim.cpu_fallback_frames(),
        }
    }

    /// As [`RunBackend::composite`], but with an explicit CPU worker-thread count
    /// — the deterministic seam a test uses to force the multi-band parallel
    /// branch (or the serial branch) regardless of the host CPU count, then assert
    /// which ran via [`RunBackend::band_count`].
    ///
    /// The output is **byte-identical** for any `n_threads` on the CPU path (the
    /// band split only partitions the same deterministic per-pixel pipeline; see
    /// [`crate::pipeline::composite_with_threads`]), so this changes only which
    /// code path executes, never the pixels — production still sizes the fan-out
    /// from `auto_thread_count()` via [`RunBackend::composite`]. `n_threads` is
    /// ignored by the GPU backend, which does not use CPU band scratch.
    ///
    /// # Errors
    ///
    /// Identical to [`RunBackend::composite`].
    #[doc(hidden)]
    pub fn composite_with_thread_count(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
        n_threads: usize,
    ) -> Result<Nv12Image> {
        match self {
            Self::Cpu(cpu) => cpu.composite_with_thread_count(
                canvas_w, canvas_h, canvas, background, tiles, n_threads,
            ),
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => {
                let _ = n_threads;
                gpu.composite_inline(canvas_w, canvas_h, canvas, background, tiles)
            }
            Self::SimulatedGpu(sim) => {
                let _ = n_threads;
                sim.composite_inline(canvas_w, canvas_h, canvas, background, tiles)
            }
        }
    }
}

#[cfg(test)]
mod target_tests {
    use super::{normalize_pci_bus_id, AdapterMatchInfo, GpuTarget};

    fn adapter(pci: &str, vendor: u32, device: u32, name: &str) -> AdapterMatchInfo {
        AdapterMatchInfo {
            pci_bus_id: pci.to_owned(),
            vendor_id: vendor,
            device_id: device,
            name: name.to_owned(),
        }
    }

    #[test]
    fn empty_target_is_inert_and_matches_nothing() {
        // An all-`None` target pins nothing: `is_some()` is false and it never
        // matches a real adapter, so the caller keeps its default adapter path.
        let target = GpuTarget::none();
        assert!(!target.is_some());
        assert!(!target.matches(&adapter("0000:01:00.0", 0x10de, 0x2882, "RTX 4060")));
    }

    #[test]
    fn nvml_8hex_domain_bus_id_matches_vulkan_4hex_domain() {
        // The live cross-format case: NVML reports an 8-hex-digit domain, wgpu a
        // 4-hex-digit domain, for the SAME card. Normalisation must make them
        // compare equal so the P2000 the placement engine chose is the adapter
        // wgpu acquires.
        let target = GpuTarget {
            pci_bus_id: Some("00000000:01:00.0".to_owned()),
            ..GpuTarget::default()
        };
        assert!(target.is_some());
        assert!(target.matches(&adapter("0000:01:00.0", 0x10de, 0x1c30, "Quadro P2000")));
    }

    #[test]
    fn bus_id_mismatch_does_not_match_even_with_same_vendor() {
        // Two NVIDIA cards on different PCI slots: the bus id discriminates them
        // exactly (do NOT fall through to a vendor-only match — vendor alone is
        // never a discriminator).
        let target = GpuTarget {
            pci_bus_id: Some("00000000:01:00.0".to_owned()),
            vendor_id: Some(0x10de),
            device_id: Some(0x2882),
            name: Some("RTX 4060".to_owned()),
        };
        // Different bus AND different device id (the other card): no match.
        assert!(!target.matches(&adapter("0000:02:00.0", 0x10de, 0x1c30, "Quadro P2000")));
    }

    #[test]
    fn vendor_device_pair_matches_when_bus_id_absent() {
        // No bus id known (a backend that does not expose one): the (vendor,
        // device) PCI pair is the fallback discriminator.
        let target = GpuTarget {
            pci_bus_id: None,
            vendor_id: Some(0x10de),
            device_id: Some(0x1c30),
            name: Some("Quadro P2000".to_owned()),
        };
        assert!(target.matches(&adapter("", 0x10de, 0x1c30, "Quadro P2000")));
        // A different device id (same vendor) must NOT match on the pair.
        assert!(!target.matches(&adapter("", 0x10de, 0x2882, "RTX 4060")));
    }

    #[test]
    fn vendor_alone_never_matches() {
        // Only the vendor id is known (no device id, no bus id, no name): the
        // pair gate requires BOTH vendor and device, so this matches nothing.
        let target = GpuTarget {
            vendor_id: Some(0x10de),
            ..GpuTarget::default()
        };
        assert!(!target.is_some());
        assert!(!target.matches(&adapter("0000:01:00.0", 0x10de, 0x2882, "RTX 4060")));
    }

    #[test]
    fn name_is_the_last_resort_discriminator() {
        // No bus id, no PCI pair — only the adapter name. An exact name match is
        // the weakest but still better than blindly grabbing GPU0.
        let target = GpuTarget {
            name: Some("Quadro P2000".to_owned()),
            ..GpuTarget::default()
        };
        assert!(target.is_some());
        assert!(target.matches(&adapter("0000:01:00.0", 0x10de, 0x1c30, "Quadro P2000")));
        assert!(!target.matches(&adapter("0000:01:00.0", 0x10de, 0x2882, "RTX 4060")));
    }

    #[test]
    fn normalize_reduces_domain_and_lowercases() {
        assert_eq!(normalize_pci_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_bus_id("0000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_bus_id("0000:0A:00.0"), "0000:0a:00.0");
        // A short domain is left-padded to the 4-digit Vulkan width.
        assert_eq!(normalize_pci_bus_id("0:01:00.0"), "0000:01:00.0");
        // No colon: returned lower-cased unchanged (still self-equal).
        assert_eq!(normalize_pci_bus_id("garbage"), "garbage");
    }
}
