# ADR-I001: Engine isolation primitives — arc_swap::ArcSwapOption (latest-state) + tokio::sync::broadcast (drop-oldest events)

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-03
- **Source brief:** [realtime-api.md](../research/realtime-api.md), [resilience-and-av.md](../research/resilience-and-av.md)
- **Realizes:** Invariant #10 (isolation); see [ADR-RT004](ADR-RT004.md), [ADR-W008](ADR-W008.md), [ADR-P001](ADR-P001.md)

## Decision

The engine→outside (control / preview / realtime) hand-off uses two wait-free primitives and NOTHING else: latest-state is published through `arc_swap::ArcSwapOption<T>` (a single atomic pointer swap, no lock, latest-wins) and discrete events are fanned out through `tokio::sync::broadcast` (bounded, drop-oldest on the slow-consumer side). The engine only ever `store`s a new `Arc` snapshot and `send`s an event; it NEVER takes a lock a consumer can hold, NEVER `.await`s a client, and NEVER calls a fallible publish path that can block on a full queue. This REPLACES the hand-rolled `std::sync::Mutex`-guarded ring buffer used in the first build-out pass, which made the engine's publish path contend on the same mutex that subscribers held while draining — a structural back-pressure channel from the outside world into the protected core.

## Rationale

Invariant #10 requires that control/preview/realtime be *physically incapable* of back-pressuring the engine, not merely "usually fast". The hand-rolled `Mutex` ring failed this by construction: the publisher and every consumer serialized on one lock. The adversarial review reproduced a **783 µs publish stall** on the engine thread under deliberate consumer lock-contention — an output-clock-class defect, since a tick that has to wait ~0.8 ms to publish state can miss its deadline. `ArcSwapOption::store` is a single wait-free atomic swap with no reader/writer mutual exclusion, and `broadcast::send` returns immediately and drops the oldest entry for a lagging receiver (surfaced to that receiver as `Lagged(n)`, never to the sender). With both primitives the engine's publish cost is bounded and consumer-independent. The fix was validated by a **100 000-tick soak** with hostile, stalled, and flapping subscribers attached: zero missed ticks, zero engine-side stalls, bounded memory. This is the concrete realization of the structural-isolation contract specified in ADR-RT004 and the lock-free desired-state hand-off in ADR-W008.

## Alternatives considered

- **`std::sync::Mutex` ring buffer (the replaced design)** — rejected: publisher and consumers share one lock, so a slow/contending consumer stalls the engine; measured 783 µs publish stall.
- **`tokio::sync::watch` for latest-state** — viable and equivalent for single-value latest-wins; `ArcSwapOption` was chosen for the data-plane snapshot because the swap is wait-free without entering the tokio runtime and the `Option` models the "no snapshot yet" startup state directly. `watch` remains acceptable per ADR-RT004/ADR-W008 where an async wake-up is wanted.
- **`RwLock` on engine state** — rejected: reader contention and priority inversion can still stall the writer (the engine).
- **Unbounded `mpsc` from engine to each client** — rejected: the canonical OOM failure mode; unbounded growth instead of bounded drop.

## Consequences

- The engine carries no client-fillable queue on its side; the only bounded, client-fillable queue lives in the per-connection pump task (ADR-RT004), which subscribes to the broadcast and reads the `ArcSwap` snapshot.
- Consumers must tolerate loss: a `broadcast` receiver can observe `Lagged(n)` and must respond with a re-snapshot (read the current `ArcSwap` value) rather than treating it as fatal.
- Latest-state is conflating by nature — a consumer that reads slowly sees only the newest snapshot, never a backlog. Lossless event streams that genuinely cannot tolerate a gap must carry an explicit gap marker + re-snapshot (per ADR-RT004), not a larger engine-side buffer.
- A CI chaos/soak gate (stalled + flapping subscriber → zero engine-side stall, bounded memory) guards this path against regression; the 100 000-tick soak is its first instance.
