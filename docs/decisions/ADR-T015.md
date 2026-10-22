# ADR-T015: Switcher timing — every duration is an integer frame count at the exact rational output cadence; ms converts via exact rationals (round-half-up, minimum 1 frame); every progress is a pure function of the tick

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-11
- **Source brief:** [production-switcher.md](../research/production-switcher.md)
- **Builds on / relates to:** [ADR-T001](ADR-T001.md) (fixed-cadence output clock, `out_pts = f(tick)`,
  invariant #1), [ADR-T003](ADR-T003.md) (exact rationals, never float fps, invariant #3),
  [ADR-T012](ADR-T012.md) (the "reference informs, never paces" posture this ADR mirrors for
  transitions), [ADR-0054](ADR-0054.md) (switcher state machine at the frame-boundary seam;
  Tick-widened control hook), [ADR-0055](ADR-0055.md) (transition taxonomy), [ADR-0057](ADR-0057.md) /
  [ADR-0058](ADR-0058.md) (media players / stinger mezzanine), [ADR-0059](ADR-0059.md) (switcher
  audio / AFV), [ADR-M012](ADR-M012.md) (config blocks), [ADR-W021](ADR-W021.md) (REST surface)

## Context

A switcher is timing machinery: transition rates, dip dwell, FTB rate, DSK auto rates, stinger
pre-roll/trigger/clip windows, macro waits, AFV ramps. De-facto industry practice splits on units —
some ecosystems express rates in frames, others in milliseconds — and the repo's invariant #3
forbids the easy wrong answer (float seconds). The substrate for the right answer is **BUILT** and
verified:

- `Rational` (`crates/multiview-core/src/time.rs:35`) and the `Fps` newtype
  (`crates/multiview-config/src/schema.rs:33`) — an exact `"num/den"` cadence whose deserializer
  *deliberately rejects* bare floats like `29.97` (NTSC 1001-family exactness).
- `Tick { index, pts }` and `OutputClock::tick()` (`crates/multiview-engine/src/clock.rs:136`,
  `:222`) — the canonical `out_pts = f(tick)`; tests run it over a deterministic
  `ManualTimeSource` (`clock.rs:88`).
- `SampleClock` (`crates/multiview-audio/src/cadence.rs:36`) — the exact per-tick audio sample
  budget by integer remainder accumulation (the NTSC 1601/1602 alternation at 30000/1001 @ 48 kHz),
  with `total_at(tick_count)` (`cadence.rs:80`) and `advance_to` (`cadence.rs:110`) as pure
  functions of the tick index.
- The per-sample equal-power `GainRamp` (`crates/multiview-audio/src/mixer.rs:35`) and
  `ProgramBus::repoint_crossfade(..., ramp_frames)` (`crates/multiview-audio/src/program.rs:217`) —
  the pop-free crossfade nucleus, parameterised in **sample frames**.
- The frame-domain precedent for synthetic timing: `frame_index(subsecond_ns, cadence: Rational)`
  (`crates/multiview-config/src/timer.rs:510`) — the TIMER source already computes `:FF` fields as
  integers against the exact cadence.

What does **not** exist: the frame-boundary control hook receives no `Tick`
(`FnMut(&mut CompositorDrive<Nv12Image>)`, `crates/multiview-engine/src/runtime.rs:167`, invoked at
`runtime.rs:439`), so no seam today can compute a deterministic per-tick progress.
[ADR-0054](ADR-0054.md) widens the hook; this ADR pins what the switcher does with the `Tick` it
then receives.

## Decision

### 1. Canonical unit: integer frame counts at the program's exact rational cadence

Every switcher duration — transition `rate`, dip rate, wipe rate, DVE rate, FTB rate, DSK auto
rate, stinger `pre_roll`/`trigger_point`/`clip_duration`/`mix_rate`, macro `wait` — is stored and
applied as an **integer count of output frames** (`u32`) at the owning program's exact rational
cadence (`Fps`). Config schema (`[switcher]` blocks, [ADR-M012](ADR-M012.md)) carries **frames
only** (`rate_frames = 30`); there is no float-seconds field anywhere, matching the `Fps`
float-rejection posture. Frame counts are the invariant under cadence change: a 30-frame mix is
30 frames at any cadence (1.001 s at 30000/1001, 0.6 s at 50/1) — rates-in-frames is the de-facto
industry practice for switcher panels, and it keeps stored documents cadence-portable without a
migration.

### 2. API milliseconds convert at the boundary via exact rationals, with a pinned rounding rule

The REST surface ([ADR-W021](ADR-W021.md)) accepts **either** `rate_frames` **or** `rate_ms`
(exactly one — the both-populated-consistency rejection precedent from the routing schema). A
millisecond input converts **once, immediately, at the API boundary** using integer arithmetic
only, then the stored/applied value is frames:

```
frames(ms) = max(1, floor((2·ms·num + 1000·den) / (2000·den)))      cadence = num/den fps
```

i.e. **round-half-up to the nearest frame, minimum 1 frame** (a requested duration never silently
becomes a 0-frame no-op). Worked examples (normative test vectors):

| input | cadence | frames | exact duration |
|---|---|---|---|
| `rate_ms = 1000` | 60000/1001 | 60 | 60·1001/60000 s = **1001 ms** |
| `rate_ms = 250` | 60000/1001 | 15 | 15015/60000 s = **250.25 ms** |
| `rate_ms = 1000` | 30000/1001 | 30 | 30030/30000 s = **1001 ms** |
| `rate_ms = 20` | 25/1 | 1 (0.5 → half-up) | **40 ms** |
| `rate_ms = 8` | 25/1 | 1 (0.2 → min clamp) | **40 ms** |

Responses and state snapshots echo `frames` (canonical) plus a display-only integer `ms`
(rounded), so clients see the quantisation rather than discovering it. Conversion happens exactly
once — durations are never stored in ms and re-converted per use (double rounding).

### 3. Progress is a pure function of the tick; the T-bar is exact fixed-point, conflated

- **AUTO:** an in-flight transition's progress is `clamp((tick.index − start_index) /
  duration_frames)`, carried as the exact integer pair *(elapsed_frames, duration_frames)* — never
  a float, never wall clock. The switcher state machine evaluates it inside the Tick-widened
  frame-boundary hook ([ADR-0054](ADR-0054.md)); floats may appear only at the shader/pixel
  boundary where the rational is converted for blending (pixels are approximate; timing is exact).
  Same start tick + same duration ⇒ a bit-identical progress sequence, unit-testable over
  `ManualTimeSource` with zero real time ([ADR-R011](ADR-R011.md) determinism gate).
- **Manual T-bar:** an idempotent **absolute** position setter. Engine state stores it as `u16`
  fixed-point (`0..=65535`, 65535 = complete — no float in engine state; >15-bit resolution
  exceeds any wipe's pixel granularity). The wire value is an **integer in `0..=10000`** (basis
  points — integers on the wire, never floats, [ADR-W021](ADR-W021.md)), quantised half-up onto
  the `u16` engine fixed point on apply (wire `10000` ⇒ `65535` = complete). Inbound positions
  are **conflated latest-wins**: the drain coalesces to one
  applied position per destination per tick (the `RouteApplier` batch-coalescing precedent,
  `crates/multiview-engine/src/route.rs:31`), and the client/wire cadence is pinned at **≈ 30 Hz**
  (the house 10–30 Hz conflation band; one command per pointer-move sample is forbidden). Outbound
  `transition.progress` is likewise conflated ~30 Hz ([ADR-RT008](ADR-RT008.md)).
- **AUTO-from-T-bar handoff:** resuming AUTO from a manual position `p` computes
  `elapsed_frames = round_half_up(p · duration_frames)` and re-anchors
  `start_index = tick.index − elapsed_frames`; progress is thereafter pure `f(tick.index)` again.
  A held T-bar is a pure value (no timer runs); position 65535 completes the transition
  (flip-flop per [ADR-0055](ADR-0055.md)).
- **Dip phase boundary:** [ADR-0055](ADR-0055.md) Decision 3 splits a `d`-frame dip at
  `⌊d × switch_point⌋` — deliberately **floor**, not the §2 half-up rule, so the first
  (scene→dip) phase never runs past the operator's split point. `d` itself converts from ms via
  the §2 rule; validation requires `d ≥ 2` and clamps the boundary into `[1, d − 1]` so each
  phase gets at least one frame (a 1-frame dip cannot carry two phases and is rejected at
  validation, not silently degraded).

### 4. Stinger windows are frames against the canvas cadence; asset cadence must match exactly

`pre_roll`, `trigger_point`, `clip_duration`, and `mix_rate` are integer frame counts. A stinger
asset must carry a frame rate **exactly equal** to the canvas cadence (reduced-`Rational`
equality, e.g. 60000/1001 ≡ 60000/1001, never "≈59.94") — validated at import into the media
library ([ADR-0057](ADR-0057.md)/[ADR-0058](ADR-0058.md), which also require canvas resolution and
explicit trigger-point metadata) and re-validated at arm time against the M/E's program. Playout
then advances **exactly one mezzanine frame per output tick** (1:1 frame-locked, no retiming, no
resampling); `trigger_point + mix_rate ≤ clip_duration` is enforced at validation
([ADR-0055](ADR-0055.md)). An asset authored for a different cadence is refused with a typed error
(import-transcode is the remedy, not silent retiming).

### 5. AFV ramps derive their sample budgets from `SampleClock` — never from a per-tick average

An audio-follow-video crossfade spanning an `N`-frame video transition starting at tick `t₀` uses

```
ramp_frames = SampleClock::total_at(t₀ + N) − SampleClock::total_at(t₀)
```

— the exact cumulative-sample delta (`cadence.rs:80`), fed to
`ProgramBus::repoint_crossfade(..., ramp_frames)` / the master `GainRamp`
([ADR-0059](ADR-0059.md)). Never `N × samples_per_tick` with a rounded average: at 30000/1001 @
48 kHz the per-tick budget alternates 1601/1602 (1601.6 exact), so the naive product is off by up
to a frame and drifts the audio ramp off the video edge. The video and audio rails share one
anchor — the same `t₀`/`N` from the same switcher state machine — so the ramp ends on the exact
tick the transition completes. FTB's audio fade uses the same mechanism over `ftb_rate_frames`.

### 6. Macro waits: `wait_frames` canonical; `wait_ms` converts at execution time; at-least semantics

Macro steps ([ADR-0054](ADR-0054.md) §macros) carry `wait_frames` (canonical) or `wait_ms`
(converted with the §2 rule **at execution time** against the target program's then-current
cadence — a macro authored under 50/1 must not bake in stale frame counts if the program was
re-built at 60000/1001). Semantics are **at-least-N-frames**: the sequencer is a control-plane
task (invariant #10 — never engine-side), so it guarantees ≥ `N` frames between the previous
step's apply tick and the next step's apply tick, anchored to tick indices observed from engine
state — never to wall-clock sleeps. Scheduling jitter can only lengthen a wait, never shorten it,
and a multi-op step still lands atomically at one frame boundary (the batch-at-tick semantic,
[ADR-RT008](ADR-RT008.md); the openly published obs-websocket protocol's serial-frame batch mode
is the precedent for this contract shape).

### 7. FTB rate is independent; nothing in the switcher ever reads the wall clock

`ftb_rate_frames` is its own field, independent of any M/E's transition rate (de-facto practice:
FTB is the master stage with its own speed, [ADR-0056](ADR-0056.md)). And globally: **no switcher
timing path reads wall-clock time.** Transitions, FTB, stinger playout, DSK autos, and
frame-domain waits are functions of `tick.index` alone (invariants #1/#3); the wall clock exists
in this subsystem only as the API-boundary ms convenience of §2. The posture mirrors
[ADR-T012](ADR-T012.md): external time *informs* (an operator types "1000 ms"), it never *paces*.

## Rationale

The tick counter is the only clock the output obeys (ADR-T001), so anything that must land on, or
finish on, an exact output frame has to be denominated in that counter's units — frames. Float
seconds at 1001-family cadences accumulate the classic ~3.6 s/hour error (invariant #3's founding
example); ms-as-storage invites double rounding; wall-clock anchoring drifts against the tick rail
and has no answer for "what is the progress while held mid-travel". Integer frames + exact
rational conversion makes every timing question answerable as integer arithmetic, every transition
bit-reproducible in tests, and every audio ramp sample-exact against its video edge — reusing
machinery that already exists (`Fps`, `Rational`, `SampleClock`, `GainRamp`) rather than minting a
second timing dialect. The rounding rule is pinned (half-up, min 1) because unpinned rounding is
exactly the kind of cross-component drift (control vs engine vs SPA) that produces off-by-one-frame
bugs no integration test attributes correctly.

## Alternatives considered

- **Float seconds/ms as the internal representation.** *Rejected — invariant #3.* `29.97` is not a
  number the cadence math may ever see; the `Fps` deserializer already rejects it, and float
  progress breaks bit-reproducible transition tests.
- **Wall-clock-anchored transitions (`started_at + elapsed/duration`).** *Rejected — drift and
  pause semantics.* The wall clock and the tick rail diverge (that is *why* ADR-T001 exists);
  progress would jitter against frames, completion would not land on a frame boundary, and a held
  T-bar or paused AUTO has no clean representation. `f(tick.index)` gives exactness, determinism,
  and trivially correct hold/resume for free.
- **Per-output-cadence transition rates (one transition rendered at several cadences).** *Out of
  scope by construction.* A transition is per-M/E **inside one program** with one cadence
  ([ADR-0054](ADR-0054.md)); cross-program feeds (aux/RT-12) carry no transitions because mix math
  is impossible across encoded streams. Multi-cadence delivery of one program is a rendition
  concern, not a switcher-timing concern.
- **Storing both ms and frames in config/state.** *Rejected — two sources of truth.* The schema
  discipline (routing precedent) is one canonical field plus a rejected-if-inconsistent
  convenience input at the API boundary only.
- **Round-to-nearest-even / truncation for ms→frames.** *Rejected.* Banker's rounding is
  surprising at the operator-facing 0.5 boundary and truncation turns "20 ms at 25 fps" into 0
  frames; half-up with a 1-frame floor is the least-surprise rule and is trivially specifiable as
  integer arithmetic (§2 formula).
- **A float `0..1` T-bar position in engine state.** *Rejected.* Quantising once at the boundary
  to `u16/65535` keeps engine state float-free and snapshots byte-reproducible, at resolution far
  beyond visual relevance.

## Consequences

- **One conversion site.** ms→frames lives in one shared control-plane helper with the §2 vectors
  as unit tests; `multiview-config` gains frame-count fields only. The SPA shows both
  (`frames` + echoed `ms`) so operators see the quantisation ([ADR-W021](ADR-W021.md)).
- **The Tick-widened hook is load-bearing** for this entire ADR ([ADR-0054](ADR-0054.md)): until
  the hook receives the `Tick`, no deterministic progress seam exists (verified absent today,
  `runtime.rs:167`/`:439`). Transition timing tests run frame-by-frame over `ManualTimeSource`
  with zero wall-clock dependence — the property/chaos gates of [ADR-R011](ADR-R011.md) build on
  that determinism.
- **Cadence changes re-time frame-denominated durations in wall terms** (a 30-frame mix shortens
  when a program is rebuilt at a faster cadence). This is the documented, industry-conventional
  behaviour; operators who care re-enter ms and get a fresh conversion.
- **Audio exactness is inherited, not re-proven:** AFV/FTB ramps ride `SampleClock`'s
  already-tested integer budgets, so the switcher adds no new rounding surface to audio
  ([ADR-0059](ADR-0059.md)).
- **Wire discipline:** T-bar input ≈ 30 Hz conflated latest-wins; `transition.progress` outbound
  conflated ~30 Hz and excluded from the replay ring ([ADR-RT008](ADR-RT008.md)) — high-rate
  switcher telemetry can never bloat replay or back-pressure the engine (invariant #10).
- **Stinger imports are strict** (exact cadence equality, explicit trigger metadata): the media
  pipeline ([ADR-0057](ADR-0057.md)/[ADR-0058](ADR-0058.md)) must surface clear typed errors and an
  import-transcode path, or operators will be confused by refusals of "59.94" assets on
  60000/1001 canvases.
