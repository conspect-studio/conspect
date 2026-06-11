# multiview-engine — agent notes (PROTECTED OUTPUT CORE)

The heart of the product: the **fixed-cadence output clock**, compositor drive,
supervisor/actors, hot-reconfiguration, and the admission/degradation loop. This is where the
two load-bearing invariants live — treat them as hard rules, not guidelines.

- **Inv #1 (output-clock):** one monotonic clock emits exactly one valid, correctly-timestamped
  frame **per tick, forever**, independent of any input. `out_pts = f(tick)`. **No data-plane
  path may block** on an input, a client, or a lock an input/client holds. Inputs are *sampled*,
  never *pacing*.
- **Inv #10 (isolation):** the engine **never `.await`s a client** and never sends on a channel
  a slow consumer can fill. Control/preview/realtime are watch/broadcast + bounded drop-oldest.
  If you add a channel from engine → outside, **prove it cannot stall the engine** (CI chaos gate).
- **Inv #9 (degradation):** sense→estimate→plan→apply with hysteresis; shed load tile-by-tile
  cheapest-impact-first **before** program output is touched. Bounded queues drop, never grow.

**No `unwrap`/`expect`/`panic!` on the hot path.** Hold last-good; return/handle errors.
Re-stamp all output PTS/DTS from the tick counter (inv #3) — never feed raw input PTS to the muxer.

**A change that risks #1 or #10: stop, write a design note, add a chaos/soak test.**

Read first: [core-engine §4–§12](../../docs/research/core-engine.md) ·
[resilience-and-av](../../docs/research/resilience-and-av.md) ·
[streaming-gotchas §0](../../docs/research/streaming-gotchas.md) · ADR-T001 / R001 / R004 /
E007 / RT004. Map: [codebase-map](../../docs/development/codebase-map.md).
