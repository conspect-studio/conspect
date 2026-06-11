# ADR-R010: Make-before-break parallel-output migration primitive — the implementable Class-2 cutover contract

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-10
- **Source briefs:** [resilience-and-av.md](../research/resilience-and-av.md) §3.2/§3.3, [management-capability-matrix.md](../research/management-capability-matrix.md) §1.3, [core-engine.md](../research/core-engine.md)
- **Supersedes / refines:** none — this **pins the execution contract** that [ADR-R004](ADR-R004.md) and [ADR-M005](ADR-M005.md) name but leave unspecified.

## Context

Invariant #11 splits every management change into Class-1 (hot, atomic scene-graph swap at a frame
boundary) and **Class-2** (a change to a *pinned* output parameter — geometry-beyond-max, codec,
pixel-format/bit-depth/chroma, GOP structure, audio track *layout*, subtitle track-*set*, canvas
res/fps — that the encoder structurally cannot reconfigure live and that forces new SPS/PPS + IDR).
[ADR-R004](ADR-R004.md) decided Class-2 is implemented as a **parallel-output migration**
("make-before-break"); [ADR-M005](ADR-M005.md) decided the API surfaces it
(`POST /api/v1/outputs/{id}/migrate`, `202 {operation_id}`, outcome on the realtime stream).

Neither pins the **primitive itself**. Two independent work items now both require it as a *shared
dependency* — and each currently stops at "the supervisor + scene-swap machinery execute it" without
that machinery being specified:

- **CTL-6** (control: parallel-output migration) — the `POST .../migrate` handler and the
  `RouteClass::Class2 → 202 {operation_id}` path in `multiview-control` (`routing.rs`,
  `routes/routing.rs`) hand a Class-2 change to the engine and report the outcome.
- **GPU-5c** (engine: closed-loop re-placement) — `PlacementController::observe` returns
  `PlacementProposal::Migrate(MigrationPlan)` / `Split(SplitPlan)` (`multiview-engine/src/placement.rs`,
  ADR-0018). The controller explicitly *proposes only*; its doc comment states "the supervisor +
  make-before-break mechanism execute the parallel spin-up + IDR-aligned cutover + teardown."

A Class-2 *config* edit (CTL-6) and a GPU *re-placement* (GPU-5c) are the **same physical operation**:
stand up a second egress with the new pinned config (or on the new device), let both run, cut
consumers over at a frame/IDR boundary, drain and stop the old one. Building it twice would risk two
subtly-different cutovers — and any defect here is a defect in the heart of the product (inv #1 + #10).
This ADR pins the one contract both consume.

The as-built engine already provides **most** of the load-bearing pieces this contract composes, so
the coordinator's lifecycle (spin-up / warm / drain / stop) is mostly assembly. **One piece does not
yet exist and is genuinely new work this contract mandates:** the cross-`Program` sink-cutover bridge
that lets the engine re-point a live sink from OLD's rendition to NEW's at SWAP. The as-built state:

- *(exists)* A **`Program`** is a self-contained, independently-supervised output actor with **its own**
  output clock, runtime, `StopSignal`, isolated `EnginePublisher`, and a **bounded drop-oldest** egress
  (`multiview-engine/src/programset.rs`). `ProgramSet::start` admits + spawns a program **without
  touching** siblings; `ProgramSet::stop` raises only that program's `StopSignal`, drains its egress,
  and joins. The supervisor **never `.await`s a program on the data plane** — it samples a wait-free
  ticks counter. SPIN-UP, WARM (counter sampling), and DRAIN/STOP all compose existing calls.
- *(exists, in `multiview-output` only)* **`PacketRouter::move_sink`** (`multiview-output/src/fanout.rs`,
  RT-12) is the runtime sink-mover: a pure routing-table re-key that re-points an existing
  `Arc<dyn PacketSink>` (keeping its identity, bounded buffer, and connection) from one rendition to
  another, at a frame boundary, **never blocking** and **never erroring** (returns `false` on a no-op).
- *(NOT built — the new work)* **No `move_sink`/`PacketRouter` is referenced in `multiview-engine`
  today**, and there is no engine-level crosspoint for "output ← program" cutover. `multiview-engine`'s
  `route.rs` carries only intra-`Program` crosspoints (`RouteIntent::Video`/`Audio`/`Subtitle`); the
  enum is `#[non_exhaustive]` *precisely* with the note that the **`output ← program`, RT-12** kind
  "can be added without a breaking change" — i.e. it is documented as a deliberate seam but **the seam
  is empty**. The SWAP step below therefore depends on infrastructure that the CTL-6/GPU-5c adapters
  must **build**: an engine-side RT-12 crosspoint that drives `PacketRouter::move_sink` across two
  `Program`s' fan-outs at a frame boundary (a cross-`Program` packet-router handle the coordinator can
  re-key). This ADR pins the contract that bridge must satisfy; it does not pretend the bridge exists.
