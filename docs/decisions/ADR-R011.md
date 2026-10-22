# ADR-R011: Switcher resilience — transitions under source failure, keyer/media loss policies, control-plane fault isolation, and chaos/soak gates

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-11
- **Source briefs:** [production-switcher.md](../research/production-switcher.md), [resilience-and-av.md](../research/resilience-and-av.md)
- **Relates:** [ADR-T002](ADR-T002.md) (hold-last-good), [ADR-R004](ADR-R004.md)/[ADR-R010](ADR-R010.md) (Class-1/Class-2 contract), [ADR-R009](ADR-R009.md) (output-validity probe + chaos/soak), [ADR-E007](ADR-E007.md) (admission/degradation), [ADR-0054](ADR-0054.md)/[ADR-0055](ADR-0055.md)/[ADR-0056](ADR-0056.md)/[ADR-0057](ADR-0057.md)/[ADR-0058](ADR-0058.md) (the switcher surfaces these policies govern), [ADR-P007](ADR-P007.md) (PVW bus).

## Context

The production-switcher layer multiplies the ways an input can fail *while it matters*: mid-mix,
as the fill of an on-air key, as a stinger covering the cut, as a media player rolling on a take.
Every one of those moments needs a **pinned, evented, testable outcome** — the alternative is the
operator discovering the policy live. Bad inputs are the product's purpose: a dying source is the
normal case, never an exception path.

The as-built substrate these policies compose (paths verified in-tree 2026-06-11):

