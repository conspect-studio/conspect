# Design note — production switcher first slice: a real dual-bus runtime on the tested core

Status: **Proposed** · Class: **R3** ([engineering.md](../standards/engineering.md) §A — the
slice touches the output-clock seam and the isolation seam) · Branch `feat/switcher-runtime`
· Base `f5c99bae` · Design base `feat/rc-switcher-core` `d8996868`

Related: [ADR-0030](../decisions/ADR-0030.md), [ADR-0054](../decisions/ADR-0054.md),
[ADR-0055](../decisions/ADR-0055.md), [ADR-0056](../decisions/ADR-0056.md),
[ADR-0057](../decisions/ADR-0057.md), [ADR-0058](../decisions/ADR-0058.md),
[ADR-0059](../decisions/ADR-0059.md), [ADR-M012](../decisions/ADR-M012.md),
[ADR-W021](../decisions/ADR-W021.md), [ADR-RT008](../decisions/ADR-RT008.md),
[production-switcher.md](../research/production-switcher.md),
[switcher-audio.md](../research/switcher-audio.md),
[production-switcher-backlog.md](../development/production-switcher-backlog.md).
The tested-core implementation status is
`docs/development/production-switcher-status.md` on `feat/rc-switcher-core` `d8996868`
(that file is not on this branch). ADR-M011 (Cast output driver) and ADR-M013 (outbound-dial
SSRF guard) are unrelated to this slice; they are listed only because the approved-design set
named them.

## 1. Why this note exists

The first operator-visible Program/Preview slice risks invariant #1 (one valid frame per tick,
forever) and invariant #10 (the engine never waits for a client). It therefore follows the R3
path: stop, write this design note, and ship the chaos/soak gate with the change
([engineering.md](../standards/engineering.md) §A). The note pins the seven decisions that the
implementation must not re-litigate: re-integration order, real dual-bus output routing,
overlap handling, tick-exact Cut/Auto and frame origin, prime holds and bounded preview leases,
honest program audio, and the API/BOLA contract.

## 2. Starting point, verified in source

This branch (`f5c99bae`) is `release/mda001-rc3` plus the on-demand source work
(`crates/multiview-engine/src/demand.rs`, `crates/multiview-cli/src/live_sources.rs`) and GPU
NV12+A alpha (`crates/multiview-compositor/src/gpu/compositor.rs`). It has the switcher
**design** (ADR-0030/0054–0059/M012/W021/RT008, the brief, the backlog) but **not** the tested
core: `crates/multiview-core/src/switcher/`, `crates/multiview-engine/src/switcher.rs`,
`crates/multiview-config/src/switcher.rs`, the tick-widened control hook, the render-plan
resolver, and the shared SPA realtime service are all absent here. They exist, tested, on
`feat/rc-switcher-core` `d8996868` (see the status table there).

The run path is unchanged: one `"main"` program owns one `OutputClock`, one
`CompositorDrive::compose(tick)` from pooled scratch
(`crates/multiview-engine/src/drive.rs:470-560`, `ComposeScratch` at `:701`), and the tick
projection clones the one composited canvas into an `Arc` that feeds the preview slot, the
display/NDI mailboxes and the bake/encode fan-out
(`crates/multiview-cli/src/pipeline.rs:4172-4244`). Outputs read no bus selector:
`OutputCrosspoint` carries `{output, program}` only (`crates/multiview-config/src/routing.rs:203-231`),
and every configured output carries the one program canvas.

## 3. First-slice target (operator-visible, end to end)

An operator can, on one M/E inside the `"main"` program:

1. select a source **and** a layout onto the **PVW** bus;
2. select a source/layout onto **PGM** directly (punch), or take PVW to PGM with **Cut** or
   **Auto**;
3. watch PGM and PVW monitors (reduced-refresh stills are acceptable for monitors);
4. assign a configured output to **PGM** or **PVW** and have that output carry the selected
   bus, at full rate;
5. do all of it from the SPA, with correlated operation outcomes.

Cut/Auto are the only transitions in this slice. Mix/dip rendering is part of the slice only
as the Cut/Auto geometry (see §6); wipes, DVE, keyers, media, macros, tally and aux are not.

