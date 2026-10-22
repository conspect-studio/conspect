//! The compositor **drive loop** (invariants #1 and #2).
//!
//! At every output tick the drive loop:
//!
//! 1. **Samples** each tile's [`TileStore`] *without blocking* — it reads the
//!    last-good frame (or, if the tile has never produced one or has starved
//!    past its no-signal threshold, falls back to a `NoSignal` slate card). It
//!    **never awaits an input**: a dead, stalled, or empty producer simply
//!    yields its held/placeholder frame.
//! 2. **Assembles** the composite request from the active [`Layout`], mapping
//!    each cell's normalized rectangle to canvas pixels.
//! 3. **Invokes** the configured compositor backend
//!    ([`multiview_compositor::backend::RunBackend`] — the pure-Rust CPU
//!    reference by default, or the GPU compositor with CPU fallback when a run
//!    prefers the GPU) to produce exactly one tagged output [`Nv12Image`] for
//!    the tick.
//!
//! The result is one valid composited frame per tick, on time, forever —
//! regardless of input health. The loop holds no locks an input could hold and
//! makes no `.await` on any input (it is fully synchronous).
//!
//! ## Pooled per-tick scratch (no per-tick churn)
//!
//! The resolve step needs three small scratch buffers each tick — the `held`
//! Arc vector keeping each sampled frame alive, the `placements` vector, and the
//! z-order index vector. These come from a **pool reused across ticks**
//! (`ComposeScratch`, held behind a [`RefCell`]) that is cleared-and-refilled,
//! never reallocated, so the protected output clock does no per-tick scratch
//! allocation in steady state (CLAUDE.md safety rule §5: "frame buffers come from
//! per-device pools allocated at start, never per-frame"). The pool grows at most
//! once per layout-geometry (when a larger cell count first appears) and then
//! stays stable — a bounded, one-time reservation, never proportional to the tick
//! count. The pool is single-threaded (the drive composes on exactly one thread,
//! the output clock), so the `RefCell` is never contended and the borrow never
//! crosses an `.await`; invariant #1 (wait-free on the hot path) holds by
//! construction. The composited canvas itself is the one genuine per-tick
//! allocation the compositor backend must produce — it is **moved** into the
//! returned frame, never copied here.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::backend::{RunBackend, RunBackendKind};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image, Tile};
use multiview_core::layout::Layout;
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_framestore::TileStore;

use crate::clock::Tick;
use crate::error::{Error, Result};
use crate::slate::{failover_slate_image, FailoverSlate};

/// A frame composited for one output tick: the tagged canvas image, its
/// presentation timestamp, and the per-source states sampled this tick.
///
/// The states drive telemetry (the tile state machine) and the degradation
/// loop; the `pts` is re-stamped onto every output packet downstream
/// (invariant #3).
#[derive(Debug, Clone)]
pub struct CompositedFrame {
    /// The tick this frame was produced for.
    pub tick: Tick,
    /// The tagged output canvas (NV12, BT.709 limited per the canvas).
    pub canvas: Nv12Image,
    /// Per-source lifecycle state sampled this tick (`source_id -> state`).
    ///
    /// Includes every cell-bound source; a cell with no bound source is omitted.
    pub source_states: HashMap<String, SourceState>,
}

impl CompositedFrame {
    /// The presentation timestamp of this frame.
    #[must_use]
    pub const fn pts(&self) -> MediaTime {
        self.tick.pts
    }
}

