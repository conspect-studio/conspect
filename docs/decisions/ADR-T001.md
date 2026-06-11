# ADR-T001: Single internal monotonic timeline + fixed-cadence output clock drives the compositor

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Carry all internal timestamps as i64 nanoseconds on one monotonic media timeline. Run a free-running output clock seeded from CLOCK_MONOTONIC at a fixed rational cadence (1001-family safe, e.g. 60000/1001). Output frame N gets out_pts=N in 1/out_fps timebase, produced on an absolute-deadline ticker (sleep_until + busy-spin tail) independent of input jitter. Per tick, the GPU compositor samples each tile's frame with media_time nearest-but-not-after N/out_fps, holding the last good frame on starvation. Re-stamp ALL output PTS/DTS from the output counter; never propagate input PTS to the muxer.

## Rationale

This is the only design that guarantees bulletproof continuous output: a stalled, bursting, or wrong-fps input can never stall or speed up the output because the output is gated by the wall clock, not any input. Verified as the model used by OBS (os_gettime_ns fixed FPS render loop), GStreamer (single pipeline clock, running-time) and Membrane LiveCompositor. Source-level verification confirmed a shared filtergraph is gated by its slowest input (framesync FFERROR_NOT_READY), so input-driven output is rejected.

## Alternatives considered

Drive output from a master input's PTS (breaks when that input dies/wraps); event-driven output on every input frame (bursting, variable fps); one shared ffmpeg filter_complex with -fps_mode cfr (one stalled no-EOF input freezes the whole graph).

## Consequences

Requires a custom clock/pacer and per-tile frame store rather than a single ffmpeg CLI graph. Needs quanta/spin_sleep for sub-ms cadence (OS sleep granularity ~1-15 ms; absolute deadlines prevent cumulative drift). Output frame spacing must be measured/validated, not assumed.
