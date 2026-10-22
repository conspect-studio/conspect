# ADR-W022: Live overlay apply — add / edit / remove on the running engine

- **Status:** Accepted
- **Area:** Web/API stack ↔ engine · overlays management · live-apply (invariant #11)
- **Date:** 2026-06-10
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md),
  [efficiency](../research/efficiency.md); builds on ADR-W015 (typed CRUD + `X-Multiview-Apply`),
  ADR-W008 (command bus), ADR-W018 (live source apply — the per-document Upsert/Remove pattern and
  the header-honesty discipline), ADR-W019 (live layout apply — frame-boundary swap + `job.progress`
  observability), ADR-R008 (serializable layer stack), ADR-0016/0023 (overlay bake)

## Context

`POST`/`PUT`/`DELETE /api/v1/overlays/{id}` validate (ADR-W015) and store — but every response says
`X-Multiview-Apply: restart` unconditionally and the running picture never changes. That violates
the instant-apply doctrine (invariant #11): an overlay change is the canonical Class-1 mutation — it
alters only what is drawn over the composited canvas, never the canvas geometry, the encoder, or any
output session.

### How the run path renders overlays today (investigated, as-built)

The runtime consumer of `multiview_config::Overlay` (`{id, kind: String, target, z, params}` —
params verbatim, lossless) is **one derivation**, evaluated **once** at `Pipeline::build`:

- `analog_clock_from_config(&config.overlays, w, h)` (`crates/multiview-cli/src/pipeline.rs`) reads
  the **first** `kind == "clock"` entry whose `face` param is `"analog"` and produces an
  `AnalogClockSpec` (centre/radius/tz). It is captured into the `Send` `BakeContext`, from which the
  **bake-consumer thread** (off the output-clock loop, ADR-0025) builds its non-`Send`
  `OverlayBaker` once (`StreamBaker::new`) and rasterizes every output frame's overlay draw list.

Everything else the baker draws — per-tile labels, dB meters, state/fault flags, captions, the
digital wall-clock readout — is fixed multiviewer chrome derived from the layout/sources, **not**
from `[[overlays]]`. The kinds `label`, `tally_border`, `image`, and `subtitle` have **no renderer
in any current build**; a `clock` with a digital face coincides with the always-on chrome readout.
The `--software` run path and the `ffmpeg`-without-`overlay` build have **no baker at all**.

So the runtime "layer stack" is *not* rebuilt per tick and is *not* dirty-region cached against the
config: it is a build-once derivation living on the bake-consumer thread, fed per-tick dynamics.
The cheapest correct live apply is therefore **swap the derivation inputs** (the overlay working
set) and let the consumer thread re-derive — not a per-primitive layer-stack mutation protocol.

Hard constraints (unchanged from ADR-W018/W019):

- **Inv #1:** the frame-boundary drain runs on the output-clock loop — no I/O, no locks shared with
  producers, no unbounded work, no rasterization.
- **Inv #10:** control may never back-pressure the engine — bounded bus, `try_submit`, shed honestly.
- **Header honesty:** `X-Multiview-Apply: live` only when the running picture actually follows the
  change; kinds/builds that cannot render the document are stored + warned, never lied about.

## Decision

### 1. Two new engine commands (per-document, mirroring ADR-W018)

`Command::UpsertOverlay { op, overlay: Box<multiview_config::Overlay> }` and
`Command::RemoveOverlay { op, id }` ride the existing bounded, non-blocking command bus (ADR-W008).
Per-document Upsert/Remove is chosen over a whole-set `ApplyOverlays` because (a) the route only
knows the one mutated document — no repository list/IO at request or drain time; (b) the drain
already owns the engine's working-config mirror, so an id-keyed upsert/remove keeps that set
coherent the same way `UpsertSource` does; (c) the renderer derives from the whole mirrored set
anyway, so the set-level swap happens engine-side from the mirror (one place, one truth).

### 2. Frame-boundary drain: O(set) data swap, no heavy work, no hub

`CommandDrain` (the per-tick control hook on the output-clock loop) applies a drained overlay
command as **pure data mutation**:

1. upsert-by-id / remove-by-id on the working config's `overlays` vec (mirror — same discipline as
   live sources; removing an unknown id publishes no new set but is warned **and** surfaced as an
   `apply_overlay_held` outcome, symmetric with a held upsert — never a silent drop);
2. bump a monotonic **generation** and publish the full mirrored set as one
   `Arc<OverlaySet { generation, overlays }>` into a shared lock-free
   **`OverlayApplySlot`** (`Arc<ArcSwap<OverlaySet>>` — the same isolation primitive as the
   subtitle re-point slot, direction reversed: drain → bake consumer);
3. emit a `job.progress` outcome (phase `apply_overlay`, or `apply_overlay_held` with the reason)
   on the drop-oldest event stream — the ADR-W019 observability pattern;
4. for any overlay kind the running build does **not** render (`label`/`tally_border`/`image`/
   `subtitle`, and `clock` without an analog face), `tracing::warn!` names the kind and says the
   stored document produces no visual change in this build — stored + mirrored + warned, never
   silent, never lied about.

The bake consumer (already off the clock thread) checks the slot's generation **once per frame**
(wait-free `ArcSwap::load`); on change it re-derives its overlay render state — today
`analog_clocks_from_config` over the new set, O(overlays) pure math, **one face per analog-face
entry in working-set order** (no first-wins) — and the **next baked frame** draws the new faces.
That is a clean frame-boundary (Class-1) transition: a frame is baked entirely from the old set or
entirely from the new set, never mixed.

