# ADR-E008: Cost-model-driven planner with a capability+cost registry

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Maintain a registry keyed by (stage, backend, codec) recording decode-scale tier, zero-copy availability, session/engine caps, and measured decode-ms/encode-ms (calibrated at startup via ffmpeg -benchmark differencing plus per-engine telemetry, refined continuously from live frame times). The planner consumes this registry to choose the cheapest-that-fits plan and to feed admission/degradation. Tag the negotiated backend on every Tracy/NVTX/signpost zone so cost auto-attributes to the right engine.

## Rationale

Static per-GPU tables cannot span the hardware range (old dGPU, iGPU, APU, base Apple, low-RAM); Apple gives no public session count and NVENC throughput varies by part. Verification CONFIRMED ffmpeg -benchmark captures only host CPU/RAM (HW decode/encode is off-CPU), so the model must combine -benchmark with per-engine telemetry and measured fps.

## Alternatives considered

Hard-coded per-GPU capacity tables — brittle, wrong for unlisted/old hardware. Admit-then-observe with no model — visible overload before correction. Util%-only model — over/under-provisions (Apple GPU reads ~0 during transcode; NVIDIA %enc can read 0 / %dec cap ~50%).

## Consequences

Needs a startup calibration phase and a continuous learning path. Per-engine accounting (NVDEC vs NVENC vs SM vs media-engine vs memory-bandwidth) and per-device for multi-GPU. Registry is the shared source of truth for HAL negotiation and the planner.
