# Per-source wall-clock + multi-source time-of-day sync

> **Status:** design brief (verification-hardened). **Decision:** [ADR-0038](../decisions/ADR-0038.md).
> **Builds ON (reference, not duplicated):** [ADR-T003](../decisions/ADR-T003.md) unified timing
> (per-input PTS unwrap + monotonic-guard + rebase-to-ns; output re-stamp from the tick; exact
> rationals, never float fps); the wall-clock **reference** (`multiview-engine/src/sysref.rs`
> `classify_system` + `multiview-engine/src/ptp.rs` `PtpServo`/`ReferenceTracker` lock-state machine);
> the **framestore** sample-by-media-time (`multiview-framestore/src/tile.rs` `read_at` latch-on-tick
> + LIVE→STALE→RECONNECTING→NO_SIGNAL ladder); the **input pacer/jitter**
> (`multiview-input/src/{pacer,jitter,normalize}.rs`); [ADR-0032](../decisions/ADR-0032.md) (HLS
> serving + `utc0@tick0` PROGRAM-DATE-TIME anchor) and the PTP-is-reference-not-pacer principle
> ([timing-architecture](timing-architecture.md) §6, [ADR-0020]; note repo **ADR-0033** is AES67
> audio I/O, *not* the PTP-reference ADR — cite the brief/ADR-0020 for that principle).
> Nothing below re-derives those — it composes them.
>
> **Invariants touched:** #1 (output-clock, never stalls), #2 (last-good ladder), #3 (unified
> timing, exact rationals), #5/#7 (decode-once), #10 (isolation), #11 (live-apply class).
> The greenfield surface is confirmed: `rg` finds **no** `WallClockRef`/`WallClockTier`/`sync_mode`/
> `SyncMode`/output-PDT in `crates/`.

This feature gives each input an **honest, runtime-measured wall-clock trust tier**, lets the
operator **Use** or **Discard** that wall-clock per source, and adds a per-program **time-of-day
sync mode** that aligns multiple cameras to a common reference instant — *without ever stalling the
output clock* and *without a second decoded frame copy*.

---

## 0. Wall-clock standards table (what to detect, per transport)

Each transport carries (or omits) a different standard mapping the source's media PTS to an absolute
wall-clock. The detector extracts a per-source **affine map** `wall(pts) = wall_anchor + rescale(pts − media_anchor, rate)`
from whatever is present. Ranked by **trust strength** (strongest first):

| Standard | Transport | What it gives | Trust ceiling | As-built in repo |
|---|---|---|---|---|
| **PTP / IEEE 1588 / SMPTE ST 2059** | ST 2110 / SDI facility | Grandmaster timeline; RTP 90 kHz rides it → true media→wall map + phase | **Trusted** (strongest; phase-accurate) | servo built — `ptp.rs` `PtpServo`/`ReferenceTracker` |
| **RP188 / ATC** | ST 2110-40 ANC | Ancillary SMPTE timecode (a *label*, rides PTP for phase) | Trusted *only* joined to PTP; else **Suspected** | ANC depacketized (`st2110/v40.rs` DID/SDID) — decode-to-`Timecode` **missing** |
| **RTCP Sender Report** | RTP / RTSP / WebRTC | NTP-1900 ↔ RTP pair = per-source **capture**→wall map (RFC 3550 §6.4.1) | **Trusted** with a fresh estimate | **missing** — `webrtc/transport.rs` surfaces only the bare 32-bit RTP timestamp |
| **EXT-X-PROGRAM-DATE-TIME** | HLS | PDT(segment) → first-sample PTS = UTC map (RFC 8216 §4.3.2.6) | **Trusted** (ms-class, NTP-disciplined origin) | **missing** — `hls.rs` is a *master-playlist-only* parser (no media-playlist / PDT scan) |
| **DASH `availabilityStartTime` + UTCTiming** | MPEG-DASH | Manifest UTC anchor + segment template | Trusted | **missing** (analogue of PDT) |
| **TEMI** | MPEG-TS | Adaptation-field affine timeline + NTP/PTP (ISO/IEC 13818-1) | **Trusted** | **missing** entirely |
| **DVB TDT/TOT** | MPEG-TS | Network **service-of-day** UTC, **no PTS bind** | **None** (coarse house hint only) | parsed — `mpegts/tdt.rs` `DvbTime::to_unix_seconds`, `tot.rs` |
| **SMPTE ST 12-1 SEI** | H.264 pic_timing / H.265 time_code | Per-frame HH:MM:SS:FF, **no inherent UTC date/anchor** | **Suspected at best** (a label, not a wall-clock) | **missing** — mirror the `A53CC` side-data hook (`multiview-ffmpeg/src/caption_decode.rs`) with `S12M_TIMECODE` |
| **NTP** | host clock | Loose discipline for the host reference | (reference only, not a per-source map) | `sysref.rs` `classify_system` over `adjtimex` |
| **(none)** | plain RTSP w/o SR, HLS w/o PDT, SRT, RTMP, file, synthetic Bars/Solid/Clock | nothing | **None** | reclock-to-house only |

