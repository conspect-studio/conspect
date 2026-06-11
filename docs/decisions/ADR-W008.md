# ADR-W008: Engine-command bus: actor + lock-free desired-state hand-off

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

The API is a thin shell that converts validated requests into bounded mpsc Commands (+ oneshot for sync replies) to a supervisor actor, which pushes desired-state to the render/encode hot path via tokio::watch / arc-swap / triple-buffer; async reconfig returns 202 + an operation id confirmed later on the SSE event stream.

## Rationale

Guarantees the real-time render/encode threads never block on HTTP I/O, mutexes, or awaits; bounded channels give backpressure (try_send → 429/503); watch/arc-swap give lock-free latest-state pickup at frame boundaries; 202+event honestly models non-instant frame-boundary reconfiguration.

## Alternatives considered

Shared RwLock on engine state (risks priority inversion / stalling frames); calling render threads directly from handlers (couples HTTP timeouts to engine latency); synchronous-only API (blocks on engine).

## Consequences

Requires the engine to pick up new desired-state via a lock-free mechanism and apply it atomically at a frame boundary — this must be designed in, not bolted on; idempotency keys needed on start/stop/swap; clients must treat 2xx as accepted, not 'already live'.
