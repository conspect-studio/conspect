# ADR-T016: Layered consumption offsets — a universal input offset plus independent per-output / per-layout / per-switcher overrides

- **Status:** Proposed
- **Area:** Timing / Core Engine / Input
- **Date:** 2026-06-13
- **Source brief:** [input-and-consumption-offsets.md](../research/input-and-consumption-offsets.md)
- **Extends:** [ADR-T001](ADR-T001.md) (output clock owns time), [ADR-T008](ADR-T008.md) (A/V sync +
  per-input jitter buffer), [ADR-0034](ADR-0034.md) (per-stream crosspoints — an offset lives on a
  consumption/crosspoint), [ADR-0030](ADR-0030.md) (passthrough/transcode/multiview programs are the
  consumers)
- **Relates to:** [ADR-T017](ADR-T017.md) (A/V semantics + realization/memory),
  [ADR-0038](ADR-0038.md) (automatic wall-clock alignment — the manual offset trims on top),
  [ADR-0059](ADR-0059.md)/[ADR-0077](ADR-0077.md) (the switcher per-strip lip-sync delay = the
  per-switcher audio offset), [ADR-M005](ADR-M005.md)/[ADR-R010](ADR-R010.md) (apply class),
  [ADR-T015](ADR-T015.md) (exact-rational ms→frames), [ADR-RT004](ADR-RT004.md) (inv #1/#10)

## Context

Operators must be able to **delay (offset) a source** and to set that offset at the level that
matters: a **universal** per-input offset that flows to every consumption, or an **independent
override** for a direct passthrough/transcode **output**, a **layout**, or the **switcher**,
separately when needed. The same source can be on a passthrough output, in a layout, and on the
switcher simultaneously, each wanting its own (or the shared) offset. No such operator control exists
today: the engine has jitter buffers ([ADR-T008](ADR-T008.md)), last-good stores, and *automatic*
time-of-day alignment ([ADR-0038](ADR-0038.md)), but no per-consumption manual offset.

## Decision

1. **An offset is a tick-shifted read index, never a clock change** (invariant #1). A consumer reads
   the source store at `tick − offset` (video) / `cursor − offset` (audio); the output clock is
   untouched. A positive offset reads an older buffered frame; a negative offset is realized as
   delay-of-others ([ADR-T017](ADR-T017.md)) or honestly capped. All offset realization is
   consumer-side, bounded, drop-oldest — the engine never awaits, never blocks (inv #1/#10).
2. **Four levels, replace semantics.** A `universal input offset` on the `Source` is the baseline.
   Each consumption — a passthrough/transcode `Output`, a layout cell binding, the switcher input —
   may carry an **override** that *replaces* the universal value for that consumption:
   `effective = override ?? universal ?? 0`. (Operator intent: "all separately if needed, *or* just a
   universal offset" → replace, not add; an additive mode is a recorded alternative.)
3. **An offset is a property of the consumption/crosspoint** ([ADR-0034](ADR-0034.md)). The per-output/
   per-layout/per-switcher override lives on the route (source→output / source→cell / source→strip),
   so it composes with the existing routing model and the switcher per-strip audio delay
   ([ADR-0077](ADR-0077.md)) is exactly the per-switcher audio override.
4. **Apply class** ([ADR-M005](ADR-M005.md), inv #11): an offset change within the available buffer
   depth is **Class-1** (re-point the read index at a frame boundary, anti-click envelope for audio);
   a change that crosses the buffer depth (into / deeper-into the encoded-delay regime) is
   **Reset-lite / Class-2** ([ADR-R010](ADR-R010.md) make-before-break), surfaced via the dry-run plan
   before applying.
5. **Config & API are additive** ([ADR-T015](ADR-T015.md) exact-rational ms→unit conversion,
   round-half-up, minimum **1 sample for audio / 1 frame for video** — the floor is per media kind so
   an audio-only sub-frame skew remains expressible): `Source.offset { audio_ms?, video_ms? }`, an
   optional `offset` override on each consumption, `PATCH` on the existing resource routes (Class-1 /
   Reset-lite via `X-Multiview-Apply`), an `offset_changed` realtime event (conflated). **The
   `schema_version` is unchanged under an explicit compatibility rule:** every new field is optional
   and defaults to zero / inherit, so a config without them parses and behaves exactly as before
   (older configs accepted unchanged); the change is forward-additive only, and an older binary
   rejecting an unknown `offset` field (deny-unknown-fields) is acceptable rather than silently
   dropping it.
6. **Passthrough offset realization is pinned per topology, not assumed to decode.** A direct
   passthrough output ([ADR-0030](ADR-0030.md)) may never decode, so the decoded-ring / encoded-delay
   read model of [ADR-T017](ADR-T017.md) does not apply unchanged. A non-zero offset on a true
   passthrough output is realized **either** by classifying it transcode-class (forces a decode-behind
   — one mechanism, the default) **or** by bounded packet-delay remux with PTS/DTS re-stamp to the
   program clock (inv #3, never raw input PTS) + keyframe-aligned start + mux continuity handling. The
   choice is an open question tracked in the brief; neither path touches the output clock.

## Rationale

- **Invariant #1 by construction** — the offset only changes *which buffered sample a tick maps to*,
  exactly the discipline of the switcher crossfade and the subtitle/audio control seams; no clock
  moves, nothing waits for an input.
- **Replace matches the operator's words** and keeps resolution trivial and predictable; the override
  is the whole truth for that consumption.
- **Offset-as-crosspoint-property** reuses the routing + apply-class machinery instead of inventing a
  parallel one, and makes the switcher's existing per-strip delay a special case rather than a
  duplicate.

## Alternatives considered

- **A — One global per-input offset only (no per-consumption).** Rejected: the operator explicitly
  needs different offsets for a passthrough output vs a layout vs the switcher concurrently.
- **B — Additive overrides (universal + per-consumption delta).** Recorded as an option; not the
  default — "use a universal offset *or* set separately" reads as replace, and replace is simpler to
  reason about. Revisit if operators want a global shift plus local nudges.
- **C — Realize offsets by delaying the input/clock.** Rejected outright: any path that re-paces the
  output clock or blocks on an input violates invariant #1 — the whole product promise.

## Consequences

- **Positive.** Operators get the exact layered control requested; it reuses crosspoints, apply-class,
  the dry-run plan, and the switcher strip delay; audio offsets are ~free (one cursor each).
- **Negative / cost.** Video offsets cost buffer memory ([ADR-T017](ADR-T017.md)); the config + UI
  grow an offset control at four levels (mitigated by "inherits universal" defaults).
- **Risks.** A large offset edit must be classified honestly (Class-1 vs Reset-lite) so an operator is
  not surprised by a re-warm; the dry-run plan must compute the class from the *current* buffer depth.
- **Deferred (named).** The additive mode, the negative-offset-cap UX, and per-consumption independent
  *large* video offsets (the one second-decode case) are open questions in the brief, admission-gated
  ([ADR-E007](ADR-E007.md)).