/// Drives the configured compositor backend once per output tick over a fixed
/// set of per-source [`TileStore`]s and a (hot-swappable) [`Layout`].
///
/// The drive loop owns shared [`Arc`] handles to each source's frame store; the
/// decoders publish into the *same* stores concurrently. Reading is lock-free
/// and never blocks (the framestore guarantees a definite answer every time), so
/// the loop upholds invariant #1 even when every input is absent.
///
/// The payload `T` is the per-tile frame type the stores hold. The default
/// [`CompositorDrive<Nv12Image>::compose`] holds [`Nv12Image`] directly and
/// dispatches the composite through the configured
/// [`RunBackend`] — the CPU
/// reference by default, or the wgpu GPU compositor (with CPU fallback) when a
/// run injects a GPU-preferred backend via [`CompositorDrive::with_backend`].
/// State sampling ([`CompositorDrive::sample_states`]) is available for any `T`.
pub struct CompositorDrive<T> {
    /// Per-source last-good-frame stores, keyed by source id.
    stores: HashMap<String, Arc<TileStore<T>>>,
    /// The active layout (hot-swappable via [`CompositorDrive::set_layout`]).
    layout: Arc<Layout>,
    /// Cell-id → layout cell index, populated via
    /// [`CompositorDrive::with_cell_ids`] / [`CompositorDrive::set_cell_ids`].
    ///
    /// This is the O(1) lookup that makes [`CompositorDrive::rebind_cell`] a pure
    /// pointer re-point: it resolves a cell id to its position in `layout.cells`
    /// so the binding can be mutated in place **without** a full `solve_layout` /
    /// `validate` re-solve (a pure source re-point leaves geometry unchanged, so
    /// revalidation is unnecessary — RT-6 / ADR-0034). Empty when no ids were
    /// supplied (the drive then has no addressable cells to re-point, and
    /// `rebind_cell` is an honest error).
    cell_index: HashMap<String, usize>,
    /// The fixed canvas color (ADR-C001 SDR BT.709 limited by default).
    canvas_color: CanvasColor,
    /// The "no signal" slate card, composited for a down tile that has **no**
    /// per-cell failover policy supplied (the default / back-compat path — the
    /// caller-provided card, byte-identical to the prior behaviour).
    nosignal_card: Arc<Nv12Image>,
    /// Per-cell failover-slate policy (`on_loss`), aligned with `layout.cells`
    /// order — `cell_slates[i]` is the policy for `layout.cells[i]` (ADR-0027 /
    /// ADR-0030). Populated via [`CompositorDrive::with_cell_slates`] /
    /// [`CompositorDrive::set_cell_slates`]. **Empty** (the default) means every
    /// down cell shows `nosignal_card`, exactly as before. When present, a down
    /// cell shows the slate its policy selects (`Bars` / `NoSignal` / `Black`),
    /// built once and cached in `slate_cache`.
    cell_slates: Vec<FailoverSlate>,
    /// The per-policy slate-image cache + build odometer. Each distinct
    /// [`FailoverSlate`] image is built **once** at canvas size (lazily, on first
    /// use) and reused every tick — the protected output clock does no per-tick
    /// slate allocation (invariant #1). Behind a [`RefCell`] so the lazy build
    /// runs under `compose`'s `&self`; the drive composes on exactly one thread
    /// (the output clock), so the cell is never contended and the borrow never
    /// crosses an `.await`.
    slate_cache: RefCell<SlateCache>,
    /// The canvas background (linear canvas-gamut), shown where no tile covers.
    background: LinearRgba,
    /// The compositor backend the per-tick composite dispatches through.
    ///
    /// Defaults to the CPU reference ([`RunBackend::cpu`]) so an unconfigured
    /// drive is byte-for-byte the prior behaviour. A run that prefers the GPU
    /// injects a GPU-or-CPU-fallback backend via [`CompositorDrive::with_backend`]
    /// (the GPU is degradation-safe: a missing/failed adapter falls back to the
    /// CPU reference, never stalling the output clock — invariant #1).
    backend: RunBackend,
    /// Pooled per-tick resolve scratch (the `held` Arc vector, `placements`, and
    /// the z-order index vector), reused tick-over-tick instead of reallocated
    /// every tick on the output clock (CLAUDE.md safety rule §5). Behind a
    /// [`RefCell`] so [`CompositorDrive::compose`] keeps its `&self` signature
    /// (it is called on immutable drives in tests and by-reference from the
    /// runtime); the drive composes on exactly one thread (the output clock), so
    /// the cell is never contended and the borrow never crosses an `.await`.
    /// Only the `Nv12Image` payload uses the scratch (the resolve step is
    /// monomorphic over [`Nv12Image`]); a non-`Nv12Image` drive simply never
    /// borrows it.
    scratch: RefCell<ComposeScratch>,
}

impl<T> CompositorDrive<T> {
    /// Build a drive loop for `layout` with per-source frame `stores`.
    ///
    /// `nosignal_card` is the slate composited for any tile whose source is
    /// absent / `NoSignal`. `background` fills uncovered canvas pixels.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidLayout`] if the layout fails
    /// [`Layout::validate`].
    pub fn new(
        layout: Arc<Layout>,
        stores: HashMap<String, Arc<TileStore<T>>>,
        nosignal_card: Nv12Image,
        canvas_color: CanvasColor,
        background: LinearRgba,
    ) -> Result<Self> {
        layout
            .validate()
            .map_err(|e| Error::InvalidLayout(e.to_string()))?;
        Ok(Self {
            stores,
            layout,
            cell_index: HashMap::new(),
            canvas_color,
            nosignal_card: Arc::new(nosignal_card),
            cell_slates: Vec::new(),
            slate_cache: RefCell::new(SlateCache::default()),
            background,
            backend: RunBackend::cpu(),
            scratch: RefCell::new(ComposeScratch::default()),
        })
    }

