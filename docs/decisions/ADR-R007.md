# ADR-R007: Subtitle ingest -> libass burn-in (off hot path) + format-aware discrete passthrough

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Ingest CEA-608/708 (AVFrame A53_CC side data / movie+subcc / CCExtractor), DVB-sub, teletext (libzvbi, page-aware), WebVTT/SRT/ASS. Normalize all to ASS and rasterize with libass (>=0.17 + libunibreak/HarfBuzz/FriBidi) in dedicated threads with a lock-free latest-overlay handoff; the compositor samples per frame and holds/drops if late. NEVER place the libavfilter subtitles/ass filter in the live output path. Passthrough as discrete tracks per a capability matrix: HLS/LL-HLS (segmented WebVTT renditions + in-band 608/708, X-TIMESTAMP-MAP in 90kHz recomputed per segment); MPEG-TS/SRT (DVB-sub PIDs + teletext + 608/708 via A53_CC SEI on output frames). RTMP = burn-in or single 608 only. NDI = burn-in default (FFmpeg NDI muxer has no caption path). FFmpeg cannot rasterize text->DVB-sub: feed libass bitmaps to the dvbsub encoder.

## Rationale

libass alpha bitmaps fit the premultiplied GPU pipeline; decoupling protects invariant A (a stalled/garbage subtitle source degrades to 'no overlay this frame', not an output stall). Verified: embedded 608/708 are side data, not mappable streams; encoders re-emit them via ff_alloc_a53_sei so captions survive re-render; RTMP cannot carry discrete subtitle tracks (CONFIRMED); NDI has no selectable subtitle track and FFmpeg's NDI muxer sets subtitle_codec=NONE (CORRECTED -- official 2025 NDI CEA-708 metadata exists but is not reachable via FFmpeg).

## Alternatives considered

libavfilter subtitles filter end-to-end (synchronous, can stall -- rejected for live); burn-in only (loses selectable/accessible tracks); passthrough only (some outputs/consumers can't render captions, and burn-in is needed for per-tile placement).

## Consequences

Accessible discrete tracks where the format allows; always-works burn-in elsewhere. libass-rs/libass-sys are thin (vendor/fork) and libass is CPU-only (budget CPU + upload sparse alpha). X-TIMESTAMP-MAP per-segment math is a known desync hazard to get right. NDI subtitle passthrough is an advanced direct-SDK best-effort feature only.
