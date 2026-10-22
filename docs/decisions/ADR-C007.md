# ADR-C007: Transition and keyer color law — linear-light premultiplied `over` stays the only blend domain; optional perceptual progress curve; pinned keyer threshold domains; HDR canvas behavior

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-11
- **Source brief:** [production-switcher.md](../research/production-switcher.md)
  (color foundations: [color-management.md](../research/color-management.md))
- **Builds on:** [ADR-C003](ADR-C003.md) (composite in linear light with
  premultiplied alpha, inv #8 step 5), [ADR-C004](ADR-C004.md) (range exactly
  once, expand on input), [ADR-C005](ADR-C005.md) (per-tile BT.2390 HDR→SDR
  tone-mapping in linear), [ADR-C006](ADR-C006.md) (always tag output)
- **Relates to:** [ADR-0055](ADR-0055.md) (transition engine — progress =
  f(tick)), [ADR-0056](ADR-0056.md) (keyers — insertion points),
  [ADR-T015](ADR-T015.md) (exact-rational durations/progress),
  [ADR-T003](ADR-T003.md) (never float fps), [ADR-E002](ADR-E002.md)
  (NV12-throughout)

## Context

The production-switcher layer (ADR-0055/ADR-0056) adds dissolves, dips,
fade-to-black, and keyers on top of the one blend the product has: Porter-Duff
**source-over of premultiplied alpha in linear light**
(`crates/multiview-compositor/src/blend.rs:103-118`), applied identically by
the CPU reference (`fold_tile_into_band`,
`crates/multiview-compositor/src/pipeline.rs:1139-1147`) and the wgpu kernel
(step 5, `crates/multiview-compositor/src/gpu/shaders/composite.wgsl:131-135`).
That domain is load-bearing (ADR-C003, inv #8): linear-light compositing is the
only physically correct law for scaling, edges, and mixed-gamut tiles.

Legacy switching practice, however, mixed **gamma-encoded code values**
directly — de-facto industry practice from the analog/SDI era, never a
standard. The two laws differ visibly during a dissolve:

- **Pacing.** With linear weights, a fade from black at progress `t` displays
  roughly `t^(1/2.4)` of full code value on a BT.1886 display — three-quarters
  "in" at `t = 0.5`. Linear-light fades look front-loaded (pop in early,
  linger at the tail) next to a code-value fade.
- **Midpoint.** A 50 % linear mix conserves light, so a content↔content
  dissolve holds mid-transition screen brightness slightly *higher* than the
  gamma-domain mix, whose mid-dissolve dip (each side at code-value weight 0.5
  sums to less light than either endpoint) is the look operators grew up with.

Separately, keyer math (ADR-0056) needs its threshold domains pinned —
clip/gain luma keys and chroma-distance keys each have a "right" space — and
the HDR canvas modes (PQ/HLG transfer functions are implemented in both
kernels, `crates/multiview-compositor/src/transfer.rs:206-225`) need defined
transition behavior.

The existing verification harness this ADR pins everything to (verified):

- **CPU oracle, byte-exact:** the production tile-driven kernel must stay
  byte-identical to the preserved reference kernel for every case, including a
  property test over random tile stacks
  (`crates/multiview-compositor/tests/composite_tile_driven.rs:5-8`, `:78-79`,
  `:222`); the LUT'd transfer path is gated against the analytic reference
  (`tests/lut_vs_reference.rs`).
- **GPU vs CPU, SSIM/PSNR (never bit-exact):** Y **and** UV SSIM ≥ **0.98**,
  PSNR ≥ **40 dB** against the CPU oracle
  (`crates/multiview-compositor/tests/gpu_compositor.rs:164-167`); the GPU
  overlay image blit carries its own 0.98 / 38 dB floor
  (`tests/overlay_gpu_dispatch.rs:272-273`).

## Decision

### 1. One blend domain: linear-light premultiplied `over` — for everything

Every transition (mix, dip, wipe softness, FTB) and every keyer composites in
the **existing** linear-light premultiplied `over` — the only blend domain in
the product, on both kernels. No gamma-domain blend path is added, ever.
Transition and keyer math reduce to *weights and alphas fed into the existing
operator*; the inv #8 step order (range-expand → matrix → EOTF → primaries →
**blend in linear** → OETF → encode → tag) is untouched, and ADR-C003/C004
remain fully in force.

