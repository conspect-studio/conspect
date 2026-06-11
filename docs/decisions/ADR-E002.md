# ADR-E002: NV12-throughout pixel-format policy; YUV->RGB only in-shader

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Keep the entire pipeline in NV12 (YUV 4:2:0, 1.5 B/px) end-to-end. Never store RGBA. Convert YUV->RGB inside the compositor shader at the small tile size (sample Y + interleaved UV planes, apply BT.601/709/2020 matrix in-shader). Allocate per the decoder's reported linesize/alignment, not width*1.5. Pick NV12 8-bit as the single canonical working format; normalize any P010/10-bit tiles to it on-GPU before compositing.

## Rationale

RGBA is 2.67x the memory and bandwidth of NV12 (7.9 vs 3.0 MB at 1080p); hardware decoders output NV12 natively. Verification CONFIRMED NVDEC output stays YUV (resize never produces RGB) and that in-shader YUV sampling avoids ever materializing an RGBA copy per tile — multiplicative across N tiles, the difference between fitting and OOM on small GPUs.

## Alternatives considered

Convert to RGBA once at decode (simpler shaders) — rejected: 2.67x memory/bandwidth across all tiles plus a full-frame convert pass. Mixed 8/10-bit working format — rejected: forces per-tile convert passes.

## Consequences

Compositor needs per-colorspace sampler/pipeline variants (immutable Vulkan YCbCr samplers bake in matrix/range). FFmpeg overlay_*/xstack_* require all inputs share exact NV12 layout or they silently insert conversions — must pre-normalize and add telemetry that fails loudly on inserted scale/convert.
