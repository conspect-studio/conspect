# ADR-T003: Per-input timestamp normalization: unwrap, genpts fallback, monotonic guard, discontinuity re-anchor

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Before the frame store, each input passes through: (a) delta-based wrap unwrap (33-bit TS / 32-bit RTP via int32 delta accumulation into a 64-bit counter); (b) genpts-equivalent fallback for AV_NOPTS using measured cadence; (c) monotonic guard (clamp/drop backwards PTS); (d) delivery-time epoch anchoring (offset = master_now - first_pts); (e) discontinuity re-anchor (offset += continuous_time - new_raw_time) on EXT-X-DISCONTINUITY, TS discontinuity_indicator, or |jump|>~10 s. Schedule by best_effort_timestamp (B-frame display order). On the output muxer, let libavcodec assign DTS; for any stream-copy path clamp dts=max(dts,last_dts+1), pts=max(pts,dts).

## Rationale

Verifications confirmed no single flag works: +genpts only fills PTS when DTS exists; avoid_negative_ts only shifts leading negatives; correct_ts_overflow handles one wrap and its RTP heuristic caused a FALSE rollover at ~13h14m corrupting output (MediaMTX #622). av_interleaved_write_frame ABORTS on the first non-monotonic DTS. Owning the timeline is how GstAggregator/compositor work.

## Alternatives considered

-copyts/-start_at_zero (inherits upstream pathology); trusting libavformat pts_wrap_reference (misfires on bogus SDP rtptime); aggressive DTS clamp on real B-frame copy (corrupts reorder - re-encode instead).

## Consequences

For RTSP prefer your own depacketizer/unwrapper or strip RTP-Info rtptime, since corruption can happen inside libavformat before your code sees packets. MUST test past wrap boundaries with synthetic timestamps - a 24/7 service that ran fine for an hour fails overnight.
