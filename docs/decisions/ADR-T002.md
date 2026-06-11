# ADR-T002: Per-tile frame resampling = hold-last-good + duplicate/drop (sample on output tick)

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Each input decodes independently into a per-tile single-slot latest-frame cell (overwrite policy). The compositor samples it on each output tick: duplicate (hold) when the source is slow, drop (newest-wins) when fast. No blending by default. Offer linear-blend (framerate) and motion-compensated (minterpolate) only as explicit opt-in per-tile quality modes. Detect real fps from a rolling median of decoded PTS deltas; never trust r_frame_rate/avg_frame_rate/SDP.

## Rationale

Mathematically equivalent to nearest/previous-PTS resampling with implicit dup-on-stall and drop-on-overrun, at zero motion-interpolation cost (the fps/videorate model, confirmed dup/drop-only on FFmpeg 8.1.1). Bounded memory via single-slot overwrite. minterpolate does per-frame block motion estimation (tens of ms/HD) and cannot sustain N tiles at 50/60 fps live; framerate ghosts on motion.

## Alternatives considered

Global minterpolate (stalls under load); per-input fps/videorate filter chains feeding one xstack (couples all inputs to one graph clock).

## Consequences

A 30 fps source on 60 fps output shows each frame twice (judder); 60->50 drops irregularly. Accepted unless the spec secretly requires smooth low-fps motion (OPEN QUESTION) - if so, this flips to minterpolate and limits tile count; benchmark first. Real input fps still needed for jitter-buffer sizing and drop-only-vs-duplicate policy.
