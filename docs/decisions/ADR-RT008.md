# ADR-RT008: Switcher realtime topic and event taxonomy

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-11
- **Source brief:** [production-switcher.md](../research/production-switcher.md)
- **Relates to:** [ADR-RT002](ADR-RT002.md) (envelope), [ADR-RT003](ADR-RT003.md) (snapshot/replay),
  [ADR-RT004](ADR-RT004.md) (isolation), [ADR-RT007](ADR-RT007.md) (mixed-cadence topic precedent),
  [ADR-0055](ADR-0055.md) (transition engine), [ADR-W021](ADR-W021.md) (REST surface)

## Decision

**One coarse `switcher` topic.** Add `Topic::Switcher` (wire string `switcher`) to the deliberately
coarse `#[non_exhaustive]` `Topic` enum (BUILT — `crates/multiview-events/src/topic.rs:15`), scoped
finer with the envelope `id` (M/E, keyer, player, or macro id), never with more topics — the same
rule ADR-RT007 applied to `devices`. The addition follows the verified new-topic checklist: `Topic`,
the `Event` enum, `topic_for_event` (`crates/multiview-control/src/realtime.rs:686`),
`event_scope_id` (`realtime.rs:665`), the connect-time snapshot path in **both** transports
(`run_ws_session`, `realtime.rs:878`; `sse_handler` mirrors it), and the AsyncAPI generator
(`crates/multiview-events/src/asyncapi.rs`).

**Two cadence lanes on the one topic**, reusing ADR-RT007's per-event-type ring-exclusion mechanics
(`topic.is_high_rate() || event.is_conflated()`, documented at `topic.rs:108-123` and implemented at
`event.rs:1306-1314`):

1. **Lossless lifecycle events — kept in the bounded replay ring.** `me.crosspoint`
   `{me, bus: program|preview, source}` — emitted on every bus crosspoint change however caused
   (PVW selection, direct program punch, cut, transition completion including the flip-flop
   swap), so a panel tracks bus state from deltas alone (a gap longer than the ring heals via
   the `$snapshot` per-M/E `{program, preview}` mirror below); `transition.started`,
   `transition.completed`, `transition.aborted`, `transition.degraded` (`{reason}`, e.g.
   `stinger_underrun` — [ADR-R011](ADR-R011.md)), `ftb.engaged`, `ftb.released`,
   `keyer.on_air`, `keyer.off_air`, `keyer.dropped_on_loss` ([ADR-R011](ADR-R011.md)),
   `media.player_state` (cued/playing/paused/stopped/eof transitions — discrete state changes, not
   per-frame position), `macro.started`, `macro.step`, `macro.completed`, `macro.halted`
   (`{step, reason}` — [ADR-R011](ADR-R011.md)). Wire names follow the
   shipped `noun.verb` style (`tile.state`, `salvo.armed` — `event.rs:1258-1286`). These are rare,
   operator-meaningful facts: a control surface or the SPA can reconstruct authoritative switcher
   history from them, so they are cheap to keep lossless and must never be conflated. Payloads
   carry the post-change facts (e.g. `transition.completed` carries the M/E id, transition kind,
   the resulting PGM/PVW crosspoints, and whether flip-flop swapped the buses) so consumers never
   have to infer state from event order alone.
