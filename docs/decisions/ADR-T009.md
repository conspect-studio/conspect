# ADR-T009: Per-tile media-time ring uses O(capacity) copy-on-write publish, not an in-place O(1) ring

- **Status:** Accepted
- **Area:** Streaming/Timing (framestore)
- **Date:** 2026-06-04
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md), [streaming-gotchas.md](../research/streaming-gotchas.md)
- **Relates to:** [ADR-T002](ADR-T002.md) (last-good-frame + tile state machine)

## Context

`TileStore` (`crates/multiview-framestore/src/tile.rs`) keeps a bounded
(`RING_CAPACITY = 256`) **media-time ring** of recently-published frames behind an
`ArcSwap<Vec<RingEntry<T>>>`. The ring backs latch-on-tick sampling: `read_at(now)`
/ `state_at(now)` `partition_point`-binary-search the ring for the entry whose
stamp is nearest-not-after `now`, so the output samples the frame that *should* be
displayed at tick `now` (correct under ahead-decode / future-stamped frames — see
[ADR-T002] and the `state_at` freshness fix).

`publish_arc` currently appends via `ring.rcu(|current| { Vec::clone(current);
push; drain overflow })` — a read-copy-update that **clones the whole ≤256-entry
Vec on every published frame** (O(capacity)). A tracked follow-up asked whether
this could be an O(1) in-place ring.

Two hard constraints frame the decision:

1. **The read is the invariant-#1/#10-critical path.** The output clock samples
   `read_at`/`state_at` every tick and **must be lock-free, must never block on a
   writer, and must never observe a torn/inconsistent ring** (a half-written entry
   or a momentarily non-ascending order would corrupt the binary search). The
   `ArcSwap` snapshot gives exactly that: the reader binary-searches a *stable,
   immutable* `Arc<Vec<…>>` and clones one `Arc<T>`.
2. **The publish is on the sampled input thread, never the output hot path.**
   Inputs are *sampled, never pacing* (invariant #1); `publish_arc` runs on each
   source's own ingest/decode thread. Its cost does not — and cannot — stall the
   output clock.

## Decision

**Keep the O(capacity) copy-on-write publish. Do not pursue an in-place O(1) ring.**

The copy-on-write `rcu` is what makes the lock-free, torn-read-free, binary-search
read possible: each publish produces a new immutable snapshot, so a concurrent
reader always sees a consistent, fully-ordered ring. The O(capacity) cost is
~256 `Arc` pointer-clones plus one small `Vec` allocation, on the **sampled input
thread**, per published frame — microseconds, dominated by the surrounding
decode/scale work, and structurally incapable of affecting the output cadence.

## Alternatives considered (and why rejected)

- **In-place fixed-slot ring + atomic write cursor (true O(1) publish).** The
  writer would overwrite slots a concurrent reader is binary-searching, breaking
  the ascending-order precondition and risking torn entries. Making it safe needs
  a **seqlock or epoch scheme** — which adds read-side *retry and complexity to the
  invariant-#1 hot read path* to save work on the non-hot input thread. Wrong
  trade: it spends the budget where it is scarce (the read) to save it where it is
  plentiful (the sampled write).
- **Per-slot `ArcSwapOption` + full scan on read.** Removes the per-publish copy
  but the wrapped ring is no longer globally sorted, so the reader must scan all
  `capacity` slots (O(capacity) **on the hot read path**) instead of an
  O(log capacity) binary search. Strictly worse for invariant #1.
- **Persistent/structural-sharing vector (e.g. `im::Vector`): O(log n) publish.**
  Pulls in a dependency and makes the hot-path index O(log n), so the binary
  search becomes O(log² n) — a hot-read regression to shave a non-hot-path cost.
- **`Mutex<VecDeque>`: O(1) push/pop.** A lock the reader can be made to wait on —
  a direct invariant-#1 violation. Rejected outright.

## Consequences

The publish is O(capacity); a future profile that ever shows it material on a
specific deployment can lower `RING_CAPACITY` (256 is generous — the read only
needs enough history to cover the latch-on-tick window and reconnect re-anchoring)
far more cheaply and safely than introducing a seqlock. The lock-free,
consistent-snapshot read — the property invariants #1/#10 actually depend on —
stays simple and provably correct. Revisit only if a real workload demonstrates
the input-thread publish cost is a bottleneck *and* a seqlock read is proven not to
regress the output sampling path.
