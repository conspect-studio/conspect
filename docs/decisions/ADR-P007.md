# ADR-P007: Switcher preview bus (PVW) — same-tick second composite, cue/pre-warm as the ARM mechanism, honest readiness, monitor tiers, and the PVW-vs-preview-subsystem vocabulary

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-11
- **Source brief:** [production-switcher.md](../research/production-switcher.md) (also
  [preview-subsystem.md](../research/preview-subsystem.md))
- **Builds on / relates to:** [ADR-P001](ADR-P001.md) (read-only taps, drop-oldest, shed-first),
  [ADR-P002](ADR-P002.md) (per-scope transport ladder), [ADR-P003](ADR-P003.md) (lazy
  activation/auto-stop), [ADR-P004](ADR-P004.md) (cue = the pre-warm worker), [ADR-P005](ADR-P005.md)
  (mandatory fidelity labels), [ADR-P006](ADR-P006.md) (WHEP completion), [ADR-0054](ADR-0054.md)
  (M/E inside one program; render-plan resolver), [ADR-0055](ADR-0055.md) (transition engine),
  [ADR-T015](ADR-T015.md) (switcher timing), [ADR-RT008](ADR-RT008.md) /
  [ADR-W021](ADR-W021.md) (control surface), [ADR-R011](ADR-R011.md) (switcher resilience),
  [ADR-E007](ADR-E007.md) (degradation control loop)

## Context

A production switcher needs a **preview bus (PVW)**: the operator stages the *next* scene, looks at
it, then takes it to program with a cut or a timed transition. Nothing in the repo renders one
today, and the word "preview" is already taken by a different subsystem. As-built, verified:

- The engine composes exactly **one** canvas per program per tick: `OutputClock::tick()` at
  `crates/multiview-engine/src/runtime.rs:430`, the frame-boundary control hook
  (`FnMut(&mut CompositorDrive<Nv12Image>)`, no `Tick` argument — `runtime.rs:167`, invoked at
  `runtime.rs:439`), then `CompositorDrive::compose(tick)` at `runtime.rs:443` (**BUILT**).
  [ADR-0054](ADR-0054.md) widens the hook to receive the `Tick` and promotes the per-tick placement
  derivation to a render-plan resolver.
- The repo's "preview" is a **monitoring tap of the program canvas**, not a second composite: the
  run stores the per-tick pre-overlay canvas `Arc` into a wait-free slot
  (`crates/multiview-cli/src/pipeline.rs:2122`) of type
  `ProgramSlot = Arc<ArcSwapOption<Nv12Image>>` (`crates/multiview-cli/src/preview.rs:28`), served
  as JPEG stills by `CliPreviewProvider` (`preview.rs:38`) under `/api/v1/preview/*` (**BUILT**).
- `TileStore` is lock-free multi-reader (`read_at`,
  `crates/multiview-framestore/src/tile.rs:421`) with the producer-liveness vs on-screen-picture
  split (`state_at`, `tile.rs:355`) and a startup-readiness predicate `is_primed`
  (`tile.rs:311`) (**BUILT**) — a second composite can sample the same stores with zero extra decode.
- The preview crate's tap/WHEP machinery (`TapRegistry`,
  `crates/multiview-preview/src/lib.rs:85`) is **BUILT in-crate but UNWIRED** in the run: the
  binary registers `no_whep()` (`crates/multiview-control/src/state.rs:542`) and never constructs a
  `TapRegistry`. [ADR-P006](ADR-P006.md) completes WHEP as its own lane.
- The degradation ladder is **BUILT as a pure type** (`DegradationAction`, `#[non_exhaustive]`,
  `crates/multiview-hal/src/degradation.rs` — five preview rungs shed first, rung 4 =
  `DropOffAirCueDecoders` "ADR-P004 Tier B workers"); the engine `ControlLoop` that would drive it
  live is **SCAFFOLD** (no run-path caller). The ADR-P004 cue/pre-warm worker itself is **DOC-ONLY**.

## Decision

### 1. PVW rendering: a second composite against the SAME `Tick`, inside the same program

