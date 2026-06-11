# ADR-R004: Pin output session params; seamless edits via atomic scene-graph swap; Class-2 changes via parallel-output migration

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

PIN output geometry, codec, GOP structure, pixel format, framerate, and audio/subtitle track layout for the life of an output session; set NVENC maxEncodeWidth/Height to the largest canvas ever needed at session creation. Apply all layout/tile/source/overlay edits via an atomic double-buffered scene-graph pointer swap at a frame boundary, exposed as Preview->Program with Cut and Crossfade. Drive live keyframe cadence with infinite-GOP init + per-frame forceIDR (no restart). Implement any change to resolution-beyond-max / codec / pixel-format / bit-depth / chroma / GOP-structure / track-set as a NEW parallel output instance + consumer migration (original left running until cutover) behind a make-before-break hot-standby encoder.

## Rationale

Verification CONFIRMED: NVENC cannot reconfigure GOP/idrPeriod/frameIntervalP or sync/async mode and can only change resolution within pre-allocated max (still injecting an IDR); VideoToolbox cannot change resolution live at all (must recreate session). Output codec/format/track/resolution changes force new SPS/PPS + IDR + a consumer-visible discontinuity (HLS EXT-X-DISCONTINUITY rebuffer; RTMP/many players break) -> 'live resolution change without faltering' is FALSE for standard consumers. CORRECTION honoured: GOP CADENCE is fully controllable live via infinite-GOP + forceIDR without a restart.

## Alternatives considered

In-place NVENC resolution reconfigure (works encoder-side but not seamless downstream); live codec/track changes on a running output (breaks long-lived consumers); libavfilter graph for layout (cannot add/remove filters at runtime).

## Consequences

No discontinuity is ever needed under normal operation -> stream perpetually valid. Changing pinned params requires a controlled parallel-output session (more code, more GPU/encoder resources during cutover). UI must distinguish seamless (Class 1) from reset-requiring (Class 2) edits.
