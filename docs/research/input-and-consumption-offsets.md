# Multiview — Input & Consumption Offsets (layered A/V delay)

**Area:** Timing / Core Engine / Input (cross-cutting: `multiview-input` · `multiview-framestore` ·
`multiview-audio` · `multiview-engine` · `multiview-config` · web/)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-T016](../decisions/ADR-T016.md) (layered consumption offsets — universal input +
per-output / per-layout / per-switcher overrides), [ADR-T017](../decisions/ADR-T017.md) (A/V offset
semantics + muxed-vs-separate feed topology + bounded realization).
**Extends:** [timing-architecture.md](timing-architecture.md) / [input-timing-and-sync.md](input-timing-and-sync.md),
[ADR-T008](../decisions/ADR-T008.md) (A/V sync + per-input jitter buffer — the bounded ring the
offset reads from), [ADR-0038](../decisions/ADR-0038.md) / [wall-clock-sync.md](wall-clock-sync.md)
(the **automatic** time-of-day alignment buffer — a manual offset trims on top of, or instead of, it),
[ADR-0059](../decisions/ADR-0059.md) / [ADR-0077](../decisions/ADR-0077.md) /
[switcher-audio.md §16](switcher-audio.md) (the per-strip lip-sync delay — the **per-switcher audio**
expression of this model), [ADR-0034](../decisions/ADR-0034.md) / [decoupled-routing.md](decoupled-routing.md)
(an offset is a property of the **crosspoint / consumption**), [ADR-0030](../decisions/ADR-0030.md) /
[multi-program.md](multi-program.md) (passthrough / transcode / multiview programs are the consumers).
**Backlog:** `OFFSET-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> Operators need to **delay (offset) a source's media** — audio and/or video, on a muxed feed or on
> separate feeds — and to set that offset at the level that matters: a **universal** per-input offset
> that flows everywhere, or an independent override for a **direct passthrough/transcode output**, a
> **layout**, or the **switcher**, separately if needed. This brief pins one uniform model:
> **an offset is a tick-shifted read index into the per-source bounded store — never a change to the
> output clock** (invariant #1). It is the missing operator control on top of the engine's existing
> jitter buffers, last-good stores, and automatic wall-clock alignment.

---

## 0. Headlines

- **An offset is a read-index shift, not a clock.** The output clock ticks forever, independent of
  any offset (inv #1). A consumer samples the source store at `tick − offset_frames` (video) /
  `cursor − offset_samples` (audio). A **positive** offset reads an *older* (already-buffered) frame;
  a **negative** offset (show the future) is impossible directly and is realized as **delay-of-others**
  (§4), or capped. Beyond the bounded buffer it degrades honestly (hold-last / silence-fill), never
  stalls (inv #2/#5).
- **Three override levels over one baseline.** `universal input offset` → optionally overridden,
  independently, by a `per-output` (passthrough/transcode), a `per-layout`, and a `per-switcher`
  offset. Override **replaces** the universal value for that consumption; absent an override the
  universal applies; absent both, zero.
- **Audio and video are independent**, and the model is identical whether A+V arrive **muxed on one
  feed** (the offsets re-time demuxed audio vs video = lip-sync) or as **separate feeds** (each is its
  own source with its own offset to a common reference).
- **Integer time only** (inv #3): offsets are exact frame / sample counts; `ms →` units via exact
  rationals, round-half-up ([ADR-T015](../decisions/ADR-T015.md)) — minimum **1 sample** for audio,
  **1 frame** for video (the floor is per media kind; an audio-only sub-frame skew is expressible).
  Never float.
- **The switcher already has the audio half of this** — the per-strip lip-sync delay of
  [ADR-0077](../decisions/ADR-0077.md) §4/§16 *is* the per-switcher audio offset; this brief
  generalizes it to video and to every consumption, and the switcher strip delay becomes its
  switcher-level expression.

## 1. The requirement

The operator gap, verbatim intent:

1. **Input offsets, both audio and video** — and handle where they arrive **on one feed
   (A/V together)** vs **separately (A and V as distinct feeds)**.
2. **Per-output offset** for a **passthrough/transcode direct-to-output** (each output independently).
3. **Per-layout offset** (the input's use in a multiview composition).
4. **Per-switcher offset** (the input feeding the M/E), separately if needed.
5. **Or just a universal offset on the input** that applies everywhere.

This is a *layered* control, not a single delay: the same source can be on a passthrough output, in a
layout, and on the switcher **at the same time**, each wanting its own (or the shared) offset.

## 2. The model — a tick-shifted read, never a clock change (invariant #1)

Multiview's data plane is already built for this. Every source writes a **bounded store**:

- **Video:** the per-tile last-good store + tile state machine (`multiview-framestore`, inv #2) — but
  a *single-slot* last-good store only carries `offset = 0` (latest). A non-zero per-consumer video
  offset needs **depth** — see §5 for the memory decision (small offsets = a bounded decoded ring;
  large offsets = the ENCODED-pre-decode delay of [ADR-0038](../decisions/ADR-0038.md)).
- **Audio:** the `AudioStore` ring + `read(frames)` that silence-fills gaps
  ([ADR-0059](../decisions/ADR-0059.md): a ~2 s ring, one `AtomicI64` cursor) — an offset is a
  **cursor started `offset_samples` behind live**, read by the existing `read`.

**The offset is realized entirely by *where each consumer reads*, on the consumer side, off the hot
output loop.** The output clock samples at the tick; the offset only changes which buffered sample
the tick maps to. No path blocks, no clock is re-paced, nothing waits for an input (inv #1). This is
the same discipline as the switcher's `repoint_crossfade` and the subtitle/audio control seams:
consumer-owned, drop-oldest, the engine never awaits (inv #10).

## 3. Levels & precedence

| Level | Where set | Applies to | Realized at |
|---|---|---|---|
| **Universal input** | the `Source` (per source; optional split `audio_offset` / `video_offset`) | every consumption of that source, unless overridden | the source's stores / decode-behind |
| **Per-output** | a passthrough/transcode `Output` (program of kind `passthrough`/`transcode`, [ADR-0030](../decisions/ADR-0030.md)) | that output only | that program's read cursor |
| **Per-layout** | a layout's use of the source (a cell binding) | that layout/multiview only | that program's tile read |
| **Per-switcher** | the switcher's use of the source (an M/E input / audio strip) | the switcher program only | the switcher's tile read + the audio strip delay ([ADR-0077](../decisions/ADR-0077.md)) |

**Resolution (replace, not add):** `effective_offset(consumption) = override(consumption) ??
universal_input_offset ?? 0`. The operator's words — "all separately if needed, *or* just use a
universal offset" — pin **replace** semantics: a per-consumption override is the whole offset for
that consumption, not a delta. (An *additive* "universal + trim" mode is a recorded option, §10.)

A consumption is a **crosspoint** ([ADR-0034](../decisions/ADR-0034.md)): source→cell, source→bus,
source→output. The override therefore lives on the crosspoint/route, and changing it is the same
Class-1 re-point as any crosspoint edit (§6).

**Passthrough realization (open).** A *direct passthrough* output ([ADR-0030](../decisions/ADR-0030.md))
may never decode — the decoded-ring / encoded-pre-decode read-index model of §5 assumes a
decode/transcode path. A non-zero offset on a true passthrough output must therefore be realized at
the **packet level**: bounded packet delay with PTS/DTS re-stamp (re-based to the program clock, inv
#3, never raw input PTS), keyframe-aligned start so the downstream decoder is not handed a mid-GOP
packet, and continuity handling at the mux boundary. The simpler default is to **classify any non-zero
offset on a passthrough output as transcode-class** (it forces a decode-behind), which keeps one
mechanism; the bounded-packet-delay remux is the efficiency option to pin per topology. Tracked as an
open question (§10).

## 4. A/V split & feed topology

Audio and video offsets are **independent values**. What an offset *means* depends on how the source
arrives:

- **Muxed feed (A+V together, one source).** Video offset shifts the video read; audio offset shifts
  the audio read; their **difference is the lip-sync correction** (the classic "audio leads/lags
  video by N ms"). The operator can set both (absolute, vs the program clock) or, in the common case,
  just the relative skew — the UI exposes both an `a/v skew` shortcut and the two absolute values.
- **Separate feeds (A and V are distinct sources — e.g. camera video + desk/USB mic, or an external
  console master).** Each source has its own offset to a **common reference** (the program clock).
  Aligning the two is "delay the earlier source until the later one"; this is exactly the
  switcher-audio external-console workflow ([switcher-audio.md §13/§16](switcher-audio.md)) and the
  per-strip delay ([ADR-0077](../decisions/ADR-0077.md)) generalized to also offset the video source.
- **Negative offset (advance).** You cannot read the future. A negative per-consumption offset is
  realized **relative within that consumption** — delay every *other* tile/source in that program by
  `|offset|` so the target appears earlier — and a negative *universal* offset is relative to the
  program's other inputs. It is bounded by what the other sources' buffers allow and is honestly
  capped (and surfaced) rather than silently clamped (the switcher-audio rule: "negative delay is
  delay-of-others — never violate the output invariant"). Because "delay-of-others" mutates the
  effective timing of *other* sources, a negative offset is **never a single-source operation**:
  resolve it **per program** by normalizing a program-local offset vector — subtract the minimum
  requested (most-negative) offset across that program's consumptions, so the vector becomes
  non-negative and each entry is a positive read-behind, then cap each by its source's available
  buffer and **surface the realized vector**. This makes two simultaneous negative offsets
  well-defined (their relative skew is preserved) and never changes the *configured* offsets — only
  how they realize within one program.

The model is uniform because, muxed or separate, every offset is a read-index shift on a bounded
store; "muxed vs separate" only decides whether the audio and video reads share a source id.

## 5. Realization & memory (the efficiency decision)

| Path | Mechanism | Cost |
|---|---|---|
| **Audio offset (any level)** | a cursor `offset_samples` behind live over the **one** `AudioStore` ring, via the [ADR-0059](../decisions/ADR-0059.md) §5 `AudioReader` multi-cursor view — N consumers, N cursors, **one ring** | one `AtomicI64` per (source, consumption); no audio duplication |
| **Small video offset** (≤ a small bound, e.g. ≤ ~0.5–1 s) | a **bounded decoded ring** of NV12 frames per source (depth = max small offset), shared by all consumers via per-consumer read indices | decoded-frame memory — **expensive at 4K** (the [ADR-0038](../decisions/ADR-0038.md) warning: ~3.1 GB per 4K source per second), so the small-ring bound is modest and per-host-budgeted (inv #5/#9) |
| **Large video offset** | the **ENCODED-pre-decode delay** of [ADR-0038](../decisions/ADR-0038.md): the decoder runs `D` behind, the framestore stays bounded (~6 MB vs ~3.1 GB/4K) | encoded-buffer memory — cheap; this is how large offsets / time-of-day alignment scale |

**Multiple consumers, different offsets, one source:** never duplicate the source pipeline. Audio
multiplies the cursor only (`AudioReader`). Video multiplies the read index over the shared ring for
small offsets; for *independent large* video offsets the ENCODED-pre-decode delay is per-program
(decode-once-use-many is preserved where the offset is shared; a program needing a *different* large
video offset than its peers is the one case that costs a second decode-behind — flagged in the
efficiency budget and admission-gated, [ADR-E007](../decisions/ADR-E007.md)). Beyond any bound the
read degrades to hold-last (video) / silence (audio) — never a stall (inv #2).

## 6. Live-apply classification (invariant #11)

- **An offset change is Class-1** within the buffered range: re-point the read index / cursor at a
  frame boundary, exactly like a crosspoint re-point ([ADR-0034](../decisions/ADR-0034.md)) or the
  switcher audio gain/route ([ADR-0059](../decisions/ADR-0059.md) §4). A small audio offset step uses
  the per-strip anti-click envelope; a video offset step is a frame re-point (a one-frame visual step,
  honestly the same as a cut).
- **A change that crosses the available buffer depth** (e.g. raising a video offset past the decoded
  ring into the encoded-delay regime, or deepening the encoded delay `D`) is **Reset-lite / Class-2**
  ([ADR-M005](../decisions/ADR-M005.md) / [ADR-R010](../decisions/ADR-R010.md)): it re-warms the
  delay line via make-before-break so the consumption is never silently broken. The dry-run plan
  ([ADR-M005](../decisions/ADR-M005.md)) surfaces which class an offset edit is **before** applying.

## 7. Relationship to automatic wall-clock sync (ADR-0038)

[ADR-0038](../decisions/ADR-0038.md) provides **automatic** content-time alignment (detect a source
wall-clock, align multiple sources to a synced time-of-day instant via a per-program pre-decode
buffer). This brief's offset is the **manual operator trim**:

- When auto-sync is on, the manual offset is applied **relative to the aligned baseline** (auto aligns
  to `T = now − D`; the manual offset adds/subtracts on top).
- When auto-sync is off (free-run / reclock-to-house), the manual offset is relative to the program
  clock arrival.
- They share the **same** ENCODED-pre-decode delay machinery for large video offsets — the manual
  offset is, mechanically, an operator-set `D` (or a delta on the auto `D`). This is why the model
  reuses ADR-0038 rather than inventing a second video delay path. **`D` stays the shared
  program-alignment horizon, not a per-consumption value.** A small/medium per-consumer offset is a
  read-delta over the *existing* `D`-deep delay line (it must not move the whole program baseline);
  only when a single consumption asks for a large offset **deeper than the program's current `D`**
  does it either raise the shared `D` (if all consumers can ride it) or instantiate a **separate
  decode-behind** for that consumption (the second-decode case, §5, admission-gated). The dry-run
  plan (§6) states which of the two an edit triggers, so a negative-or-manual-on-top-of-auto edit
  never silently shifts the program for everyone.

## 8. Config / API / UI

- **Config (additive, `schema_version` unchanged).** A `Source` gains an optional
  `offset { audio_ms?, video_ms? }` (the universal baseline). Each consumption gains an optional
  `offset` override: a passthrough/transcode `Output`, a layout cell binding, and a switcher input/
  strip. `ms` is converted to exact frame/sample counts at the boundary (round-half-up, min 1 **sample
  for audio / 1 frame for video**, [ADR-T015](../decisions/ADR-T015.md)). Integer/`chrono` time only
  on the data plane (inv #3). **Compatibility rule** for keeping `schema_version`: all new fields are
  optional and **default to zero / inherit** (a config without them parses and behaves exactly as
  before — older configs are accepted unchanged); the change is forward-additive, so an older binary
  rejecting an unknown `offset` field on a newer config is acceptable (deny-unknown-fields) rather
  than silently ignoring it.
- **API.** `PATCH` the source offset and each consumption's offset under the existing resource routes
  (`/api/v1/sources/{id}`, the output/layout/switcher routes), Class-1 (or Reset-lite, surfaced via
  `X-Multiview-Apply` / the dry-run plan). Realtime: an `offset_changed` event on the relevant topic,
  conflated.
- **UI.** An **offset control wherever a source is consumed** (the ubiquitous affordance, cf.
  [webui-operability-gaps.md](webui-operability-gaps.md)): a universal control on the source, plus a
  per-output / per-layout cell / per-switcher-strip override with an "inherits universal" default and
  a clear "overriding universal" badge. An **A/V skew** shortcut on muxed feeds; an A/V-sync test
  source (flash+beep, [switcher-audio.md §16](switcher-audio.md)) for calibration.

## 9. Efficiency budget (standing review)

- Audio offsets are ~free (one atomic cursor each).
- Video offsets cost **buffer memory**: small offsets = a bounded decoded ring (modest, per-host
  budgeted); large offsets = the cheap ADR-0038 encoded delay. The expensive case — a single source
  needing **independent large** video offsets across multiple consumptions — is admission-gated
  ([ADR-E007](../decisions/ADR-E007.md)) and surfaced as a cost (it can force a second decode-behind),
  preserving decode-once-use-many wherever the offset is shared.
- Nothing here runs on the output-clock loop; all reads are consumer-side, bounded, drop-oldest
  (inv #1/#10).

## 10. Open questions (honest)

1. **Replace vs additive override.** Pinned: per-consumption override **replaces** the universal
   value. An additive `universal + per-consumption trim` mode is a recorded alternative if operators
   want a global shift plus local nudges — decide after MVP use.
2. **Small-video-ring bound.** The per-host depth that switches a video offset from decoded-ring to
   encoded-delay needs measurement on real hardware (4K decoded-frame memory is the constraint).
3. **Negative-offset cap UX.** How to surface the relative-advance ceiling (bounded by the *other*
   sources' buffers) without confusing operators who expect "−500 ms" to always work.
4. **Per-consumption independent large video offsets** — whether to allow the second-decode-behind
   cost at all on constrained hosts, or to refuse it via admission and require a shared offset.
5. **Auto-sync interaction precedence** — when [ADR-0038](../decisions/ADR-0038.md) auto-sync and a
   manual offset both move, the compose order is "manual on top of auto"; whether to also expose an
   "ignore auto, manual absolute" mode is a follow-on.
6. **Passthrough offset realization** — whether a non-zero offset on a true (never-decoded)
   passthrough output should force transcode-class (one mechanism, §3 default) or be realized as
   bounded packet-delay remux with PTS/DTS re-stamp + keyframe-aligned start; pin per output topology
   before implementation.

## 11. References

Project: [timing-architecture.md](timing-architecture.md), [input-timing-and-sync.md](input-timing-and-sync.md),
[wall-clock-sync.md](wall-clock-sync.md), [switcher-audio.md](switcher-audio.md),
[decoupled-routing.md](decoupled-routing.md), [multi-program.md](multi-program.md),
[ADR-T008](../decisions/ADR-T008.md), [ADR-T015](../decisions/ADR-T015.md),
[ADR-0038](../decisions/ADR-0038.md), [ADR-0059](../decisions/ADR-0059.md),
[ADR-0077](../decisions/ADR-0077.md), [ADR-0034](../decisions/ADR-0034.md),
[ADR-0030](../decisions/ADR-0030.md). Invariants #1/#2/#3/#5/#9/#10/#11 ([conventions §5](../architecture/conventions.md)).
