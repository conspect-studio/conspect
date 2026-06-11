# ADR-C003: Composite in linear light with premultiplied alpha

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

Perform all scaling and alpha blending in LINEAR light in a >=16-bit float (Rgba16Float) canvas buffer using premultiplied alpha. Per tile: range-expand -> YUV'->R'G'B' matrix -> linearize via the tile's own EOTF -> (if needed) linear-light primaries 3x3 to canvas gamut -> (if HDR->SDR) tone-map in linear -> scale/composite in linear -> canvas OETF -> YUV encode with canvas matrix+range. Linearize PQ/HLG/BT.709-OETF/BT.1886 manually in-shader; use GPU *_SRGB formats only for genuine sRGB-curve paths.

## Rationale

Light is linear; filter taps and src-over alpha are only correct on linear values. Gamma-space compositing causes dark fringing on edges/semi-transparent overlays and wrong midtones (NVIDIA 'Importance of Being Linear'), independent of all other color correctness. Heterogeneous tiles with different primaries/TRC can only be blended coherently after converging to one linear working space. This mirrors libplacebo and zimg.

## Alternatives considered

Compositing in gamma/YUV space — cheaper but visibly wrong on edges, overlays, scaling, and mixed-gamut tiles; acceptable ONLY as a fast path for opaque, same-space, same-resolution, non-scaled tiles. 8-bit linear buffer — rejected (severe banding). 32F buffer — rejected (2x bandwidth, negligible quality gain at 1080p/4K).

## Consequences

Extra ALU/bandwidth per tile (EOTF + matrix + optional gamut + tone-map + OETF). Requires correct, full-precision TRC/matrix constants and chroma-siting-aware upsampling. The Rgba16Float canvas is the single intermediate for both SDR and HDR canvas modes.
