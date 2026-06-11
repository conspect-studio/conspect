# ADR-E003: Composite once, encode the canvas once per rendition

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Composite all tiles into the output canvas and encode exactly once per rendition. Default to the platform fixed-function encoder (NVENC P4-P5 low-latency / QSV medium+low_power / VideoToolbox -q:v on Apple Silicon / VAAPI/AMF); fall back to x264 veryfast+zerolatency only when no hardware encoder exists. Minimal/zero B-frames and lookahead; fixed closed GOP = fps x segment_seconds with scene-cut keyframes disabled. Architect as many-decoders / few-encoders.

## Rationale

Encode is the most expensive and most capacity-capped stage; hardware encoders cost ~1/15th CPU and ~1/17th power of x264 at near-identical live quality and are largely preset-invariant. A multiview is decode-heavy/encode-light, which fits consumer hardware and the NVENC session cap. Verification CONFIRMED the hardware-vs-software quality/efficiency tradeoff and the VideoToolbox ~200 kbps default-without--b:v/-q:v trap.

## Alternatives considered

Per-tile or per-output re-encode — rejected: multiplies the most expensive stage and blows session caps. CPU x264 everywhere — only when no usable HW encoder, or when bandwidth (not CPU) is the binding constraint and a slow preset is sustainable.

## Consequences

Closed fixed GOP enables clean HLS segmenting from the single encode. VideoToolbox -q:v/B-frames are Apple-Silicon-only and must be set explicitly. QSV low_power CBR/VBR needs HuC firmware (i915.enable_guc) — detect before relying on it.