**Why no `LiveSourceHub`-style worker:** an overlay apply triggers no heavy work. The analog face
is ring/stroke primitives (no glyphs); text glyphs were already rasterized lazily with a per-glyph
atlas cache *on the consumer thread* at draw time. There is no producer to spawn or join. Drain
cost is the id-keyed vec mutation plus one `Vec<Overlay>` clone into the published `Arc` —
operator-paced, bounded by the bus capacity per tick, the same order of work as the existing
`upsert_source` config mirror. The soak gate (§5) proves the claim.

The slot is seeded at `Pipeline::build` with the boot config's overlays (generation 0), so the
consumer's boot-derived state and the slot agree before any command arrives.

### 3. Per-collection live-apply capability on `AppState` (header honesty)

ADR-W018 noted honesty bound (b): the control plane could not see the engine build's features and
therefore over-promised `live` for a `clock` source on a build that cannot render it. This ADR
closes that class of lie for overlays and establishes the pattern:

`AppState` gains `live_apply: LiveApplyCaps` (default: nothing live). The **binary** — the only
place that knows both the compiled features and the chosen run path — injects the truth at wiring
time. For overlays the capability carries a *predicate over the document*,
`OverlayLiveCapability::renders(&Overlay) -> bool`, because render-ability is finer than the kind
string (a `clock` document renders live **iff** its `face` is `analog`; the digital readout is
independent always-on chrome whose presence an overlay document neither adds nor removes today):

| run path / build | injected capability |
|---|---|
| `run` full pipeline, `ffmpeg`+`overlay` | `Some` — renders ⇔ `kind == "clock"` ∧ `face == "analog"` |
| `run` full pipeline, `ffmpeg` without `overlay` | `None` (identity bake — nothing renders) |
| `run --software` (any features) | `None` (no baker on that path) |
| store-only deployments / tests | `None` by default |

Route behaviour (`routes/overlays.rs`, after the store write + audit — mirroring sources):

| Mutation | Condition | Header |
|---|---|---|
| POST | capability present ∧ `renders(doc)` ∧ `UpsertOverlay` enqueued | `live` |
| POST | capability present ∧ ¬`renders(doc)` (still enqueued — the mirror stays coherent; the drain warns) | `restart` |
| PUT | capability present ∧ (`renders(new)` ∨ `renders(previous)`) ∧ `UpsertOverlay` enqueued — editing a rendered face **away** (e.g. analog → digital) makes it vanish at the next frame, itself a live-visible change | `live` |
| PUT | capability present ∧ ¬`renders(new)` ∧ ¬`renders(previous)` (still enqueued; no pixels change) | `restart` |
| DELETE | capability present ∧ `renders(previous doc)` ∧ `RemoveOverlay` enqueued | `live` |
| DELETE | capability present ∧ ¬`renders(previous)` (still enqueued) | `restart` |
| any | no capability (software path, overlay-less build, no engine), bus full/closed, or stored body does not parse as `Overlay` | `restart` |

The ADR-W018 honesty bounds carry over: a submit that is never drained (engine stops first)
converges on restart semantics — the stored document remains the durable truth.

### 4. What each overlay kind does on live apply (truth table)

On the `ffmpeg`+`overlay` pipeline run (capability present):

| kind | live apply effect | header |
|---|---|---|
| `clock`, `face = "analog"` | the analog face appears / moves / re-zones / disappears on the next baked frame (derivation rule: **every** analog-face entry renders its own face, in working-set order — no first-wins) | `live` |
| `clock`, edited analog → digital/no face | the previously-rendered face **vanishes** on the next baked frame — a live-visible change (`live` iff `renders(new)` ∨ `renders(previous)`) | `live` |
| `clock`, digital/no face (before and after) | mirrored into the working set; the program's digital readout is independent always-on chrome — no visual change; drain warns | `restart` |
| `label` / `tally_border` / `image` / `subtitle` / unknown kinds | stored losslessly (params verbatim), mirrored into the working set, drain warns "no renderer in this build" | `restart` |

On `--software` or without the `overlay` feature: **every** kind stores + `restart` (no command is
enqueued — there is no seam to apply it through).

When a future renderer lands for a kind (e.g. `tally_border`), the binary widens its injected
predicate and the same seam carries it — no new protocol.

### 5. Guardrails & gates

- **Inv #1 soak:** `live_overlay_churn_flood_never_falters_the_output_clock` — a full-speed
  background flood of overlay upserts/removes against a running software engine with the seam
  wired; exactly N frames for N ticks, never faltered (the `stored_layout_apply_storm` /
  `live_source_churn_flood` pattern).
- **Inv #10:** the bus stays bounded (`try_submit` sheds → header degrades to `restart`); the slot
  is a wait-free single-writer `ArcSwap`; the consumer never blocks on the drain or vice versa.
- **Pixel-level proof:** a drain-published set change visibly changes the baked NV12 frame
  (headless — the same `OverlayBaker`/`apply_overlays_to_nv12` harness the existing bake tests
  use), and removing the entry restores the bare-canvas pixels.
- **Header truth both builds:** route tests with and without the injected capability.

## Consequences

- Overlay add/edit/remove is Class-1 live on the real pipeline: stored, applied at the next frame
  boundary, observable via `job.progress`, honest in the header — no restart, no output
  interruption.
- The per-collection `LiveApplyCaps` seam on `AppState` is the designed home for the parallel
  source/network live-add lane's capability signal (ADR-W018's honesty bound (b) closes the same
  way there).
- The whole-set publish (`Vec<Overlay>` clone per applied command) is deliberately simple; if
  overlay sets ever grow large enough to matter on the drain, the slot can carry an im-style
  persistent structure without changing the protocol. Today's sets are operator-scale (units to
  tens) and the soak gates the cost.
- Kinds without renderers remain honestly `restart`; building those renderers (ADR-R008's full
  layer stack, ADR-0016 dirty-region bake) is unchanged future work that will plug into this seam.