The PVW bus is rendered as a **second compose pass evaluated against the same `Tick` in the same
program** ([ADR-0054](ADR-0054.md)): the render-plan resolver derives a PVW placement list from the
M/E's preview scene state and dispatches one more `RunBackend::composite` on the clock thread,
sampling the **same `Arc`-shared `TileStore`s** (no extra decode). PVW is **never** a second
`ProgramSet` program: sibling programs run independent `OutputClock`s by design, and a
frame-aligned take plus mix/wipe math require both scenes under **one** clock in **one** composite
([ADR-0054](ADR-0054.md)). The program composite and its publish complete **first** in the tick
sequence; the PVW compose consumes tick slack only.

### 2. Admission + degradation: PVW is accounted, sheddable — with a mid-transition carve-out

The PVW composite is a first-class load item: it is **admission-accounted** when armed/enabled
(planner composite Mpix/s, [ADR-E007](ADR-E007.md) / [ADR-0054](ADR-0054.md)), and it gets its
**own degradation rungs**, inserted between the existing preview rungs (1–5) and the tile rungs
(6–8) of `DegradationAction` (`#[non_exhaustive]`, so the insertion is additive):

1. halve the PVW monitor publish rate (slot updated every Nth tick);
2. reduce the PVW composite resolution;
3. compose PVW every Nth tick (switching **state** stays per-tick live; only the picture coarsens);
4. suspend the standalone PVW composite entirely (monitor badges stale; cut/auto still work).

All four apply **before any tile or program rung** — but **after** the monitoring-preview rungs,
because PVW is operator switching state, not best-effort monitoring. The CPU reference budget gate
(`crates/multiview-compositor/benches/composite_realtime.rs:39`, `TICK_BUDGET` = 40 ms/tick) is
re-run with the second compose enabled, and the reduced-rate/resolution PVW tier is the documented
answer when it doesn't fit. **Carve-out (never shed mid-transition):** while a transition is
in flight, both scenes are sampled by the **program** composite — that cost is program-affecting
and is never shed (complete-then-degrade; under sustained overload AUTO demotes to CUT at
arm/admission time, [ADR-0055](ADR-0055.md)). The sheddable item is only the standalone PVW
*monitor* composite.

### 3. Cue/pre-warm: [ADR-P004](ADR-P004.md)'s worker IS the PVW arm mechanism (WARM-ON-ARM)