    /// The cumulative number of times the per-tick resolve **scratch pool** has
    /// reserved backing capacity (grown), across the lifetime of this drive.
    ///
    /// This is the pool's allocation odometer: it bumps on the first compose (the
    /// one-time warm-up that sizes the pool to the layout) and again only if a
    /// later layout swap grows the cell count past the high-water mark. In steady
    /// state — a stable layout composed for any number of ticks — it does **not**
    /// advance, which is exactly the per-tick-allocation gate (CLAUDE.md safety
    /// rule §5): the count is bounded by the layout-geometry changes, never by the
    /// tick count. Exposed for the allocation-count test and for telemetry.
    #[must_use]
    pub fn scratch_reservations(&self) -> u64 {
        self.scratch.borrow().reservations
    }

    /// Attach the cell ids (in `layout.cells` order) so cells can be addressed by
    /// id for a live re-point ([`CompositorDrive::rebind_cell`]).
    ///
    /// `ids[i]` is the id of `layout.cells[i]` (or [`None`] for an unnamed cell,
    /// which is then not re-pointable). The shared [`Layout`] type carries no
    /// per-cell id, so the drive holds this id → index map alongside it; the
    /// caller (which solved the layout from a config that *does* carry cell ids)
    /// supplies the parallel id list. Ids beyond the cell count are ignored;
    /// builder form for ergonomic construction.
    #[must_use]
    pub fn with_cell_ids(mut self, ids: Vec<Option<String>>) -> Self {
        self.set_cell_ids(ids);
        self
    }

    /// Set (or replace) the cell-id → index map from `ids` (in `layout.cells`
    /// order). See [`CompositorDrive::with_cell_ids`].
    pub fn set_cell_ids(&mut self, ids: Vec<Option<String>>) {
        self.cell_index.clear();
        for (index, id) in ids.into_iter().enumerate() {
            if index >= self.layout.cells.len() {
                break;
            }
            if let Some(id) = id {
                self.cell_index.insert(id, index);
            }
        }
    }

    /// Attach the per-cell **failover-slate policy** (`on_loss`), in `layout.cells`
    /// order — `slates[i]` is the policy for `layout.cells[i]` (ADR-0027 /
    /// ADR-0030). A down cell then composites the slate its policy selects
    /// (`Bars` → SMPTE bars, `NoSignal` → the signal-lost card, `Black` → black),
    /// built once and reused per tick.
    ///
    /// Builder form for ergonomic construction. **Omitting this** (or passing an
    /// empty list) keeps the prior behaviour: every down cell shows the
    /// caller-provided `nosignal_card`, byte-identical to before. A `slates`
    /// shorter than the cell count leaves the trailing cells on the default card;
    /// entries beyond the cell count are ignored.
    #[must_use]
    pub fn with_cell_slates(mut self, slates: Vec<FailoverSlate>) -> Self {
        self.set_cell_slates(slates);
        self
    }

    /// Set (or replace) the per-cell failover-slate policy from `slates` (in
    /// `layout.cells` order). See [`CompositorDrive::with_cell_slates`].
    pub fn set_cell_slates(&mut self, mut slates: Vec<FailoverSlate>) {
        slates.truncate(self.layout.cells.len());
        self.cell_slates = slates;
    }

    /// The cumulative number of distinct failover-slate **images built** so far.
    ///
    /// Each distinct [`FailoverSlate`] policy in use builds its canvas-size slate
    /// image **once** (lazily, on first compose) and reuses it every tick, so this
    /// odometer is bounded by the number of distinct policies (≤ 3), **never** by
    /// the tick count — exactly the no-per-tick-slate-allocation gate (invariant
    /// #1, CLAUDE.md safety rule §5). Exposed for the build-once test and
    /// telemetry.
    #[must_use]
    pub fn slate_builds(&self) -> u64 {
        self.slate_cache.borrow().builds
    }

