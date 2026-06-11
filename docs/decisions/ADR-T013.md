# ADR-T013 — The shared RTP-audio → AudioStore program-clock rebase seam

**Status:** Proposed (2026-06-10)
**Area:** streaming/timing (T) — audio ingest timing
**Drivers:** more than one RTP-audio ingest is landing (WebRTC Opus under IN-6, AES67 / ST 2110-30
PCM under AES67-1, and any future RTP-audio source); without one pinned rebase contract each would
invent a divergent path from a sample-rate-keyed RTP timestamp to the audio store, and they would
drift apart in subtle, soak-only-visible ways (wrap, anchor, gap placement).
**Related:** [ADR-T003](ADR-T003.md) (per-input PTS normalization — the *video/general* seam this
extends for audio), [ADR-T001](ADR-T001.md) (single internal monotonic timeline + output clock),
[ADR-T006](ADR-T006.md) (long-run drift: resample, never drop/dup), [ADR-T008](ADR-T008.md) (A/V
sync & jitter), [ADR-T010](ADR-T010.md) (AES67/ST 2110-30 is the open Dante interop path),
[ADR-0033](ADR-0033.md) (AES67 send+receive *implementation* — the first concrete consumer of this
seam), [ADR-R005](ADR-R005.md) (audio mix/route, the program bus, the canonical 48 kHz/f32 format).

## Context

Multiview now has, or is landing, more than one ingest whose audio arrives as **RTP packets with a
sample-rate-keyed media-clock timestamp**:

- **WebRTC contribution (IN-6).** `multiview-input/src/webrtc/transport.rs` depacketizes the
  negotiated **video** PT into keyframe-gated access units. Its `FrameProducer::next_frame` loop
  (today around line 618) explicitly **drops every non-video payload type** —
  *"Only the negotiated video PT is depacketized for now; other PTs (audio) are sampled and dropped
  here (the seam for an audio rebaser)."* The audio PT (Opus, RTP clock 48 000 Hz) has nowhere to go.
- **AES67 / SMPTE ST 2110-30 (AES67-1, [ADR-0033](ADR-0033.md)).** `multiview-input/src/st2110/v30.rs`
  is a **pure** L16/L24 PCM depacketizer: `V30Payload::parse` validates whole sample groups and
  exposes per-channel `i32` samples, but does no timing — its RTP timestamp advances by **sample
  groups at the stream's audio rate** (e.g. +48 per 1 ms packet at 48 kHz), not the video 90 kHz.
  ADR-0033 §3 names a future `Aes67AudioProducer` that would turn `V30Payload` → `AudioBlock` → an
  `AudioStore::publish_at(frame)`, "interpret the media-clock timestamp at **48 kHz**, not the video
  90 kHz", and honour the `a=mediaclk:direct` offset.
- **Future RTP audio** (any further contribution/transport that hands us RTP audio).

These all converge on the same destination: a per-source **[`AudioStore`]**
(`multiview-audio/src/store.rs`). The store is **frame-indexed** — it carries an absolute frame
counter (`base_frame` / `read_frame`), is gap-free by construction (silence-fills any unwritten or
evicted span via `read`), is lock-free, and already exposes the re-point primitives `seek_to(frame)`
/ `seek_to_live_edge()`. From there `ProgramBus::tick` samples exactly `samples_per_tick` frames per
output tick. The store's working format is the canonical **48 kHz / f32 / interleaved** program
format ([ADR-R005](ADR-R005.md)).

The existing timing seam is **[ADR-T003](ADR-T003.md)**'s `PtsNormalizer` (`normalize.rs`): it
unwraps a 32-bit RTP timestamp (`WrapBits::Rtp32`) or a 33-bit MPEG-TS timestamp, applies a genpts
fallback, re-anchors on discontinuity, enforces a strict monotonic guard, and rebases onto the
internal **nanosecond** timeline as a `MediaTime`. But it is built for the **video tile** path: it
emits a `MediaTime` (an instant), it assumes one timestamp per *frame*, its genpts fallback is keyed
on a video *cadence* (fps), and it hard-codes a single per-input timebase. Audio is different in
three load-bearing ways:

1. **The RTP clock rate is the audio sample rate** (Opus 48 000, AES67 L24 48 000/96 000), not 90 kHz
   — and for AES67 the on-wire rate may differ from the canonical 48 kHz store rate.
2. **The store wants an absolute frame index**, not a `MediaTime` instant — gap/reorder tolerance and
   silence-fill are expressed in *frames*, and the program bus pulls *frames*.
