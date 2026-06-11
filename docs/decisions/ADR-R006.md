# ADR-R006: In-process EBU R128 metering, read-only and non-blocking, two cadences, true-peak gated

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Use the pure-Rust ebur128 crate, one EbuR128 per input and per output track. Tap audio read-only into a bounded lock-free SPSC ring (drop-oldest) per track; run DSP on separate threads -- never on the media/compositor/encoder thread, never apply back-pressure. Two cadences: sample-peak + ~50ms RMS at video frame rate for per-tile overlays; M/S/I/LRA/dBTP at 10Hz for UI/compliance. CAP true-peak (4x oversampling) to tracks that display dBTP; enable FTZ/DAZ on meter threads. On a source SR/channel change call change_parameters for continuity (it PRESERVES I/LRA) or reset() for a clean restart -- choose per intent. Stream to the browser over one multiplexed WebSocket at 10-25Hz, binary, numeric-only; ballistics applied client-side. Normalize only the program bus (loudnorm); leave discrete tracks unaltered.

## Rationale

Verification CONFIRMED metering must be read-only/non-blocking or it can stall the deadline-bound output (true-peak is ~2.5-3x costlier, far worse on ARM/Apple-Silicon). Verification REFUTED the original claim that change_parameters resets I/LRA: it re-inits filters/resampler/peak but PRESERVES integration history; reset()/new instance is required to actually restart I/LRA. ebur128 is BS.1770/Tech-3341/3342 compliant, pure Rust (no FFI build hassle), Send+Sync.

## Alternatives considered

FFmpeg ebur128/astats filters (stderr/metadata scraping, parallel decode path, fixed 10Hz) -- keep only as a validation cross-check; inline metering on the audio thread (adds jitter/back-pressure -- rejected); per-meter sockets / pushing audio (connection bloat / bandwidth).

## Consequences

Standards-correct meters without risking the output clock. Must size the meter thread pool for the Apple-Silicon true-peak cost and document the change_parameters-vs-reset contract so per-tile integrated loudness behaves predictably across input reconfigs.
