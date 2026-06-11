# ADR-P003: On-demand activation + auto-stop lifecycle (cost ~zero when idle)

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-02
- **Source brief:** [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Gate ALL preview cost behind subscriber refcounts per (entity, mode={grid|focus|llhls}). First subscriber starts the tap / cue decoder / preview encoder; last unsubscribe tears it all down after a short linger (5-15 s) to avoid thrash on grid scroll. Teardown is triggered on BOTH explicit close (DELETE/WHEP resource, socket close) AND timeout (ICE/RTCP/HTTP idle), plus an idle watchdog that force-stops a tap if no read occurs for N seconds (guards against leaked refcounts on abrupt drops). The program-canvas downscale blit is conditionally SKIPPED entirely when subscriber count is 0; per-output taps are not registered when nobody watches; off-air cue decoders are SIGKILLed when idle (unless the source has been bound). The grid is viewport-driven: only on-screen cells are subscribed. Current subscriber count and tap-active state are exposed via the descriptor endpoints for observability and a CI assertion that idle cost returns to ~0.

## Rationale

Efficiency on commodity hardware demands that nobody-watching equals near-zero cost. Refcounting with linger balances cost against scroll thrash; dual connect+timeout teardown plus a watchdog closes the leak vectors (abrupt socket drops, silent WebRTC disconnects). Conditional GPU blit means even the cheap program tap costs nothing at rest. Viewport-driven subscription makes a 200-source list affordable. Exposing subscriber/tap state lets CI hard-assert the idle-cost invariant.

## Alternatives considered

Always-on taps for instant view (rejected: violates the cost-~zero-idle requirement, especially on iGPU/APU/base-Apple). Teardown only on explicit close (rejected: leaks on abrupt drops). No linger (rejected: churns encoders/decoders on every grid scroll). Per-viewer taps (rejected: defeats encode-once-serve-many).

## Consequences

First view of a cold entity has a brief warm-up latency (acceptable; mitigated for off-air by the cue pre-warm). Requires correct refcount bookkeeping and a watchdog, and an idle-cost CI assertion. Linger means a small, bounded residual cost for a few seconds after the last viewer leaves.