2. **Conflated latest-wins lanes — excluded from the replay ring** via `Event::is_conflated`:
   - `transition.progress` `{me, elapsed_frames, duration_frames}` — progress as integer frame
     counts (exact rational `elapsed/duration`, never a float; invariant #3 / [ADR-T015](ADR-T015.md)).
     Published **publisher-side at ~30 Hz**: the producer emits every `N` ticks where
     `N = max(1, ceil(rate_num / (rate_den × 30)))` in integer arithmetic (e.g. `60000/1001` →
     `N = 2` ≈ 29.97 Hz), plus unconditionally at the first and final tick of the transition so
     endpoints are never sampled away.
   - **Tally rides the existing `Topic::Tally` / `tally.state` lane** (BUILT — `topic.rs:49-50`,
     `event.rs:1275`), not a new switcher lane. Internally-derived tally ([ADR-MV006](ADR-MV006.md))
     is edge-triggered — it changes only at switcher lifecycle edges (cut/take, transition
     start/complete, keyer on/off, crosspoint change; both transition sources stay program-tallied
     for the whole window), never per tick — so the existing lossless treatment of `tally.state`
     stands and no conflation change is needed there.
   - **Audio meters ride the existing `audio.meters` high-rate topic** (`Topic::is_high_rate`,
     `topic.rs:121` — already ring-excluded). `Event::AudioMeter` exists with zero emitters today;
     wiring the ~30 Hz conflated emitter is [ADR-0059](ADR-0059.md)'s item, not this ADR's.

**Connect-time snapshot via a latest-wins registry.** A control-plane `SwitcherStateRegistry`
(copy of the devices pattern: `DeviceStatusRegistry`,
`crates/multiview-control/src/devices/registry.rs:25`, read by `devices_snapshot_frames`,
`realtime.rs:493`, seeded in both `run_ws_session` and `sse_handler` — all BUILT and verified)
holds the latest switcher state, written by the CLI `CommandDrain` publisher *after* each
frame-boundary apply — a wait-free control-plane store, never an engine await. On connect, every
client receives a `$snapshot` frame on the `switcher` topic with the full shape: per-M/E
`{program, preview, armed next-transition set, transition {kind, rate_frames, in_flight:
{elapsed_frames, duration_frames}}, upstream keyers on-air}`, downstream keyers `{on_air, tied}`,
FTB `{engaged, level as elapsed/duration frames}`, media players `{asset, state, position_frames}`,
and running macros `{id, step, total_steps}`. Re-snapshot heals the conflated lanes after any gap,
exactly as ADR-RT003/RT007 prescribe; `$resync` rebuilds wholesale.

**The transport gap, and the pinned cadence decision.** Verified against the code, not the briefs:
`run_ws_session` (`realtime.rs:878-928`) **never reads inbound frames** — its loop only awaits
`session.next_delta()` and writes; `$subscribe`/`$set_rate`/`$resume` exist **as types only**
(`event.rs:1095-1107`, wire names at `event.rs:1248-1252`); no caller ever sets a resume cursor
(`SessionStream` is constructed with `resume_after = None`; the replay branch is test-only,
`realtime.rs:548-552`). Every WS/SSE client therefore receives the entire firehose. **Pinned:**
the switcher ships with **publisher-side ~30 Hz conflation now**; implementing per-client
`$subscribe`/`$set_rate` in `SessionStream` is a **follow-on Lane F item** in the
[switcher backlog](../development/production-switcher-backlog.md), not a prerequisite. A
publisher-side cap bounds the lane at the source for every client simultaneously, which is the
only honest control point the shipped transport has; per-client rate negotiation becomes
load-bearing only when many concurrent panels want different cadences.

**Correlation: new `CorrKey` variants — do not repeat the `Route*` gap.** Verified:
`CorrKey::for_command` (`realtime.rs:74-145`) correlates only start/stop and named-salvo
lifecycle; `SwapSource`/`RouteVideo`/`RouteAudio`/`RouteSubtitle`/`ApplyLayout` return `None`, so
their `202` operation ids **never appear as `corr` on any event** — the SPA's fire-and-toast
problem. Switcher commands get keyed outcomes: `CorrKey::Transition { me, phase: Started|Completed }`,
`CorrKey::Ftb { me, phase: Engaged|Released }`, `CorrKey::Keyer { keyer, phase: OnAir|OffAir }`,
`CorrKey::MediaPlayer { player, state }`, and `CorrKey::Crosspoint { me, bus }` — the PVW-set and
direct-punch verbs ([ADR-W021](ADR-W021.md) `…/preview`, `…/program`) each resolve on their
`me.crosspoint` outcome, never fire-and-forget. An `auto` submit registers **both** `Started` and
`Completed` keys so both lifecycle events echo the op id; `cut` registers `Completed` only; FTB and
keyer commands carry an explicit target state ([ADR-W021](ADR-W021.md)), preserving the existing
rule that only unambiguous outcomes are keyed — never mis-correlated. Macro events need **no**
`CorrKey` projection: the macro sequencer is control-plane code
([production-switcher §10.2](../research/production-switcher.md)) that
owns its operation id and stamps `corr` directly onto `macro.started`/`macro.step`/`macro.completed`.

**Frame-boundary batch semantic for multi-op takes.** A batch is **one** command-bus submission
carrying an ordered list of sub-commands, drained and applied in its entirety inside a single
tick's drain window (the frame-boundary hook, `crates/multiview-engine/src/runtime.rs:432-444`;
the CLI `CommandDrain` already drains all pending commands per tick —
`crates/multiview-cli/src/control.rs`). The open-protocol precedent is the openly published
obs-websocket 5.x protocol specification (`docs/generated/protocol.md`), whose `RequestBatch`
`executionType` `SERIAL_FRAME` processes a whole batch serially within one graphics-frame
boundary — that semantic maps 1:1 onto the existing drain. A batch must respect the per-tick caps
(`MAX_REPOINTS_PER_TICK = 32`, `control.rs:598`): an oversize batch is **rejected at submit
(`422`)**, never silently split across ticks. The batch yields one operation id; each constituent's
lifecycle events carry it as `corr`.