    /// The source id currently bound to the cell named `cell_id`, if that cell is
    /// addressable and bound. Reflects any [`CompositorDrive::rebind_cell`] applied
    /// so far (introspection / control-plane echo).
    #[must_use]
    pub fn effective_cell_source(&self, cell_id: &str) -> Option<String> {
        let index = *self.cell_index.get(cell_id)?;
        self.layout.cells.get(index).and_then(|c| c.source.clone())
    }

    /// Replace the compositor backend the per-tick composite dispatches through.
    ///
    /// A run that prefers the GPU passes
    /// [`RunBackend::select(Some(target))`](RunBackend::select), where `target`
    /// is the load-aware admission decision (the device the placement engine
    /// chose, or `GpuTarget::none()` for the default adapter): the GPU is used if
    /// an adapter initializes, else it transparently falls back to the CPU
    /// reference (invariant #1 — a missing/failed GPU never stalls or crashes the
    /// run). The default backend is the CPU reference, so callers that never call
    /// this keep the prior behaviour exactly.
    #[must_use]
    pub fn with_backend(mut self, backend: RunBackend) -> Self {
        self.backend = backend;
        self
    }

    /// The kind of compositor backend this drive composites through
    /// ([`RunBackendKind::Cpu`] or, under `wgpu` with a live adapter,
    /// `RunBackendKind::Gpu`). For telemetry / introspection.
    #[must_use]
    pub const fn backend_kind(&self) -> RunBackendKind {
        self.backend.kind()
    }

    /// The active layout.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Hot-swap the active layout (a Class-1/Class-2 reconfiguration applies it
    /// at a frame boundary — i.e. between ticks).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidLayout`] if the new layout fails validation; the
    /// existing layout is retained on error so the loop never adopts a bad one.
    pub fn set_layout(&mut self, layout: Arc<Layout>) -> Result<()> {
        layout
            .validate()
            .map_err(|e| Error::InvalidLayout(e.to_string()))?;
        self.layout = layout;
        Ok(())
    }

    /// Register or replace a source's frame store.
    pub fn insert_store(&mut self, source_id: impl Into<String>, store: Arc<TileStore<T>>) {
        self.stores.insert(source_id.into(), store);
    }

    /// The registered frame store for `source_id`, if any.
    ///
    /// Used by the live-apply drain (ADR-W018) to **reuse** an existing store on
    /// an edit-by-id upsert, so the bound tile holds last-good through a
    /// producer swap instead of flashing the slate.
    #[must_use]
    pub fn store(&self, source_id: &str) -> Option<&Arc<TileStore<T>>> {
        self.stores.get(source_id)
    }

    /// Unregister a source's frame store (live remove, ADR-W018), returning
    /// whether a store was registered under that id.
    ///
    /// Cells still bound to the id composite their **failover slate** from the
    /// next [`CompositorDrive::compose`] tick (a missing store is the honest
    /// `NoSignal` path in `sample_cell`), and a subsequent
    /// [`CompositorDrive::rebind_cell`] to the id is a held error — never a
    /// panic, never a stall (invariants #1/#2). O(1) map removal at the frame
    /// boundary; the producer teardown happens off the clock thread.
    pub fn remove_store(&mut self, source_id: &str) -> bool {
        self.stores.remove(source_id).is_some()
    }

