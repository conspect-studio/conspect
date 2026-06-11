# ADR-E006: Adaptive dirty-region recompositing & frame-rate harmonization (opt-in)

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Provide a 'static-friendly' mode that skips recompositing tiles which produced no new frame and harmonizes each input's fps to the output clock, driven by source-provided new-frame signals (decoder timestamps / frame arrival) — never pixel-diff readback on the hot path. Keep it adaptive/off by default for full-motion content.

## Rationale

For dashboard/security/low-fps feeds the recomposite+scale is the expensive saved work (encoders already skip unchanged macroblocks). Verification CONFIRMED dirty-region savings only for static/low-fps inputs and that pixel-diff readback can be net-negative (burns the bandwidth it aims to save).

## Alternatives considered

Always full-frame recomposite every tick — wastes work on static feeds. Pixel-diff change detection on the hot path — rejected: readback cost can exceed savings. Always-on dirty-region — rejected: net-negative for full-motion.

## Consequences

Requires reliable per-source new-frame signaling and an fps-harmonization stage. Benefit is workload-dependent; expose enable/threshold as policy and measure before enabling on a given deployment.
