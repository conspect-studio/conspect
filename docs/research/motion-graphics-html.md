# Multiview — HTML/CSS/JS Motion Graphics — Engine, Templating/Data-Binding, and Render-to-VT

**Area:** Compositor / Overlays / Media (motion graphics from web technologies)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0080](../decisions/ADR-0080.md) (HTML/CSS/JS motion-graphics engine — offscreen GPU surface, engine isolation, efficiency budget), [ADR-0081](../decisions/ADR-0081.md) (templating / data-binding — window-variable injection API + optional template language; n-source graphics), [ADR-0082](../decisions/ADR-0082.md) (render-to-VT — bake an HTML graphic to a media asset)
**Extends:** [ADR-R008](../decisions/ADR-R008.md) (overlay layer stack + portable premultiplied-RGBA atlas/quad contract), [ADR-0016](../decisions/ADR-0016.md) (overlay renderer paths, one linear blend in our compositor), [ADR-0056](../decisions/ADR-0056.md) (keyers — fill/key, multi-box composition, on-clock vs off-thread split), [ADR-0058](../decisions/ADR-0058.md) (NV12+A alpha media payload), [media-playout](media-playout.md) (media library + players — the render-to-VT target), [url-input](url-input.md) (sibling brief — generic web/URL ingest, with which this brief shares a browser-engine cost discussion)
**Backlog:** `MGFX-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **The one mental model.** An **HTML graphic** is a self-contained bundle (HTML + CSS + JS + assets) rendered by an **embedded browser engine** off the engine thread into a **premultiplied RGBA surface**, which Multiview consumes through the **already-specified overlay/keyer design contracts** ([ADR-R008](../decisions/ADR-R008.md)/[ADR-0016](../decisions/ADR-0016.md)/[ADR-0056](../decisions/ADR-0056.md)/[ADR-0058](../decisions/ADR-0058.md), all Proposed — existing design dependencies this brief builds on) — never through the browser's own render pass, never on the output clock. The app feeds the graphic live data by **setting named `window` variables and calling an update hook** (an injection API), with an **optional Mustache-class template language with restricted built-in helpers** (Handlebars over the same injected data) as a convenience for authors. A graphic may declare **N source slots** so it can be used as a **wipe** (`Source1 → Source2`) or a **PIP + scorecard** (the graphic samples N program textures, exactly as the keyer multi-box composition already does). The same graphic can be **rendered once to an alpha media asset (render-to-VT)** so it plays back through the media player with zero browser cost. This is heavy, opt-in plumbing: a browser engine is a large dependency with a real CPU/GPU/RAM budget, so the whole subsystem rides an **off-by-default feature** and an explicit admission budget.

---

## 0. Headlines

1. **HTML graphics are a new *source/overlay/fill kind*, not a new compositor.** They render to a **premultiplied RGBA surface** off-thread and enter the canvas through one of the three contracts that already exist: the overlay portable atlas+quad ([ADR-R008](../decisions/ADR-R008.md)/[ADR-0016](../decisions/ADR-0016.md), monitoring-style, input-decoupled), or — when the graphic must be **frame-coordinated with switcher state and sample program video** — the keyer fill path as an **NV12+A** ([ADR-0058](../decisions/ADR-0058.md)) graphics fill consumed at the verified keyer insertion point ([ADR-0056](../decisions/ADR-0056.md)). We never let the browser blend into its own framebuffer (that would bypass invariants #5/#8 — the exact mistake [ADR-0016](../decisions/ADR-0016.md) rejected for `glyphon`).

2. **A browser engine is heavy — so it is opt-in and budgeted.** A full web engine (CEF/Chromium, or Servo) is the single largest dependency the product could pull in: hundreds of MB of binary, a multi-process sandbox or a large in-process surface, and per-instance CPU/GPU/RAM. It ships behind an **off-by-default `html-graphics` feature**, is **never in the default LGPL-clean build**, and every running graphic instance is **admission-accounted** ([ADR-E007](../decisions/ADR-E007.md)) like a decode/composite load (§6, [ADR-0080](../decisions/ADR-0080.md)).

3. **Isolation is non-negotiable (invariant #10).** The browser is a **producer**, the engine a **sampler**. The graphic publishes frames into a **lock-free latest-slot store** (the same `TileStore`/last-good substrate every source uses, [ADR-0058](../decisions/ADR-0058.md) shows the NV12+A payload riding it with zero framestore changes); the engine `read_at`s per tick and **never awaits the browser**. A wedged, slow, crashing, or GC-pausing graphic is indistinguishable from a stalled camera — the tile holds last-good and rides the state ladder (invariant #2). The browser cannot back-pressure, de-pace, or stall the output clock (invariant #1).

4. **Frame pacing via external BeginFrame, not the browser's wall clock.** Embedded engines expose an **external-BeginFrame / paint-on-demand** mode (web-verified 2026-06: CEF `SendExternalBeginFrame` replaces the browser's internal frame timer with host-issued BeginFrames; offscreen rendering surfaces frames through `OnPaint`/`OnAcceleratedPaint`). Multiview drives the graphic at the **output cadence as exact rationals** (invariant #3 — never float fps) so a graphic's CSS/rAF animation advances by tick, not by an independent timer that would beat against the program (§2.3). **External BeginFrame alone does *not* make a page deterministic** — JS timers (`setTimeout`/`setInterval`), async resource/font/network loads, layout, and `Math.random`/`Date.now` are not synchronised by it. Frame-exact bakes and production use therefore additionally require the **deterministic authoring subset** of §2.5 (controlled rAF timestamp, no network, pinned fonts/assets, no wall-clock/unseeded-random APIs, a paint-timeout policy, conformance-validated).

5. **Templating: present options, recommend the data-injection API + optional Handlebars — do not mandate.** The operator asked which is best. There are two distinct jobs: (a) **live per-frame data binding** (a name in a lower-third that changes, a score that ticks) and (b) **authoring-time structure** (build the DOM from a data record). The recommendation (§3) is a **`window`-variable injection API as the primary live-data path** (`window.mvData` + a `window.mvUpdate()` hook the app calls), with an **optional template language (Handlebars — a Mustache superset that adds a restricted set of built-in helpers; *not* logic-less — Mustache is the strictly logic-less one) as an author convenience** layered *on top of* the same injected data. Arbitrary custom Handlebars helpers are **banned** unless separately sandboxed and reviewed. Injection is the lower-level, always-available contract; the template engine is sugar, never a requirement.

6. **N-source graphics reuse the keyer multi-box composition — the graphic *samples* N program textures.** The operator's two examples — a **wipe** (`Source1 → Source2`) and a **PIP with scorecard** — are one mechanism: the graphic declares **named source slots**, Multiview binds each slot to a program/source texture, and the graphic composites them (with masks, transforms, a CSS `mix-blend` look) over its own DOM. This is the [ADR-0056](../decisions/ADR-0056.md) **multi-box composition** pattern (a background plus z-ordered boxes, rendered as a same-tick pre-pass), with the box geometry/animation authored in CSS/JS instead of the layout schema (§4, [ADR-0081](../decisions/ADR-0081.md)).

7. **Render-to-VT bakes a graphic to a media asset (alpha clip), then plays it with zero browser cost.** A deterministic graphic (no external live data, or a captured data take) is rendered **frame-by-frame via external BeginFrame** into an **NV12+A** clip and imported into the media library ([media-playout](media-playout.md)/[ADR-0057](../decisions/ADR-0057.md)). Playback is then an ordinary alpha media player — mezzanine-resident, frame-accurate, no browser on the hot path (§5, [ADR-0082](../decisions/ADR-0082.md)). "VT" = the operator's term for a video-tape/clip slot; here it is a media-player asset.

8. **Color order is honored exactly (invariant #8).** The browser emits **sRGB premultiplied RGBA** (its native compositor color space). Multiview treats that surface like any RGBA overlay/fill: linearize (sRGB EOTF) → blend in linear light → (for HDR programs) map to the canvas transfer/primaries → OETF → encode → tag. The browser's own sRGB framebuffer blend is **bypassed**; we take its surface, not its render pass — identical posture to [ADR-0016](../decisions/ADR-0016.md). Premultiplied vs straight alpha is declared once and never double-applied (§2.4).

9. **Two consumption tiers, by frame-coordination need.** **Monitoring tier** (clocks, bugs, lower-thirds that need no program-video sampling and tolerate drop-oldest): the overlay path, off-thread egress bake ([ADR-0016](../decisions/ADR-0016.md)) — cheapest, already input-decoupled. **Production tier** (graphics that must land on the same tick as a take, or that sample program video — wipes, PIP+scorecard, stinger-style transitions): the on-clock keyer/multi-box path ([ADR-0056](../decisions/ADR-0056.md)) fed an NV12+A fill ([ADR-0058](../decisions/ADR-0058.md)). The author picks the tier; the data contract is the same (§1.3).

10. **MVP is deliberately small.** One engine choice behind the feature; the injection API + N-source slots; the overlay (monitoring) consumption tier; render-to-VT for the deterministic case. The optional template language, the on-clock production tier for live (un-baked) program-sampling graphics, and HDR-native graphic authoring are phased (§7).

---

## 1. Why HTML graphics — and the honest cost

### 1.1 Why web technologies for motion graphics

Broadcast lower-thirds, tickers, scoreboards, full-screen transitions, and clocks are overwhelmingly authored as **HTML/CSS/JS** in practice: it is the most widely-known motion design surface (CSS transitions/animations, `<canvas>`/WebGL/WebGPU, SVG, web fonts), it decouples *design* from *engine* (a designer ships a bundle; the engine never recompiles), and it data-binds naturally (the DOM is already a data-driven view). The open-source **CasparCG HTML producer** is the canonical conceptual demonstration that this works for live television: it renders an HTML page through an embedded Chromium and mixes it into the channel. We cite it as a **conceptual reference only** — no code, design, or trademark is copied; Multiview's plumbing (NV12+A, the keyer insertion points, the output-clock isolation) is original and built from the open contracts already in this repo.

### 1.2 The cost (state it plainly)

A browser engine is the **heaviest plausible dependency** in the product:

- **Binary size + build:** CEF/Chromium is hundreds of MB of prebuilt binary and a non-trivial toolchain; Servo is a large Rust build. Either dwarfs the rest of the workspace.
- **Process/sandbox model:** Chromium is multi-process (browser + renderer + GPU process); a sandboxed embed adds IPC and lifecycle complexity. An in-process engine trades the sandbox for fault-blast-radius.
- **Per-instance runtime:** each live graphic is a full layout/style/paint/JS pipeline with its own heap, GC, and GPU surface — easily tens to hundreds of MB RAM and a CPU core's worth of work under animation, plus a GPU surface.
- **License:** CEF is BSD; Chromium pulls a large, mixed but redistributable license set; Servo is MPL-2.0. None is GPL-contaminating, but the **footprint is incompatible with the default LGPL-clean, no-native-dep build** ([conventions §7](../architecture/conventions.md)). This is **opt-in only**.

**Conclusion:** the feature is real and worth building, but it is **never default-on**, is **budgeted per instance**, and shares its cost discussion with the [url-input](url-input.md) sibling brief (generic web/URL ingest faces the same "a browser is heavy" tradeoff for a different reason).

### 1.3 The two consumption tiers (pick by need, not by default)

| Tier | When | Path | Frame-coordination | Cost posture |
|---|---|---|---|---|
| **Monitoring** | Clocks, bugs, lower-thirds, tickers; no program-video sampling; drop-oldest tolerable | Overlay portable RGBA atlas+quad ([ADR-R008](../decisions/ADR-R008.md)/[ADR-0016](../decisions/ADR-0016.md)), off-thread egress bake | Input-decoupled by design; a graphic frame may be dropped under overload | Cheapest; already isolated |
| **Production** | Must land on the take tick; samples program video (wipe / PIP+scorecard); stinger-style | Keyer fill ([ADR-0056](../decisions/ADR-0056.md)) as NV12+A ([ADR-0058](../decisions/ADR-0058.md)), or multi-box pre-pass | On-clock, frame-exact; survives/participates in transitions | One alpha-blended tile (or pre-pass) per tick, admission-accounted |
| **Baked (render-to-VT)** | Deterministic graphic or captured take; no live browser at playout | Render once to NV12+A clip → media player ([media-playout](media-playout.md)) | Frame-accurate playout (media player); zero browser on hot path | Browser cost paid once at bake; playout is ordinary media |

The **author selects the tier**; the **injection data contract (§3) and N-source contract (§4) are identical across tiers**. Render-to-VT (§5) is the recommended default for anything that *can* be deterministic, because it converts a recurring per-frame browser cost into a one-time bake.

---

## 2. Rendering-engine options + recommendation; isolation

### 2.1 Options (open engines only)

| Engine | License | Embedding posture | Offscreen + paint-on-demand | Notes |
|---|---|---|---|---|
| **CEF (Chromium Embedded Framework)** | BSD (CEF) over Chromium | Mature C/C++ embedding API; multi-process | **Yes** — offscreen rendering (OSR) via `OnPaint` (CPU buffer) / `OnAcceleratedPaint` (shared GPU texture); `SendExternalBeginFrame` for external frame pacing | Most battle-tested for broadcast (the CasparCG precedent). Heaviest binary; shared-texture OSR is most mature on Windows D3D11 (Multiview is Linux/macOS — see §2.2). (unverified: exact Linux/macOS accelerated-OSR maturity at our pinned version; verify at integration) |
| **Servo** | MPL-2.0 | Rust-native, embeddable WebView API (under active development); WebRender + `surfman` offscreen GPU surfaces | Partial / evolving — offscreen rendering and embedding are an active 2024–2025 workstream | Native Rust (no C++ multi-process bridge), aligns with the workspace; embedding API is younger and less broadcast-proven than CEF. `surfman` provides accelerated offscreen surfaces. |
| **WebKitGTK / WPE WebKit** | LGPL/BSD (WebKit) | GTK/WPE embedding; WPE is the embedded-focused port | Offscreen via FDO/EGL backends | Linux-centric; WPE is designed for set-top/embedded offscreen use. (unverified: macOS posture; license-footprint review needed) |
| **Wry/Tao (system WebView)** | MIT/Apache | Thin Rust wrapper over the OS WebView | Not designed for deterministic offscreen broadcast capture | **Rejected** for the render path: depends on the host OS WebView, no external-BeginFrame, no broadcast-grade offscreen capture. Fine for the *control UI*, not for graphics rendering. |

### 2.2 Recommendation

**Pin the engine behind a trait, default to CEF for the first shippable tier, and keep Servo as the Rust-native track.** Rationale:

- **CEF is the proven broadcast path** (the open CasparCG precedent) with a mature **external-BeginFrame + offscreen** contract — the two features the output-clock isolation (§2.3) depends on. It is the lowest-risk way to ship a *working* graphic.
- **Servo is the strategically aligned engine** (Rust-native, MPL-2.0, `surfman` offscreen GPU surfaces, no C++ multi-process bridge) and should be tracked as the second backend; it removes the largest foreign dependency if/when its embedding API and offscreen capture mature.
- **Abstract behind a `GraphicsEngine` trait** in a new feature-gated module so the engine is swappable and the rest of Multiview sees only "a producer that publishes premultiplied RGBA frames into a store." This mirrors the HAL/backend-registry posture (a backend behind a feature, [conventions §3/§4](../architecture/conventions.md)).

**Honest caveat:** accelerated offscreen rendering (`OnAcceleratedPaint` → a GPU shared texture, avoiding a GPU→CPU→GPU round trip) is most mature on Windows/D3D11, which Multiview does not target. On Linux/macOS the **MVP path is the CPU-surface readback** (`OnPaint` → a premultiplied RGBA buffer we upload), accepting the copy — bounded and budgeted exactly like the overlay upload bandwidth [ADR-0016](../decisions/ADR-0016.md) already accounts (~249 MB/s for a full 1080p RGBA layer; §6). The accelerated path is a per-platform optimization, gated behind capability detection, not the MVP contract. (This is the same "no stable wgpu external-texture import" reality [efficiency §3](efficiency.md) and [ADR-R008](../decisions/ADR-R008.md) already pinned.)

### 2.3 Isolation — invariant #10, in detail

The browser is a **best-effort producer that is physically incapable of back-pressuring the engine.**

- **Producer/sampler split.** The graphic engine runs on **its own thread(s)/process**, renders, and **publishes** the finished premultiplied surface into a **lock-free latest-slot store** — the same `TileStore<T>` substrate that holds last-good frames for every source (the NV12+A payload rides it "with zero framestore changes," [ADR-0058](../decisions/ADR-0058.md)). The engine `read_at`s the store per tick (`f(tick.pts)`), never the browser.
- **The engine never awaits the browser.** No lock is shared with the output-clock loop; the command/data channel into the graphic is **bounded drop-oldest** (the subtitle-seam / `AudioControlHandle` RCU precedent, [ADR-0059](../decisions/ADR-0059.md) §1). A graphic that GC-pauses, hangs a JS loop, crashes its renderer process, or simply renders slowly **holds last-good** (invariant #2) and rides LIVE→STALE→…→NO_SIGNAL; persistent loss drops the layer/keyer to its off-air-safe policy ([ADR-R011](../decisions/ADR-R011.md)).
- **External BeginFrame for cadence.** Multiview issues BeginFrame at the output cadence (exact rationals, invariant #3) so the graphic advances by tick. If a BeginFrame's paint does not arrive in time, the engine **does not wait** — it samples the last-good slot and the graphic catches up; the program never de-paces (invariant #1). The browser's internal frame timer is disabled (`SendExternalBeginFrame` semantics).
- **Crash containment.** A renderer-process crash (Chromium multi-process) is contained: the supervisor restarts the graphic in the background while the store holds last-good, honestly reporting `recovering` — the supervised-reconnect-bracket posture [media-playout §14](media-playout.md) applies to player threads, applied here to the graphic.
- **CI chaos gate.** The MP-1 wedge-a-consumer pattern ([ADR-0037](../decisions/ADR-0037.md), [ADR-0059](../decisions/ADR-0059.md)) extends to "wedge / kill / starve the graphic engine; the output clock never gaps and RSS stays bounded."

### 2.4 Color (invariant #8) and alpha

- The browser composites in **sRGB and emits premultiplied RGBA** (its native compositor output). Multiview consumes that surface exactly like the overlay RGBA atlas: **declare it premultiplied once**, linearize via the sRGB EOTF, blend in linear light, and — for HDR programs — map to the canvas transfer/primaries (sRGB→BT.2020 PQ/HLG, the [ADR-C005](../decisions/ADR-C005.md)/[ADR-0016](../decisions/ADR-0016.md) 203-nit anchor) before OETF/encode/tag.
- **We never use the browser's own render-to-framebuffer blend** (it blends in 8-bit sRGB, bypassing linear light and NV12-throughout — the precise [ADR-0016](../decisions/ADR-0016.md)/§Alternatives rejection). We take the surface and run it through *our* fixed pipeline.
- For the **production tier**, the RGBA surface is converted to **NV12+A** at the producer boundary (one off-thread conversion, off the hot path) so the keyer math consumes it at the verified insertion point with straight alpha premultiplied exactly once in linear light ([ADR-0058](../decisions/ADR-0058.md) Decision 2, [ADR-0056](../decisions/ADR-0056.md) §4). HDR-native graphic authoring (a graphic that wants to emit >8-bit / wide-gamut) is out of scope for v1 (the NV12+A mezzanine is 8-bit, [ADR-0058](../decisions/ADR-0058.md)); v1 graphics are sRGB and tone-mapped onto the canvas.

### 2.5 Deterministic authoring subset (required for frame-exact bakes / production tier)

External BeginFrame steps the renderer but does not make a web page deterministic on its own. A graphic that must be **frame-exact** — a render-to-VT bake (§5) or a live production-tier graphic whose per-tick visual state must be correct on the take tick — must conform to a constrained authoring subset, validated at import (a conformance check, akin to the [media-playout](media-playout.md) §4 import gate):

- **No network at render time** — all assets (fonts, images, data) packaged in the bundle; no outbound fetch (see §7.8 security default).
- **Pinned fonts/assets** loaded before the first BeginFrame; the renderer reports font-ready before frame 0 (no on-air font-load race, the [ADR-0016](../decisions/ADR-0016.md) determinism concern).
- **No wall-clock / unseeded-random APIs** for animation state: `Date.now`/`performance.now`/`Math.random` are shimmed to a seeded, BeginFrame-derived value so animation is a pure function of tick (master-clock-derived, invariant #3).
- **rAF timestamp is the host's per-BeginFrame time**, not a free-running clock; `setTimeout`/`setInterval` are discouraged for animation (they are not stepped by BeginFrame) — animation drives off rAF / CSS timelines only.
- **Paint-deadline / timeout policy.** Each BeginFrame has a bounded paint deadline. **For a live tier this never blocks the engine** — a missed deadline samples last-good (§2.3, invariant #1/#10); **for an off-line bake** (§5) a slow frame simply takes longer (no real-time pressure, no substitution). A page that persistently overruns the live deadline is flagged non-conformant for the production tier and steered to render-to-VT.

This subset is **advisory for the monitoring tier** (drop-oldest tolerant) and **required for the production tier and all bakes**. The exact shim/validation surface is pinned at implementation; the v1 conformance gate at minimum enforces "offline bundle, fonts pre-loaded, no network."

---

## 3. Templating / data-binding: window-variable injection vs handlebars

The operator asked which is best. **Two jobs, two answers.**

### 3.1 The two jobs

1. **Live data binding (per-take / per-frame):** the name in a lower-third changes, a clock ticks, a score increments. This is **runtime**, frequent, and must be cheap and isolation-safe.
2. **Authoring-time structure:** build the DOM/SVG once from a data record (a roster, a results table). This is **one-shot** and an *author convenience*, not a live path.

### 3.2 Option A — `window`-variable injection API (recommended primary)

Multiview injects a stable namespace into the page and calls a hook:

```js
// Multiview sets (and re-sets on each update):
window.mvData   = { lower_third: { name: "A. Operator", title: "Director" },
                    score: { home: 3, away: 1 } };
