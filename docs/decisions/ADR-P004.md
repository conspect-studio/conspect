# ADR-P004: Off-air cue mechanism = the pre-warm worker (one machinery for look + take)

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-02
- **Source brief:** [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Preview an off-air source by spinning up a lightweight, process-isolated cue decoder (the SAME Tier B worker model as program inputs: AVIOInterruptCB + per-protocol timeouts + outer DNS watchdog + circuit breaker + supervised backoff), flagged low-priority and admission-controlled against the per-engine budget so cueing can never starve the program. The cue decoder decodes at thumbnail rate and low res only (NVDEC cuvid -resize / VideoToolbox reduced-res / VAAPI-QSV SFC, skip_frame=nokey), requesting a lower substream/ABR variant where available. This cue worker IS the existing 'new input pre-warmed off-air' mechanism: it establishes connection, jitter buffer, decoder, and capability probe, so a subsequent POST /api/inputs/{id}/bind is an atomic scene-graph swap at a frame boundary (Class 1 seamless reconfig) with zero connect/decode startup glitch. Cue endpoints are authenticated, scheme-allowlisted (SSRF guard), rate-limited, and capped in concurrency; idle cues auto-stop (SIGKILL) after linger unless bound.

## Rationale

Off-air sources have no existing decode, so some decode is unavoidable to preview them — but doubling it as the pre-warm worker means one mechanism serves both 'let me look before I take it' and 'take it instantly', eliminating connect/decode latency on bind and avoiding a second machinery. Process isolation contains FFI hangs/segfaults from malformed/hostile URLs. Thumbnail-rate low-res + admission control + caps + rate-limiting bound the cost and the SSRF/DoS surface. This directly realizes the broadcast Preview->Program cue-then-take model for inputs.

## Alternatives considered

A full-pipeline decode for cueing (rejected: too expensive, and wasteful since most cues never go to air). A separate throwaway cue path distinct from pre-warm (rejected: duplicates machinery and reintroduces bind latency). In-process (non-isolated) cue decode on Linux/NVENC (rejected: a hostile URL could wedge/crash the core). Unbounded cueing (rejected: starves program decode/VRAM/CPU and is an SSRF/DoS vector).

## Consequences

Cueing consumes real (if small) decode-engine + VRAM budget, so it is capped and rate-limited and shed before program work. The bind path and the cue path must share one worker abstraction. SSRF/DoS protections (auth, scheme allowlist, rate limit, resource caps) are mandatory on the cue endpoint. On base Apple silicon, cue decode competes with the single decode engine, so cue is thumbnail-rate and admission-gated harder there.
