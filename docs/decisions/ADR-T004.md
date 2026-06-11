# ADR-T004: HLS ingest pacing: custom PTS-to-wall-clock pacer, not -re

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Between demux and compositor, run a custom pacer: anchor (anchor_wall, pts0) on first frame; release each frame when now() >= anchor_wall + (pts - pts0); bounded ring buffer (~0.5-3 s pre-roll); bounded catch-up (<=~1.25x) for small drift, never instant seek; re-anchor on EXT-X-DISCONTINUITY or large PTS jump. Start at live edge minus hold-back (live_start_index=-3 or EXT-X-START via prefer_x_start=1). Detect live vs VOD (EXT-X-ENDLIST/PLAYLIST-TYPE). Set seg_max_retry>0 (capped); try http_multiple=0 if bursting severe.

## Rationale

-re is documented verbatim as 'equivalent to -readrate 1', is for files, and FFmpeg warns it can cause packet loss on live input; after a stall it refills via an UNTHROTTLED burst. readrate_catchup is CLI-only fftools (unavailable to libav-linked Rust at any version) and only in FFmpeg 8.0+ (confirmed present in our 8.1.1 build but not in 7.1.x/distro builds). HLS bursts are segment-granular, which a per-packet read ceiling cannot reshape.

## Alternatives considered

-re/-readrate 1 (no smoothing, drift-behind, packet loss); readrate_catchup (CLI-only, version-gated); unbounded queue (OOM, time-warped tiles).

## Consequences

Must implement and own the pacer in Rust (mandatory for in-process libav). Any free-running downstream stage reintroduces bursting, so the whole consume path must be wall-clocked. libavformat may not reliably expose EXT-X-DISCONTINUITY to the app - infer from PTS behavior/stream-param changes as a fallback.
