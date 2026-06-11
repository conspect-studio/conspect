# ADR-RT004: Structural backpressure isolation with per-topic conflation and meter sampling

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-02
- **Source brief:** [realtime-api.md](../research/realtime-api.md)

## Decision

The engine publishes state into tokio::watch (latest-wins) and discrete events into tokio::broadcast, and NEVER awaits a client. One per-connection session-pump task selects over subscribed receivers, conflates state, samples meters from a single watch::<MeterSnapshot> at the client's clamped rate_hz (10–30 Hz, default 20), assigns seq, and try_sends into a small bounded per-connection mpsc (~256) — the only client-fillable queue. broadcast Lagged(n) is converted to $lag + re-snapshot. Overflow policy: state/meters conflate, lossless streams emit a gap marker + re-snapshot, logs drop-oldest, persistent wedge closes the socket. Enforced code rule: try_send only on hot branches; no realtime future is .awaited on a data-plane thread.

## Rationale

This is the non-negotiable requirement and the realtime mirror of the engine's bulletproof-output invariant (resilience-av.md) and efficiency.md's 'bound every queue to depth 1–3, drop-oldest; unbounded queues are the OOM failure mode.' watch::send/broadcast::send never block the sender, so the compositor/encoder/output core cannot be stalled by any subscriber. Meter conflation in the per-connection task decouples wire rate from production rate so a 1000 Hz meter stream costs nothing extra for a slow client.

## Alternatives considered

Unbounded per-connection queues (canonical OOM failure); blocking/awaiting sends from the engine (reintroduces backpressure — forbidden); per-subscriber synchronous meter taps on the audio thread (backpressures audio — violates resilience-av §5); a global shared queue (head-of-line blocking across clients).

## Consequences

Slow clients lose/coalesce their own messages or are disconnected (one socket, never a frame/stall). Lagged(n) handling is the most error-prone spot and must be unit-tested per topic. A backpressure conformance test (stalled consumer → zero engine/output-validity impact + bounded memory) is mandatory. Per-connection metrics must be aggregated, never labeled by connection id.