    /// Re-point the cell named `cell_id` to sample source `source_id`, **LIVE** —
    /// the O(1) crosspoint re-point (RT-6 / ADR-0034, instant VIDEO→cell switch).
    ///
    /// This is a pure source re-point: only which store the cell samples changes,
    /// the cell's geometry/placement is untouched, so it **skips
    /// `solve_layout`/`validate`** entirely. The next [`CompositorDrive::compose`]
    /// tick draws the new source — applied at the frame boundary by the engine's
    /// per-tick control hook, never blocking the output clock (invariant #1: this
    /// is a binding mutation, not a wait on the new source). The compositor scales
    /// the new source into the cell at composite time, so a source of any
    /// geometry composites correctly (scale-at-composite — no clip/smear).
    ///
    /// The target `source_id` must have a registered [`TileStore`] (a declared,
    /// decoding-or-primed source); the cell id must be addressable (supplied via
    /// [`CompositorDrive::with_cell_ids`]). Either being absent is an honest
    /// [`Error::Rebind`] and the prior binding is **held** unchanged — never a
    /// panic, never a silent mis-route.
    ///
    /// # Errors
    ///
    /// [`Error::Rebind`] if `cell_id` is unknown, has no addressable index, or
    /// `source_id` has no registered store.
    pub fn rebind_cell(&mut self, cell_id: &str, source_id: &str) -> Result<()> {
        let Some(&index) = self.cell_index.get(cell_id) else {
            return Err(Error::Rebind(format!("unknown cell id {cell_id:?}")));
        };
        if !self.stores.contains_key(source_id) {
            return Err(Error::Rebind(format!(
                "cell {cell_id:?}: target source {source_id:?} has no registered store"
            )));
        }
        // Mutate ONLY the bound source of the addressed cell, in place. This is a
        // copy-on-write of the small `Layout`/cell vector when the `Arc` is
        // shared (it is not on the hot loop — `compose` borrows, never clones the
        // `Arc`), and crucially it does NOT call `solve_layout` (which re-derives
        // every cell from the config and allocates) nor `validate` (geometry is
        // unchanged on a pure source re-point). The data-plane read in
        // `sample_cell` then samples the new store on the next tick.
        let layout = Arc::make_mut(&mut self.layout);
        let Some(cell) = layout.cells.get_mut(index) else {
            // The id map and layout disagree (cell removed without updating ids):
            // hold rather than panic.
            return Err(Error::Rebind(format!(
                "cell {cell_id:?}: index {index} is out of range"
            )));
        };
        cell.source = Some(source_id.to_owned());
        Ok(())
    }
}

impl<T> CompositorDrive<T> {
    /// Sample every cell-bound source's state as of `now`, without blocking.
    ///
    /// Pure read of the lock-free stores; used for telemetry and the
    /// degradation signal independently of producing a frame.
    ///
    /// Classifies each tile on the **latched** frame — the one the compositor
    /// actually draws at `now` via
    /// [`read_at`](multiview_framestore::TileStore::read_at) — by calling
    /// [`state_at`](multiview_framestore::TileStore::state_at), **not**
    /// producer-liveness [`state`](multiview_framestore::TileStore::state). The two
    /// diverge for an ahead-decoding source: a future-stamped newest frame makes
    /// `state` report `LIVE` while the on-screen picture has frozen and aged, so
    /// telemetry/degradation must follow the drawn picture (`state_at`) to match
    /// what [`compose`](CompositorDrive::compose) renders this tick.
    #[must_use]
    pub fn sample_states(&self, now: MediaTime) -> HashMap<String, SourceState> {
        let mut states = HashMap::new();
        for cell in &self.layout.cells {
            if let Some(source) = &cell.source {
                if let Some(store) = self.stores.get(source) {
                    states.insert(source.clone(), store.state_at(now));
                }
            }
        }
        states
    }
}

