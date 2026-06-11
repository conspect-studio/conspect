# ADR-P001: Preview isolation model: read-only taps, drop-oldest, Tier A, shed-first

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-02
- **Source brief:** [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Enforce preview isolation STRUCTURALLY, not by best-effort. Every preview tap is a read-only consumer of a capacity-1 latest-wins / drop-oldest slot or a separate depth-1-3 drop-oldest ring (the same lock-free triple-buffer / Arc-swap / watch-channel pattern resilience-av and efficiency already mandate): input preview reads the existing multiview-framestore last-good-frame slot, program preview reads its OWN dedicated downscale ring (never the encoder's NV12 readback ring), output preview registers as a separate consumer on the existing multiview-serve tee fan-out with refcounted O(1) packet clones. All preview work lives in the supervised Tier A task tier (control-plane analog), never the protected Output/Clock Core. Off-air cue decoders run in the Tier B process-isolated, SIGKILL-able worker model. Preview decode/encode is admission-controlled against the same multiview-planner per-engine budgets at LOWEST priority and is the FIRST rung shed by the degradation ladder; program output encoder sessions are reserved first. A CI/soak chaos test stalls and SIGKILLs preview consumers and asserts the program output is byte-for-byte unaffected with zero added frame-interval jitter and zero zero-gap-SLO violations.

## Rationale

The non-negotiable requirement is that a slow/missing/malicious preview consumer can never disturb or back-pressure the program output path. Lock-free drop-oldest slots make back-pressure structurally impossible (a slow reader gets stale data or nothing); reusing the already-mandated framestore/tee patterns means no new hot-path machinery; Tier separation contains panic/OOM/segfault; admission + shed-first guarantees preview loses every resource fight against program. The chaos assertion makes the guarantee provable rather than hoped-for.

## Alternatives considered

Bounded blocking queues the producer pushes into (rejected: any full queue back-pressures the protected path). Running preview in the output core for lower latency (rejected: couples preview faults to the invariant). A soft priority hint without hard admission (rejected: cannot guarantee preview never starves program under contention). Pixel-diff/synchronous readback for taps (rejected: burns the very bandwidth efficiency aims to save and risks coupling).

## Consequences

Preview frames may be stale or dropped under load — acceptable and surfaced to the operator. Requires careful audit that no preview code path ever holds a producer-awaited ref or shares the encoder readback ring. Adds a dedicated no-back-pressure chaos test to the soak suite as a hard gate. Slightly more rings/slots to allocate, but all small and conditional (zero when idle).
