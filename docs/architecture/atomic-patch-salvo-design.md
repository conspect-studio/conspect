# Atomic patch salvo recall — design proposal (#308, #47)

Status: **proposal, not implemented**. A cross-vendor R3 panel must resolve and
review this proposal before implementation. It does not claim that the current
mixed recall is atomic.

## Measured implementation boundary

At the source snapshot used for this note, the HTTP salvo take already submits
patch members via `take_set`, but then submits legacy recall as a **second**
command (`crates/multiview-control/src/routes/salvos.rs:389-406,417-456`). A
successful patch admission followed by a failed recall admission is therefore
possible. The existing legacy `take_salvo` enqueues each source recall separately
and emits `SalvoTaken` (`crates/multiview-cli/src/control.rs:3235-3266`); the
subsequent per-tick video queue is capped and applies intents individually
(`control.rs:2695-2736`). Neither is a single mixed-recall transaction.

The existing patch set validates shape, source, destination and gates before
admission (`crates/multiview-control/src/routes/patch_set.rs:498-559`), reserves
one operation, and uses `SetArm` to coordinate mixer and output halves
(`patch_set.rs:643-745`). The output router consumes a bounded take mailbox
(`crates/multiview-cli/src/output_routing.rs:711-735,2638-2649`) and advances
sets in `on_tick` before feed (`output_routing.rs:2133-2160`). Its existing
proposal and completion protocol is in `output_routing_sets.rs:817-899,933-959`.
The engine's fresh-frame path invokes `control.before_compose` immediately
before `drive.compose` (`crates/multiview-engine/src/runtime.rs:1092-1116`).
Catch-up repeats can invoke `on_repeat` but emit the previous canvas
(`runtime.rs:1042-1077`); composing can fail and emit last-good instead
(`runtime.rs:1114-1137`). Do not equate a control mutation with a visible frame.

## Decision to validate: one prepared, masked transaction

Choose **refuse whole**, not partial application. Off-clock, authenticate every
selected destination and source, resolve the layout and all stream references,
check media compatibility and reserve all bounded resources. Encode the chosen
mask and the complete prepared change as one bounded transaction with one
operation ID. Empty selected scope is a no-op explicitly reported as such.
Unknown source, unknown slot, slot-medium mismatch, unknown layout and
layout-not-admissible use the existing typed refusal vocabulary. A full mailbox,
stale revision, failed reservation or unready resource refuses the whole set;
release all reservations and publish one terminal refusal. Do not enqueue a
legacy `TakeSalvo` after a separately admitted patch.

The mask is applied **before** validation and reservation: unselected layout,
video, audio, tally and UMD components are neither checked for availability nor
mutated. Every selected component belongs to the same transaction. Define and
version the serialized mask with explicit defaults preserving existing saved
salvos; authorize selected objects at take time. A scope that leaves audio
unselected must preserve its prior state even when video changes. Subtitle
crosspoints remain refused for this increment: the existing plan explicitly
returns `slot-medium-mismatch` and directs them to `/patch/take`
(`routes/salvos.rs:499-518`); no subtitle participant in the same transaction
has yet been proven. Do not silently omit them.

## Proposed commit boundary and nonblocking condition

Prepare all work and storage before the clock. Extend the existing `SetArm`
protocol, rather than fan out independent commands: all selected mixer, output
and legacy participants must be ready and pinned to the same scene/config
revision. The output router proposes a future tick T; the switcher and legacy
participant stage their changes for T. At `EngineRuntime`'s fresh-frame
`before_compose` call for T, perform only a bounded, prevalidated whole-scene
swap; compose T from that state. The output router's `on_tick(T)` and dispatcher
must publish the corresponding output moves for T, not earlier. Release a
success outcome only after composition and all output participants commit at T.

This is an implementation obligation, not a claim about today's code. In
particular, repeat ticks may not commit a visible change: a repeat reuses the
old canvas. If T is missed, reschedule the **whole** transaction or refuse it;
never publish a success on a repeat. If composition fails, retain or restore the
old state before any output participant exposes the new state. Existing code
already admits residual failure after mixer commit; proving rollback or changing
the visibility protocol is a blocker to this proposal, not an exception to
atomicity. No participant may publish a partial success. A hardware sink fault
after a successful commit must be reported separately; it cannot retroactively
claim an uncommitted recall succeeded.

For invariant #1, all I/O, allocation, resolution, authorization and blocking
coordination occur off-clock. The tick reads a bounded prepared object, performs
bounded swaps and never waits on an input or client. For invariant #10, one
finite-size transaction occupies one bounded mailbox slot; queues and payloads
have explicit count and byte caps, use nonblocking admission, and reject whole
on saturation. Defer destruction of replaced/aborted storage off-clock. These
are required properties to prove in code and profiling, not properties inferred
from the existing router: `advance_sets` currently constructs `Vec`s
(`output_routing_sets.rs:817-850`). The R3 panel must settle bounded cost,
rollback/commit visibility and cross-thread ordering before implementation.

## Required verification

1. Real engine tick-path integration test with at least two distinct crosspoints
   and a selected layout/audio combination. Capture every emitted frame's tick,
   PTS, scene revision and all destination selections; assert exactly one valid
   correctly stamped frame per tick and that every selected member changes at
   one identical T, with no mixed old/new frame. Assert unselected audio remains
   unchanged. Count outcomes per operation and require one terminal result.
2. Negative control: one impossible selected member (unknown source, wrong
   medium, unavailable layout and bus refusal in separate cases) produces a
   typed refusal and **no changed member in any observed frame**. Race a stale
   scene revision and a mailbox-full admission; verify no reservation leaks.
3. Chaos/soak: flood bounded recall requests while a consumer stalls and encoder
   readiness is delayed; inject a missed proposal, repeated tick and compose
   failure at T. Capture the full frame sequence and p99/max cadence and assert
   the published limits without widening any existing threshold. Verify each
   accepted transaction either lands wholly once at one tick or refuses wholly;
   drops/refusals are visible, queue occupancy remains bounded, and stop drains
   or reports pending work. Add a deliberate partial-apply mutant or test hook
   and prove the negative no-intermediate-frame assertion fails.
4. Cross-vendor panel and independent review cover memory bounds, no lock/alloc/
   free on the clock, concurrency, state revision races, output encoder faults,
   and operator-visible outcomes. Do not label #308 complete on unit-only proof.