impl CompositorDrive<Nv12Image> {
    /// Produce exactly one composited frame for `tick` (invariant #1).
    ///
    /// Samples each cell's source store at `tick.pts` (last-good / `NoSignal`
    /// card), maps cells to canvas pixels, and runs the configured compositor
    /// backend (CPU reference by default; GPU-with-CPU-fallback when a run
    /// prefers the GPU). **Never blocks and never awaits an input.** A tile with no
    /// usable frame contributes the `NoSignal` slate, so the canvas is always
    /// valid and on time.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Canvas`] only for a structurally impossible canvas the
    /// compositor rejects (e.g. odd dimensions). Input health is *never* an
    /// error: a dead source yields its held frame or the slate, not a failure.
    pub fn compose(&self, tick: Tick) -> Result<CompositedFrame> {
        let now = tick.pts;
        let canvas = &self.layout.canvas;

        // The per-tick resolve scratch comes from the pool (cleared-and-reused),
        // not freshly allocated this tick (CLAUDE.md safety rule §5). The pool is
        // single-threaded (this is the one output-clock thread) and the borrow is
        // released before this method returns, so it never crosses an `.await` and
        // never stalls the clock (invariant #1).
        let mut scratch = self.scratch.borrow_mut();
        let cell_count = self.layout.cells.len();
        scratch.begin(cell_count);
        let ComposeScratch {
            held,
            placements,
            order,
            ..
        } = &mut *scratch;

        // The result `source_states` is the frame's OUTPUT (moved to the consumer
        // downstream), so it is genuinely produced each tick — not scratch.
        let mut source_states: HashMap<String, SourceState> = HashMap::new();

        // Draw order: ascending z (lower z first / bottom). Sort cell indices.
        order.extend(0..cell_count);
        order.sort_by_key(|&i| self.layout.cells.get(i).map_or(i32::MIN, |c| c.z));

        for &i in order.iter() {
            let Some(cell) = self.layout.cells.get(i) else {
                continue;
            };
            let (image, state) = self.sample_cell(i, cell, now);
            if let Some(source) = &cell.source {
                source_states.insert(source.clone(), state);
            }
            let (dst_x, dst_y, dst_w, dst_h) = cell_dst_rect(cell, canvas.width, canvas.height);
            held.push(image);
            placements.push(Placement {
                image_index: held.len().saturating_sub(1),
                dst_x,
                dst_y,
                dst_w,
                dst_h,
                opacity: cell.opacity,
            });
        }

        // Borrow the held images for the compositor's `Tile` slice. This slice is
        // a per-tick local, NOT pooled: a `Tile<'_>` borrows into the pooled
        // `held` vector, so its lifetime is the borrow — a `Vec<Tile<'_>>` cannot
        // be a reused struct field in safe Rust (`unsafe` is forbidden here). It
        // is small (one thin `&Nv12Image` + a rect + an opacity per cell), and the
        // large per-tick churn — the `held` Arc vector, `placements`, and the
        // z-order index vector — is what the pool eliminates. Each tile carries
        // its DESTINATION cell rect so the compositor scales the source into the
        // cell at composite time (scale-at-composite, RT-6 / ADR-0034 / inv #6): a
        // source decoded at any geometry — including one just re-pointed in —
        // composites correctly into the cell, never clipped or smeared. When the
        // source already matches the cell size (the steady state:
        // decode-at-display-resolution) the scale is a no-op and the result is
        // byte-for-byte the prior 1:1 placement. The cell's per-tile opacity
        // drives the compositor's premultiplied linear-light `over` blend.
        let tiles: Vec<Tile<'_>> = placements
            .iter()
            .filter_map(|p| {
                held.get(p.image_index).map(|img| {
                    Tile::scaled(img.as_ref(), p.dst_x, p.dst_y, p.dst_w, p.dst_h, p.opacity)
                })
            })
            .collect();

        // Dispatch the composite through the selected backend (CPU reference by
        // default; GPU-with-CPU-fallback when a run prefers the GPU). The call
        // is synchronous and never blocks on an input or a client; on the GPU
        // path a submit/readback failure surfaces as a typed error here, which
        // becomes `Error::Canvas` — the drive returns rather than stalling the
        // clock, and the caller holds last-good (safety rule §3, invariant #1).
        let canvas_image = self
            .backend
            .composite(
                canvas.width,
                canvas.height,
                self.canvas_color,
                self.background,
                tiles.as_slice(),
            )
            .map_err(|e| Error::Canvas(e.to_string()))?;

        // Drop the borrowing tiles, then release the held `Arc`s now (rather than
        // pinning them until the next tick's `begin`), keeping the pooled
        // capacity. The next `begin` clears whatever remains, so correctness does
        // not depend on this; it shortens last-good frame retention to within the
        // tick. `drop(tiles)` first releases its immutable borrow of `held`.
        drop(tiles);
        held.clear();
        drop(scratch);

        Ok(CompositedFrame {
            tick,
            canvas: canvas_image,
            source_states,
        })
    }

    /// Sample one cell's image and state without blocking: the bound source's
    /// held/fresh frame, or this cell's **failover slate** when there is nothing
    /// usable (the slate its `on_loss` policy selects, or the default
    /// `nosignal_card` when no per-cell policy was supplied).
    ///
    /// `index` is the cell's position in `layout.cells`, used to look up the
    /// per-cell policy in `cell_slates`.
    fn sample_cell(
        &self,
        index: usize,
        cell: &multiview_core::layout::Cell,
        now: MediaTime,
    ) -> (Arc<Nv12Image>, SourceState) {
        let Some(source) = &cell.source else {
            // An unbound cell always shows the slate.
            return (self.slate_for_cell(index), SourceState::NoSignal);
        };
        let Some(store) = self.stores.get(source) else {
            return (self.slate_for_cell(index), SourceState::NoSignal);
        };
        // Latch-on-tick: sample by the OUTPUT media instant `now`, selecting the
        // source frame nearest-but-not-after it (streaming-gotchas §1). This is
        // what makes a tile advance 1:1 with output time even when the output
        // loop momentarily runs slower than real-time and the producer decoded
        // ahead — a plain latest-wins read would race the tile to the newest
        // decoded frame.
        let read = store.read_at(now);
        let state = read.state();
        match read.frame() {
            Some(frame) => (Arc::clone(frame), state),
            None => (self.slate_for_cell(index), state),
        }
    }

