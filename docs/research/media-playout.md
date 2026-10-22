# Multiview — Media Library, Media Players & Alpha Media

> **Design brief — Media Library, Media Players & Alpha Media.** Authoritative
> research/design record for the asset + playout half of the production-switcher
> layer. Produced by a verification-hardened multi-agent research workflow
> (2026-06-11). This is a **design for an unbuilt feature** — nothing in this
> brief exists as a media library, media player, or alpha-media path today; every
> reference to *existing* code names a real, verified path and is labelled
> **BUILT / SCAFFOLD / DOC-ONLY**. Companion brief:
> [production-switcher](production-switcher.md) (the M/E, transition, keyer and
> bus model). Decisions: [ADR-0057](../decisions/ADR-0057.md) (media library +
> players), [ADR-0058](../decisions/ADR-0058.md) (NV12+A alpha path),
> [ADR-M012](../decisions/ADR-M012.md) (resource model),
> [ADR-W021](../decisions/ADR-W021.md)/[ADR-RT008](../decisions/ADR-RT008.md)
> (control/realtime surface), [ADR-T015](../decisions/ADR-T015.md) (timing),
> [ADR-R011](../decisions/ADR-R011.md) (resilience). Transition-side contracts
> (stinger arming, trigger windows) live in
> [ADR-0055](../decisions/ADR-0055.md); keyer-side fill/key contracts in
> [ADR-0056](../decisions/ADR-0056.md); audio-follow in
> [ADR-0059](../decisions/ADR-0059.md).
> **Invariants in play:** #1 (output clock — players are *sampled*, never
> pacing), #2 (last-good-frame), #3 (exact-rational timing — durations are
> integer frame counts), #5 (NV12-throughout — alpha media gets a budgeted
> NV12+A extension, never per-tile RGBA video), #8 (color order — alpha is
> premultiplied in-shader in linear light), #10 (isolation — import/transport
> can never back-pressure the engine).

> **Vendor posture.** All vocabulary here is generic industry terminology
> (media library, media player, still store, stinger, fill/key, trigger
> point). Where a behavior is industry consensus without a written standard it
> is labelled *de-facto industry practice*. External references are open,
> published documents only: FFmpeg documentation, the CasparCG AMCP protocol
> (open project), and the obs-websocket protocol (open project). No commercial
> product is named, compared to, or excluded by name
> ([CODE_OF_CONDUCT](../../CODE_OF_CONDUCT.md)).