window.mvConfig = { canvas: { w: 1920, h: 1080 }, fps_num: 60000, fps_den: 1001 };

// The author implements this; Multiview calls it after every injection:
window.mvUpdate = function (data) {
  document.getElementById("name").textContent = data.lower_third.name;
  // ...drive CSS animations, canvas, etc.
};
```

- **Why primary:** it is the **lowest-level, always-available contract** — no template engine required, no opinion on how the author structures the DOM, and it maps directly onto the bounded-drop-oldest data seam (§3.4). It is the broadcast-standard pattern (a controlled global + an update callback) and is engine-agnostic (works identically on CEF or Servo). It also cleanly supports **animation control verbs** (`window.mvPlay()`/`mvNext()`/`mvStop()` for in/out animations) the switcher can call on a take.
- **Data delivery is structured JSON**, validated against an optional per-graphic **data schema** (§3.3) before injection, so a malformed update is rejected at the control plane, never crashes the page mid-air.

### 3.3 A per-graphic manifest (the contract)

Each graphic bundle declares a manifest (the desired-state resource, validated like every other config, [ADR-M012](../decisions/ADR-M012.md)):

```toml
# graphic.manifest (sketch — pinned in ADR-0081)
id = "lower-third-v2"
entry = "index.html"
canvas = { w = 1920, h = 1080 }      # native authoring size; tier decides scaling
fps = "60000/1001"                   # exact rational (inv #3)
[data]                               # optional JSON schema for window.mvData
  "$schema" = "https://json-schema.org/draft/2020-12/schema"
  type = "object"
  # ...properties validated before injection
