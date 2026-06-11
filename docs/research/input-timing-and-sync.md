> **Design brief — Streaming/Timing (Input-side timing & frame-sync).** Authoritative research/design record backing the implementation. Produced by a verification-hardened research workflow (2026-06-03). Canonical crate/API naming lives in [docs/architecture](../architecture/). Decisions derived from this brief: [ADR-0021](../decisions/ADR-0021.md); cross-references [ADR-T001](../decisions/ADR-T001.md), [ADR-T002](../decisions/ADR-T002.md), [ADR-T003](../decisions/ADR-T003.md), [ADR-T004](../decisions/ADR-T004.md), [ADR-T006](../decisions/ADR-T006.md), [ADR-0020](../decisions/ADR-0020.md). This brief **deepens** (does not duplicate) the input-side timing material in [timing-architecture.md](timing-architecture.md) §4 (Layer C) and [streaming-gotchas.md](streaming-gotchas.md) §0–§2.

---

# Input Timing & Frame-Sync: best-effort PTS normalisation + wall-clock pacer + sample-at-tick

**Audience:** engineers building `multiview-input` (the PTS normaliser, jitter buffer, and pacer), `multiview-ffmpeg` (`StreamVideoDecoder`, `best_effort_timestamp`/genpts), `multiview-framestore` (last-good store + tile state machine), and `multiview-cli`/`multiview-engine` (the ingest loop that wires them). This is the single place that answers, for the **input side**: *given a frame fresh out of `avcodec_receive_frame`, what is its true presentation time, how do we pace it to wall-clock, and which buffered frame does the output sample at tick `N`?*

