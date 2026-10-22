# Multiview — Current Output/Input Defects: Diagnosis & Verification Plan

**Area:** Triage (cross-cutting: output / input / audio / control / web)
**Status:** Development triage doc (Proposed) — **diagnosis only; not fixes.** No ADRs.
**Date:** 2026-06-13
**Source:** operator feature-request intake 2026-06-13
([feature-intake-2026-06-13.md](feature-intake-2026-06-13.md)).
**Governs nothing on its own** — each defect is mapped to the brief/ADR that
already owns its design; fixes ship under those, TDD-first, after hardware
verification.

> The operator reported seven concrete defects against a running build. This
> document is a **triage register**, not a fix. For each defect it states the
> symptom as reported, the ADR/brief that governs the relevant subsystem, a set
> of falsifiable hypotheses, a reproduction, and **exactly what must be observed
> on real hardware on the deploy FFmpeg build** before any code changes — then
> names the candidate fix area without writing code. Two repo lessons frame the
> whole exercise: **validate every ingest/output fix on the deploy FFmpeg version
> on real hardware** (an 8.x behaviour change already invalidated one shipped
> ingest fix), and **bad inputs are the purpose** — a stale tile, a decode error,
> or a reconnect is the *normal* operating condition for a live multiview and must
> never be dismissed as "a separate problem".

---

## 0. Purpose & method

This is **diagnosis, not a change**. The repo's standing rules apply with extra
force here because every one of these defects lives on the
ingest→decode→composite→encode→mux→serve path that the deploy FFmpeg version and
the GPU/hardware behaviour actually shape:

- **Verify, don't assume.** Nothing below is a confirmed root cause. Each defect
  carries hypotheses ranked by likelihood and a hardware observation that would
  confirm or refute each. A hypothesis is not promoted to "the bug" until the
  observation is recorded on the deploy build.
- **Deploy FFmpeg, real hardware.** ADR-T011's HLS-WebVTT fix was *ineffective on
  FFmpeg 8.x* because 8.x removed the `strict` gate the fix relied on — caught
  only on the hardware deploy build. Treat any libav-touching claim here as
  unverified until reproduced on the shipped image's FFmpeg, on the GPU box, not
  on a workstation or in CI's software-only path.
- **Bad inputs are the whole product.** Three of these defects (false stale,
  false no-audio, the bursting that surfaces after a stall) are *exactly* the
  failure-mode handling the product exists to do well. The triage must not
  "fix" them by discarding the awkward input; it must make the tile state
  machine, the audio store, and the output pacer ride the bad input correctly.
- **Invariants are not negotiable during diagnosis.** Every candidate fix below
  is constrained by **invariant #1 (the output clock is untouchable — no path may
  block or de-pace program output; inputs/cues/meters are *sampled*)** and
  **invariant #10 (control/preview/telemetry/logging never back-pressure the
  engine)**. A "fix" that pulls an input or a client onto the output-clock loop
  is wrong by construction and is rejected here before it is written.

**Priority three** (operator-impact first): **(P1) HLS program output bursting**,
**(P2) no audio on program output**, **(P3) false stale / no-signal with signal
present**. The other four are real but lower-blast-radius.

**What this doc does not do.** It writes no code, no schema, no tests. It does
not re-open the *designs* in the governing briefs/ADRs — those are correct on
paper; the question is whether the *as-built* path matches them, which only
hardware observation answers.

---

## 1. Defect register

Each subsection: **Symptom (as reported)** · **Governing ADR/brief** · **What the
code does today (cited)** · **Hypotheses** · **Repro** · **Verify on hardware** ·
**Candidate fix area (no code)**.

A note on citations: file:line references point at the **current** worktree and
were grepped, not assumed. They locate the seam; they are not a claim that the
seam is the bug.

---

### 1.1 (P1) HLS program output "bursting" — only HLS checked

**Symptom (as reported).** The HLS program output "bursts": a player on the HLS
output fast-forwards / plays faster than 1.0× / jumps to the live edge, and
downstream consumers receive a backlog in a rush rather than one segment per
segment-duration of wall-clock. The operator has only checked HLS; whether RTSP /
NDI / push exhibit it is unknown.

**Governing ADR/brief.** [ADR-T005](../decisions/ADR-T005.md) (wall-clock-paced,
GOP-aligned segments; custom origin for true LL-HLS),
[streaming-gotchas §4 "HLS / LL-HLS OUTPUT Pacing"](../research/streaming-gotchas.md),
and [hls-delivery.md](../research/hls-delivery.md) (the as-built delivery gap
analysis). ADR-T005 names this exactly: *"Output bursting is segments published
faster than realtime … catching up after a stall by flushing a backlog."*