- *(exists)* Class-1 edits already land via an **atomic double-buffered scene-graph pointer swap at a
  frame boundary** ([ADR-R004](ADR-R004.md), `CompositorDrive::set_layout`).

## Decision

**Define a single make-before-break migration primitive — a five-phase, supervisor-driven lifecycle —
and make it the only execution path for every Class-2 change, whether the trigger is a control-plane
config edit (CTL-6) or an engine-internal placement decision (GPU-5c).** It is a *coordinator over the
`ProgramSet` + scene-swap primitives* plus **one new piece of engine infrastructure the adapters must
build — the RT-12 `output ← program` crosspoint** that drives `PacketRouter::move_sink` across two
`Program`s' fan-outs (see Context: `multiview-output`'s `move_sink` exists, but it is **not yet wired
into `multiview-engine`**). The coordinator introduces **no** new data-plane *channel* into the engine
and is driven entirely off the hot path; the RT-12 crosspoint is a frame-boundary routing-table re-key,
not a channel a client can fill.

### 1. The migration value object

Both triggers reduce to one typed request the coordinator consumes:

- `target`: the desired pinned config (CTL-6: the new `EncodeProfile`/canvas/track-set) **or** the new
  device placement (GPU-5c: `MigrationPlan { from, to }` / `SplitPlan`).
- `cutover`: `Cut` (immediate at the next boundary) — the default and only mode for v1; a crossfade is
  **not** offered for Class-2 (the two egresses are distinct bitstreams, not blendable pictures).
- `consumers`: the set of `(rendition_id, sink_id)` currently attached to the OLD output that must be
  re-pointed. Sinks keep their identity across the move (`PacketRouter::move_sink`).
- `idr_aligned`: always `true` by construction — the output GOP is pinned and IDRs are driven by
  `forceIDR`, so the cutover boundary is also an IDR boundary (matches
  `MigrationPlan::idr_aligned == true`).

A migration is identified by the `operation_id` the API already returns in the `202`; its terminal
outcome (`Migrated` / `RolledBack { reason }`) rides the realtime stream (ADR-RT002 envelope), never a
synchronous response.

### 2. The five-phase lifecycle (each phase is a typestate; failure before SWAP rolls back)

1. **VALIDATE (off-thread).** Confirm the `target` is admissible *now*: capability-gate the new pinned
   config / device against the `CapabilityReport` (ADR-M007) and run the cost-model admission check
   (ADR-E007/E008) for running OLD **and** NEW concurrently. If admission fails, the migration is
   **rejected before any resource is touched** — OLD is untouched, the API reports the rejection. This
   is where "not enough encoder sessions / GPU headroom to hold both" is caught.
