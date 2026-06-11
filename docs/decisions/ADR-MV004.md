# ADR-MV004: Introduce a multi-head output model and salvo/scheduled layout automation

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-02
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md)

## Decision

Generalize the single composited output into a multi-head model: one engine renders multiple independent output heads, each with its own canvas resolution, layout/preset, source assignment and overlay set, with optional video-wall spanning of one logical layout across heads (per-display geometry/offset + bezel compensation) where Multiview feeds a physical wall. Add named salvos — atomic multi-tile recalls (layout + source assignment + UMD + tally arming) applied on an arm/take, recallable by API, soft/hardware panel, GPI, NMOS, schedule (time/calendar) or event (SCTE-104/35, BXF). Output orientation (portrait/landscape) and per-tile rotation are per-head properties.

## Rationale

Multi-head independent layouts and salvo/scheduled recall are near-universal in professional multiviewers and unlock operational workflows (different walls/operators, event-driven layout changes) that a single-output compositor cannot express. The salvo concept is the multiviewer analogue of a router salvo and composes cleanly with Multiview's existing layout presets and live hot-swap.

## Alternatives considered

Single output only (rejected: cannot drive multiple independent walls/operators); per-tile changes without atomic salvos (rejected: partial application causes visible inconsistency on take); hard-coded schedule outside the engine (rejected: must be a first-class, auditable engine feature); treating SCTE-35 as a mandatory trigger (rejected — exposed as a design option, not assumed).

## Consequences

Multiple heads multiply encode/compose cost and must respect admission control and the output-clock invariant per head. Salvos need arm/preview/confirm semantics and conflict handling with live operators. Video-wall bezel compensation is lower priority for a stream product and is gated behind an explicit physical-wall output-mapping use case.