3. **One RTP packet carries many frames** (a 1 ms AES67 packet = 48 frames; an Opus packet = 480 at
   10 ms), so the rebase maps a *packet's start* to a frame index and the payload's own sample count
   advances from there — there is no per-frame timestamp to normalize.

There is no ADR that pins **the single seam** all three ingests must share. ADR-0033 sketches the
AES67 half in passing (`publish_at`, 48 kHz), and the WebRTC code leaves a TODO comment. If WebRTC,
AES67, and the next source each grow their own "RTP timestamp → store frame" glue, they will diverge
on wrap handling, anchoring, the discontinuity rule, and silence-fill placement — exactly the class
of bug ADR-T003 exists to prevent for video, and exactly the kind that only shows up in a 24/7 soak
(a wrap at ~24.8 h for a 48 kHz 32-bit RTP clock; an SSRC/`mediaclk` re-anchor; a packet-reorder
window). This ADR pins that seam **once**.

## Decision

Pin **one** rebase seam — *"depacketized RTP audio → absolute `AudioStore` frame index on the unified
ns timeline"* — that **every** RTP-audio ingest (WebRTC Opus, AES67 PCM, future) routes through. The
contract has five stages; they are the audio analogue of ADR-T003 and must not be re-implemented
per-source.

### 1. Depacketize is per-codec; rebase is shared

Each ingest owns only its **codec-specific** depacketize step, producing a small typed value:

```
(rtp_timestamp: u32, sample_rate_hz: u32, ssrc: u32, marker: bool, frames: AudioBlock-or-PCM)
```

- WebRTC: the Opus RTP payload → decoded PCM at the negotiated 48 kHz clock (decode is the engine
  driver's job, behind the `webrtc` feature, exactly as the H.264/video PT is handled today).
- AES67: `V30Payload` (L16/L24) → i32 → f32 (`/32768` for L16, `/8388608` for L24), the 48 kHz
  media clock, per ADR-0033 §3.

Everything **after** that typed value is the **shared** seam. The depacketizer never invents a
store frame index, never touches the ns timeline, and never calls `AudioStore` directly.

### 2. The RTP clock is the *audio sample rate* — keyed per stream, never assumed 90 kHz

The rebaser is constructed with the stream's **declared RTP clock rate** (from the SDP `a=rtpmap`
clock, e.g. `opus/48000/2`, `L24/48000/8`). This is the single most important divergence from the
video path: the video `WrapBits::Rtp32` rebase assumes 90 kHz; an audio rebaser that copied it
would mis-scale every timestamp. The seam therefore **takes `sample_rate_hz` as a constructor
parameter** and uses it as both the unwrap timebase (`1 / sample_rate_hz` seconds per tick) and the
frame rate for the frame-index mapping.

### 3. Unwrap + anchor reuse ADR-T003's algorithm, expressed in frames

The 32-bit RTP timestamp is unwrapped with **the same delta-based accumulator** ADR-T003 uses
(`WrapBits::Rtp32`; a negative delta beyond half-modulus is a forward wrap) — *not* a value compare
and *not* libavformat's `pts_wrap_reference` heuristic (which has misfired in production; T003 §
Rationale). The unwrapped 64-bit tick count is mapped to an **absolute store frame index** at the
canonical store rate:

```
store_frame = anchor_frame + rescale(unwrapped_rtp - anchor_rtp, 1/sample_rate_hz, 1/store_rate_hz)
```