**Many real sources carry NONE.** A plain RTSP without SR, an HLS without PDT, an SRT/RTMP/file pull,
and every synthetic source (`SourceKind::Bars`/`Solid`/`Clock`) have no wall-clock → they are
**reclock-to-house only** ([timing-architecture](timing-architecture.md):150 "when in doubt, treat as
untrusted").

---

## 1. R1 — per-source DETECT → TRUST tier → USE | DISCARD → RECLOCK-TO-HOUSE

### Detection is runtime, never authored

Per source, the detector extracts the media-PTS→wall-clock mapping from whatever standard the
transport carries, and a **per-source reuse of the engine's existing lock-state classifier** yields
the tier. We do **not** build a parallel state machine: `ptp.rs` `LockState{Freerun, Acquiring,
Locked, Holdover}` + `sysref.rs` `classify_system` (tolerance / stale / holdover arithmetic) already
partition the lifecycle exactly along the trust boundary (`is_disciplined()` is true for
`Locked|Holdover`, false for `Freerun|Acquiring`).

| `WallClockTier` | Lock-state map | Meaning |
|---|---|---|
| **Trusted** | `Locked` | present + plausible + fresh — *the only tier that can content-sync* |
| **Suspected** | `Holdover` | present but jittery / out-of-tolerance / implausible-jump — falls to reclock-to-house |
| **None** | `Freerun` / `Acquiring` | absent or stale (Acquiring = honest "not yet a lock") — reclock-to-house |

**Per-origin sample adapter (required).** The classifier consumes a `PtpSample{offset_ns, delay_ns}`.
Non-PTP origins (RTCP SR, PDT, TEMI, SEI TC) carry **no native `delay_ns`**, and their "offset" is an
*anchor-drift* (measured wall vs expected wall at a sampled PTS), not a path delay. So each origin
constructs a **synthetic offset/freshness sample** that drives the **same** `classify_system`
arithmetic, with the PTP-shaped `delay_outlier_pct`/`step_threshold` guards **disabled** for non-PTP
origins (no path-delay measurement exists) and Holdover driven **purely from staleness**. The
lifecycle thresholds (`lock_samples`, `stale_after_ns`, `holdover_window_ns`) are reused but tuned per
tier (Loose vs Tight). **A bare timecode (SEI/RP188) caps at `Suspected`** — it is a *label*, not a
disciplined offset, so it never spuriously reaches `Trusted` through the classifier.

### Operator verb + the result

- **`WallClockChoice { Use, Discard }`** — the per-source operator verb (config carries *only* this;
  default `Use` when `tier == Trusted`, else `Discard`).
- **`WallClockRef`** — the result: when **Used**, the affine map
  `wall(pts) = wall_anchor_ns + rescale(pts − media_anchor_ns, rate)`; when **Discarded/None**, the
  **house stamp** `house_anchor_ns = master_now at first arrival`.
- **`SyncMode { ContentSynced(WallClockRef), HouseClocked }`** per source — surfaced so the UI shows
  which mode each tile is in.

New types live in `multiview-core::time` (beside `MediaTime`/`Rational`/`rescale`) or
`multiview-input::wallclock`; **serde internally-tagged**, exact `i64` ns / `Rational`, never float
(inv #3, conventions §5):

```
WallClockTrust { tier: WallClockTier, origin: WallClockOrigin, choice: WallClockChoice }
WallClockOrigin { Ptp, RtcpSr, ProgramDateTime, Temi, DvbTdtTot, SmpteSeiTimecode, Rp188Atc, None }
```

`WallClockOrigin` is name-aligned with the existing `overlay/timecode.rs` `TcSource{Ltc, Vitc,
AtcRp188, Generated}`, extended with `RtcpSr`/`ProgramDateTime`/`Temi`/`Ptp`.

### Reclock-to-house is the AS-BUILT default — Discard needs no new data-plane code

`multiview-input/src/normalize.rs` already anchors the first frame to `master_now`
(`offset = master_now_ns.saturating_sub(raw_ns); master_now_ns`) and thereafter advances by the
**input's own PTS deltas** off that anchor (monotonic-guarded, saturating). That **is** reclock-to-
house. So **Discard/None requires no new data-plane code**; the net-new branch is the **Use** path:
when Trusted + Used, rebase the source onto the **common** wall-clock timeline
(`wallclock_ref.media_to_wall(raw_pts)`) instead of arrival, plus the per-source toggle.

> **Precise semantics (honesty in the gloss).** Reclock-to-house = *anchor the first frame to
> house-now, then advance by the input's own PTS deltas from that anchor*. It is **not** a per-frame
> arrival re-stamp — jitter is deliberately kept out of the timeline (`normalize.rs` keeps the input's
> media cadence rebased to a single house anchor). "Arrival = our-now" describes the **anchor**, not
> every frame.

### The honest content-sync rule (load-bearing)

Reclock-to-house aligns by **ARRIVAL**, not by the real capture instant — the capture-time
information was *thrown away*. Therefore:

> **A discarded/house-clocked source CANNOT be truly content-synced.** It is **"house-clocked /
> live"**: lowest latency, roughly arrival-aligned, **never frame-accurate to the event**.

This is grounded in code (`normalize.rs` overwrites `raw_ns` with `master_now` at frame 0;
`tile.rs` `read_at(now)` then latches arrival-derived frames — there is no capture-time key left to
sample on) and in spec: RFC 3550's RTCP SR pairs the RTP timestamp with the wallclock **when the
data was sampled** (the *capture* instant), not arrival; variable per-source transit (HLS 6–30 s) means
arrival-anchoring substitutes a different, path-dependent zero point per source. Two cameras of one
event over different paths get *different* house anchors → not frame-accurate.
[timing-architecture](timing-architecture.md):142,150,171 corroborate ("two cameras can share
identical timecode yet be phase-misaligned … which is why genlock exists alongside timecode").

**True multi-cam content-sync requires a Trusted common-timeline wall-clock** (PTP, or an SR/PDT/TEMI
map). And it is **tier-qualified**:

- **Tight tier (PTP/ST 2110):** *frame-accurate* — sub-frame phase, because RTP-from-PTP rides the
  grandmaster timeline.
- **Loose tier (SR/PDT):** *time-of-day-accurate* (ms-class, NTP-disciplined) — genuine content-sync,
  vastly better than arrival-aligned, **but not guaranteed sub-frame**.

So the sync badge must say *"frame-accurate (Tight) / time-of-day-accurate (Loose)"* and must **never
over-promise sub-frame phase on the Loose tier**.

**Shared timecode ≠ phase-aligned.** Embedded SMPTE/RP188 timecode is information *about the source*,
not a sync signal — two cameras can carry identical timecode yet be phase-misaligned. So `origin =
SmpteSeiTimecode | Rp188Atc` **caps at Suspected** unless joined to an external UTC/PTP anchor; a TC
alone can never enter `SyncMode::ContentSynced`, only inform a display/alignment label or a
low-confidence house hint. The UI must say *"TC present, phase unknown"*, not *"synced"*.

### Extraction per standard (each its own additive slice, ranked by trust)

- **PTP / ST 2059** (Trusted; servo built) — join the ST 2110 RTP 90 kHz timestamp with the
  `ReferenceTracker` estimate into the affine map. **RP188/ATC** rides ST 2110-40 ANC (`st2110/v40.rs`
  depacketizes DID/SDID; decode-to-`Timecode` is the missing step).
- **RTCP Sender Report** (RFC 3550 §6.4.1) — **missing**; add an SR receiver (str0m RTCP callback /
  libav side channel) producing the `(ntp, rtp)` anchor.
- **HLS EXT-X-PROGRAM-DATE-TIME** (RFC 8216 §4.3.2.6) — **missing**; add a media-playlist scanner
  mapping PDT(segment) → first-sample PTS.
- **DASH `availabilityStartTime` + UTCTiming** — **missing** (analogue).
- **MPEG-TS TEMI** (ISO/IEC 13818-1) — **missing**; DVB TDT/TOT is parsed but is service-of-day with
  no PTS bind → low-confidence house hint, never content-sync.
- **SMPTE ST 12-1 SEI** — **missing**; add `frame.side_data(S12M_TIMECODE)` mirroring the existing
  `A53CC` hook. Suspected at best.

**Invariant safety.** Detection / trust / reclock all live **input-side**; the source stays
**sampled, never pacing** (inv #1) and the per-source trust state publishes through a wait-free
latest-slot so control/preview reads never stall ingest (inv #10).

---

## 2. R2/R3 — per-PROGRAM ENCODED-pre-decode alignment buffer + MAX→OFFLINE

### Buffer placement = INGEST, ENCODED, PRE-DECODE (decode-once)

To align a synced source to reference instant `T`, hold its **compressed packet stream** in a bounded
per-source delay buffer and run **its decoder `D`-behind real-time** so its framestore naturally holds
the frame for `T` when the output samples at `T`. This is **decode-once (inv #5/#7)**: exactly ONE
decoded copy, in the normal framestore, which stays its normal bounded size
(`tile.rs` `RING_CAPACITY = 256`). The delay lives at ingest in **compressed bytes**; the decoder only
ever produces frames *around* `T`, so the ring keeps its normal ~256-frame window around `T` and the
latch-at-`T` read finds the right frame.

> **Correction folded from review (load-bearing).** The existing `jitter.rs` `ReorderBuffer` is
> **post-decode** (instantiated as `ReorderBuffer<ProducedFrame>`, where `ProducedFrame` carries
> *decoded* NV12/P010 pixels), and the `pacer.rs` deadline machinery is **not wired** into the live
> path. Sizing *that* buffer to MAX would buffer **decoded** 4K frames — the prohibitive ~3.1 GB/source
> the design forbids. **Therefore the delay buffer is a NET-NEW `ReorderBuffer<EncodedPacket>`**
> (compressed bytes) inserted **between the demuxer and the decoder**, byte-bounded to `MAX × bitrate`,
> drop-oldest, released at `deadline = wall_clock(pts) + D` via the pacer (which must also be wired in).
> The framestore ring is **never** enlarged to MAX seconds of decoded 4K.

### Per-PROGRAM, not global

The delay buffer is per synced source within a program; **`D` is shared** across the program
(`D` = the slowest synced source's latency, `≤ MAX`). A single **direct/passthrough** program syncs to
nothing ⇒ **NO buffer**, free-run, lowest latency. (`ProgramKind::Passthrough` is not yet in the enum —
it is a future MP-3 slice; today's validation rejects a **single-cell `Multiview`** + WallClockSync.)
The per-program boundary (`MultiviewProgram` owns its own `OutputClock` + `CompositorDrive`) is where
sync mode + MAX + `D` live. `D` is a **latched, hysteresis-damped per-program constant**, changed only
at Class-2 seams — never a per-tick live read (else the output label jitters / goes non-monotonic).

### State model + MAX→OFFLINE (NOT "out of sync")

Per synced source, a **thin orthogonal overlay** on the existing lifecycle ladder — it adds **no
render states**:

| `SyncStatus` | Meaning | Render |
|---|---|---|
| **Synced** | a frame for `T` is available within the alignment window | tracks `read_at(T)` |
| **CatchingUp** | the encoded delay buffer is still warming after start/re-anchor | show last-good |
| **Offline** | the source is **more than MAX behind** our wall-clock — the frame for `T` will never arrive | rides the **existing** fail-to-slate ladder |
| **HouseClocked** | tile is house-aligned (no content-sync) | normal `read_at(tick.pts)` |

**OFFLINE ≠ out-of-sync.** In sync mode the engine keys the framestore read on **`T = our_now − D`**
instead of `tick.pts`. A source whose latest decoded frame is `> MAX` behind `T` latches an aged frame
and rides STALE→NO_SIGNAL **naturally** via the existing `read_at`/`classify` ladder (`tile.rs`
`state_at` + `state.rs`), failing to **last-good then the NoSignal slate** (inv #1/#2). OFFLINE trips
that path **exactly like a dead source** — it **never** emits a degraded out-of-sync picture.
`SyncStatus::Offline` is the *sync-layer reason*; the lifecycle ladder still drives the render. The
read is a **pure, non-blocking atomic snapshot** of the bounded ring — the OFFLINE decision is computed
*on* the read (lag classification), never as a wait.

**OFFLINE threshold = MAX.** So MAX bounds **both** the buffer memory (`MAX × bitrate`) **and** the
offline decision. MAX is operator-configurable, **hard-capped** (e.g. ≤ 30 s), tier-default
(~10 s Loose, ~0.5–1 s Tight).

> **Config-coherence rule (folded from review).** Pin `thresholds.nosignal ≤ MAX` for a WallClockSync
> program, so a source `> MAX` behind `T` is **guaranteed** to have aged past `nosignal` and renders the
> **slate** (true fail-to-slate) rather than holding a `> MAX`-stale last-good frame that would
> misrepresent the synced instant.

**Inv #1 never violated.** `OutputClock::pts_at(index) = MediaTime::from_tick(index, cadence)` is pure
(never accumulated; `deadline_nanos = seed + pts_at`), keeping the **cadence** house-locked. The sync
change is **only** *which* media instant the non-blocking `drive.rs` `sample_cell` reads at (`T`
instead of `tick.pts`). **As-built today `drive.rs` computes `let now = tick.pts;`** — the
`T = tick.pts − D` substitution is the **single net-new sample-seam change**, leaving `pts_at`/
`deadline_nanos`/cadence byte-for-byte unchanged. `D` is derived in integer ns, clamped `0 ≤ D ≤ MAX`,
and **never** flows into `pts_at`/`deadline_nanos`.

---

## 3. R4 — output cadence house-locked, but LABEL = synced instant `T = now − D`

The cadence/label split is the engine's native shape: `out_pts = f(tick)` is **pure**; the advertised
wall-clock **label** is a separate projection that does not feed back into pacing
([timing-architecture](timing-architecture.md) §6 layer A "WHEN a frame emits" vs layer D "WHAT UTC it
is"; [ADR-0032] `utc0@tick0`, "never summed EXTINF"). The label is computed from one monotonic→UTC
anchor:

> **`label_pts(N) = utc0 + rescale(N, cadence) − D`**

- **FREE-RUN:** `D = 0` ⇒ label = `utc0 + rescale(N)` = our wall-clock (today's only behaviour).
- **SYNCING:** `D > 0` ⇒ label = `house_clock(N) − D` = `T`, the reference instant the tiles are
  aligned to — **NOT our emission wall-clock** (which would falsely advertise "now" while showing
  content from `now − D`). Because the cadence is house-locked, `house_clock(N) − D = our_now − D = T`,
  so the three forms are algebraically equal.

`D` is subtracted **once at the label-projection seam, never on the data plane** — it changes only the
number written into the carrier, never `out_pts`, never the deadline, never which frame is sampled.

**The three carriers (all greenfield on output):**

1. **HLS EXT-X-PROGRAM-DATE-TIME** — `hls/media.rs` `render()` emits only `#EXTINF` (no PDT field on
   `Segment` today). Add a PDT field + emit RFC-3339 UTC = `label_pts(segment_start_tick)`.
2. **H.264 pic_timing / H.265 time_code SEI (SMPTE ST 12-1)** — inject HH:MM:SS:FF (tick-phase) + label
   TOD into the output bitstream (overlay models the value display-only today). Confirm the
   `ffmpeg-next` binding name (`Type::S12M_TIMECODE`) at implementation time.
3. **RTCP SR NTP↔RTP for RTP out / NDI** — for true RTP out, emit an RFC-3550 SR pairing NTP from the
   synced instant with the RTP timestamp. **NDI does NOT use RTCP SR** — it carries its own 100 ns
   timecode field; map *that* to the synced instant (a distinct path; do not imply NDI emits an SR).

**On any `D` change** (FreeRun↔Sync, or slowest-source re-derivation): step the label, mark the next
HLS segment `EXT-X-DISCONTINUITY` + re-anchor, jam the SEI/RTCP-SR — gated behind the **Class-2**
plan/dry-run. **Never** step the PDT silently on a "continuous" timeline.

**TAI/UTC hazard.** PTP/SMPTE-Epoch are **TAI**; PDT/SR are **UTC/NTP-1900**. Apply the current
TAI-UTC offset (37 s) and the PTP `currentUtcOffset` before writing any UTC label, all in integer ns
(inv #3).

### Loose vs Tight tiers

- **Loose** (HLS/RTSP, 6–30 s end-to-end): default MAX ~10 s; trust comes from SR/PDT/TEMI estimates.
- **Tight** (PTP/ST 2110): default MAX ~0.5–1 s, often sub-frame.

The **Tight** tier **disciplines** (never jams) the monotonic pacing clock with the `ptp.rs` servo.

> **As-built caveat (folded from review).** The servo's `frequency_ppb` is **computed but not wired**:
> `OutputClock::deadline_nanos` takes no rate/slew input, and `seed_nanos` is read once and never
> re-anchored. Syntonization is therefore **proposed**, not as-built. To implement safely:
> - Add an explicit **cadence-rate-scale** input to `OutputClock` (a slewed-rate `Rational`), so the
>   slew adjusts the *rate* of tick advance only; `pts_at(index)` stays a pure integer function and
>   `seed_nanos` is **never** stepped mid-run (a phase step = Class-2).
> - **Bound** the per-update ppb slew and clamp accumulated rate deviation, so a bad servo sample can
>   never visibly speed/slow output (inv #1).
> - Seed the ST 2059-1 SMPTE-Epoch origin (1970 TAI) at **start/relock only**; a mid-run re-jam is
>   **Class-2**.
> - Keep any AES67/RTP send-timestamp computation **separate** from the CLOCK_MONOTONIC tick clock
>   (honour the PTP-is-reference-not-pacer principle; do not write a second servo for pacing).

PTP-lost rides the built ladder: Locked → Holdover (coast, flagged) → Freerun → `ReferenceSelector`
falls to the NTP-disciplined system clock; the output label follows the badge state, and `D` reverts
toward 0 as sources drop out of tight sync.

### Outbound presentation epoch — the consumer-side projection (ADR-M010)

The three carriers above write the synced instant *into the media*. The managed-devices/display-node
design ([ADR-M010](../decisions/ADR-M010.md), [display-out](display-out.md)) additionally publishes
the same anchor as a machine-readable **presentation epoch**, so downstream presenters (our display
nodes first) can choose frames against a common wall timeline. This extends §3's outbound story; the
inbound DETECT/trust machinery (§1) and the servo rules above are unchanged.

- **One outbound `WallClockRef` per program** — the same exact-affine type
  (`multiview-core/src/wallclock.rs`, `i128` intermediates via `rescale`, never float) used on the
  inbound **Use** path, here anchoring **output tick ↔ disciplined wall ns**. The wall estimate comes
  from the existing reference machinery (the `ptp.rs` servo, or the NTP/chrony-disciplined system
  clock via `sysref.rs`) and **disciplines an estimate only — it never paces the tick loop** (inv #1,
  exactly the Tight-tier rule above).
- **Distribution:** (a) `{stream_id, WallClockRef, link_offset, clock_source/quality}` on the control
  WS (versioned in `multiview-events`, conflated latest-wins — inv #10 already holds on that channel);
  (b) **RTCP SR** NTP↔RTP on the RTSP output and (c) **`EXT-X-PROGRAM-DATE-TIME`** on HLS, both
  stamped from the *same* epoch (carriers 3 and 1 above — one anchor, every surface agrees);
  (d) optionally **RFC 7273** `a=ts-refclk`/`a=mediaclk` in the RTSP SDP — a cheap standards-aligned
  advertisement; standard receivers must still opt in, so it is not load-bearing for our own nodes.
- **Link-offset semantics (AES67's rule applied to video):** a fixed per-deployment receiver delay
  added to `wall_at(pts)` (default ≈ 2× max network jitter + decode time, e.g. 100–300 ms).
  **Uniformity across receivers is what matters, not smallness.** A receiver presents the frame whose
  `wall_at(pts) + link_offset` is closest to its next vblank — repeat if early, drop if late: pure
  pull-side frame *choice*, never feedback into the engine. A consumer that loses the WS keeps its
  last epoch and free-runs, drift-bounded — graceful degradation, the output never falters.
- **Epoch vs label:** the epoch publishes the program's tick↔wall *map*; the R4 label (`T = now − D`)
  is the *value* the carriers write. One anchor, two projections — and an epoch re-anchor follows the
  same Class-2 discontinuity rules as a `D` change (step the label, `EXT-X-DISCONTINUITY`, jam the
  SEI/SR).
- **Tier honesty carries through:** epoch-disciplined display nodes achieve frame accuracy (the
  PTP/chrony tiers in [display-out](display-out.md)); vendor decoders remain bounded-drift and Cast
  seconds-class — the epoch cannot improve a device that exposes no presentation-timing control.

---

## 4. Efficiency — encoded-vs-decoded budget

The whole feasibility of a MAX-window alignment buffer rests on placing the delay in **compressed
bytes pre-decode**, not decoded frames. The decoded alternative is **prohibitive** *and* is a second
decoded copy (violates decode-once #5/#7); the encoded buffer adds **zero** decoded copies and is
bounded by `MAX × peak_bitrate`.

A decoded 4K NV12 frame (inv #5 = 1.5 B/px): `3840 × 2160 × 1.5 = 12,441,600 B = 12.44 MB/frame`.

| Buffer | MAX = 0.5 s | MAX = 1 s | MAX = 10 s | 2×2 @ MAX = 10 s |
|---|---|---|---|---|
| **Decoded** 4K NV12 @ 25 fps (prohibitive; + a *second* decoded copy) | 0.156 GB/src | 0.311 GB/src | **3.11 GB/src** | **12.44 GB** |
| **Encoded** 4K H.264 @ ~5 Mbps (streaming-tier; the chosen design) | 0.31 MB/src | 0.625 MB/src | **6.25 MB/src** | **25 MB** |
| **Encoded** 4K H.264 @ ~25–50 Mbps (broadcast-tier) | 1.6–3.1 MB/src | 3.1–6.25 MB/src | 31–62 MB/src | 125–250 MB |

- **Ratio is bitrate-dependent:** `~500×` cheaper at 5 Mbps streaming-tier (`3.11 GB / 6.25 MB ≈ 498×`);
  `~50–100×` at 25–50 Mbps broadcast-tier. Even at a pessimistic 50 Mbps the encoded 2×2 is 250 MB vs
  12.44 GB decoded — still trivially bounded. **The load-bearing point is not the headline number** but
  that the encoded buffer is bounded at `MAX × peak_bitrate` and adds **zero** decoded copies.
- **The actual memory bound is `MAX × peak_bitrate`** — pin `configured_max_bitrate` so a 50 Mbps source
  is explicitly accounted for; a property test must assert `buffer_bytes ≤ MAX × configured_max_bitrate`
  and forbid a `ReorderBuffer<DecodedFrame>` at MAX depth.
- **Framestore stays bounded:** the decoder runs `D`-behind, producing frames only around `T`; the ring
  keeps `RING_CAPACITY = 256` (~10.2 s @ 25 fps), unchanged. The copy-on-write publish is `O(capacity)`
  on the **sampled input thread**, never the output clock ([ADR-T009]).
- **Latency budget:** added program latency = `D` = slowest synced source's latency `≤ MAX`. MAX
  hard-caps **worst-case `D`** (the operator cannot request an unbounded delay). A direct/passthrough
  single-source program adds `D = 0`.
- **Bounded everywhere on the data plane** (inv #5/#7 + safety rule 5): the encoded delay buffer is
  strictly bounded, **drop-oldest** ("drops, never grows"), released at deadline via the pacer; no
  unbounded growth, no per-frame allocation. **Degradation is invariant-safe:** if a 4K decoder cannot
  sustain `D`-behind in real time, the encoded buffer fills to MAX then drop-oldest, and the source goes
  **OFFLINE** — not a memory blow-up.

---

## 5. Config / API / UI (greenfield surface)

### Config

- **Per-source** (`multiview-config` `schema.rs` `Source`, beside `auth`/`color_override`/`captions`/
  `gpu_pin`, internally-tagged): `wall_clock: Option<SourceWallClock { choice: Use | Discard }>`.
  Config carries **only** the operator verb (default `Use` when `tier == Trusted`, else `Discard`); the
  detected tier is **measured at runtime, never authored**.
- **Per-program** (`program.rs` `ProgramSpec`; semantics on `ProgramKind::Multiview`):
  `sync: { mode: FreeRun (default) | WallClockSync, max_window: <validated Duration newtype, tier-bounded>,
  tier: Loose | Tight }`. Reach the legacy flat `MultiviewConfig` path via desugar onto the synthesized
  `main` program. **Default FreeRun ⇒ exact zero behaviour change.** Internally tagged, never
  `untagged`; exact rationals, never float.
- **R4 label** is a per-output / per-program enable toggle (greenfield — not covered by ADR-0032 serving
  infra or ADR-T003 input timing).
- **Validation** (mirror the lower-to-core-then-validate pattern, `ConfigError::Validation`): (1)
  **reject** a single-cell `Multiview` (and, when it lands, a `Passthrough`) program with
  `mode = WallClockSync` (syncs to nothing → no buffer); (2) **bound** `max_window` within the tier MAX;
  (3) **flag** (warning, runtime — not static) a `Used` source whose detected confidence can only be
  None/house as house-align-only.

### API

- **Per-source** Use/Discard rides the generic source CRUD body (`control/routes/sources.rs`,
  ETag/If-Match, RBAC, audit; engine-side validation on apply).
- **Per-program** sync/`max_window` change **alters latency `D`** ⇒ **Class-2 / Reset-lite** (inv #11).
  The API **must** expose a plan/dry-run surfacing "will reset N outputs / consumers reconnect"
  **before** apply, wired through the **existing** `RouteClass{Class1, ResetLite, Class2}` classifier
  (`control/src/routing.rs`) — not a parallel mechanism. A **new capability-matrix row**
  `Program.sync.{mode, max_window, tier}` is required. Rides the config-revision commit/diff/rollback
  path.

### Realtime (inv #10 — conflated, drop-oldest, never back-pressures the engine)

- **Per-source `WallClockTrust`** on the `Inputs` topic — extend `events` `event.rs` `InputConnection`
  with tier + origin + effective mode (Used | Discarded→house).
- **Per-tile `SyncStatus`** on the `Tiles` topic — orthogonal to `LifecycleState`; **Offline maps onto
  the existing NO_SIGNAL / fail-to-slate path** (no new render state). Register in openapi/asyncapi + the
  generated TS types.

### UI (web/, React 19, generated OpenAPI client — do not hand-write types)

1. Per-source trust **badge** (Trusted/Suspected/None) + Use/Discard **toggle** (by `SourcePalette`).
2. Per-program sync toggle (FreeRun/WallClockSync) + MAX control + tier selector with the **Class-2
   confirm**.
3. Per-tile sync indicator (Synced/Catching-up/Offline/House + skew) extending `TileStateBadge.tsx` on
   the orthogonal sync axis; for TC-only sources display *"TC present, phase unknown"*, never "synced".
4. Program rollup "N/M synced, K offline" badge + the added-latency **`D`** readout on the `SystemPage`
   program card.

All best-effort, tolerate dropped/conflated events. TS sync/trust fields may ship ahead of the
AsyncAPI regen via the `envelope.ts` `TileStateDeltaData` extension precedent, then be folded in via
`cargo xtask gen-openapi` + `npm run generate:events`.

---

## 6. References

- Invariants: CLAUDE.md §2 / [conventions](../architecture/conventions.md) §5 (#1, #2, #3, #5, #7, #10, #11).
- [ADR-T003](../decisions/ADR-T003.md) unified timing; [ADR-T001] output-clock; [ADR-T009] framestore COW.
- [ADR-0032](../decisions/ADR-0032.md) HLS serving + `utc0@tick0` PDT anchor.
- [ADR-M010](../decisions/ADR-M010.md) outbound presentation epoch + link offset + sync groups; [display-out](display-out.md) node presentation discipline.
- [ADR-0020] the WHEN/WHAT timing-layer split + slew-not-step; [timing-architecture](timing-architecture.md) §6/§7.
- [streaming-gotchas](streaming-gotchas.md) §3/§4 (latency tiers, RTCP-SR alignment of separate RTP sessions).
- [core-engine](core-engine.md) §9.1/§9.2.
- Standards: RFC 3550 §6.4.1 (RTCP SR), RFC 8216 §4.3.2.6 (EXT-X-PROGRAM-DATE-TIME), SMPTE ST 12-1 +
  H.264 pic_timing / H.265 time_code SEI, ISO/IEC 13818-1 (TEMI; DVB TDT/TOT), SMPTE ST 2059 / IEEE 1588 (PTP).