## 4. Decision 1 — re-integrate the tested core first, then build the runtime

The two branches diverged, so this is a port, not a merge: `f5c99bae` has demand/lifecycle/
i192 that `d8996868` lacks; `d8996868` has the switcher core that `f5c99bae` lacks. Stage S0
brings the tested core across **additively** and proves it on this base with no runtime
behaviour change (`multiview run` still runs N=1 and never instantiates a bus). Only then do
S1–S5 build the runtime.

Core files to port (from `d8996868`): `crates/multiview-core/src/switcher/{mod,timing,descriptor,kernel}.rs`
+ `lib.rs` + `tests/switcher.rs`; `crates/multiview-engine/src/switcher.rs` + `lib.rs` +
`Cargo.toml` + `tests/{switcher_state_machine.rs,render_plan_alloc.rs,render_plan_seam.rs,route_apply.rs}`;
the tick-widened hook in `runtime.rs` and the resolver seam in `drive.rs`; the
`programs`/desugar/crosspoint-validation delta in `crates/multiview-config/src/{lib.rs,routing.rs,diff.rs}`;
the CLI desugar + program-audio-declared delta; the control
`boot_model`/`concurrency`/`openapi_schemas`/`routes` delta (only where this base does not
already have it — `crates/multiview-control/src/concurrency.rs` and `web/src/api/operations.ts`
are already present); `web/src/realtime/{shared.ts,correlatedCommands.ts,operations.ts}` and
the generated `docs/api/openapi.json` + `web/src/api/schema.ts`.

Conflict rule: this base's demand/GPU-alpha behaviour wins; the switcher core is additive. The
`crates/multiview-cli/src/pipeline.rs` delta (LANE-CORE) is serialised — no other stage may
write that file while S0 runs. S0's "done" is the focused core/config/engine/control tests
green on this base and the full CI set green, with no new runtime behaviour.

## 5. Decision 2 — two buses, one clock, one tick origin

ADR-0054 is binding: an M/E lives **inside one** ADR-0030 program. PGM and PVW are two scene
states under one `OutputClock`. Per tick the engine resolves the PGM scene and, **only when a
consumer exists**, the PVW scene, and composes each against the **same `Tick`** with its own
pooled scratch (no scratch shared across buses). A second `ProgramSet` program for PVW is
rejected: independent clocks cannot guarantee a frame-aligned take.

Frame origin. `tick.index` from the one `OutputClock` is the sole timebase and origin for
switcher state. PTS is re-stamped from the tick counter with exact rationals (never float fps)
exactly as `drive` does today. A transition records `start_tick` at the frame boundary on which
the command is applied; Cut is the degenerate one-tick case; Auto completes exactly when
`tick.index − start_tick == duration_frames`. Nothing else may re-origin, pause or reseed the
clock — not a prime hold, not a lease, not a monitor.

## 6. Decision 3 — Cut/Auto exact, bounded pooled dual-scene geometry, no flat union on overlap

- **Cut** swaps which scene is PGM on exactly the applied tick (the `rebind_cell` machinery at
  `crates/multiview-engine/src/drive.rs:396` generalised to a whole-scene swap) and
  flip-flops PVW/PGM per M/E config.
- **Auto** runs the armed transition at its integer frame count: `progress` is a pure exact
  rational of `tick.index − start_tick`, so a missed tick cannot desync it. No wall clock
  enters switcher state (hard gate 1).
- **Geometry is pooled and bounded.** Both scenes' `held`/`placements`/`order` buffers come
  from per-scene reusable pools of the `ComposeScratch` kind (zero steady-state allocation,
  asserted as in `tests/render_plan_alloc.rs`). A non-overlapping mix is one z-ordered
  union tile list with incoming opacity = progress, bounded by `MAX_TILES` (64,
  `crates/multiview-compositor/src/gpu/compositor.rs`).
- **Overlap is not emulated.** A flat union of two internally-overlapping scenes produces the
  through-blend defect (the far scene shows through the near one). It is forbidden. At
  **arm time**, before any engine command is enqueued, the two scenes' integer destination
  rects are intersected; on any intersection — or if the union would exceed `MAX_TILES` — the
  arm is **rejected with a typed error** (`SwitcherArmError::OverlappingScenes { cells }` /
  `TileBudgetExceeded`). `/plan` and the verb return RFC 9457 `422`, PGM and audio are
  unchanged, and no partial transition exists.
