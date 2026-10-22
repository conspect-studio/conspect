# Multiview — URL as an Input: Headless Web Page Source with JS Payload, Refresh & Conditional Reload

**Area:** Input / Config / Compositor-adjacent (synthetic-source ingest path) + Control / Web
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0083](../decisions/ADR-0083.md) (URL-as-input `SourceKind` — headless web page source: JS-on-load / on-page-event, interval refresh, conditional refresh on page-text).
**Extends:** [motion-graphics-html.md](motion-graphics-html.md) + [ADR-0080](../decisions/ADR-0080.md) (the shared headless render engine — **the overlay surface**; this brief is its **source-side twin**), [ADR-0027](../decisions/ADR-0027.md) (synthetic sources are first-class `SourceKind`s through the one uniform ingest path), [clock-timer-sources.md](clock-timer-sources.md) (the in-process generator-loop precedent), [efficiency.md](efficiency.md) (decode/render-at-display-resolution, bounded queues, admission ladder), [object-detection-ai.md](object-detection-ai.md) (the read-only sampled-consumer pattern reused for DOM-text polling), [managed-devices.md](managed-devices.md) (the vendor-neutral / nominative-only posture).
**Backlog:** `URLIN-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> The operator asked for a **URL as an input**: point Multiview at a web page, place it in a tile (or as a whole canvas), **execute a JavaScript payload on load or on a page event**, **refresh on an interval**, and **simply refresh if / if-not some text is present on the page**. This is a *source* — a live picture that flows through the exact same ingest→last-good-store→compositor path as an RTSP camera or a colour-bars generator — and it is the **source-side twin** of the motion-graphics overlay ([ADR-0080](../decisions/ADR-0080.md)): same headless render engine, different *placement*. An overlay draws transparent graphics *on top of* the composited canvas; a **web-page source** is content that fills a tile. This brief designs the `SourceKind`, the JS-injection model, the refresh strategies, and proves the design cannot touch the output clock (#1) or back-pressure the engine (#10). Everything here is "should / would / proposed."

---

## 0. Headlines

1. **A web page is a `SourceKind`, not a special case.** `SourceKind::WebPage { url, … }` joins `Bars`/`Solid`/`Clock`/`Timer`/`Rtsp`/… as a first-class, internally-tagged variant on the existing serde union (`crates/multiview-config/src/schema.rs:217`). Downstream of ingest **nothing knows or cares** that a tile is a web page — it publishes `Nv12Image` frames into its per-tile last-good store exactly like every other kind ([ADR-0027](../decisions/ADR-0027.md)). The *only* new `match kind` is "render via the headless engine vs synthesize-in-process vs open+decode via libav."

2. **It shares the ADR-0080 headless engine — it does not fork it.** The motion-graphics overlay and the web-page source are **two placements of one renderer**: a feature-gated, sandboxed headless browser driven over an open instrumentation protocol (Chrome DevTools Protocol — CDP — verified below), rendering at the **target display resolution** (#6) and handing frames to NV12. The overlay composites with alpha *over* program; the source fills *a tile*. The engine, its launch flags, its crash-supervision, its licence posture, and its efficiency budget are **owned by [ADR-0080](../decisions/ADR-0080.md)**; this brief adds the *source ingest wiring* and the *refresh / JS / conditional* control surface on top.

3. **JS payload, two triggers — on-load and on-page-event.** The operator can attach a script that runs **once per navigation, before page scripts** (CDP `Page.addScriptToEvaluateOnNewDocument` — pre-document injection), and/or **on demand against the live page** (CDP `Runtime.evaluate`) driven by a *page event* (a DOM mutation, a console signal, a custom `window` event the payload itself raises). The payload is operator-authored content, sandboxed and resource-bounded; it **never** runs on the output-clock thread.

4. **Refresh, two laws — interval and conditional.** **Interval:** re-navigate (CDP `Page.reload` / `Page.navigate`) every *N* seconds — a wall-clock cadence sampled off-engine, never a frame gate. **Conditional:** poll the page's visible text off-engine (CDP `Runtime.evaluate` of `document.body.innerText`, or a `DOM` snapshot) and reload **if** a string/regex is present, or **if-not** present, on a bounded poll interval with hysteresis. The condition is the operator's literal ask: *"simple refresh if / if-not text on page."* Polling is a **read-only sampled consumer** (the object-detection tap pattern, [object-detection-ai.md §2](object-detection-ai.md)) — a slow/stuck page yields a stale frame, never a stall.

5. **Invariants are structural, not best-effort.** The render runs in its **own process** (the browser child) and its own task; it publishes frames into a lock-free single-slot store (`crates/multiview-framestore/src/latest.rs:75`); the compositor reads *latest-or-placeholder* (`crates/multiview-framestore/src/tile.rs:371`) every tick regardless. A frozen page, a runaway script, a navigation hang, or a crashed browser child rides the **tile state machine** (LIVE→STALE→RECONNECTING→NO_SIGNAL, invariant #2) — the output clock never waits for it (#1) and it can never back-pressure the engine (#10).

6. **Vendor-neutral, open-standard, licence-gated.** The instrumentation protocol (CDP) and the offscreen-render technique are open and documented; we drive *a* Blink/Chromium-class browser the operator supplies at runtime, named nominatively, **never bundled** — the same posture [ADR-0080](../decisions/ADR-0080.md) sets and the ZowieTek/Cast precedent in [managed-devices.md](managed-devices.md) uses. The browser is a heavy, off-by-default dependency behind a Cargo feature; the default build stays LGPL-clean and GPU-free.

---

## 1. A web page as a `SourceKind` (shares the ADR-0080 engine)

### 1.1 The seam we extend

`SourceKind` is an internally-tagged serde union — `#[serde(tag = "kind", rename_all = "snake_case")]`, `#[non_exhaustive]` — at `crates/multiview-config/src/schema.rs:217`. Adding a variant is **additive** (schema stays back-compat, the established pattern for `Timer` and `Clock`):

