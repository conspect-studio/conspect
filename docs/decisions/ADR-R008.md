# ADR-R008: Overlay rendering: serializable layer stack, glyphon/Vello+SDF, premultiplied alpha, dirty-region uploads, input-decoupled

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Adopt an OBS-style serializable ordered Layer stack (kind, target full|tileN, transform, z, opacity, blend, clip, color-space, data-binding, visibility); compositor walks it each frame blending premultiplied 'over'. Render text + small images with cosmic-text + glyphon on wgpu (CustomGlyph for logos/icons/meter caps); rich graphics with Vello/Vello-Hybrid BUT use hand-written SDF/textured-quad shaders for must-never-fail elements (SIGNAL LOST card, meters) with assets atlas-resident at startup. Use premultiplied alpha end-to-end (wgpu OVER = src=One/dst=OneMinusSrcAlpha); premultiply swash/zeno straight-coverage before upload, upload already-premultiplied libass as-is. Dirty-region / sub-texture uploads + persistent atlas caching; meters as uniform-driven quads. Blend in linear light; per-layer color-space tag; sRGB->BT.2020 PQ/HLG for HDR programs. Overlays render purely from local state, never from a live input frame. Output overlays as a backend-agnostic premultiplied RGBA atlas + quad list.

## Rationale

Verified: premultiplied OVER is correct and a straight/premultiplied mismatch halos EVERY antialiased edge (silent, pervasive); full-canvas re-upload is ~249MB/s @1080p30 (competes with decode/encode, tight on Apple unified memory) so dirty-region+atlas caching is load-bearing; no stable wgpu external-texture import; overlays must be input-decoupled so the alert path is drawable when all inputs/GPU are gone; Vello is alpha/compute-dependent so critical overlays need a robust fallback.

## Alternatives considered

FFmpeg drawtext/overlay filtergraph (disruptive to reconfigure live, scales poorly with N tiles -- fallback only); straight alpha (fringing); Vello for everything (alpha-stage risk on critical paths); re-uploading whole canvas each frame (bandwidth starvation).

## Consequences

Web-UI/template-fully-drivable overlays at near-constant per-frame cost; correct edges and HDR color. Requires disciplined premultiply handling per rasterizer, atlas growth/eviction management, and a portable atlas+quad output contract so any compositor backend (wgpu/Metal/CUDA-NPP) can consume it.
