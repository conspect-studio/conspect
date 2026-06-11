# ADR-R002: Three-tier fault isolation: process-isolate FFI ingest and encoder; protect the output core

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Tier A (pure-Rust control/compositor logic) = supervised tokio tasks/threads with catch_unwind at boundaries. Tier B (per-input FFmpeg/NDI/SRT ingest) = OS-process-isolated workers on Linux. Tier C (NVENC/CUDA encoder) = OS-process-isolated worker with proactive recycling. The output/clock/mux core is the most-protected unit, never restarted, decoupled from B/C by bounded drop-newest queues and shared-memory rings. On macOS/Apple-Silicon, B/C may run as in-process re-initable threads (more stable FFI) with aggressive watchdog. Use libav AVIOInterruptCB + per-protocol timeouts so common network stalls self-recover in-thread without a kill.

## Rationale

Verification CONFIRMED catch_unwind cannot catch FFI segfaults/aborts/foreign-exceptions/hangs and is a no-op under panic=abort; and that a thread wedged in blocking FFI cannot be safely killed (no safe Rust thread cancellation), while an OS process can be SIGKILLed without corrupting shared state. NVENC/CUDA have documented multi-day leaks and INVALID_DEVICE degradation cleared only by process restart. The verified nuance: most input stalls (network) ARE recoverable in-thread via interrupt callbacks/timeouts; only residual non-interruptible wedges (DNS, CPU spin, native deadlock, crashes) need the process-kill path.

## Alternatives considered

All-threads (no containment against native faults); all-process (heavier, more IPC); relying on catch_unwind alone (refuted as insufficient).

## Consequences

True fault containment for crash-prone native code; the output core survives any input/encoder crash. Costs IPC + memory + doubled (per-platform) test surface. Need shared-memory ring transport and (for GPU frames) CUDA IPC / DMA-BUF or staging copies.