```toml
[[source]]
id    = "scoreboard"
[source.kind]
kind  = "web_page"
url   = "https://[2001:db8::20]/scoreboard?compact=1"   # IPv6-first; bracket the literal
viewport = { width = 1280, height = 720 }                # rendered at display-res (#6)
# JS payload + refresh policy — §2, §3
```

Downstream this is the **synthetic-source ingest precedent made concrete**: [ADR-0027](../decisions/ADR-0027.md) already established that bars/solid/clock are produced *in-process* by a generator that is a *peer of a decode thread* and publishes into the per-tile store each tick. The clock/timer brief shipped that loop — `generator_loop` (`crates/multiview-cli/src/synth.rs:913`), `render` (`crates/multiview-cli/src/synth.rs:208`), `SyntheticKind::from_source_kind` (`crates/multiview-cli/src/synth.rs:115`), re-rendering only when the displayed content changes (`render_key`, `synth.rs:367`). **A web-page source is the same shape with one difference: the per-frame pixels come from the headless render engine instead of a pure-Rust band fill.** The publish target is identical: `TileStore::publish_arc` into the lock-free `LatestSlot` (`crates/multiview-framestore/src/latest.rs:75`).

### 1.2 Source vs overlay — the load-bearing distinction (KEY NOTE)

This is **a distinct surface from [motion-graphics-html.md](motion-graphics-html.md)**, and the distinction must stay crisp to avoid duplicating shipped work:

| | Motion-graphics ([ADR-0080](../decisions/ADR-0080.md)) | Web-page source (this brief, [ADR-0083](../decisions/ADR-0083.md)) |
|---|---|---|
| **Role** | **Overlay** — transparent graphics drawn *over* the composited canvas | **Source** — opaque content that fills *a tile* (or the whole canvas) |
| **Pixel model** | NV12 **+ alpha** (straight/premultiplied per the overlay path) | NV12 (opaque); a web page's background is the tile's content |
| **Placement** | the overlay/DSK stage, post-composite | the layout grid, an input like any other |
| **Lifecycle** | follows the program; one per program | one per source instance; many can run |
| **Live-apply** | overlay show/hide, Class-1 | source add/remove/swap — Class-1 add, Class-2 only if the canvas topology changes ([ADR-R004](../decisions/ADR-R004.md)) |
| **Shared** | the **headless render engine, launch flags, sandbox, crash-supervision, licence posture, efficiency budget** ([ADR-0080](../decisions/ADR-0080.md)) | — same engine — |

The engine is **one implementation, two callers.** ADR-0080 owns the renderer crate/module; this brief consumes its `render_to_nv12(url|html, viewport, instant) -> Nv12Image` surface from the source ingest path and adds the source-only concerns (interval/conditional refresh, the JS-on-event trigger wired to *this* page instance). Cross-link both ways; do not re-specify the renderer here.

### 1.3 Capture technique (verified)