    /// The slate image a **down** cell at `index` composites.
    ///
    /// Resolves the cell's per-cell [`FailoverSlate`] policy (`cell_slates[index]`)
    /// to its canvas-size slate image, **built once** and cached
    /// (`slate_cache`) — reused every tick (invariant #1: no per-tick slate
    /// allocation on the output clock). The slate is scaled-at-composite into the
    /// cell rect downstream (RT-6), exactly like the held source frame.
    ///
    /// When the cell has **no** per-cell policy (the default / back-compat path —
    /// `cell_slates` empty or shorter than `index`), returns the caller-provided
    /// `nosignal_card`, byte-identical to the prior behaviour. A slate that fails
    /// to build (a structurally impossible canvas the compositor rejects) also
    /// falls back to `nosignal_card` rather than stalling the clock.
    fn slate_for_cell(&self, index: usize) -> Arc<Nv12Image> {
        let Some(&policy) = self.cell_slates.get(index) else {
            return Arc::clone(&self.nosignal_card);
        };
        let mut cache = self.slate_cache.borrow_mut();
        if let Some(image) = cache.images.get(&policy) {
            return Arc::clone(image);
        }
        // First use of this policy: build its canvas-size image once and cache it.
        let canvas = &self.layout.canvas;
        match failover_slate_image(policy, canvas.width, canvas.height, self.canvas_color) {
            Ok(image) => {
                let image = Arc::new(image);
                cache.images.insert(policy, Arc::clone(&image));
                cache.builds = cache.builds.saturating_add(1);
                image
            }
            // A policy whose slate cannot be built on this canvas falls back to
            // the default card — the output clock never stalls (invariant #1).
            Err(_) => Arc::clone(&self.nosignal_card),
        }
    }
}

/// The per-policy failover-slate **image cache** + build odometer for one drive.
///
/// Each distinct [`FailoverSlate`] policy in use builds its canvas-size slate
/// image **once** (lazily) and reuses it every tick; `builds` is the cumulative
/// build count (bounded by the number of distinct policies, ≤ 3, **never** the
/// tick count — invariant #1, CLAUDE.md safety rule §5). Held behind a
/// [`RefCell`] inside [`CompositorDrive`] and touched only by the single-threaded
/// [`CompositorDrive::compose`] (the output clock), so it is never contended and
/// the borrow never crosses an `.await`.
#[derive(Default)]
struct SlateCache {
    /// The built slate image per policy (built once, reused per tick).
    images: HashMap<FailoverSlate, Arc<Nv12Image>>,
    /// Cumulative count of slate-image builds (the no-per-tick-allocation gate's
    /// odometer).
    builds: u64,
}

/// Internal: a resolved tile placement referencing an entry in the `held` vec.
struct Placement {
    image_index: usize,
    dst_x: u32,
    dst_y: u32,
    /// The cell's pixel width on the canvas (the destination the source scales
    /// into; scale-at-composite).
    dst_w: u32,
    /// The cell's pixel height on the canvas.
    dst_h: u32,
    /// The cell's per-tile opacity (straight alpha), carried to the compositor.
    opacity: f32,
}

/// The per-tick resolve **scratch pool**: reusable buffers cleared-and-refilled
/// each tick instead of allocated and dropped on the output clock (CLAUDE.md
/// safety rule §5). Held behind a [`RefCell`] inside [`CompositorDrive`] and
/// touched only by the single-threaded [`CompositorDrive::compose`] (the output
/// clock), so it is never contended and the borrow never crosses an `.await`
/// (invariant #1: wait-free on the hot path).
///
/// `reservations` is the pool's allocation odometer: [`ComposeScratch::begin`]
/// bumps it whenever it must grow a backing store to fit the current cell count
/// (a one-time event per new high-water layout geometry), and leaves it untouched
/// when the existing capacity already fits — so in steady state composing any
/// number of ticks adds **zero** reservations. [`CompositorDrive::scratch_reservations`]
/// surfaces it for the allocation-count gate and telemetry.
#[derive(Default)]
struct ComposeScratch {
    /// Keeps each sampled frame's `Arc` alive across the composite call.
    held: Vec<Arc<Nv12Image>>,
    /// The resolved tile placements (small `Copy` records).
    placements: Vec<Placement>,
    /// The cell draw order (indices into `layout.cells`), sorted by ascending z.
    order: Vec<usize>,
    /// Cumulative count of backing-store reservations (pool growths) — the
    /// per-tick-allocation gate's odometer.
    reservations: u64,
}

