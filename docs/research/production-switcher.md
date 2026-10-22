> **Design brief — Production Switcher Layer.** Authoritative research/design record for the
> live production switcher layer (M/E buses, transitions, keyers, media players, tally,
> macros). Produced by a verification-hardened multi-agent research workflow (2026-06-11).
> This is a **design for an unbuilt feature** — sections describing current Multiview
> behaviour are verified against the code; every reference to *existing* code names a real,
> verified path.

> **Vendor posture.** This brief uses generic industry vocabulary only (M/E stage, PGM/PVW,
> T-bar, dip, wipe, stinger, upstream/downstream keyer, clean feed, media library, macro,
> memory). Where a behaviour is industry consensus without a governing standard it is
> labelled **de-facto industry practice**. Cited external references are open/published
> documents exclusively: the TSL UMD protocol specification, AMWA NMOS IS-07, SMPTE/EBU
> practice documents, SCTE-35/104, the obs-websocket protocol document, CasparCG AMCP, and
> the FFmpeg documentation. See [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md).

# Multiview — Production Switcher Layer

**Buses are routing state and transitions are pure per-tick render functions: the protected
output clock samples switcher state once per tick at the existing frame-boundary control
seam and composes exactly one valid frame, so nothing in the switcher — no transition, no
keyer, no T-bar move, no macro, no monitor — can pace, stall, or back-pressure the engine
(invariants #1 and #10 hold by construction).**

Companion brief: [media-playout.md](media-playout.md) (media library, media players, alpha
media — referenced, not duplicated, here). Decision records:
[ADR-0054](../decisions/ADR-0054.md) (architecture),
[ADR-0055](../decisions/ADR-0055.md) (transition engine),
[ADR-0056](../decisions/ADR-0056.md) (keyers),
[ADR-0057](../decisions/ADR-0057.md) (media library/players),
[ADR-0058](../decisions/ADR-0058.md) (alpha media path),
[ADR-0059](../decisions/ADR-0059.md) (switcher audio),
[ADR-M012](../decisions/ADR-M012.md) (management resource model),
[ADR-RT008](../decisions/ADR-RT008.md) (realtime),
[ADR-W021](../decisions/ADR-W021.md) (control surface + SPA),
[ADR-P007](../decisions/ADR-P007.md) (preview bus + cue),
[ADR-T015](../decisions/ADR-T015.md) (timing),
[ADR-C007](../decisions/ADR-C007.md) (color law),
[ADR-MV006](../decisions/ADR-MV006.md) (derived tally + TSL reconciliation),
[ADR-R011](../decisions/ADR-R011.md) (resilience).
Backlog: [production-switcher-backlog.md](../development/production-switcher-backlog.md)
(`SW-` prefix).

## 0. Headlines

1. **An M/E lives inside ONE program.** Mix/wipe math needs both scenes in one composite —
   it is *impossible across encoded streams* — so PGM and PVW are two scene states under
   one `OutputClock`, evaluated by one `CompositorDrive` per tick. The PVW bus is a
   **second compose against the SAME `Tick`** inside the same program, never a second
   `ProgramSet` program (independent clocks would break frame-aligned take). §4.1.
2. **Build vs route.** Anything that must blend rides the in-program render plan; anything
   that genuinely needs its own encode (aux buses, independent feeds) is an ADR-0030
   program reached through the RT-12 output←program crosspoint
   (`RouteIntent` is `#[non_exhaustive]` for exactly this —
   `crates/multiview-engine/src/route.rs:50-54`; `PacketRouter::move_sink` is BUILT in
   `crates/multiview-output/src/fanout.rs:212` with zero engine callers). §4.4.
3. **The switcher state machine is a pure value machine in `multiview-engine`**, the
   established salvo/tally/alarm house style: mutate own state, return state for the
   engine to apply at the frame boundary. It sits exactly where `CommandDrain` already
   lives — the per-tick control hook (`crates/multiview-engine/src/runtime.rs:430-439`) —
   which is **widened to receive the `Tick`** (back-compat preserved) so transition
   progress is a deterministic `f(tick.index)`. §4.2.
4. **A render-plan resolver replaces per-tick layout mutation.** `compose()` already
   derives a transient placement list from `layout.cells` each tick out of pooled scratch
   (`crates/multiview-engine/src/drive.rs:449-494`, `ComposeScratch` at `:670`). That
   derivation is promoted to an injectable resolver
   `fn(scene_states, switcher_state, tick) -> placements` — synchronous,
   allocation-pooled, invariant #1 by construction. §4.3.
5. **DSKs and FTB move ON-CLOCK.** Downstream keyers and fade-to-black must be
   frame-coordinated with transition state, so they composite inside the engine render
   plan. The existing **off-thread egress overlay bake**
   (`crates/multiview-cli/src/overlays.rs:208`,
   `apply_overlays_to_nv12` in `crates/multiview-compositor/src/overlay/subpass.rs:1181`)
   is **not** frame-coordinated and stays for monitoring-style overlays only (labels,
   meters, captions, watermark). This split is explicit and budgeted. §4.5.
6. **Clean feed = a tap point along ONE program's render plan**, not an extra program.
   The pre-overlay canvas `Arc` is already published per tick — the preview `ProgramSlot`
   receives the canvas **before** the off-thread overlay bake
   (`crates/multiview-cli/src/pipeline.rs:2122`) — that is the verified clean-feed anchor.
   Bus tap taxonomy: `{program (post-FTB), dirty (post-DSK, pre-FTB), clean (pre-DSK),
   preview, me[n], aux[j]}`, adopting ADR-0037's CLEAN/DIRTY vocabulary. §4.6.
7. **Mix ships with zero compositor changes** via the flat-list fast path: both scenes'
   cells in one z-ordered tile list with the incoming side's opacity ramped per tick —
   per-tile opacity is wired end-to-end into the premultiplied linear-light `over` blend
   (`crates/multiview-compositor/src/blend.rs:1-10`). Scenes with internal overlap
   (PiP-over-PiP) take the correct scene pre-render path (2 extra composites, NV12
   intermediates), detected at arm time. §5.2.
8. **Transitions degrade, never stall**: an in-flight transition is program-affecting and
   is **never shed mid-flight** (complete-then-degrade); under overload AUTO demotes to
   CUT at arm/admission time; the PVW composite is admission-accounted and shed before any
   program tile (a new ladder rung between the preview and tile rungs). §5.6, §15.
9. **Keyer math has verified insertion points** that preserve invariant #8: luma key alpha
   on post-range-expand code-value Y, chroma distance in linear RGB after YUV→RGB, both
   multiplied in before the premultiplied linear-light `over`
   (CPU: `fold_tile_into_band`, `crates/multiview-compositor/src/pipeline.rs:1075`;
   GPU: `composite.wgsl` `TileParams` carries padding fields at
   `crates/multiview-compositor/src/gpu/shaders/composite.wgsl:14-21`). Fill+key is one
   extra per-tile source reference — the GPU already binds per-tile texture-array layers. §6.
10. **The pop-free audio nucleus already exists and is engine-tested** — per-sample
    equal-power `GainRamp` (`crates/multiview-audio/src/mixer.rs:35`) and
    `ProgramBus::repoint_crossfade` (`crates/multiview-audio/src/program.rs:217`) — but
    `Command::RouteAudio` is HELD in production
    (`crates/multiview-cli/src/control.rs:933-953`): the bus is owned by the bake-consumer
    thread with no control seam. The first audio slice is that seam, the twin of the
    subtitle seam (`SubtitleRouteHandle`, `crates/multiview-cli/src/captions.rs:130-153`).
    AFV ramps derive from the SAME switcher tick window as video. §8.
11. **Tally is a per-tick pure derivation** over the composition graph — recursive
    *contributes-to-program*: both transition sources mid-transition, keyer fill AND key
    sources of on-air keyers, stinger media while it covers, sources inside on-air
    multi-box compositions and nested M/Es. Facts feed the EXISTING pure `TallyArbiter`
    (`crates/multiview-engine/src/tally/arbiter.rs:145`) so internal and external
    (TSL/IS-07/router) facts merge under one conflict policy; the arbiter is finally
    wired into the run loop, replacing the `SetTallyOverride` echo. §9.
12. **Memories extend `Salvo`; macros are a control-plane sequencer.** Salvo
    (`crates/multiview-config/src/salvo.rs:59`; engine arm/take/cancel BUILT,
    `crates/multiview-engine/src/salvo.rs:158-189`) gains recall-scope masks. Macros
    replay ordinary `Command`s with wait steps off-clock (invariant #10), desugaring onto
    the same engine intents as live commands — one apply path. Batched multi-op takes get
    a frame-boundary batch semantic (the openly-published obs-websocket `SERIAL_FRAME`
    request-batch execution type maps 1:1 onto the existing drain). §10.
13. **All timing is exact**: durations are integer frame counts at the exact rational
    output cadence; the API accepts milliseconds and converts via exact rationals; AFV
    sample budgets derive from `SampleClock`; never float fps or float seconds
    ([ADR-T015](../decisions/ADR-T015.md)). §13.
14. **No new crate** ([ADR-0054](../decisions/ADR-0054.md), devices/ADR-I004 precedent):
    state machine + render plan in `multiview-engine`, primitives in
    `multiview-compositor`, shared types in `multiview-core`, schema in
    `multiview-config`, control domain in `multiview-control`, panel in `web/`.
    `conventions.md` §3/§4 need no edit. §16.
15. **MVP = one M/E, designed for N** (§18): PGM/PVW, cut/auto/T-bar, mix+dip+FTB with
    audio-follow, flip-flop, 2 linear/luma DSKs with NV12+A stills, media library + 2
    players, program audio AFV + master gain + meters, clean program tap, derived
    red/green tally through the arbiter, REST/WS surface with plan/take, SPA panel with
    shortcuts, macros + memories. Wipes/DVE/stinger/chroma/aux/multi-M/E are phased
    post-MVP.

## 1. Operator model

The operator-facing model is the de-facto industry-standard production switcher, expressed
in generic vocabulary (see the vendor-posture note above):

- **M/E (mix/effects) stage.** One switching stage with two background buses — **program
  (PGM)**: what is on air; **preview (PVW)**: what is staged next — plus N **upstream
  keyers (USKs)** that participate in transitions, then (at the canvas level, shared
  across the M/E hierarchy) M **downstream keyers (DSKs)**, then **fade to black (FTB)**
  as the final master stage. Multiview models one M/E for the MVP with the type system
  designed for N (nested M/Es re-enter as bus-selectable sources, §6.5).
- **Cut** swaps PGM and PVW crosspoints instantly at a frame boundary. **Auto** runs the
  armed transition at its stored rate. The **T-bar** is a manual absolute progress control
  over the same transition function. All three are *the same state machine*; cut is the
  degenerate 1-frame case.
- **Flip-flop** (de-facto industry practice): on transition completion, PGM and PVW swap,
  so PVW always holds "what just left air / what goes next". Default ON, configurable per
  M/E. **Direct program punch** (selecting a source straight onto PGM, a hot cut) is
  supported.
- **Preview-transition** rehearses the armed transition on the PVW composite only —
  program output is untouched.
- **Next-transition arming** selects which elements the next transition moves:
  `{background, key[1..n]}` in any combination. Key-only transitions (background unarmed)
  work. Each DSK additionally has independent **TIE** (join the next background
  transition), **CUT** (instant on/off), and **AUTO** (mix at the DSK's own rate).
- **FTB** sits *after* the DSKs and fades everything — program, keys, DSKs — together at
  its own rate, with an optional audio-follow flag (master audio fades on the same window,
  §8.3). While at black the FTB control indicates the engaged state (de-facto industry
  practice).
- **Aux buses** are independently routable outputs that can select any source, any bus
  tap, or a media player. **Clean feed** is the program *before* the DSKs (§4.6).
- **Media players** are bus-selectable channels playing assets out of the **media
  library** (stills and clips, with alpha where the format carries it); a **multi-box
  composition** (a background plus z-ordered boxes) is likewise a bus-selectable source
  (§6.5). Detail: [media-playout.md](media-playout.md).
- **Macros** are authored command sequences with wait steps (macro *recording* —
  capturing live operator commands into a macro definition — is explicitly post-MVP,
  §10.2); **memories** (snapshots) store and recall switcher state with recall-scope
  masks (§10).

## 2. Vocabulary: the switcher PVW bus is not the preview subsystem

Two distinct things share the word "preview"; this brief and every ADR/API keep them
disjoint ([ADR-P007](../decisions/ADR-P007.md)):

| Term | What it is | Namespace |
|---|---|---|
| **Preview bus (PVW)** | *Operator switching state*: the staged scene of an M/E, rendered as a second composite per tick, target of cut/auto/T-bar. | `/api/v1/switcher/*`, `switcher` realtime topic |
| **Preview subsystem** | *Monitoring*: the existing taps/JPEG/WHEP machinery (ADR-P001..P006) that lets a UI look at inputs/program/outputs. Strictly isolated, never program-affecting. | `/api/v1/preview/*`, existing topics |

The PVW bus *uses* the preview subsystem to be seen (a `ProgramSlot`-pattern still per bus
tap, §12.2), but it is not part of it. In prose: "PVW"/"preview bus" always means the
switcher bus; "preview subsystem"/"monitoring" always means ADR-P001..P006.

## 3. As-built substrate inventory (verified 2026-06-11)

Every row verified by opening the file in this worktree. The switcher builds **on** these
seams; nothing here is aspirational. Labels: **BUILT** (wired into `multiview run`),
**SCAFFOLD** (compiles + tested, zero production callers), **DOC-ONLY**.

| Surface | Status | Verified path | Switcher relevance |
|---|---|---|---|
| Per-tick run loop: pace → `OutputClock::tick()` → control hook → `compose(tick)` → publish | BUILT | `crates/multiview-engine/src/runtime.rs:401-458` (hook invoked at `:439`); `clock.rs:136` (`Tick{index, pts}`), `:192-227` (`out_pts = f(tick)`) | The frame-boundary seam the switcher state machine occupies. Hook is `FnMut(&mut CompositorDrive<Nv12Image>)` — **receives no `Tick`** (`runtime.rs:401`); widening it is SW work. |
| `CompositorDrive`: `set_layout` / O(1) `rebind_cell` / `compose` from pooled scratch | BUILT | `crates/multiview-engine/src/drive.rs:318`, `:376`, `:449-494`; `ComposeScratch :670`; `cell_dst_rect :728` | `rebind_cell` is the live crosspoint primitive; the per-tick placement derivation is what the render-plan resolver generalizes. |
| Compositor tile contract: integer dst rect + uniform opacity, z-sorted flat list; premultiplied linear-light `over` | BUILT (CPU + wgpu) | `crates/multiview-compositor/src/pipeline.rs:413-427` (`Tile`), `:1075` (`fold_tile_into_band`); `blend.rs:1-10`; GPU `gpu/compositor.rs` (`MAX_TILES=64` at `:36`, pass-chaining template `composite_with_overlays :326`); `gpu/shaders/composite.wgsl:14-21` (`TileParams` padding) | Mix needs nothing new; wipes/DVE/keys extend `Tile` + both kernels. CPU perf gate: 40 ms/tick budget (`benches/composite_realtime.rs:39`, 1280×720@25 galleries). |
| `Cell.crop` / `Cell.rotation` (quarter-turn) / `FitMode` | SCAFFOLD (type-level) | `crates/multiview-core/src/layout.rs:116-145`; config mapper spreads defaults — "does not yet surface them" (`crates/multiview-config/src/lib.rs:543-546`) | Modeled + validated but **ignored by the render path** today; DVE work wires them. |
| Command bus + frame-boundary `CommandDrain`, coalesced O(1) repoints (32/tick, 256 backlog) | BUILT | `crates/multiview-control/src/command.rs:119` (13 variants); `crates/multiview-cli/src/control.rs:598` (`MAX_REPOINTS_PER_TICK`), `:604`, `:755` | The proven non-back-pressuring apply path all switcher verbs ride. |
| `RouteApplier::apply_video` / `apply_audio` (coalesced last-wins per destination) | BUILT (video) / SCAFFOLD (audio) | `crates/multiview-engine/src/route.rs:297`, `:358-375` (note `RouteIntent::Audio` `gain_db`/`mute` destructured away at `:367`) | Audio apply has zero production callers — `Command::RouteAudio` is HELD with a warn (`crates/multiview-cli/src/control.rs:933-953`). |
| Pre-overlay clean canvas publish (`ProgramSlot`) | BUILT | `crates/multiview-cli/src/preview.rs:28` (type), `pipeline.rs:2122` (`preview.store(...)` **before** the bake consumer) | The verified clean-feed anchor (§4.6) and the per-bus monitor pattern (§12.2). |
| Off-thread overlay bake (labels, meters, badges, captions, watermark) | BUILT | `crates/multiview-cli/src/overlays.rs:208` (`OverlayBaker`); `crates/multiview-compositor/src/overlay/subpass.rs:1181` (`apply_overlays_to_nv12`); SDF house style `rect_coverage :853`, `point_segment_distance :979` | Stays as the **monitoring** overlay path; DSK/FTB move on-clock (§4.5). The SDF style is the wipe-mask template. |
| `ProgramSet` (N supervised programs, own clocks, shared `TimeSource`) | BUILT (runs exactly one "main" program) | `crates/multiview-engine/src/programset.rs:487`, `:535`; CLI drives one program (`pipeline.rs:1573-1585` doc) | Reserved for aux/independent feeds. `programs:` config root (MP-5) is unbuilt — a named prerequisite slice (§11.1). |
| RT-12 output←program crosspoint | SCAFFOLD (empty seam) | `PacketRouter::move_sink` `crates/multiview-output/src/fanout.rs:212/:331`, `EncodeOnceDriver :299`; `RouteIntent` `#[non_exhaustive]` "output ← program, RT-12" comment `crates/multiview-engine/src/route.rs:50-54`; no `Command::RouteOutput` exists | The aux-bus routing seam ([ADR-R010](../decisions/ADR-R010.md) names it; the switcher pass finally builds it — post-MVP, §18). |
| Salvo arm/take/cancel (atomic frame-boundary batch) | BUILT (engine + config + REST; CLI drain applies source recalls) | `crates/multiview-engine/src/salvo.rs:69/:113/:158/:169/:189`; `crates/multiview-config/src/salvo.rs:59` | The memory/snapshot seed (§10.1). |
| `TallyArbiter` + profiles + GPIO model; TSL v3.1/4.0/5.0 codecs | SCAFFOLD (pure, unwired; `SetTallyOverride` echoes) | `crates/multiview-engine/src/tally/arbiter.rs:145/:185`; `crates/multiview-input/src/tsl/{v31,v40,v50}.rs`; `crates/multiview-output/src/tsl/`; cross-reference drift: `tally/mod.rs:3` cites ADR-MV001 (the alarm ADR) for the arbiter | §9 wires the arbiter; [ADR-MV006](../decisions/ADR-MV006.md) reconciles the codecs against the published TSL spec. |
| Audio: `AudioStore`/`SampleClock`/`Mixer`/`ProgramBus`; equal-power `GainRamp`; `repoint_crossfade` | BUILT (behind `--program-audio`; control of it SCAFFOLD) | `crates/multiview-audio/src/program.rs:99/:217/:306/:316`; `mixer.rs:35/:254`; `SwitchTier`/`ApplyClass` `program.rs:40-70` | The AFV nucleus (§8). `Event::AudioMeter` exists with zero production emitters (`crates/multiview-events/src/event.rs:111/:1138/:1260`). |
| File ingest: `ingest_loop`/`open_and_stream`, `PtsWallClock` pacing, EOF `HoldForever` | BUILT | `crates/multiview-cli/src/pipeline.rs:5533`, `:5812`, `:6189`, `:1187` | Media players extend **this** path — explicitly NOT the unwired `multiview-input` `IngestPump` scaffold (the divergent twin). `Demuxer::seek` exists with zero callers (`crates/multiview-ffmpeg/src/demux.rs:549`). |
| Framestore: lock-free multi-reader `TileStore`, `read_at` latch-on-tick, state ladder, `is_primed` | BUILT | `crates/multiview-framestore/src/tile.rs:311` (`is_primed`) | Stores are `Arc`-shared by design: a PVW bus samples the SAME stores with **zero extra decode**. |
| Cue / pre-warm worker (ADR-P004) | DOC-ONLY | only a degradation rung mentions it; control registers `no_whep()` (`crates/multiview-control/src/state.rs:542`) | Built alongside the PVW bus — it IS the PVW arm mechanism (§12.1). |
| Degradation `ControlLoop` / `PlacementController` | SCAFFOLD | `crates/multiview-engine/src/degrade.rs:61`; `placement.rs:261` (no run-path callers); live shedding = egress drop-on-overload only | Admission accounting for PVW/transition composites needs this finally wired (§15.3). |
| Live-apply classifier + plan/take | BUILT | `crates/multiview-control/src/routing.rs:222` (`classify`); `routes/routing.rs` plan/take split | The invariant-#11 template every switcher verb follows (§11.2). |
| Realtime: envelope, snapshot-then-delta, conflated lanes | BUILT — but `$subscribe`/`$set_rate`/`$resume` are **types-only**: the WS session never reads inbound frames | `crates/multiview-control/src/realtime.rs:878-928` (`run_ws_session` send-only loop); `CorrKey::for_command :106-145` (every `Route*` returns `None` — uncorrelated) | [ADR-RT008](../decisions/ADR-RT008.md) pins publisher-side 30 Hz conflation now, `$subscribe` as a follow-on; switcher commands get NEW `CorrKey` variants (do not repeat the `Route*` gap). |
| Subtitle re-point seam (`SubtitleRouteHandle`, polled by the bake consumer) | BUILT | `crates/multiview-cli/src/captions.rs:130-153` | The exact template for the audio control seam (§8.1). |
| Discrete dwell steppers (`RoundRobin`/`FreezeTile`) | SCAFFOLD | `crates/multiview-engine/src/cycle.rs` (value machines, unwired) | Confirms: there is **zero video animation machinery** anywhere; the transition engine is net-new. |

**Stale-brief drift flags (per CLAUDE.md §8 — code wins, briefs lag).** The as-built tables
in [multi-program.md](multi-program.md) and [decoupled-routing.md](decoupled-routing.md)
predate the RT-/MP- landings: `ProgramSet`, salvo arm/take/cancel, `RouteIntent`/
`RouteApplier`, `repoint_crossfade`, the routing plan/take classifier, TSL codecs, and the
X.733 alarm machine have all landed since those briefs were written. This brief cites the
code directly; do not cite those tables for baseline claims. Additionally,
`crates/multiview-engine/src/tally/mod.rs:3` cites ADR-MV001 for the arbiter where
ADR-MV002 is the tally ADR (code-comment fix is a backlog lane item, noted in
[ADR-MV006](../decisions/ADR-MV006.md)).

## 4. Architecture

### 4.1 M/E inside ONE program

A transition is per-pixel math over **both** scenes: `mix(A, B, t)`, a wipe mask selecting
between A and B, a DVE moving B over A. That math requires both scenes as composable
images **in one composite pass** — it cannot be performed across two independently encoded
streams without decoding them again (paying latency, generation loss, and a decode the
engine never budgeted). Therefore:

- An **M/E stage lives inside one ADR-0030 program**, evaluated by `CompositorDrive` at
  the output tick. PGM and PVW are two *scene states* (source crosspoints + layer state)
  under **one clock**.
- The **PVW bus is a second compose against the SAME `Tick`** in the same program. Tile
  stores are lock-free multi-reader `Arc`s (§3), so the PVW composite costs zero extra
  decode — only the second composite itself, which is admission-accounted (§15.1).
- A second `ProgramSet` program is explicitly **rejected** for PVW: each program owns an
  independent `OutputClock`, and two clocks cannot guarantee that "take" lands on the same
  frame on both sides. Frame-aligned take is the whole product here.

### 4.2 The switcher state machine — pure value machine at the frame-boundary seam

House style (salvo `crates/multiview-engine/src/salvo.rs`, tally arbiter, alarm engine):
a `SwitcherState` value machine in `multiview-engine` that consumes operator intents
(drained from the existing bounded command bus) and **returns** the per-tick resolved
state for the engine to apply. It performs no I/O, takes no locks shared with producers,
and never awaits.

It runs at the per-tick control seam — the hook `run_inner` invokes between
`clock.tick()` and `drive.compose(tick)`
(`crates/multiview-engine/src/runtime.rs:430-439`). Today that hook is
`FnMut(&mut CompositorDrive<Nv12Image>)` and **receives no `Tick`**
(`runtime.rs:401`), so it cannot know which tick it mutates for. The hook signature is
**widened to receive the `Tick`** (`FnMut(&mut CompositorDrive<Nv12Image>, Tick)`), with a
back-compat adapter for existing callers. This is the only `runtime.rs` change the
switcher needs, and it touches the invariant-#1 seam — it ships with the chaos/soak proof
pattern the MP-1 tests established (wedge a consumer, sibling keeps ticking) plus
deterministic `ManualTimeSource` transition-timing tests ([ADR-R011](../decisions/ADR-R011.md)).

Transition progress is a **pure function of the tick index**:

```
progress(tick) = clamp_0_1( (tick.index − start_index) / duration_frames )
```

as an exact rational — `duration_frames` is an integer frame count at the output cadence;
the API accepts milliseconds and converts via exact rationals
([ADR-T015](../decisions/ADR-T015.md)). No wall clock, no floats, no stored "elapsed"
accumulators. A missed tick can never desynchronize a transition because progress is
recomputed from the index, exactly as `out_pts = f(tick)` is
(`crates/multiview-engine/src/clock.rs:192`).

### 4.3 The render-plan resolver

`CompositorDrive::compose` already rebuilds a transient placement list from
`layout.cells` every tick out of pooled scratch
(`crates/multiview-engine/src/drive.rs:449-494`). The switcher **promotes that derivation
to an injectable resolver**:

```
fn resolve(scene_states: &SceneStates, switcher: &SwitcherTickState, tick: Tick)
    -> &Placements   // allocation-pooled, like ComposeScratch
```

- **Idle** (no transition in flight): resolves the PGM scene exactly as today — the
  resolver degenerates to the current per-tick derivation; zero behavioral change.
- **Mix in flight, no internal overlap**: the flat-list fast path — both scenes' cells in
  one z-ordered list, incoming opacity = `progress(tick)` (§5.2).
- **Scene pre-render** (overlapping scenes, wipes, DVE): the resolver emits a small
  ordered plan of passes (scene A → intermediate, scene B → intermediate, final blend),
  fused on-GPU via the existing pass-chaining template
  (`composite_with_overlays`, `crates/multiview-compositor/src/gpu/compositor.rs:326`)
  with **one** NV12 readback per tick; on CPU the passes run banded within the measured
  budget (§15.2).
- The resolver is synchronous, never allocates in steady state (scratch-pool discipline),
  and is the **only** place per-tick switcher geometry/opacity is computed. Per-tick
  `set_layout` spam is explicitly rejected: it re-validates and clones the `Arc` per call
  — allocation on the hot path.

`rebind_cell` (`drive.rs:376`) remains the cut primitive for individual crosspoints; a
**cut** of the whole M/E is the resolver swapping which scene state is "PGM" at one tick —
the same atomic frame-boundary semantics `set_layout` has today, without the re-solve.

### 4.4 Build vs route — what is an M/E, what is a program

| Need | Mechanism | Why |
|---|---|---|
| PGM/PVW, transitions, USKs, DSKs, FTB, multi-box compositions | **In-program render plan** (one clock, one encode) | Blending needs both scenes in one composite; invariant #7 says don't pay an encode you don't need. |
| Aux buses, feeds with independent codec/res/bitrate, clean-feed *recordings* (ADR-0037) | **ADR-0030 programs + RT-12 output←program crosspoint** | These genuinely need their own encode. `RouteIntent::Output` + `Command::RouteOutput` + the `PacketRouter::move_sink` bridge (`crates/multiview-output/src/fanout.rs:212` — BUILT, zero engine callers) is the deliberate empty seam ADR-R010 reserved; the switcher pass builds it once, shared with Class-2 make-before-break. Post-MVP (§18). |

An aux bus that merely *selects a source or tap* (no separate encode rendition demanded)
can also be served as an additional rendition of the one program where formats coincide —
encode-once-mux-many (invariant #7) stays the default posture; the crosspoint decides.

### 4.5 The on-clock DSK/FTB stage vs the monitoring-overlay bake

Two graphics paths exist after this design, with a hard rule for what goes where:

| Path | Where it runs | Frame-coordinated with switcher state? | Carries |
|---|---|---|---|
| **On-clock keyer stage** (USKs in the M/E plan; DSKs + FTB as final render-plan stages) | Inside `compose(tick)` on the output-clock thread | **Yes** — same tick, same resolver | Production graphics: keys, fill+key media, DSK graphics, FTB |
| **Monitoring bake** (existing) | Off-thread on the bake consumer, after the preview/display taps | No (it lags the tick by the queue) | Labels, audio meters, state badges, safe-area, clocks, captions, watermark — monitoring-style overlays (`crates/multiview-cli/src/overlays.rs:208`) |

Rationale: a DSK that must "TIE" into a transition, or an FTB that must hit black on the
same frame the audio reaches −∞, cannot live on a path that is deliberately decoupled from
the tick (the bake consumer receives frames over a bounded drop-oldest queue — correct for
monitoring, wrong for production keying). The cost of moving DSK/FTB on-clock is budgeted
in §15.2; the monitoring bake is unchanged and keeps its existing isolation proof.

FTB implementation (pinned in [ADR-0056](../decisions/ADR-0056.md)): a solid
full-canvas cover tile (configurable colour, default black, defined in canvas space per
[ADR-C007](../decisions/ADR-C007.md)'s dip/FTB target rule) at the top of the final
compose list whose weight rides the ramp on the
FTB's own integer-frame rate — the existing `Tile` + linear `over` already compute
exactly a linear fade to the cover (`out = (1−w)·under` for black), zero new kernel code (perceptual
ramp option per [ADR-C007](../decisions/ADR-C007.md)). The engine **keeps composing
beneath an engaged FTB** so the clean/monitoring taps and ISO paths stay live.

### 4.6 Bus taps and clean feeds

Tap taxonomy along ONE program's render plan (CLEAN/DIRTY vocabulary per
[ADR-0037](../decisions/ADR-0037.md)):

```
me[n] ──► USK stage ──► [M/E output tap: me[n]]
                              │
                    (top-level M/E = program path)
                              ▼
                    ┌── [clean tap: pre-DSK] ──► clean feed (CLEAN)
                    DSK 1..m
                    └── [dirty tap: post-DSK, pre-FTB]
                              ▼
                            FTB
                              ▼
                    [program tap: post-FTB] ──► program (DIRTY)
        PVW composite ──► [preview tap]
        aux[j] ──► (RT-12 routed program / rendition)
```

- `{program (post-FTB), dirty (post-DSK, pre-FTB), clean (pre-DSK), preview, me[n],
  aux[j]}` is the tap set — matching [ADR-0056](../decisions/ADR-0056.md) §1's
  canvas-order diagram; a "program-no-FTB" feed binds to the `dirty` tap.
- **Key-fill / key-alpha aux taps** (external-keyer feeds — the on-air keyer's fill and
  its alpha/matte routed as separate outputs, key-alpha rendered as a luminance image)
  are **post-MVP**: additional named tap points on the same render-plan model, not
  designed in this pass (§18).
- **Verified anchor:** the engine already publishes the pre-overlay canvas `Arc` per tick
  into the preview `ProgramSlot` *before* the off-thread overlay bake
  (`crates/multiview-cli/src/pipeline.rs:2122`) — today's "program preview" is therefore
  already a clean-ish tap (clean of *monitoring* overlays). The design formalizes taps as
  named points the resolver exposes; each tap an output/recorder/monitor can bind.
- A tap consumed only by monitors costs one `ArcSwap` store (wait-free). A tap consumed by
  an encoder costs a rendition (invariant #7 accounting, §15.4). FTB is *after* the clean
  tap: by de-facto industry practice the clean feed follows FTB only if configured to —
  pinned in [ADR-0056](../decisions/ADR-0056.md) (default: clean feed does **not** ride
  FTB; the program tap does).

## 5. Transition engine

Full decision record: [ADR-0055](../decisions/ADR-0055.md).

### 5.1 Taxonomy and parameters

All rates/durations are integer frame counts at the output cadence (API: ms in, exact
rational conversion, [ADR-T015](../decisions/ADR-T015.md)).

| Kind | Parameters | Notes |
|---|---|---|
| `cut` | — | Degenerate 1-frame case of everything below. |
| `mix` | `rate` | Dissolve. Blend domain pinned in [ADR-C007](../decisions/ADR-C007.md): linear-light premultiplied `over` (the one existing code path), with an optional perceptual progress-curve mapping applied to **t**, never to pixels. |
| `dip` | `rate`, `dip_source` (any bus-selectable source incl. solid-color generators), `switch_point` (exact rational in `(0, 1)`, default ½) | A→dip_source→B; `switch_point` is where the A/B swap happens under the dip ([ADR-0055](../decisions/ADR-0055.md); boundary-frame arithmetic per [ADR-T015](../decisions/ADR-T015.md)). |
| `wipe` | `pattern`, `softness`, `border_width`, `border_fill_source`, `position_xy`, `reverse`, `flip_flop` | Wave 2. Per-tile SDF mask functions in the house style (`rect_coverage`/`point_segment_distance`, `crates/multiview-compositor/src/overlay/subpass.rs:853/:979`); `TileParams` has padding fields to carry mask id+params (`composite.wgsl:14-21`). |
| `dve_push` / `dve_squeeze` | `direction`, `rate` | Wave 2. Needs per-tile affine transform + sub-pixel placement — and finally wires the modeled-but-ignored `FitMode`/`crop`/`rotation` (`crates/multiview-config/src/lib.rs:543-546`). |
| `stinger` (media transition) | `player`, `pre_roll`, `clip_duration`, `trigger_point`, `mix_rate` | Wave 3 (needs alpha media, [ADR-0058](../decisions/ADR-0058.md)). Validated `trigger_point + mix_rate ≤ clip_duration` (frames). A hard cut-point is the degenerate `mix_rate = 1` frame case — the superset model. |
| FTB | `rate`, `audio_follow: bool` | Master stage, §4.5; not an M/E transition but driven by the same progress machinery. |

### 5.2 Mix: flat-list fast path vs scene pre-render

- **Fast path (zero compositor changes).** For scenes whose cells do not internally
  overlap, a dissolve is both scenes' cells in **one** z-ordered tile list with the
  incoming cells' opacity = `progress(tick)`: per-tile opacity is already wired
  end-to-end (config → `Cell.opacity` → placement → both kernels) into the premultiplied
  linear-light `over` (`crates/multiview-compositor/src/blend.rs`). Cost: one composite
  with up to 2×N tiles (GPU `MAX_TILES=64` bounds the union — an admission check at arm
  time, §15.3).
- **Scene pre-render (general correctness).** A scene with internal overlap (PiP over a
  background cell) mixed via the flat list would blend *through* the overlap incorrectly.
  Detected **at arm time** (static geometry analysis), such scenes render as: scene A →
  NV12 intermediate, scene B → NV12 intermediate, final composite of two full-canvas
  tiles with ramped opacity. `Nv12Image` is both compose output and compose input, so
  this works today; on GPU the three passes fuse via the existing chaining template with
  one readback (`gpu/compositor.rs:326`). Costs and the NV12 re-quantization note: §15.2.

### 5.3 Controls

- `set_transition(kind, params)` / `set_rate(frames)` — arm-time configuration.
- `auto()` — start the armed transition; progress = `f(tick.index)`.
- `cut()` — immediate swap at the next frame boundary.
- **T-bar** — an **idempotent absolute progress setter**: the surface conveys
  *positions*, conflated latest-wins in the control plane (one slot, never one command
  per sample — the route-coalescing rule generalized). One story everywhere: the **wire
  carries an integer basis-point position `0..=10000`** (REST + events — integers, never
  floats, [ADR-W021](../decisions/ADR-W021.md)); **engine state is u16 fixed point
  `0..=65535`**, quantised half-up on apply ([ADR-T015](../decisions/ADR-T015.md),
  authoritative). The state machine treats an in-flight T-bar as overriding the auto
  ramp; reaching the top (wire `10000` / engine `65535`) completes the transition
  exactly as auto completion does. Wire cadence is pinned in
  [ADR-T015](../decisions/ADR-T015.md) / [ADR-RT008](../decisions/ADR-RT008.md).
- **Abort semantics** (pinned in [ADR-0055](../decisions/ADR-0055.md)): `abort()` snaps
  progress to `0` at the next frame boundary — PGM unchanged, no flip-flop, armed set
  retained; a *timed* reverse is simply driving the T-bar back (the absolute setter
  already provides it). `cut()` during an in-flight transition completes it instantly
  (jump to `1`, same completion path — never a second transition stacked).
- **Flip-flop** default ON, configurable per M/E; **direct program punch** supported;
  an explicit **PGM↔PVW swap** is the degenerate cut with flip-flop ON — no separate
  verb is provided; **preview-transition** renders the armed transition on the PVW
  composite only.

### 5.4 Next-transition arming and DSK coupling

The armed element set is `{background, key[1..n]}` in any combination; each armed
element's on-air state toggles through the transition; key-only transitions work
(background unarmed). DSKs are canvas-level and independent of the M/E armed set, with
**TIE** (join the next background transition), **CUT** (instant), **AUTO** (the DSK's own
rate) — the cross-vendor consensus shape, recorded as de-facto industry practice in
[ADR-0055](../decisions/ADR-0055.md)/[ADR-0056](../decisions/ADR-0056.md).

### 5.5 Wave phasing

- **Wave 1 (MVP):** cut, mix, dip, FTB — flat-list fast path + scene pre-render fallback.
- **Wave 2:** wipes (SDF masks), DVE push/squeeze (per-tile affine + sub-pixel placement +
  wiring `FitMode`/crop/rotation). The single biggest compositor work item; both CPU and
  GPU kernels, CPU-oracle/GPU-SSIM test pattern.
- **Wave 3:** stinger (alpha media, [ADR-0058](../decisions/ADR-0058.md) /
  [media-playout.md](media-playout.md)).

### 5.6 Degradation interaction

- An in-flight transition is **program-affecting**: it is never shed mid-flight.
  **Complete-then-degrade** — the control loop may act only at the next idle boundary.
- Under overload, **AUTO demotes to CUT at arm/admission time** (the operator's intent —
  "change the picture" — is honored; the luxury — "smoothly" — is shed first).
- The **PVW composite is admission-accounted** and occupies a new degradation-ladder rung
  **between the preview rungs and the program-tile rungs**: reduced-rate/resolution PVW
  first, then PVW off, before any program tile is touched
  ([ADR-P007](../decisions/ADR-P007.md), §15.3).

## 6. Keyers

Full decision record: [ADR-0056](../decisions/ADR-0056.md). Color kernels:
[ADR-C007](../decisions/ADR-C007.md).

### 6.1 Model

- **Upstream keyers (USKs):** M/E-scoped scene layers; participate in next-transition
  arming; z-ordered above the background buses within the M/E plan.
- **Downstream keyers (DSKs):** canvas-level, post-M/E, pre-FTB, on-clock (§4.5).
- **FTB:** the final master stage after the DSKs (§4.5).
- `KeySource { fill: SourceRef, key: Option<SourceRef>, premultiplied: bool }`.
- `KeyType { luma, linear /* fill+key */, chroma, pattern, dve_pip }` with
  `clip`/`gain`/`invert` and a rectangular **garbage matte**. Key priority is explicit
  z-order. **MVP key types: linear/alpha + luma + dve_pip**; chroma + pattern later.

### 6.2 Math insertion points (invariant #8 preserved)

Key alpha is computed in the kernel front half and **multiplied into tile alpha before
the premultiplied linear-light `over`** — the pipeline order (range-expand → matrix →
linearize → blend) is untouched:

- **Luma key:** threshold on **post-range-expand code-value Y** (operator-familiar
  clip/gain semantics), before the YUV→RGB matrix.
- **Chroma key:** distance in **linear RGB** after YUV→RGB + EOTF.
- Insertion points exist in both kernels: CPU `fold_tile_into_band`
  (`crates/multiview-compositor/src/pipeline.rs:1075`) computes per-pixel source alpha at
  exactly the right step; the GPU `TileParams` struct has padding to carry key
  parameters (`composite.wgsl:14-21`). Both kernels are pinned by the existing
  CPU-oracle/GPU-SSIM test pattern.

### 6.3 Fill + key

A second per-tile source reference: the GPU compositor already binds per-tile
texture-array layers, so a key source is one more layer index in `TileParams`; the CPU
kernel reads the second store in the same fold. The key plane samples through the same
latch-on-tick `read_at` as fill (both sides hold last-good independently — §14).

### 6.4 Alpha media

Keyed media (stills/clips with alpha) ride the **NV12+A** framestore payload extension
(NV12 + one R8 alpha plane, 2.5 B/px) — never per-tile RGBA video (invariant #5), never
bolted onto the overlay stack (overlays are input-decoupled by design). Alpha is
premultiplied at the compositor boundary. Detail and format policy:
[ADR-0058](../decisions/ADR-0058.md) and [media-playout.md](media-playout.md).

### 6.5 Multi-box composition source

The existing `Layout` model (background + overlapping z-ordered cells,
`crates/multiview-core/src/layout.rs`) **is** a multi-box composition. The design exposes
it as a **bus-selectable composition source** rendered as a **drive-internal pre-pass in
the same tick** — explicitly NOT a chained `ProgramSet` program, which would add one tick
of latency plus NV12 generation loss per hop. Nested M/Es re-enter the same way (a
`SourceKind` program-reference variant with a document-level cycle check,
[ADR-M012](../decisions/ADR-M012.md)). The SPA reuses the layout-editor pure model for
the box editor ([ADR-W021](../decisions/ADR-W021.md)).

## 7. Media library and media players (summary)

Owned by the companion brief [media-playout.md](media-playout.md) and
[ADR-0057](../decisions/ADR-0057.md)/[ADR-0058](../decisions/ADR-0058.md); load-bearing
facts repeated here once:

- **Library ≠ players.** The media library is asset storage (stills, clips, audio;
  import/validation/transcode pipeline). Media **player channels** are bus-selectable
  sources. Vocabulary: "media library", "media player", "still store".
- Players extend the **production file ingest path** — the CLI `ingest_loop`
  (`crates/multiview-cli/src/pipeline.rs:5533`), NOT the unwired `multiview-input`
  `IngestPump` scaffold (the divergent twin; stated explicitly so nobody builds on the
  wrong one). Transport: cue (open + decode first frame + pause; primed via
  `is_primed`), play/pause (gate `PtsWallClock`, `pipeline.rs:6189`), seek
  (`Demuxer::seek` exists with zero callers, `crates/multiview-ffmpeg/src/demux.rs:549`),
  loop (in-place `Demuxer::seek` to the in-point on clean EOF; the supervised reconnect
  bracket is the failed-seek fallback, [ADR-0057](../decisions/ADR-0057.md)), EOF policy
  `{hold_last_frame | loop | black | auto_off}`, play-on-take (roll-clip semantics),
  frame-accurate start. A still is decode-once + `HoldForever` — the EOF-hold path
  already proves the semantics (`pipeline.rs:1187`).
- **Alpha formats** are constrained to what FFmpeg actually decodes with alpha (per the
  FFmpeg documentation): ProRes 4444/4444XQ, qtrle/Animation, PNG/TGA sequences,
  VP9-alpha via libvpx-vp9. HEVC-with-alpha is rejected or import-transcoded (FFmpeg
  decodes its alpha as opaque). Imports require canvas resolution + output frame rate +
  explicit trigger-point metadata.
- **Stinger efficiency** (standing review): pre-decode at import into a bounded in-memory
  NV12+A mezzanine, hard cap + decode-ahead ring for longer clips, pool-allocated at
  load, never per-frame (budget: §15.5).
- **Transport-control protocol posture:** native API only for v1, with cue/auto-chain
  semantics in the spirit of the open CasparCG AMCP model; VDCP/AMP are legacy
  RS-422-era external-server protocols — out of scope, revisit on demand.

## 8. Audio

Full decision record: [ADR-0059](../decisions/ADR-0059.md).

### 8.1 The audio control seam (first slice)

The pop-free nucleus is BUILT and tested — per-sample equal-power `GainRamp`
(`crates/multiview-audio/src/mixer.rs:35`) and `ProgramBus::repoint_crossfade`
(`crates/multiview-audio/src/program.rs:217`) — but unreachable live:
`Command::RouteAudio` is HELD with a warning
(`crates/multiview-cli/src/control.rs:933-953`) because the `ProgramBus` is moved into
and owned by the bake-consumer thread with no re-point seam. The fix is the **subtitle
seam's twin**: an `AudioControlHandle` (wait-free pending slot, the
`SubtitleRouteHandle` pattern verbatim — `crates/multiview-cli/src/captions.rs:130-153`)
created at pipeline start, threaded into the consumer, polled once per `StreamItem`
before the bus ticks. Zero new invariant risk: the bake consumer is already off the hot
loop.

### 8.2 AFV (audio-follow-video)

Per-input mode `{fade_with_transition (default) | hard_cut}`. The crossfade is
`repoint_crossfade` with

```
ramp_frames = SampleClock::total_at(start_tick + transition_frames)
            − SampleClock::total_at(start_tick)   // exact cumulative-sample delta, ADR-T015 §5
```

— the **integer cumulative-sample delta, never the naive
`transition_frames × samples_per_tick` product**: the per-tick budget is non-integer at
NTSC cadences (1601/1602 alternation around 1601.6 exact), so the product is off by up
to a frame and drifts the audio ramp off the video edge
([ADR-T015](../decisions/ADR-T015.md) §5, [ADR-0059](../decisions/ADR-0059.md) §2) —
driven by the **same switcher state machine tick window as video**: the transition's
start tick and integer frame count are the single source of truth for both.

### 8.3 Master gain, FTB fade, mute

- A **master/program gain stage** — one multiply pass in `ProgramBus::mix`
  (`program.rs:316`) — carries master gain and the FTB audio fade (an equal-power
  `GainRamp` over the FTB's frame window when `audio_follow` is set).
- **Gain-preserving per-strip mute** (today mute = unroute, which loses gain state) and
  honoring `RouteIntent::Audio`'s `gain_db`/`mute` (currently destructured away,
  `crates/multiview-engine/src/route.rs:367`).

### 8.4 Per-bus audio — the one real data-structure change

One `ProgramBus` per audio-bearing bus (program, aux with audio, clean). `AudioStore` is
**single-cursor**: a second bus reading the same store would tear the read cursor. This
is **the** real audio data-structure change; [ADR-0059](../decisions/ADR-0059.md) pins the
fix — a **multi-cursor `AudioReader` view** (split cursor from data; the ring window is
already an `ArcSwap` snapshot, so each bus owns its own cursor over the shared window)
over the store-per-(source, bus) alternative. Everything else is composition of existing
pieces.

### 8.5 Meters, classification, defaults

- **Live meters** are required before the switcher UI ships: `Event::AudioMeter` exists
  with zero production emitters (`crates/multiview-events/src/event.rs:111/:1138`); wire
  it from the bake consumer at ~30 Hz conflated (the existing conflated-lane rule).
- **Class tension resolved**: the program-bus crossfade is **Class-1** — this path always
  decodes→mixes→re-encodes, so the re-point is hot. `SwitchTier::ClickFree`'s Class-2
  self-description (`crates/multiview-audio/src/program.rs:40-70`) refers to the unbuilt
  coded-passthrough alternative; docs are updated accordingly.
- **Program audio becomes config-declared and default-on for switcher use** (today it is
  the `--program-audio` CLI flag only).

## 9. Tally

Full decision record: [ADR-MV006](../decisions/ADR-MV006.md) (extends
[ADR-MV002](../decisions/ADR-MV002.md)).

### 9.1 Internally-derived tally — recursive contributes-to-program

A per-tick **pure derivation over the composition graph** (same value-machine style):

- **PROGRAM-tallied** iff reachable from any on-air output through the live composition:
  sources in the PGM scene; **both transition sources mid-transition** (the universal
  rule); **keyer FILL and KEY sources** of on-air keyers (USK and DSK); **stinger media
  while it covers**; sources inside on-air **multi-box compositions and nested M/Es**
  (recursive). The known industry anti-pattern — overlay/composition inputs that
  contribute to air but never tally — is explicitly designed out: tally walks the same
  graph the resolver renders.
- Sources behind an **engaged FTB stay PROGRAM-tallied** — the composition is live
  behind black and exposed instantly on release (the safe default, pinned with
  rationale in [ADR-MV006](../decisions/ADR-MV006.md)).
- **PREVIEW-tallied** iff reachable from the top-level M/E's PVW scene.
- **Amber** = ISO/record usage (ADR-0037 alignment).

### 9.2 Arbiter wiring

`TallyFacts` from the derivation feed the **existing pure `TallyArbiter`**
(`crates/multiview-engine/src/tally/arbiter.rs:145`, `resolve` at `:185`) so internal and
external facts (TSL ingest, IS-07, router) merge under one `ConflictPolicy`. The switcher
pass wires the arbiter into the run loop (replacing the `SetTallyOverride` echo), spawns
the existing-but-unspawned `run_tally_ingest`, and publishes resolved states on the
existing drop-oldest event stream — the engine never awaits a tally consumer
(invariant #10).

### 9.3 TSL reconciliation (summary; on-wire detail in ADR-MV006)

Verified against the published TSL UMD protocol specification, the in-repo codecs
(`crates/multiview-input/src/tsl/`, `crates/multiview-output/src/tsl/`) deviate from the
spec and must be reconciled **before any TSL egress ships**: v5.0 DLE must be `0xFE` (not
`0x10`); the v5.0 CONTROL bit order is RH(0-1)/Text(2-3)/LH(4-5); the invented DLE/ETX
terminator is dropped; DMSGs with CONTROL bit 15 set (control data) are skipped; v4.0 is
rebuilt as v3.1 + CHKSUM(mod-128) + VBC + XDATA. Round-trip property tests structurally
cannot catch these (encoder and decoder share one value model) — golden on-wire vectors
hand-transcribed from the spec are mandatory. Ports are always configurable (vendor port
conventions vary; none is normative).

## 10. Macros and memories

### 10.1 Memories (snapshots) = Salvo + recall-scope masks

`Salvo` is BUILT end-to-end (config `crates/multiview-config/src/salvo.rs:59`; engine
arm/take/cancel `crates/multiview-engine/src/salvo.rs:158-189`; REST + SPA). Memories
extend it with **recall-scope masks** — which groups a recall applies, wire shape pinned
in [ADR-M012](../decisions/ADR-M012.md): recall body
`{scope: {sources, keyers, transition, audio}}`, each a bool defaulting `true` — and the
RT-16 mixed-recall shape (video + audio + keyers + transition settings in one atomic
frame-boundary batch). This **supersedes ADR-MV004's salvo-only automation story** (§17).

### 10.2 Macros = a control-plane sequencer (never engine-side)

A macro is an ordered list of ordinary `Command`s with wait steps
(`wait_frames` / `wait_ms`, ms converted via exact rationals). The sequencer runs **in the
control plane**, submitting each step through the same bounded bus as live commands —
steps desugar onto the same engine intents, so exactly one apply path exists and a macro
can never do something the API cannot. It is structurally incapable of violating
invariant #10: it is just another client. Failure policy: halt + event, never engine
impact ([ADR-R011](../decisions/ADR-R011.md)). Macro **recording** — capturing live
operator commands into a macro definition — is explicitly **post-MVP** (§18): a capture
layer over the same `Command` stream, never a second apply path.

### 10.3 Frame-boundary batches

Multi-op takes (e.g. cut M/E 1 + DSK 2 off + aux re-route, atomically) get a batch
semantic: all commands in the batch drain at one tick. The openly-documented
obs-websocket protocol's request-batch `SERIAL_FRAME` execution type (process the whole
batch on one render frame) is the published precedent and maps 1:1 onto the existing
frame-boundary drain — cited as an open protocol document, not a product comparison.
Realtime shape: [ADR-RT008](../decisions/ADR-RT008.md).

## 11. Management surface — config, REST, realtime, SPA (summary)

The ADRs carry the normative detail; this section pins only the shape.

### 11.1 Config ([ADR-M012](../decisions/ADR-M012.md))

- Additive, exactly the routing precedent: new optional `#[serde(default)]` blocks on the
  `non_exhaustive` `MultiviewConfig` — `switcher` (mix_effects[], downstream_keyers[],
  aux_buses[], compositions[], transition presets/wipe patterns), `media_library`,
  `media_players`, `macros`. Internally-tagged unions only (never `untagged`, ADR-0010); per-item
  `validate()` + a document-level `validate_switcher()` with cross-refs and a **cycle
  check** for composition/M/E re-entry.
- New `SourceKind` variants (additive on the `non_exhaustive` enum,
  `crates/multiview-config/src/schema.rs:217`): `Still`, a media-player binding, and
  program re-entry `{program: ProgramId}`; the dead core `Cell` crop/rotation fields are
  surfaced (`crates/multiview-config/src/lib.rs:543-546`).
- **Desired-state only** in config (the devices precedent): live bus state (crosspoints,
  T-bar, keyer on-air, FTB level) is engine-owned and mirrored read-only. Warm-restart is
  an explicit, separate, persisted snapshot resource — optional, off by default,
  control-plane-owned (decision recorded in ADR-M012).
- **MP-5 is a named prerequisite slice** the switcher program lands first:
  `programs: Vec<ProgramSpec>` schema root + desugar + cross-validation that every
  `OutputCrosspoint.program` exists — which also fixes the currently-unvalidated program
  string (only checked non-empty, `crates/multiview-config/src/routing.rs:434-441`).

### 11.2 REST + realtime ([ADR-W021](../decisions/ADR-W021.md), [ADR-RT008](../decisions/ADR-RT008.md))

- Bare verbs, kebab paths (ADR-W017 practice):
  `/api/v1/switcher/mix-effects/{id}/preview|program` (PVW crosspoint set / direct
  program punch), `.../cut|auto|ftb`, `.../transition` (set type/rate),
  `.../tbar` (conflated absolute; wire = integer basis points `0..=10000`, §5.3),
  `/api/v1/switcher/downstream-keyers/{id}/on-air|off-air|auto|tie`,
  `/api/v1/switcher/aux-buses/{id}/route`,
  `/api/v1/media/players/{id}/load|cue|play|pause|stop|seek`, `/api/v1/macros/{id}/run`;
  master gain/mute is `PATCH /api/v1/switcher/audio {master_gain_db, master_mute}`
  ([ADR-W021](../decisions/ADR-W021.md), cross-referenced from
  [ADR-0059](../decisions/ADR-0059.md), which keeps the engine `MasterEnvelope`
  command). FTB is a per-program master stage *after* the DSKs (§4.5); the REST verb
  addresses it via the M/E that owns the program canvas (one M/E in the MVP), so
  `.../mix-effects/{id}/ftb` is the **address**, not the stage location
  ([ADR-0056](../decisions/ADR-0056.md)).
- **Plan/take split per invariant #11**: every state-changing take is classified
  Class-1/Reset-lite/Class-2 with a `/plan` dry-run (the routing plan/take precedent,
  `crates/multiview-control/src/routing.rs:222`); `200` for immediate Class-1, `202` +
  operation id for timed/Class-2; `Idempotency-Key` everywhere; **new `CorrKey` variants**
  so cut/auto/ftb outcomes correlate on the stream (today every `Route*` command returns
  `None` — `crates/multiview-control/src/realtime.rs:106-145` — a gap this design does
  not repeat).
- Realtime: one coarse `switcher` topic. Lossless lifecycle events
  (`transition.started/completed`, `ftb.engaged`, `keyer.on_air`, `media.player_state`,
  `macro.step`) ride the replay ring; `transition.progress` (payload
  `{me, elapsed_frames, duration_frames}` — integer frames, never a float ratio,
  [ADR-RT008](../decisions/ADR-RT008.md)) and `audio.meter` are conflated latest-wins
  (~30 Hz), excluded from replay, with connect-time snapshot frames (the device-status
  pattern); **tally is edge-triggered and stays on the existing lossless `tally.state`
  lane** (publish-on-change, replay-ring — [ADR-RT008](../decisions/ADR-RT008.md)).
  **Verified gap**: `$subscribe`/`$set_rate`/`$resume` are
  types-only — `run_ws_session` never reads inbound frames
  (`realtime.rs:878-928`); the pinned decision is **publisher-side 30 Hz conflation
  now**, per-client `$subscribe` as a follow-on lane item.
- **Control-surface friendliness is a first-class requirement**: long-lived WS with full
  snapshot on connect + deltas, stable string IDs, idempotent single-shot commands, and
  boolean state-feedback queries (source-on-pgm, keyer-on-air, ftb-active,
  media-playing) — the shape a module for the open-source Companion control-surface
  runtime (Bitfocus) needs without polling. An official Companion module, an OSC
  namespace, and a MIDI surface adapter are post-MVP adapter items (§18).
- Both spec generators update in the same change (utoipa ApiDoc + `rest_routes` table
  test; `multiview-events` AsyncAPI) + `gen-openapi`/`gen-asyncapi` + web `generate:api`.
- IPv6-first per [conventions §10](../architecture/conventions.md): the control plane
  binds dual-stack `[::]:8080` (the documented default,
  `crates/multiview-config/src/lib.rs:98-101`); every example leads IPv6:

```bash
curl -X POST 'http://[::1]:8080/api/v1/switcher/mix-effects/me1/auto' \
  -H 'Authorization: Bearer …' -H 'Idempotency-Key: 7d6e1c1e-…'
# → 202 { "operation_id": "…", "kind": "switcher.auto" }  — outcome on ws://[::1]:8080/api/v1/ws
```

### 11.3 SPA ([ADR-W021](../decisions/ADR-W021.md))

`/switcher` lazy route + `NAV_ITEMS` entry; **spec-first** so the typed `openapi-fetch`
path is used (no raw-fetch bypass); **one shared realtime connection service** (today
every hook opens its own WS — `useEngineEvents`/`useSystemMetrics`/`useHealth` migrate
onto it before switcher topics are added); an operation-correlation layer (store
`operation_id`, resolve against envelope `corr`) + `crypto.randomUUID` Idempotency-Key in
the shared submit helper; a **global keyboard-shortcut subsystem** (none exists): number
keys = PVW selection, Shift+number = PGM, dedicated CUT/AUTO keys, disabled in editable
elements/dialogs, visible shortcut reference, every shortcut paired with an on-screen
button, live-region announcements. Tally/bus state per ADR-W011 (never color alone):
red/filled+label PGM, green/outline+label PVW. Monitors: 1 Hz JPEG stills first
(replicate the `ProgramSlot` pattern per bus tap), WHEP as a fidelity upgrade lane.
The multi-box/DVE editor imports the layout-editor pure model — never forks it.

## 12. Preview bus rendering and cue/pre-warm

Full decision record: [ADR-P007](../decisions/ADR-P007.md).

### 12.1 Cue = pre-warm = the PVW arm

ADR-P004's pre-warm worker (DOC-ONLY today, §3) **is** the PVW arm mechanism: one
machinery for "look at it" and "take it" (RT-7 WARM-ON-ARM, prime-gated via
`TileStore::is_primed`, `crates/multiview-framestore/src/tile.rs:311`). Preview of a cold
source is dishonest without it — selecting a source onto PVW warms it, so the subsequent
take is Class-1. Built alongside the PVW bus, not after it.

### 12.2 PVW visibility and cost

The PVW composite is a second compose per tick in the same program (§4.1), surfaced to
monitors via a per-bus `ProgramSlot` replica (one `ArcSwapOption<Nv12Image>` per tap,
wait-free store from the tick projection — the `crates/multiview-cli/src/preview.rs:28`
pattern) + the existing JPEG provider. Its cost is admission-accounted;
reduced-rate/resolution PVW is the documented degradation rung (§5.6, §15.3).

## 13. Timing ([ADR-T015](../decisions/ADR-T015.md), summary)

- Every duration is an **integer frame count at the exact rational output cadence**
  (e.g. `30000/1001`); API milliseconds convert via exact rationals at admission, never
  floats.
- Transition progress, stinger trigger frames, DSK/FTB rates, and AFV sample budgets all
  derive from `tick.index` + `SampleClock` — one timebase, no wall clock anywhere in
  switcher state.
- T-bar conflation cadence is pinned (publisher-side ~30 Hz; the control plane holds one
  latest-wins slot per M/E).

## 14. Resilience ([ADR-R011](../decisions/ADR-R011.md), summary)

- **Transition under source loss:** each side samples last-good per invariant #2 — a
  dying source can never stall a transition; the tile state machine + slate policy apply
  per scene. The transition completes on schedule regardless of input health.
- **FTB is always available** regardless of source health (it operates on the canvas).
- **Media player underrun** → the player's EOF policy; **keyer fill/key loss** → keyer
  drops to off-air-safe (configurable); **macro sequencer failure** → halt + event, never
  engine impact.
- Every new seam ships with the chaos/soak gate pattern (wedge a consumer, the program
  keeps ticking — the MP-1 proof shape) and deterministic `ManualTimeSource` tests for
  transition timing.

## 15. Efficiency (standing review)

The numbers that gate admission and shape the design. Canvas reference: 1920×1080 NV12 =
3.110 MB/frame (1.5 B/px).

### 15.1 The PVW composite (steady-state cost of having a switcher)

One extra composite per tick, same tile count as PGM: roughly **2× composite cost, zero
extra decode** (stores are shared, §3). On GPU this is one more compute dispatch inside
the tick; on CPU it must fit the measured budget (the CPU compositor's hard 40 ms/tick
gate, `crates/multiview-compositor/benches/composite_realtime.rs:39`, is the enforcement
template — a 2-scene bench gate is added in the same style). PVW is admission-accounted
**before enabling** and is the first thing degraded (reduced rate → reduced resolution →
off) — shed before any program tile (§5.6).

### 15.2 Transition windows (transient cost)

- **Flat-list mix/dip:** one composite with up to 2×N tiles for the window only. GPU
  bound: union ≤ `MAX_TILES` (64, `gpu/compositor.rs:36`) — checked at arm time.
- **Scene pre-render:** 2 extra composites + 1 blend pass per tick during the window.
  GPU: passes fuse via the existing chaining template (`composite_with_overlays`,
  `gpu/compositor.rs:326`) — **one** NV12 readback per tick, not three. CPU: ~3× compose
  cost for the window; the admission check may demote AUTO→CUT (§5.6).
- **NV12 re-quantization note:** invariant #5 makes intermediates NV12 — each pre-render
  generation costs one 8-bit 4:2:0 re-quantization. Acceptable for a transition window
  (≤ a few seconds, moving pictures); NOT acceptable as a steady-state chain — which is
  why multi-box composition is a same-tick pre-pass kept in linear float on GPU, and why
  program-chaining through `TileStore`s is rejected (§6.5).
- **DSK/FTB on-clock:** each active DSK adds one tile (+ its key layer) to the final
  pass; FTB is one multiply pass. Idle DSKs/FTB cost zero (resolver emits nothing).

### 15.3 Admission and the ladder

Nothing today admits or accounts a transient second composite — the `ControlLoop`
(`crates/multiview-engine/src/degrade.rs:61`) and `PlacementController` are SCAFFOLD with
no run-path callers (§3). The switcher work finally wires the admission seam: the PVW
composite and an in-flight transition become first-class load items in the planner's
composite Mpix/s budget; arm-time checks (tile-union bound, scene-pre-render cost,
AUTO→CUT demotion) happen **before** the engine is asked to do anything (invariant #9).
New ladder rung order: preview-subsystem rungs → **PVW bus rung** → program-affecting
rungs.

### 15.4 Taps and renditions

A monitor-only tap = one wait-free `ArcSwap` store per tick (≈ free). An encoded tap
(clean-feed recording, aux with its own format) = one full rendition: encoder session +
mux fan-out — the invariant-#7 ledger, gated through the same admission seam. Aux buses
that can share the program rendition do (encode-once-mux-many stays the default).

### 15.5 Stinger mezzanine (budget summary; detail in [media-playout.md](media-playout.md))

NV12+A is 2.5 B/px (1.5 NV12 + 1.0 R8 alpha). A 3 s 1080p60 stinger fully pre-decoded:
`1920·1080·2.5 B × 180 ≈ 0.93 GB` — versus ≈ 1.49 GB as RGBA (4 B/px), the reason
invariant #5 extends to NV12+A rather than admitting RGBA video. Mezzanines are
pool-allocated at load with a hard cap + decode-ahead ring for longer clips
([ADR-0058](../decisions/ADR-0058.md)); never per-frame allocation, never hot-path decode.

### 15.6 Memory & churn discipline

Render-plan resolver output is scratch-pooled (`ComposeScratch` discipline); switcher
state is plain values (no per-tick allocation); T-bar/meter/progress lanes are conflated
latest-wins slots, not queues. No new channel from engine→outside exceeds the existing
drop-oldest proof obligations.

## 16. Crate placement ([ADR-0054](../decisions/ADR-0054.md))

**No new crate** (devices/ADR-I004 precedent): switcher state machine + render-plan
resolver + tally derivation in `multiview-engine`; compositor primitives (masks,
transforms, key math, NV12+A blend) in `multiview-compositor`; shared types (key/transition
descriptors, tap ids) in `multiview-core`; schema in `multiview-config`; control domain in
`multiview-control`; panel in `web/`. Dependency direction is unchanged; therefore
[conventions.md](../architecture/conventions.md) §3/§4 need **no edit** — API paths and
vocabulary are pinned in the ADRs.

## 17. What this design extends and supersedes

**Extends (builds on, does not duplicate):**
[ADR-0030](../decisions/ADR-0030.md) (programs/`ProgramSet`),
[ADR-0034](../decisions/ADR-0034.md) (crosspoint model — an M/E crosspoint is a TIER-1→2
route; aux is TIER-2→3),
[ADR-R004](../decisions/ADR-R004.md)/[ADR-R010](../decisions/ADR-R010.md) (Class-1 swap +
make-before-break; RT-12 is the aux seam),
[ADR-P004](../decisions/ADR-P004.md) (cue/pre-warm = the PVW arm),
[ADR-MV002](../decisions/ADR-MV002.md) (tally arbitration — internal facts join it),
[ADR-0037](../decisions/ADR-0037.md) (CLEAN/DIRTY vocabulary; "record the clean-feed tap"),
[ADR-R008](../decisions/ADR-R008.md)/[ADR-0016](../decisions/ADR-0016.md) (layer-stack +
atlas contract for the monitoring path).

**Supersedes (explicitly, in the same design pass):**

| Superseded | By | Note |
|---|---|---|
| [ADR-MV004](../decisions/ADR-MV004.md)'s salvo-only automation story | §10, [ADR-M012](../decisions/ADR-M012.md) | Memories = salvo + recall-scope masks; macros = control-plane sequencer; one apply path. |
| Capability matrix `/v1/program` + `/v1/program/preview` + `program:take` rows ([management-capability-matrix.md](management-capability-matrix.md):20, :42, :209-215) | The M/E resource model, [ADR-W021](../decisions/ADR-W021.md)/[ADR-M012](../decisions/ADR-M012.md) | The single-bus cue-then-take sketch is replaced by M/E PGM/PVW + `/api/v1/switcher/*`. |
| [layout-and-config.md §7.1](../templates/layout-and-config.md) `program:take` sketch (`:295-307`) | [ADR-0055](../decisions/ADR-0055.md) | `duration_ms` floats-adjacent sketch replaced by integer-frame transitions + plan/take. |
| [resilience-and-av.md](resilience-and-av.md)'s "Crossfade (per-frame alpha interpolation)" line (`:56`, `:156`) | §5 | Generalized to the full transition family with the flat-list/pre-render split. |

## 18. MVP boundary and phases

**MVP — one M/E, designed for N:**

- PGM/PVW buses; cut / auto / T-bar; mix + dip + FTB (with audio-follow); flip-flop.
- 2 DSKs (linear/luma; stills with alpha via NV12+A).
- Media library + 2 media players (stills + clips, loop, play-on-take, EOF policy).
- Program audio: AFV + master gain + live meters (the audio seam, §8).
- Clean program tap (pre-DSK, §4.6).
- Multiview red/green tally — derived (§9.1), arbiter-wired (§9.2).
- Switcher REST/WS surface with plan/take + correlated events (§11.2).
- SPA panel with keyboard shortcuts (§11.3).
- Macros (command sequences with waits) + memories (salvo recall-scope extension) (§10).

**Post-MVP phases:** wipes; DVE push/squeeze (+ fit/crop/rotation wiring); stinger (alpha
media Wave 3); USK chroma/pattern keys; aux buses via RT-12; key-fill / key-alpha aux
taps (external-keyer feeds, §4.6); preview-transition; multi-M/E; WHEP bus monitors;
an official Companion module, an OSC namespace, and a MIDI surface adapter (§11.2);
macro recording (§10.2); a **native graphics renderer** (templates, data-bound text,
clock/score widgets, font management, preview-before-air, take-coupled graphics
automation) — explicitly deferred: MVP production graphics are media-based fills
(NV12+A stills/clips, [ADR-0058](../decisions/ADR-0058.md)) and the existing overlay
text stack remains monitoring-only; TSL egress; GPI/GPO (NMOS IS-07 first);
ISO-recording tie-in (ADR-0037 rows). Dependency order and PR-sized slices:
[production-switcher-backlog.md](../development/production-switcher-backlog.md).

## 19. Open questions (honest)

1. **`AudioReader` multi-cursor view** (§8.4) — pinned in
   [ADR-0059](../decisions/ADR-0059.md), but the contention profile of N cursors over one
   `ArcSwap` window under real per-bus load wants a measurement before the lane starts.
2. **Per-client realtime subscription** — publisher-side 30 Hz conflation is pinned now;
   whether `$subscribe`/`$set_rate` lands before multi-panel deployments need it is a
   follow-on lane call ([ADR-RT008](../decisions/ADR-RT008.md)).
3. **Warm-restart snapshot semantics** — the resource exists (off by default,
   [ADR-M012](../decisions/ADR-M012.md)); what "recall on start" means for sources that
   are not yet primed (take-blocked vs slate) needs operator feedback after MVP use.
4. **HDR (PQ/HLG) canvas under dips through saturated colors** — the dissolve law is
   pinned ([ADR-C007](../decisions/ADR-C007.md)); dip-source tone mapping on an HDR
   canvas has a recorded default but limited real-content validation.
5. **Decode-at-display-resolution vs PGM/PVW size divergence** — decode size is coupled
   to the first bound cell today (`cell_pixel_size`,
   `crates/multiview-cli/src/pipeline.rs:4127`); a source full-screen on PGM and as a
   small box elsewhere wants max-consumer-size negotiation (invariant #6). The switcher
   inherits this pre-existing gap; tracked as a backlog item, not solved here.
6. **GPU tile budget under heavy unions** — `MAX_TILES=64` bounds flat-list transitions
   of two dense scenes (§15.2); whether to raise the bound or force pre-render above it
   is left to measurement on real hardware.

## 20. References

- In-repo: the ADR set and companion brief linked throughout; the as-built inventory (§3)
  is the verified baseline; [conventions.md](../architecture/conventions.md) for naming,
  IPv6-first (§10), and invariants.
- Open external references (the only kind this brief cites): the TSL Products UMD
  protocol specification (v3.1/v4.0/v5.0 wire formats —
  <https://tslproducts.com/wp-content/uploads/Manuals/Control/tsl-umd-protocol.pdf>);
  AMWA NMOS IS-07 (event & tally
  transport); SMPTE/EBU operational-practice documents (program/clean-feed conventions);
  SCTE-35/104 (automation triggers, future); the obs-websocket protocol document
  (request batches, `SERIAL_FRAME`); CasparCG AMCP (media transport-control semantics);
  the open-source Companion control-surface runtime (Bitfocus — open project, like
  CasparCG/obs-websocket; the post-MVP module target, §11.2/§18);
  the FFmpeg documentation (alpha-capable codec behavior). Where no standard governs
  (M/E layout, flip-flop, DSK tie, stinger trigger semantics) the behavior is labelled
  **de-facto industry practice** and decided explicitly in the ADRs.
