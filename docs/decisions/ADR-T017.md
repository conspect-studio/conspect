# ADR-T017: A/V offset semantics — muxed vs separate feeds, independent audio/video, and bounded realization

- **Status:** Proposed
- **Area:** Timing / Core Engine / Input
- **Date:** 2026-06-13
- **Source brief:** [input-and-consumption-offsets.md](../research/input-and-consumption-offsets.md)
- **Extends:** [ADR-T016](ADR-T016.md) (the layered offset levels + replace semantics),
  [ADR-0059](ADR-0059.md) (`AudioReader` multi-cursor view — one ring, many cursors),
  [ADR-0038](ADR-0038.md) (the ENCODED-pre-decode delay — the cheap path for large video offsets),
  [ADR-T002](ADR-T002.md) (last-good store + state machine), [ADR-E001](ADR-E001.md)/[ADR-E002](ADR-E002.md)
  (decode-at-display-res, NV12)
- **Relates to:** [ADR-T008](ADR-T008.md) (jitter buffer), [ADR-0077](ADR-0077.md) (per-strip lip-sync
  delay), [ADR-E007](ADR-E007.md) (admission/degradation), [ADR-T015](ADR-T015.md) (exact time),
  [ADR-RT004](ADR-RT004.md) (inv #1/#10)

## Context

[ADR-T016](ADR-T016.md) pins *where* an offset is set; this ADR pins *what an offset means for audio
vs video and for the two feed topologies*, and *how it is realized within bounded memory* without ever
touching the output clock. Audio and video must be offset **independently**, and the engine must
handle both an **A+V muxed feed** (one source) and **separate A and V feeds** (distinct sources).

## Decision

1. **Audio and video offsets are independent values** (`audio_offset` / `video_offset`), each an exact
   sample/frame count ([ADR-T015](ADR-T015.md)).
2. **Muxed feed (A+V one source).** The video offset shifts the video read, the audio offset shifts
   the audio read, and **their difference is the lip-sync skew**. The UI exposes both absolutes and an
   `a/v skew` shortcut. The two reads come from the same source's video store and `AudioStore`.
3. **Separate feeds (A and V are distinct sources).** Each source carries its own offset to the
   **common program-clock reference**; aligning them is "delay the earlier to the later". This is the
   external-console / camera-plus-mic case ([switcher-audio.md §13](../research/switcher-audio.md))
   generalized so the *video* source can also be offset. The model is uniform — muxed vs separate only
   decides whether the audio and video reads share a source id.
4. **Negative offset = delay-of-others, normalized per program.** The future cannot be read. A
   negative per-consumption offset is realized **relative within that consumption** (delay the other
   tiles/sources by `|offset|` so the target appears earlier); a negative universal offset is relative
   to the program's other inputs. Because this mutates the effective timing of *other* sources it is
   never a single-source op: resolve it **per program** by computing a program-local offset vector,
   subtracting the minimum (most-negative) requested offset so the vector is non-negative (each entry
   a positive read-behind), capping each by its source's available buffer, and **surfacing the
   realized vector**. Two simultaneous negative offsets are thus well-defined (their relative skew is
   preserved) and the *configured* offsets are never mutated globally — only how they realize within
   one program. It is **honestly capped + surfaced**, never silently clamped, and **never** realized
   by advancing the output clock (inv #1).
5. **Bounded realization (the memory decision, inv #5/#9):**
   - **Audio (any offset):** a cursor `offset_samples` behind live over the **one** `AudioStore` ring,
     via the [ADR-0059](ADR-0059.md) `AudioReader` multi-cursor view — one `AtomicI64` per
     (source, consumption), no audio duplication.
   - **Small video offset:** a **bounded decoded NV12 ring** (depth = max small offset), shared via
     per-consumer read indices. Per-host budgeted because 4K decoded frames are expensive
     (~3.1 GB/4K-source/s, the [ADR-0038](ADR-0038.md) figure).
   - **Large video offset:** the **ENCODED-pre-decode delay** of [ADR-0038](ADR-0038.md) — the decoder
     runs `D` behind; the framestore stays bounded (~6 MB vs ~3.1 GB). The manual offset is,
     mechanically, an operator-set `D` (or a delta on the auto-sync `D`), which is why this reuses
     ADR-0038 rather than a second video-delay path. **`D` stays the shared program-alignment
     horizon, not a per-consumption value:** a per-consumer offset is a read-delta over the existing
     `D`-deep delay line and must not move the program baseline. Only a consumption asking for a depth
     **greater than the program's current `D`** either raises the shared `D` (if all consumers ride
     it) or instantiates a **separate decode-behind** for that consumption (the second-decode case
     below); the dry-run plan ([ADR-T016](ADR-T016.md) / [ADR-M005](ADR-M005.md)) states which,
     so a manual-on-top-of-auto edit never silently shifts the whole program.
   - **Beyond any bound:** degrade to hold-last (video) / silence-fill (audio); never stall (inv #2).
6. **Decode-once-use-many preserved where the offset is shared.** Multiple consumers with the *same*
   offset share the ring/decode; the only second-decode case is a single source needing **independent
   large** video offsets across consumptions — admission-gated ([ADR-E007](ADR-E007.md)) and surfaced
   as a cost.

## Rationale

- **One uniform mechanism** (a read-index shift on a bounded store) covers muxed and separate, audio
  and video, every level — minimal entropy, every existing wait-free property preserved.
- **Reuse the cheap delay** — large video offsets ride the proven ADR-0038 encoded-pre-decode buffer,
  so a multi-second offset costs megabytes, not gigabytes.
- **Honesty over silent clamping** — a negative offset that the other buffers cannot satisfy is capped
  *visibly*; the operator sees the limit instead of a mysterious mis-sync.

## Alternatives considered

- **A — RGBA/decoded duplication per consumer for video offsets.** Rejected: N× decoded-frame memory;
  the shared ring + per-consumer index (small) or shared encoded-delay (large) is the bounded answer.
- **B — A single combined A/V offset only.** Rejected: lip-sync and separate-feed alignment require
  independent audio and video values.
- **C — Realize negative offset by reading ahead.** Impossible (no future); rejected — it is
  delay-of-others or a cap.

## Consequences

- **Positive.** Lip-sync (muxed) and source-to-source alignment (separate) are one model; audio
  offsets are ~free; large video offsets are cheap via ADR-0038; the switcher per-strip delay
  ([ADR-0077](ADR-0077.md)) is the audio special case.
- **Negative / cost.** Small video offsets cost a bounded decoded ring (per-host budgeted); the one
  independent-large-video-offset case can force a second decode-behind (admission-gated).
- **Risks.** The decoded-ring depth bound needs hardware measurement (4K memory); the negative-cap and
  the second-decode policy are open questions in the brief.
- **Deferred (named).** The decoded-ring depth, the negative-cap UX, and the per-consumption
  independent-large-offset policy are tracked in [input-and-consumption-offsets.md §10](../research/input-and-consumption-offsets.md).
