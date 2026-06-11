# ADR-R009: Resilience testing: always-on output-validity probe as SLO arbiter, layered chaos, soak/fuzz, GPU-less CI

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Build an always-on, format-aware output-validity probe as the single arbiter of 'never falters', emitting numeric SLIs to Prometheus (frame-interval jitter, zero gaps, PTS-monotonic, TR-101-290 priority-1=0, freeze/black/silence, all discrete audio/subtitle tracks present + correctly mapped). Compose TSDuck (TS/SRT), Apple mediastreamvalidator (HLS, asserting discontinuities are correctly TAGGED not absent), FFmpeg detection filters, and a custom PTS/interval/track checker. Layer fault injection: Toxiproxy (TCP), tc/netem via Pumba (UDP/SRT/RTSP/NDI), container kill/pause (hung source), mutating test sources (codec/res/fps/aspect), proptest-state-machine for live config/layout chaos (shrinkable repros), and first-class total-blackout + GPU-loss scenarios asserting zero gaps. Soak multi-day under continuous chaos watching RSS/FD/GPU-mem + PTS-vs-wallclock drift. Fuzz demux/parse + config/OpenAPI with cargo-fuzz/arbitrary + afl.rs (hang detection). Run the full suite on a CPU/software backend on every PR; reserve GPU runners for a periodic backend-parity + device-loss job.

## Rationale

You cannot prove 'never falters' from inputs -- only by continuously consuming real output and asserting numeric invariants. Verified: input-clock-driven or single-encoder designs falter on blackout/device-loss, so those scenarios MUST be injected; some discontinuities are legal/unavoidable so assert tagging not absence; RTMP single-track and NDI single-stream mean per-format expectation tables are required; software CI cannot reproduce NVENC/CUDA device loss so a real-GPU job is mandatory; per-track MAPPING (not just count) must be verified via injected tones.

## Alternatives considered

Player-side QoE only (too late/coarse); manual visual checks (not provable/CI-able); asserting zero discontinuities ever (false failures on legal splices); GPU-on-every-PR (expensive/flaky).

## Consequences

A measurable, gating definition of bulletproofness runnable on every PR. Requires the modular per-stage backend negotiation to expose a first-class CPU/software fallback, a per-format expectations matrix, and agreed numeric SLO thresholds (gap definition, max recovery window) before the suite can pass/fail.
