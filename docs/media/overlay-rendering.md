# Efficient Overlay Rendering — Design Brief

> **Area:** Media / Compositor · Overlays
> **Status:** Verification-hardened design brief (no code this phase)
> **Date:** 2026-06-03
> **Owns crates:** [`multiview-overlay`](../../crates/multiview-overlay) (models + draw-list),
> [`multiview-compositor`](../../crates/multiview-compositor) (rasterizer + GPU/CPU draw)
> **Decision:** [ADR-0016](../decisions/ADR-0016.md) (this brief's load-bearing choices)
> **Builds on:** [ADR-R008](../decisions/ADR-R008.md), [ADR-R007](../decisions/ADR-R007.md),
> [ADR-E006](../decisions/ADR-E006.md), [ADR-C003](../decisions/ADR-C003.md),
> [ADR-E002](../decisions/ADR-E002.md)
> **Read alongside:** [resilience-and-av §7](../research/resilience-and-av.md),
> [color-management §2,§5,§6](../research/color-management.md),
> [efficiency §2.2,§2.6](../research/efficiency.md),
> [core-engine §8.2,§13](../research/core-engine.md)

This brief is the authoritative source for **how Multiview draws overlays onto the program/tile
output most efficiently**: per-tile text labels, clocks (analog + digital), audio meters
(PPM/VU/peak/true-peak bars + goniometer/correlation + R128), confidence scopes
(waveform/vectorscope/histogram/parade), subtitle burn-in (SRT/VTT/ASS), safe-area / center-cross
markers, tally borders, UMD, alert cards, and the IDENTIFY flash. Efficiency is the point: every
choice below is justified against bytes-moved and per-frame allocation, not feature count.

---

## 0. TL;DR (the load-bearing decisions)

1. **One blend path, in the engine's linear canvas.** Overlays are blended into the existing
   `Rgba16Float` **linear-light** canvas *after* the tile composite pass and *before* the NV12
   encode pass, using the same premultiplied source-over operator the tiles use
   ([`blend::over`](../../crates/multiview-compositor/src/blend.rs), invariant #8). We do **not** use
   any third-party renderer's own render pass — doing so would blend text in the framebuffer's
   color space (sRGB/8-bit) and bypass linear-light + NV12-throughout. (See §4.)
2. **Raster glyph atlas, not SDF/MSDF.** Multiview renders overlays at a known, fixed canvas pixel
   size (decode-at-display-res, invariant #6), so SDF/MSDF scale-invariance buys nothing while
   costing offline generation, rounded-corner artifacts (SDF) and ~3× atlas memory (MSDF). A raster
   shelf-packed atlas wins. Reserve a hand-written SDF *only* for vector chrome (rounded alert-card
   corners, meter-bar rounding). (See §3.1.)
3. **`cosmic-text` for CPU shape+layout+rasterize+atlas; Multiview's compositor for the draw.** We
   adopt the `glyphon` stack (`cosmic-text` + `swash` + `etagere`) for **shaping, rasterization and
   atlas management only**, and feed the cached glyph quads into Multiview's own
   [`DrawList`/`DrawQuad`](../../crates/multiview-overlay/src/resolve.rs) contract (ADR-R008). The same
   `swash` bitmaps serve the GPU path (atlas upload) **and** the CPU reference path (blit into the
   linear canvas) — one text engine, two consumers, no metric divergence. (See §3.2, §4.4.)
4. **Cache static, re-rasterize only dynamic.** Per-glyph caching is automatic and is the key lever:
   a clock ticking `12:34:56 → 12:34:57` only ever rasterizes the 10 digits + `:` *once total*.
   Static labels (tile names, safe-area legends, UMD between revisions) cost zero rasterization and
   zero upload on an unchanged frame. Gate all re-work on a content revision/hash — never per frame.
   (See §5.)
5. **Dirty-region uploads + bounded atlas.** Full-canvas RGBA re-upload is ~249 MB/s @1080p30 and
   competes with decode/encode; only animated regions (meter bars, clock seconds, pulsing cards) are
   re-uploaded, into a persistent atlas with bounded memory and LRU eviction. (See §5.3, §6.)
6. **Meters/scopes/markers are cheap GPU primitives, not rasterized images.** Meter bars,
   safe-area boxes, center crosses, tally borders, goniometer/correlation, and scope graticules are
   **uniform-driven quads / lines** evaluated in-shader — no per-frame bitmap. Only their numeric
   readouts go through text. Sample data is conflated to ~10–30 Hz at the source (invariant #10).
   (See §3.3, §3.4.)
7. **Subtitles off the hot path.** ASS → `libass` (behind the off-by-default `libass` feature) and
   SRT/VTT → the pure-Rust `cosmic-text` path both render on a dedicated worker into a "latest
   rendered overlay" slot the compositor samples; the synchronous libavfilter `subtitles`/`ass`
   filter is **never** in the live path. (See §3.5.)

---

## 1. Requirements → render classes

Every required overlay maps to exactly one of four render classes. The class determines the cost
model and which subsystem renders it.

| Overlay | Render class | Dynamic part | Update cadence |
|---|---|---|---|
| Per-tile **text label** / tile name | Text (atlas) | string only on change | on change |
| **UMD** (under-monitor display) | Text (atlas) | string on revision bump | on change |
| **Digital clock** / program timecode | Text (atlas) | seconds field | 1 Hz (or frame on timecode) |
| **Analog clock** | Vector primitive (lines) | hand angles | 1 Hz (or smooth) |
| **Audio meters** (PPM/VU/peak/true-peak) | Vector primitive (quads) + text scale | bar heights, peak-hold | ~10–30 Hz conflated |
| **Goniometer / correlation** | Vector primitive (points/line) | sample cloud, correlation | ~10–30 Hz conflated |
| **Confidence scopes** (waveform/vectorscope/histogram/parade) | Vector primitive (quads/points) + graticule | bin counts | ~10–30 Hz conflated |
| **Subtitle burn-in** (SRT/VTT/ASS) | Rasterized image (worker) | cue text on cue change | on cue boundary |
| **Safe-area / center-cross markers** | Vector primitive (lines) | none (static) | static |
| **Tally borders** | Vector primitive (quads) | color/brightness on state | on state change |
| **Alert cards** | SDF rounded-rect + text | state, pulse phase | on state / pulse |
| **IDENTIFY flash** | Vector primitive (quad) | square-wave phase | on phase |

Three render paths cover all of them:

- **Text path** — `cosmic-text`/`swash`/`etagere` glyph atlas; quads emitted into the `DrawList`.
- **Primitive path** — uniform-driven quads/lines/points evaluated in the compositor shader; *zero*
  per-frame rasterization (the meter is geometry, not a picture). SDF only for rounded chrome.
- **Rasterized-image path** — `libass` (ASS) or `cosmic-text` (SRT/VTT) on a worker thread into a
  cached premultiplied RGBA bitmap; the compositor samples the latest.

All three converge on the **same portable contract**: a premultiplied-RGBA atlas + a back-to-front
quad list, blended in linear light. There is **no stable cross-vendor external-texture import in
wgpu** ([efficiency §2.5](../research/efficiency.md)), which is exactly why the contract is geometry
+ identity, not GPU handles (ADR-R008).

---

## 2. Where it lives

```
multiview-overlay  (pure Rust, no native dep in default build)
  ├─ layer.rs / resolve.rs   OverlayStack → DrawList<DrawQuad>   (the WHAT + WHERE)
  ├─ clock / scopes / tally / umd / timer / identify / safearea / alert
  │                          pure render MODELS (values, angles, bin counts, phases)
  └─ caption_probe.rs        subtitle/caption metadata model

multiview-compositor  (rasterizer + draw; GPU behind `wgpu`, CPU reference always on)
  ├─ blend.rs                premultiplied OVER in linear light (shared by tiles + overlays)
  ├─ gpu/compositor.rs       composite pass → Rgba16Float linear canvas → encode pass → NV12
  └─ overlay/ (new, gated)   text rasterizer (cosmic-text/swash/etagere), atlas, primitive shaders,
                             the overlay blend sub-pass, CPU-reference blit
```

**Division of responsibility (unchanged from the existing scaffold):**

- `multiview-overlay` stays **pure model + layout math**. It already emits the backend-agnostic
  [`DrawList`](../../crates/multiview-overlay/src/resolve.rs); the scope/meter/clock modules already
  produce drawable values (`Histogram::bins`, `Waveform::columns`, `Vectorscope::bins`, hand angles,
  square-wave phase). It must **never** touch the GPU or a C library.
- `multiview-compositor` gains a new **feature-gated overlay module** that turns those models into
  pixels: the glyph rasterizer + atlas, the primitive shaders, and the overlay blend sub-pass.
- The **rasterizer is shared** between the GPU fast path and the CPU reference path so the two
  oracles never diverge on text metrics.

**Feature flags** (conventions §4): the text/primitive overlay path rides under the existing
`wgpu` feature for the GPU draw and is available pure-Rust for the CPU reference; **`libass`**
(off by default, conventions §4 / ADR-R007) adds ASS rasterization and pulls the
libass/HarfBuzz/FriBidi C toolchain. SRT/VTT need **no** native dependency.

---

## 3. The three render paths in detail

### 3.1 Why raster atlas, not SDF/MSDF

A raster glyph atlas wins for Multiview on every axis that matters here: fast preprocessing (rasterize
on demand, no offline build), correct hinting, color/emoji glyphs, and "all characters." SDF/MSDF
only pay off when you scale/rotate/zoom *the same glyph* arbitrarily or want cheap outline/glow/
shadow effects — at the cost of slow offline generation, SDF rounded-corner artifacts, and ~3×
atlas memory for MSDF (distance in RGB). Multiview renders overlays at a **known, fixed canvas pixel
size** (invariant #6), so the scale-invariance benefit is unused.
[Red Blob Games, *Distance field fonts*](https://www.redblobgames.com/articles/sdf-fonts/) is
explicit: "SDF is not always the best choice for fonts."

**Decision:** raster atlas for glyphs. Reserve a hand-written **SDF rounded-rect** shader *only* for
vector chrome (alert-card corners, meter-bar rounding) where evaluating one distance function is
cheaper than rasterizing a bitmap and gives crisp resolution-independent corners.

### 3.2 Text: `cosmic-text` + `swash` + `etagere`, drawn by our compositor

We adopt the `glyphon` stack but use it surgically:

- **`cosmic-text`** does CPU shaping + layout (Unicode BiDi, Arabic/Indic complex shaping, font
  fallback) and owns `SwashCache`. As of `cosmic-text` 0.15+ the shaper is **HarfRust** (a Rust
  HarfBuzz port; earlier versions used `rustybuzz`) — still MIT/Apache, cargo-deny clean.
- **`swash`** rasterizes glyphs to bitmaps; **`etagere`** shelf-packs them into a dynamic atlas.
- **Multiview's compositor draws the cached glyph quads** through the existing `DrawList`/`DrawQuad`
  premultiplied-RGBA-atlas + quad-list contract — **not** through `glyphon`'s `TextRenderer::render`.

**Why not `glyphon`'s renderer directly (load-bearing):** `glyphon`'s `prepare()/render()` targets a
wgpu **render pass** with fixed-function alpha blending against an 8-bit sRGB/linear surface
(`ColorMode::Accurate = Rgba8UnormSrgb`, `ColorMode::Web = Rgba8Unorm`) and blends in the
framebuffer's color space. Multiview's compositor has **no render pass** — it composites tiles into an
`Rgba16Float` **linear** canvas inside **compute** passes
([`gpu/compositor.rs`](../../crates/multiview-compositor/src/gpu/compositor.rs)) then a compute encode
pass writes NV12. Using `glyphon`'s `render()` would composite text in the wrong color space,
bypassing invariant #8 (blend in linear light) and invariant #5 (NV12-throughout). So we take
`cosmic-text`/`swash`/`etagere` for shaping + rasterization + atlas management and keep **one**
linear-light premultiplied blend path for tiles *and* overlays.

**Version compatibility (verified 2026-06):** `glyphon` 0.11.0 (2026-04-13) requires `wgpu ^29.0.0`
and `cosmic-text ^0.18`; the workspace pins **wgpu 29.0.3**
([`Cargo.lock`](../../Cargo.lock)) and the compositor depends on `wgpu = "29"`
([`multiview-compositor/Cargo.toml`](../../crates/multiview-compositor/Cargo.toml)). So the stack is
version-compatible **today, no fork needed**. `cosmic-text`/`swash`/`etagere` are all
permissive (MIT OR Apache-2.0, + Zlib for some), so the default build stays LGPL-clean with **no
native C dependency**. Pin the transitive `cosmic-text` via `glyphon`'s lockfile and re-run
`cargo deny check` after any bump (the `rustybuzz → harfrust` swap and the 0.15→0.18 line change the
shaping dependency graph).

**Alpha correctness (load-bearing, verified):** `swash`/`cosmic-text` rasterize normal glyphs as
**straight 8-bit coverage** (`swash::image::Image` `Alpha` format; subpixel RGBA only for color
emoji). Multiview's blend operates on **premultiplied linear RGBA**
([`blend::over`](../../crates/multiview-compositor/src/blend.rs)), so we **must** premultiply
(`rgb * coverage, coverage`) at/just-before atlas upload — exactly as
[`TextStyle.color`](../../crates/multiview-overlay/src/layer.rs) already documents ("premultiplied at
upload time, ADR-R008"). This is the opposite of `libass`, whose bitmaps are **already**
premultiplied (`r,g,b ≤ a`) and must **not** be premultiplied again. A mismatch halos *every*
antialiased edge — a silent, pervasive bug ([resilience-and-av §7](../research/resilience-and-av.md)).

### 3.3 Meters as geometry (not pictures)

PPM/VU/peak/true-peak bars, peak-hold ticks, the R128 momentary/short/integrated needles, the scale
graticule, the goniometer (Lissajous) trace, and the correlation meter are **uniform-driven quads,
lines, and points** evaluated in the compositor shader. The per-frame data is a *handful of floats*
pushed in a small uniform — not a rasterized image (ADR-R008 "meters-as-geometry"). The
[`MeterStyle`](../../crates/multiview-overlay/src/layer.rs) carries the static styling
(channels, orientation, peak-hold); the engine pushes levels each tick. Because audio meters are
**high-rate**, the values are **conflated to ~10–30 Hz at the source** before they ever reach the
draw path ([realtime-api](../research/realtime-api.md), invariant #10) — the engine never blocks on
a meter consumer.

- **Numeric readouts** (e.g. `-18.0 LUFS`, `-3 dBTP`) and channel letters go through the text path
  but change at most a few Hz, so they re-rasterize at most their changed digits.
- **R128 cadence** matches ADR-R006: two cadences, true-peak gated, read-only and non-blocking.

### 3.4 Confidence scopes + markers + tally + IDENTIFY

- **Scopes** (`scopes.rs` already computes the pure models): the waveform is per-column min/max/mean
  → vertical line segments; the histogram/parade is `BINS` counts → bars; the vectorscope is a
  `BINS × BINS` grid → a points/heat quad with a polar graticule. The **bin reduction is computed on
  CPU from samples the engine extracts** (never a pixel-diff readback on the hot path,
  [efficiency §2.6](../research/efficiency.md)); the renderer only turns counts into quads/points.
  Sample extraction is conflated like meters.
- **Safe-area / center-cross markers** are **static line primitives** — pure geometry, drawn once
  into the atlas (or evaluated in-shader) and never re-rasterized. The
  [`safearea`](../../crates/multiview-overlay/src/safearea.rs) model already produces the rectangles
  and cross.
- **Tally borders** are **quad fills** with brightness-scaled color from
  [`tally::TallyState`](../../crates/multiview-overlay/src/tally.rs); plus a small text label per lit
  region so the state reads beyond color alone (WCAG, ADR-W011). They change only on state.
- **IDENTIFY flash** is a single full-tile quad whose alpha follows a square wave over the media
  clock ([`identify.rs`](../../crates/multiview-overlay/src/identify.rs)) — one uniform, no raster.
- **Alert cards** combine an **SDF rounded-rect** background with a text headline; the background is
  evaluated in-shader (crisp corners, no bitmap) and the text rides the atlas. Critical cards
  (SIGNAL LOST) keep their glyphs **atlas-resident at startup** so they draw at the instant of
  failure with **zero upload** ([resilience-and-av §3,§7](../research/resilience-and-av.md)).

All of these are **input-decoupled** (ADR-R008): they render purely from local state + the media
clock, never from a decoded input frame, so the alert path is drawable when every input *and* the
GPU's tile pipeline are gone.

### 3.5 Subtitle burn-in off the hot path

- **ASS/SSA →** normalize to ASS internally and rasterize with **`libass`** (behind the
  off-by-default `libass` feature, ADR-R007) into sparse premultiplied alpha-coverage bitmaps.
- **SRT/VTT →** the **pure-Rust `cosmic-text` path** (the same engine as labels) — no native dep.
- **Both** run on a **dedicated worker thread** with a lock-free *latest-rendered-overlay* handoff;
  the compositor samples the latest each frame and holds/drops if late. The synchronous libavfilter
  `subtitles`/`ass` filter is **never** in the live output path — it can stall (load-bearing,
  [resilience-and-av §6](../research/resilience-and-av.md)). Pin `libass ≥ 0.17` with
  libunibreak + HarfBuzz + FriBidi for Unicode wrap / CJK / RTL.
- In-bitstream caption passthrough (`AV_FRAME_DATA_A53_CC` side data on the canvas frame) is
  orthogonal and handled at encode time (ADR-R007) — it is *not* a burn-in.

---

## 4. Integration with the compositor pipeline

### 4.1 Where the overlay blend slots in (invariant #8)

The fixed color order (invariant #8) ends in the canvas-linear working buffer **before** OETF/encode.
Overlays must be blended **there**, in linear light, premultiplied:

```
[composite pass]  tiles → range-expand → YUV→RGB → linearize → primaries → blend  ──┐
                                                                                     ▼
                                                            Rgba16Float LINEAR canvas
                                                                                     │
[overlay sub-pass]  draw cached glyph quads + primitive quads/lines (premultiplied   │
                    linear RGBA) over the canvas with blend::over  ──────────────────┤
                                                                                     ▼
[encode pass]       OETF → RGB→YUV → range-compress → NV12 (Y R8 + UV Rg8) → tag ────► output
```

Overlay colors authored in sRGB are **linearized to the canvas working space first** (and converted
to the canvas primaries — sRGB→BT.2020 PQ/HLG when the program is HDR), then premultiplied, then
blended — otherwise overlays shift color on HDR programs
([color-management §5,§6](../research/color-management.md); resilience-and-av §7). Tagging happens
once on the encoded output and is verified with ffprobe (invariant #8) — the overlay sub-pass does
not touch tags.

### 4.2 GPU fast path

A new compute (or lightweight render-into-storage) sub-pass between the existing composite and
encode passes:

- **Glyph/image quads** sample the persistent atlas texture (premultiplied linear RGBA) and blend
  `over` the `Rgba16Float` canvas. The `DrawList` is already back-to-front; each `DrawQuad` carries
  `dest`, `opacity`, `blend`.
- **Primitive quads/lines** (meters, markers, tally, scopes, SDF chrome) are evaluated analytically
  from a small per-frame uniform/storage buffer — no texture sample, no upload.
- **Batching:** one `prepare()`-equivalent gathers *every* overlay's glyph quads for the frame and
  appends them to one quad buffer; primitives append to another. The sub-pass issues the batched
  draws once. `glyphon`'s two-phase `prepare(...[TextArea...]...)/render()` is the model — many text
  areas, one prepare, one render, a shared `Cache` for pipeline/layout/shader.

This reuses the compositor's existing device/queue, bounded buffers, and the SSIM/PSNR validation
harness (GPU output is never bit-exact, ADR-I002).

### 4.3 The overlay atlas

A **persistent, bounded** atlas texture (premultiplied RGBA) holds glyph bitmaps + cached
static-image overlays + atlas-resident critical assets:

- Per-glyph insert keyed on `(font_id, glyph_id, size, subpixel_bin)` (`swash`/`etagere`); each
  unique glyph is uploaded **once** with LRU eviction (`glyphs_in_use` trimmed each frame). `grow()`
  doubles the atlas (capped to device limits) and re-uploads on overflow.
- **Sub-texture (dirty-region) uploads only**: when a label's string changes, only its *new* glyphs
  upload; unchanged glyphs are already resident.
- **Bounded** by an explicit byte cap, allocated from the data-plane budget at start — never grown
  per frame beyond the cap (invariant on bounded memory, ADR-E005).

### 4.4 CPU reference path

The CPU reference compositor ([`pipeline.rs`](../../crates/multiview-compositor/src/pipeline.rs)) uses
the **same** `cosmic-text`/`swash` rasterizer: it blits the same coverage bitmaps into the
`Rgba16Float` linear canvas with the same [`blend::over`](../../crates/multiview-compositor/src/blend.rs).
No second font engine is introduced, so the CPU oracle and GPU path produce identical text geometry
(metrics, kerning, wrap) — only sub-pixel rounding differs, covered by the SSIM/PSNR threshold.
Primitives are evaluated by the same closed-form math on CPU. (If a lighter CPU-only build ever
wants to drop `cosmic-text`, `fontdue`/`ab_glyph` are pure-Rust options, but they lack complex-script
shaping — acceptable only for Latin labels/timecode, never subtitles/i18n; keeping `cosmic-text`
everywhere avoids divergent metrics. This is an option, not the plan.)

---

## 5. The efficiency model

### 5.1 Cache static overlays; re-rasterize only dynamic content

The static/dynamic split maps cleanly onto `cosmic-text`'s per-`Buffer` model:

- **Static** labels (tile names, safe-area legends, UMD between revisions, scope graticules) keep
  their `Buffer` untouched across frames. `prepare()` finds every glyph already in the atlas and
  uploads **nothing**; the draw is just a quad re-blit (cheap).
- **Dynamic** content calls `set_text()` **only when the string actually changes** — UMD revision
  bump, clock second tick, meter numeric readout. Per-glyph caching means the changed string reuses
  resident glyphs; only genuinely new glyphs rasterize/upload.

`cosmic-text` additionally caches **shaping**: `BufferLine` holds `Cached<ShapeLine>` /
`Cached<Vec<LayoutLine>>`, and the `shape-run-cache` feature caches shape plans per run. Re-shaping
happens only when a `Buffer`'s text changes.

### 5.2 Change-detection is mandatory (the mpv cautionary tale)

mpv's `osd-overlay` performance regression
([mpv #7615](https://github.com/mpv-player/mpv/issues/7615)) is the canonical bug: calling
`osd:update()` at high rate with **unchanged** content drove the GPU to 60–100% because it
re-rendered + flushed caches every frame. The fix is change-detection — do work only when content
differs. **Multiview must gate `set_text()` / re-`prepare()` / atlas upload on a content
hash/revision, never per frame.** The overlay models already carry the hooks: `ClockStyle.show_seconds`
drives the per-second upload; UMD updates on a revision bump that fires only on a *visible* change.

### 5.3 Dirty-region / damage tracking + upload bandwidth

A full 1080p RGBA layer is ~8.3 MB/frame ≈ **249 MB/s @30**, ~498 @60, ~995 @4K30 — it competes
directly with decode/encode/compositor bandwidth and is tight on Apple unified memory
([efficiency §2.2,§2.5](../research/efficiency.md); resilience-and-av §7). Therefore:

- **Never re-upload the whole canvas.** Upload only animated regions (changed glyphs, meter bars,
  clock seconds, pulsing card alpha) via sub-texture writes into the persistent atlas.
- Meters/markers/tally/IDENTIFY/SDF chrome are **uniform-driven** — they re-upload *nothing*, just a
  few floats per frame.
- This is the overlay analogue of ADR-E006's tile dirty-region recompositing: skip work for parts
  that did not change, driven by source/state change signals, **never** by pixel-diff readback on
  the hot path (which burns the bandwidth it aims to save).

### 5.4 Zero per-frame allocation + bounded memory

- The atlas, the glyph quad buffer, the primitive buffer, the per-`Buffer` shaping caches, and the
  subtitle latest-rendered slot are **allocated once** and reused; the draw path appends into
  pre-sized buffers (invariant on bounded memory; ADR-E005 frame-pool discipline). No `Vec` growth
  or texture (re)allocation on the per-tick path.
- The atlas has an explicit **byte cap** with LRU eviction; subtitle/static-image bitmaps come from a
  bounded cache. Overflow grows once (capped) or evicts — never unbounded.
- No `unwrap`/`expect`/`panic` on this path (hot-path safety rule #3): a rasterization or atlas error
  holds last-good / skips the layer, it never crashes the output.

### 5.5 Draw batching

One gather pass per output frame collects **all** overlay glyph quads (with premultiplied colors +
dest rects) into one buffer and all primitives into another; the sub-pass issues them as batched
draws — not one draw per label. This keeps overlay cost ≈ constant per frame and independent of N
tiles (resilience-and-av §7: "near-constant per-frame cost").

---

## 6. Efficiency targets / budgets (to benchmark against)

These are the budgets a perf-regression CI gate (ADR-E009) should hold the overlay subsystem to.
They are *engineering targets to measure*, anchored to the bandwidth/allocation model above — not
guarantees.

| # | Target | Rationale / anchor |
|---|---|---|
| T1 | **Overlay upload ≤ ~1 MB/s** in steady state on a typical multiviewer (clocks ticking, meters at 30 Hz, static labels). | vs ~249 MB/s for naive full-canvas re-upload @1080p30 ([efficiency §2.2](../research/efficiency.md)). |
| T2 | **Zero atlas re-rasterization** for an unchanged frame; **only changed glyphs** rasterize on a text change. | per-glyph `SwashCache`; mpv #7615 lesson (§5.2). |
| T3 | **Zero per-frame heap allocation** on the overlay draw path (steady state). | bounded-memory invariant; ADR-E005. |
| T4 | **Bounded atlas**: hard byte cap (e.g. start at 2048×2048 premultiplied RGBA ≈ 16 MB, grow capped to device limit), LRU eviction, never unbounded. | invariant on bounded memory. |
| T5 | **Overlay per-frame GPU cost ≈ constant in N tiles** (one batched prepare + one batched draw). | batching, §5.5; resilience-and-av §7. |
| T6 | **Meter/scope sample rate conflated to ≤ 30 Hz** at the source; the engine never blocks on a meter/scope consumer. | invariant #10; realtime-api; ADR-R006. |
| T7 | **CPU-reference text geometry == GPU text geometry** (same `cosmic-text` metrics); image difference within the SSIM ≥ 0.98 / PSNR ≥ 40 dB gate. | single rasterizer (§4.4); ADR-I002. |
| T8 | **Critical assets (SIGNAL LOST card glyphs, slate) atlas-resident at startup** → drawable with **zero upload** at the instant of failure. | resilience-and-av §3,§7. |
| T9 | **Subtitle/overlay rasterization fully off the hot path**; a late render holds/drops, never stalls the output clock (invariant #1). | resilience-and-av §6; safety rule §7.1. |
| T10 | **Overlay sub-pass adds no extra full-canvas readback**; it draws into the existing linear canvas before the single encode pass. | invariant #5 NV12-throughout; §4.1. |

A golden-frame CPU test (bit-exact on the CPU reference) plus the GPU SSIM/PSNR threshold validate
correctness; the budgets above validate efficiency.

---

## 7. Chosen crates, bundled font, licensing posture

| Concern | Choice | License | Build posture |
|---|---|---|---|
| Shaping + layout + rasterization + atlas | `cosmic-text` (+ `swash`, `etagere`); `glyphon` 0.11 as the wgpu-29-compatible reference | MIT OR Apache-2.0 (+ Zlib) | Pure Rust, no native dep; in default + `wgpu` builds |
| Complex shaping engine | HarfRust (via `cosmic-text` 0.15+) | MIT/Apache | transitive; cargo-deny clean |
| ASS/SSA subtitle raster | `libass` (≥ 0.17 + HarfBuzz + FriBidi + libunibreak) | ISC/GPL-compatible C lib | **off-by-default `libass` feature** (adds C toolchain, ADR-R007) |
| SRT/VTT subtitle raster | `cosmic-text` (same as labels) | MIT/Apache | pure Rust, no native dep |
| Vector chrome (rounded rects, meters, scopes, markers) | hand-written WGSL SDF / quad / line shaders in `multiview-compositor` | project (Multiview Source-Available Non-Commercial License) | in-house; no dep |

**Bundled font:** ship a permissively-licensed, broad-coverage default font so labels/clocks/UMD
render with **no host-font dependency** and deterministic metrics across platforms. Candidates:
**Noto Sans** family (OFL 1.1) for broad Unicode + a monospaced face for timecode, or **Inter**
(OFL 1.1) for UI labels. OFL 1.1 is redistributable and compatible with Multiview's source-available
project license (font licensed separately under OFL, attributed in `LICENSE`/`NOTICE`). The bundled
font also guarantees the SIGNAL LOST / clock glyphs are available at the instant of failure. Operator
fonts can be added via `fontdb` for i18n coverage, but the **bundle is the guaranteed floor**.

**Licensing discipline (conventions §7):** the default and `wgpu` builds stay **LGPL-clean with no
native C dependency** — `cosmic-text`/`swash`/`etagere`/`glyphon` are all permissive. `libass` is the
only native overlay dependency and is **opt-in**. `cargo deny check` gates the graph; re-run after
any `cosmic-text`/`glyphon` bump (the rustybuzz→harfrust swap and 0.15→0.18 line change deps).

---

## 8. Phased implementation plan

1. **Phase 0 — contract + primitives (pure Rust, no GPU).** Finalize the `DrawList`/`DrawQuad`
   extensions for primitives (line/quad/SDF descriptors) and confirm the overlay models
   (`clock`, `scopes`, `meter`, `tally`, `safearea`, `identify`, `umd`) emit complete drawable
   values. Golden-frame unit tests on the CPU reference. *(Most models already exist.)*
2. **Phase 1 — CPU reference text + primitives.** Integrate `cosmic-text`/`swash` rasterization into
   the CPU compositor; blit premultiplied coverage into the `Rgba16Float` canvas via `blend::over`;
   evaluate primitive math on CPU. Golden-frame tests for labels, clocks, meters, markers, tally.
3. **Phase 2 — GPU overlay sub-pass (behind `wgpu`).** Persistent bounded atlas + sub-texture
   uploads; the batched glyph-quad + primitive sub-pass between composite and encode; SDF chrome.
   SSIM/PSNR-gated against the Phase-1 CPU oracle (ADR-I002).
4. **Phase 3 — dynamic-content efficiency.** Change-detection gating (revision/hash), dirty-region
   uploads, meter/scope conflation wiring (~10–30 Hz), atlas LRU eviction + growth. Bench against
   T1–T10; wire the perf-regression CI gate (ADR-E009).
5. **Phase 4 — subtitles.** SRT/VTT via `cosmic-text` on a worker into the latest-rendered slot;
   ASS via the off-by-default `libass` feature (premultiplied-as-is). Fidelity validation vs libass
   reference renders.
6. **Phase 5 — HDR + color.** sRGB→canvas-primaries linearization for overlays; sRGB→BT.2020 PQ/HLG
   when the program is HDR; ffprobe verify the output tag is unaffected.

---

## 9. Open questions / risks

1. **Compute-vs-render overlay sub-pass.** The composite/encode passes are compute today; do we add a
   third compute pass that reads+writes the storage `Rgba16Float` canvas (needs read access to the
   storage texture, or a ping-pong), or a small render pass into the same texture? Both keep the
   blend in linear light; the compute-with-ping-pong avoids a render pipeline but costs a canvas-sized
   copy. **Measure** which is cheaper on the target SKUs (iGPU bandwidth-bound).
2. **Subpixel-bin atlas growth.** `swash`'s 4×4 subpixel bins multiply unique glyph entries; for
   multiviewers with many distinct sizes/positions the atlas can grow. Validate the byte cap (T4)
   under realistic label diversity; consider snapping label baselines to integer pixels to collapse
   subpixel bins where antialiasing quality permits.
3. **`cosmic-text`/`glyphon` version pin.** The rustybuzz→harfrust swap (0.15) and 0.15→0.18 line
   change the shaping dep graph and may shift metrics slightly; pin via lockfile, re-run `cargo deny`,
   and re-bless golden frames on any bump.
4. **HDR overlay color.** sRGB→PQ/HLG overlay conversion must anchor white sensibly (a 100% sRGB white
   label at 203-nit reference white, not at peak) so labels are not blinding on HDR programs; reuse the
   per-tile tone-mapping anchor (ADR-C005). Needs a visual check.
5. **libass premultiplication boundary.** libass output is already premultiplied; the
   `swash`/`cosmic-text` path is straight coverage. The boundary between the two in a single atlas/draw
   list must be unambiguous (per-quad "already premultiplied" flag or separate sub-list) so we never
   double-premultiply. Encode this in the contract.
6. **Analog clock smoothness vs cost.** A sweeping second hand updates a primitive uniform every
   frame (cheap) but a stepped hand updates 1 Hz; expose as policy and default to stepped to honor
   change-detection (T2).
7. **Decode-at-display-res for scopes.** Scope sample extraction must sample the *tile* surface at a
   conflated rate, not force a full-res readback — confirm the engine exposes a cheap conflated sample
   tap (consistent with preview taps, ADR-P001) rather than a hot-path readback.

---

## Sources

- [Red Blob Games — *Distance field fonts* (raster atlas vs SDF/MSDF tradeoffs)](https://www.redblobgames.com/articles/sdf-fonts/)
- [`glyphon` on crates.io (0.11.0, 2026-04-13; requires `wgpu ^29`, `cosmic-text ^0.18`)](https://crates.io/crates/glyphon)
- [`glyphon` repository (architecture: cosmic-text + etagere + wgpu textured quads)](https://github.com/grovesNL/glyphon)
- [`cosmic-text` repository + docs (HarfRust shaping, swash rendering, SwashCache per-glyph cache)](https://github.com/pop-os/cosmic-text)
- [`SwashCache` docs (CacheKey: font/glyph/size/subpixel)](https://docs.rs/cosmic-text/latest/cosmic_text/struct.SwashCache.html)
- [mpv issue #7615 — osd-overlay perf regression on unchanged input (change-detection lesson)](https://github.com/mpv-player/mpv/issues/7615)
- Internal: [resilience-and-av §7](../research/resilience-and-av.md), [color-management §2,§5,§6](../research/color-management.md), [efficiency §2.2,§2.5,§2.6](../research/efficiency.md), [core-engine §8.2,§13](../research/core-engine.md); [ADR-R008](../decisions/ADR-R008.md), [ADR-R007](../decisions/ADR-R007.md), [ADR-E006](../decisions/ADR-E006.md), [ADR-C003](../decisions/ADR-C003.md), [ADR-E002](../decisions/ADR-E002.md), [ADR-R006](../decisions/ADR-R006.md), [ADR-I002](../decisions/ADR-I002.md).