**The one mental model: the media *library* is control-plane storage + import
policy; a media *player* is an ordinary bus-selectable source — a
transport-controlled extension of the production file-ingest path that
publishes frames into the same lock-free last-good tile stores the engine
already samples per tick. The engine never knows a tile is "media": it latches
whatever frame the store holds at `tick.pts`, so a player can be cued, paused,
looping, underrunning, or dead and the output clock never notices (invariants
#1/#2/#10 by construction).**

---

## 0. Headlines

1. **Library ≠ players.** The **media library** is an asset store (stills,
   clips, audio assets) with an import/validation/transcode pipeline — pure
   control-plane, desired-state, CRUD'd like every other resource. **Media
   player channels** are a small fixed set of bus-selectable *sources* that
   load library assets and expose transport (cue/play/pause/seek/loop). The
   separation is de-facto industry practice and it maps 1:1 onto the repo's
   existing desired-state-vs-engine-owned split ([ADR-0057](../decisions/ADR-0057.md)).
2. **A player is ~⅓ built already — in the CLI pipeline, not in
   `multiview-input`.** The production file path (`ingest_loop` →
   `open_and_stream`, `crates/multiview-cli/src/pipeline.rs:5533/5812`,
   **BUILT**) already does open → decode → PTS-normalize → wall-clock pace by
   PTS (`PtsWallClock`, `pipeline.rs:6189`, invariant #4) → publish into a
   `TileStore` with `NoSignalPolicy::HoldForever` (`pipeline.rs:1184-1188`),
   plays a finite file once and **holds its last frame forever**
   (`pipeline.rs:5575-5579`). What is missing is exactly the transport layer:
   cue, play/pause, seek, loop, EOF policy, frame-accurate start. **Players
   extend this path.** The `multiview-input` `IngestPump`/`FrameProducer`
   stack (`crates/multiview-input/src/source.rs:110/169`) is a **SCAFFOLD
   parallel implementation with zero production consumers — do not build on
   it** (§2.1).
3. **Every transport primitive has a verified seam.** Cue = open + decode
   first frame + publish + hold, completion gated on `TileStore::is_primed`
   (`crates/multiview-framestore/src/tile.rs:311`, **BUILT**). Play/pause =
   anchor/suspend the existing `PtsWallClock` pacer (its backwards-PTS
   re-anchor at `pipeline.rs:6205-6208` is the verified discontinuity
   handler). Seek = `Demuxer::seek` (`crates/multiview-ffmpeg/src/demux.rs:549`,
   **BUILT with zero production callers**) + decode-to-frame. Loop = in-place
   `Demuxer::seek(in_point)` + decoder flush on clean EOF, with the existing
   supervised reconnect bracket (`pipeline.rs:5561-5590`) as the failed-seek
   fallback ([ADR-0057](../decisions/ADR-0057.md), §7.5). EOF policy = a
   store-policy + player-state decision on the already-proven hold path.
4. **The player timeline is output-anchored, not file-relative.** The boot-time
   file path stamps frames with *source-relative* media time
   (`PtsWallClock::publish_time`, `pipeline.rs:6241`) — correct only because
   ingest starts at run start. A player cued mid-show must stamp
   `play_anchor + (pts − in_point)` where `play_anchor` is the output media
   time of the designated start tick, monotonic across loop laps. That single
   rule makes `read_at`'s latch-on-tick (`tile.rs:400-417`) deliver
   frame-accurate start, gapless loops, and honest freshness classification
   for free (§7.2).
5. **Frame-accurate start is a stamping rule, not a scheduling race.** Play
   carries a `start_tick`; frame *k* is stamped
   `start_tick_pts + k × frame_period` (exact rational). The engine latches
   nearest-not-after per tick, so command-delivery jitter (≤ one drain) never
   shifts which frame lands on which tick. Play-on-take = the switcher state
   machine issuing play with `start_tick = take_tick − pre_roll_frames`
   ([ADR-0055](../decisions/ADR-0055.md) validates the window).
6. **Alpha media rides NV12+A (NV12 + one full-resolution R8 alpha plane,
   2.5 B/px) — never per-tile RGBA video (invariant #5), never the overlay
   stack** (overlays are input-decoupled by design; a video-rate media fill
   does not belong there). The payload carries **straight** alpha;
   premultiplication happens in-shader in linear light exactly where the
   compositor premultiplies `Cell.opacity` today, preserving invariant #8.
   [ADR-0058](../decisions/ADR-0058.md) carries the decision; §6 carries the
   why + budgets.
7. **Format policy is pinned by what FFmpeg actually decodes with alpha**
   (verified externally): ProRes 4444 / 4444 XQ, QuickTime Animation (`qtrle`),
   PNG/TGA image sequences, and VP9-with-alpha via `libvpx-vp9` (the native
   VP9 decoder ignores the alpha side stream). **HEVC-with-alpha decodes as
   fully opaque in FFmpeg** — it is rejected at import or import-transcoded,
   never silently accepted (§5).
8. **Stingers are pre-decoded at import into a bounded in-memory NV12+A
   mezzanine** — playout publishes pooled, already-decoded frames; zero decode
   on the hot path. A 3 s 1080p60 stinger is 180 frames × 5 184 000 B ≈
   **0.93 GB** NV12+A vs ≈ **1.49 GB** RGBA (4 B/px) — and the rejected
   2 B/px subsampled-alpha variant (≈ 0.75 GB) loses keyer-edge fidelity for
   an 0.18 GB saving (§6.2). Longer clips get a hard cap + a pool-allocated
   decode-ahead ring, never unbounded residency (§6.3, standing efficiency
   review).
9. **Stills are decode-once + `HoldForever`.** The EOF-hold path proves the
   semantics today (a finite file's tile holds its last frame forever,
   **BUILT**); a still is that path with exactly one frame. An optional
   `publish_arc` heartbeat (`tile.rs:253`) keeps the freshness ladder honest
   for intentionally-held content (§8).
10. **Import does the work; the hot path does none.** Probe, policy checks,
    transcode, mezzanine pre-decode all run at import/cue time on
    control-plane / ingest threads. The engine tick only ever `read_at`s a
    store. Import failures are honest, typed, per-asset errors
    (RFC 9457 `problem+json`) — never a half-imported asset, never an engine
    effect (§4, §14).
11. **Media audio is ordinary program-bus audio.** A player's embedded audio
    decodes on a per-player audio thread into an `AudioStore` and routes onto
    the existing `ProgramBus` (the production unity-routing seam,
    `pipeline.rs:1960-1973`, **BUILT**); transport commands fan to video and
    audio readers from one transport state so they cannot diverge.
    AFV under transitions is [ADR-0059](../decisions/ADR-0059.md)'s
    per-sample `GainRamp` machinery (§11); ducking is a post-MVP mixer item
    (named in ADR-0059's deferred list, undesigned).
12. **Protocol posture: native API only for v1.** Transport semantics align
    with the open CasparCG AMCP precedent (cue shows the in-point frame,
    play-on-take, hold-on-last-frame, background-load/auto-chain as a later
    tier). VDCP and AMP are legacy RS-422-era external-playout-server
    protocols — explicitly out of scope, revisited only on a concrete interop
    demand (§12).
13. **MVP = stills + clips, loop, play-on-take, EOF policy, 2 player
    channels**, NV12+A stills for the DSKs, embedded audio to the program
    bus. Post-MVP: stinger clip transitions (Wave 3 of
    [ADR-0055](../decisions/ADR-0055.md)), auto-chain cue, audio-only assets,
    external automation interop (§15).

---

## 1. Vocabulary: library, players, still store

- **Media library** — the asset store: every still, clip, and (post-MVP)
  audio asset the facility has imported, with probed + declared metadata and
  validation state. A control-plane resource collection
  (config-as-code exportable, [ADR-M012](../decisions/ADR-M012.md)). It does
  **not** play anything.
- **Media player** (channel) — a bus-selectable source with transport. A
  facility configures a small fixed set (MVP: 2). A player *loads* a library
  asset and then cues/plays/pauses/loops it. To the switcher
  ([production-switcher](production-switcher.md)) a player is just a source id
  selectable on PGM/PVW/aux, usable as a keyer fill, or referenced by a
  stinger transition.
- **Still store** — not a third subsystem: the still path is the media library
  holding still assets plus a player (or a direct `Still` source binding,
  §15) displaying one. The term is kept because operators use it; in this
  design it names a *usage*, not a component.

The separation is de-facto industry practice (asset storage distinct from
player channels) and matches the repo's Device precedent: desired state
(assets, player definitions, default policies) in config/resources; live
transport state (playing/paused/position) engine-owned and mirrored read-only
([ADR-M012](../decisions/ADR-M012.md)).

---

## 2. As-built substrate (verified) — what exists, what is a trap

| Capability | As-built today | Status | Gap a player must fill |
|---|---|---|---|
| File open → decode → publish | `ingest_loop` (`crates/multiview-cli/src/pipeline.rs:5533`) supervises `open_and_stream` (`:5812`): libav open with `rw_timeout`, best-video-stream select, NVDEC-preferring decoder, `PtsNormalizer` (invariant #3), scale-to-tile, publish. | **BUILT** | Transport layer only. |
| File pacing | `PtsWallClock` (`pipeline.rs:6189`): anchors first PTS to wall clock, releases each frame on time (invariant #4, never `-re`); backwards PTS **re-anchors instead of stalling** (`:6205-6208`); waits poll `stop` in ≤ 50 ms slices. | **BUILT** | Pause/resume = suspend/re-anchor; verified re-anchor path is the seek/loop discontinuity handler. |
| EOF behavior | Clean EOF drains the decoder and returns; a finite source's thread ends and "its tile now holds its last-good frame forever" (`pipeline.rs:5575-5579`, `IngestPlan.live` doc `:1051-1054`). | **BUILT** | This *is* `eof_policy: hold_last_frame`. The other three policies (§7.4) are new. |
| Hold semantics | Every source store is built `NoSignalPolicy::HoldForever` (`pipeline.rs:1184-1188`); both readers return the held frame even at `NO_SIGNAL` — `read` (`crates/multiview-framestore/src/tile.rs:371-396`) and `read_at` (`tile.rs:421-444`). | **BUILT** | Nothing — this is the still/hold substrate. |
| Latch-on-tick sampling | `CompositorDrive::compose(tick)` samples each cell's store at `now = tick.pts` (`crates/multiview-engine/src/drive.rs:449-460`, `read_at` at `:579`); selection = newest entry `at ≤ now` (`tile.rs:400-417`). | **BUILT** | Players must stamp output-anchored times (§7.2) — the only timeline rule that makes mid-show cue correct. |
| Prime gating | `TileStore::is_primed` (`tile.rs:311`); startup prime-wait hard-capped at `PRIME_WAIT_BUDGET` = 1 500 ms (`pipeline.rs:5249`, invariant #1). | **BUILT** | Reuse as the cue-complete signal; the switcher's WARM-ON-ARM ([ADR-P007](../decisions/ADR-P007.md)) reads the same bit. |
| Container seek | `Demuxer::seek(timestamp)` — `AV_TIME_BASE` µs, lands on/before target, full-range bracket so libav picks the nearest keyframe (`crates/multiview-ffmpeg/src/demux.rs:549-553`). | **BUILT, zero callers** | Wire it: seek → decode-discard to the exact frame → publish + hold. |
| Reconnect bracket | `ingest_loop`'s loop: capped-exponential backoff + jitter, healthy-streak reset (`pipeline.rs:5561-5590`). | **BUILT** | The failed-seek fallback leg of loop rides it (§7.5); player thread crash recovery inherits it. |
| Live source add/remove | `Command::UpsertSource`/`RemoveSource` (`crates/multiview-control/src/command.rs:238`) + `LiveSourceHub` off-thread producer spawn (`crates/multiview-cli/src/live_sources.rs:189`); only **synthetic** kinds live-apply ([ADR-W018](../decisions/ADR-W018.md)). | **BUILT (synthetic-only)** | Player channels are *pre-declared* sources (fixed set), so they sidestep the live-spawn restriction: load/cue swap what the existing thread plays, not the thread itself. |
| Program audio | Per-source audio decode threads → `AudioStore`s → all routed at unity onto `ProgramBus` in the bake consumer (`pipeline.rs:1960-1973`); `enable_program_audio` (`:1490`); `repoint_crossfade`/`GainRamp` (`crates/multiview-audio/src/program.rs:217`, `mixer.rs:35`); `SampleClock` exact budgets (`cadence.rs:36`). | **BUILT** (CLI-flag-gated) | Per-player audio store + transport fan-out (§11); the control seam is [ADR-0059](../decisions/ADR-0059.md)'s. |
| Alpha anywhere | None. `Nv12Image` has no alpha plane (`crates/multiview-compositor/src/pipeline.rs:169`); NDI BGRA alpha is **explicitly dropped** at ingest (`crates/multiview-input/src/ndi/convert.rs:369`); config has no `has_alpha`. | **missing** | The NV12+A path, [ADR-0058](../decisions/ADR-0058.md) + §6. |
| Media options in config | `SourceKind::File { path }` — path only (`crates/multiview-config/src/schema.rs:341-344`); no loop/in/out/hold, no still kind, no asset model. | **missing** | [ADR-0057](../decisions/ADR-0057.md) schema (§15). |

### 2.1 The parallel-implementation trap (load-bearing warning)

`multiview-input` contains a *second*, divergent ingest core: the
`FrameProducer` trait + `IngestPump` + `Pacer`
(`crates/multiview-input/src/source.rs:110/169`, `pacer.rs:76`). It is
**SCAFFOLD**: compiled, documented, unit-tested — and consumed by **no
production path**. The binary's real ingest is the CLI pipeline using
`multiview-ffmpeg` directly (only `PtsNormalizer`, the NDI producer, and the
HLS playlist parser from `multiview-input` are production-consumed). **The
media-player work builds on the CLI `ingest_loop` path and must not touch
`IngestPump`** — extending the scaffold would produce a player that works in
tests and does not exist in `multiview run`. (Same audit finding as
[ADR-I005](../decisions/ADR-I005.md)'s twin-`EncodedPacket` trap; stated here
because it is the single most likely wrong turn for this feature.)

---

## 3. Asset model

An asset is one library entry. Two metadata families, kept separate because
they have different truth sources:

**Probed (machine truth, written by import, read-only):**

- `container`, `video_codec`, `pixel_format`, `audio` (streams: codec, rate,
  channels, language) — from the existing demux/inventory machinery
  (`Demuxer::streams()`/`inventory()`, **BUILT**).
- `width`, `height`; `fps` as an **exact rational** (e.g. `60000/1001`) —
  never a float (invariant #3).
- `duration_frames` (integer, at the asset's native cadence) — the canonical
  length. Wall-clock duration is *derived* for display only.
- `has_alpha` + `alpha_verified` (§5: verified by decoding, not by codec
  name), `premultiplied` (probed where the container declares it; else
  declared).
- `bytes_on_disk`, content hash (cache key + integrity).

**Declared (operator intent, validated at import and on edit):**

- `kind`: `still | clip | audio` (audio assets post-MVP, §15).
- `label`, free-form id (the repo's non-empty-unique-string convention).
- `in_point_frames` / `out_point_frames` (optional trim, integer frames).
- `trigger_point_frames` (stingers only — the frame at which the clip fully
  covers the canvas and the underlying background transition may start;
  generic term for the de-facto industry "transition point"). **Required** for
  an asset to be usable as a stinger.
- `default_eof_policy` (§7.4), `default_loop` (bool).
- `premultiplied` override (when the container is silent; §6.1).
- `usage` hints: `stinger`, `graphics_fill`, `general` — drives validation
  strictness (§4.2).

**Stinger/graphics-fill assets additionally require**
([ADR-0057](../decisions/ADR-0057.md)): native resolution
**equal to the canvas** and native fps **equal to the output cadence** (exact
rational equality, `1001`-aware). A mismatch is a validation failure with a
one-click remediation: import-transcode to conform (§4.3). Rationale: a
full-canvas alpha clip that needs scaling or rate conversion at playout would
put resample work on the playout path and frame-count ambiguity into the
trigger window — both rejected.

All timing fields are **integer frame counts**; the API accepts milliseconds
and converts through exact rationals per
[ADR-T015](../decisions/ADR-T015.md) — `frames = round(ms × fps_num /
(1000 × fps_den))`, never float fps/seconds.

---

## 4. Import & validation pipeline

Import is a control-plane operation (202 + operation id; long-running results
on the realtime stream per [conventions §6](../architecture/conventions.md)).
It never touches the engine. Stages:

### 4.1 Register

`POST /api/v1/media/assets` with either an upload or a reference
(`path` under the configured media root, or a URL — IPv6-first, e.g.
`https://[2001:db8::15]/assets/open-title.mov`). The asset is created in state
`importing`.

### 4.2 Probe + policy

Open with the existing demux machinery (on a blocking control-plane task, off
every engine thread), extract the probed family (§3), then apply the format
policy (§5):

- accepted as-is → state `ready`;
- accepted-with-transcode (HEVC-alpha, fps/resolution conform for stingers)
  → §4.3;
- rejected → state `failed` with a typed, remediation-bearing problem detail
  (e.g. *"HEVC alpha layers decode as opaque in the deployed FFmpeg; re-export
  as ProRes 4444 / qtrle / PNG sequence / VP9-alpha, or enable
  import-transcode"*). **Never** a silent partial import
  ([ADR-0057](../decisions/ADR-0057.md)).

**Alpha is verified by decoding, not by allowlist alone:** import decodes
probe frames (first, `trigger_point`, midpoint) and inspects the alpha plane.
The codec allowlist gates *admission*; the decode probe catches the silent
failure class (a build whose `libvpx-vp9` is absent decodes VP9 opaque; an
allowlisted file can still carry a degenerate alpha track). A stinger whose
probe frames are all fully opaque gets a warning — a stinger that never
exposes transparency is almost certainly a mastering error.

### 4.3 Import-transcode (optional, off by default per asset class)

Conforms a rejected-but-convertible asset to an accepted intra-frame
alpha format. Default target: **`qtrle`** (FFmpeg-native encoder, intra, RLE,
LGPL-clean, alpha-capable); alternatives `prores_ks` (4444 profile) and PNG
sequence — pinned in [ADR-0057](../decisions/ADR-0057.md). The transcode also
performs stinger conform (scale to canvas, exact-rational fps conversion) so
playout never does. Runs with bounded parallelism on the control plane;
progress events on the stream; the original is retained until the transcode
validates (decode-probe of the *output*).

### 4.4 Mezzanine pre-decode (stingers / short alpha clips)

Assets flagged `stinger` (and graphics fills under the cap) are pre-decoded to
the in-memory NV12+A mezzanine **at import or first load**, so a stinger take
never decodes on the hot path (§6.3). The mezzanine is pool-allocated at load,
size-validated against the cap *before* allocation, and dropped under explicit
unload or library-budget pressure — never grown per-frame.

---

## 5. Format policy — the FFmpeg alpha truth table

Verified against FFmpeg behavior (and to be re-verified at import time per
§4.2 — the deploy ffmpeg version is the only truth that matters, a lesson
already learned on this repo's HLS path):

| Format | FFmpeg decode-with-alpha | Policy |
|---|---|---|
| ProRes 4444 / 4444 XQ | **Yes** (native decoder; alpha up to 16-bit). ≈ 330 / ≈ 500 Mbit/s at 1080p30-class rates — heavy I/O, cheap intra multithreaded CPU decode. | Accept. Preferred mastering format for stingers. |
| QuickTime Animation (`qtrle`) | **Yes** (native; RLE, cheap). | Accept. Also the default import-transcode target. |
| PNG / TGA image sequences | **Yes** (universal; per-frame zlib cost for PNG). | Accept. Sequence cadence is *declared* (a directory of stills has no intrinsic fps); stinger conform rules apply. |
| VP9 with alpha | **Yes, only via `libvpx-vp9`** — the alpha rides a side stream the native decoder ignores. | Accept **iff** the import decode-probe shows real alpha (this also catches builds without libvpx); else reject with remediation. |
| HEVC with alpha | **No.** FFmpeg decodes the alpha layer as fully opaque (8.0 added only libx265 alpha *encoding*). | **Reject or import-transcode** (§4.3). Never silently accept — an "alpha" stinger that plays opaque covers the program with a black-matted card. |
| Anything else (H.264, opaque HEVC, etc.) | Opaque. | Accept for *opaque* clips/stills (general media); `has_alpha = false`; unusable as stinger/graphics-fill-with-key. |

Stills accept the common image formats via the same libav path (decode-once);
PNG/TGA with alpha produce NV12+A stills for the keyers, JPEG et al. produce
opaque NV12 stills.

---

## 6. The alpha path — NV12+A (summary; decision in ADR-0058)

### 6.1 Shape and boundary contract

A new **optional framestore payload extension**: NV12 planes + one
full-resolution R8 alpha plane — **2.5 B/px** (NV12's 1.5 + 1.0). It is used
**only** by media/graphics-fill sources that actually carry alpha; every
opaque tile stays plain NV12 (invariant #5 intact: we never materialize RGBA
per tile, and we never widen the common path). It is **not** bolted onto the
overlay stack: overlays are input-decoupled by design
([ADR-R008](../decisions/ADR-R008.md)) and must never grow a video-rate
dependency.

The plane carries **straight (non-premultiplied) coverage**. Premultiplication
cannot be done correctly in the gamma-coded Y′CbCr domain, so it happens
**in-shader, in linear light**, at exactly the step where the compositor
premultiplies `Cell.opacity` today — invariant #8's order is untouched, and
tile alpha simply becomes `cell.opacity × media_alpha` at the existing
premultiply point. Sources mastered premultiplied are un-premultiplied at
import (linear-domain divide, off the hot path) — import normalizes, the
kernel has exactly one form ([ADR-0058](../decisions/ADR-0058.md) pins the
kernel math). Externally supplied *shaped* (premultiplied) fills are the
separate fill+key case, declared via `KeySource.premultiplied`
([ADR-0056](../decisions/ADR-0056.md), which also pins how the same plane
serves linear/fill+key keying).

### 6.2 Why not RGBA, and the budget arithmetic (exact)

1080p frame = 1920 × 1080 = 2 073 600 px.

| Payload | B/px | Bytes/frame (1080p) | 3 s @ 60/1 (180 frames) |
|---|---|---|---|
| NV12 (opaque) | 1.5 | 3 110 400 | ≈ 0.56 GB |
| **NV12+A (this design)** | **2.5** | **5 184 000** | **≈ 0.93 GB** |
| NV12 + subsampled alpha (rejected) | 2.0 | 4 147 200 | ≈ 0.75 GB |
| RGBA8 (rejected) | 4.0 | 8 294 400 | ≈ 1.49 GB |

NV12+A is 62.5 % of RGBA's footprint *and* keeps the entire color pipeline on
the canonical NV12 path (one conversion story, invariant #5/#8). The 2 B/px
variant (alpha subsampled 4:2:0 like chroma) was considered and **rejected**:
keyer edges are exactly where quarter-resolution alpha is visible (fringing on
text/graphics edges), and the saving over full-res alpha is 0.18 GB on the 3 s
example — fidelity loses to a 20 % discount. At 2160p the same stinger is
20 736 000 B/frame ≈ 3.73 GB per 3 s — which is why the cap exists (§6.3).

### 6.3 Bounded mezzanine + decode-ahead ring (standing efficiency review)

- **Mezzanine-resident tier** (stingers, short graphics loops): the whole
  trimmed clip pre-decoded to NV12+A in a pool allocated at load. Admission:
  `duration_frames × bytes_per_frame ≤ per-asset cap` and library-wide budget
  (defaults pinned in [ADR-0058](../decisions/ADR-0058.md); the caps are
  config, the *existence* of caps is not). Playout = publishing pooled `Arc`
  frames on schedule — zero decode, zero allocation per frame.
- **Decode-ahead tier** (clips over the cap): a pool-allocated ring of N
  pre-decoded frames (illustratively 32 frames ≈ 158 MiB at 1080p NV12+A)
  filled by the player's decode thread ahead of the publish cursor. The ring
  absorbs decode jitter (PNG inflate spikes, disk latency) and crosses loop
  seams (the tail of lap *n* and the head of lap *n+1* coexist in the ring →
  gapless loops even when the underlying container re-opens).
- **Pools, never per-frame allocation; bounded, never growing** (safety
  rule #5). Underrun does not grow anything: the store holds last-good and the
  player reports underrun (§14).

---

## 7. Player transport semantics

### 7.1 Channel model and command path

Players are **pre-declared sources** (`media_players` config block, MVP: 2),
so each owns a supervised ingest thread from boot — load/cue change *what the
thread plays*, never spawn/teardown on the engine path. Transport commands
flow through the one apply path everything else uses: REST verb →
`Command::MediaTransport { player, op, .. }` on the bounded bus
(`try_submit`, 202 + operation id) → the CLI `CommandDrain` at the frame
boundary (which knows the current `Tick`, so it stamps tick-anchored
parameters) → a **bounded, conflated-latest-wins-per-op-class mailbox** the
player thread polls between frames. The engine never awaits the player; the
player never paces the engine (invariants #1/#10). Outcome + state events
publish drop-oldest (`media.player_state`,
[ADR-RT008](../decisions/ADR-RT008.md)).

Player state machine (pure value type, deterministic, property-tested —
house style):

```
Idle ── load(asset) ──▶ Loading ──▶ Cued(primed) ── play(start_tick) ──▶ Playing
                                      ▲    │ ▲                            │  │
                                      │   seek │ pause/resume ◀──────────┘  │
                                      │        ▼                            ▼
                                    stop ◀── Paused                EOF ──▶ per eof_policy:
                                                                   Holding | Playing(loop) |
                                                                   Black | Off
```

### 7.2 The timeline rule (load-bearing)

`read_at` latches the newest entry with `at ≤ tick.pts` and ages freshness by
`tick.pts − at` (`tile.rs:400-417`). The boot-time file path stamps
*source-relative* times (`publish_time = pts − first_pts`,
`pipeline.rs:6227-6246`) — fine at boot, wrong for a player cued at minute 40.
**Players stamp output-anchored times:**

```
publish_at(k) = anchor_pts + k × frame_period        (exact rationals)
anchor_pts    = output media time of start_tick      (play / play-on-take)
```

monotonic across loop laps (`anchor` accumulates one trimmed-clip-length per
lap). Consequences, all free: latch-on-tick maps frame k to exactly one output
tick (frame-accurate start, §7.3); loops never publish backwards media time
(the ring stays well-ordered); the freshness ladder measures real playout lag,
so a healthy player reads `LIVE` and a wedged one honestly ages.

### 7.3 Cue, play, pause, frame-accurate start

- **`load`** — bind an asset; open the container (or attach the mezzanine);
  state `Loading`.
- **`cue [frame]`** — `Demuxer::seek` to at-or-before the in-point
  (`demux.rs:549` — keyframe-bracketed), decode-discard to the **exact**
  in-point frame, publish it once, hold. Cue completes when
  `TileStore::is_primed` (`tile.rs:311`) — the same bit the switcher's
  WARM-ON-ARM gate reads ([ADR-P007](../decisions/ADR-P007.md)), so "cued" in
  the player API and "safe to take" in the switcher are one fact. The cued
  frame is real picture in the store: PVW shows it immediately.
- **`play [start_tick]`** — anchor per §7.2 and start the
  `PtsWallClock`-paced publish loop. Omitted `start_tick` = next tick after
  the drain. Command latency cannot smear frames: stamps derive from
  `start_tick`, not from when the mailbox was read, and the publish loop may
  publish slightly ahead (the store ring holds 256 entries; latch-on-tick
  aligns).
- **play-on-take** — the switcher state machine (not the operator) issues
  `play` with `start_tick = take_tick − pre_roll_frames` when a take involves
  this player (stinger, or a cued clip armed on a bus with roll-clip
  semantics — de-facto industry practice). Validation of the window is
  [ADR-0055](../decisions/ADR-0055.md)'s (§9).
- **`pause` / resume** — pause stops the clip cursor and keeps the picture: the
  player heartbeat-republishes the held frame via `publish_arc`
  (`tile.rs:253`, an `Arc` clone, no copy) with advancing stamps, so the tile
  reads `LIVE`-and-frozen rather than aging into `NO_SIGNAL`. Resume
  re-anchors the pacer (the verified re-anchor path) and continues stamping
  from the pause point.
- **`seek frame`** — pause-seek-cue composition; while seeking, the store
  holds the last frame (invariant #2 — never black).
- **`stop`** — halt and re-cue to the in-point (picture = in-point frame).
  Unload only via `load` of another asset or explicit eject.
- There is deliberately **no separate clear-to-black transport verb**: an
  operator clears a channel by ejecting it (the store then follows the cell's
  slate policy) — the `black` EOF policy (§7.4) provides the terminal
  black/transparent frame for automated ends.

### 7.4 EOF policy

Per-player, defaultable per-asset: `eof_policy ∈ { hold_last_frame | loop |
black | auto_off }`.

- **`hold_last_frame`** (default) — exactly today's **BUILT** behavior
  (`HoldForever`, `pipeline.rs:5575-5579`): the tile freezes on the final
  frame. The de-facto industry default for clip players.
- **`loop`** — §7.5.
- **`black`** — publish one terminal frame (opaque assets: black; alpha
  assets: **fully transparent**, which on a keyer means the key cleanly
  vanishes) and hold it.
- **`auto_off`** — publish the terminal transparent/black frame **and** report
  `Ended` in `media.player_state`; the switcher state machine treats the
  channel as off-air-safe (a DSK fed by it drops per
  [ADR-R011](../decisions/ADR-R011.md)'s keyer-loss policy; an aux/bus
  selection falls to the configured slate). The player itself never mutates
  bus state — it reports, the switcher decides (one state machine owns the
  buses).

Underrun is **not** EOF: a starved decode-ahead ring holds last-good and
reports `underrun`; the EOF policy fires only on true end-of-asset (§14).

### 7.5 Loop

The **primary loop path is in-place** ([ADR-0057](../decisions/ADR-0057.md)
Decision 4): clean EOF with `loop` policy seeks back to the in-point on the
open container (`Demuxer::seek(in_point)`, `demux.rs:549`, + decoder flush)
and keeps decoding — zero re-open cost, same stamping rules. Stamps stay
monotonic via the lap offset (§7.2); `PtsWallClock`'s backwards-PTS re-anchor
(`:6205-6208`) absorbs the PTS discontinuity. A **failed seek** falls back to
a clean re-open through the **verified reconnect bracket**: the looping
player is marked live-like, so clean EOF re-enters `ingest_loop`'s bracket
(`pipeline.rs:5561-5590`) with the healthy-streak reset giving zero backoff
after a clean lap. Honest caveat (fallback leg only): a cold container
re-open can exceed one frame period — the tile then *holds* (never glitches)
for the gap. Gapless guarantees come from the decode-ahead ring crossing the
seam (§6.3) — mezzanine-resident assets are perfectly gapless by construction.

---

## 8. Stills

A still is the degenerate clip: decode **once** at load (via the same libav
open — PNG/TGA/JPEG all decode on this path), publish, hold. The EOF-hold
precedent proves the semantics end-to-end today (**BUILT**: finite file →
thread ends → `HoldForever` shows the frame forever). Two refinements:

- **Freshness honesty:** a one-frame publish ages through the tile ladder into
  `NO_SIGNAL` (picture intact under `HoldForever`, but state badges and the
  fault detector would report a dead source). The player heartbeat
  (`publish_arc` re-stamp, §7.3-pause) keeps an intentionally-held still
  `LIVE`. Cheap (an `Arc` clone per cadence period), and it keeps
  [ADR-MV006](../decisions/ADR-MV006.md)'s derived tally + monitoring truthful.
  Final pinning in [ADR-0057](../decisions/ADR-0057.md).
- **Alpha stills** (PNG/TGA with alpha) publish NV12+A frames (§6) — these are
  the MVP DSK fill: a lower-third card with real transparency keyed by the
  downstream keyer ([ADR-0056](../decisions/ADR-0056.md)), composited
  premultiplied in linear light.

Stills bound directly to a layout cell (no transport needed) may use the
`Still` source kind (§15) instead of occupying a player channel — same decode
path, no transport surface.

---

## 9. Stinger usage contract (transition detail in ADR-0055)

A stinger is a **media transition**: a full-canvas alpha clip played over the
transition while the background cuts/mixes underneath at the moment the clip
covers. This brief owns the *asset + player* half; the *transition* half
(arming, progress, degradation) is [ADR-0055](../decisions/ADR-0055.md).

The contract between them, validated at **arm time** (never mid-take):

- Asset: `has_alpha` (verified, §4.2), canvas-resolution + output-cadence
  conform (§3), `trigger_point_frames` declared.
- Parameters (all integer frames at the output cadence):
  `pre_roll`, `clip_duration` (= trimmed asset length), `trigger_point`,
  `mix_rate`; **validated `trigger_point + mix_rate ≤ clip_duration`** — a
  hard cut under the cover is the degenerate `mix_rate = 1` frame case.
- Player: the referenced channel must be `Cued(primed)` for the arm to report
  ready (WARM-ON-ARM); the take issues play-on-take with
  `start_tick = take_tick − pre_roll` (§7.3).
- Playout is mezzanine-resident (§6.3): a stinger take performs **zero
  decode** on the hot path; the engine samples NV12+A frames like any tile and
  the transition's progress stays a pure `f(tick.index)` — a slow disk
  cannot stretch a transition (invariant #1). A stinger that starves while
  covering **demotes to a plain mix on the original schedule — never a frozen
  cover** (§14 / [ADR-R011](../decisions/ADR-R011.md)).

Stinger audio (the clip's embedded whoosh) routes per §11 with
[ADR-0059](../decisions/ADR-0059.md)'s transition-window ramps.

---

## 10. Graphics fills for keyers

A keyer needs a **fill** (picture) and a **key** (coverage)
([ADR-0056](../decisions/ADR-0056.md): `KeySource { fill, key: Option,
premultiplied }`). Alpha media collapses the pair: one NV12+A source is
fill-and-key in a single store — the R8 plane *is* the key signal, multiplied
in at the verified insertion point before the premultiplied linear-light
`over`. Explicit two-source fill+key (separate fill and key feeds, e.g. from
an external graphics renderer) remains expressible — the GPU already binds
per-tile texture-array layers, so a key source is one more layer reference —
but library media never needs it. Stills with alpha (§8) are the MVP fill;
clip fills (animated lower thirds, bugs) are the same path at video rate, with
the decode-ahead ring (§6.3) bounding their cost.

---

## 11. Audio of media

A player's embedded audio is **ordinary program-bus audio** — no new mixer
concepts:

- Per-player audio decode (the **BUILT** per-source pattern: decode →
  resample to the canonical 48 kHz stereo → `AudioStore`), routed onto the
  `ProgramBus` exactly as every source is today (`pipeline.rs:1960-1973`).
  Gains/mute/AFV ride [ADR-0059](../decisions/ADR-0059.md)'s audio control
  seam — the player adds **no** second audio control path.
- **One transport, two readers.** The production path opens separate libav
  contexts per elementary consumer; a player's video and audio readers
  therefore share **one** transport state (anchor, lap offset, pause flag) and
  both derive stamps from it — video per §7.2, audio sample budgets via
  `SampleClock` (`crates/multiview-audio/src/cadence.rs:36`, exact `1001`-aware
  rationals). Stamps from shared state, never per-thread wall clocks: the two
  readers cannot drift.
- **Transition interaction (summary; detail in ADR-0059):** play-on-take
  brings the player's strip up under the same equal-power `GainRamp` window as
  the video transition — `ramp_frames = SampleClock::total_at(s + T) −
  SampleClock::total_at(s)`, the exact cumulative-sample delta, never the
  naive `T × samples_per_tick` product ([ADR-T015](../decisions/ADR-T015.md)
  §5); a stinger's audio plays at unity for the clip duration
  while the background strips crossfade at the trigger window; `auto_off`/EOF
  ramps the strip down (never a hard digital cut — pop-free is the bus's
  existing guarantee, `repoint_crossfade`, **BUILT**).
- Media audio joins the bus **pre-loudnorm**, so the program loudness
  controller (BS.1770-based, **BUILT**) governs it like any source — imported
  promos at hot mastering levels do not blow the program.

Audio-only assets (beds, ROT) are post-MVP (§15): same library, a player
variant publishing only an `AudioStore`.

---

## 12. Protocol posture

- **v1 control = the native API only**: REST verbs + realtime events
  ([ADR-W021](../decisions/ADR-W021.md)/[ADR-RT008](../decisions/ADR-RT008.md)),
  `POST /api/v1/media/players/{id}/load|cue|play|pause|stop|seek`, assets CRUD +
  import at `/api/v1/media/assets`. Long-lived WS with snapshot-then-delta
  and stable string ids keeps the surface controller-friendly
  (control-surface contract per
  [production-switcher](production-switcher.md)). Examples are IPv6-first:
  `curl -X POST 'http://[2001:db8::10]:8080/api/v1/media/players/mp1/cue'`.
- **Semantics align with the open CasparCG AMCP precedent** (open project):
  cue shows the in-point frame; play-on-take; hold-on-last-frame;
  background-load with auto-play-on-foreground-end ("auto-chain") as the
  post-MVP cue tier. Citing semantics, not implementing AMCP wire protocol in
  v1.
- **VDCP and AMP are out of scope**
  ([ADR-0057](../decisions/ADR-0057.md)): both are RS-422-era protocols for
  driving *external* broadcast playout servers from automation (VDCP's spec is
  privately held; AMP is a vendor TCP protocol, IANA port 3811). Multiview's
  players are internal sources, not an external playout server; nothing in
  the repo references either protocol today. Revisit only on a concrete
  customer interop demand, as an adapter in front of the native API — never
  as the native surface.
- Frame-batched multi-op control (cue player + arm transition + take in one
  frame-coordinated batch) comes from the switcher's batch semantic — the
  openly-documented obs-websocket `SERIAL_FRAME` batch precedent maps onto the
  existing frame-boundary drain ([ADR-W021](../decisions/ADR-W021.md)).

---

## 13. Efficiency (standing review)

| Cost | Where it lands | Budget posture |
|---|---|---|
| Probe + validation decode | Import, control plane | Seconds per asset; bounded parallelism; zero engine impact. |
| Import-transcode | Import, control plane | Minutes-class worst case (ProRes-rate I/O); 202 + progress events; bounded parallelism. |
| Stinger mezzanine | Load time, RAM | Exact: `frames × 2.5 B/px × px` (§6.2 table); per-asset cap + library budget enforced **before** allocation; pool-allocated, freed on unload. |
| Decode-ahead ring | Cue/playout, RAM + 1 CPU decode thread | Fixed N-frame pool (≈ 158 MiB at 32 × 1080p NV12+A); refilled off-hot-path; underrun → hold, never grow. |
| Clip playout decode | Player ingest thread | = one file-source decode (already budgeted ingest class); intra alpha codecs are multithread-cheap, I/O-heavy (ProRes ≈ 330–500 Mbit/s reads). Admission-accounted like any decode ([ADR-E001](../decisions/ADR-E001.md) Mpix/s budgets). |
| Mezzanine playout | Player thread | `Arc` publish per frame period — no decode, no copy, no allocation. |
| Stills | Load once | One frame; heartbeat = an `Arc` clone per cadence period (atomics, no copy). |
| NV12+A compositing | Engine tick | +1 R8 texture fetch + one multiply per covered pixel for alpha tiles only; opaque tiles unchanged (invariant #5 common path untouched). Kernel cost pinned with the CPU-oracle/GPU-SSIM pattern in [ADR-0058](../decisions/ADR-0058.md). |
| Player audio | Bake consumer | One more bus strip — the mixer is already N-strip. |

Hot-path total for the flagship case (stinger take): **zero decode, zero
allocation, one extra alpha-blended tile** for `clip_duration` frames — the
transition window only.

---

## 14. Resilience ([ADR-R011](../decisions/ADR-R011.md) tie-in)

- **The engine never waits for media.** Players publish; the engine samples
  (`read_at`) — a wedged player is indistinguishable from a stalled camera:
  the tile holds last-good and rides the state ladder (invariant #2,
  `read_at`, `tile.rs:421-444`). Chaos gate: wedge a player thread mid-take; the
  transition completes on schedule from held frames (the MP-1 wedge-a-consumer
  proof pattern, applied to a producer).
- **Underrun ≠ EOF.** Ring starvation on an ordinary (non-stinger) player
  holds last-good + raises `media.player_state: underrun` (+ a health warning
  with remediation, invariant #2); the EOF policy (§7.4) fires only at true
  end-of-asset. A **stinger** that starves *while covering* is different
  ([ADR-R011](../decisions/ADR-R011.md) §4): the transition engine **demotes
  to a plain mix on the original schedule** — never a frozen cover frame —
  emitting `transition.degraded { reason: stinger_underrun }`; progress stays
  `f(tick)` and the program never stalls.
- **Import failure honesty.** Probe/transcode failures produce per-asset
  typed `problem+json` with remediation; the asset is `failed`, never
  half-`ready`; nothing engine-side ever depends on an import in flight
  ([ADR-0057](../decisions/ADR-0057.md)).
- **Keyer/stinger feed loss.** `auto_off`/`Ended`/player-death surfaces to the
  switcher state machine, which drops the affected keyer to off-air-safe
  (configurable) — the *switcher* decides bus consequences; the player only
  reports.
- **Crash recovery.** Player threads sit in the supervised reconnect bracket
  (**BUILT**) — a libav fault re-opens with backoff while the tile holds; the
  transport state machine re-cues to its last commanded state, honestly
  reporting `recovering`.
- **Deterministic tests.** Transport timing under `ManualTimeSource`;
  property tests on the pure transport machine (cue/play/seek/loop/EOF
  sequences never emit non-monotonic stamps); golden trigger-window
  validation vectors.

---

## 15. Config & API summary (decisions live in the ADRs)

Schema ([ADR-0057](../decisions/ADR-0057.md) /
[ADR-M012](../decisions/ADR-M012.md), the additive routing-block precedent —
`#[serde(default)]` on the `non_exhaustive` root, internally-tagged unions,
per-item `validate()` + document-level cross-refs):

- `media_library`: `root` (storage dir), `assets[]` (§3 declared fields;
  probed fields are mirrored read-only, never authored).
- `media_players[]`: id, default asset/policies, audio routing defaults.
- `SourceKind` additions (`non_exhaustive`, additive): `Still { path | asset }`
  for transport-less stills; a media-player binding kind referencing a player
  channel id. The existing `File { path }` stays untouched (legacy direct
  tiles); a both-populated ambiguity is structurally impossible because the
  new kinds are distinct tags ([ADR-0010](../decisions/ADR-0010.md)).
- **Desired state only** in config: assets, players, defaults. Live transport
  state (state machine phase, position-in-frames, current asset) is
  engine-owned, mirrored via `media.player_state` events + connect-time
  snapshot ([ADR-RT008](../decisions/ADR-RT008.md)); REST mirrors are
  read-only.

API ([ADR-W021](../decisions/ADR-W021.md)): assets CRUD + 202 import
operations; player bare verbs (`load|cue|play|pause|stop|seek`) with
`Idempotency-Key`, correlated outcome events (new `CorrKey` arms — the Route*
no-correlation mistake is not repeated); position surfaces as a conflated
~30 Hz lane at most (clients can derive position client-side from
`start_tick` + rate between events). Capability-matrix rows land with
[ADR-M012](../decisions/ADR-M012.md): load/cue/play/pause/stop/seek are
**Hot** (Class-1 — store writes + state machine, no engine reconfiguration);
asset import is a control-plane operation (no apply class — it never touches
the engine).

---

## 16. MVP boundary & phasing

**MVP** (aligned with the switcher MVP,
[production-switcher §18](production-switcher.md)):

- Library: stills + clips; import probe/policy/decode-verification;
  NV12+A stills (PNG/TGA) for the 2 DSKs.
- Players: **2 channels**; load/cue/play/pause/stop/seek; loop;
  play-on-take; EOF policy (all four); output-anchored timeline +
  frame-accurate start; embedded audio → program bus with AFV.
- No import-transcode (reject-with-remediation only), no stinger transitions
  (the *transition* is [ADR-0055](../decisions/ADR-0055.md) Wave 3; the
  NV12+A substrate ships with the DSK stills so Wave 3 lands on proven
  plumbing).

**Post-MVP:**

- Stinger clip transitions (mezzanine playout, §9 contract) — Wave 3.
- Import-transcode (HEVC-alpha conversion, stinger conform) (§4.3).
- Auto-chain cue (background-load, AMCP-precedent semantics).
- Audio-only assets + audio-bed player variant.
- Animated clip fills for keyers at video rate (substrate ships earlier;
  productized once keyer Wave 2 lands).
- Playlists (ordered multi-clip rundowns over player channels — builds on
  auto-chain cue) and clip-end automation (a macro triggered on
  `media.player_state: Ended`) — not designed in this pass.
- External automation interop (VDCP/AMP adapters) — on demand only (§12).

---

## 17. References (open/published only)

- FFmpeg documentation — codec/decoder capabilities (ProRes, `qtrle`,
  PNG/TGA, `libvpx-vp9` alpha side-data decode; HEVC alpha-layer status).
- CasparCG AMCP protocol documentation (open project) — cue/play/load
  semantics precedent.
- obs-websocket protocol documentation (open project) — frame-boundary
  request-batch precedent (`SERIAL_FRAME`).
- RFC 9457 — Problem Details (import error surface).
- ITU-R BS.1770 — loudness measurement (program-bus normalization the media
  audio inherits).
- De-facto industry practice (labelled as such in-text): library/player
  separation, cue-shows-first-frame, roll-clip-on-take, hold-on-last-frame
  default, stinger trigger-point delivery metadata.