One machinery for *look* and *take*. Selecting a source onto PVW (a preview-bus crosspoint change)
**arms** it: for any PVW-scene source with no live decoder, the off-thread side spawns/retains an
ADR-P004 cue worker (process-isolated, low-res/thumbnail-rate, admission-controlled, SSRF-guarded —
all of ADR-P004 unchanged) that publishes into the **same `TileStore` the take will bind**. The
worker primes the store; `TileStore::is_primed` (`tile.rs:311`) is the readiness predicate; the
subsequent take is then a Class-1 frame-boundary swap with zero connect/decode glitch (the
WARM-ON-ARM semantics the decoupled-routing backlog's RT-7 item pins). Deselecting from PVW starts
the ADR-P004 idle linger; the worker auto-stops unless re-armed or taken. The cue worker is built
**alongside** the PVW bus — shipping a PVW bus without it would stage cold sources, which §4
forbids presenting as ready.

### 4. Honest readiness: badge always; AUTO may be admission-gated; CUT is never refused

A cold, un-warmed source on PVW shown without qualification is **dishonest preview** — the operator
believes they have looked at what will go to air. Pinned policy:

- **Per-source readiness** is carried in the switcher state snapshot and events
  ([ADR-RT008](ADR-RT008.md)): `ready` (primed or already decoding on-air), `warming` (cue worker
  up, not yet primed), `cold` (no decoder — e.g. cue shed by the `DropOffAirCueDecoders` rung or
  refused by admission), `failed` (worker circuit-breaker open). The SPA renders it as text+shape
  badges, never colour alone ([ADR-W011](ADR-W011.md), [ADR-W021](ADR-W021.md)).
- **The PVW picture itself stays honest by construction:** the PVW composite samples through the
  same tile state machine (`state_at`) and slate policy as program, so a cold source renders the
  NO_SIGNAL placeholder and a stale source is badged stale — never a frozen frame passed off as live.
- **CUT is never refused** (operator sovereignty; a take of a cold source lands the slate/placeholder
  per policy, invariant #2 / [ADR-R011](ADR-R011.md)). **AUTO** may be gated: an optional per-M/E
  `require_ready_for_auto` flag refuses AUTO at admission time with a typed error while any armed
  source is not `ready` — a refusal, never a hold (no unbounded "waiting to take" state).
- **Fidelity labelling carries over:** the PVW monitor image is by construction a pre-encode canvas
  approximation (there is no PVW program encoder), so it is labelled per the
  [ADR-P005](ADR-P005.md) doctrine (`PreEncodeCanvasApprox`).

### 5. PVW monitor surface: per-bus wait-free slot + JPEG first; WHEP later

Monitoring pictures of switcher buses are delivered by the **preview subsystem**, replicating the
proven `ProgramSlot` pattern: one wait-free `Arc<ArcSwapOption<Nv12Image>>` slot **per bus tap**
(`pvw`, `clean`, `me[n]`, `aux[j]` — the [ADR-0054](ADR-0054.md) tap taxonomy; `program` keeps its
existing slot), stored from the per-tick projection exactly like `pipeline.rs:2122`, and served as
JPEG stills by the `CliPreviewProvider` pattern at

```
GET http://[::1]:8080/api/v1/preview/buses/{bus}.jpg      (e.g. /api/v1/preview/buses/pvw.jpg)
```

(IPv6-first examples; the listener binds dual-stack `[::]` per conventions §10). JPEG-first matches
the [ADR-P002](ADR-P002.md) ladder and blocks nothing on the unwired WHEP stack; motion preview
arrives later as `POST /api/v1/preview/buses/{bus}/whep` via the verified-unwired `TapRegistry` plus
the [ADR-P006](ADR-P006.md) machinery, as a fidelity-upgrade lane. Note: today's program JPEG is the
**pre-overlay** canvas (stored before the off-thread bake, `pipeline.rs:2110-2122`) — once
[ADR-0054](ADR-0054.md) moves DSK/FTB on-clock, the `program` slot carries post-DSK/post-FTB program
and the pre-DSK **clean** tap becomes its own slot; the two must land in the same push so the
program monitor never silently changes meaning.

### 6. Vocabulary: "preview bus (PVW)" vs "preview subsystem" — disjoint names, disjoint namespaces

This section is normative for the whole switcher doc set, the API, and the UI.

| Term | Means | Lives at |
|---|---|---|
| **Preview bus / PVW** | Per-M/E **operator switching state**: the scene staged to take next, its crosspoints, readiness, preview-transition state. | `/api/v1/switcher/*` ([ADR-W021](ADR-W021.md)); engine render plan; `switcher` realtime topic ([ADR-RT008](ADR-RT008.md)) |
| **Preview subsystem** | The **monitoring** stack of [ADR-P001](ADR-P001.md)–[ADR-P006](ADR-P006.md): read-only, best-effort, shed-first taps (JPEG/MJPEG/WHEP) of pictures that already exist. | `/api/v1/preview/*`; crate `multiview-preview` |

Rules: (a) the two API namespaces stay **disjoint** — switcher state and verbs never appear under
`/api/v1/preview/*`, and monitoring pictures never under `/api/v1/switcher/*` (the switcher
namespace answers "what is staged"; the preview namespace answers "show me a picture of it");
(b) new docs/UI/identifiers never use bare "preview" for switcher state — write "PVW" or
"preview bus" (UI bus buttons are labelled `PGM`/`PVW` with shape+text, [ADR-W011](ADR-W011.md));
"preview" unqualified refers only to the monitoring subsystem; (c) bus tap ids (`pvw`, `clean`,
`me1`…, `aux1`…) are reserved tokens following the `"main"`/`"prog"` reserved-name precedent in
`multiview-config`; (d) the crate name `multiview-preview` and its `/api/v1/preview` base are
unchanged — the PVW bus adds **no** crate ([ADR-0054](ADR-0054.md) crate-placement decision).

**Invariant posture.** The PVW composite is part of the program's render plan, evaluated
synchronously per tick — sampled, never pacing, allocation-pooled (invariant #1); every monitor
surface is a wait-free latest-wins slot read by best-effort consumers that are physically incapable
of back-pressuring the engine (invariant #10); the cue workers are process-isolated, off-thread,
admission-capped (ADR-P004), and their loss degrades a badge, never a tick.

## Rationale

Mix/wipe math needs both scenes in one composite under one clock, so PVW must live inside the
program — and once it does, the marginal machinery for a *useful* PVW is small: stores are already
multi-reader, the slot/JPEG monitor pattern already exists, and the readiness predicate already
exists. The cue worker is the one genuinely new runtime piece, and ADR-P004 already
verification-hardened its design; binding it to PVW arm gives broadcast cue-then-take semantics
with a single warm mechanism instead of two. Admission accounting plus dedicated shed rungs keep
the second composite from ever competing with program output (invariant #9 ordering: monitoring
preview first, PVW monitor next, tiles, then program) — while the mid-transition carve-out keeps an
in-flight transition whole, because a transition that degrades mid-travel is an on-air artefact.
The vocabulary split exists because the collision is real: the repo has six shipped/Proposed ADRs
and an API namespace called "preview" that mean *monitoring*; overloading the same word for
switching state would corrupt docs, code search, and operator expectations alike.

## Alternatives considered

- **PVW as a second `ProgramSet` program.** *Rejected — clock alignment.* Sibling programs own
  independent `OutputClock`s (by design, for isolation); a take would have to align two free-running
  clocks, and a transition would need both scenes in one composite anyway. The PVW bus must share
  the program's `Tick`.
- **Full-fidelity always-on PVW encode (a standing second encoder/rendition for PVW).** *Rejected —
  admission cost.* A permanent encoder session per M/E for a picture the operator glances at is the
  wrong default under invariant #9; the tiered monitor (wait-free slot → JPEG → WHEP-on-demand,
  lazy per [ADR-P003](ADR-P003.md)) delivers the need at near-zero idle cost. Aux feeds that
  genuinely need an independent encode are the RT-12 output-crosspoint path ([ADR-0054](ADR-0054.md)),
  not PVW.
- **Ship the PVW bus without the cue worker (preview cold sources as-is).** *Rejected — dishonest
  preview.* Staging a source the engine has never decoded shows a placeholder until take and then
  pays full connect/decode latency on air; that breaks the cue-then-take contract PVW exists to
  provide. ADR-P004 names the same conclusion for input cueing.
- **Refuse takes (including CUT) of un-primed sources.** *Rejected — operator sovereignty and
  resilience.* The switcher must always cut (a dying source mid-show still needs to be cut away
  *from*); honesty is delivered by badges, slate policy, and the optional AUTO admission gate, not
  by locking the bus.
- **Burn readiness badges into the PVW monitor image on the clock thread.** *Rejected.* Per-tick
  text rendering on the hot path duplicates the off-thread overlay bake's job; the SPA renders
  state-driven badges from the realtime snapshot, and the in-picture slate/stale presentation
  already rides the tile state machine.
- **Reuse `/api/v1/preview/*` for switcher state (or `/api/v1/switcher/*` for monitor JPEGs).**
  *Rejected.* Conflating operator state with monitoring breaks the isolation story (state is
  authoritative and replayed; pictures are best-effort and shed) and re-creates the vocabulary
  collision this ADR exists to end.

## Consequences

- One new sustained cost item when PVW is enabled: a second composite per tick per M/E (bounded by
  the re-run `composite_realtime` gate; NV12 1080p canvas ≈ 3.1 MB per slot frame, one `Arc` per
  bus tap), with a documented shed tier and an admission account — and one new transient cost item
  (both-scene sampling) during transitions that is deliberately *not* sheddable.
- The ADR-P004 cue worker moves from DOC-ONLY to a build prerequisite of the PVW bus (same lane);
  its degradation rung (`DropOffAirCueDecoders`) finally gets a live producer, and wiring the
  SCAFFOLD `ControlLoop` admission seam into the run becomes load-bearing for switcher work
  ([ADR-0054](ADR-0054.md) backlog).
- `/api/v1/preview` gains one scope (`buses/{bus}`), mirrored into OpenAPI and the SPA ladder
  ([ADR-W023](ADR-W023.md)); the switcher API surface stays state-only. The pre-overlay→on-clock
  DSK transition re-anchors the `program` slot meaning and ships together with the `clean` tap.
- Docs/UI carry the normative vocabulary; reviewers should reject new text that uses bare "preview"
  for bus state. ADR-P001–P006 are unaffected in substance: the preview subsystem gains a scope but
  its isolation, lifecycle, labelling, and transport doctrines apply to PVW monitors unchanged.
- Readiness, badges, and the AUTO gate add fields to the switcher state model
  ([ADR-RT008](ADR-RT008.md) snapshot, [ADR-M012](ADR-M012.md) per-M/E config) — pinned here so
  the control-surface ADRs carry them from day one.
