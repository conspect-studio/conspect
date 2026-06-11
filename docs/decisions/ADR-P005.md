# ADR-P005: Output preview = tap the REAL encoded bitstream; label real-vs-approx always

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-02
- **Source brief:** [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Default per-output preview to a TAP of the output's REAL encoded packet stream at the existing multiview-serve encode-once-mux-many fan-out point, decoded back (at reduced resolution where the backend supports it; skip_frame=nokey for thumbnails) for the operator — a confidence/return-feed monitor. For HLS/LL-HLS outputs, preview by replaying the output's own already-published segments (byte-for-byte consumer experience, zero extra encode). For NDI/host-only outputs, tap the emitted host frame and label its color CONVENTION (no encoded bitstream exists). A PRE-ENCODE CANVAS APPROX is shown ONLY when a real tap is genuinely impossible (e.g. no encode session, NDI host-only with no decode tap), with an explicit reason. EVERY preview surface (thumbnail, MJPEG, WHEP) carries a non-negotiable on-video label REAL ENCODED OUTPUT (tap: <protocol>) vs PRE-ENCODE CANVAS APPROX (driven by /api/outputs/{id}/preview/source), and the verification UX surfaces the resolved color tuple + post-encode ffprobe pass/fail (from bitstream VUI/SEI + fMP4 colr, honoring frame > codec ctx > container > policy precedence), re-run after every encode (re)init/remux/cutover with a freshness timestamp (stale = amber).

## Rationale

The entire point of an output preview is to catch per-rendition color-tag, scaling, GOP, and encode-artifact differences and confirm 'what the consumer actually gets' — a pre-encode canvas tap cannot reveal any of those. Tapping the real already-encoded packets reuses existing fan-out with zero extra encode; replaying published HLS segments is the literal consumer view at zero cost. Mandatory labeling prevents an operator from signing off color/encode correctness on an approximation, which would defeat the feature and re-introduce the exact silent color bugs the color runbook warns about. Reusing the existing ffprobe verification gate keeps a single source of truth for delivered color.

## Alternatives considered

Always preview a pre-encode canvas approximation (rejected: hides the very artifacts the operator must verify). Spin up a second full encode of the canvas per output just to preview (rejected: forbidden by encode-once policy; session-budget hostile). Silently mixing real and approx without a label (rejected: dangerous, could certify wrong output). Trusting config for color tags instead of the bitstream (rejected: the bug class is exactly when the encoder writes something other than config asked).

## Consequences

Decoding every previewed rendition back to pixels adds decode-engine load (worst on Intel/AMD/CPU where decode-downscale is post-decode), so it is reduced-res + I-frames-only + concurrency-capped, and HLS outputs prefer the zero-decode published-playlist path. The fidelity label and /preview/source endpoint are required and CI/UX-tested for presence. NDI previews are explicitly distinguished as convention-tagged emitted frames, not VUI-tagged encoded renditions. Verification freshness must be tracked and surfaced (amber on stale).