**Scope.** The OUTPUT side is solid and out of scope: a fixed-cadence monotonic clock emits one frame per tick, `out_pts = f(tick)`, never paced by an input (invariant #1; [ADR-T001](../decisions/ADR-T001.md)). This brief is **everything upstream of the per-tile store**: per-frame timestamp acquisition → normalise → pace → publish; and the **sampling rule** the output applies at each tick (Layer C of [timing-architecture.md](timing-architecture.md); invariant #2). It is the failure-mode and design detail behind the [streaming-gotchas.md](streaming-gotchas.md) §0 three-stage pipeline.

---

## 0. The bug this brief exists to kill (and the root cause)

**Observed symptom.** A file/VOD source tile (e.g. `/tmp/bbb_clip.mp4`, a synthetic mpeg2 capture, an HLS VOD-as-live) plays **ultra-fast** — racing ~10 s of content into ~1 s of output — and then **freezes**.

**Root cause (from reading the running pipeline, 2026-06-03).** Multiview has **two divergent input-PTS paths**, and the bulletproof one is **not wired into the running ingest loop**:

- **(a) The correct, documented path — currently UNUSED by file/VOD ingest.** `multiview-input/src/normalize.rs::PtsNormalizer` (delta-unwrap → genpts-from-declared-cadence → discontinuity re-anchor → monotonic guard → rebase to a master-anchored ns timeline) plus `multiview-input/src/pacer.rs::Pacer` (clock-injected, anchor-first-PTS-to-now, bounded `5/4` = 1.25× catch-up). These match [ADR-T003](../decisions/ADR-T003.md)/[ADR-T004](../decisions/ADR-T004.md) and are unit/property testable without sleeping.
- **(b) The actual ingest loop — what runs today.** `multiview-cli/src/pipeline.rs::open_and_stream` feeds `decoded.meta.pts` (from `StreamVideoDecoder`) into an ad-hoc `PtsWallClock` that anchors `base_instant`/`base_pts` on the **first decoded frame** and sleeps until `now - base_instant >= pts - base_pts`.

The failure is a property of path (b) the moment the per-frame PTS stream has *any* defect:

1. If `best_effort_timestamp` intermittently returns `AV_NOPTS_VALUE` (observed empirically on `net29_97.ts` past the first several frames — `best_effort_timestamp=N/A` while `pts` is fine), `StreamVideoDecoder::next_pts` falls back to a **hard-coded** `FALLBACK_STEP_NS = 33_366_667` (≈29.97 fps). For a 25 fps PAL mpeg2 capture (40 ms true period) or a 24 fps clip (41.7 ms) that synthesises the **wrong cadence** — but worse, when the *first* frames have good PTS and a run of frames then collapses to `0`/near-constant, the per-frame `delta ≈ 0` and **every `wait_for` returns immediately** → the source RACES.
2. When real PTS resumes (or after the genpts tail re-aligns), the pacer computes a **far-future** release deadline relative to the stale anchor → the tile **FREEZES** until wall-clock catches up.

This is the classic *ultra-fast-then-freeze*: a pacer whose per-frame deltas briefly collapse to zero, then jump. **The fix is structural, not a tweak:** route file/VOD (and every live input) through the existing `PtsNormalizer` + `Pacer`, retire the ad-hoc `PtsWallClock`, and pass the stream's real `r_frame_rate` so the genpts fallback uses the *measured/declared* cadence (per [ADR-T003](../decisions/ADR-T003.md)), not a 29.97 constant. This brief specifies that pipeline precisely.

---

## 1. Layer C-in: per-frame timestamp acquisition (the only timestamp worth trusting)

### 1.1 Use `best_effort_timestamp`, not raw `pts`, never `dts`

A bare decoded `frame->pts` is frequently `AV_NOPTS_VALUE` or unreliable after decode (mpeg2/H.264 with B-frames); `frame->pkt_dts` is *decode* order, not display order. The field engineered for this is `best_effort_timestamp`.

> **VERIFIED** (FFmpeg `master` `libavcodec/decode.c`, fetched 2026-06-03): every decoded frame is stamped `frame->best_effort_timestamp = guess_correct_pts(dc, frame->pts, frame->pkt_dts);`. The function's own comment: *"Attempt to guess proper monotonic timestamps for decoded video frames which might have incorrect times. Input timestamps may wrap around, in which case the output will as well."* It keeps running counters, incrementing `dc->pts_correction_num_faulty_dts += dts <= dc->pts_correction_last_dts;` and `dc->pts_correction_num_faulty_pts += reordered_pts <= dc->pts_correction_last_pts;`, then selects: *if faulty-pts count ≤ faulty-dts count (or `dts` is `AV_NOPTS_VALUE`) and `reordered_pts` is valid → use `reordered_pts`; otherwise use `dts`.*

> **VERIFIED** (FFmpeg `AVFrame` doxygen, ffmpeg.org/doxygen/trunk, fetched 2026-06-03): `best_effort_timestamp` = *"frame timestamp estimated using various heuristics, in stream time base"*; `pts` = *"Presentation timestamp in time_base units (time when frame should be shown to user)"*; `pkt_dts` = *"DTS copied from the AVPacket that triggered returning this frame…"*.

So `best_effort_timestamp` is the reordered container PTS when PTS looks at least as reliable as DTS, else DTS — exactly the field a pacer must read. In `ffmpeg-next`, `Frame::timestamp()` returns `best_effort_timestamp` (mapping `AV_NOPTS_VALUE → None`). Multiview's `StreamVideoDecoder` line 132 already does `decoded.timestamp().or_else(|| decoded.pts())` — that **ordering is correct**; keep it.

### 1.2 `avcodec_receive_frame` returns frames in presentation (display) order

> **VERIFIED EMPIRICALLY** (`ffprobe`/`showinfo` on `/tmp/refsrc`, 2026-06-03): the decoder buffers and reorders B-frames internally; frames emerge from `receive_frame` in strictly-increasing PTS order. `bcast25.ts` (mpeg2, `has_b_frames=1`): PTS `174045, 177645, 181245, 184845, 188445…` (+3600 ticks @ 1/90000 = 40 ms = 25 fps). `net29_97.ts` (h264, `has_b_frames=2`): `132006, 135009, 138012, 141015…` (+3003 ticks = 1/29.97 s). So received PTS is monotonic and can be scheduled directly; only the **output encoder** needs DTS, which libavcodec assigns.

> **CAVEAT (do not over-trust):** this is *empirical/practical* behaviour, **not** an explicit `avcodec_receive_frame` API guarantee — the doxygen says only *"Return decoded output data from a decoder."* Therefore the `PtsNormalizer` **monotonic guard must stay** as belt-and-braces; do not remove it on the assumption input is always monotonic.

### 1.3 Genpts fallback — from *measured/declared* cadence, not a constant

When `best_effort_timestamp` (and `pts`) are both `AV_NOPTS_VALUE`, synthesise a timestamp by advancing the previous emitted instant by **one true frame period**. `PtsNormalizer` derives `frame_period_ns` from the per-input declared `cadence` rational (`Rational::new(cadence.den, cadence.num)` → rescale to ns) and is correct. The defect is in `StreamVideoDecoder::next_pts`, which uses a **hard-coded `FALLBACK_STEP_NS = 33_366_667`** (≈29.97 fps) for *any* stream — wrong by ~11 % for 25 fps PAL, ~25 % for 24 fps. **Resolution:** the normaliser, not the decoder, owns the genpts fallback; pass it the stream's real `r_frame_rate`. (Caveat per [streaming-gotchas §2](streaming-gotchas.md): FFmpeg `-fflags +genpts` only fills PTS *when DTS exists* — we own this ourselves and do not rely on the flag.)

### 1.4 33-bit / 32-bit wrap unwrap — delta-based, never the libav heuristic

`2^33 / 90000 = 26 h 30 m 43.7 s` (MPEG-TS PTS); `2^32 / 90000 ≈ 13.25 h` (RTP). Unwrap **delta-based** into a 64-bit counter: if `(cur − last) < −2^(bits−1)` add `2^bits`. Do **not** trust libavformat `pts_wrap_reference`/`correct_ts_overflow` — its RTP heuristic fired a **false** rollover at ~13h14m on a bogus SDP `rtptime` (MediaMTX #622, per [streaming-gotchas §2](streaming-gotchas.md)/[ADR-T003](../decisions/ADR-T003.md)). `PtsNormalizer::unwrap` (`WrapBits::Mpeg33` modulus `1<<33`, half `1<<32`; `WrapBits::Rtp32` symmetric) implements exactly this. **Must test past the wrap boundary** with synthetic timestamps — a 24/7 service that ran fine for an hour fails overnight.

### 1.5 The mpegts initial-PTS offset is real, large, and harmless once anchored

> **VERIFIED EMPIRICALLY** (`ffprobe` on `/tmp/refsrc`, 2026-06-03): `bcast25.ts` video `start_pts=174045` (`start_time≈1.9338 s`); `net29_97.ts` `start_pts=132006` (`≈1.4667 s`); `net_tos.ts` `≈1.4417 s`. This is the well-known mpegts muxer demux/decode-delay offset (the PES write doubles `max_delay`, per [streaming-gotchas §2](streaming-gotchas.md)). **Key point:** this is an **absolute epoch offset** — the per-frame *deltas* are preserved — so it is fully absorbed by anchoring `offset = master_now − first_pts`. It is **not** the ultra-fast bug; do **not** try to strip it specially.

### 1.6 Exact-rational rescale to ns

Carry internal time as i64 ns via `multiview_core::time::rescale` (exact rational, never float fps — float drifts ~3.6 s/hour for 29.97). TS time_base is exactly `1/90000`; NTSC rates are exact rationals (`30000/1001`, `60000/1001`; `r_frame_rate=30000/1001` confirmed on `net29_97.ts`). For sentinel-bearing libav values use the equivalent of `AV_ROUND_NEAR_INF|AV_ROUND_PASS_MINMAX` semantics so `INT64_MIN/MAX` pass through unchanged.

**The acquisition chain (per input, per frame), as it must be wired:**
```text
best_effort_timestamp  ──(or pts; else None)──►  PtsNormalizer.normalize(raw, master_now_ns)
   raw present:  unwrap(delta, WrapBits) → rescale(ticks, in_tb, 1/1e9) = raw_ns
   raw == None:  raw_ns = last_raw_ns + frame_period_ns        // genpts, MEASURED cadence
   first frame:  offset = master_now − raw_ns;  media = master_now
   |jump|>10s OR pending discontinuity:  re-anchor (continue = last_media + frame_period)
   else:         media = raw_ns + offset
   monotonic guard:  if media ≤ last_media → media = last_media + 1
   ⇒ MediaTime (strictly increasing ns on the internal timeline)
```

---

## 2. The pacer (Layer C ingest pacing — anchor-first-PTS-to-now, release-by-PTS)

The pacer answers *when* a normalised frame is released into the per-tile store, so that on connect/reconnect a backlog of already-published segments (HLS) or a fast file read does not flood the tile (invariant #4). It is **not** the output clock — it gates the *input* side; the output still samples at its own monotonic tick.

### 2.1 The rule (`multiview-input/src/pacer.rs::Pacer`, already correct)

```text
on first frame (or re-anchor): anchor = (now_ns, pts_ns);  Release::Now
else:                          deadline = anchor_wall + (pts − pts0)
                               now ≥ deadline ? Release::Now : Release::At(deadline)
```

- **Clock-injected.** `submit(pts, now_ns)` is a pure function of `(pts, now)`; pacing decisions are deterministically testable without sleeping. This is what makes the test matrix in §6 flake-free.
- **Re-anchor on discontinuity.** A flagged `EXT-X-DISCONTINUITY` (`mark_discontinuity`) or an inferred `|pts − last| > discontinuity_ns` (~10 s) re-anchors `(anchor_wall, pts0)` to the new frame → `Release::Now`, so the timeline continues forward instead of scheduling a far-future release (the freeze half of the bug).
- **Bounded catch-up, never a seek.** `release_deadline_catchup` shrinks the wall-clock interval by the exact-rational rate `num/den` (default `5/4` = 1.25×) to recover *small* drift; it never advances instantly. Hard re-sync is reserved for true discontinuities.
- **Bounded buffer, drop-oldest.** The pacer sits behind `jitter.rs::ReorderBuffer` (PTS-ordered, capacity-bounded, evicts smallest-PTS at capacity, drops items at/below the release watermark). Connect-burst overflow is *absorbed*, never grows memory, never back-pressures (invariant #2/#10).

### 2.2 File vs live

| | **File / VOD-as-live** | **Live (RTSP/SRT/RTMP/TS/NDI)** |
|---|---|---|
| Pacing | **Mandatory** — a file reads as fast as disk allows; without the pacer it floods (the bug). Anchor first PTS to now, release by PTS. | Pacing is light (the network already paces) but the **same** pacer is used so reconnect bursts and HLS segment backlogs are smoothed. |
| `-re` | **Never.** See below. | **Never.** |
| Catch-up | After a stall, bounded 1.25× toward target pre-roll. | Bounded 1.25×; on a true timeline break, re-anchor (drop-to-live), never flush a backlog at full speed. |

> **VERIFIED** (FFmpeg `master` `doc/ffmpeg.texi`, fetched 2026-06-03): `-re (input)` *"Read input at native frame rate. This is equivalent to setting `-readrate 1`."* `-readrate` is *"Mainly used to simulate a capture device or live input stream (e.g. when reading from a file)"* and *"Should not be used with a low value when input is an actual capture device or live stream as it may cause packet loss."* This is a **CLI-only fftools** behaviour and the wrong tool regardless: Multiview owns its own libav-linked PTS→wall-clock pacer (invariant #4; [ADR-T004](../decisions/ADR-T004.md)). After a stall, `-re`'s wall-anchored budget refills via an **unthrottled burst** — the opposite of a fix.

### 2.3 Why the pacer's anchor must use *normalised* PTS

The ad-hoc `PtsWallClock` anchors on raw `decoded.meta.pts` and re-anchors only on `pts < base_pts`. That misses the two real failure modes: (i) a *run of equal/near-zero* PTS (genpts collapse) — deltas ≈ 0, no backward step, so it never re-anchors but releases everything instantly; (ii) a *large forward* jump that is a genuine discontinuity (treated as a far-future release → freeze). Feeding the pacer the **monotonic, re-anchored** output of `PtsNormalizer` removes both: equal PTS becomes `last+1` (a 1 ns step, still effectively immediate but harmless), and large jumps are re-anchored *before* the pacer ever sees them.

---

## 3. The frame-sync rule (which buffered frame the output samples at tick `N`)

The output clock owns Layer A; the per-tile store + sample-at-tick is the **frame synchroniser** (Layer C of [timing-architecture.md](timing-architecture.md)). Each normalised+paced frame lands in the per-tile store stamped with its `media_time`. At each output tick `N` (target `t_N = N · den/num` ns on the internal timeline):

```text
f = frame_store[i].nearest_at_or_before(t_N)   // pick the frame this tick should show
if f is None: f = frame_store[i].last_good()    // HOLD on starvation — NEVER block
composite(f)
```

This single rule is mathematically equivalent to per-tile nearest/previous-PTS resampling with implicit dup-on-stall and drop-on-overrun, at zero motion-interpolation cost. It delivers, for free:

- **Drop / repeat (off-speed sources).** A source faster than the output cadence has multiple frames between ticks → only the nearest-at-or-before survives (drop, newest-wins). A slower source has no new frame for several ticks → `last_good()` repeats. A 50/60 fps source on a 25/30 output halves naturally; a 24 fps source on 25 produces an occasional repeat (a slightly juddery, **stall-free** tile — do **not** inverse-telecine in the live path).
- **Off-output-fps & 1001 family.** Because `t_N` is computed from the exact rational cadence, a 29.97 source on a 30 output (or vice-versa) drifts by exactly one repeated/dropped frame every ~33 s — absorbed silently. Never compare or pace with float fps.
- **A/V alignment.** Audio is carried through the *same* normaliser rebasing as its video and mixed on the master running-time, keeping skew inside the EBU R37 window (−60/+40 ms; bias audio slightly late). Separate RTP A/V sessions are reconciled via RTCP SR then rebased ([streaming-gotchas §7](streaming-gotchas.md); [ADR-T008](../decisions/ADR-T008.md)).
- **Drift absorption (long-run).** Independent source crystals drift tens–hundreds of ppm; nearest-at-or-before sampling *is* continuous drop/repeat for video. Audio uses continuous soft resampling by measured ppm ([ADR-T006](../decisions/ADR-T006.md)) — never audible drop/dup.
- **Burst / stall.** A bursting source overwrites the slot (newest wins, bounded memory); a stalled source holds last-good and rides the tile state machine LIVE→STALE→RECONNECTING→NO_SIGNAL (invariant #2; [ADR-T002](../decisions/ADR-T002.md)).

**The critical separation the bug violated:** the pacer (Layer C ingest) decides *when a frame enters the store*; the output tick decides *which stored frame is shown*. The output never speeds up or freezes because of an input — only the **tile content** changes. The ultra-fast-then-freeze symptom was an *ingest pacing* failure (frames entered the store too fast, then not at all) masquerading as an output problem.

---

## 4. Composition with the invariants and ADR-0020

| Invariant / ADR | How the input-side design honours it |
|---|---|
| **#1 output-clock** ([ADR-T001](../decisions/ADR-T001.md)) | The pacer gates *ingest*, never the output tick. The output samples last-good-or-placeholder; no input PTS ever paces emission. |
| **#2 last-good + state machine** ([ADR-T002](../decisions/ADR-T002.md)) | `nearest_at_or_before` then `last_good()`; lock-free single-slot store; tile freshness ladder driven by paced arrival. |
| **#3 unified timing** ([ADR-T003](../decisions/ADR-T003.md)) | `PtsNormalizer`: best-effort → unwrap → genpts(measured cadence) → re-anchor → monotonic guard → rebase; i64 ns / exact rationals, never float fps. |
| **#4 HLS ingest pacing** ([ADR-T004](../decisions/ADR-T004.md)) | `Pacer`: anchor-first-PTS-to-now, release-by-PTS, bounded 1.25× catch-up, re-anchor on discontinuity; never `-re`. |
| **#10 isolation** | Bounded drop-oldest `ReorderBuffer`; the engine never awaits an input; a flooding source cannot back-pressure. |
| **ADR-0020 Layer C** | This brief is the input-half detail of Layer C (per-input frame-sync). Untrusted per-source PTS is the default; a verifiably ST 2110/PTP-locked source maps onto the common timeline for true cross-source phase, but is still *sampled, never pacing*. |

---

## 5. Prior art (cited)

- **GStreamer `GstAggregator`/`videoaggregator`/`compositor` live mode** is the direct model for sample-at-tick: in live mixing it *"waits on the clock and if a pad does not have a buffer in time it ignores that pad"* and can *"produce output for the current time, no matter if there is enough input or not"* — i.e. deadline-driven aggregation that holds/skips a pad rather than blocking. (GStreamer docs/devel, accessed 2026-06-03.) Companion: `min-upstream-latency`, `ignore-inactive-pads`, leaky `queue2`.
- **FFmpeg `best_effort_timestamp` / `guess_correct_pts`** (FFmpeg `master` `libavcodec/decode.c`; `AVFrame` doxygen, accessed 2026-06-03) — the verified basis for §1.1.
- **FFmpeg `-re`/`-readrate`** (FFmpeg `master` `doc/ffmpeg.texi`, accessed 2026-06-03) — the verified basis for §2.2 (files-only, never live).
- **OBS render loop** — fixed canvas FPS, GPU-composite the latest source frame, drop over-rate / duplicate under-rate ("frames missed due to rendering lag") — the same per-tile dup/drop policy ([streaming-gotchas §1](streaming-gotchas.md)).
- **SMPTE / broadcast frame synchroniser** — a buffer ≥1 frame that drop/repeats whole frames to align an asynchronous input to a reference-locked output; software analog is NDI Frame Synchronization (drop/insert video, dynamic-resample audio, return last-good when none arrived) ([timing-architecture.md §4.1](timing-architecture.md)).
- **MediaMTX #622** — the libavformat RTP wrap heuristic firing a false rollover (basis for §1.4's "own the unwrap"; [streaming-gotchas §2](streaming-gotchas.md)).

---

## 6. Deterministic test matrix (adversarial input timing × expected sampling behaviour)

**Test philosophy — no wall-clock flake.** Two layers, both deterministic:

1. **Normaliser/pacer unit + property tests (no sleeping).** `PtsNormalizer::normalize(raw, master_now_ns)` and `Pacer::submit(pts, now_ns)` are pure functions of injected timestamps + an injected clock. A test feeds a **scripted raw-PTS sequence** and an **injected `now_ns` schedule**, and asserts the exact emitted `MediaTime` sequence and the exact `Release::Now` / `Release::At(deadline)` decisions against a **golden release schedule**. No real time elapses; the assertion is on values, not durations.
2. **Frame-sync golden table (no sleeping).** Drive the store + `nearest_at_or_before(t_N)` over a scripted set of stored `(media_time, frame_id)` and a scripted tick sequence `t_0..t_K`; assert the exact `frame_id` (or `last_good`) composited per tick. CPU-only, bit-exact (golden-frame), per the CI tiers.

To *generate* real inputs for end-to-end/soak validation, the lavfi recipes below produce each pattern; the deterministic assertion still runs on the injected-clock layer (the real files are for the GPU-tier SSIM/PSNR + soak gates, not the flake-free unit gate).

| # | Pattern | Generate (ffmpeg lavfi / refsrc) | Injected-clock assertion (golden) |
|---|---|---|---|
| 1 | **CFR baseline 30** | `ffmpeg -f lavfi -i testsrc2=r=30:d=10 cfr30.mp4` | Normaliser: emitted ns = `anchor + n·33_366_667` (±0 with exact rational). Pacer: `Release::At(anchor_wall + n·period)`. Frame-sync at output 30: tick `N` → frame `N` (1:1). |
| 2 | **CFR 25 (PAL)** | `ffmpeg -f lavfi -i testsrc2=r=25:d=10 cfr25.mp4` / `bcast25.ts` (synthetic mpeg2) | Genpts/period derived from **25** (40 ms), not 29.97. At output 25: 1:1. At output 30: assert the golden drop/repeat pattern (3 new : 2 repeat over 5 ticks ≈ 25/30). |
| 3 | **CFR 24** | `ffmpeg -f lavfi -i testsrc2=r=24:d=10 cfr24.mp4` / `/tmp/bbb_clip.mp4` | Period 41.7 ms. At output 25: one repeat every ~24 ticks. **Regression guard for the 29.97-constant bug:** asserting period≠33.37 ms fails the old `FALLBACK_STEP_NS`. |
| 4 | **VFR (variable)** | `ffmpeg -f lavfi -i testsrc2=r=30 -vf 'random=frames=8' …` or mix `setpts` segments | Pacer releases each frame at `anchor_wall+(pts−pts0)`; assert monotonic non-decreasing releases; frame-sync picks nearest-at-or-before — no race, no freeze across the rate change. |
| 5 | **B-frames / non-monotonic received** | `net29_97.ts` (h264 `has_b_frames=2`), `bcast25.ts` (mpeg2) | Feed the **received** (display-order) PTS; assert strictly increasing emitted ns; assert no reliance on `pkt_dts`. Negative control: feed a deliberately non-monotonic sequence → monotonic guard emits `last+1`, never a backward step. |
| 6 | **No-PTS (all `AV_NOPTS`)** | `ffmpeg -f lavfi -i mandelbrot=r=25 -fflags -genpts …` (strip PTS) | Genpts fallback advances by the **declared 25 fps** period; assert emitted ns = `anchor + n·40ms`; pacer paces at 25, no instant flood. |
| 7 | **mpegts ~1.44 s start offset** | `net29_97.ts` / `net_tos.ts` (start_pts 132006 / start_time ≈1.4417 s) | First frame anchors to `master_now` regardless of the 1.44 s offset; assert subsequent deltas equal the source deltas (offset absorbed); **assert it does NOT cause a 1.44 s freeze or fast-forward.** |
| 8 | **Mid-stream discontinuity** | concat two clips with different epochs: `ffmpeg -i a.ts -i b.ts -filter_complex concat=n=2:v=1 …` or HLS w/ `EXT-X-DISCONTINUITY` | `mark_discontinuity()` (or inferred `|jump|>10s`) → normaliser continues at `last_media+period`; pacer re-anchors `(anchor_wall, pts0)`; assert NO far-future release (no freeze) and NO backlog flush (no race). |
| 9 | **PTS gap (stall then resume)** | `ffmpeg -f lavfi -i testsrc2=r=30 -vf 'select=not(between(n,90,150))' …` (drop 2 s) | Within the gap: frame-sync returns `last_good()` (held tile), output unaffected. On resume (gap < 10 s): pacer schedules normally; (gap > 10 s): re-anchor. Assert exact held-frame ids during the gap. |
| 10 | **33-bit MPEG-TS wrap boundary** | synthesise raw PTS crossing `2^33` (unit test array; no encoder needed) | `WrapBits::Mpeg33` delta-unwrap: assert continuous 64-bit ns across the wrap (no backward jump, no false rollover). **Soak corollary:** a long synthetic run that crosses the boundary must not glitch. |
| 11 | **32-bit RTP wrap boundary** | synthesise raw RTP timestamps crossing `2^32` (unit test array) | `WrapBits::Rtp32` symmetric unwrap; **negative control:** a bogus single huge value must NOT trigger a false rollover (the MediaMTX #622 class of bug). |
| 12 | **Off-output-fps grid 24/25/29.97/30/50/60** | `testsrc2=r={24,25,30000/1001,30,50,60000/1001}` and `net29_97.ts`(29.97), `net30.ts`(30), `net_tos`(24) | For each source-fps × output-cadence pair, assert the exact golden drop/repeat ratio over a full LCM period (e.g. 60→30 = drop every other; 24→30 = repeat 1 in 4; 29.97→30 = one repeat per ~1001 ticks). All computed via exact rationals; assert no float drift over a 1 h simulated tick count. |
| 13 | **Live HLS connect burst** | a live/VOD-as-live playlist with ≥3 segments pre-published (a local packager / `http://127.0.0.1:8888/test/index.m3u8`) | Pacer absorbs the multi-segment backlog into the bounded buffer (overflow → drop-oldest, `ReorderBuffer`); assert releases are wall-clock-paced (not back-to-back) and memory stays bounded. |

**The single regression test that would have caught the bug:** pattern #6 (no-PTS at 25 fps) + #3 (24 fps) through the *real wiring*: feed a sequence whose `best_effort_timestamp` is good for frames 0..9 then `None` for 10..30, with an injected `now_ns` that advances at 25 fps. The correct path emits a smooth 40 ms cadence and the pacer issues evenly-spaced releases. The buggy `PtsWallClock` + 29.97-constant path emits a run of immediate releases (race) followed by a far-future deadline (freeze). Assert the emitted release schedule equals the golden 40 ms grid — a pure-value assertion, zero wall-clock flake.

---

## 7. Cross-references, gaps, unverified items

**Deepens / does not contradict:** [timing-architecture.md §4](timing-architecture.md) (Layer C) and [streaming-gotchas.md §0–§2](streaming-gotchas.md) (three-stage pipeline, wrap/discontinuity/B-frame rules). It is the input-half implementation detail behind both. **ADRs:** new [ADR-0021](../decisions/ADR-0021.md) records the input-timing decision; builds on [ADR-T001](../decisions/ADR-T001.md)/[ADR-T002](../decisions/ADR-T002.md)/[ADR-T003](../decisions/ADR-T003.md)/[ADR-T004](../decisions/ADR-T004.md)/[ADR-T006](../decisions/ADR-T006.md) and the Layer-C framing of [ADR-0020](../decisions/ADR-0020.md).

**Confirmed (current, dated, authoritative):** `best_effort_timestamp = guess_correct_pts(...)` and the faulty-pts/dts selection (FFmpeg `master` `decode.c`, 2026-06-03); `AVFrame` field semantics (FFmpeg doxygen, 2026-06-03); `-re`=`-readrate 1`, files-only, packet-loss warning (FFmpeg `master` `doc/ffmpeg.texi`, 2026-06-03); GStreamer live-aggregator waits-on-clock / ignores-late-pad (GStreamer docs/devel, 2026-06-03); the empirical `net29_97.ts` PTS stream, mpegts start offsets, and `r_frame_rate=30000/1001` (`ffprobe` on `/tmp/refsrc`, 2026-06-03).

**Unverified / to validate (do not assert as fixed):**
- That `avcodec_receive_frame` *always* returns display-ordered/monotonic frames is **empirical, not an API guarantee** — keep the monotonic guard. *(unverified as a contract.)*
- The exact intermittent `best_effort_timestamp=N/A` pattern on `net29_97.ts` past frame ~10 was observed in one capture; the design is robust to it regardless, but the *frequency*/cause across builds is **unverified**.
- The default `discontinuity_ns` (~10 s) and catch-up rate (`5/4`) are heuristics; tune against soak data, not asserted as universal constants.
- End-to-end no-race/no-freeze under real GPU + real network is a **GPU-tier + soak** assertion (SSIM/PSNR + ≥72 h zero-gap), separate from the flake-free unit gate above.

---

## Sources

- FFmpeg `master` `libavcodec/decode.c` (`guess_correct_pts`, `best_effort_timestamp`) — https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/decode.c (accessed 2026-06-03)
- FFmpeg `AVFrame` doxygen (`best_effort_timestamp`, `pts`, `pkt_dts`) — https://ffmpeg.org/doxygen/trunk/structAVFrame.html (accessed 2026-06-03)
- FFmpeg `master` `doc/ffmpeg.texi` (`-re`, `-readrate`) — https://github.com/FFmpeg/FFmpeg/blob/master/doc/ffmpeg.texi (accessed 2026-06-03)
- GStreamer `GstAggregator` documentation — https://gstreamer.freedesktop.org/documentation/base/gstaggregator.html (accessed 2026-06-03)
- GStreamer `GstVideoAggregator` documentation — https://gstreamer.freedesktop.org/documentation/video/gstvideoaggregator.html (accessed 2026-06-03)
- GStreamer `gst-plugins-base/gst-libs/gst/video/gstvideoaggregator.c` — https://github.com/GStreamer/gst-plugins-base/blob/master/gst-libs/gst/video/gstvideoaggregator.c (accessed 2026-06-03)
- MediaMTX issue #622 (false RTP wrap rollover) — referenced via [streaming-gotchas.md §2](streaming-gotchas.md)
- IETF RFC 8216 / draft-pantos-hls-rfc8216bis-17 (HLS, `EXT-X-DISCONTINUITY`) — https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis-17
