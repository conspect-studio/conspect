# ADR-T005: HLS/LL-HLS output: wall-clock-paced, GOP-aligned segments; custom origin for true LL-HLS

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Pace segment PUBLICATION to wall clock (the master output clock guarantees <= one segment per hls_time of real time); after a stall, drop to live - never flush a backlog. Emit closed fixed GOP (-g N -keyint_min N -sc_threshold 0 + forced keyframes at segment boundaries; x265 open-gop=0; choose fps/segment so keyint divides evenly). Standard HLS: hls_segment_type fmp4, hls_flags +delete_segments+temp_file+independent_segments+program_date_time+omit_endlist; PROGRAM-DATE-TIME anchored to a real monotonic->UTC clock. For true sub-second LL-HLS build a custom CMAF + HTTP/2 blocking origin (EXT-X-PART/PART-INF/SERVER-CONTROL/PRELOAD-HINT, blocking _HLS_msn/_HLS_part) or use GStreamer hlscmafsink / republish via MediaMTX.

## Rationale

Output bursting is segments published faster than realtime. FFmpeg's lhls flag emits only EXT-X-PREFETCH and is documented 'not Apple's version LHLS' (verified on ffmpeg-formats.html); there is no EXT-X-PART option, so FFmpeg cannot produce LL-HLS that Safari/hls.js lowLatencyMode consume. hls.js maxLiveSyncPlaybackRate under-fires and never recovers post-stall drift (issues #4681/#6350) - player catch-up is secondary only.

## Alternatives considered

-lhls expecting low latency (silently ignored by players); relying on maxLiveSyncPlaybackRate as primary fix (flaky); VFR output (variable segment durations, pacing chaos).

## Consequences

True LL-HLS requires building/operating an HTTP/2 blocking origin (axum/hyper + tokio::sync::Notify per part) - load-test against real Safari (HTTPS required) and hls.js behind the actual CDN (chunked-transfer for parts forbidden; many CDNs handle blocking poorly). For browser players you do not control, no-bursting cannot be guaranteed.
