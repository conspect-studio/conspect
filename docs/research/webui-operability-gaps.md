# Multiview — WebUI Operability Gaps: Ubiquitous Preview, Live-Apply-Everywhere, Re-Assess Hardware, Selectable Tracks, Subtitle Toggle

**Area:** Web/API stack ↔ control ↔ engine (operability — cross-cutting: preview / live-apply / placement / routing / overlays)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0090](../decisions/ADR-0090.md) (ubiquitous non-disruptive preview), [ADR-0091](../decisions/ADR-0091.md) (live-apply-everywhere + staging + pending banner + high-risk gate), [ADR-0092](../decisions/ADR-0092.md) (operator-triggered hardware re-assessment with interrupt confirmation), [ADR-0093](../decisions/ADR-0093.md) (typed track/stream selection replacing free-text), [ADR-0094](../decisions/ADR-0094.md) (subtitle layer enable/disable in the layout editor).
**Extends:** [preview-subsystem.md](preview-subsystem.md) ([ADR-P002](../decisions/ADR-P002.md)/[ADR-P003](../decisions/ADR-P003.md)), [decoupled-routing.md](decoupled-routing.md) (`StreamInventory`), [self-aware-placement.md](self-aware-placement.md) ([ADR-0035](../decisions/ADR-0035.md)), [web-api-stack.md](web-api-stack.md); live-apply ([ADR-M005](../decisions/ADR-M005.md)/[ADR-M012](../decisions/ADR-M012.md)/[ADR-W018](../decisions/ADR-W018.md)/[ADR-W022](../decisions/ADR-W022.md)/[ADR-R004](../decisions/ADR-R004.md)/[ADR-R010](../decisions/ADR-R010.md)); tracks/subtitles ([ADR-M004](../decisions/ADR-M004.md)/[ADR-0036](../decisions/ADR-0036.md)/[ADR-R007](../decisions/ADR-R007.md)/[ADR-0019](../decisions/ADR-0019.md)/[ADR-W004](../decisions/ADR-W004.md)).
**Backlog:** `WUI-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> The product's north star is **WebUI-manages-everything**: every standard multiviewer/switcher
> capability is operable from the browser, and the operator never drops to a config file, an SSH
> session, or a daemon restart to make a change. The operator's 2026-06-13 intake names five places
> that promise still leaks: you cannot preview a stream wherever it is referenced; some changes still
> require download-edit-restart instead of applying live; there is no way to ask the system to
> re-assess and re-place hardware on demand; tracks are typed as free text; and the layout editor
> cannot toggle subtitles. This brief audits each gap against the as-built seams, proposes a design
> grounded in shipped machinery (preview taps, the `X-Multiview-Apply` header, the `/plan` classifier,
> the placement controller, `StreamInventory`, the per-source caption selector), and avoids
> re-deciding anything already settled. It is **vendor-neutral** and **docs-only**; nothing here is
> implemented.

---

## 0. Headlines

1. **Preview must be everywhere a stream is named, and never disrupt the page you are on.** Today
   preview is a real, isolated side-channel ([preview-subsystem.md](preview-subsystem.md)) with built
   endpoints (`GET /api/v1/preview/program.jpg`, `/preview/inputs/{id}.jpg`, `/preview/inputs`, WHEP
   focus — `crates/multiview-control/src/routes/preview.rs`). The gap is **UI ubiquity**: a small
   "preview" affordance attached to *every* input/output/tile/track reference, opening an overlay
   that taps the existing snapshot/MJPEG/WHEP path and never navigates away or mutates state.
   [ADR-0090](../decisions/ADR-0090.md) pins this as a non-disruptive, refcounted, viewport-driven
   affordance — pure UI + a thin descriptor, **zero** new engine surface.

2. **Live-apply must be the default everywhere; "download-edit-restart" is a defect, not a workflow.**
   The instant-apply doctrine and invariant #11 are already realised for sources
   ([ADR-W018](../decisions/ADR-W018.md)), overlays ([ADR-W022](../decisions/ADR-W022.md)), layouts,
   and routing — but several surfaces still answer `X-Multiview-Apply: restart` honestly because the
   live seam is not built (audio-routing `crates/multiview-control/src/routes/audio.rs:156`,
   outputs `routes/outputs.rs:126`, devices `routes/devices.rs:191`). [ADR-0091](../decisions/ADR-0091.md)
   does three things: (a) a **live-apply coverage audit** turning each remaining `restart` into a
   tracked item with a designed seam; (b) a **staging / write-but-don't-apply** mode + a **persistent
   pending-changes banner** so an operator can author a risky change and apply it deliberately; and
   (c) a **high-risk gate** that requires explicit confirmation for Class-2 (consumer-disrupting)
   applies. This is the [Terraform `plan`→`apply`](https://spacelift.io/blog/terraform-dry-run)
   discipline and the [Cloudscape unsaved-changes](https://cloudscape.design/patterns/general/unsaved-changes/)
   pattern applied to a live broadcast system.

3. **Re-assess hardware on demand is a first-class operator action — with a loud interrupt warning.**
   The placement subsystem ([self-aware-placement.md](self-aware-placement.md),
   [ADR-0035](../decisions/ADR-0035.md)) re-plans *autonomously* and never fragments a running island.
   What is missing is an **operator-triggered** "re-assess now" that re-runs DETECT→PLAN and, if it
   would move a running pipeline (a Class-2 make-before-break, [ADR-R010](../decisions/ADR-R010.md)),
   shows exactly what would be interrupted and requires explicit confirmation.
   [ADR-0092](../decisions/ADR-0092.md) makes the re-assessment a dry-run-first action: preview the
   plan, then confirm the interrupt.

4. **Tracks are typed, not free text.** The typed surfaces exist in config
   (`CaptionSelector` at `crates/multiview-config/src/schema.rs:476`; subtitle/audio crosspoints in
   `crates/multiview-config/src/routing.rs`) but the SPA renders free-text because no per-input
   **discovered** inventory reaches it (`captionsTrack`/`audioTracks` are raw strings —
   `web/src/resources/forms.ts:423`, `:1032`). [ADR-0093](../decisions/ADR-0093.md) surfaces
   [decoupled-routing.md](decoupled-routing.md)'s `StreamInventory` read-only over the API and feeds a
   typed `<TrackSelect>`, exactly mirroring [ADR-0036](../decisions/ADR-0036.md)'s replacement of the
   free-text codec field — capability-greyed per [ADR-M004](../decisions/ADR-M004.md).

5. **The layout editor can enable/disable subtitles and pick the subtitle source.**
   [ADR-0019](../decisions/ADR-0019.md) already owns a per-source caption selector
   (`auto | off | teletext_page N | track id | embedded_cc | sidecar`); the gap is a UI binding in the
   layout editor and a per-output subtitle selection. [ADR-0094](../decisions/ADR-0094.md) binds the
   selector to a per-tile/per-layer toggle (react-konva editor, [ADR-W004](../decisions/ADR-W004.md))
   and surfaces burn-in-vs-passthrough per [ADR-R007](../decisions/ADR-R007.md).

6. **Defects ↔ features.** Gaps #4 and #5 are *also live defects* on the running build, triaged in
   [current-defects-2026-06-13.md §1.5/§1.6](../development/current-defects-2026-06-13.md): the
   selectable-tracks and subtitle-toggle items are both "ship under this brief". This document is the
   feature/design home those defect rows point at.

7. **Invariants hold by construction.** Every surface here is control/preview/aux — it **samples** the
   engine and **never** back-pressures it (inv #10), and never touches the output clock (inv #1). Live
   applies ride the already-built bounded command bus drained at the frame boundary; preview rides the
   built isolation-safe taps; re-placement rides the off-thread placement controller and the
   make-before-break primitive. **No new engine-inward channel is introduced by any item.**

---

## 1. The WebUI-manages-everything north star and the five gaps

The stated goal (operator, standing): *every standard multiviewer feature is operable from the WebUI,
the WebUI manages all functionality, and comprehensive in-app docs explain it.* The instant-apply
doctrine sharpens it: *the operator should NEVER have to download a file, put it somewhere, and
restart for a change to take effect* — changes apply instantly (Class-1 frame-boundary or Class-2
make-before-break), outputs are never interrupted unless functionally impossible, and every change
re-assesses hardware allocation.

Against that north star, the intake names five concrete leaks. The table maps each to the as-built
ground truth and the ADR that closes it:

| # | Operator request (faithful) | As-built today (cited) | Closes via |
|---|---|---|---|
| 1 | Anywhere the WebUI references a video/audio input or output, an easy way to **preview** it **without interrupting** the current workflow | Preview taps + endpoints **built** (`routes/preview.rs`; `CliPreviewProvider`, `crates/multiview-cli/src/preview.rs`); isolation-safe. **No ubiquitous, non-disruptive UI affordance.** | [ADR-0090](../decisions/ADR-0090.md) |
| 2 | The user should **NEVER** download a file, place it, and restart for a change to take effect; high-risk changes may have a **write-but-don't-apply** gate + a **pending-changes warning bar** | Live-apply **built** for sources/overlays/layouts/routing (`X-Multiview-Apply: live`); several surfaces still honestly `restart` (audio-routing/outputs/devices). **No staging mode, no pending banner, no high-risk confirm gate.** | [ADR-0091](../decisions/ADR-0091.md) |
| 3 | **Refire hardware assessment + allocation**; make the user confirm they know it can interrupt everything | Placement re-plans **autonomously** off-thread ([ADR-0035](../decisions/ADR-0035.md)); make-before-break primitive **designed** ([ADR-R010](../decisions/ADR-R010.md)). **No operator-triggered re-assess action, no interrupt-confirm.** | [ADR-0092](../decisions/ADR-0092.md) |
| 4 | Tracks should be **selectable**, not a free-text list | Typed `CaptionSelector` + crosspoints in config; demux enumerates all streams but the input path **discards** non-video rows ([decoupled-routing.md §2](decoupled-routing.md)); SPA renders free text (`forms.ts:423`, `:1032`). | [ADR-0093](../decisions/ADR-0093.md) |
| 5 | The layout editor **can't enable/disable subtitles** | Per-source selector **built** in config ([ADR-0019](../decisions/ADR-0019.md)); ingest+burn-in wired (#47). **No layout-editor binding; no per-output subtitle selection.** | [ADR-0094](../decisions/ADR-0094.md) |

Three principles govern all five and keep them honest:

- **Read what is real, do not duplicate what shipped.** Preview, the `X-Multiview-Apply` machinery,
  the placement controller, `StreamInventory`, and the caption selector all exist (or are fully
  designed). Each ADR is a *thin* surface over them, never a re-implementation.
- **Honesty over optimism.** A `restart` answer is correct when the seam is not built; the fix is to
  *build the seam*, not to lie in the header ([ADR-W018](../decisions/ADR-W018.md) honesty bounds).
  A preview that is shed under load shows "preview reduced", never a frozen frame pretending to be
  live ([preview-subsystem §6](preview-subsystem.md)).
- **Vendor-neutral throughout.** The UX patterns cited (dry-run/plan, pending-changes banner,
  dirty-state confirm) are open, generic interaction patterns, not a copy of any product's surface.

---

## 2. Ubiquitous preview — a non-disruptive affordance (extends P002/P003)

### 2.1 What already exists (do not rebuild)

[preview-subsystem.md](preview-subsystem.md) is the authoritative design and large parts are built:

- **Three scopes, isolation-structural.** INPUT (read the existing `TileStore` slot, no second
  decode), PROGRAM (a downscale tap), OUTPUT (a tap of the real encoded packets). A slow/absent
  preview consumer can never back-pressure the engine — capacity-1 drop-oldest slots, own task tier,
  admission-shed-first ([preview-subsystem §2](preview-subsystem.md)).
- **Built endpoints** (`crates/multiview-control/src/routes/preview.rs`): `GET /api/v1/preview/program.jpg`,
  `GET /api/v1/preview/inputs/{id}.jpg`, `GET /api/v1/preview/inputs` (previewable ids),
  and `POST /api/v1/preview/{program|inputs/{id}|outputs/{id}}/whep` focus with a `503`+fallback hint
  when the focus cap/encoder budget is exhausted.
- **Built provider** (`crates/multiview-cli/src/preview.rs`, `CliPreviewProvider`): wait-free program
  slot + the **live-updatable** shared per-input store map (`SharedStores`, RCU per
  [ADR-W018](../decisions/ADR-W018.md)) so live-added inputs are previewable; NV12→JPEG on the request
  task, never on the clock loop.
- **Lifecycle** ([ADR-P003](../decisions/ADR-P003.md)): subscriber refcounts per `(entity, mode)`,
  linger on last-leave, viewport-driven grid, idle watchdog, cost ~0 when nobody watches.
- **Transport selection** ([ADR-P002](../decisions/ADR-P002.md)): cheap JPEG grids by default, one
  on-demand WHEP focus, LL-HLS fallback.

### 2.2 The gap and the design

The gap is **not** transport or isolation — it is that preview is reachable only from dedicated
preview pages, not *wherever a stream is named*. The operator wants: in the source list, the output
editor, the layout editor's source palette, the router crosspoint matrix, the track inspector, a tile
context menu — **everywhere a video or audio entity is referenced** — a one-click "preview" that opens
an overlay (popover/drawer/lightbox) *over the current page*, never a route change, never a mutation.

[ADR-0090](../decisions/ADR-0090.md) pins this as the **`<PreviewAffordance>` contract**:

- **A single reusable component** keyed by a typed `PreviewTarget = { scope: input|program|output, id }`,
  rendered as a small hover/focus button on any element that names a stream. Clicking opens a
  `<PreviewOverlay>` layered above the current view (Radix popover/dialog,
  [conventions §8](../architecture/conventions.md)) — the page underneath keeps its state, including
  an in-progress edit. Closing the overlay returns focus exactly where it was (WCAG 2.1 AA focus
  management). **Non-disruptive is the contract**, enforced by an interaction test.
- **Default mode = the cheapest tap** for that scope ([ADR-P002](../decisions/ADR-P002.md)): a polled
  snapshot / MJPEG thumbnail. A "Focus / Live" toggle upgrades the *one* open overlay to WHEP; closing
  reverts and frees the encoder ([ADR-P003](../decisions/ADR-P003.md) refcount + linger). Opening a
  second focus demotes the first — exactly the existing one-focus-per-operator ergonomic.
- **Audio preview** is the same affordance over an audio entity: the overlay shows the live meter
  (numeric, over the existing realtime lane) and offers an audition tap of the program/track bus where
  a preview audio path exists; until an audio preview encode lands it is **meter-only**, stated
  honestly (no silent pretend-audio). This dovetails with the meter-wiring work in
  [ADR-0059](../decisions/ADR-0059.md)/[switcher-audio.md](switcher-audio.md) — the affordance consumes
  meters, it does not build them.
- **One thin new read-only descriptor**: `GET /api/v1/preview/targets/{scope}/{id}` (or a field on the
  existing list endpoints) returning `{ available_modes, fidelity_label, subscriber_count }` so the
  affordance can grey unavailable modes and render the mandatory fidelity label
  (`REAL ENCODED OUTPUT` vs `PRE-ENCODE CANVAS APPROX`, [preview-subsystem §6](preview-subsystem.md)).
  This is a capability read off a cached descriptor — **invariant #10 by construction**. `subscriber_count`
  exposes who-is-watching activity, so it is **role/scope-gated** (admin/operator only); non-admin
  callers see a coarse `in_use | capacity_limited` status instead of an exact count, to avoid leaking
  operational activity across users/tenants.

### 2.3 Why it cannot disrupt anything

The affordance never issues a state-changing request. It opens a transport that is, by
[preview-subsystem §2](preview-subsystem.md), incapable of back-pressuring the engine; under load the
planner sheds preview *first* and the overlay shows "preview reduced — system busy" rather than
freezing. Because it overlays rather than navigates, an operator mid-edit on a form keeps every
unsaved field — which is the precise behaviour the staging work in §3 also depends on. The only new
server surface is one cached read; no encoder session is allocated until the operator explicitly
focuses.

**Efficiency note (this section).** Idle cost is zero (no tap until an overlay opens; viewport-driven
in grids; [ADR-P003](../decisions/ADR-P003.md)). An open snapshot overlay is one HW downsample + one
JPEG at thumbnail rate; one WHEP focus is one budgeted preview encode, shed first under pressure. No
full-res host round-trip; NV12 throughout to the small thumb (inv #5/#6).

---

## 3. Live-apply-everywhere audit + staging + pending banner + high-risk gate

### 3.1 The doctrine and what is already live

The instant-apply doctrine (operator, 2026-06-10) and invariant #11 require that a management change
applies **instantly** — Class-1 hot at a frame boundary, or Class-2 via make-before-break — and the
API surfaces *which* before applying ([ADR-M005](../decisions/ADR-M005.md)). The machinery is built:

- **The honest header**, `X-Multiview-Apply: live|restart`, computed per request from a
  per-collection `LiveApplyCaps` capability the binary injects
  (`crates/multiview-control/src/live_apply.rs`, `state.rs:622`). This closes the "control plane can't
  see the engine's features" lie class ([ADR-W022](../decisions/ADR-W022.md) §3).
- **Live source apply** ([ADR-W018](../decisions/ADR-W018.md)): `UpsertSource`/`RemoveSource` on the
  bounded bus, register-on-drain / produce-on-hub, with per-kind header truth.
- **Live overlay apply** ([ADR-W022](../decisions/ADR-W022.md)): per-document upsert/remove, a
  generation slot, frame-boundary re-derivation, header honesty per render-ability.
- **Live routing** (`crates/multiview-control/src/routing.rs` `RouteClass{Class1,ResetLite,Class2}`),
  surfaced via `POST /api/v1/routing/plan` and `/routing/{kind}/take` ([decoupled-routing.md §8/§9](decoupled-routing.md)).
- **The `/plan` dry-run classifier** ([ADR-M005](../decisions/ADR-M005.md)/[ADR-M012](../decisions/ADR-M012.md) §9):
  every switcher/routing op returns its class *before* a state-changing apply.

### 3.2 The remaining `restart` surfaces (the audit)

Several mutation routes still answer `restart` because the live seam is genuinely not built — and they
say so honestly:

| Surface | As-built | Header today | Path to live |
|---|---|---|---|
| Audio routing (`gain`/`mute`/route) | schema+REST only; pipeline does not read `[audio]`; `RouteAudio` held | `restart` (`routes/audio.rs:156`) | [ADR-0059](../decisions/ADR-0059.md) audio control seam (un-holds `RouteAudio`; gain/mute → Class-1) |
| Output edit (non-pinned fields) | `with_apply_restart` (`routes/outputs.rs:126`) | `restart` | Per-field classification: pinned params → Class-2 migration ([ADR-R010](../decisions/ADR-R010.md)); non-pinned (labels/bitrate) → Class-1 ([ADR-M004](../decisions/ADR-M004.md)) |
| Devices | `with_apply_restart` (`routes/devices.rs:191`) | `restart` | Device desired-state apply ([ADR-M008](../decisions/ADR-M008.md) registry) — out of this brief's scope, tracked |
| Captions/subtitle selector | config-level only | n/a (no live seam) | [ADR-0094](../decisions/ADR-0094.md) §6 (subtitle re-point seam, Class-1) |

[ADR-0091](../decisions/ADR-0091.md) **does not invent new live seams** for these — those belong to
their owning ADRs (0059, R010, M004, 0094). It pins the **audit discipline**: every mutation route
declares a live-apply target class and a tracked backlog item, so "still `restart`" is always a named,
scheduled gap with a designed seam, never an accepted permanent state. The north star is *zero*
surfaces that require download-edit-restart; the audit is how we drive the count to zero.

### 3.3 Staging — write-but-don't-apply (the operator's explicit ask)

Instant-apply is the default, but the operator explicitly wants the *option*, for high-risk changes,
to **write the change without implementing it now**, with a visible **pending** state. This is the
[Terraform plan→saved-plan→apply](https://spacelift.io/blog/terraform-dry-run) pattern: author and
review the exact change, apply it deliberately later. [ADR-0091](../decisions/ADR-0091.md) designs it
as a thin layer over the **already-built versioned resource store**
(`crates/multiview-control/src/resource_store.rs`, monotonic `Version`→`ETag`/`If-Match`→412):

- **Staging is opt-in per mutation** via an `apply` query/header (`apply=now` default; `apply=stage`
  to write-but-hold). A staged write **persists to the resource store** (so it survives, is exportable,
  and round-trips through config-as-code) but **does not enqueue the engine command** — the running
  picture is untouched, and the response carries `X-Multiview-Apply: staged` plus the *would-be* class
  from `/plan`.
- **Staged changes are visible and revertible.** `GET /api/v1/pending` lists every staged delta with
  its target resource, would-be class, and author/time; `POST /api/v1/pending/apply` applies them
  (each through its own normal live seam, in dependency order, with the high-risk gate of §3.5);
  `DELETE /api/v1/pending/{id}` discards one. Staging is **desired-state only** — it can never freeze
  live engine state (the [ADR-M012](../decisions/ADR-M012.md) §7 desired/live split is binding).
- **It composes with everything.** A staged change is just a resource version not yet applied; the
  existing ETag/If-Match concurrency, BOLA scoping, and 422 field-path validation all apply unchanged.
- **Applied vs staged reads are explicitly separated.** A normal `GET` returns the *applied* desired
  state (what is — or would be — on air); staged deltas are visible **only** through `/pending` or an
  explicit draft/staged view, and config-as-code export defaults to the applied state (staged deltas
  exported only on an explicit `?include=staged`). A staged write must never make an unapplied desired
  state read back as if it were active — the [ADR-M012](../decisions/ADR-M012.md) §7 desired/live split
  is the binding model and staging adds a third, clearly-labelled *staged-draft* facet.

### 3.4 The persistent pending-changes banner

When the pending set is non-empty, the SPA shows a **persistent warning bar** — the
[Cloudscape "communicating unsaved changes"](https://cloudscape.design/patterns/general/unsaved-changes/)
and [Oracle ADF dirty-state](https://www.oracle.com/application-development/technologies/adf/unsaveddatawarning.html)
pattern — stating *"N changes are staged and not yet applied"* with **Review / Apply all / Discard
all** actions, and a per-change **would-be class** badge (Class-1 hot vs Class-2 disruptive). The
banner is driven by `GET /api/v1/pending` + a `pending.changed` realtime event on the existing
drop-oldest broadcast (inv #10). It also covers the *page-local* dirty-form case (an in-progress edit
not yet submitted) via the standard snapshot-FormData dirty check, so the operator is never surprised
by lost edits on navigation. The banner is **informational + actionable**, never blocking: the system
keeps running on the applied state until the operator chooses to apply.

These are two **distinct states** with different scope, persistence, and permitted actions, and the UI
must keep them separate: a *server-side staged* delta (persisted, shared, BOLA-scoped, applied via
`/pending/apply`) versus a *page-local dirty form* (client-only, never persisted, resolved by Save or
Discard on that page). "Apply all" acts **only** on server-side staged deltas and can never apply an
unsaved client-only edit; the two are rendered as separate sections/badges so the operator is never
misled about what an action affects.

### 3.5 The high-risk gate

A Class-2 apply is consumer-disrupting by definition (new SPS/PPS + IDR; HLS `EXT-X-DISCONTINUITY`;
RTMP/many players reconnect — [ADR-R004](../decisions/ADR-R004.md)/[ADR-R010](../decisions/ADR-R010.md)).
[ADR-0091](../decisions/ADR-0091.md) requires an **explicit confirmation step** for any apply the
`/plan` classifier returns as Class-2 (or that the capability matrix flags high-risk): a confirm dialog
that names *what will be interrupted* (which outputs, which consumers, the expected discontinuity) and
requires a deliberate action — the [dirty-state destructive-confirm](https://www.oracle.com/application-development/technologies/adf/unsaveddatawarning.html)
pattern. Class-1 hot changes need no gate (they are seamless). The gate is **UI + the existing
`/plan` response**; the server already returns `202 {operation_id}` for Class-2 and reports the
terminal `Migrated`/`RolledBack` outcome on the realtime stream — the gate adds no new engine surface,
only the confirmation in front of the existing migrate call.

**Efficiency note (this section).** Staging adds one resource-store version per staged change and one
small `GET /pending` read; the banner consumes one conflated realtime event. No hot-path cost; the
engine sees a staged change only when (and as) it is applied through the normal bounded bus.

---

## 4. Re-assess hardware on demand + explicit interrupt confirmation

### 4.1 What exists

[self-aware-placement.md](self-aware-placement.md) / [ADR-0035](../decisions/ADR-0035.md) build the
SENSE→DETECT→WARN→PLAN→APPLY loop: a `CapabilityReport` at build/re-plan time, a `HealthWarning` model
+ `GET /api/v1/health`, the ADR-0018 `select_device`/`PlacementController` planner (off-thread, ~1 Hz),
and a slow control tick that applies O(1) frame-boundary-safe changes. The placement controller
*proposes only* (`PlacementProposal::{Hold,Shed,Migrate,Split}`,
`crates/multiview-engine/src/placement.rs:117/:363`); a `Migrate` executes as a whole-island Class-2
make-before-break on the supervisor path ([ADR-R010](../decisions/ADR-R010.md)), **never** on the clock
thread, and **never** fragments a running island (the GPU-placement principle).

### 4.2 The gap and the design

The loop re-plans **autonomously** on load signals. The operator wants to **manually refire** the whole
assessment-and-allocation pass — e.g. after plugging in a GPU, changing drivers, or freeing a co-tenant
NVR — and to be **warned that doing so can interrupt everything**. [ADR-0092](../decisions/ADR-0092.md)
adds an **operator-triggered re-assessment** that is *dry-run-first*:

1. **`POST /api/v1/system/placement/reassess?dry_run=true`** re-runs DETECT (refresh the
   `CapabilityReport` — re-probe adapters/encoders) and PLAN (`select_device` over the current demand +
   a fresh `DeviceLoad` snapshot) **without applying**, returning a typed **`ReassessmentPlan`**: for
   each running pipeline/island, `{ stays | would_migrate { from, to, reason, class: Class2 } |
   would_shed }`, plus any new `HealthWarning`s the refreshed report raises (e.g. a now-usable GPU that
   was silently on CPU — the exact ADR-0035 bug). This runs off the clock thread, so **inv #10 holds** —
   but the DETECT refresh is **not free**: re-probing adapters/encoders (throwaway `open_as` / `*_cuvid`
   opens) consumes scarce decoder/encoder/session slots and could perturb a running pipeline. The
   re-assess therefore prefers **cached/read-only capability detection**, and any live device probe is
   **admission-budgeted** (skipped, not forced, when no spare session exists) — returning a
   `cannot-safely-probe-while-live` result for that backend rather than touching a live device. A
   dry-run never deallocates or restarts a running pipeline.
2. **The UI shows the plan and the blast radius.** If every island `stays`, the apply is a no-op and is
   labelled as such (no interrupt). If any island `would_migrate`, the confirm dialog enumerates
   **exactly which outputs/consumers would see a discontinuity** (each migration is a Class-2 cutover),
   mirroring the §3.5 high-risk gate.
3. **`POST .../reassess` (apply)** requires explicit confirmation and executes each accepted migration
   through the **one** make-before-break primitive ([ADR-R010](../decisions/ADR-R010.md)) — spin-up
   NEW alongside OLD, warm, cut at an IDR, drain+stop OLD — so even a re-placement that *does* interrupt
   consumers does so with exactly one correctly-signalled discontinuity and never a gap (inv #1). The
   anti-storm damps (cooldown / per-GPU budget / min-gain, ADR-0018 §4.6) still gate whether a migration
   is *worth* proposing; an operator-forced re-assess can override the cooldown but not the affinity/
   no-fragment hard gate.

### 4.3 Why the confirmation is mandatory and honest

The operator explicitly asked to *confirm they know it will potentially interrupt everything*. The
design refuses to hide that: the dry-run plan is the source of truth for the blast radius, and the
apply is gated behind a confirm that names the affected consumers. A re-assess that would change nothing
is shown as safe and applies without ceremony; a re-assess that would move N islands shows N
discontinuities before the operator commits. There is no "apply and hope" path. The terminal outcomes
ride the realtime stream like any other migration.

**Efficiency note (this section).** A dry-run is a `CapabilityReport` refresh (one throwaway
open/probe per HW backend at target res, [ADR-0035](../decisions/ADR-0035.md)) + one `select_device`
evaluation over a `DeviceLoad` snapshot — bounded, off the clock thread, paid only when the operator
asks. The apply's cost is the make-before-break transient (OLD+NEW concurrently), admission-gated in
the primitive's VALIDATE phase ([ADR-R010](../decisions/ADR-R010.md)) so it cannot starve a running
output.

---

## 5. Selectable tracks — typed `StreamInventory`, replace free-text

### 5.1 The gap (also a live defect)

This is both a feature gap and [current-defects §1.5](../development/current-defects-2026-06-13.md): the
SPA renders audio/subtitle tracks as free text (`web/src/resources/forms.ts:423` `captionsTrack`,
`:1032` `audioTracks`, `parseTrackList`) because **no discovered per-input inventory reaches it**. The
typed *destinations* exist in config (`CaptionSelector` `schema.rs:476`; `SubtitleCrosspoint` and the
independently-keyed routing maps in `routing.rs`), and the demuxer enumerates every elementary stream
(`crates/multiview-ffmpeg/src/demux.rs:301`, language at `:318`) — but the input path **discards** the
non-video rows ([decoupled-routing.md §2](decoupled-routing.md)), so there is nothing typed to render.

### 5.2 The design (mirror ADR-0036's free-text-codec fix)

[ADR-0093](../decisions/ADR-0093.md) surfaces [decoupled-routing.md §3](decoupled-routing.md)'s
**`StreamInventory`** read-only over the API and feeds a typed selector — the exact shape
[ADR-0036](../decisions/ADR-0036.md) used to kill the free-text codec field:

- **Stop discarding the inventory.** Emit all `StreamInventory` rows (V/A/S/Data/Timecode), each with a
  `StableStreamId` (kind-scoped: TS=PID, HLS=`group_id+name`, soft-key fallback), `kind`, codec,
  `language: Option<Bcp47>`, `default`, and a **key-stability tier** (hard/soft) so a soft-keyed
  selection is an operator-visible reorder risk ([decoupled-routing.md §3](decoupled-routing.md)). The
  generalisation of the libav/TS/HLS discovery is decoupled-routing's work; this ADR consumes it.
  The HLS `group_id+name` example is not unique on its own across renditions, duplicate names, or
  language variants and is vulnerable to playlist churn: the `StableStreamId` key must therefore include
  enough fields (URI / group / type / name / language) plus the stability tier — or carry an explicit
  collision-disambiguation strategy — to be unique within an input; decoupled-routing owns the exact key
  composition and tie-break.
- **`GET /api/v1/inputs/{id}/streams` → `StreamInventory`** (a pure read off the ingest actor's
  demuxer, off-engine, inv #10; `input.streams` deltas on re-probe / PMT-version bump —
  [decoupled-routing.md §9](decoupled-routing.md)).
- **A typed `<TrackSelect>`** (built on the existing `KindSelect` generic + shadcn `Select`,
  [conventions §8](../architecture/conventions.md)) fed by a `useStreamInventory(inputId)` hook,
  rendering `CAM-A · A2 · spa · 5.1`-style options with **impossible cells greyed** by the
  [ADR-M004](../decisions/ADR-M004.md) carrier capability matrix (HLS=select-one, NDI=channel-map,
  RTMP=single-program). Replaces the free-text `captionsTrack`/`audioTracks` fields.
- **Selectors stay typed end-to-end**: the SPA emits the typed `CaptionSelector::Track { id }` /
  audio crosspoint (`StreamRef { input_id, kind, selector }`,
  [decoupled-routing.md §4](decoupled-routing.md)), never a raw string the server must re-parse.
  Free-text remains acceptable **only** as an "advanced / not-yet-probed" escape with a clear warning,
  mirroring ADR-0036's reason-tooltip discipline.

**Efficiency note (this section).** Inventory is computed once at open and on re-probe (it already is —
the fix is to *not throw it away*); the API read is a cached snapshot; the SPA hook is React-Query
cached. Zero hot-path involvement, no decode added (presence is structural, from the demux enumeration).

---

## 6. Subtitle toggle in the layout editor + per-output subtitle selection

### 6.1 The gap (also a live defect)

[current-defects §1.6](../development/current-defects-2026-06-13.md): the layout editor offers no
control to turn subtitles on/off for a tile or the program. The machinery is built — the per-source
caption **selector** ([ADR-0019](../decisions/ADR-0019.md): `auto | off | teletext_page N | track id |
embedded_cc | sidecar`) and the ingest+burn-in path (#47) — but there is **no UI binding** in the
react-konva editor, and no per-output subtitle track selection.

### 6.2 The design

[ADR-0094](../decisions/ADR-0094.md) binds the existing selector to the layout editor and adds
per-output subtitle selection, surfacing the burn-in-vs-passthrough capability:

- **Per-tile subtitle control in the editor** ([ADR-W004](../decisions/ADR-W004.md) react-konva +
  dnd-kit): each tile/cell gets a subtitle panel with an **on/off toggle** + a typed **track select**
  (the §5 `<TrackSelect>` over that tile's source inventory). Because `CaptionSelector` is **source-global**,
  a per-tile toggle must **not** drive it directly — toggling one tile would otherwise disable caption
  ingest/render for every other tile sharing that source. The per-tile **render enablement** is therefore
  a **layout/subtitle-crosspoint** property (the tile's caption layer bound/unbound), while
  `CaptionSelector::Off` is reserved for **source-level ingest disable only**. The editor must
  **disambiguate** the two senses of "off" ([decoupled-routing.md](decoupled-routing.md)):
  *don't ingest at all* (source selector `Off`, affects all tiles on that source) vs *don't render this
  layer on this tile* (subtitle crosspoint unbound) — the panel labels which it is doing, and the
  per-tile toggle defaults to the layer-render sense.
- **Per-output subtitle selection.** Where a transport can carry discrete subtitle tracks
  ([ADR-R007](../decisions/ADR-R007.md): HLS/LL-HLS segmented WebVTT + in-band 608/708; MPEG-TS/SRT
  DVB-sub/teletext/608; RTMP = burn-in or single-608; NDI = burn-in), the output editor offers a typed
  per-output subtitle-track set with **capability-greyed** options and a clear **burn-in vs passthrough**
  badge, derived read-only from the [ADR-R007](../decisions/ADR-R007.md) capability matrix. This is the
  subtitle analogue of [ADR-M004](../decisions/ADR-M004.md)'s audio output-track ownership.
- **Live-apply class.** A subtitle layer re-point onto an existing layer/track is **Class-1**
  (re-point the `Arc<CueStore>` the layer samples, clear-on-switch for the boundary frame —
  [decoupled-routing.md §5/§8](decoupled-routing.md)); a subtitle **track-set** change that alters a
  pinned output's passthrough set is **Class-2** ([ADR-R010](../decisions/ADR-R010.md)). The editor
  surfaces the class via `/plan` before applying, and the per-source selector toggle rides the
  subtitle re-point seam (the [ADR-0059](../decisions/ADR-0059.md) subtitle-seam precedent —
  `SubtitleRouteHandle` ArcSwap, drained off the hot path), so toggling captions is live, not a restart.
- **Honesty caveat (packaging).** Captions render only where the build includes the `overlay`
  (and `libass` for styled) feature; the deploy `nvidia` preset has historically omitted `overlay`
  (HLS-WebVTT memory). The editor's subtitle control must reflect the **build capability** via
  `LiveApplyCaps`/the capability report — a toggle over an absent feature is greyed with a reason,
  never a silent no-op (the [ADR-W022](../decisions/ADR-W022.md) header-honesty discipline). Fixing the
  deploy preset to ship the caption path is a packaging prerequisite tracked alongside this UI work.

**Efficiency note (this section).** Captions decode **only when shown** (selector ≠ `off` and the tile
is on-canvas — [ADR-0019](../decisions/ADR-0019.md)), so an editor toggle to `off` *removes* decode
cost; cost scales with caption-bearing tiles, not sources. No hot-path change; the cue store is a
bounded sampled read off the clock loop (inv #1/#10).

---

## 7. Invariants, isolation, and the efficiency budget

- **#1 (output clock).** Nothing here paces or blocks the output clock. Preview, staging, the pending
  banner, the re-assess dry-run, inventory reads, and the caption toggle are all control/preview/aux:
  they **sample** the engine. Live applies (source/overlay/routing/audio/subtitle) ride the bounded
  command bus drained *between* `clock.tick()` and `compose()` ([decoupled-routing.md §2](decoupled-routing.md)).
  Bus capacity is checked **before** the mutation is committed: a `now` apply that cannot be enqueued is
  rejected pre-commit (no resource-store write) or auto-staged into `/pending` with an operator-visible
  status — it is **never** accepted/written and then silently dropped, which would create stored-vs-live
  drift. Either way the clock never waits for the submit. A re-placement that interrupts consumers does
  so via make-before-break with two independent clocks, never a gap ([ADR-R010](../decisions/ADR-R010.md)).
- **#10 (no back-pressure).** Every new API surface is a read off a cached/wait-free snapshot or a
  bounded `try_submit`; every new realtime signal (`pending.changed`, reassess outcomes, `input.streams`)
  rides the existing drop-oldest broadcast. The engine **never awaits** a preview/control/UI consumer.
  No item adds an engine-inward channel — preview reuses the built isolation-safe taps, staging reuses
  the resource store, re-assess reuses the off-thread placement controller + the supervisor migration
  path.
- **#6 (decode-at-display-res) / #8 (color order).** Preview decodes/downsamples at thumbnail size
  (cue & output taps), never full-res ([preview-subsystem §3](preview-subsystem.md)); the output fidelity
  label preserves the color-tag truth (#8 verification is unchanged). Inventory adds no decode.
- **IPv6-first.** Any preview/WHEP/realtime examples and any new endpoint bind dual-stack `[::]`,
  bracket IPv6 URL literals, and emit SDP `c=IN IP6` ([conventions §10](../architecture/conventions.md)).
- **Efficiency budget (whole brief).** Mem: one resource-store version per staged change; bounded
  pending list; preview slots are capacity-1 drop-oldest. CPU/GPU: zero when nobody previews; one HW
  downsample+JPEG per open snapshot; one budgeted WHEP per focus, shed first; a dry-run is a bounded
  probe+score pass. IO: one cached read per descriptor/inventory/pending poll; realtime is conflated
  drop-oldest. **No per-tick cost is added on the data plane by any item here.**

---

## Open questions

1. **Staging granularity and dependency ordering.** When `POST /api/v1/pending/apply` applies a batch
   of staged changes, what is the correct dependency order (e.g. a source must exist before a routing
   crosspoint references it), and should partial-apply-on-error roll back or stop-and-report? The
   resource store's per-object versioning gives a starting order; the precise transaction semantics need
   a decision (default proposed: apply in resource-dependency order, stop-and-report on first
   422/conflict, never partially apply a Class-2 mid-batch without its own gate).
2. **Audio preview audition.** §2 makes audio preview meter-only until an audio preview-encode/audition
   path exists. Is an audition tap (decode a track to a short Opus/PCM preview stream over WHEP/WS) worth
   the encoder budget, or do meters + the program WHEP focus suffice? Defer to the
   [switcher-audio](switcher-audio.md) meter work landing first.
3. **Operator-forced re-assess vs anti-storm.** §4 lets an operator override the placement cooldown.
   Should there be a rate-limit on *manual* re-assess (to prevent an operator turning the anti-storm
   damps off by clicking repeatedly), and should a forced re-assess that the planner would *not* have
   proposed require a second, stronger confirmation?
4. **Pending-banner scope across operators.** The pending set is server-side (resource store), so two
   operators see the same staged changes. Should staging be per-operator (draft namespace) or shared
   (one pending queue), and how does BOLA scoping interact with "apply all"? Default proposed: shared
   queue, each change BOLA-scoped to its object, "apply all" applies only changes the caller may apply.
5. **Free-text escape lifetime.** §5 keeps a free-text track escape for not-yet-probed sources. Once
   `StreamInventory` is reliable for a source kind, should the escape be removed for that kind, or kept
   permanently as an advanced override? Default proposed: keep, but warn and prefer the typed path.
6. **Ubiquity vs clutter.** A preview affordance on *every* stream reference risks visual noise. Should
   it be always-visible, hover/focus-only, or behind a per-page "preview mode" toggle? Default proposed:
   hover/focus-reveal with a keyboard affordance (WCAG), never a persistent button per row.