- **Documented restriction.** This slice therefore does not dissolve between two scenes whose
  tiles overlap. Complete pre-render is the next slice: scene A and scene B each pre-render to
  a pooled full-canvas NV12 intermediate, then one blend pass reads both. That path is bounded
  to three canvases (A, B, output) and is admission-credited as two extra composites; it is
  **not** a licence to fall back to the flat union. The held-out test for the defect stays in
  the suite.

## 7. Decision 4 — output bus routing is real, never an ignored field

**Config.** `OutputCrosspoint` gains an optional `bus` selector; `program` keeps its meaning
(the ADR-0030 program). `bus` defaults to `program` and accepts only the reserved tap tokens
of ADR-M012 §6 — `program` (post-FTB), `clean` (pre-DSK), `preview`. Unknown tokens, reserved
collisions, and a `preview`/`clean` bus with no declared M/E are rejected at validation
(`crates/multiview-config/src/routing.rs:434-451` is the template to extend). A field that is
parsed and ignored is a defect, not a placeholder.

**Runtime.** The tick projection publishes one canvas per active tap (the `ProgramSlot`
pattern, `crates/multiview-cli/src/preview.rs:29`). Each configured output selects the canvas
named by its `bus` and is encoded per **rendition** — one encode per `(program, bus)` with
≥1 sink, fanned by reference (`PacketRouter`/`EncodeOnceDriver`,
`crates/multiview-output/src/fanout.rs:212,299`), so N outputs on one bus share one encode
(invariant #7) and a `preview`-bound output genuinely transmits the PVW composite.

**Monitors vs outputs.** The preview subsystem (JPEG/WHEP) may degrade the PVW monitor to a
reduced rate/resolution — that is the documented PVW ladder rung. An **encoded output** bound
to `preview` may never silently fall back to PGM and may not be degraded below the rate its
configuration demands: it is a demand that pins the PVW rung. If PVW must be shed while such
an output exists, that output goes to its typed slate/black with an event — never to PGM.
Both anti-behaviours (field ignored; silent PGM substitution) have dedicated tests
(`switcher_runtime.rs`, `switcher_first_slice.rs`).

## 8. Decision 5 — prime holds and bounded preview leases never touch the clock

- **WARM-ON-ARM.** Selecting a cold source onto PVW takes a bounded `DemandLease`
  (`ConsumerClass::Preview`, `Rails::VIDEO`) through the existing on-demand manager
  (`crates/multiview-engine/src/demand.rs:745`, `:355`). The warm set is bounded
  (`MAX_TIMED_LEASES`-style cap, `demand.rs:1018`), released when the source leaves the bus or
  the run stops, and torn down by the demand worker `close after` — never by the clock.
- **TAKE hold, never clock hold.** A take that targets an unprimed source
  (`TileStore::is_primed`, `crates/multiview-framestore/src/tile.rs:435`) is held
  **control-side** with a bounded hold deadline. The engine keeps emitting every tick and every
  other output keeps producing. On prime, the take applies at the next frame boundary (exact
  tick); on hold expiry the take fails with a typed, correlated outcome and PGM/audio stay
  unchanged. The take frame is never a slate (hard gate 4). No path may block, sleep on, or
  pause the output clock awaiting a prime, a lease or a monitor.

## 9. Decision 6 — program audio is honest; no false AFV

The PGM scene's audio is on the program bus. The PVW scene's audio is never mixed into program,
so there is no false audio-follow-video. A Cut/Auto re-points program audio at the applied
frame boundary through the existing seam (`AudioControlHandle`,
`crates/multiview-cli/src/audio.rs:182`; `ProgramBus::repoint_crossfade`,
`crates/multiview-audio/src/program.rs:217`), which is a bounded equal-power per-sample ramp.

This slice **does not** expose an `afv`/`fade_with_transition` setting, because the
tick-window-matched AFV ramp (integer cumulative-sample delta via `SampleClock`, ADR-0059 §2 /
SW-14) is not implemented here. Advertising it would be a false claim. Full AFV stays tracked
in §12.

## 10. Decision 7 — 202 + correlated operation; BOLA on the M/E and every reference

**API.** Every mutating switcher verb carries an `Idempotency-Key` and returns **`202
{operation_id, kind}`**. Authoritative completion is the engine's applied event, delivered on
the realtime stream and correlated by a new `CorrKey::Switcher*` variant echoing `corr`
(`crates/multiview-control/src/realtime.rs:88`). There is **no `200 {applied: true}` before
the engine has received and drained the command.** The existing
`POST /api/v1/routing/{kind}/take` shape, which returns `200 {applied:true}` as soon as the
command is enqueued (`crates/multiview-control/src/routes/routing.rs:193-208`), is explicitly
**not** copied; aligning it is tracked follow-up. `POST …/plan` is a dry run returning the
classification (Class-1/Class-2/held), the selected transition, and arm-time refusals
(overlap, tile budget, unprimed). Reads answer `200` from the engine-owned mirrored state.
Spec lockstep is mandatory: utoipa + the `rest_routes` table + AsyncAPI + regenerated SPA
types in the same change.

**BOLA.** A switcher request names an M/E and references sources, layouts and outputs. Every
handler authorizes the M/E id **and every referenced id** —
`authorize_object` for sources/layouts/the M/E, `authorize_output` for outputs (its
`program:` namespace rule included), `authorize_scope(Program(..))` where a program is named
(`crates/multiview-control/src/auth.rs:313-347`); list handlers filter rows with
`authorize_object`; a missing or out-of-scope reference fails closed with `403` before any
command is enqueued. Authorizing only `me1` is a defect. A dedicated test drives a principal
scoped to `me1` plus one source, and asserts denial for each out-of-scope source, layout and
output the request names.

## 11. Invariant posture and failure behaviour

- **#1.** The clock thread never starts/stops/waits for a source, a lease, a monitor or a
  prime. A not-yet-primed bus composites last-good/`NO_SIGNAL` per the existing store ladder;
  a prime hold is control-side; a wedged monitor drops oldest.
- **#10.** Every engine→outside path here is a wait-free cell store, an unpark, or a bounded
  drop-oldest ring. The arm-time overlap/tile check is integer geometry on the control side,
  before the engine is asked to do anything (invariant #9/#11).
- **#7.** One encode per `(program, bus)` rendition with ≥1 sink; nothing is encoded without a
  consumer.
- **#5/#8.** Both buses stay NV12 (plus the existing straight-alpha plane where present); the
  compositor's premultiply/linear-light order is untouched.

| Failure | Behaviour |
|---|---|
| Source dies on either bus | last-good per scene, then the existing tile ladder; the transition still completes on schedule |
| Arm on internally-overlapping scenes | typed `422`, PGM/audio unchanged, no transition |
| Arm exceeds the tile budget | typed `422`, PGM/audio unchanged |
| Take on an unprimed source | held control-side, clock keeps ticking; applied on prime, or a typed correlated failure on hold expiry |
| Preview lease cannot start | PVW shows `NO_SIGNAL`/last-good honestly; the clock and PGM are unaffected |
| PVW rung shed with a `preview`-bound output | that output fails to its slate/black with an event — never silently PGM |
| Monitor consumer wedges | drop-oldest; the tick never stalls |
| Program audio seam absent | the route is held with a warning; no silent bus change |
| Command storm | last-wins coalescing per M/E destination; bounded per-tick drain work |

## 12. First slice vs remaining full scope

Tracked, not dropped. The following stay open in
[production-switcher-backlog.md](../development/production-switcher-backlog.md) and must not
be closed or silently deleted by this slice:

| Remaining | Row / decision | Why not now |
|---|---|---|
| USK keyers (luma/linear/DVE-pip, fill+key, mattes) | SW-9; ADR-0056 | needs keyer kernels + alpha GPU path wiring |
| DSK stage, TIE/CUT/AUTO, FTB | SW-10; ADR-0056 | on-clock canvas stages above the M/E |
| Aux buses + output←program crosspoint | SW-27; ADR-0030; ADR-R010 | needs a second encode/`move_sink` runtime; config surface only today |
| Multi-M/E (`mix_effects: Vec<MixEffects>`, `me<n>` re-entry) | ADR-M012 §1/§6; status “operator’s extra scope” | type system designed for N; one M/E is instantiated |
| Layout/composition as a bus-selectable source | SW-25/ADR-0056 §5; ADR-M012 `compositions` | this slice selects a layout **on** a bus; nesting a composite **as** a source is a render-plan pre-pass |
| Wipes, DVE push/squeeze, stinger | SW-24/25/26 | post-MVP waves |
| Media library + players | SW-11/12 | separate lanes |
| AFV, master gain, FTB audio, meters | SW-14; ADR-0059 | §9 keeps audio honest until this lands |
| Tally derivation/arbiter | SW-16 | plan-driven tally is its own slice |
| Macros, memories/recall masks | SW-21/22 | control-plane sequencer |
| `clean` tap as a distinct encoded rendition | §7 | the token is validated; the pre-DSK tap equals PGM in this slice (no DSK yet) |

## 13. Stage DAG (disjoint write scopes)

Rule: two stages may run concurrently only when their write scopes do not intersect. S0 is
serial (LANE-CORE). Allowed parallel pairs: {S2, S3} and {S4, S5}.

| Stage | Deliverable | Write scope (workspace-relative) | Depends on | Gate |
|---|---|---|---|---|
| **S0** | Tested core ported to this base; no runtime change | `crates/multiview-core/**`, `crates/multiview-engine/{src/switcher.rs,src/runtime.rs,src/drive.rs,src/route.rs,tests/{switcher_state_machine*,render_plan_*,route_apply}.rs}`, `crates/multiview-config/{src/switcher.rs,src/lib.rs,src/routing.rs,src/diff.rs,tests/**}`, `crates/multiview-cli/{src/cli.rs,src/main.rs,src/pipeline.rs,tests/**}`, `crates/multiview-control/{src/boot_model.rs,src/openapi_schemas.rs,src/routes/**,tests/**}`, `web/src/realtime/**`, `web/src/api/{operations*,operationStatus*}.ts`, `docs/api/openapi.json` | — | focused tests §14 + full CI; `multiview run` unchanged |
| **S1** | Bus selector + M/E config, validation | `crates/multiview-config/src/{switcher.rs,routing.rs,lib.rs,diff.rs}`, `crates/multiview-config/tests/**` | S0 | `legacy_single_program`, `switcher_passthrough`, `routing` |
| **S2** | Dual-bus render plan, Cut/Auto, overlap reject, taps | `crates/multiview-engine/src/{switcher.rs,drive.rs,runtime.rs,route.rs,lib.rs}`, `crates/multiview-engine/tests/switcher_*.rs` | S1 | `switcher_runtime` + `switcher_chaos` + soak |
| **S3** | Switcher API, `/plan`, corr, BOLA | `crates/multiview-control/src/routes/switcher.rs`, `crates/multiview-control/src/{command.rs,realtime.rs,openapi.rs,openapi_schemas.rs,routes/mod.rs}`, `crates/multiview-control/tests/switcher_routes.rs`, `docs/api/{openapi,asyncapi}.json`, `web/src/api/schema.ts` | S1 | `switcher_routes`, `corr`, authz tests + spec lockstep |
| **S4** | CLI runtime: dual compose, per-output bus, leases, take hold | `crates/multiview-cli/src/{pipeline.rs,control.rs,preview.rs,audio.rs}`, `crates/multiview-cli/tests/switcher_first_slice.rs` | S2, S3 | `switcher_first_slice`, `demand_lifecycle`, chaos |
| **S5** | SPA `/switcher` panel, monitors, output selector, correlation | `web/src/**` (except `web/src/api/schema.ts`), `web/e2e/switcher.spec.ts`, `web/src/locales/**` | S3 | `npm --prefix web run lint/test/build`, `test:e2e` |
| **S6** | ADR (R3-required), status doc, full sweep, independent review | `docs/decisions/ADR-00xx.md`, `docs/decisions/README.md`, `docs/development/production-switcher-status.md` | S4, S5 | 3-lens panel + full CI |

S6 carries the R3-required ADR record for the first slice; this note is the plan document, not
the ADR.

## 14. Tests, chaos and soak — exact commands

Derived from the repository's existing targets. No smoke scripts: every gate is a named test
target already wired into the workspace.

Focused (per changed crate; introduced targets are named by their stage):

```bash
cargo test -p multiview-core --test switcher                          # S0
cargo test -p multiview-config --test legacy_single_program           # S0
cargo test -p multiview-config --test switcher_passthrough            # S0
cargo test -p multiview-config --test routing                         # S0/S1
cargo test -p multiview-engine --test switcher_state_machine          # S0
cargo test -p multiview-engine --test render_plan_seam                # S0
cargo test -p multiview-engine --test render_plan_alloc               # S0
cargo test -p multiview-engine --test route_apply                     # S0
cargo test -p multiview-engine --test switcher_runtime                # S2
cargo test -p multiview-control --test routing_routes                 # S0
cargo test -p multiview-control --test switcher_routes                # S3
cargo test -p multiview-control --test corr                           # S3
cargo test -p multiview-control --test auth                           # S3
cargo test -p multiview-control --test realtime_authz_revocation      # S3
cargo test -p multiview-control --test whole_system_scope_guard       # S3
cargo test -p multiview-cli --test switcher_first_slice               # S4
cargo test -p multiview-cli --test demand_lifecycle                   # S4
cargo test -p multiview-cli --test program_audio_pipeline             # S4
```

Chaos (the MP-1 shape: a wedged consumer never stalls the tick). `switcher_chaos` adds the
four vectors of §7–§9: wedged PVW monitor consumer, command storm mid-transition, wedged
preview-lease teardown, and a take held on prime while a sibling output keeps emitting.

```bash
cargo test -p multiview-engine --test switcher_chaos
cargo test -p multiview-engine --test isolation
cargo test -p multiview-engine --test probe_starvation_chaos
cargo test -p multiview-engine --test cadence_hold
```

Soak. `runtime.rs` is the existing 100k-tick integrated soak/chaos proof
(`crates/multiview-engine/tests/runtime.rs:119`); `switcher_soak` is the long churn soak
(Cut/Auto/T-bar-equivalent churn with a bounded memory assertion), run on demand exactly like
the repository's other long `#[ignore]`d proofs.

```bash
cargo test -p multiview-engine --test runtime
cargo test -p multiview-engine --test switcher_soak -- --ignored --nocapture
```

Admission/perf (the 2×N union and PVW second composite stay inside the measured budget):

```bash
cargo bench -p multiview-compositor --bench composite_realtime
```

Full CI set — pre-PR and post-review, at most two workspace runs per task
([AGENTS.md](../../AGENTS.md)):

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny check
```

Web and spec lockstep:

```bash
npm --prefix web ci
npm --prefix web run lint
npm --prefix web run test
npm --prefix web run build
npm --prefix web run test:e2e -- e2e/switcher.spec.ts
cargo xtask gen-openapi && cargo xtask gen-asyncapi
npm --prefix web run generate:api && npm --prefix web run generate:events
```

Where the tool is present, the backlog's per-row mutation gate applies to the changed code:
`cargo mutants --in-diff`.

## 15. Independent review (no user approval gate)

Starting and landing the slice require no user/operator approval gate (standing authorisation;
ADR-G005 delegates routine merge to the agent). The mandatory gate is the R3 independent
review: a **three-lens cross-vendor panel** — (1) engine/invariants #1/#10, (2) API/authz/BOLA
and spec lockstep, (3) operator UX and honesty (monitors, buses, no false AFV). Findings are
repaired and re-reviewed; a `needs_revision`/`reject` verdict fails the gate. The ADR in S6
records the outcome.

## 16. Open questions / host verification

1. **PVW cost on CPU.** The second composite must fit the tick budget; the PVW rung order is
   documented but its real-hardware cost needs the bench gate on a host.
2. **Warm-set cap value.** `MAX_TIMED_LEASES` (64) bounds leases, not the operator-meaningful
   warm-set size; the slice needs a measured default.
3. **`clean` tap with no DSK.** In this slice `clean` equals `program` modulo FTB; whether the
   token should be rejected until DSK exists is a UX call (current decision: accept and
   document).
4. **Hold deadline default.** The bounded prime-hold duration needs an operator-visible
   default and an event on expiry.