**Spec-generator obligations.** Any change to this taxonomy updates, in the same push: the AsyncAPI
generator (`crates/multiview-events/src/asyncapi.rs`), the committed spec (`docs/api/asyncapi.json`
— embedded via `include_str!` at `crates/multiview-control/src/openapi.rs:28` and drift-tested by
`crates/multiview-control/tests/asyncapi.rs`), regenerated via `cargo xtask gen-asyncapi`. The
OpenAPI/utoipa side is [ADR-W021](ADR-W021.md)'s obligation.

**Invariant posture.** Producers are the CLI publisher (after the frame-boundary apply, publishing
into the existing bounded drop-oldest broadcast) and control-plane tasks (macro sequencer,
registry); the engine never produces directly to clients and never awaits one (invariant #10).
Transition progress is *reported from* `f(tick)` (invariant #1) — events describe the clock, they
never drive it; a wedged subscriber loses samples, never frames.

## Rationale

One coarse topic matches the established subscription model and lets the operator panel subscribe
once and switch exhaustively on `t` (ADR-RT002's discriminated union). The two-cadence split is
exactly the structure ADR-RT007 introduced for `devices` — `switcher` is its second user, which
promotes the per-event-type ring-exclusion rule from a one-off to the documented general mechanism.
The cadences differ because the semantics differ: lifecycle events are discrete facts a consumer
must not miss (a control surface that misses `transition.completed` shows a stuck T-bar; the replay
ring exists precisely for these), while progress is telemetry whose latest value supersedes all
prior values — conflation is *correct*, not lossy. Publisher-side 30 Hz is the repo's standing
conflation doctrine for high-rate telemetry and the only enforcement point that exists given the
verified types-only state of `$subscribe`/`$set_rate`; shipping a per-tick lossless lane into the
current firehose would be the exact anti-pattern the isolation doctrine forbids. The new `CorrKey`
variants close the verified observability gap that makes today's routing commands fire-and-forget
on the wire — a switcher panel needs authoritative completion, not a toast.

## Alternatives considered

**Lossless full-rate (per-tick, e.g. 50/60 Hz) progress lane** — rejected: per-tick events would
dominate and evict the bounded replay ring (whose job is to protect lifecycle history), multiply
across the firehose to every connected client with no per-client rate control in the shipped
transport, and add nothing — progress is derivable to the frame from
`transition.started {start_tick, duration_frames}` plus local interpolation, with `~30 Hz` samples
correcting drift. **Per-M/E topics** (`switcher.me1`, …) — rejected: explodes the deliberately
coarse closed topic set, churns subscriptions on M/E reconfiguration, and duplicates what the
envelope `id` filter already provides (the ADR-RT007 argument, unchanged). **Engine-side macro
events** (engine emits macro lifecycle as it applies steps) — rejected: macros are a control-plane
sequencer per [production-switcher §10.2](../research/production-switcher.md) (invariant #10 — a
sequencer with wait steps has
no business on the clock thread); the engine sees only ordinary commands arriving at frame
boundaries, so only the sequencer can narrate the macro. **Implement `$subscribe`/`$set_rate`
first, then ship the switcher lanes** — rejected as a prerequisite: it serializes the switcher
behind a transport feature the MVP does not need (one operator panel tolerates the firehose today
— the SPA already runs three sockets per session), while publisher-side conflation bounds the new
lane unconditionally; it is scheduled as a follow-on, not abandoned.

## Consequences

The SPA and any external control surface subscribe once to `switcher` (plus the existing `tally`),
receive a complete `$snapshot`, and hold authoritative live state thereafter — `GET` mirrors become
cold-snapshot fallbacks, never the live path. The session pump's per-event-type ring rule now has
two users (`devices`, `switcher`) and must be tested for the second: resume-after-gap replays
switcher lifecycle losslessly while `transition.progress` arrives as a fresh snapshot-then-stream.
A disconnect longer than the ring loses intermediate lifecycle events; `$resync` rebuilds, and the
durable record is the audit log, not the WS stream (ADR-RT003 semantics, unchanged). Until
`$subscribe` lands, every client still receives all topics — the switcher adds at most ~30
conflated progress events/sec per in-flight transition plus rare lifecycle events, which is
bounded and budgeted; the follow-on lane item removes the firehose for all topics at once. UIs
must render progress honestly: samples arrive at ~30 Hz and may be interpolated *between* received
values, but never extrapolated past a `transition.completed` that has not arrived. The AsyncAPI
document grows one channel and ~16 message schemas, all additive minor changes under the v1
envelope that old clients ignore (ADR-RT002 versioning).