- **BUILT** — the tile state machine: `multiview-framestore/src/state.rs` classifies
  LIVE→STALE→RECONNECTING→NO_SIGNAL as a pure function of last-publish age (defaults
  500 ms / 2 s / 10 s), with `state` (producer liveness) vs `state_at` (on-screen picture)
  distinguished, over lock-free hold-last-good stores ([ADR-T002](ADR-T002.md), inv #2).
- **BUILT** — per-cell slate policy: `on_loss FailoverSlate { bars | no_signal | black }`
  (`multiview-config/src/failover.rs:44`), already applied per cell by the run path.
- **BUILT** — the chaos-gate precedent: the MP-1 wedge test
  (`multiview-engine/tests/programset.rs:243`) wedges one program's egress with a stuck consumer
  and asserts the sibling **and the wedged program's own clock** keep ticking — the template
  every new seam must copy.
- **BUILT** — deterministic time: `ManualTimeSource` (`multiview-engine/src/clock.rs:88`) drives
  the runtime tick-by-tick in tests with no wall clock and no sleeps.
- **DOC-ONLY** — the always-on output-validity probe ([ADR-R009](ADR-R009.md)) as the single SLO
  arbiter of "never falters"; the switcher inherits it unchanged.

Invariant posture up front: nothing in this ADR adds a data-plane wait. Every policy below is a
*classification consumed at the frame boundary* (inv #1), every event rides the existing
drop-oldest broadcast (inv #10), and every duration is an **integer frame count at the exact
rational output cadence** — transition progress is `f(tick.index)` ([ADR-0055](ADR-0055.md)/
[ADR-T015](ADR-T015.md)), so no failure mode can even *express* a stall. The failure events
these policies mandate — `keyer.dropped_on_loss`, `transition.degraded { reason }`,
`macro.halted { step, reason }` — are part of [ADR-RT008](ADR-RT008.md)'s **lossless lifecycle
vocabulary** (rare, operator-meaningful, replay-ring); RT008 enumerates them alongside the rest
of the switcher event set.

## Decision

### 1. Source loss mid-transition: the transition completes on schedule, structurally

Transition progress is a pure function of the tick index; **no property of any input appears in
the progress function**, so a dying source *cannot* slow, pause, or extend a transition — by
construction, not by defensive code. Each side of the transition keeps sampling its sources'
stores per tick exactly as a steady-state scene does: a failing source serves last-good, then its
tile state machine classification and the **per-cell `on_loss` slate policy apply per scene** —
the outgoing and incoming scenes each render their own slates, and the mix/wipe blends whatever
each side honestly shows. Completion lands at `arm_tick + duration_frames`, invariant under any
input behaviour; flip-flop, tally release ([ADR-MV006](ADR-MV006.md)) and lifecycle events fire
on that tick. A source that *recovers* mid-transition simply resumes being sampled — no special
case, no re-arm.

### 2. FTB is always available — synthetic black needs no source

Fade-to-black is the operator's last-resort control and must work precisely when everything else
is broken: it is a master-stage ramp toward a generated black frame ([ADR-0056](ADR-0056.md)),
pure `f(tick)`, consuming **no input**. Total-input blackout — every tile slated — does not
degrade FTB in any way; [ADR-R009](ADR-R009.md)'s total-blackout chaos scenario extends to assert
FTB engage/release with zero output gaps while all sources are dead.

### 3. Keyer fill/key loss: a configurable keyer policy — never a stuck half-key

A keyer is treated as a **unit**: loss of *either* plane (fill or key) is loss of the keyer.
The two failure planes are never split — one plane must not sample slate while the other keys it
on-air (a garbage matte cutting a bars slate into program is strictly worse than either honest
outcome). `KeyerLossPolicy` (config, per keyer, [ADR-M012](ADR-M012.md)):

- **`drop_to_off_air` (default, safe):** when the triggering classification is reached, the keyer
  takes itself off-air at a frame boundary (its own rate or cut, per its AUTO config), emits
  `keyer.dropped_on_loss`, and **stays off** until an operator re-arms it — no auto-relatch on
  recovery (a lower-third popping back unbidden is silent wrongness).
- **`hold_last_good` (explicit opt-in):** the keyer keeps compositing the last-good fill/key
  pair. Honest about its risk: a frozen graphic with stale content stays on-air until acted on.
  The event still fires; the UI badges the keyer STALE.

The trigger rides the existing tile state machine — default trigger state **RECONNECTING**
(default ≥ 2 s of no frames), configurable per keyer: tighter (`stale`) for fast-moving graphics,
looser (`no_signal`) for resilient stills. The dwell thresholds are the store's existing ones;
no second timing mechanism is introduced.

### 4. Media players: underrun/EOF honours the pinned EOF policy; stingers degrade to a mix

- A media player hitting **EOF or a decode underrun** behaves per its
  [ADR-0057](ADR-0057.md) EOF policy `{ hold_last_frame | loop | black | auto_off }` — the
  hold-last-frame path is the framestore's existing `HoldForever` semantics, already proven by
  the file-source run path. Underrun mid-playout is EOF-equivalent for policy purposes: the
  store serves last-good while the policy's outcome (hold/loop-seek/black/off-air) applies at
  the frame boundary, evented as `media.player_state`.
- A **stinger that starves while covering a transition** ([ADR-0055](ADR-0055.md)/
  [ADR-0058](ADR-0058.md)) must never freeze mid-cover: the transition engine **demotes to a
  mix** for the remaining frames — the underlying cut still lands at the armed trigger frame,
  the mix completes on the original schedule, and `transition.degraded { reason: stinger_underrun }`
  is emitted. The stinger mezzanine ([ADR-0058](ADR-0058.md) pre-decoded, pool-allocated ring)
  makes this a cold-path event by design; the policy exists so the cold path is still a defined
  picture. Degrade gracefully, never freeze a half-covered frame on program.

### 5. Macro sequencer failure: halt + event — zero engine impact

Macros are a control-plane sequencer replaying ordinary `Command`s with wait steps
([ADR-0054](ADR-0054.md)/[ADR-W021](ADR-W021.md)); the engine does not know macros exist. A step
failure (refused command, missing resource, wait overrun, sequencer panic-guard) **halts the
macro at that step** and emits `macro.halted { step, reason }` on the realtime stream — it never
retries silently and never partially-applies beyond the completed steps. Invariant #10 posture:
the failure domain is entirely control-plane; the engine sees only the same bounded,
non-blocking command queue it always drains, and a halted (or wedged) sequencer is
indistinguishable from an idle client.

### 6. PVW composite failure: PVW slates, PGM is untouched

The PVW bus is a second compose against the same tick ([ADR-P007](ADR-P007.md)); its failure
domain is **isolated from program by construction**. A PVW compose error (backend error, pool
exhaustion) yields a PVW slate frame + a rate-limited event — the program compose for that tick
is unconditional and unaffected, and PVW re-attempts next tick. PVW is also the documented
degradation rung *below* program (shed before any program tile, [ADR-E007](ADR-E007.md) ladder
extension pinned in [ADR-0055](ADR-0055.md)): under pressure PVW degrades rate/resolution, then
sheds entirely, before program output is touched.

### 7. Admission refusal surfaces at arm — never silent, never mid-flight

Arming a transition/keyer/PVW configuration that does not fit the current budget
([ADR-E007](ADR-E007.md)) is refused **at arm/plan time** with an RFC 9457 `problem+json`
response (conventions §6): **422-class** when the ask is structurally impossible on this build/
hardware (the typed `ConfigError` capability precedent), **409-class** when it is valid but does
not fit *now* (transient headroom), each carrying the failing budget term. Under overload, AUTO
demotes to CUT **at arm time** with the demotion in the plan response ([ADR-0055](ADR-0055.md)) —
the operator always learns the real outcome before the take. An **in-flight transition is
program-affecting and is never shed**: complete-then-degrade, no mid-transition surprise.

### 8. Chaos, soak, and mutation gates — every new seam ships with its wedge test

- **Wedge-a-consumer chaos test per new seam** (the MP-1 precedent,
  `multiview-engine/tests/programset.rs:243`): every seam the switcher adds — PVW frame taps,
  tally publication ([ADR-MV006](ADR-MV006.md)), media-player state events, audio meters,
  per-bus monitor stills — gets a test that wedges its slowest consumer and asserts the engine's
  tick counter advances on cadence regardless. A seam without its wedge test does not merge
  (CI chaos gate, inv #10).
- **Deterministic transition-timing tests** on `ManualTimeSource`
  (`multiview-engine/src/clock.rs:88`): arm at tick *N*, assert exact per-tick progress values
  (exact rationals — never float comparisons), completion at exactly `N + duration_frames`,
  flip-flop/tally/AFV edges on their exact ticks, and the §1/§4 loss scenarios replayed
  tick-by-tick (kill a store's producer at tick *k*, assert the composite and the completion
  tick are unchanged).
- **Continuous-transition soak**: hours-long runs with transitions firing continuously
  (auto-cycling mix/dip/FTB at randomized rates, keyers toggling, players looping) under the
  [ADR-R009](ADR-R009.md) harness, watching RSS/FD/GPU-mem and PTS-vs-wallclock drift — the
  switcher's per-tick state machine and pooled scratch must show zero steady-state allocation
  growth.
- **Mutation-tested state machine**: the switcher/transition/keyer-policy state machines are
  pure value machines and get `cargo mutants --in-diff` as a PR gate (a MISSED mutant in changed
  code fails the PR) plus the nightly full run, per the engineering guardrails — coverage is the
  floor, mutation score the target.
- **The [ADR-R009](ADR-R009.md) output-validity probe remains the single SLO arbiter during
  transitions**: frame-interval jitter, gaps, PTS monotonicity and track presence are asserted
  *while* transitions fire — "the transition completed" is not success if the probe saw a gap.

## Alternatives considered

- **Pause-the-transition-on-loss** (freeze progress until the source recovers): rejected —
  violates determinism (progress would no longer be `f(tick)`; the completion tick becomes
  unbounded) and operator expectation (de-facto industry practice: an AUTO completes at its
  rate, full stop), and a held half-mix is a *worse* sustained picture than completing onto an
  honestly-slated tile. It also reintroduces input pacing through the back door — the precise
  thing invariant #1 forbids.
- **Abort-and-revert the transition on loss**: rejected — a revert is itself a program change
  the operator did not request, the dying source is on *both* sides of the revert (it was
  visible pre-transition or is visible post-transition), and the abort decision would need a
  liveness heuristic racing the transition clock.
- **`hold_last_good` as the keyer default**: rejected as default — stuck-graphics risk: a frozen
  lower-third silently displaying stale content on program is the canonical silent-wrongness
  failure. Available as explicit opt-in only (§3).
- **Per-plane keyer degradation** (slate the dead fill, keep keying): rejected — a half-key is
  never a defined picture; the keyer fails as a unit (§3).
- **Stinger underrun freezes the cover frame** until media resumes: rejected — an indefinite
  full-screen freeze on program, gated on a dead media source, is the worst available outcome;
  the mix demotion completes on schedule (§4).
- **Engine-side macro execution** (sequencer inside the engine for frame-accuracy): rejected —
  inv #10; frame-accurate multi-op application already exists via the frame-boundary batch drain
  ([ADR-0054](ADR-0054.md)), so the sequencer gains nothing by living on the data plane except
  the ability to break it.
- **Best-effort (untested) seam isolation** — relying on review to keep new seams non-blocking:
  rejected — the MP-1 wedge pattern exists precisely because isolation claims must be executable;
  every seam ships its chaos test or does not ship (§8).

## Consequences

- Every switcher-era failure mode has exactly one pinned outcome, an event, and a deterministic
  test shape; none can affect tick cadence by construction. The operator-facing rule is
  uniform: **the show completes on schedule; what degrades is the picture content, honestly.**
- New config surface (keyer loss policy + trigger state, media EOF policy, stinger-demotion
  behaviour is fixed-not-configurable) — all desired-state, validated per
  [ADR-M012](ADR-M012.md), all Class-1 to change.
- CI grows a wedge test per seam, a deterministic transition-timing suite, and a soak lane;
  PR cost is bounded (`--in-diff` mutants) with the full mutation run nightly.
- The keyer default (`drop_to_off_air`, no auto-relatch) trades automatic recovery for
  predictability; facilities wanting sticky graphics opt into `hold_last_good` per keyer and
  accept the badged risk.
- A stinger underrun visibly changes the transition's look (mix instead of cover) — documented,
  evented, and vastly preferable to a frozen cover; the mezzanine design makes it rare.
- The probe ([ADR-R009](ADR-R009.md)) gains transition-aware scenarios (loss-mid-mix, FTB under
  blackout, keyer-drop, stinger-starve) — these become held-out acceptance cases for the
  switcher lanes, not author-written tests.