### 2. Optional perceptual progress curve — applied to `t`, never to pixels

Each transition (and FTB) carries a `progress_law`:

- **`linear`** (default): blend weights are the raw progress —
  physically-linear light mix.
- **`perceptual`** (opt-in): the *scalar* progress is remapped before it
  becomes a blend weight. The law: **a content side's linear weight is the
  `γ`-power of its gamma-domain (legacy) weight**, `γ = 12/5` (the BT.1886
  exponent 2.4, stored as an exact rational), oriented per transition shape:

  ```
  fade-in from a flat field:            w(t) = t^γ            (incoming content)
  fade-out to a flat field (incl. FTB): w(t) = 1 − (1 − t)^γ  (cover weight)
  content ↔ content mix (one knob):     w(t) = t^γ / (t^γ + (1 − t)^γ)
  ```

  The fade forms reproduce a legacy code-value fade **exactly** on a
  power-law (BT.1886) canvas: displayed output is `t·A′` / `(1−t)·A′`. A dip
  is two flat-field legs (out to the dip color, in from it), so **dips and
  FTB are exact** too. The mix form is the single-knob compromise — a
  symmetric, monotonic curve (`w(0)=0`, `w(1)=1`, `w(1−t)=1−w(t)`,
  `w(0.5)=0.5`) that tracks each side's power law near that side's endpoint
  and is an **approximation** mid-dissolve.