The headless browser is driven over the **Chrome DevTools Protocol** ([ChromeDevTools/devtools-protocol](https://chromedevtools.github.io/devtools-protocol/)). Two frame-capture modes exist and we choose between them deliberately:

- **`Page.startScreencast`** — event-driven, fires `Page.screencastFrame` per produced frame, each acknowledged with `Page.screencastFrameAck` (the ACK is producer flow-control — frames are throttled until the prior is acked). It is a DevTools **debugging-capture** path: each frame is a **compressed image** (`jpeg`/`png`, base64-encoded), not an NV12/raw video surface. (Verified: [devtools-protocol issue #17](https://github.com/ChromeDevTools/devtools-protocol/issues/17), [headless-dev screencast thread](https://groups.google.com/a/chromium.org/g/headless-dev/c/6XKLTi5bsZA).) **Candidate** live-capture implementation, not a verified architectural fact; a true offscreen-render surface owned by [ADR-0080](../decisions/ADR-0080.md) (CEF/OSR-style) is the alternative and may be preferred.
- **`Page.captureScreenshot`** — a single high-latency snapshot (reported **7–25 s** in pathological cases; [devtools-protocol issue #28](https://github.com/ChromeDevTools/devtools-protocol/issues/28)). **Rejected** for the live path — far too slow and bursty for per-tick sampling; usable only as a one-shot fallback for a static page.

**Pinned: a screencast-style frame stream as the live capture, sampled by the source's publish loop** — with the *exact* capture surface (CDP screencast vs an ADR-0080 offscreen-render surface) deferred to a validation spike (Open question §1; the spike gates frame cadence, image-decode + base64 cost, ACK flow-control behaviour, and idle-frame semantics before implementation). The capture delivers a frame stream into a bounded **drop-oldest** ring; the source's generator-loop peer takes *latest-or-last-good* each output tick and publishes NV12. A page that produces no new frames simply re-publishes the last-good (re-stamped from the tick counter, #3) — exactly the clock-source `render_key` "republish cached frame" behaviour (`synth.rs` §). **Cost budget (must be carried, not assumed cheap):** if the capture is CDP screencast, every frame is a base64-encoded compressed image that must be base64-decoded → image-decoded (jpeg/png) → color-converted to NV12; that per-frame decode + conversion can dominate CPU and is budgeted in the efficiency ladder like every other input — it is **not** a free sample-and-publish. An ADR-0080 offscreen-render surface that already yields raw frames avoids that decode and is the lower-cost path if available. (unverified) Where the browser uses a GPU offscreen path, the **vendor/GPU boundary copy to NV12 is budgeted** like every other input — there is no cross-vendor on-GPU zero-copy ([efficiency.md](efficiency.md)).

> Note on a known footgun (verified, context only): some pipelines virtualise the browser's clock to force deterministic frame production ([Replit, "Browsers don't want to be cameras"](https://blog.replit.com/browsers-dont-want-to-be-cameras)). Multiview does **not** need determinism — it *samples* a live page like any other live input (#1: inputs are sampled, never pacing) — so we take frames as the screencast produces them and hold-last-good between them. We do **not** drive the page from the output clock.

---

## 2. JS payload: execute on load + on page events

The operator wants to **"execute a javascript payload on load or page event."** Two distinct, composable triggers, both off-engine:

### 2.1 On load — pre-document injection (CDP `Page.addScriptToEvaluateOnNewDocument`)

A registered script runs **once per navigation, before any page script**, on every document the target creates ([`Page.addScriptToEvaluateOnNewDocument`](https://chromedevtools.github.io/devtools-protocol/tot/Page/#method-addScriptToEvaluateOnNewDocument), whose `worldName` parameter selects an isolated world for execution — verified, a Page-domain method). Use it for: hiding chrome/cookie banners, forcing a layout/zoom, stripping animations, setting auth via an injected token, or installing the *page-event hooks* of §2.2. Config:

```toml
[source.kind.script]
on_load = """
  document.documentElement.style.background = '#000';
  for (const el of document.querySelectorAll('.cookie-banner')) el.remove();
"""
isolated_world = true   # run in a separate JS world; default true (least interference)
```

### 2.2 On page event — `Runtime.evaluate` driven by a signal

A second payload runs **in response to a page event**: a DOM mutation the operator cares about, a `console.log` sentinel, or a custom `window` event the on-load hook raises. Mechanism: the on-load script (§2.1) installs a `MutationObserver` / event listener that, on firing, emits a CDP-observable signal (`Runtime.consoleAPICalled`, or a binding via `Runtime.addBinding`); the off-engine **page-control task** observes it and runs the operator's `on_event` payload via `Runtime.evaluate` against the live page. This keeps the *decision* in operator JS (expressive) and the *action* on the controlled CDP channel (bounded, supervised).

```toml
[source.kind.script]
on_event = { when = "binding:scoreReady", run = "window.__mv_settle()" }
```

### 2.3 Isolation & safety posture for operator JS

- **The payload is operator content, sandboxed.** It runs inside the browser child process (its own address space), in an isolated JS world where possible, under the browser's site-isolation sandbox. It has **no synchronous engine/control path** — it cannot block, await, or back-pressure the engine. It *can* still consume the browser child's own CPU/GPU/RAM, drive network requests, and emit CDP events; those are contained by the child's resource ceilings (below) and by treating the CDP event stream as a **bounded drop-oldest, coalesced** consumer (per-source rate limit on console/binding/mutation signals) so an event flood degrades only its own tile, never the off-engine page-control task's liveness.
- **Resource-bounded.** Per-source CPU/RAM ceilings on the browser child (cgroup/rlimit at launch, owned by [ADR-0080](../decisions/ADR-0080.md)); a runaway script is *the child's* problem and is killed+restarted by supervision (§4), surfacing as a tile going STALE→NO_SIGNAL — **never** an engine hang.
- **Errors are sampled, not propagated.** A throwing payload, a CDP timeout, or a navigation failure is logged ([observability-logging.md](observability-logging.md)) and rides the tile state machine; it does not `?`-propagate into any data-plane path.
- **Secrets via reference, never plaintext.** Auth tokens injected by `on_load` come from the existing `SourceAuth { secret_ref }` pointer (`crates/multiview-config/src/schema.rs:182`), resolved at launch — never written into config or logs.
- **No arbitrary local file/`file://` access** by default; the page is a **network** source (IPv6-first URLs, bracketed literals). `file://` and local data are an explicit, validation-gated opt-in.

---

## 3. Refresh: interval + conditional (refresh if / if-not text present on page)

Both refresh laws run on the **off-engine page-control task** — a sampled consumer, never a frame gate.

### 3.1 Interval refresh

Re-load the page every *N* seconds on a wall-clock cadence (CDP `Page.reload`, or `Page.navigate` for a full reset). The cadence is sampled off-engine; the *rendered* frame stream is unaffected during reload — the source **holds last-good** across the navigation gap (#2), so the tile shows the previous frame (badged STALE if the reload exceeds the staleness threshold), then snaps to the new render. Integer-seconds cadence; never a float (#3 discipline). Example:

```toml
[source.kind.refresh]
interval_seconds = 30      # re-load every 30 s; 0 disables
hard_navigate    = false   # false = Page.reload (warm); true = Page.navigate (cold)
```

### 3.2 Conditional refresh — *if / if-not text on page*

The operator's literal ask. Poll the page's **visible text** off-engine and reload on a text condition:

- **Sense:** every `poll_seconds`, run `Runtime.evaluate` of `document.body.innerText` (or a CSS-selector-scoped `.innerText`) on the live page — the standard text-extraction read; the `DOM` domain's snapshot is the alternative for structured reads ([CDP DOM domain](https://chromedevtools.github.io/devtools-protocol/tot/DOM/), verified). *(unverified: `innerText` reflects rendered/visible text and is the pragmatic choice over `textContent` which includes hidden nodes.)*
- **Match:** test the extracted text against an operator pattern — a plain substring or a regex. **Bounded:** the extracted text is capped at a `max_text_bytes` ceiling (truncate-and-flag on overflow); the regex is compiled by a linear-time engine (Rust `regex`, no catastrophic backtracking) with a compiled-size/complexity limit; each match runs under a wall-clock timeout. A hostile/heavy rule degrades only *this* off-engine poll task (a missed poll), never the output clock (#1) or the engine (#10).
- **Act:** `refresh_if` (reload **when the text appears**) or `refresh_if_not` (reload **when the text is absent**). Exactly one of the two per rule; both can be expressed as a small rule list.
- **Hysteresis / debounce:** a `min_interval_seconds` floor and an edge-trigger (reload on the *transition* into the matching state, not every poll while it holds) prevent a reload storm — the same sense→decide-with-hysteresis discipline the degradation loop uses (#9).

```toml
[[source.kind.refresh.text_rules]]
selector  = "#status"          # optional; default = whole body
if        = "OFFLINE"          # substring or /regex/; reload WHEN present
min_interval_seconds = 10

[[source.kind.refresh.text_rules]]
if_not    = "LIVE"             # reload WHEN this text is ABSENT
poll_seconds = 5
```

**Why off-engine + drop-oldest:** the text poll is a **read-only sampled consumer** modelled on the detection tap ([object-detection-ai.md §2](object-detection-ai.md), [ADR-P001](../decisions/ADR-P001.md)): it reads a page that *already exists*, on a bounded interval, and a slow/stuck `Runtime.evaluate` (or a wedged page) just *misses a poll* — it never blocks anything. The decision to reload is enqueued onto the same bounded page-control command channel as the interval timer; the engine is never in the loop.

### 3.3 Precedence & interaction

Interval and conditional refresh compose: a conditional reload **resets** the interval timer (no double-reload race); a reload in flight **coalesces** further reload requests (latest-wins, drop-oldest) so a flapping condition cannot queue unbounded navigations. All of this lives in the page-control task's small state machine — bounded, off-engine, testable as a pure unit.

---

## 4. Isolation & efficiency

### 4.1 Invariant #1 — the output clock is untouchable

The output clock emits one valid frame per tick forever, independent of this source. The web-page render, the JS payloads, and both refresh laws run on **the browser child process + an off-engine page-control task** — *never* the output-clock thread. The compositor reads the tile's last-good store every tick (`crates/multiview-framestore/src/tile.rs:371`); if the page is mid-reload, frozen, slow, or its browser child is dead, the read returns **last-good or the placeholder slate** — the tick still fires, on time. `out_pts = f(tick)` is untouched. **Inputs are sampled; this one is no exception.**

### 4.2 Invariant #10 — no back-pressure on the engine

There is **no channel from this source into the engine that a slow page can fill.** The render→NV12 hand-off is a publish into a lock-free single-slot store (`crates/multiview-framestore/src/latest.rs:75`) — wait-free, overwrite-latest, the engine never awaits the producer. The screencast→sample ring and the page-control command channel are **bounded drop-oldest**. A crashed/hung browser child is reaped by a **supervisor** (the [ADR-0027](../decisions/ADR-0027.md)/input supervised-reconnect precedent: backoff-restart the child, tile rides RECONNECTING) — the failure is contained to the tile, exactly like a dropped RTSP camera. This is the same structural isolation [object-detection-ai.md](object-detection-ai.md) and the recorder ([ADR-0037](../decisions/ADR-0037.md)) rely on.

### 4.3 Invariant #6 — render at display resolution

The browser viewport is set to the **tile's displayed pixel size**, not some fixed 1080p that is then downscaled. A 320×180 scoreboard tile renders at ~320×180; the cost is budgeted in rendered megapixels/sec like every decoded input ([efficiency.md §2.1](efficiency.md)). On a layout change that resizes the tile, the viewport is re-set (a cheap CDP `Emulation.setDeviceMetricsOverride`) — a hot, Class-1 change where the canvas topology is unchanged.

### 4.4 Color (#8)

The browser renders sRGB; the NV12 conversion enters the fixed color pipeline at the normal place (range-expand → matrix → linearize → … → tag output). The source is **tagged** like any input; no reordering. (unverified) Wide-gamut/HDR web content is out of scope for v1 — the page is treated as sRGB/BT.709 and tagged as such; an HDR web source is a future axis owned by [color-management.md](color-management.md) if demand appears.

### 4.5 Efficiency budget (mem / cpu / gpu / io)

- **Memory:** one browser child per web-page source (heavy: tens–hundreds of MiB resident — the dominant cost and the reason this is a deliberate, bounded feature, not a free one). The frame ring is bounded (depth 1–3 drop-oldest NV12 at tile res). Per-source RAM ceiling enforced at launch; over-ceiling → kill+restart, tile rides the state machine. **No unbounded buffers anywhere.**
- **CPU:** the browser's own render/JS threads (sandboxed, cgroup-capped) dominate; the Multiview-side per-tick cost is a *sample-and-publish* (a pointer swap + an occasional NV12 copy when a new frame arrived) — cheap, the clock-source precedent. The text poll is one `Runtime.evaluate` per `poll_seconds` — seconds-scale, negligible.
- **GPU:** if the browser uses GPU compositing, it is a **GPU co-tenant** subject to the placement engine / degradation ladder (#9) like inference ([object-detection-ai.md §6](object-detection-ai.md)) — it is shed cheapest-impact-first before program output is touched. A software-rendered browser is the GPU-free fallback (CPU cost only).
- **IO:** the page fetches its own resources over the network (IPv6-first; bracket literals; honour the secret-ref auth). No disk write on the data plane. Logs are rate-limited ([observability-logging.md](observability-logging.md)).
- **Cost when unused:** **zero** — no web-page source declared ⇒ no browser child, no feature code on the hot path (the feature is Cargo-gated and off by default).

### 4.6 Networking — IPv6-first

URLs are IPv6-first; IPv6 literals are **bracketed** (`https://[2001:db8::20]/…`); the browser child inherits the host's dual-stack resolution. IPv4 targets are legacy-interop only ([ipv6-first.md](ipv6-first.md), [conventions §10](../architecture/conventions.md)). No new listening socket is introduced by this source (it is a *client* of the target page).

### 4.7 Vendor / licence posture

The headless browser is **operator-supplied at runtime, never bundled or vendored** — Multiview drives *a* CDP-speaking Blink/Chromium-class browser the operator installs, named nominatively and factually, with a non-affiliation stance mirroring the ZowieTek/Cast driver posture ([managed-devices.md](managed-devices.md)). The CDP wire shapes are implemented from the **open, published** protocol ([devtools-protocol](https://chromedevtools.github.io/devtools-protocol/)) — we redistribute no vendor SDK and no vendor spec text. The whole capability is behind an **off-by-default Cargo feature** (shared with [ADR-0080](../decisions/ADR-0080.md)); the default build stays LGPL-clean, GPU-free, and browser-free. Browser licensing (e.g. the operator's chosen Chromium build) is the operator's to satisfy — flagged, deferred to the operator's proprietary-stance call.

---

## 5. Open questions

1. **Which browser, and how is it discovered?** Driving "a CDP-speaking browser the operator supplies" needs a discovery/launch contract (binary path, version floor, the launch flags for headless+offscreen+sandbox). This is **owned by [ADR-0080](../decisions/ADR-0080.md)**; this brief assumes its answer. If ADR-0080 instead embeds a specific engine (e.g. a Servo/WebRender-class pure-Rust renderer to avoid the heavyweight browser), the JS-payload and text-poll surface here must be re-checked against that engine's scripting/inspection API — a Rust-native engine may not speak CDP. **Flag: the §2/§3 mechanisms are CDP-specific; if the shared engine is not CDP, re-map them.**

2. **`innerText` vs a structured DOM read for the text condition.** `innerText` is pragmatic but (unverified) layout-dependent and excludes some hidden content; a `DOM.getDocument`/snapshot read is more precise but heavier and its node-ids churn on mutation ([chromedp #762](https://github.com/chromedp/chromedp/issues/762)). Default `innerText`; offer a selector scope; revisit if operators need structured matches (e.g. attribute values).

3. **Audio from a web page.** A page can play audio (HTML5 `<audio>`/`<video>`, WebAudio). v1 is **video-only** — the page's audio is not captured. Capturing it would route through the browser's audio sink into `multiview-audio`'s per-source path; designed but out of scope here. Named, not silently dropped.

4. **Authenticated / interactive pages.** Beyond an injected token, some pages need a login flow or cookies. v1 supports `on_load` token injection + a secret-ref; a full cookie-jar / session-persistence model is deferred. OAuth/redirect flows are explicitly out of scope.

5. **ToS / legal caveat.** Rendering and capturing third-party web pages may violate a site's terms of service or copyright. This is an **operator responsibility**; the design neither encourages nor polices it. Flagged for the operator's call; docs must carry the caveat vendor-neutrally.

6. **Determinism / time-virtualisation.** We deliberately do *not* virtualise the page clock (§1.3) — we sample. If a future use-case needs frame-accurate, deterministic web video (e.g. a scripted lower-third animation timed to program), that is the **overlay/motion-graphics** surface's problem ([ADR-0080](../decisions/ADR-0080.md)), not the live-source surface's. Keep the split.

7. **Crash-storm bound.** A page that reliably crashes the browser child on load would restart-storm. The supervisor needs a circuit-breaker (after K failures in a window, park the source in NO_SIGNAL and warn, stop restarting) — the recorder/back-off precedent ([ADR-0037](../decisions/ADR-0037.md)). Pinned as a requirement; exact K/window TBD at implementation.

---

### Cross-references

- **Overlay twin:** [motion-graphics-html.md](motion-graphics-html.md) + [ADR-0080](../decisions/ADR-0080.md) — same engine, the overlay placement.
- **Ingest precedent:** [ADR-0027](../decisions/ADR-0027.md) (synthetic sources first-class) + [clock-timer-sources.md](clock-timer-sources.md) (the generator-loop in `crates/multiview-cli/src/synth.rs`).
- **Sampled-consumer pattern:** [object-detection-ai.md](object-detection-ai.md) + [ADR-P001](../decisions/ADR-P001.md) (read-only, drop-oldest, shed-first).
- **Invariants:** [ADR-T001](../decisions/ADR-T001.md) (#1 output clock), [ADR-RT004](../decisions/ADR-RT004.md) (#10 isolation), [ADR-R004](../decisions/ADR-R004.md) (#11 Class-1/Class-2), [efficiency.md](efficiency.md) (#6/#9).
- **Decision:** [ADR-0083](../decisions/ADR-0083.md).
</content>
</invoke>
