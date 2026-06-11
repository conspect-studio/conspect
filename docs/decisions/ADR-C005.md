# ADR-C005: HDR->SDR tone-mapping default: per-tile BT.2390 EETF anchored at 203-nit reference white

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

When HDR (PQ/HLG/BT.2020) tiles are composited into the default SDR BT.709 canvas, tone-map each tile DOWN, per-tile, in linear light, using a built-in BT.2390 EETF (Hermite-spline roll-off) anchored on BT.2408 reference white = 203 cd/m^2 (HDR diffuse white -> SDR 100%, highlights roll off). Decode HLG via inverse-OETF + OOTF (system gamma = 1.2 + 0.42*log10(Lw/1000), nominal Lw=1000 nits) before tone-mapping. Optionally integrate libplacebo (spline/st2094 + scene-thresholded peak detection) as a premium path; offer FFmpeg vf_tonemap (hable/mobius via zscale sandwich) as a libav fallback. Scope HDR output to HDR10 (static) + HLG passthrough only; dynamic per-scene HDR metadata is out of scope for live operation.

## Rationale

A roll-off curve anchored at diffuse white is the single rule that prevents one bright HDR tile from washing out/crushing the SDR tiles (the burned-by bug); linear normalization to peak crushes SDR. Per-tile (not per-canvas) tone-mapping is required because tiles differ. BT.2390 EETF is standardized, cheap, deterministic, and temporally stable (avoids live flicker). 203 nits is ITU-R BT.2408 operational guidance matching Android/W3C compositing practice. Dynamic per-scene HDR metadata is impractical for a live multiview.

## Alternatives considered

Linear scale to peak — rejected (crushes SDR, the core bug). Per-frame dynamic/local tone-mapping — risks temporal flicker/pumping; only with scene-threshold/smoothing. libplacebo-only — adds a heavy Vulkan dependency and per-frame peak instability. Compositing PQ/HLG/709 code values directly — rejected (different nonlinear encodings, grossly wrong brightness).

## Consequences

Active only when HDR tiles meet an SDR canvas. HLG needs a chosen display peak/system gamma. For the opt-in HDR canvas, the inverse path (SDR->HDR up to ~203 nits/58% PQ) and canonical canvas static metadata are needed. Requires a tone-map stage in the linear pipeline between primaries-convert and composite.