The mapping is evaluated **once per tick on the progress scalar** in the
render-plan resolver (ADR-0054/ADR-0055) — *not* per pixel, *not* in the
kernels. Both compositor kernels are byte-for-byte unaffected by the law; CPU
and GPU agree trivially because they receive the same weight. Timing stays
exact: `t` is the exact-rational `frames_elapsed / duration_frames`
(ADR-T015, inv #3); the curve output feeds the same `f32` opacity field tiles
already carry, so floats appear only where opacity is already a float.

**Documented operator-visible difference (required in the operator docs and
the in-app help):** with `linear`, mid-dissolve brightness is physically
correct and slightly higher than legacy practice, and fades run perceptually
front-loaded; with `perceptual`, fades, dips, and FTB match legacy
code-value fades exactly (power-law canvas), while a content↔content
dissolve's *per-pixel* midpoint still differs from a true gamma-domain mix
(a single weight summing to 1 cannot reproduce the gamma mix's mid-dissolve
luminance dip) — the residual difference is inherent to keeping one blend
domain and is the accepted cost. Operators wanting the full legacy
mid-transition look use a dip. Default is `linear` (off).

### 3. Keyer threshold domains (pinned for ADR-0056)

- **Luma key clip/gain operate on post-range-expand normalized Y** — the
  code-value luma after step 1 (`range::expand_luma`,
  `crates/multiview-compositor/src/range.rs:49`; `pipeline.rs:101-104`;
  `composite.wgsl:113`), *before* the matrix. Rationale: operator-familiar
  (clip/gain against the source's own levels is de-facto industry practice),
  range-independent (limited and full sources normalize to the same [0,1]
  scale, ADR-C004), and cheapest (no extra conversion). Formula per ADR-0056:
  `a = clamp((y_norm − clip) × gain, 0, 1)`, optional invert.
- **Chroma key distance is computed in linear RGB after the YUV→RGB matrix
  conversion + EOTF — in the tile's own gamut** (between inv #8 steps 3
  and 4: after `pipeline.rs:110-114` / `composite.wgsl:120-126`, before the
  primaries convert). Linear RGB because code-value/YUV distances bend with
  the transfer curve and matrix (green-screen tolerance would vary with scene
  brightness); **tile gamut** (not canvas gamut) so the key is a property of
  the *source* — the same source keys identically on a BT.709 and a BT.2020
  canvas, since a 3×3 primaries transform does not preserve the distance
  metric. The operator's reference color is converted once per frame (CPU
  side, into the tile's linear space) and passed as a uniform; the kernels add
  only a per-pixel distance + soft-threshold. Chroma keying is post-MVP
  (ADR-0056); the domain is pinned now so the kernels are extended once.
- **Linear/alpha keys** consume the key plane (fill+key pair's Y, or the
  NV12+A alpha plane per [ADR-0058](ADR-0058.md)) as normalized alpha
  directly; clip/gain are available as trims in the same normalized space.
- Key alpha multiplies into the tile's straight alpha **before** the
  premultiply + `over` (the verified insertion point,
  `pipeline.rs:1139-1147` / `composite.wgsl:131-135`) — after the blend would
  be wrong (double-counted coverage) and is forbidden.

### 4. Kernels stay pinned by the existing gates

Every transition/keyer kernel change (key-alpha sampling, garbage-matte clip,
wipe masks) lands **CPU-reference-first** and extends the verified gates with
keyed/transition test patterns:

1. the tile-driven production kernel stays **byte-identical** to the CPU
   oracle (`composite_tile_driven.rs` pattern, incl. the property test);
2. the GPU path matches the CPU oracle at **SSIM ≥ 0.98 / PSNR ≥ 40 dB on Y
   and UV** (`gpu_compositor.rs:164-167` floors, GPU-tagged runners);
3. mid-transition frames at pinned progress values (`t ∈ {0, 1/4, 1/2, 3/4,
   1}` as exact rationals) become golden frames on the CPU path, so a future
   "optimization" cannot silently move the dissolve law.

One law, two kernels, one oracle — no per-domain forks to gate.

### 5. HDR canvas (PQ/HLG) behavior under mixes, dips, and FTB

- The working space is unchanged: the linear `Rgba16Float` canvas (ADR-C003)
  is the single intermediate for SDR and HDR canvases; PQ/HLG EOTF/OETF exist
  in both kernels (`transfer.rs:206-225`). Mixes on an HDR canvas blend the
  same linear-light values — nothing transition-specific is added to the
  color pipeline.
- **Per-tile tone-mapping is unaffected by transitions** ([ADR-C005](ADR-C005.md)):
  HDR→SDR (and SDR-on-HDR placement) happens per tile, upstream of the blend,
  exactly as for a static composite; a dissolve never re-tone-maps or changes
  a tile's mapping mid-transition (no pumping tied to progress).
- **Dip-to-color and FTB targets are defined in canvas space:** the operator
  specifies the dip/black color against the canvas (canvas-transfer code
  values), converted through the canvas EOTF into a linear canvas-gamut color
  once at arm time, then blended like any source. "Black" is code-value black
  = linear 0 on SDR, PQ, and HLG alike, so FTB is well-defined on every
  canvas.
- The `perceptual` progress curve keeps the fixed `γ = 12/5` exponent on HDR
  canvases. The gamma-domain reference it approximates is SDR-era practice
  and has no PQ/HLG equivalent; on HDR canvases the curve is documented as a
  pacing preference only (still symmetric/monotonic, endpoints exact), and
  `linear` remains the default and the recommendation.

### Invariant posture

Inv #8 is intact: the order is unmodified, range is still handled exactly once
(ADR-C004), and the output is still tagged + verified (ADR-C006). Inv #1/#10:
the progress law is evaluated at the frame-boundary control seam as pure
`f(tick)` — transitions are *sampled* per tick, never pacing, and no color
decision ever awaits a client. Inv #3: progress and durations are exact
rationals/integer frames; the only float is the opacity scalar the kernels
already take.

## Rationale

- **One blend domain is the whole point of ADR-C003.** A second (gamma)
  domain would double the kernel surface (CPU + GPU × 2 domains), reintroduce
  the dark-fringing/wrong-midtone artifacts on edges and scaled content that
  linear compositing exists to remove, and make wipes/keys (which blend at
  soft edges every frame, not just mid-transition) inconsistent with tiles.
- **A progress curve buys most of the legacy look for none of the cost.** The
  dominant operator-visible artifact of linear dissolves is pacing, and pacing
  is a property of the scalar weight — fixable outside the kernels, exactly
  reproducing legacy fades/dips on SDR. The residual per-pixel midpoint
  difference is small, honest, and documented.
- **Threshold domains follow the data.** Clip/gain on normalized code-value Y
  matches both operator muscle memory and the kernel's step-1 value that is
  already in hand; chroma distance is only stable in linear RGB, and only
  source-stable in the tile's own gamut.

## Alternatives considered

- **Gamma-domain pixel blending for transitions** (mix code values like
  legacy practice). Rejected: violates ADR-C003's single-blend-domain
  decision, splits both kernels and the whole test matrix (oracle + SSIM gates
  per domain), and is visibly wrong for everything *else* the blend does
  (scaling, soft edges, mixed-gamut tiles) — the legacy look is approximated
  via the progress curve instead.
- **Per-transition blend-domain switch** (operator chooses linear or gamma
  per transition). Rejected: kernel/test matrix explosion (every keyer × wipe
  × domain combination needs oracle + GPU gates), path-dependent pictures
  (ADR-0055's flat-list vs scene-pre-render paths must render identically,
  which a pixel-domain switch breaks), and an invitation to inconsistent
  house styles.
- **Per-side dual-weight perceptual law** (incoming `t^γ`, outgoing
  `(1−t)^γ`, reproducing the gamma mid-dissolve luminance dip on the
  scene-pre-render path). Rejected for v1: the flat-list fast path has a
  single opacity knob, so the two ADR-0055 mix paths would render *visibly
  differently* depending on an internal optimization choice — the
  single-scalar symmetric curve renders identically on both. Recorded as a
  possible future `progress_law` variant gated on pre-render-only.
- **Applying the perceptual curve in-shader / per pixel.** Rejected: it is a
  scalar function of `t`; per-pixel evaluation changes kernels for zero
  benefit and adds a CPU/GPU transcendental-parity risk the SSIM gate would
  have to absorb.
- **Keyer thresholds in canvas-gamut linear or in YCbCr code values.**
  Rejected: canvas-gamut distances make a key's tolerance depend on the
  output configuration; YCbCr/code-value distances bend with transfer and
  matrix, making green-screen tolerance scene-brightness-dependent.

## Consequences

- **Positive:** one blend law, one CPU oracle, one GPU gate — transitions and
  keyers extend the verified pipeline instead of forking it; legacy-feeling
  fades/dips/FTB are available per transition with zero kernel cost; keyer
  domains are pinned before any keyer code exists, so ADR-0056's MVP and the
  post-MVP chroma/pattern keys target the same math.
- **Negative / cost:** the default look differs from legacy gamma-domain
  switchers mid-dissolve (and, until `perceptual` is selected, in fade
  pacing) — a real, documented operator-facing difference that support and
  docs must own; `progress_law` is one more per-transition parameter
  (schema/API/UI surface, [ADR-M012](ADR-M012.md)/[ADR-W021](ADR-W021.md));
  golden mid-transition frames add CPU CI time (bounded: a handful of pinned
  `t` values per transition type).
- **Verification debt accepted:** the `perceptual` law's "matches legacy"
  claim is exact for flat-field fades/dips/FTB on power-law canvases; the
  content↔content mix compromise is validated qualitatively (side-by-side
  operator review) rather than by a numeric gate, and the docs say so. The
  curve forms themselves are pinned by unit tests on the resolver (exact
  values at rational `t`), independent of the kernels.
