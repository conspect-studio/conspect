# ADR-R003: Supervision, backoff, circuit breakers, watchdogs, and bounded memory

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Build an OTP-style supervision tree (ractor-supervisor: OneForOne for independent inputs, RestForOne for dependent stages) with per-level restart-intensity meltdown limits (not equal at every level; period 5-10 min). Reconnects use backon (exponential + jitter); per-endpoint failsafe circuit breakers gate hot-looping and half-open probes. Every worker writes a monotonic AtomicU64 heartbeat; a watchdog declares missed deadlines dead and restarts/kills. Cooperative shutdown via tokio-util CancellationToken + TaskTracker, ALWAYS paired with a hard timeout + process kill. Enforce no-panic hot path (clippy deny unwrap/panic), RAII Drop for every FFI handle, refcounted AVFrames + av_frame_ref before crossing boundaries, bounded buffer pools.

## Rationale

Principled self-healing without crash loops; jitter avoids thundering-herd reconnects; circuit breaker stops hammering dead endpoints; heartbeats catch HUNG (not crashed) workers that panic-catching misses; hard timeout guarantees an FFI-wedged worker is force-terminated. Refcounted frames prevent the silent AVFrame-reuse corruption verification flagged. backoff crate is unmaintained (RUSTSEC-2025-0012) -> backon.

## Alternatives considered

Hand-rolled supervisor (re-implements restart intensity/escalation); cancellation-only shutdown (hangs on FFI-wedged workers); unbounded queues (memory blowup).

## Consequences

Self-healing across transient faults; bounded memory; deterministic restart semantics. ractor-supervisor/task-supervisor are 0.1.x -> pin and be prepared to vendor/fork. Mature primitives (tokio-util, backon, failsafe, wgpu) depended on directly.
