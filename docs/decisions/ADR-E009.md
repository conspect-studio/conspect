# ADR-E009: Per-tier efficiency budgets enforced by perf-regression CI

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Define density as a per-tier vector budget (max tiles @ target fps per core, per GB RAM/VRAM, per watt, plus engine ceilings: decoded MP/s per engine, encode-session count, SFC ratio limits) and publish conservative default configs per tier. Gate CI on iai-callgrind (deterministic instruction/allocation counts, hard fail) plus criterion wall-clock on a dedicated/self-hosted runner; track both and end-to-end density metrics over time (Bencher/github-action-benchmark). Run real 'max tiles @ target fps' density tests on a self-hosted hardware matrix nightly.

## Rationale

Commodity-hardware viability is the product goal, so the headline metric must be 'how many tiles fit' under each constraint per tier. Verification CONFIRMED criterion is unreliable on shared cloud runners (and may not return non-zero on regression), so iai-callgrind must be the hard gate; and that published density anchors are SKU/preset-specific and must be calibrated on target hardware.

## Alternatives considered

A single global 'streams supported' number — ignores tier differences and per-engine caps. criterion-only with cloud thresholds — noisy/false alarms. FPS-only budget — ignores RAM and power, the real constraints on cheap hardware.

## Consequences

Requires a self-hosted runner matrix (entry dGPU, Intel iGPU, AMD APU, base M-series, low-RAM box) and a repeatable per-watt measurement (RAPL/NVML power/IOReport AVE). Instrumentation must compile out in production; profiling has nonzero overhead on the hot per-tile path.