2. **SPIN-UP (make).** `ProgramSet::start` admits + spawns the NEW `Program` (new pinned config, or
   pinned to the new device for GPU-5c) **alongside** the running OLD one. NEW gets its own output
   clock, runtime, isolated publisher, and bounded egress. NEW spins up *cold*: it begins emitting
   valid frames on its own clock immediately (slate until its tiles fill, per the tile state machine,
   inv #2), but **no consumer is attached to it yet**. OLD keeps emitting, uninterrupted. Spin-up of
   NEW failing (encoder init error, device-lost) is `ActorExit::Failed`; the coordinator tears NEW down
   and rolls back — OLD never noticed.
3. **WARM / READY-GATE.** The coordinator (a) **pre-attaches an internal keepalive sink** to NEW's
   target rendition so that rendition is **encoding (warm)** before SWAP, then (b) waits — **off the
   data plane, by sampling NEW's wait-free ticks counter** (`ProgramHandle::ticks_counter`) — until NEW
   has emitted ≥ *N* valid frames and is at an IDR boundary (its next `forceIDR` tick). It never
   `.await`s NEW's egress. A WARM timeout (`migration_warm_timeout`) elapsing → rollback. The keepalive
   sink is a discard/no-op `PacketSink` whose sole purpose is to make NEW's rendition *active* (≥1 sink
   ⇒ encode-once-mux-many turns its single encode on, inv #7): without it NEW's rendition would be
   **cold** (no sinks ⇒ not encoded), and the SWAP's `move_sink` onto a cold rendition would itself
   trigger the *first* encode at the cutover instant (the documented `PacketRouter::move_sink`
   cold-rendition behaviour: "if `to` was cold the driver will encode it once on the next tick"). This
   gate is what makes the cut seamless **and zero-new-encode**: NEW is *already producing a valid
   keyframe-led bitstream into a live (keepalive) sink* before any real consumer sees it.
4. **SWAP (break-after-make, the only critical instant).** At a single frame/IDR boundary, the
   coordinator drives the **new engine-side RT-12 `output ← program` crosspoint** (see Context — the
   piece the adapters build) to re-point **all** `consumers` from OLD's rendition(s) to NEW's via
   `PacketRouter::move_sink` — a pure routing-table re-key. Because `move_sink` is a non-blocking,
   non-erroring table operation **and NEW's target rendition is already encoding** (it was made warm in
   WARM by the keepalive sink — encode-once-mux-many, inv #7), the re-point spawns **zero** new encodes
   and drops **zero** output frames. (Without the WARM-phase keepalive this would not hold: the *first*
   consumer arriving on a cold rendition is what turns its encode on, so the keepalive pre-attach is the
   load-bearing precondition for the zero-new-encode guarantee, not an optimisation.) The keepalive sink
   is then dropped once ≥1 real consumer is attached (the rendition stays warm via the real consumer).
   This is the moment a consumer sees the discontinuity (new SPS/PPS + IDR; HLS gets a correctly-signalled
   `EXT-X-DISCONTINUITY`); OLD is no longer fed those consumers but **is still emitting** to nothing yet.
5. **DRAIN + STOP (break).** After SWAP, OLD has no attached consumers. The coordinator drains OLD's
   in-flight egress (bounded; the egress thread ends on channel close — the `SINK_WEDGE_GRACE` posture
   already in `Program::Drop`) and then `ProgramSet::stop`s OLD: raise only OLD's `StopSignal`, join its
   egress thread if finished (a wedged thread is *shed*, never blocks teardown), free its
   encoder/device resources. Siblings and NEW are untouched. The migration emits its terminal
   `Migrated` event with the freed `operation_id`.

### 3. Rollback path (any failure at phases 1–3 is invisible; phase 4 is the point of no cheap return)

- **Before SWAP (phases 1–3):** rollback is free and total — tear down the half-built NEW `Program`
  (`ProgramSet::stop` on NEW), leave OLD exactly as it was. No consumer ever moved, so **no consumer
  saw anything**. The API reports `RolledBack { reason }` (admission-denied / spin-up-failed /
  warm-timeout / device-lost). This is the common failure mode and it is *non-disruptive by
  construction*.
- **After SWAP (phases 4–5):** the consumers are on NEW. "Rollback" here is **not** a quiet undo — it
  is a *second, forward* make-before-break migration back to an OLD-config Program (consumers already
  paid one discontinuity; undoing costs a second one). The coordinator therefore treats post-SWAP as
  *committed* and only re-migrates on an explicit operator/auto-recovery trigger, never silently. If
  NEW *fails* after SWAP it is a normal supervised program failure: the `Supervisor` restarts NEW under
  its `RestartPolicy` (capped backoff) — the consumers ride NEW's own slate/last-good during the
  restart (inv #1/#2), exactly as any running program would.

### 4. How the primitive preserves invariant #1 (output never falters)

- **Two independent clocks, never coupled.** OLD and NEW each own a fixed-cadence output clock; neither
  ever waits on the other. At no phase does any clock block — SPIN-UP adds a clock, SWAP is a routing
  re-key between two already-running clocks, STOP removes a clock. There is **no instant where a
  consumer's rendition has no producing clock**: OLD produces until SWAP, NEW produces from before SWAP.
- **NEW is keyframe-ready before cutover.** The WARM gate guarantees NEW emits a valid IDR-led
  bitstream *before* a consumer is moved onto it, so the consumer's first NEW packet is decodable — no
  black gap, no rebuffer beyond the format-mandated discontinuity.
- **The cut is a non-blocking table op.** The RT-12 crosspoint's `move_sink` cannot stall; it returns
  immediately and never errors. A frame is never dropped at the cut (the boundary is an IDR tick of
  NEW's own clock), and because NEW's rendition was made warm in WARM the cut spawns no encode at the
  critical instant — the encode was already paid for one tick earlier.
- **Admission is checked first.** Running both egresses concurrently is cost-gated in VALIDATE, so the
  migration cannot starve the running OLD program of encoder sessions / GPU headroom mid-flight.

### 5. How the primitive preserves invariant #10 (no back-pressure)

- **The coordinator runs on the control/IO plane, off the data plane**, and drives the lifecycle by
  **sampling wait-free counters** (`ProgramHandle::ticks_counter`, `egress_dropped`) — it **never
  `.await`s a `Program`'s egress** and the supervisor never awaits a program task on the data plane
  (the as-built `ProgramSet` contract).
- **No new engine-inward channel.** The primitive composes `ProgramSet::start`/`stop` + wait-free reads
  with the **new RT-12 `output ← program` crosspoint** that the adapters build (which itself is just a
  frame-boundary `PacketRouter::move_sink` table re-key across two `Program`s — an in-process call, not
  a channel). The new infrastructure adds a *crosspoint*, **not** an inward channel a slow consumer
  could fill, so the isolation guarantee is preserved by construction and must be re-proven by the chaos
  gate when the bridge lands. The Class-2 trigger arrives over the **existing** lock-free desired-state
  hand-off (ADR-W008) for CTL-6, or as a returned `PlacementProposal` value for GPU-5c — in both cases
  the engine *pulls* the request, a slow control client can never push into or stall the engine.
- **Outcome is broadcast, not awaited.** The terminal `Migrated`/`RolledBack` event is published on the
  drop-oldest broadcast (ADR-RT004/I001); the migration's progress is reported to the API via the
  `operation_id` on the realtime stream — the coordinator never blocks on a client consuming it.

### 6. GPU-5c vs CTL-6 — one primitive, two adapters

- **CTL-6** builds the `target` from the new pinned `EncodeProfile`/canvas/track-set and the
  `consumers` from the output's current sinks; the API classifier (`routing.rs`) already returns
  `Class2 → 202 {operation_id}` and the handler invokes the primitive.
- **GPU-5c** builds the `target` from `MigrationPlan { from, to }` (the NEW `Program` is pinned to `to`)
  — the pinned *config* is **unchanged**, only the device moves. The placement controller's anti-storm
  damps (cooldown / per-GPU budget / min-gain, ADR-0018 §4.6) gate *whether* a migration is proposed;
  the primitive is *how* an accepted one executes. `Split` is the same lifecycle with a NEW that is a
  two-GPU split island (ADR-0018 §20).
- Both feed the *identical* five-phase coordinator. The only difference is the value object's `target`;
  the cutover, drain, rollback, and invariant guarantees are shared verbatim.
- **The new RT-12 `output ← program` crosspoint + keepalive-sink mechanism is built once, shared by
  both.** Whichever adapter lands first builds the engine-side bridge (the cross-`Program`
  `PacketRouter::move_sink` handle and the WARM-phase keepalive sink — see Context and §2 phases 3–4),
  and the second adapter reuses it. This is the one piece of genuinely new engine infrastructure this
  ADR mandates; the rest of the coordinator is assembly over the as-built `ProgramSet` + scene-swap.

## Alternatives considered

- **In-place reconfigure of the running output** (rejected, [ADR-R004](ADR-R004.md) verification):
  NVENC cannot reconfigure GOP structure / sync-async mode; VideoToolbox cannot change resolution live
  at all. Class-2 *by definition* is the set the encoder cannot absorb in place.
- **Break-before-make** (stop OLD, then start NEW): a guaranteed output gap on the migrated consumers —
  violates inv #1. Rejected outright.
- **A separate migration mechanism for GPU re-placement vs config edits** (rejected): they are the same
  physical operation; two implementations means two cutovers to keep correct and two places for an
  inv-#1/#10 defect to hide. One primitive, two thin adapters.
- **Crossfade cutover for Class-2** (rejected for v1): the two egresses are independent bitstreams, not
  blendable canvases; a crossfade would require decoding both. Cut at an IDR boundary is the honest
  primitive. (Class-1 scene swaps keep their Cut/Crossfade choice — that is a different mechanism.)
- **Synchronous `202`-then-block** (rejected): the API returns `202 {operation_id}` and the outcome
  rides the realtime stream; blocking a request thread on a multi-second warm-and-cut would couple a
  client to the engine.

## Consequences

- **One shared, testable primitive.** CTL-6 and GPU-5c both depend on it; it can be soak/chaos-tested
  once (start↔swap↔drain↔stop under load; admission-deny; warm-timeout rollback; spin-up-fail rollback;
  wedged-sink shed at drain) and both consumers inherit the guarantees. The chaos gate must prove the
  coordinator cannot stall a running clock at any phase.
- **Cost during cutover.** Holding OLD + NEW concurrently transiently doubles that output's
  encoder-session / GPU footprint; VALIDATE refuses a migration that would not fit, surfacing a clear
  rejection rather than a mid-flight starvation. This is the documented price of make-before-break.
- **Consumers see exactly one discontinuity** per Class-2 change, correctly signalled (HLS
  `EXT-X-DISCONTINUITY`; RTMP/many players reconnect) — the migration banner (ADR-M005) is mandatory in
  the UI. Reset-lite (in-max NVENC resolution) and Class-1 stay on their existing in-place / scene-swap
  paths; only true Class-2 uses this primitive.
- **Post-SWAP is committed.** Undoing a completed migration is itself a second forward migration (a
  second discontinuity), never a silent revert — operators are shown this in the plan/dry-run.
- **No new public crate or data-plane channel — but one new engine crosspoint to build.** The primitive
  lives in `multiview-engine` as a coordinator over `ProgramSet` + scene-swap, and the follow-up
  implementation work (CTL-6, GPU-5c) wires the two adapters to this one contract. That work is **not
  purely a coordinator**: it must also build the engine-side RT-12 `output ← program` crosspoint that
  bridges `PacketRouter::move_sink` (today defined only in `multiview-output`, and **not referenced in
  `multiview-engine`**) across two `Program`s' fan-outs, plus the WARM-phase keepalive sink. The
  `RouteIntent` enum in `route.rs` is already `#[non_exhaustive]` with this exact RT-12 seam noted, so
  the addition is non-breaking — but it is new code, not an existing call the coordinator merely
  invokes. No new *channel* into the engine is introduced; the chaos gate re-proves inv #1/#10 when the
  bridge lands.