using exact integer/rational math (`multiview_core::time::rescale`, i64 ns / i128 intermediates) —
**never float**, never a per-sample accumulator that drifts (inv #3; T003). When the wire rate equals
the store rate (the AES67 Class-A and Opus common case, both 48 kHz) the rescale is identity. The
**anchor** (`anchor_frame`, `anchor_rtp`) is set on the first packet of a stream (or SSRC), so the
first received audio lands at the output clock's "now" exactly as the video path anchors the first
frame — the store then carries absolute frames and `ProgramBus` A/V-aligns via the shared timeline
(ADR-T008). AES67's `a=mediaclk:direct=<offset>` is subtracted from the RTP timestamp **before**
unwrap (ADR-0033 §3), so the offset never leaks into the frame mapping.

### 4. Discontinuity / re-anchor / loss is the store's job, expressed as a seek

A timeline break — a new **SSRC**, a `mediaclk` change, or an unwrapped-tick jump beyond the
ADR-T003 threshold (~the audio equivalent of 10 s) — **re-anchors** rather than propagating a
multi-hour skip, identical in spirit to T003's discontinuity re-anchor. The rebaser surfaces the
break; the **store** absorbs it: because `AudioStore::read` is **gap-free by construction**
(silence-fills any unwritten/evicted span) the seam never has to manufacture a fill block, and a
re-anchor is a `seek_to`/`seek_to_live_edge` on the absolute cursor, not a buffer flush. **Reorder**
within a bounded window is handled by absolute-frame placement (`publish_at(frame)` — net-new in
ADR-0033, the absolute-index peer of today's append-only `publish`): a late packet writes to its
true frame index; a packet older than the surviving window is dropped (drop-oldest, never grows;
inv #2/#5). A packet beyond the bounded reorder window is dropped, **never** waited on.

### 5. Isolation + drift are unchanged (inv #1/#10, ADR-T006)

The rebaser runs on the **ingest/decode side**, never the output clock; the store is the wait-free
hand-off; `ProgramBus` samples on the tick. The rebaser **never paces** and **never back-pressures**
— a stalled or dead RTP audio source simply stops publishing and the store rides silence-fill
(inv #1/#10). Long-run wire-vs-house drift is the **existing** ADR-T006 mechanism (PTP-/measured-ppm
soft fractional resample, "resample never drop/dup"); the rebase seam produces the absolute frame
index that resampler reconciles, and adds no second drift policy.

### Where the seam lives

The shared rebaser is an **`multiview-input`** type (peer of `normalize::PtsNormalizer`), pure and
unit/property-testable in the default build with no native deps — call it the *RTP-audio rebaser*.
The `webrtc` and `st2110` producers depend on it; they supply only the per-codec depacketize and the
declared clock rate. `multiview-audio::AudioStore` gains the absolute-index `publish_at(frame)` write
(ADR-0033 already commits to it) as the single store entry point for rebased audio. No new crate.

## Consequences

- **One audited path.** WebRTC Opus, AES67 PCM, and any future RTP-audio source share the same wrap,
  anchor, discontinuity, reorder, and silence-fill behaviour — so a soak finding (a wrap at ~24.8 h,
  an SSRC re-anchor, a reorder edge) is fixed **once** and is property-tested once, not re-found per
  transport.
- **The WebRTC "video PT only" TODO becomes a wiring task, not a design task.** The comment at
  `transport.rs` line ~618 is satisfied by routing the audio PT into this seam (under IN-6b, with the
  str0m driver + Opus decode); no new timing model is invented at that site.
- **AES67 (ADR-0033) is the conformance vehicle, not a separate model.** ADR-0033's `Aes67AudioProducer`
  + `publish_at` + 48 kHz interpretation are this seam's first concrete consumer; ADR-0033 keeps the
  *implementation* detail (SDP, SAP, PTP, packetizer), this ADR owns the *shared rebase contract*.
- **Sample-rate is explicit, never assumed.** The 90 kHz video assumption cannot leak into audio: the
  rebaser is keyed on the SDP-declared audio rate, and rescales to the canonical 48 kHz store rate
  with exact integer math (identity when equal).
- **No regression to inv #1/#10 or T003.** The seam reuses T003's unwrap/anchor/monotonic discipline,
  runs off the hot path, and leans on the store's existing gap-free/lock-free guarantees, so it adds
  no new way to stall or back-pressure the engine.

## Alternatives rejected

- **Let each ingest carry its own RTP-timestamp→frame glue** (the status quo: a TODO in WebRTC, a
  prose sketch in ADR-0033). This is exactly the divergence ADR-T003 was written to prevent for
  video; for audio it would split wrap/anchor/reorder behaviour across transports and surface only in
  soak. Rejected — pin one seam.
- **Reuse `PtsNormalizer` unchanged for audio.** It emits a `MediaTime` instant keyed on a video
  *cadence*, assumes one timestamp per frame, and hard-codes a single timebase; the audio store wants
  an absolute *frame index*, one timestamp per *packet* (many frames), and a stream-keyed sample
  rate. Sharing the **algorithm** (delta-unwrap, anchor, discontinuity, monotonic) is right; sharing
  the **video-shaped output type** is wrong. Rejected in favour of an audio-shaped seam that reuses
  the algorithm.
- **Assume 90 kHz / a fixed rate.** Copying the video `WrapBits::Rtp32` path would mis-scale every
  audio timestamp (Opus/AES67 ride 48 kHz). Rejected — key on the SDP clock rate.
- **Rebase to a `MediaTime` and let the store convert.** Pushes the rate-keyed frame math into the
  store and re-introduces a float/round seam per consumer. The store is already frame-indexed and
  gap-free; rebase straight to its native coordinate. Rejected.
- **Map RTP time to wall-clock per packet and place by arrival.** That is the *degraded* (Freerun)
  fallback ADR-0033 §8 already defines for reference loss, not the primary path; making it primary
  imports jitter into the timeline. Rejected as the default; retained only as graceful degradation.
