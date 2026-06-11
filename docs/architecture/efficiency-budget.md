# Efficiency Budget

> **Status:** authoritative per-stage efficiency budget for the Multiview data plane.
> Pairs with [`docs/research/efficiency.md`](../research/efficiency.md) (the "why") and
> ADR-E001..E008. **Invariants enforced:** #5 NV12-throughout (1.5 B/px, never RGBA per tile),
> #6 decode-at-display-resolution (budget in MP/s), #7 encode-once-mux-many, #9 bounded queues.
>
> **Honesty note:** every "current cost" below is an **engineering estimate `(est)`** derived
> from the code (allocation sites + pixel arithmetic), *not* a runtime measurement — there is no
> alloc/bandwidth bench in CI yet (see [CI bench-gate spec](#ci-bench-gate-spec)). The byte counts
> per allocation are *exact* (verified against the struct layouts and `vec!`/`create_texture`
> call sites cited); only the per-second **rates** are estimates, because they multiply the exact
> per-tick byte cost by an assumed cadence and source count. Rates use the default **25 fps**
> cadence unless noted; the cadence is config-driven (`config.canvas.fps.rational()`,
> `multiview-cli/src/pipeline.rs:511`), so every rate scales linearly with the configured fps
> (e.g. ×2 at 50 fps, ×2.4 at 60 fps).
>
> **Pixel constants (exact):**
> - 1080p **NV12** = 1920·1080·1.5 = **3,110,400 B ≈ 3.11 MB** (Y 2,073,600 + UV 1,036,800)
> - 1080p **Rgba16Float** (8 B/px) = 1920·1080·8 = **16,588,800 B ≈ 16.59 MB**
> - 1080p **PremulRgba accumulator** (`[f32;4]` = 16 B/px) = 1920·1080·16 = **33,177,600 B ≈ 33.18 MB**
> - 1080p **UYVY** (2 B/px) = 1920·2·1080 = **4,147,200 B ≈ 4.0 MB** (33% larger than NV12)
> - 4K **NV12** = 3840·2160·1.5 = **12,441,600 B ≈ 12.44 MB**

## Per-stage budget table

| Stage | Budget ceiling | Current cost @1080p25 (est) | @4K25 (est) | Status | Ref (file:line) |
|---|---|---|---|---|---|
| **Decode** (per engine) | Budget in **MP/s** (inv #6), not stream count. Per-tile working set ≤ **NV12 1.5 B/px** at *display* size. Per-source surfaces from a bounded pool — **no per-frame alloc**. | ~4 heap allocs + 2 tile-sized memcpys **per decoded frame per source** (fresh `Video::empty` sws dst + 2 fresh plane `Vec`s). 480×270 tile ≈ 194 KB ×2 ≈ 388 KB/frame; ×9 src ≈ **87 MB/s churn** | scales with tile area | **OVER** (no pool) | `multiview-cli/src/pipeline.rs:4225-4240`; `multiview-ffmpeg/src/scale.rs:115-130` |
| **Framestore** (per source) | Ring depth = **latch-on-tick + reconnect window only** (target 32–64). Per-publish CoW = Arc-pointer clone × depth (no pixel copy). Slot read = lock-free binary search, **0 heap**. | `RING_CAPACITY=256` → per publish: ~256 `Arc::clone` (atomic incr) + ~256 `Arc` decr on old-snapshot drop + one ~4 KiB `Vec` alloc/free. ×9 src@25 ≈ **~115k atomic ops/s + ~900 KiB/s allocator churn**. **Retention NOT solved:** a full ring pins up to **256 distinct live `Arc<Nv12Image>` = ~760 MiB pixels per tile** (~6.7 GiB across 9 src @1080p). | same (count-based) | **OVER** (cap 16× oversized; retention) | `multiview-framestore/src/tile.rs:199,262-276,70-82` |
| **Compositor — GPU** | Per-tick surfaces from a **per-device pool allocated once** (efficiency.md §2.4 / safety rule §5): **0 alloc/tick** steady-state. | `record_composite`+`encode_and_read` `create_texture`/`create_buffer` **every tick**, freed via `Drop`: canvas_lin Rgba16Float **16.59 MB** + tile arrays (y_array R8 + uv_array Rg8) ~3.11 MB + y_out 2.07 + uv_out 1.04 + 2 readback (256-B row-padded) 2.21+1.11 + uniforms ~0 = **~26.13 MB/tick no-overlay (~653 MB/s)**; **+ second overlaid Rgba16Float 16.59 = ~42.72 MB/tick overlay (~1.07 GB/s)** | ~2.6 GB/s | **OVER** (no pooling) | `multiview-compositor/src/gpu/compositor.rs:545,559,615,712,726,807-853`; struct caches only pipelines/overlay `:57-69` |
| **Compositor — CPU** (default backend) | Pooled/double-buffered output planes; **no `vec![0;..]` zero-init** (fill writes every byte); pooled, clear-not-realloc band accumulator. **0 alloc/tick** steady-state. | Output canvas `vec![0;w·h]`+`vec![0;w·h/2]` = **3.11 MB/tick alloc+memset** (~78 MB/s) + premul band acc `vec![bg_premul; span_pixels]` (16 B/px) + `vec![false; span_pixels]` (1 B/px). Accumulator is **covered-span-scoped** (O(covered_rows×w), not unconditional full-frame): all-background bands alloc 0; a **fully-tiled multiview** worst case ≈ **33.18 MB + 2.07 MB = ~35.25 MB/tick** (~881 MB/s). LutSet rebuilt/tick ≈ 41 KB (BT.709). | canvas 12.44 MB/tick; acc worst case ~141 MB/tick | **OVER** (no pool); acc bounded by coverage (#46) | `multiview-compositor/src/pipeline.rs:523-524,856-857,513-521` |
| **Overlay (CPU bake, dirty-rect path)** | Linear accumulator sized to **dirty-rect extent**, not full canvas. Output NV12 **pooled + only dirty rows copied**. | `LinearCanvasBuffer::transparent(w,h)` (16 B/px) = **33.18 MB/tick alloc+zero** (~830 MB/s) for a buffer processed **rects-only** (typically ~99% untouched) + **ONE** full NV12 plane passthrough copy (`y/uv.to_vec()`) = **3.11 MB/tick** (~78 MB/s). `Nv12Image::new` **moves** the planes (no second copy). | ~133 MB/tick acc + ~12.4 MB/tick copy | **OVER** (compute region-limited #41; **alloc is full-canvas**) | `multiview-compositor/src/overlay/subpass.rs:1221,1200-1201,1253` |
| **Encode** (per rendition) | **Encode-once** (inv #7). NV12-native HW encoders skip swscale. Software NV12→YUV420P required but **pooled** (no per-tick frame alloc). | NV12→AVFrame bridge copy (`nv12_to_video` fresh `Video::new` + row memcpy) **~3.11 MB/tick** (~78 MB/s, **avoidable**) + swscale NV12→YUV420P fresh `Video::empty` **~3.11 MB/tick** (~78 MB/s, **necessary for sw codecs**). ~6.2 MB/tick alloc churn. | ×4 area | **OVER** on the bridge copy; swscale within (sw codecs) | `multiview-cli/src/pipeline.rs:3149-3166`; `multiview-output/src/sink.rs:362`; `multiview-ffmpeg/src/scale.rs:115` |
| **Output / fan-out** | **Share coded packets by ref-count, never deep-copy** (efficiency.md:54). N sinks ⇒ **0 payload copies**. | `fan_packets` `packet.clone()` → ffmpeg-next `av_packet_ref` + **`av_packet_make_writable` → deep payload memcpy per sink**. Bitrate-bound: ~0.75 MB/s @2 Mbps×3; ~3.75–7.5 MB/s @10–20 Mbps×3. Muxer (`mux.rs:207-213`) never mutates payload ⇒ make_writable unnecessary; **all N copies removable**. | same (bitrate-bound) | **OVER** (make_writable unnecessary) | `multiview-cli/src/pipeline.rs:1942`; `multiview-ffmpeg/src/packet.rs:146` |
| **NDI out** | Send **NV12 natively** (SDK exposes `NDIlib_FourCC_video_type_NV12`, identical layout) — 0 conversion, 1.5 B/px wire. | `nv12_to_uyvy` fresh `Vec(w·2·h)` + full 4:2:0→4:2:2 convert **per send**: **4.0 MB UYVY/tick alloc** + ~7.26 MB/tick memory-pass (read 3.11 + write 4.15) + 33% wire vs NV12. **Sys layer already exposes `NdiVideoFourCc::Nv12`** (`send.rs:55`); only the output seam hardcodes UYVY. Built+tested, **not yet wired** (no live caller in pipeline.rs). | ×4 area | **OVER** (100% removable — fix-before-wire) | `multiview-output/src/ndi/convert.rs:183`; `ndi/output.rs:112`; `multiview-ndi-sys/src/send.rs:55` |

### Aggregate per-tick alloc ceiling (the headline target)

**Target: ZERO heap/GPU allocations on the steady-state compose→encode→mux hot path** (safety
rule §5: "frame buffers come from per-device pools allocated at start, never per-frame"). Today
the hot path allocates+frees, per **1080p tick**:

- **GPU backend:** ~26.13 MB (no overlay) **OR** ~42.72 MB (with overlay), **OR**
- **CPU backend:** canvas 3.11 MB + band-acc up to ~35.25 MB (fully-tiled worst case);
- **+ canvas deep-copy 3.11 MB** (`multiview-cli/src/pipeline.rs:1074` — a genuine full-plane
  `Nv12Image::clone()`, not the cheap Arc bump the adjacent comment implies);
- **+ overlay acc 33.18 MB + NV12 passthrough copy 3.11 MB** (overlay feature);
- **+ encode bridge 3.11 MB + swscale 3.11 MB**;
- **+ NDI 4.0 MB** (when wired).

This is the budget gap the pooling work (EFF-0..EFF-9 below) must close. **Corrections vs the
prior audit draft:** the CPU band accumulator is **covered-span-scoped, not unconditional
full-frame** (an all-background band allocates 0; the 33.18 MB is a fully-tiled worst case, +2.07
MB for the coverage bitmap); the overlay bake copies the NV12 planes **once (3.11 MB), not twice
(6.2 MB)** — `Nv12Image::new` moves the `Vec`s; and the framestore Arc refactor fixed copy
*amplification* but **not retention** — a full 256-deep ring still pins ~760 MiB of pixels per tile.

## Standing audit cadence

Tie this audit into the **engineering guardrails** (CLAUDE.md §"Engineering guardrails";
[agent-guardrails.md](../development/agent-guardrails.md)). Re-run the data-plane efficiency audit:

1. **At every subsystem boundary** — when a PR first touches any of: `multiview-compositor`,
   `multiview-framestore`, `multiview-overlay`, `multiview-input` (decode/ingest),
   `multiview-output` (encode/fan-out/NDI), the engine drive loop, or
   `multiview-cli/src/pipeline.rs` hot loop. Re-confirm the per-stage row(s) it touches still
   match the table; update the `(est)`/status cell **in the same PR**.

2. **Before ANY hot-path merge** (blocking, same tier as the typing/TDD pillars) — any diff to
   decode→composite→encode→mux, the output clock, or the frame stores requires: (a) the
   [CI bench-gate](#ci-bench-gate-spec) green, and (b) a one-line note in the PR confirming **no
   new per-tick allocation** was introduced on the hot path (or a justified, pooled exception with
   the pool's allocation site cited).

3. **Quarterly full re-run** — re-run the complete data-plane sweep (all 8 stages) and reconcile
   every `(est)` against the then-current bench numbers; demote any estimate that now has a
   measured value to `(measured)`.

4. **On any invariant-#1/#5/#9 touch** — a change that adds a channel from engine→outside,
   materializes RGBA on the data plane, or grows a queue bound triggers an immediate targeted
   re-audit + a chaos/soak run (per CLAUDE.md §2).

**Audit output format:** the JSON-then-table synthesis (area → hotPaths with file:line +
per-tick/rate + estCost + verdict + fix). The orchestrator diffs successive runs to catch
regressions.

## CI bench-gate spec

**Current state (verified against HEAD):**

- **No CI job runs any bench.** Workflows are `ci.yml`, `docker.yml`, `release-plz.yml`,
  `release.yml`; `grep -niE 'bench|criterion' .github/workflows/` returns nothing. The criterion
  benches (`composite_realtime.rs`, `overlay_efficiency.rs`) are `harness=false` `[[bench]]`
  targets invoked only by `cargo bench`, which no workflow calls.
- **The libtest budget gate exists but is deliberately skipped.**
  `tile_driven_meets_budget_9up_1080p` (`tests/composite_tile_driven.rs:377`) *would* run under
  `cargo test --workspace`, but CI explicitly skips it: `ci.yml:54`
  `cargo test --workspace -- --skip tile_driven_meets_budget`.
- **All existing gates are wall-clock** (`Instant::now()`/`elapsed()` median asserts) —
  noise-prone on shared runners (efficiency.md §7) and they gate **frame time, not allocations
  or bandwidth**. The noise-immune `iai-callgrind` instruction/alloc gate efficiency.md §7
  prescribes as the hard fail is used **nowhere**.

Add three gate classes:

| Gate | Tool | What it measures | Threshold (fails the PR if exceeded) |
|---|---|---|---|
| **Per-tick ALLOC gate** (primary, **blocking**, deterministic) | `iai-callgrind` (instruction + alloc counts) or `dhat` heap profiler in a `#[test]` harness | Heap allocations **per `composite()` call** (CPU path) and per encode/bake call | **0 allocations** steady-state for each pooled stage once its pooling lands; until then a **pinned per-stage ceiling** committed to `bench-baseline.json`, ratcheted down as EFF-* lands. **Any *increase* vs baseline fails.** Deterministic ⇒ noise-immune (efficiency.md §7). |
| **Per-tick BYTES gate** (**blocking**) | `dhat` (bytes) + counting-allocator wrapper | Total bytes alloc'd + memcpy'd per tick on compose→encode→mux | Per-stage MB/tick ceiling from the table: CPU canvas ≤ 3.11 MB; band acc ≤ covered-span extent; overlay acc ≤ dirty-rect extent; encode ≤ swscale-only (bridge = 0); GPU ≤ pooled-set; framestore publish ≤ (ring-depth × 16 B Vec + ring-depth atomic ops). **Regression vs baseline fails.** |
| **GPU surface-pool COUNT gate** (self-hosted GPU runner) | instrumented `GpuContext` counter | `device.create_texture` / `create_buffer` calls per `composite()` | **0** per tick once GPU pooling (EFF-0) lands; before that, the committed count. Any per-tick allocation fails. |
| **Frame-time gate** (secondary, **advisory/warn**) | existing `criterion` (`composite_realtime.rs`, `overlay_efficiency.rs`) | median `composite()` wall-clock | ≤ **50% of tick** to leave encode/mux headroom: 25 fps=20 ms, 30 fps=16.7 ms, 50 fps=10 ms, 60 fps=8.3 ms. Wall-clock = **advisory (warn)**, never hard-fail (noise). |

**Wiring:**

1. Add a `bench` job to `.github/workflows/ci.yml` that runs the `iai-callgrind`/`dhat` ALLOC +
   BYTES gates on the **CPU reference path** (GPU-free, CI-eligible) and compares against a
   committed `bench-baseline.json`. The **ALLOC gate is the blocking PR gate** (deterministic).
2. Add a **self-hosted GPU job** that runs the GPU surface-pool COUNT gate.
3. **Un-skip** the `tile_driven_meets_budget` budget test or, preferably, convert it +
   `composite_realtime.rs` to the **advisory** wall-clock warn-tier and actually run them in the
   bench job.

**Measurement plan (turning `(est)` → `(measured)`):** each table `(est)` becomes `(measured)`
once dhat/iai reports its alloc count + bytes for that stage. Priority to instrument:
(1) GPU surface set (largest, 26–42 MB/tick), (2) CPU band accumulator + covered bitmap,
(3) overlay bake accumulator, (4) canvas deep-copy, (5) encode bridge + swscale,
(6) framestore publish CoW (count atomic ops + Vec churn via counting wrapper),
(7) NDI convert (when wired).

## Remediation backlog (ranked, dependency-ordered)

Each item is PR-sized. Savings are exact per-tick byte counts × the default 25 fps cadence
(scale linearly with configured fps). "Confirmed" = verified against HEAD with the cited
file:line.

### EFF-0 — Pool the GPU compositor's per-tick transient surfaces
**`multiview-compositor/src/gpu/compositor.rs:545,559,615,712,726,807-853`** (struct fields
`:57-69` cache only pipelines/layouts/overlay — no texture pool). `record_composite` +
`encode_and_read` + `read_plane` `create_texture`/`create_buffer` the full set **every tick** and
free via `Drop`: `canvas_lin` (16.59 MB) + tile arrays (~3.11 MB) + `y_out`/`uv_out` (3.11 MB) +
2 readback buffers (3.32 MB) + (overlay) second `overlaid` canvas (16.59 MB).
- **Savings:** ~26.13 MB/tick no-overlay (~653 MB/s @25), ~42.72 MB/tick overlay (~1.07 GB/s);
  ~2.6 GB/s @4K25 of allocate-then-free GPU churn.
- **Fix:** persist the canvas-sized, run-stable surfaces (`canvas_lin`, `overlaid`, `y_out`,
  `uv_out`, readback buffers, MAX_TILES tile arrays) as `GpuCompositor` fields allocated once
  per run-geometry — exactly as the overlay images texture-array (`:167`) already is.
- **Invariants:** respects #1/#5/#7/#10 (linear-light canvas is required by #8, is full-canvas
  not per-tile RGBA, so #5 holds). **Largest single win.**
- **Deps:** none. Gated by the GPU surface-pool COUNT gate.

### EFF-1 — Pool the CPU band accumulator + coverage bitmap
**`multiview-compositor/src/pipeline.rs:856-857`** `vec![bg_premul; span_pixels]` (16 B/px) +
`vec![false; span_pixels]` (1 B/px), alloc+free per tick. Already covered-span-scoped (#46), so
the win is the **per-tick alloc churn**, peaking on fully-tiled multiview.
- **Savings:** up to ~35.25 MB/tick (~881 MB/s @25) for full-coverage multiview (the common
  case); ~141 MB/tick @4K25. Sparse/PiP layouts already allocate less.
- **Fix:** pool acc + covered as per-thread/per-band drive scratch (clear-and-reuse, not realloc).
  Optional follow-up: `f16x4` (8 B/px) to halve the accumulator.
- **Invariants:** does not change blend math (#8); #5/#7/#10 unaffected.
- **Deps:** none.

### EFF-2 — Pool the overlay bake accumulator + dirty-row-only NV12 copy
**`multiview-compositor/src/overlay/subpass.rs:1221`** `LinearCanvasBuffer::transparent(w,h)`
(16 B/px, ~99% untouched — processing is rects-only at `:1206-1251`) + **`:1200-1201`** one
full NV12 plane passthrough `to_vec()` (3.11 MB; `Nv12Image::new` at `:1253` **moves**, no
second copy).
- **Savings:** ~33.18 MB/tick acc alloc+zero (~830 MB/s @25). The 3.11 MB passthrough is mostly
  unavoidable (output is a fresh shared `Arc<Nv12Image>` also held by the preview tap, so
  in-place mutation is forbidden by #10), but the **acc** is pure waste.
- **Fix:** (a) reuse one persistent `LinearCanvasBuffer` in `StreamBaker` per run (canvas dims
  fixed), reset only dirty rects per tick — preserves byte-identity (canvas-relative indexing
  unchanged); (b) optionally memcpy only non-dirty spans into a pooled output frame instead of a
  full `to_vec()`.
- **Invariants:** bake runs off the hot loop behind bounded `hot_rx` (#1/#10 safe); fresh NV12
  out (#5).
- **Deps:** none. Note ADR-0023 region-limited the *compute* but not the *allocation*; the
  `overlay_efficiency.rs` bench measures wall-time so it misses this residual.

### EFF-3 — Eliminate the per-tick canvas deep-copy on the protected output loop
**`multiview-cli/src/pipeline.rs:1074`** `Arc::new(frame.canvas.clone())` deep-copies the owned
`Nv12Image` (both `Vec<u8>` planes, 3.11 MB) **on the engine output clock**. The `:1071-1073`
comment calls it a cheap Arc clone — it is a full pixel copy. The engine owns+drops `frame` the
same iteration.
- **Savings:** ~3.11 MB/tick (~78 MB/s @25, ~155.5 MB/s @50, ~186.6 MB/s @60) of pure memcpy on
  the output clock.
- **Fix:** have the engine hand the projection an `Arc<Nv12Image>` directly (or `std::mem::take`
  the canvas out of the owned `CompositedFrame`) so the wrap is free. The single `Arc` is already
  reused for both fan-out and preview (`:1075,1077`), so one free wrap suffices.
- **Invariants:** removes work from the #1 hot path; #5/#7/#10 unaffected. **High-value because
  it's on the protected clock.**
- **Deps:** small engine-API change to `CompositedFrame` ownership handoff.

### EFF-4 — Double-buffer the CPU output canvas; drop the wasted zero-init
**`multiview-compositor/src/pipeline.rs:523-524`** `vec![0;w*h]` + `vec![0;w*h/2]` per composite
(default backend). The zero-init is immediately overwritten by `fill_band_solid` + tile fold.
- **Savings:** ~3.11 MB/tick alloc + a full-frame memset that writes every byte the encode reads
  (~78 MB/s @25, ~311 MB/s @4K25).
- **Fix:** double-buffer pooled plane `Vec`s in `CompositorDrive`; drop the zero-init (the fill
  path writes every byte).
- **Invariants:** #5/#7 unaffected.
- **Deps:** none.

### EFF-5 — Swscale directly from the NV12 planes (drop the encode bridge copy)
**`multiview-cli/src/pipeline.rs:3149-3166`** `nv12_to_video` allocs a fresh `Video::new` and
row-memcpys both planes **before** swscale runs as a separate pass (`sink.rs:362` →
`scale.rs:115`).
- **Savings:** ~3.11 MB/tick bridge alloc + ~78 MB/s traffic @25 (×4 @4K). The swscale
  NV12→YUV420P itself stays (necessary for software codecs); the **bridge** copy is removable.
- **Fix:** wrap/borrow the `Nv12Image` planes directly as the swscale source; pool the one frame
  you must keep. (For NV12-native HW encoders — NVENC/QSV/VAAPI/VideoToolbox — skip swscale
  entirely, see the inserted-conversion gate.)
- **Invariants:** #7 (still encode-once); #5 unaffected.
- **Deps:** none.

### EFF-6 — Ref-only packet sharing in fan-out (no per-sink payload copy)
**`multiview-cli/src/pipeline.rs:1942`** `packet.clone()` → ffmpeg-next
`av_packet_ref` + `av_packet_make_writable` (`multiview-ffmpeg/src/packet.rs:146`) = deep payload
memcpy per sink. The muxer (`mux.rs:207-213`) mutates only struct fields (stream_index,
pts/dts/duration) and **reads** the payload — make_writable is unnecessary.
- **Savings:** bitrate-bound — ~0.75 MB/s @2 Mbps×3 sinks, ~3.75–7.5 MB/s @10–20 Mbps×3.
  **All N copies become free** (not just N−1) since no sink ever mutates the payload.
- **Fix:** add `EncodedPacket::ref_clone` in `multiview-ffmpeg` calling `av_packet_ref` **without**
  `av_packet_make_writable` (ffmpeg-next's `Packet::clone` does not expose this), and use it at
  `:1942`. Each muxer still gets its own `AVPacket` struct to set_stream/rescale; only the
  immutable buffer is shared.
- **Invariants:** #7 (per-rendition packets still distinct structs); #1/#10 (fan-out stays
  drop-oldest, never back-pressures engine).
- **Deps:** none.

### EFF-7 — Right-size the framestore ring (RING_CAPACITY 256 → 32–64)
**`multiview-framestore/src/tile.rs:199`** `RING_CAPACITY = 256`; publish CoW at `:262-276` is a
`Vec::clone` whose width = cap (256 `Arc::clone` + 256 decr on snapshot drop + ~4 KiB Vec/publish).
The Arc refactor (`:70-82`) fixed copy *amplification* but a full ring still **retains up to 256
distinct live frames = ~760 MiB pixels per tile** (~6.7 GiB across 9 src @1080p).
- **Savings:** 256→32 cuts per-publish atomic traffic + Vec churn ~8× (~115k→~14k atomic ops/s
  @9src) **and** proportionally shrinks pinned pixel retention (~760 MiB → ~95 MiB per full tile).
- **Fix:** cap at the latch-on-tick + reconnect window (32–64), or add time-bounded eviction
  (ADR-T009 endorses lowering the cap as the cheap/safe lever).
- **Invariants:** read stays a lock-free binary search over a consistent snapshot (#1/#10 safe);
  cost is on the sampled input thread, never the output clock.
- **Deps:** none. Gated by the framestore publish gate.

### EFF-8 — Per-source decode scratch pool (no per-frame alloc)
**`multiview-cli/src/pipeline.rs:4225-4240`** two fresh plane `Vec`s +
**`multiview-ffmpeg/src/scale.rs:115-130`** fresh `Video::empty` sws dst **per decoded frame**.
Geometry is fixed once the lazy `TileScaler` is built.
- **Savings:** ~4 allocs + ~388 KB copy per frame per source; ~87 MB/s @9src/25fps.
- **Fix:** per-source 2-frame scratch pool keyed on the fixed post-scale geometry.
- **Invariants:** input-thread only (#1/#10 safe); #5/#6 unaffected.
- **Deps:** none.

### EFF-9 — NDI NV12-native send (fix-before-wire)
**`multiview-output/src/ndi/convert.rs:183`** `nv12_to_uyvy` allocs a fresh `Vec(w·2·h)` =
4.0 MB + full 4:2:0→4:2:2 convert per send (`ndi/output.rs:112`). The SDK exposes
`NDIlib_FourCC_video_type_NV12` with identical layout, and the sys layer **already** has
`NdiVideoFourCc::Nv12` (`multiview-ndi-sys/src/send.rs:55`) — only the output seam hardcodes UYVY.
**Not yet wired into pipeline.rs** (no live caller).
- **Savings:** ~4.0 MB/tick UYVY alloc (~100 MB/s @25, ~249 MB/s @60) + ~7.26 MB/tick memory
  pass + 33% wire bandwidth + the 4:2:0→4:2:2 up-sample compute — **100% removable**.
- **Fix:** add an NV12 send path on `NdiOutput` (`send_canvas` builds an NV12 `NdiVideoFrame`
  with stride=width, fourcc=Nv12) and **delete** the per-tick `nv12_to_uyvy` from the seam;
  keep UYVY only as an optional compat path. **Do this before wiring NDI output into pipeline.rs**
  so no live tick ever pays the conversion.
- **Invariants:** #5 (NV12-throughout — the documented native path); output-side only (#1/#10).
- **Deps:** must land before NDI output is wired into the run.

### EFF-10 — Cache the CPU LutSet across ticks
**`multiview-compositor/src/pipeline.rs:513-521`** `for_transfers` rebuilds EOTF (6144 f32) +
OETF (4096 f32) tables **per composite** (`transfer_lut.rs:207-221`).
- **Savings:** ~41 KB/tick (BT.709) to ~106 KB/tick (mixed PQ) + ~10k transcendental node
  evals/tick.
- **Fix:** cache `LutSet` on the drive keyed by the present transfer set; rebuild only on
  hot-reconfig.
- **Invariants:** #8 unaffected (identical tables).
- **Deps:** none.

### EFF-11 — Hoist engine drive per-tick Vec/HashMap churn
**`multiview-engine/src/drive.rs:227-267`** fresh held/placements/order(+sort)/tiles `Vec`s +
`HashMap<String, SourceState>` with per-tick `String` key clones, on the output clock.
- **Savings:** small bytes (~few KB/tick) but ~5 allocs + a sort + a rehash + String clones per
  tick at 25–50 fps on the output clock.
- **Fix:** hoist to reusable cleared fields; pre-sort z-order per layout-swap; intern source ids.
- **Invariants:** removes work from the #1 hot path.
- **Deps:** none.

### EFF-12 — Wire the HAL cost model into admission/degradation (invariant #9)
Not a byte/cycle waste but the **efficiency control plane is dead at runtime**. `ControlLoop`
(`multiview-engine/src/degrade.rs:61-159`) is complete code constructed **only in tests**
(`tests/degrade.rs:25,31,…`); `multiview-cli/src` has zero `ControlLoop`/`CostBudget`/`admit`
references. A deployment cannot reject an over-budget config nor shed load. Separately,
`CostBudget` (`multiview-hal/src/cost.rs:24-32`) models only Mpix/s on 3 stages — VRAM/cores/watts
(3 of efficiency.md §4's 4 vectors) are unmodelled, and the `select.rs:602` VRAM gate's
`predicted_pool_bytes` is **caller-supplied, not derived from NV12 1.5 B/px**.
- **Fix:** in the engine `RuntimeBuilder`, build a `ControlLoop` from the configured `CostBudget`,
  call `admit(plan)` once at start as a **hard admission gate** (reject/propagate before output
  starts — pre-roll, not the hot path, so #1 holds), then drive `step(pressure)` on the **slow
  control tick** (never the per-frame output clock, never `.await`-ing a client → #1/#10), shedding
  cheapest-impact-first via `affects_program()`. Derive `predicted_pool_bytes` from NV12 1.5 B/px ×
  pool depth so the VRAM gate reflects #5.
- **Invariants:** the wiring respects #1/#5/#9/#10 by construction (slow-tick, pre-roll gate).
- **Deps:** unlocks invariant #9 end-to-end; lower-urgency than the per-tick alloc wins but is the
  structural enabler for resource-adaptive degradation.