impl ComposeScratch {
    /// Ready the pool for a tick that resolves `cell_count` cells: clear the
    /// reused buffers (keeping their capacity) and reserve **once** if the
    /// capacity does not already fit, bumping `reservations` only when it grows.
    ///
    /// Clearing keeps the allocation; the contents are fully overwritten each tick
    /// (the resolve loop refills `held`/`placements`, `order` is re-extended), so
    /// no stale data leaks — the composed output is byte-identical to a freshly
    /// allocated scratch (proven by the byte-identity gate).
    fn begin(&mut self, cell_count: usize) {
        self.held.clear();
        self.placements.clear();
        self.order.clear();
        // One bounded reservation per new high-water cell count. `capacity()` only
        // grows, so once it covers the layout this is a no-op for every later tick
        // — the steady-state per-tick allocation is zero.
        let mut grew = false;
        if self.held.capacity() < cell_count {
            self.held.reserve(cell_count - self.held.capacity());
            grew = true;
        }
        if self.placements.capacity() < cell_count {
            self.placements
                .reserve(cell_count - self.placements.capacity());
            grew = true;
        }
        if self.order.capacity() < cell_count {
            self.order.reserve(cell_count - self.order.capacity());
            grew = true;
        }
        if grew {
            self.reservations = self.reservations.saturating_add(1);
        }
    }
}

/// Map a cell's normalized rectangle to a canvas **pixel rectangle**
/// `(dst_x, dst_y, dst_w, dst_h)`, saturating into range.
///
/// Both the origin and the far corner are floored to integer pixels and the size
/// is the difference (`right - left`, `bottom - top`), so adjacent cells whose
/// fractional edges meet (`a.x + a.w == b.x`) tile **exactly** — no gap and no
/// overlapping seam — and a full-canvas cell maps to the full canvas. The size is
/// what the compositor scales the cell's source into (scale-at-composite); when
/// the source is already decoded at this size the scale is a no-op (the prior
/// 1:1 placement, byte-for-byte).
fn cell_dst_rect(
    cell: &multiview_core::layout::Cell,
    canvas_w: u32,
    canvas_h: u32,
) -> (u32, u32, u32, u32) {
    let edge_px = |frac: f32, extent: u32| -> u32 {
        if !frac.is_finite() || frac <= 0.0 {
            return 0;
        }
        f64_floor_to_u32(f64::from(frac) * f64::from(extent), extent)
    };
    let left = edge_px(cell.x, canvas_w);
    let top = edge_px(cell.y, canvas_h);
    let right = edge_px(cell.x + cell.w, canvas_w);
    let bottom = edge_px(cell.y + cell.h, canvas_h);
    // `right >= left` and `bottom >= top` because the fractions are monotone and
    // floored identically; `saturating_sub` is defensive against a degenerate
    // non-finite edge that `edge_px` floored to 0.
    let dst_w = right.saturating_sub(left);
    let dst_h = bottom.saturating_sub(top);
    (left, top, dst_w, dst_h)
}

/// Floor a non-negative `f64` to a `u32`, clamped to `[0, max]`, with **no**
/// `as` cast (guardrail: `as_conversions` is denied).
///
/// `f64` integers up to `2^53` are exact, so for our pixel domain (`max <=
/// u32::MAX < 2^32`) the floored value is represented exactly. We recover the
/// integer with a branch-free binary search over `[0, max]`, comparing each
/// `u32` candidate (widened losslessly with `f64::from`) against `value` — total,
/// panic-free, and `as`-cast-free. Runs once per cell per tick (a handful of
/// cells), never per pixel.
fn f64_floor_to_u32(value: f64, max: u32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= f64::from(max) {
        return max;
    }
    // Largest `candidate` in `[0, max]` with `f64::from(candidate) <= value`.
    let mut lo: u32 = 0;
    let mut hi: u32 = max;
    while lo < hi {
        // `lo + (hi - lo + 1) / 2` rounds the midpoint up so the loop makes
        // progress toward `hi`; all arithmetic stays within `u32`.
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= value {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}
