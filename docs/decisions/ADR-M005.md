# ADR-M005: Live-apply vs needs-reset semantics: Class-1/reset-lite/Class-2 + listener-restart, surfaced via dry-run plan

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Classify every mutation and surface it before apply: Class-1 (hot, atomic double-buffered scene-graph swap at frame boundary or NvEncReconfigureEncoder), reset-lite (single IDR/discontinuity within pre-allocated max), Class-2 (controlled reset / parallel-output make-before-break migration for pinned-param changes), and listener-restart (control/health/metrics/TLS bind/port — control-plane reconnect, media unaffected). Provide POST /api/v1/outputs/{id}/plan and POST /api/v1/program:take?dry_run=true returning the classification + reset_reasons + draw_quad_diff. Class-2 changes execute via POST /api/v1/outputs/{id}/migrate (new parallel session, consumer cutover, original stopped after cutover). Canvas resolution/fps/pixfmt/working-color-space changes while bound are the inherent non-hot media path; output geometry/codec/GOP are pinned for the session.

## Rationale

24/7 'never falters' operation requires that operators always know whether an edit is seamless or disruptive before committing, and that disruptive changes are make-before-break rather than in-place. A single dry-run/plan endpoint gives the UI one authoritative classifier.

## Alternatives considered

Apply-and-hope (rejected: breaks live consumers unpredictably); treat every change as a restart (rejected: defeats hot reconfiguration, the core value); in-place mutation of pinned params (rejected: encoders cannot reconfigure structure; causes visible breaks).

## Consequences

HLS gets correctly-signalled EXT-X-DISCONTINUITY only when format demands; RTMP/many players break on Class-2 so the migration banner is mandatory. GPU-loss fall-to-CPU-encode that reduces resolution is a Class-2 transition that must be loudly alerted and audit-logged, not silent. Rollback to a different geometry/codec/track-layout is Class-2 and must show reset impact before applying.
