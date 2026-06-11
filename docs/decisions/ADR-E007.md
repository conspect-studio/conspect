# ADR-E007: Resource-adaptive degradation & admission control without breaking output

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Run a WebRTC-style closed loop (Sensors -> single in-process CapacityEstimator -> Adapter proposing one step -> Applier) on a 1-2 s tick with hysteresis and a recovery cooldown. Drive admission from an empirically calibrated per-(codec,res,fps,backend) cost model gated by CPU(cgroup)/GPU(per-engine NVML)/VRAM/encoder-session budgets. Apply a fixed cheapest-impact-first per-tile degradation ladder (decode-res -> tile-fps -> simpler scaler -> faster encoder preset -> bitrate -> output fps -> output res -> shed lowest-priority tiles), with per-tile QoS deciding order. Bounded leaky queues (drop-oldest) at every stage are the hard safety net. Auto ON, fully operator-overridable, every adaptation logged.

## Rationale

This is the battle-tested WebRTC/SFU structure (keep the estimator where the levers are; fast-down/slow-up). Verification CONFIRMED NVENC session caps are per-system, binding, and a moving driver number (now 12, GTX 1630=3) that must be probed not hard-coded; that Apple has 1 decode/1 encode engine on base parts; and that util% is an unreliable sole signal (corroborate with measured fps).

## Alternatives considered

Static caps from config — over-admit or waste headroom. Per-sensor threshold reaction with no central estimator — oscillates, double-counts shared resources. Degrade the whole output first — punishes all viewers for one overloaded tile. Manual-only — cannot survive transient/thermal spikes.

## Consequences

Must probe NVENC session budget at runtime; never hard-code a count. Near saturation cost is super-linear so leaky queues + hysteresis are the real guarantee, not the predictor. Recovery must be slow/hysteretic to avoid flapping.