**What the code does today (cited).** Per
[hls-delivery.md §2.2](../research/hls-delivery.md), the live HLS path is
**MPEG-TS, written once at finalize, non-atomically** — the playlist is written
by `std::fs::write` after the sink thread joins (`pipeline.rs:1795` per that
brief's audit), and segments accumulate monotonically with no rolling window.
PROGRAM-DATE-TIME *formatting* exists as a pure text/anchor seam
(`crates/multiview-output/src/hls/pdt.rs:1`, `:33`; outbound epoch anchor
`crates/multiview-output/src/epoch.rs:9`,
`crates/multiview-output/src/sink.rs:1400`), but the rolling-publication driver
that would emit one segment per wall-clock interval is the documented HLS-0
foundation gap, not yet built.

**Hypotheses (ranked).**

1. **Publication is not wall-clock-paced** — the segment sink writes/closes
   segments as fast as the encoder hands it frames, and on connect (or after any
   catch-up) several closed segments are advertised back-to-back, so the player
   sees the edge jump. This is the textbook bursting cause in
   [streaming-gotchas §4](../research/streaming-gotchas.md) and the most likely
   given the as-built "write once at finalize" path. The output clock paces the
   *encoder* (invariant #1), but if *publication/advertisement* of segments is
   not separately wall-clocked, a downstream re-read still bursts.
2. **A free-running consume/mux stage re-introduces bursting** — even with a
   wall-clocked encoder, any stage that drains the encoded packet queue without
   `sync=true`-equivalent pacing flushes a backlog (streaming-gotchas §4 calls
   this out: *"any free-running consume stage reintroduces bursting"*).
3. **Missing / mis-anchored EXT-X-PROGRAM-DATE-TIME or a stray long segment**
   inflating `EXT-X-TARGETDURATION` makes hls.js mis-estimate the live edge and
   chase it (secondary; PDT formatting exists but its *emission cadence* on the
   live rolling path is unverified).
4. **Player-side only** — hls.js `maxLiveSyncPlaybackRate` catch-up under-fires
   and never recovers post-stall drift (issues #4681/#6350, cited in ADR-T005).
   This is a *secondary trim*, never the primary cause; do not "fix" bursting by
   tuning the client.

**Repro.** On the deploy build, run a stable source into an HLS program output;
let it run, then induce a brief input stall (kill the source for ~5 s, restore
it). Capture the media playlist over time (`curl` the `.m3u8` on an interval) and
play it in Safari (native) and hls.js with timestamps. Bursting shows as the
segment list advancing by more than one segment per segment-duration of wall-clock,
and the player's `currentTime` advancing faster than wall-clock. The decisive
observable is **segment-file availability/publication cadence plus media-timestamp
progression**, not the media-sequence counter alone: a media-sequence jump is only
diagnostic on a **steady-state rolling live playlist** with a known segment duration
and no restart/discontinuity — at startup, during playlist-window pruning, on a
discontinuity recovery, or when the origin first advertises an existing live window,
the counter legitimately advances by more than one and would false-positive.

**Verify on hardware (deploy FFmpeg).**

- **Is publication wall-clocked?** Timestamp each new segment file's appearance
  on disk (`inotifywait` / `ls --full-time` polling) and assert the inter-arrival
  gap ≈ segment duration (±jitter), *not* a burst. This is the single decisive
  observation.
- **Re-read `curl -I` the playlist** and confirm `Cache-Control` /
  `EXT-X-TARGETDURATION` are sane and that PDT is present and monotonic→UTC
  (streaming-gotchas §4: computing PDT from summed EXTINF *drifts*). Do **not**
  assert nginx/playlist behaviour from memory
  ([hls-delivery.md §7 "Verify, don't assume"](../research/hls-delivery.md)).
- **Post-stall behaviour** is the load-bearing case (bad-inputs-are-the-purpose):
  after the induced stall, confirm the output **drops to live** rather than
  flushing the stalled-interval backlog (ADR-T005: *"after a stall, drop to live —
  never flush a backlog"*).
- Confirm whether RTSP / NDI / push show the same edge-jump, to scope the defect
  to HLS publication vs a shared upstream pacing fault.

**External corroboration (web-verified 2026-06-13).** As an implementation
principle backed by [ADR-T005](../decisions/ADR-T005.md): a player tracks the live
edge from media timestamps, target/part durations, and playlist-reload (blocking)
semantics under its own live-latency policy — not from receipt wall-clock alone.
The Multiview-relevant takeaway is that an origin which publishes/advertises closed
segments faster than realtime lets a client jump the edge, and the correct posture
after a stall is "drop to live, never flush the backlog"; that posture follows from
ADR-T005, with the exact RFC 8216 / Apple LL-HLS reload-and-part semantics cited
separately (hls.js live-edge issue #2371 illustrates edge-chasing on variable
networks). (See §4 Links.)

**Candidate fix area (no code).** The HLS-0/HLS-1 foundation in
[hls-delivery.md §6, §10](../research/hls-delivery.md): a **rolling-playlist
driver** that re-renders and atomically publishes (temp+fsync+rename) **one
segment per wall-clock interval**, drops to live after a stall, and emits PDT at
the right cadence. The output clock and encode-once fan-out are *not* touched
(invariant #1/#7 hold). This is `multiview-output` + `multiview-cli`, off the hot
path. **No LL-HLS origin is required to fix plain-HLS bursting** — that is the
separate Tier-1 work. Do not change the engine.

---

### 1.2 (P2) No audio on program output

**Symptom (as reported).** The program output carries no audio — the HLS (and
presumably other) program stream is video-only or silent.

**Governing ADR/brief.** [ADR-0059](../decisions/ADR-0059.md) (the switcher-audio
control seam — the authoritative as-built audio audit),
[ADR-R005](../decisions/ADR-R005.md) (discrete routing + program bus),
[ADR-M004](../decisions/ADR-M004.md) (Source owns attributes, Output owns the
mapping), [switcher-audio.md](../research/switcher-audio.md). Cross-cut to the
selectable-tracks defect (§1.6) and false-no-audio (§1.4).

**What the code does today (cited).** Program audio is **flag-gated and
config-silent by default**. Per [ADR-0059](../decisions/ADR-0059.md) (verified in
tree): the program-audio bus is built **only** when the run opts in via
`--program-audio` (`enable_program_audio` at
`crates/multiview-cli/src/pipeline.rs:1517`; the bus is built only inside the
opt-in branch around `pipeline.rs:2004`–`2019`; ignored without the `ffmpeg`
feature). Routes are pinned at **unity** gain and `Command::RouteAudio` is
**held** (`crates/multiview-cli/src/control.rs:933`–953 warns
*"route_audio held: the run has no per-source audio crosspoint yet"*). The config
`[audio]` block (`crates/multiview-config/src/audio.rs`) is **schema + REST only —
nothing in the pipeline reads it** (ADR-0059 table). So a config that *declares*
audio still produces silence unless the legacy CLI flag is set.

**Hypotheses (ranked).**

1. **Program audio was never enabled for the run** — the deploy invocation /
   config did not pass `--program-audio` (or the deploy image's preset omits it,
   exactly as the `nvidia` preset omitted the caption `overlay` feature — see the
   HLS-WebVTT lesson). Most likely: ADR-0059 §8 explicitly flags that program
   audio "exists only behind `multiview run --program-audio`" and proposes making
   it config-declared precisely because operators hit silence.
2. **The `ffmpeg`/AAC encoder feature is absent in the deploy build**, so the
   bus is silently skipped (program audio is "ignored without `ffmpeg`",
   `main.rs` gate per ADR-0059). A missing AAC encoder must surface as a typed
   capability error, not silence (this is the ADR-0036 silent-fallback lesson
   applied to audio).
3. **The bus is built but not muxed into the HLS output** — the AAC packets are
   produced but the HLS segment sink is opened video-only, or the audio PID/track
   is not added to the PMT. On the MPEG-TS path, AAC **encoder delay / priming**
   left unhandled can also yield *present-but-unplayable* (silent/desynced) audio
   that presents as "no audio"; the packed-audio first-sample-timestamp tag is a
   separate concern that only applies if the output uses packed/raw-AAC segments.
4. **Per-output audio mapping not consumed** — even with a bus, ADR-0059 notes
   per-output `OutputAudio` consumption is an open work-schedule item, so the
   output may not request the program bus.

**Repro.** Deploy build, a source with a known-good audio track, an HLS program
output. (a) Run *without* `--program-audio` and confirm silence. (b) Run *with*
`--program-audio` and re-check. (c) `ffprobe` the output `.m3u8`/segments for an
audio stream; if present, check it actually decodes to non-silence.

**Verify on hardware (deploy FFmpeg).**

- **Is the bus enabled at all?** Confirm whether the deploy invocation/preset
  passes `--program-audio` (or declares `[audio]`). If not, this is hypothesis 1
  and is a packaging/config defect, not an engine defect — the highest-value
  check, do it first.
- **Does an AAC encoder open?** Check the deploy FFmpeg has a usable AAC encoder
  and that the run did not silently skip audio for lack of one (capture the
  startup logs; per §2 those logs are currently not source/output-tagged, which
  itself impedes this check).
- **`ffprobe` the output**: does the HLS playlist advertise an audio rendition /
  the segments carry an audio stream? If yes-but-silent, suspect the
  priming/first-sample-timestamp muxing pitfall (web-verified below). If no audio
  stream at all, the bus is not reaching the muxer (hypothesis 3/4).
- **Distinguish "no track" from "silent track"** at the output the same way the
  input side must (§1.4): a present-but-silent AAC rendition is a *muxing/priming*
  problem; an absent rendition is a *wiring/enablement* problem. Do not conflate.

**External corroboration (web-verified 2026-06-13).** The relevant pitfall
depends on the segment format the output actually uses, which must be confirmed
first. **For MPEG-TS HLS segments** (the as-built live path per
[hls-delivery.md §2.2](../research/hls-delivery.md)), AAC is carried with its own
PID/PTS inside the transport stream, so the diagnosis is structural: verify the
audio PID is present in the PMT, the segments are muxed with the audio stream, and
audio PTS is continuous with video — the `com.apple.streaming.transportStreamTimestamp`
ID3 PRIV first-sample-timestamp requirement is a **packed-audio** (raw AAC / ID3)
concern, *not* a property of AAC-in-MPEG-TS. The pitfall that survives across both
formats is AAC **encoder delay / priming** (~2112 samples): if priming is not
declared/handled, the decoder mis-aligns and audio presents as silent or desynced.
Only if the output is later found to use packed-audio or fMP4/raw-AAC segments does
the `transportStreamTimestamp` first-sample-timestamp tag apply. (Apple TN2258;
FFmpeg-user priming thread — §4 Links.)

**Candidate fix area (no code).** The [ADR-0059](../decisions/ADR-0059.md) Lane-D
plan: (1) make program audio **config-declared** (ADR-0059 §8 — enabled by an
`[audio]` block, default-on under a `switcher` block), so silence is not the
default; (2) the pipeline **consumes the BUILT `AudioRouting` config**; (3)
per-output audio mapping reaches the muxer. The seam, gain/mute, and meters in
ADR-0059 are the larger program; **for this specific defect the enablement +
muxing-into-HLS path is the keystone.** All of it is consumer-thread /
config-side — the bus is owned by the bake consumer, never the output-clock loop
(invariant #1/#10 hold by ADR-0059's construction).

---

### 1.3 (P3) Inputs show stale / no-signal when the signal is clearly present

**Symptom (as reported).** A tile shows STALE (held last-good) or NO_SIGNAL
(slate) even though the source is plainly delivering frames.

**Governing ADR/brief.** [ADR-T002](../decisions/ADR-T002.md) (per-tile
hold-last-good + duplicate/drop, sample-on-tick),
[ADR-R001](../decisions/ADR-R001.md) (the failure-ladder state machine),
[resilience-and-av.md](../research/resilience-and-av.md),
[streaming-gotchas §1, §7](../research/streaming-gotchas.md). This is a
**bad-inputs-are-the-purpose** defect: the state machine's job is to ride exactly
this, so a *false* stale is a misclassification, not "a flaky source".

**What the code does today (cited).** The ladder is a **pure function of elapsed
time since the last fresh frame** against three thresholds —
`crates/multiview-framestore/src/state.rs:126` (`classify`) with defaults **hold
500 ms, stale 2 s, no-signal 10 s** (`state.rs:97`–108). The compositor samples
per output tick; freshness is judged by a **liveness** probe on max-seen DTS
(`crates/multiview-framestore/src/liveness.rs:12`, `:25` — *"max-seen DTS stops
advancing … a frozen encoder looping a stale DTS is detected"*). The pipeline
wires `TileThresholds` and `NoSignalPolicy::HoldForever`
(`crates/multiview-cli/src/pipeline.rs:92`, `:1206`, `:6859`).

**Hypotheses (ranked).**

1. **Freshness is judged on a timestamp that is not advancing even though pixels
   are** — the liveness check keys on max-seen DTS/PTS
   (`liveness.rs:25`). If the input's PTS/DTS is non-monotonic, wrapped (33-bit),
   reset on a discontinuity, or genpts-stalled, the *frame* is new but the
   *stamp* the freshness check reads is not, so `classify` sees a large "elapsed"
   and declares STALE→NO_SIGNAL while frames keep arriving. This is the
   most-likely cause and is squarely a **timestamp-normalization** issue
   (invariant #3; [streaming-gotchas §2](../research/streaming-gotchas.md): wrap /
   non-monotonic-DTS / discontinuity are explicitly listed footguns).
2. **The "last fresh frame" wall-clock stamp is taken on the wrong clock or not
   updated on the publish path** — if the framestore's freshness timestamp is
   set from input PTS rather than a monotonic arrival clock, an input whose PTS
   sits in the past (VOD-as-live, large jitter) reads as instantly stale even
   though it is delivering. `classify` clamps negative elapsed to Live
   (`state.rs:124`), but a *positive-but-wrong* elapsed is the failure mode.
3. **Thresholds too tight for a legitimately low-fps or bursty-but-live source** —
   a 1–5 fps camera or a source with > 500 ms inter-frame gaps trips `hold` and
   shows STALE between frames. The defaults assume broadcast cadence;
   ADR-T002 notes real fps must be detected from a rolling median of decoded PTS
   deltas, never `r_frame_rate`. If thresholds are not adapted to detected fps, a
   slow-but-present source false-positives.
4. **Reconnect/escalation state machine resetting freshness** — the ingest
   reconnect logic (`pipeline.rs:5593`+ backoff constants) could be racing the
   freshness stamp on a supervised reconnect that the source did not actually
   need.

**Repro.** Deploy build with a *known-good, continuously-delivering* source that
exhibits one of: (a) low fps (e.g. 1–5 fps), (b) non-monotonic or wrapping PTS
(a long-running TS or a source past a discontinuity), (c) VOD-as-live with
past-dated PTS. Watch the tile state. False-stale reproduces as the tile entering
STALE/NO_SIGNAL while `ffprobe -show_frames` on the same source confirms frames
are arriving in real time.

**Verify on hardware (deploy FFmpeg).**

- **Log the inputs to `classify`**: the per-source `elapsed` and the timestamp it
  was computed from, on each tick, for the affected tile. The decisive question:
  *is "elapsed since last fresh frame" computed from a monotonic arrival clock or
  from input PTS?* If frames are arriving but `elapsed` grows, hypothesis 1/2 is
  confirmed. (This check is itself blocked by the §2 logging gap — there is no
  per-source span tying the freshness numbers to the tile.)
- **Cross-check freshness vs liveness**: confirm whether STALE is driven by the
  framestore's frame-arrival stamp or by the `PacketLiveness` DTS-stall probe
  (`liveness.rs`) — they are different detectors and can disagree on a
  non-monotonic-DTS source.
- **Detected fps vs thresholds**: log the rolling-median fps (ADR-T002) and
  confirm `hold`/`stale` are wide enough for it. A 2 fps source with a 500 ms
  `hold` is *by construction* STALE half the time — that is a threshold-vs-fps
  bug, not a source bug.

**Candidate fix area (no code).** Most likely **`multiview-input` timestamp
normalization** (invariant #3: unwrap 33-bit, genpts fallback, monotonic guard,
rebase to one ns timeline) feeding a **monotonic arrival clock** into the
framestore freshness stamp — so "fresh" means "a frame arrived recently in
wall-clock", decoupled from input PTS pathology. Secondarily, **fps-adaptive
thresholds** in `multiview-framestore` / its wiring in `multiview-cli`
(thresholds derived from detected fps per ADR-T002 rather than fixed broadcast
defaults). No engine/output-clock change; the state machine stays a pure function
(invariant #1 untouched — a correctly-classified tile still rides last-good).

---

### 1.4 Inputs show no-audio when audio is present

**Symptom (as reported).** A tile/input is reported as having no audio (no meter,
"no audio" badge) when the source clearly carries audio.

**Governing ADR/brief.** [ADR-R005](../decisions/ADR-R005.md) (per-input decode →
resample → silence-fill → bus/discrete),
[ADR-R006](../decisions/ADR-R006.md) (read-only non-blocking R128 metering),
[ADR-0059](../decisions/ADR-0059.md) (meters are DSP-built but have **zero
emitters** today), [switcher-audio.md §15](../research/switcher-audio.md) (silence
detection is a *meter* feature). Cross-cut to the no-program-audio defect (§1.2).

**What the code does today (cited).** The `AudioStore` **silence-fills** any gap
or absent block so `read()` always returns exactly the requested frames
(`crates/multiview-audio/src/store.rs:13`–18, `:219`–239 — the `anullsrc`
silence-fill, load-bearing for resilience). Per
[ADR-0059](../decisions/ADR-0059.md): the metering DSP (`Ballistics`,
`LoudnessMeter`, the 30 Hz `Conflator`) is **built but idle — there is no
production publisher of `Event::AudioMeter`**, and the build-time-timeline meters
for live sources "read the −90 dB floor forever". So a live input's meter can read
silence even when audio is flowing.

**Hypotheses (ranked).**

1. **The meter is not wired, so "no audio" is actually "no meter data"** —
   ADR-0059 §6 is explicit: `Event::AudioMeter` has no emitter; the UI reads a
   permanent −90 dB / "no audio" because nothing publishes the live level. This
   is the most likely cause and is a *telemetry-wiring* defect, not an
   audio-presence defect.
2. **"No audio" conflates a silent track with an absent track** — the system
   judges audio-presence from a *level* (silence-fill makes a missing track read
   as digital silence, and a genuinely silent passage also reads silence). The
   −70 LUFS R128 gate (web-verified) excludes silence from *loudness
   measurement* but **does not** tell "silent track present" from "no track at
   all". Presence is a **structural** fact (was an audio stream discovered for
   this input?), not a loudness reading. If the badge is derived from level, a
   silent-but-present or low-level source false-reads "no audio".
3. **Audio stream discovered but not routed/decoded** — per
   [decoupled-routing.md §2](../research/decoupled-routing.md), the demuxer
   enumerates all streams but the input path historically consumed only the video
   stream and *discarded* audio rows. If the audio stream is never decoded into a
   store, there is genuinely no level — but the *cause* is dropped-at-ingest, not
   absent-at-source.
4. **Codec/layout the deploy build can't decode** — a source audio codec the
   deploy FFmpeg lacks → decode fails → silence-fill → "no audio". A decode error
   must be surfaced (bad-inputs-are-the-purpose), not silently silence-filled into
   a false "no audio".

**Repro.** Deploy build, a source with a known non-silent audio track. Observe the
input's audio meter / "no audio" indicator. Repeat with (a) a source whose audio
is genuinely silent, and (b) a source with *no* audio stream, to see whether the
UI distinguishes the three cases.

**Verify on hardware (deploy FFmpeg).**

- **Is a meter even published?** Confirm whether any `Event::AudioMeter` reaches
  the UI for a live input (ADR-0059 says no emitter exists). If not, this is
  hypothesis 1 and the "no audio" is a missing-telemetry artifact — verify before
  anything else.
- **Structural presence vs level**: check whether the source's audio stream was
  *discovered and decoded* (a demux-enumeration / store-exists fact) independently
  of its level. The correct "no audio" signal is "no audio stream discovered",
  not "level ≈ silence".
- **Three-way distinguish** at one input: non-silent / silent-but-present /
  absent must read as three distinct states. The −70 LUFS gate is for loudness
  integration, **not** presence detection — do not use a level threshold as the
  presence test (web-verified).
- **Decode-error surfacing**: confirm a failed audio decode is logged
  per-source (blocked today by §2) and not silently silence-filled into "no
  audio".

**Candidate fix area (no code).** Two distinct fixes: (a) **wire
`Event::AudioMeter`** (the ADR-0059 §6 meter-publisher glue — DSP exists, only the
emitter is missing; rides the conflated drop-oldest lane, invariant #10 holds);
(b) derive the **"audio present" badge from stream discovery**
(`multiview-input` / [decoupled-routing.md](../research/decoupled-routing.md)
`StreamInventory`) and add an explicit **silence-detection** meter state
([switcher-audio §15](../research/switcher-audio.md)) so silent-but-present is
distinct from absent. No hot-path change; metering is read-only and off-thread by
ADR-R006.

---

### 1.5 Tracks are a free-text list, not selectable

**Symptom (as reported).** In the UI, audio (and presumably subtitle/caption)
tracks appear as a free-text list rather than a typed, selectable control.

**Governing ADR/brief.**
[webui-operability-gaps.md](../research/webui-operability-gaps.md) (the operability
gap catalogue — primary), [decoupled-routing.md §3](../research/decoupled-routing.md)
(`StreamInventory` — the typed discovery surface), [ADR-M004](../decisions/ADR-M004.md)
(Source owns track attributes; the routing-matrix UI greys impossible cells),
[ADR-R005](../decisions/ADR-R005.md). This mirrors the **ADR-0036** precedent
exactly: a free-text **codec** field was replaced by a typed catalog +
capability-gated dropdown; tracks need the same treatment.

**What the code does today (cited).** The config has typed crosspoints —
`SubtitleCrosspoint` and the independently-keyed audio/subtitle routing maps
(`crates/multiview-config/src/routing.rs:191`–197, `:302`–304, with validation at
`:408`). But per [decoupled-routing.md §2](../research/decoupled-routing.md), the
demux **enumerates all streams** (`crates/multiview-ffmpeg/src/demux.rs:301`, lang
at `:318`) yet the input path **discards** the audio/subtitle rows and keeps only
the best video stream — so there is **no discovered track inventory** for the UI
to render as a typed list. With no typed inventory surfaced, the UI falls back to
free text.

**Hypotheses (ranked).**

1. **No typed track-discovery surface reaches the API/UI** — the demuxer's
   `streams()` inventory is computed then thrown away
   ([decoupled-routing.md §2](../research/decoupled-routing.md)), so the UI has
   nothing typed to offer and degrades to a free-text field. Most likely.
2. **Inventory exists internally but is not exposed read-only over the API** — a
   capabilities-style `GET` (mirroring ADR-0036's
   `/system/capabilities/codecs`) for per-input tracks is absent, so the SPA
   cannot build a `<TrackSelect>`.
3. **Purely a front-end gap** — the typed data is available but the SPA renders a
   `<TextField>` (exactly the ADR-0036 free-text-codec situation that was
   replaced with `<CodecSelect>`).

**Repro.** Deploy build, a multi-track source (e.g. an MPEG-TS with several audio
PIDs + a subtitle PID). In the UI, attempt to select an audio/subtitle track and
observe whether the control is a typed dropdown of *discovered* tracks (with
language/codec/label) or a free-text entry.

**Verify on hardware (deploy FFmpeg).**

- **Is per-input track discovery exposed?** Probe the API for a per-input track
  inventory; confirm whether discovered tracks (with the language tag/code as
  discovered — typically ISO 639-style from the container, normalized where
  possible — plus codec, channel layout, label) are available to the SPA. If absent,
  this is
  hypothesis 1/2 and is a discovery-surface gap, not a widget bug.
- **Multi-track ground truth**: `ffprobe` the source's actual track list on the
  deploy build, and compare against what the UI offers — the UI must offer
  exactly the discovered set, capability-greyed per [ADR-M004](../decisions/ADR-M004.md)
  (e.g. HLS = select-one, NDI = channel-map).

**Candidate fix area (no code).** The [decoupled-routing.md §3](../research/decoupled-routing.md)
`StreamInventory` model surfaced read-only over the API (the ADR-0036
capability-endpoint pattern), feeding a typed `<TrackSelect>` in the SPA
([webui-operability-gaps.md](../research/webui-operability-gaps.md)), with
impossible cells greyed by the [ADR-M004](../decisions/ADR-M004.md) capability
matrix. Pure config/control/web — no engine or hot-path involvement.

---

### 1.6 Layout editor cannot enable/disable subtitles

**Symptom (as reported).** The layout editor offers no control to turn subtitles
/ captions on or off for a tile (or for the program).

**Governing ADR/brief.**
[webui-operability-gaps.md](../research/webui-operability-gaps.md) (primary —
operability gap), [ADR-0019](../decisions/ADR-0019.md) (native caption ingest with
a per-source **selector** `auto | off | teletext_page N | track id | …`),
[ADR-R007](../decisions/ADR-R007.md) (burn-in vs passthrough),
[decoupled-routing.md](../research/decoupled-routing.md) (subtitle crosspoints are
independently keyed).

**What the code does today (cited).** [ADR-0019](../decisions/ADR-0019.md) defines
a per-source caption **selector** owned by the Source (`auto | off | teletext_page
N | track id | embedded_cc … | sidecar path`) and decode-only-when-shown. The
config models subtitle crosspoints (`crates/multiview-config/src/routing.rs:191`,
`:302`). A caption *ingest+burn-in* path is **reported to exist** per a prior fix
(the HLS-WebVTT fix under #47) — to verify against the deploy feature set before
relying on it (see the packaging caveat below). But there is **no layout-editor
control** binding the per-source `off`/`on` selector to a UI toggle — the selector
is config-level only.
(Memory note: captions are `overlay`-gated and the deploy `nvidia` preset omits
`overlay`, so even when toggled they may not render on the shipped image — a
**packaging** caveat to verify alongside the UI gap.)

**Hypotheses (ranked).**

1. **No UI binding for the existing selector** — the `off`/track selector
   ([ADR-0019](../decisions/ADR-0019.md)) is reachable in config but the layout
   editor exposes no toggle, so an operator cannot enable/disable subtitles from
   the editor. Most likely; a front-end + small control-surface gap.
2. **Captions feature not in the deploy build** — per the HLS-WebVTT memory, the
   `nvidia` preset omits `overlay`, so the burn-in path is absent regardless of
   any toggle. A UI toggle over an absent feature would mislead; verify the
   deploy build includes the caption path first.
3. **Subtitle enable/disable is a layout-vs-source ambiguity** — turning
   subtitles "off" could mean *don't ingest* (source selector `off`) or *don't
   render this layer* (subtitle crosspoint unbound). The editor needs to surface
   which, per the independently-keyed subtitle crosspoint model
   ([decoupled-routing.md](../research/decoupled-routing.md)).

**Repro.** Deploy build, a source with captions (TS teletext/608/708 or an HLS
WebVTT rendition). In the layout editor, attempt to enable/disable subtitles for a
tile. Observe whether any control exists and whether toggling it changes the
burned-in/overlaid captions.

**Verify on hardware (deploy FFmpeg).**

- **Does the deploy image even include the caption path?** Confirm the `overlay`
  (and `libass` where styled) feature is compiled into the shipped image — the
  HLS-WebVTT lesson is that the `nvidia` preset omitted it. If absent, the UI gap
  is moot until packaging is fixed.
- **Is the selector reachable live?** Confirm whether the per-source caption
  selector ([ADR-0019](../decisions/ADR-0019.md) `off`/track) is settable at
  runtime via the control plane, independently of the editor — that determines
  whether this is a pure SPA binding or also a control-surface gap.

**Candidate fix area (no code).** A layout-editor **subtitle enable/disable +
track-select control** ([webui-operability-gaps.md](../research/webui-operability-gaps.md))
bound to the [ADR-0019](../decisions/ADR-0019.md) per-source selector and the
[decoupled-routing.md](../research/decoupled-routing.md) subtitle crosspoint, with
the burn-in-vs-passthrough capability per [ADR-R007](../decisions/ADR-R007.md)
surfaced. **Packaging fix** (ensure the deploy preset ships the caption feature)
is a prerequisite to verify. SPA + control-surface + deploy; no hot-path change.

---

### 1.7 Logging is not source / output / layout specific

**Symptom (as reported).** Logs are not attributable — a libav error or a
state-machine transition cannot be tied to *which* source, output, or layout it
came from. (This is the cross-cutting gap; see §2.)

**Governing ADR/brief.**
[observability-logging.md](../research/observability-logging.md) (sibling brief,
this batch — the primary home for the design), and the as-built libav→tracing
bridge in `crates/multiview-ffmpeg/src/log_bridge.rs`. Treated in full in §2.

**Short version.** The libav log bridge routes every libav line into `tracing`
carrying only the libav **`component`** field (the `[hevc @ …]` class name —
`crates/multiview-ffmpeg/src/log_bridge.rs:16`, `:557`–563). It does **not** carry
which Multiview source/output/tile the line belongs to, because the callback runs
on a foreign decoder thread with no Multiview span context. So a corrupt-input
error is correctly rate-limited and routed (the anti-flood work is real and good)
but is **not attributable**. See §2 for the full diagnosis and candidate fix.

---

## 2. The cross-cutting logging gap

Every defect above hit the same wall during triage: **the logs cannot tell you
which source/output/layout an event belongs to**, so even *diagnosing* the other
six is harder than it should be. This is a first-class defect, not a convenience.

**What exists (cited, and it is genuinely good).** The libav→`tracing` bridge
(`crates/multiview-ffmpeg/src/log_bridge.rs`) does two correct things: it
**replaces libav's unbounded stderr writer** with structured `tracing` at mapped
levels, and it **rate-limits** repetitive lines (10 000 identical RPS errors
become one line plus a periodic count) — exactly the "manage the libav log so a
bad input never floods the operator" requirement from the bad-inputs principle.
The FFI is ABI-safe (the `va_list` is rendered in a C shim; the Rust callback runs
under `catch_unwind`, allocation-light, on the decoder thread).

**The gap.** The only structured field attached is the libav **`component`** (the
class name like `hevc`, read at `log_bridge.rs:489`–496, emitted at `:557`–563).
There is **no Multiview identity** — no `source_id`, `output_id`, `tile`, or
`layout` field — because:

- The libav callback fires on a **foreign/decoder thread** with no `tracing`
  span from the Multiview side in scope, so it cannot inherit a source span.
- A grep finds **no per-source/per-output `tracing` span** wrapping the ingest or
  egress work in `multiview-input` / `multiview-cli` that the callback (or the
  Rust-side log statements) would attach to. Source identity exists as plain
  `source_id` *strings threaded through function calls*
  (`crates/multiview-cli/src/pipeline.rs:2684`, `:2747`–2763) but **not as a
  span field** on the log records.

The result: an operator sees `[hevc] Error constructing the frame RPS.` with no
way to know which of N tiles it is, and a STALE transition (§1.3) or a decode
error (§1.4) cannot be correlated to a source — which is precisely why §1.3 and
§1.4's hardware checks above each say "blocked by the §2 logging gap".

**Hypotheses for the fix shape (deferred to the sibling brief).**

1. **Span-context propagation across the FFI boundary** — establish a per-source
   `tracing` span on the decoder thread (set once when the ingest worker for a
   source starts) so both Rust-side logs and the libav callback's emit inherit
   `source_id`/`tile`. The callback runs on the decoder thread, so a
   thread-local current-span is the natural carrier — but it must stay
   allocation-light and panic-safe (the existing FFI discipline) and must never
   add latency to decode.
2. **Carry an explicit identity field** on the libav `AVClass` opaque pointer or
   via a thread-local registry keyed by the decoder thread → `source_id`, so the
   `component_name` read can be augmented with the Multiview source.
3. **Structured spans at every stage seam** (ingest / decode / composite /
   encode / mux / serve) with stable `source_id` / `output_id` / `layout` fields,
   so *all* logs — not just libav's — are attributable. This is the
   [observability-logging.md](../research/observability-logging.md) brief's
   remit.

**Invariant posture.** Logging is **aux** and must obey **invariant #10**: it
**never back-pressures the engine**. The bridge already drops/rate-limits and
holds its suppressor mutex only for an O(small) lookup, never across the emit. Any
attribution fix must preserve this — a per-source span is a thread-local /
field-set operation, not a channel the engine can block on; structured-log export
(if any) must be a bounded drop-oldest sink, never a synchronous write on the hot
path. **Invariant #1** is untouched: logging is off the output-clock loop.

**Candidate fix area (no code).** Owned by
[observability-logging.md](../research/observability-logging.md): per-source /
per-output / per-layout `tracing` spans threaded through the stage seams, with the
libav bridge augmented to attach the current decoder thread's source identity. The
anti-flood bridge stays; it gains an identity field. `multiview-ffmpeg`
(bridge) + `multiview-input` / `multiview-cli` (span establishment) +
`multiview-telemetry` (export). No hot-path latency, no engine channel.

---

## 3. Verification checklist

Run on the **deploy FFmpeg build, on the GPU/hardware box**, recording each
observation. A defect is not "understood" until its decisive observation is
captured. (Order roughly by priority.)

- [ ] **(P1 bursting)** Timestamp segment-file appearances on disk; assert
  inter-arrival ≈ segment duration, not a burst (this is the primary observable).
  `curl` the `.m3u8` over time; on a **steady-state rolling playlist** (no
  restart/discontinuity/pruning) confirm media-sequence advances ≤ one segment per
  wall-clock interval. Induce a 5 s stall; confirm output **drops to live**, does
  not flush the backlog. Cross-check whether RTSP/NDI/push burst too.
- [ ] **(P1)** `curl -I` the playlist/segments; verify Content-Type /
  Cache-Control / Accept-Ranges from the *running* stack (do not assert from
  memory). Confirm PDT present, monotonic→UTC, not summed-EXTINF-derived.
- [ ] **(P2 no audio)** Confirm whether the deploy invocation/preset enables
  program audio (`--program-audio` or an `[audio]` block). This is the
  first-and-highest-value check.
- [ ] **(P2)** Confirm the deploy FFmpeg opens an AAC encoder; capture startup
  logs. `ffprobe` the output for an audio stream; if present-but-silent on the
  MPEG-TS path, check the audio PID/PMT, PTS continuity, and encoder priming; if
  absent, suspect bus-not-muxed. Distinguish the two. (Only revisit the packed-audio
  first-sample-timestamp tag if the output is found to use packed/raw-AAC segments.)
- [ ] **(P3 false stale)** Log per-source `elapsed` and the clock it is computed
  from; `ffprobe -show_frames` the same source in parallel. Confirm whether
  "elapsed" is on a monotonic arrival clock or input PTS. Log rolling-median fps;
  confirm `hold`/`stale` thresholds are wider than the inter-frame gap.
- [ ] **(no-audio input)** Confirm whether any `Event::AudioMeter` reaches the UI
  for a live input. Verify "audio present" is derived from **stream discovery**,
  not from level. Distinguish non-silent / silent-but-present / absent as three
  states.
- [ ] **(tracks)** Probe the API for per-input track discovery; compare against
  `ffprobe` ground truth; confirm the UI offers the discovered set,
  capability-greyed.
- [ ] **(subtitles)** Confirm the deploy image compiles the `overlay`/`libass`
  caption path (the `nvidia`-preset-omits-`overlay` caveat). Confirm the
  per-source caption selector is settable at runtime.
- [ ] **(logging)** Confirm whether *any* log record carries
  `source_id`/`output_id`/`layout`. Reproduce a corrupt-input flood and confirm
  the rate-limiter holds *and* that the line is now attributable (post-fix
  acceptance).
- [ ] **(every libav-touching check)** Record the exact deploy FFmpeg version and
  build flags; re-run on the hardware box, never only in CI's software path
  (the ADR-T011/FFmpeg-8.x lesson).

---

## 4. Links

**Governing ADRs.**
[ADR-T005](../decisions/ADR-T005.md) (HLS pacing / bursting) ·
[ADR-T002](../decisions/ADR-T002.md) (per-tile hold/dup/drop) ·
[ADR-R001](../decisions/ADR-R001.md) (failure-ladder state machine) ·
[ADR-R005](../decisions/ADR-R005.md) (audio routing + program bus) ·
[ADR-R006](../decisions/ADR-R006.md) (R128 metering, read-only) ·
[ADR-0059](../decisions/ADR-0059.md) (switcher audio — as-built audio audit) ·
[ADR-M004](../decisions/ADR-M004.md) (audio track-mapping ownership) ·
[ADR-0036](../decisions/ADR-0036.md) (typed catalog replacing a free-text field —
the precedent for §1.5) ·
[ADR-R007](../decisions/ADR-R007.md) (subtitle burn-in/passthrough) ·
[ADR-0019](../decisions/ADR-0019.md) (native caption ingest + per-source selector).

**Briefs.**
[hls-delivery.md](../research/hls-delivery.md) ·
[streaming-gotchas.md](../research/streaming-gotchas.md) ·
[resilience-and-av.md](../research/resilience-and-av.md) ·
[decoupled-routing.md](../research/decoupled-routing.md) ·
[switcher-audio.md](../research/switcher-audio.md) ·
[observability-logging.md](../research/observability-logging.md) (sibling, this
batch) ·
[webui-operability-gaps.md](../research/webui-operability-gaps.md) (sibling, this
batch) ·
[feature-intake-2026-06-13.md](feature-intake-2026-06-13.md) (this batch's intake).

**External standards (verified 2026-06-13 unless marked unverified).**

- LL-HLS / HLS output pacing — live-edge tracking is driven by media timestamps,
  `EXT-X-TARGETDURATION` / part duration, and playlist-reload (blocking) semantics
  under the player's live-latency policy (RFC 8216 + Apple LL-HLS), **not** by
  receipt wall-clock alone; an origin that advertises closed segments faster than
  realtime lets a client jump the edge, and variable networks fall behind when
  playback stalls to load fragments. AWS LL-HLS workflow guide; hls.js live-edge
  issue #2371; OvenMediaEngine LL-HLS. (Supports §1.1 as an implementation
  principle per ADR-T005: "drop to live, never flush backlog".)
- AAC-in-HLS muxing — for **MPEG-TS** segments (the as-built path) AAC rides its
  own PID/PTS, so verification is structural (PMT/PID present, audio muxed,
  PTS-continuous); the first-sample-timestamp tag
  (Apple `com.apple.streaming.transportStreamTimestamp` PRIV/ID3) applies to
  **packed-audio / raw-AAC** segments only, not AAC-in-MPEG-TS. AAC **encoder
  delay/priming** (~2112 samples) must be handled in either case or the decoder
  mis-aligns. Apple TN2258 "AAC Audio — Encoder Delay and Synchronization";
  FFmpeg-user priming thread. (Confirms §1.2 present-but-unplayable audio path.)
- Input-loss ladder — repeat-last-good frame → black → slate, configurable
  per-stage milliseconds. AWS MediaLive "Handling loss of video input". (Confirms
  the §1.3 ladder shape; ADR-R001 mirrors it.)
- EBU R128 gating — the **−70 LUFS absolute gate excludes silence from loudness
  measurement** and does **not** distinguish a silent-but-present track from an
  absent track; presence is structural, not a level reading. EBU Tech 3341/3342;
  Audacity R128 manual. (Confirms §1.4: don't use a level threshold as the
  presence test.)
</content>
</invoke>
