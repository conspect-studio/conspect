# ADR-T008: A/V sync & per-input jitter-buffer model

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Per-input adaptive jitter buffer (reorder by RTP seqnum, de-dup, drop too-late, bounded). Audio buffer on WebRTC NetEq principles (relative-delay histogram, 0.95-quantile target, WSOLA accelerate/expand). Size = ~3-4x RFC 3550 J + margin, capped by latency budget: LAN/SRT 50-200 ms, internet 200 ms-1 s, HLS multi-second segment-smoothing buffer. Lip-sync via RTCP SR (NTP-RTP) or container PTS, rebased to master, kept within EBU R37 +40/-60 ms (bias audio behind). Video cadence via ffplay-style thresholds (resync, don't catch up, on >10 s discontinuity). Unify audio to 48k fltp before mixing; synthesize silence/black for audio-only/video-only inputs upstream of the mixer.

## Rationale

NetEq is the most battle-tested adaptive audio jitter buffer; relative-delay measurement absorbs clock drift; WSOLA beats sample drop/insert. RTP A/V are separate sessions and cannot be aligned by RTP timestamp alone - RTCP SR is the spec bridge. SRT's own latency window IS a jitter buffer - budget across transport+app to avoid double-buffering. amix/audiomixer require identical sample rates.

## Alternatives considered

Fixed-size jitter buffer (too laggy or underruns); comparing raw cross-stream RTP timestamps (wrong - different timebases); letting the mixer handle gaps (stalls/desync when a stream is absent); stacking a big app buffer on top of SRT latency (inflated latency).

## Consequences

NetEq constants are speech-tuned (20 ms packets) and must be re-tuned for AAC/Opus music/large GOPs against measured interarrival distributions. Fallback inter-stream sync needed when RTCP SR is absent/infrequent (rely on interleaved container PTS). Minimum viable buffer sizes per LAN/WAN deployment must be validated against measured J.
