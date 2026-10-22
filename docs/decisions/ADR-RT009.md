# ADR-RT009: Connect-time broadcast watermark suppresses deltas already reflected in the snapshot

- **Status:** Accepted
- **Area:** Realtime API
- **Date:** 2026-07-10
- **Source:** cross-vendor auth-panel finding on PR #211 (reviewer A2; pre-existing defect, task #8) + the PR #230 review panel's lost-delta finding (the scoping below)

## Context

The WS/SSE realtime stream follows snapshot-then-delta + resume-by-seq
([ADR-RT002](ADR-RT002.md)/[ADR-RT003](ADR-RT003.md)): on connect a client gets a
`$snapshot` of current state "with the seq/ts it is current as of", then a stream of
deltas, one per engine event. Both transports
(`crates/multiview-control/src/realtime.rs` `run_ws_session` / `sse_handler`)
subscribe to the engine's drop-oldest event broadcast **before** reading the connect
snapshot — the correct order, because subscribing first guarantees no event is missed
between the snapshot read and the first live delta.

But **no watermark suppresses the events that land in that subscribe→snapshot window.**
An event can enter the subscription, be folded into the snapshot's source, be **included
in the connect snapshot**, and then **also be delivered again as a delta** — a duplicate;
several queued pre-snapshot transitions can replay *after* the newer snapshot, so a client
**rolls backward** transiently (violating ADR-RT003's `snapshot ⊕ ordered deltas = current
truth`).

Two facts bound the fix:

1. **Two independent sequence counters** (`crates/multiview-engine/src/isolation.rs`): the
   latest-state slot (`LatestState`) and the event broadcast (`EventStream`) each have their
   own `sequence()`. The value threaded through the connect path as `snapshot_seq`
   (`state.engine.state.sequence()`) is the **state-slot** counter, used only for the
   envelope `ts` — it is not comparable to a broadcast `SeqEvent.seq`. The watermark must
   come from the **event** counter, `state.engine.events.sequence()`.
2. **The publish path must not gain a client-shared lock** (invariant #10;
   [ADR-RT004](ADR-RT004.md)). Any fix is a **read-side, per-connection** decision on events
   already received from the bounded broadcast.

**The critical bound — the connect snapshot only reproduces two event classes.** The
connect snapshot the client receives is exactly three frame kinds: `$hello` (session
metadata, backs no delta), the **tiles** snapshot (from the engine state blob's `tiles`,
reproducing `tile.state`), and the per-device **`device.status`** snapshot (from the
`DeviceStatusRegistry`, reproducing `device.status`). **No other event appears in any
connect snapshot frame.** The device *lifecycle* events (`device.discovered`/`.mode`/
`.error`/`.adopted`/`.removed`/`.sync`), cast-session membership, `media.player_state`,
alerts, input/job/alarm/tally/salvo events, and the un-re-snapshotted conflated telemetry
(`timing.status`, `audio.meter`, `system.metrics`, `rist.link.stats`) are **lossless or
event-only with respect to the connect snapshot** — they are published event-only (many via
`DeviceBroadcaster` in the control plane, which calls `EnginePublisher::publish_event`
directly without a snapshot-backed state fold), and `device.discovered` carries no registry
id at all (it is in neither the state blob nor the registry). A watermark that drops *every*
event with `seq <= watermark` would **permanently lose** any such event that lands in the
subscribe→snapshot window — it is in no snapshot and carries no seq the client can resume,
strictly worse than the duplicate the watermark fixes.

## Decision

Capture a per-connection **broadcast watermark** = `state.engine.events.sequence()` at
connect, paired with the snapshot read, and **drop a subscribed event iff
`seq <= watermark` AND the event is *snapshot-backed*** — before issuing the per-connection
sequence. Concretely, in `crates/multiview-control/src/realtime.rs`:

- **Snapshot-backed classification.** `event_is_snapshot_backed(event)` returns `true` for
  exactly `Event::TileState` and `Event::DeviceStatus` — the only two variants the connect
  snapshot reproduces as a same-topic frame. Every other variant returns `false` and is
  **always delivered**, watermark or not.
- `SessionStream` gains `snapshot_watermark: Option<u64>` + `with_snapshot_watermark(u64)`.
  `None` = unchanged (resume path / transport-only tests).
- `SessionStream::frame_for` drops `iff seq <= watermark && event_is_snapshot_backed(event)`,
  returning `None`, **before `issue_seq`** — so the drop leaves no gap in the per-connection
  seq (resume-by-seq intact) and composes with the object-scope filter.
- Both `run_ws_session` and `sse_handler` capture the watermark **immediately after
  `subscribe()` and before the snapshot read** and pass it via `.with_snapshot_watermark`.

### Which events the watermark applies to

| Event class | In a connect snapshot frame? | Watermark |
| ----------- | ---------------------------- | --------- |
| `tile.state` | Yes — the tiles snapshot (engine state blob `tiles`) | **Drop if `seq <= watermark`** |
| `device.status` | Yes — the per-device `device.status` snapshot (registry) | **Drop if `seq <= watermark`** |
| `device.discovered` / `.mode` / `.error` / `.adopted` / `.removed` / `.sync` | No (lossless lifecycle; `device.discovered` has no registry id) | **Never drop** |
| `cast.session.*`, `media.player_state`, alerts, `input.*`, `job.progress`, alarms, tally, salvo | No (lossless) | **Never drop** |
| `timing.status`, `audio.meter`, `audio.loudness`, `system.metrics`, `rist.link.stats` | No (conflated, but **not** re-snapshotted at connect) | **Never drop** |

**Atomicity (why watermark-before-snapshot, and why it is safe *for the two backed
classes*).** Both snapshot-backed producers update the snapshot source **before** the event
is published: the engine tick publishes `publish_state(state_of(frame))` then
`publish_event(event_of(frame))` for the same frame (`runtime.rs`; `event_of` emits only
`tile.state`), and `DeviceBroadcaster::publish_status` calls `registry.set_status(..)` then
`publish_event(Event::DeviceStatus(..))` (`devices/broadcaster.rs`). So for a snapshot-backed
event with `seq <= watermark`, its snapshot fold *happens-before* its seq, *happens-before*
our watermark read, *happens-before* our snapshot read — the snapshot reflects it (or a newer
value), so dropping the delta loses nothing. Lossless/event-only variants are exempt by
class, so the ordering argument never has to cover them. This mirrors the fence idiom already
used in this crate for the discovery-scan correlation window (`routes/discovery.rs`:
`from_seq = state.engine.events.sequence()`).

## Rationale

- **Correct sequence space.** The watermark is read from the counter that stamps
  `SeqEvent.seq` and the resume cursor (`resume_after`), so `seq <= watermark` is meaningful;
  the old `snapshot_seq` (state-slot counter) is not.
- **No lost deltas — the class boundary is the safety boundary.** Only the two variants the
  snapshot literally reproduces can duplicate, and only those are ever dropped. A lossless
  lifecycle event in the window is delivered, so a client never silently loses a
  `device.discovered`/`.mode`/`.error` (RT003 losslessness).
- **No persistent backward-roll over no duplicates.** Read-side capture cannot be perfectly
  atomic with a lagging snapshot source without an engine-shared lock (forbidden by #10).
  Watermark-before-snapshot biases to *at most a transient, self-healing duplicate* of a
  backed variant (its state folded into the snapshot but its seq just above the watermark),
  never a dropped delta — within RT003's at-least-once, idempotent `snapshot ⊕ delta` rebuild.
- **Invariant #10 by construction.** `events.sequence()` and the state reads are single
  wait-free atomic loads; `event_is_snapshot_backed` is a `matches!`. No lock, no `.await`,
  no new channel, no back-pressure.
- **Composes with the contract.** Dropping before `issue_seq` keeps the per-connection seq
  gapless (resume-by-seq intact); the watermark drop runs before the
  [ADR-W005](ADR-W005.md)/[ADR-W025](ADR-W025.md) object-scope filter, which is unchanged.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **Global watermark — drop *every* event with `seq <= watermark`** | **Lost-delta defect (PR #230 panel):** event-only / lossless-lifecycle events (`device.discovered`/`.mode`/`.error`, cast/media/alert/tally/…) are in **no** connect snapshot and carry no resumable seq, so a global drop permanently loses any that land in the subscribe→snapshot window — worse than the duplicate being fixed. Scoped to snapshot-backed variants instead. |
| Reuse `snapshot_seq` (`state.engine.state.sequence()`) as the watermark | Wrong sequence space — the state-slot and event-broadcast counters are independent. |
| Capture the watermark **after** the snapshot (snapshot-first) | Given the producers' fold-then-publish order, risks dropping a backed event whose fold had not yet reached the snapshot — a lost delta, strictly worse than a self-healing duplicate. |
| Engine-side atomic `(snapshot, event-seq)` under one lock | Introduces a lock shared between the publish path and the connecting consumer — the back-pressure surface #10/[ADR-RT004](ADR-RT004.md) forbids. |
| Set `resume_after = Some(watermark)` instead of a new field | `resume_after` switches `next_delta` to the non-blocking `try_recv` replay mode; a fresh connect must stay on the blocking live-tail `recv().await`. |

## Consequences

- The systematic connect-time duplicate / transient backward-roll is fixed **for exactly the
  two event classes that can exhibit it** (`tile.state`, `device.status`); every other event
  is delivered untouched, so no lossless lifecycle signal is ever dropped by the watermark.
- **Invariant #10 preserved:** one wait-free atomic load + a `matches!` per delta; no lock,
  no `.await`, no channel into the engine, no back-pressure. The engine publish path is
  untouched.
- Resume-by-seq ([ADR-RT003](ADR-RT003.md)), the per-connection gapless seq, and the #211
  object-scope projection ([ADR-W025](ADR-W025.md)) are unchanged.
- **We commit to the invariant that the *only* snapshot-backed delta classes are
  `tile.state` and `device.status`.** If a future connect snapshot frame reproduces another
  event class (e.g. a `timing.status` re-snapshot), `event_is_snapshot_backed` MUST be
  extended in lockstep, and the producer's fold-then-publish order verified — otherwise the
  new class either duplicates (if omitted) or is lost (if added without the ordering). A
  residual sub-microsecond duplicate of a backed variant remains possible and is accepted as
  self-healing under the at-least-once contract.