[sources]                            # N source slots (§4)
  source1 = { role = "fill" }
  source2 = { role = "fill" }
[template]                           # OPTIONAL (§3.5)
  engine = "handlebars"              # or omitted (injection-only)
```

### 3.4 The data seam (isolation, invariant #10)

Live data flows through the **one bounded apply path** everything else uses: REST verb → a `Command` on the bounded bus (202 + operation id, [conventions §6](../architecture/conventions.md)) → the frame-boundary drain → a **bounded, conflated-latest-wins** RCU slot the graphic's producer thread reads between BeginFrames (the `AudioControlHandle`/subtitle-seam shape, [ADR-0059](../decisions/ADR-0059.md) §1). The engine never awaits the page; a slow page sees the **latest** data on its next paint, intermediate updates conflated. Data updates are **Class-1 hot** ([ADR-M012](../decisions/ADR-M012.md)) — a store write + a JS call, no engine reconfiguration.

### 3.5 Option B — optional template language (Handlebars, recommended as sugar)

A template engine builds HTML *from* a data record. The recommendation, if one is offered, is **Handlebars** — a **superset of Mustache** that adds a **restricted set of built-in helpers** (`if`/`unless`/`each`/`with`) authors actually need, and **precompiles** for runtime cheapness (web-verified 2026-06). Note Handlebars is **not** "logic-less" — Mustache is the strictly logic-less template language; Handlebars adds those helpers and *can* register arbitrary custom helpers, which we **ban** unless separately sandboxed and reviewed. It layers **on top of** the injected `window.mvData`: the page can either implement `mvUpdate` by hand (Option A) or `{{handlebars}}`-render a fragment from the same injected data.

- **Why offered:** authors building data-tables/rosters want declarative `{{#each}}` binding without writing imperative DOM code.
- **Why optional, never mandatory:** restricted-helper templates cannot express animation timing, easing, or canvas/WebGL — those are CSS/JS regardless. Forcing a template engine would constrain authors and add a dependency for graphics that do not need it. **Injection is the contract; Handlebars is a convenience the author may ignore.**
- **Rejected as mandatory:** a heavier framework with an arbitrary-logic runtime (full JS template frameworks, React/Vue runtimes) — unnecessary weight inside an already-heavy browser, and they re-implement what the DOM + `mvUpdate` already do.

### 3.6 Recommendation (do not mandate)

**Ship the `window`-injection API as the primary, always-available live-data contract; offer Handlebars (Mustache-superset, restricted built-in helpers only, precompiled) as an opt-in authoring convenience declared in the manifest.** Authors who want zero template engine get a clean injection contract; authors who want declarative binding get Handlebars over the same data. The choice is per-graphic, reversible, and defaulted to injection-only. (Decision: [ADR-0081](../decisions/ADR-0081.md).)

---

## 4. N-source graphics — wipe; PIP + scorecard (the graphic samples N program textures)

The operator's two examples are **one mechanism**: a graphic that declares **named source slots** and composites the **program/source textures** Multiview binds to those slots over its own DOM.

### 4.1 The contract

- The manifest declares `[sources]` slots (`source1`, `source2`, …), each with a role (`fill`, `background`, `inset`). At use time the operator/switcher **binds each slot to a bus or source** (PGM, PVW, a camera, a media player) — exactly the bus-selection the switcher already does for keyer fills and the multi-box composition ([ADR-0056](../decisions/ADR-0056.md) §5).
- Multiview makes each bound texture **available to the graphic** as a sampled image. Two delivery shapes (pinned in [ADR-0081](../decisions/ADR-0081.md)):
  1. **Engine-side composition (recommended for production tier):** the graphic does **not** receive raw video pixels in JS. Instead, the graphic's DOM declares **placeholder boxes** (with stable ids/CSS rects), and Multiview composites the bound source textures **into those rects in our compositor**, on-clock, as the [ADR-0056](../decisions/ADR-0056.md) multi-box pre-pass — the graphic supplies geometry/animation/masks, the engine supplies pixels. This keeps program video **out of the browser** (no per-frame video upload into the page, no NV12→RGBA→NV12 round trip), honoring invariant #5.

  > **Frame-exactness bound (the extraction contract).** "Frame-exact, on-clock" applies to what the **engine** evaluates as `f(tick)`: the transition/wipe **progress** is driven by the transition engine ([ADR-0055](../decisions/ADR-0055.md)), and the placeholder-rect geometry + alpha mask are read from a **bounded per-frame manifest** the browser publishes alongside its surface (rect transforms, a coverage/alpha-mask reference, transition progress) — evaluated by Multiview on the output clock, *not* read back from arbitrary browser layout per tick. If the browser's published manifest/surface for tick `k` is late, the engine samples the **last-good** manifest+surface (invariant #2/#10); the program never de-paces, but a web-authored *look* that changes shape every frame can be one tick stale — so genuinely frame-critical mask/geometry animation belongs in render-to-VT (§5), where every frame is exact. For v1, the manifest is intentionally narrow (rectangular placeholder transforms + a baked/static alpha mask + scalar transition progress); arbitrary per-tick CSS-mask extraction into compositor primitives is **out of scope** (open question §7.9). This is the deterministic extraction contract the critical review asked for: the engine never enters the page, and it never needs arbitrary browser layout — only the bounded manifest.
  2. **In-page sampling (monitoring tier / baked only):** the bound texture is delivered to the page as a frame the JS can draw (`<canvas>`/`<video>`-like). Heavier (per-frame upload into the browser) and only sane for the monitoring tier or a render-to-VT bake; **not** the production live path.

### 4.2 Wipe (`Source1 → Source2`)

A wipe graphic declares two `fill` slots and authors the **transition mask/geometry in CSS/JS** (an animated clip-path, a diagonal, a logo-shaped reveal). Bound to `source1 = PGM`, `source2 = PVW`, driven by a take, it is a **graphical wipe transition** — the switcher's transition engine ([ADR-0055](../decisions/ADR-0055.md)) drives progress as `f(tick)`; the graphic provides the wipe's *look*. This is the [ADR-0056](../decisions/ADR-0056.md) USK/transition pattern with a web-authored mask instead of an SDF wipe pattern. (The stinger-style full-canvas alpha-clip transition remains the [ADR-0055](../decisions/ADR-0055.md)/[ADR-0058](../decisions/ADR-0058.md) path; a *live* web wipe is the un-baked variant, production tier.)

### 4.3 PIP + scorecard

A PIP+scorecard graphic declares one `inset` slot (the PIP feed) plus a DOM scorecard bound to live data (§3). Multiview composites the bound source into the inset rect (engine-side, §4.1.1); the scorecard chrome + numbers are the DOM, data-bound through injection. The whole thing is **one graphic source** the switcher selects on a bus, tallied recursively (sources inside it are PROGRAM-contributing, the [ADR-0056](../decisions/ADR-0056.md) §5 recursive tally) — exactly the multi-box composition contract, with web-authored chrome.

### 4.4 Why this reuses [ADR-0056](../decisions/ADR-0056.md), not a new path

The multi-box composition ([ADR-0056](../decisions/ADR-0056.md) §5) is *already* "a background plus overlapping, z-ordered, sized/cropped boxes, usable as one source on any bus, rendered as a drive-internal pre-pass in the same tick." An N-source HTML graphic is that, with the box geometry/animation authored in CSS/JS and a DOM chrome layer on top. We **extend the composition model to accept an HTML chrome layer + web-authored box geometry**, not invent a second compositor. Config gets the same **cycle check** (a graphic cannot bind a slot to a bus that contains itself, [ADR-M012](../decisions/ADR-M012.md)/[ADR-0056](../decisions/ADR-0056.md) §5).

---

## 5. Render-to-VT — bake an HTML graphic to a media asset

The operator: *"maybe allow render-to-video so it can go into a VT."* Yes — and it is the **recommended default for any deterministic graphic**, because it converts a recurring per-frame browser cost into a one-time bake.

### 5.1 What it does

A **deterministic** graphic (no live external data, or a **captured data take** — the operator fills in the lower-third text, hits "bake") is rendered **off-line, frame-by-frame** via external BeginFrame at the output cadence into an **NV12+A** sequence, then imported into the **media library** as an alpha clip ([media-playout](media-playout.md) §4 import pipeline, [ADR-0057](../decisions/ADR-0057.md)). Playback is then an **ordinary alpha media player** — mezzanine-resident, frame-accurate, zero browser on the hot path ([ADR-0058](../decisions/ADR-0058.md) §4/§6).

### 5.2 Why bake (the efficiency case)

- **The browser cost is paid once.** A 5 s 1080p60 lower-third baked to NV12+A is ~300 frames × 5 184 000 B ≈ **1.55 GB** un-trimmed, or a bounded mezzanine after trim/cap ([ADR-0058](../decisions/ADR-0058.md) §6.3 caps + decode-ahead ring) — and at playout it is a pooled `Arc` publish per frame, **no JS, no GC, no GPU layout, and no decode on the hot path**. (Any decode of an encoded mezzanine — §5.3 lists `qtrle`/ProRes 4444/PNG sequence — happens at *import/preload* into the resident NV12+A mezzanine, not per tick at playout; for mezzanines too large to keep fully resident it is the decode-ahead ring, [ADR-0058](../decisions/ADR-0058.md) §6.3, never a synchronous hot-path decode.) A recurring on-air graphic that does not change between shows should be baked.
- **Determinism + repeatability.** A baked take is byte-identical every play; no risk of a font load race, a slow data fetch, or a JS error on air ([ADR-0016](../decisions/ADR-0016.md) cites the broadcast-graphics determinism concern).
- **Frame-accuracy comes for free** from the media player's output-anchored timeline + frame-accurate start ([media-playout](media-playout.md) §7.2/§7.3) and play-on-take.

### 5.3 The bake pipeline

1. **Resolve** the graphic + a frozen data snapshot (or "no live data") + duration in **integer frames** (exact rationals, invariant #3).
2. **Render** off-line on a control-plane task (never an engine thread): issue BeginFrame k, await paint k, capture the premultiplied RGBA surface, convert to **NV12+A straight alpha** ([ADR-0058](../decisions/ADR-0058.md)) — the render is paced by *paint completion*, not wall clock, so a slow frame just takes longer to bake (no real-time pressure off-line).
3. **Encode** to an intra alpha mezzanine format the import pipeline accepts (`qtrle` / ProRes 4444 / PNG sequence — the [ADR-0058](../decisions/ADR-0058.md) §5 / [media-playout](media-playout.md) §5 allowlist; never HEVC-alpha, which FFmpeg decodes opaque).
4. **Import** through the media library's probe/policy/decode-verify pipeline ([media-playout](media-playout.md) §4), which also runs **stinger conform** (canvas-resolution + output-cadence equality) so a baked transition is take-ready.
5. **Play** as an ordinary alpha media player / DSK fill — done.

### 5.4 Boundaries

- Render-to-VT is a **control-plane operation** (202 + operation id, progress on the realtime stream); it **never touches the engine** ([media-playout](media-playout.md) §4 import posture).
- A graphic with **genuinely live, unbakeable data** (a real-time score that updates on-air) stays a **live** graphic (monitoring or production tier, §1.3) — render-to-VT is for the deterministic case. A common middle path: bake the **chrome/animation** and inject only the live numbers over the played-back clip via a thin DOM overlay (phased, §7).
- Decision: [ADR-0082](../decisions/ADR-0082.md).

---

## 6. Efficiency budget / licensing (standing review)

### 6.1 Memory / CPU / GPU / IO

| Cost | Where it lands | Budget posture |
|---|---|---|
| Browser binary + base process | Disk + RSS at first graphic | Hundreds of MB; **off-by-default feature**, loaded only when a graphic exists. Single shared engine process where the backend supports multiplexing; else per-instance. |
| Per live graphic: layout/style/paint/JS | Graphic thread/process (off-hot-path) | Tens–hundreds of MB RAM + up to ~1 CPU core under animation + a GPU surface; **each instance admission-accounted** ([ADR-E007](../decisions/ADR-E007.md)) as a composite-class load; a hard cap on concurrent live graphics. |
| Surface readback (CPU-surface MVP) | Producer thread → upload | Full 1080p RGBA ≈ 8.3 MB/frame, ≈ 249 MB/s @30 — the [ADR-0016](../decisions/ADR-0016.md)/[efficiency §2.2](efficiency.md) overlay bandwidth figure; **dirty-region/partial-paint** (`OnAcceleratedPaint` partial-update / OSR damage rects) reduces it; accelerated shared-texture OSR (where available) removes the round trip. |
| RGBA→NV12+A conversion (production tier) | Producer thread (off-hot-path) | One off-thread convert per published frame; the engine only ever samples the store. |
| Data injection | Control plane → bounded RCU slot | Operator-rate JSON; conflated latest-wins; never paces anything ([ADR-0059](../decisions/ADR-0059.md) seam). |
| On-clock composite of a graphic fill | Engine tick | +1 alpha-blended tile (NV12+A: +1 R8 fetch + 1 multiply/covered px, [ADR-0058](../decisions/ADR-0058.md) §6) or one multi-box pre-pass ([ADR-0056](../decisions/ADR-0056.md) §8); admission-accounted, shed before program tiles (invariant #9). |
| Render-to-VT bake | Control plane, off-line | Browser cost paid once; bounded parallelism; 202 + progress; zero engine impact. Playout is pooled `Arc` publish (no decode). |

**The flagship efficiency move is render-to-VT (§5):** prefer baking to a media asset over running a live browser whenever the graphic is deterministic — it removes the browser from the hot path entirely.

### 6.2 Licensing

- **Off-by-default `html-graphics` feature.** The default build stays **LGPL-clean with no native dep** ([conventions §7](../architecture/conventions.md)); the browser engine is opt-in, exactly as `ndi`/`gpl-codecs` are.
- **Engine licenses are redistributable but heavy:** CEF (BSD) over Chromium (mixed-but-redistributable), Servo (MPL-2.0), WebKit ports (LGPL/BSD). **None is GPL-contaminating**, but the binary footprint and any bundled fonts/codecs need a `cargo deny` + NOTICE pass at integration; **we never vendor a proprietary engine**.
- **CasparCG is cited as a conceptual reference only** — no code/design/trademark copied; the plumbing is original ([CODE_OF_CONDUCT](../../CODE_OF_CONDUCT.md)).
- Web fonts/assets shipped *in* a graphic bundle are the **author's** licensing responsibility; Multiview bundles an OFL fallback font for the failure path only ([ADR-0016](../decisions/ADR-0016.md)).

### 6.3 Invariant ledger (explicit)

- **#1 (output clock):** the browser is sampled (`read_at`), never pacing; external BeginFrame advances animation by tick; a late/slow/crashed graphic holds last-good — the clock never de-paces.
- **#10 (isolation):** browser on its own thread/process; bounded drop-oldest data + command seams; the engine never awaits the page; CI chaos gate (wedge/kill/starve the graphic).
- **#5 (NV12-throughout):** production-tier graphics are NV12+A fills ([ADR-0058](../decisions/ADR-0058.md)); program video is composited **engine-side into placeholder rects**, never round-tripped through the page (§4.1.1); no per-tile RGBA video.
- **#8 (color order):** browser sRGB premultiplied surface → our fixed linear-light pipeline; the browser's own framebuffer blend is bypassed (§2.4).
- **#6 (decode-at-display-res):** a graphic renders at its target canvas size; bound source textures are decoded at display size as usual; no oversized graphic surface.
- **#3 (timing):** cadence + bake durations are exact rationals; never float fps.
- **#11 (live-apply):** spawning/retiring a live graphic and binding source slots are classified (data updates Class-1 hot; engine-layout-affecting binds may be Class-2, [ADR-M012](../decisions/ADR-M012.md)/[ADR-R004](../decisions/ADR-R004.md)).
- **IPv6-first:** a graphic that fetches assets/data over the network uses IPv6-first URLs (bracketed literals, [ADR-0042](../decisions/ADR-0042.md)); the control surface binds dual-stack `[::]`.

---

## 7. Open questions

1. **Engine pin.** CEF-first (proven, heavy, accelerated-OSR weakest on our platforms) vs Servo-first (Rust-native, MPL-2.0, embedding API younger). The brief recommends **CEF for the first shippable tier behind a `GraphicsEngine` trait, Servo tracked as the second backend** — but the pin should be re-validated against current (2026) Servo embedding/offscreen maturity at integration. (unverified: exact Linux/macOS accelerated-OSR state for either engine.)
2. **In-process vs sandboxed multi-process.** Chromium's multi-process sandbox bounds a renderer crash (good for isolation) but adds IPC + lifecycle weight; an in-process engine is lighter but a crash is more dangerous. Which default? (Leaning multi-process for blast-radius, accepting the weight.)
3. **Accelerated-OSR / zero-copy on Linux/macOS.** Can we get `OnAcceleratedPaint`-equivalent shared-texture capture into wgpu on our platforms, or is the CPU-surface readback the durable MVP (consistent with "no stable wgpu external-texture import," [efficiency §3](efficiency.md))? Measure the readback budget on an iGPU before committing the accelerated path.
4. **Production-tier *live* program-sampling.** The clean design composites bound source textures **engine-side** into placeholder rects (§4.1.1), keeping video out of the browser. Is there a graphic that genuinely needs **per-pixel program video inside JS** (a custom WebGL shader over the feed) badly enough to justify the per-frame upload into the page? If so it is render-to-VT or monitoring-tier only, never the live production path.
5. **Template language pin.** Handlebars is the recommendation for the optional sugar — but is even *that* worth shipping in v1, or is injection-only sufficient until authors ask? (Leaning: ship injection-only in MVP; add Handlebars when demanded.)
6. **Live-data over a baked clip.** The "bake the chrome, inject the live numbers over the played-back clip" middle path (§5.4) is attractive but couples a media player and a thin live graphic on one canvas region — design it as a follow-up once both halves ship.
7. **Resource caps.** What is the hard cap on concurrent live graphics, and how does it interact with the GPU placement engine ([gpu-placement-engine](gpu-placement-engine.md)) when a graphic's GPU surface competes with decode/composite? Needs a measured budget.
8. **Security/sandbox of author bundles — default policy (now), full ADR (before remote graphics).** An HTML bundle runs arbitrary JS, so the architectural posture is fixed **now**, not deferred to implementation: **offline packaged bundles only; no filesystem access outside the bundle; no outbound network unless a graphic is explicitly per-graphic allowlisted; no ambient/host credentials reachable from the page; a CSP enforced on the bundle; arbitrary Handlebars custom helpers banned (§3.5).** **Remote-loaded graphics are out of scope** until a separate security ADR pins origin model, asset packaging, font loading, credential isolation, and the allowlist mechanism. Open: the exact CSP, the allowlist grammar, and the air-gapped-facility profile.

9. **Per-tick mask/geometry extraction depth.** v1's engine-side extraction contract (§4.1.1) is deliberately narrow — rectangular placeholder transforms + a static/baked alpha mask + scalar transition progress in the per-frame manifest. Is there demand for **arbitrary per-tick CSS-mask → compositor-primitive** extraction (a clip-path animated every frame, evaluated frame-exact engine-side)? If so it needs a bounded, deterministic layout-readback contract or it is render-to-VT only (where every frame is exact). Leaning: narrow manifest in v1, render-to-VT for frame-critical web-authored masks.

10. **Producer isolation: thread vs process.** Open question 2 (in-process vs multi-process) and the §2.3 crash-containment posture imply a choice of isolation granularity for the producer (a thread sharing the host address space vs a sandboxed child process). Pin the default (leaning multi-process for blast-radius) and the supervisor restart bracket at integration.

---

## 8. References (open/published only)

- **CEF (Chromium Embedded Framework)** documentation — offscreen rendering (`OnPaint`/`OnAcceleratedPaint`), `SendExternalBeginFrame` external frame pacing, shared-texture OSR (BSD/Chromium). (web-verified 2026-06.)
- **CasparCG** HTML producer (open project) — conceptual reference for live HTML broadcast graphics via an embedded browser. Cited conceptually only. (web-verified 2026-06.)
- **Servo** project — embeddable Rust web engine, WebView API, WebRender, `surfman` accelerated offscreen surfaces (MPL-2.0). (web-verified 2026-06.)
- **Handlebars / Mustache** — Mustache is the strictly logic-less template language; Handlebars is a Mustache superset that adds built-in helpers (`if`/`each`/`unless`/`with`) plus optional arbitrary custom helpers (banned here unless sandboxed/reviewed) and precompilation — so Handlebars is *not* logic-less. (web-verified 2026-06.)
- **WPE WebKit / WebKitGTK** — embedded-focused WebKit ports with offscreen backends. (unverified: macOS posture, version specifics — verify at integration.)
- This repo: [ADR-R008](../decisions/ADR-R008.md), [ADR-0016](../decisions/ADR-0016.md) (overlay contracts), [ADR-0056](../decisions/ADR-0056.md) (keyers/multi-box), [ADR-0058](../decisions/ADR-0058.md) (NV12+A), [media-playout](media-playout.md) (render-to-VT target), [efficiency](efficiency.md) (bandwidth + no-external-texture-import reality).
</content>
</invoke>
