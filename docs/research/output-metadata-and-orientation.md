# Multiview — Output Metadata & Orientation

**Area:** Output / Config / Control / Web (encode + mux surface; layout-manager-sourced orientation)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0088](../decisions/ADR-0088.md) (output metadata model — per-transport, capability-gated), [ADR-0089](../decisions/ADR-0089.md) (output orientation — canvas-rotate vs display-rotation tag, per-output, flows to program + preview).
**Extends:** [ADR-M002](../decisions/ADR-M002.md) (`EncodeProfile` + per-output transcode model), [ADR-C006](../decisions/ADR-C006.md) (always tag output then verify with ffprobe), [ADR-M010](../decisions/ADR-M010.md) (per-output outbound stamping precedent), [ADR-M005](../decisions/ADR-M005.md) (live-apply classification).
**Relates to:** [display-out](display-out.md) (`Head.orientation` for scanout heads — a *different* axis), [preview-subsystem](preview-subsystem.md) (preview must reflect output orientation + metadata), [layout-and-config](../templates/layout-and-config.md) (the layout manager is the orientation source).
**Backlog:** `OMETA-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> The operator asks for two things on the output side: **(1)** full metadata control for every output, *where the transport and codec actually support it* (MPEG-TS service name/provider, language, title; HLS timed metadata; RTMP `onMetaData`; file/container tags); and **(2)** an **orientation** setting that **flows to program and preview** so a downstream player (VLC was the example) orients the picture correctly. The second has two genuinely different mechanisms that must not be conflated: **rotate the real pixels** (the canvas is produced rotated; every consumer sees rotated pixels) versus **emit a rotation *tag* / display matrix** the player applies at render time (the bytes are unrotated; the metadata says "present me at 90°"). This brief specifies both, makes them per-output, sources orientation from the **layout manager**, capability-gates everything, and keeps it all on the encode/mux side so the output clock (#1) and isolation (#10) are untouched.

---

## 0. Headlines

1. **Two operator asks, one output-side surface.** Per-output **metadata** and per-output **orientation** are both properties applied at **encode/mux time**, never on the output-clock path. They extend the existing `Output` model and the `EncodeProfile` of [ADR-M002](../decisions/ADR-M002.md); they sit alongside color tagging ([ADR-C006](../decisions/ADR-C006.md)) as a third "describe the output truthfully, then verify" surface.

2. **Metadata is per-transport and capability-gated.** There is no universal metadata bag. MPEG-TS carries an **SDT** (service name/provider) + **PMT** descriptors (per-stream language ISO-639) + optional timed `KLV`/`ID3`; **HLS** carries **timed metadata** (in-band ID3 in MPEG-TS PES, or out-of-band `EXT-X-DATERANGE` in the playlist) + playlist-level fields; **RTMP/FLV** carries **`onMetaData`** (an AMF script-data object: width/height/framerate/codec ids/title where the endpoint reads it); **file/container** outputs carry container tags (MP4/MOV `udta`/`ilst`, Matroska tags). The model declares operator *intent* and the output layer applies **only the subset the chosen transport+codec support** — never silently drops, never invents.

3. **Orientation is two distinct mechanisms — ship both, make the operator choose.**
   - **(a) Display-rotation TAG (the VLC case):** the canvas pixels stay landscape; the output **declares a rotation** (MP4 `tkhd` display matrix / `displaymatrix` side-data; HEVC/H.264 SEI where applicable) so a tag-aware player rotates on render. **Zero pixel cost; not honored by every player.**
   - **(b) Rotate-the-canvas (real pixels):** the composited canvas is produced at the rotated geometry, so **every** consumer (tag-blind players, capture, NDI, a passthrough display) sees correctly-oriented pixels. **One quarter-turn sampling cost; universal.**
   The operator picks per output; the default is the tag where the transport carries one and **rotate-pixels** where it does not (so a tag-blind sink is never wrong on a tag-less transport — on a tag-transport a tag-blind player still shows landscape, the residual surfaced in preview + Open question 1).

4. **Orientation source = the layout manager; it flows to program AND preview.** The orientation is an output-level setting set in the layout/output manager. Program (the real encoded output) and **preview** (the program-preview tap and the per-output preview) must render the **same** orientation the operator selected — preview honesty is non-negotiable per [preview-subsystem §6](preview-subsystem.md) (a preview that lies about orientation is a defect). When the tag path is used, the preview overlay states "tagged 90° — player-applied" so the operator knows tag-blind players will differ.

5. **Do not confuse three rotation axes.** This brief's **output orientation** is per *network/file/display output*. It is distinct from **per-cell tile rotation** (`QuarterTurn` on `Cell.rotation`, `crates/multiview-core/src/layout.rs:32`, `:144` — a tile sampling transform inside the canvas) and from **display-head orientation** (`Orientation` on `Head`, `crates/multiview-core/src/layout.rs:71`, `:460`; `HeadConfig.orientation`, `crates/multiview-config/src/wall.rs:35` — portrait *scanout* of a KMS head, [display-out](display-out.md)). The output-orientation surface **reuses the existing `QuarterTurn`/`Orientation` core types**; it does not add a fourth rotation vocabulary.

6. **Encode-once is preserved (#7) — with one honest exception.** Metadata and the **tag** path add *no* extra encode: tags/SDT/`onMetaData` are mux-time fields fanned to all sinks sharing a rendition. **Rotate-pixels at 90°/270° changes canvas geometry** (W↔H swap), so two outputs that want *different* physical orientations of the same program are, by definition, different renditions and cost a second composite/encode — the same "pixels differ ⇒ separate encode" rule [ADR-M002](../decisions/ADR-M002.md)/[ADR-E003](../decisions/ADR-E003.md) already states. 180° and a pure tag never change geometry.

7. **Everything is capability-gated and verified.** A requested field/orientation that the transport+codec cannot express is surfaced as a typed capability result **before** apply (the [ADR-M002](../decisions/ADR-M002.md) `CapabilityReport` precedent), and the **applied** result is read back from the bitstream/container with the same ffprobe assertion gate [ADR-C006](../decisions/ADR-C006.md) already mandates (extended to assert `displaymatrix`/SDT/`onMetaData` presence + values, not just color).

---

## 1. Output metadata — per-transport capability

### 1.1 Where it lives — `OutputMetadata`, declarative intent

`Output` (`crates/multiview-config/src/schema.rs:683`, internally tagged `#[serde(tag = "kind")]`, `#[non_exhaustive]`) gains an **additive** `metadata: Option<OutputMetadata>` on each variant (proposed), exactly as `audio: Option<OutputAudio>` and `gpu_pin: Option<DevicePin>` are additive today. `OutputMetadata` is a **declarative intent** struct — the operator's wishes — not a per-transport union:

```toml
[[outputs]]
kind = "hls"
path = "/hls/multiview"
codec = "h264"

[outputs.metadata]
title         = "Studio A Multiview"     # service/title (transport maps it where it can)
provider      = "Aperim Newsroom"         # SDT provider_name on TS; ignored elsewhere
language      = "eng"                      # ISO-639-2 (PMT lang descriptor / container tag)
service_id    = 1                          # DVB service_id / TS program number (optional)
description   = "Gallery confidence feed"  # container comment / SDT free-text where carried
# timed metadata opt-ins (HLS/TS): emit ID3 PES + EXT-X-DATERANGE for cue/now-playing
timed         = { id3 = true, daterange = true }
```

This is operator *intent*; the **output layer projects it onto the transport** (§1.3) and reports what landed. Nothing here is required — an absent block means "the engine's honest defaults" (a derived title from `Output::label()`, `schema.rs:1053`, and the canvas language/color tags).

`OutputMetadata` is a config-layer type in `multiview-config` (serde + the existing `schemars`/`garde` validation, [layout-and-config §9](../templates/layout-and-config.md)); the runtime carries it on the per-output `EncodeProfile`/sink descriptor so the **mux** stage applies it. It does **not** belong in `multiview-core` (no FFI/encode there) and it does **not** belong on the canvas (it is per-output, not per-program — two outputs of the same program legitimately carry different service names).

### 1.2 Per-transport capability table (what each transport can actually carry)

| Transport | Carrier | Fields it can express | Standard (verified) |
|---|---|---|---|
| **MPEG-TS** (SRT-as-TS, file `.ts`, HLS TS segments) | **SDT** (service_name, provider_name, service_type) + **PMT** descriptors (ISO-639 language per ES) + PAT program number | title→service_name, provider, language (per-stream), service_id→program_number; free-text in SDT | FFmpeg `mpegtsenc` `-metadata service_name=/service_provider=`; DVB SI SDT + PMT descriptors |
| **HLS** | fMP4 init/`.ts` segments + **playlist tags** + **timed metadata** | per-track **language** (fMP4 init box or TS PMT) — note there is **no** universal playlist "container title"; `EXT-X-DATERANGE` (out-of-band timed events, mandatory `ID`+`START-DATE`); in-band **ID3** PES (TS) | Apple HTTP Live Streaming Metadata spec (ID3 PES); HLS `EXT-X-DATERANGE` (RFC 8216 / W3C media-timed-events) |
| **RTMP / FLV** | **`onMetaData`** AMF script-data | width, height, framerate, video/audio codec id, duration, **and endpoint-read title/extras** | FLV `onMetaData` (AMF ECMA-array); Enhanced-RTMP v2 FOURCC codec ids |
| **MP4 / MOV file** | `moov/udta` / `ilst` (iTunes-style) + per-track lang | title, comment, language, encoder; **display matrix in `tkhd`** (orientation, §2) | ISO-BMFF / QuickTime `udta`; MP4 `tkhd` matrix |
| **Matroska / WebM file** | Tags element + `Track/Language` | title, language, free tags | Matroska tags spec |
| **NDI** | runtime-loaded SDK metadata frames + sender name | sender/source name; connection metadata (vendor convention) | NDI is runtime-loaded, nominative only ([conventions §7](../architecture/conventions.md)); never bundle the SDK |
| **RTSP** | SDP session-level fields (`s=`/`i=`) | session name/info; **not** rich per-program metadata | RFC 8866 SDP `s=`/`i=` |

**The model declares intent; the transport defines the projection.** A `provider` set on an HLS-fMP4-only deployment has nowhere to land except the playlist/container title — the capability report says so; we do not pretend it became a DVB SDT.

### 1.3 The projection rule and the capability report

For each `(Output, OutputMetadata)` the output layer computes an **`OutputMetadataPlan`**: for every requested field, one of `{Applied(target), Dropped(reason), Defaulted}`. `Dropped` is surfaced (it is the honest analogue of [ADR-M002](../decisions/ADR-M002.md)'s capability gating and the [ingest-all-streams isolation](../research/managed-devices.md) "warn, never silently drop" doctrine) — e.g. `language="eng"` on a bare RTMP `onMetaData` endpoint that defines no language key is `Dropped(reason="rtmp onMetaData has no language field")`. The plan is returned from the dry-run plan endpoint ([ADR-M005](../decisions/ADR-M005.md)) **before** apply, so the operator sees what will and will not land. Validation rejects only contradictions (e.g. a DVB `service_id` outside `1..=65535`), never an unsupported-but-harmless field — that degrades to `Dropped`, explicitly.

### 1.4 Timed metadata (the live, per-tick axis) — sampled, never pacing

HLS/TS timed metadata (ID3 PES + `EXT-X-DATERANGE`) is the one **time-varying** metadata surface: "now playing", a cue marker, a program-boundary event. The two carriers are **not** interchangeable: in-band **ID3 PES** is a free-form per-sample channel, whereas an `EXT-X-DATERANGE` is a *playlist date-interval* tag with mandatory `ID` + `START-DATE` (wall-clock, requiring `EXT-X-PROGRAM-DATE-TIME` semantics on the variant) — it models a bounded interval/event, not arbitrary per-sample PES, so implementers must not treat it as a generic ID3 substitute. It is generated **off the hot path** from the control plane / switcher cue stream and injected at the **mux** stage on the bake/segment thread — it **rides the output as a side stream stamped from the output tick counter** (inv #3), it never gates a frame. A late or absent cue means *no* ID3 frame this segment, never a stalled segment (inv #1). This is the same isolation posture as recording ([ADR-0037](../decisions/ADR-0037.md)) and audio meters ([ADR-0059](../decisions/ADR-0059.md)): a best-effort producer feeding a bounded, drop-capable injection point. SCTE-35 splice carriage is **out of scope here** (it is the [broadcast-cues](broadcast-cues.md) brief's domain); this brief covers descriptive/now-playing timed metadata only.

### 1.5 Relationship to color tagging (C006)

Color tags ([ADR-C006](../decisions/ADR-C006.md)) and this metadata are **the same discipline applied to different fields**: set explicitly at encode/mux, then verify from the bitstream/container with ffprobe; never assume the muxer's default. The C006 ffprobe gate is **extended** to also assert the requested metadata fields landed (SDT service_name present, `onMetaData` title present, container `language` set) — one verification pass, more assertions. Color stays the canvas's single source of truth (it is a *program* property); descriptive metadata is a per-*output* property. They do not overlap.

---

## 2. Orientation — rotate the canvas vs a display-rotation tag

### 2.1 The two mechanisms, stated plainly

| | (a) Display-rotation **TAG** | (b) **Rotate the canvas** (real pixels) |
|---|---|---|
| What changes | Metadata only: container/bitstream says "present at θ"; pixels are unrotated | The composited pixels are produced rotated; bytes are correct as-is |
| Player behaviour | Tag-aware players (VLC, ffmpeg, browsers via `tkhd`) rotate on render; **tag-blind players show it unrotated** | **Every** player/sink shows it correctly, no cooperation needed |
| Geometry | Unchanged (W×H of canvas) — but player presents H×W for odd turns | **W↔H swapped** for 90°/270° (a new rendition geometry) |
| Cost | **Zero pixel cost**; one mux/SEI field | One quarter-turn **sampling** transform (lossless, GPU-cheap — the same class as the per-cell `QuarterTurn`, `layout.rs:28`) |
| Carriers | MP4/MOV `tkhd` display matrix + `displaymatrix` side-data; H.264/HEVC SEI display-orientation where the player reads it | n/a — it is the pixels |
| Transports that can carry the **tag** | MP4/MOV/HLS-fMP4 (matrix), some via SEI; **MPEG-TS/RTSP/NDI have no robust rotation tag** → must use (b) | all |

**The VLC case is (a):** the operator wants the *file/stream* to tell the player to rotate, without re-rendering. VLC reads the MP4 `tkhd` matrix (and ffmpeg's `displaymatrix` side data) and orients accordingly — confirmed industry behaviour. So for an MP4/fMP4/HLS output, the **tag** is the cheap, correct answer. For an MPEG-TS or RTSP or NDI output — which carry **no** dependable rotation tag — the only correct answer is **(b) rotate the pixels**, because there is nothing to tell the player.

### 2.2 The per-output orientation surface

Each `Output` variant gains an **additive** `orientation: Option<OutputOrientation>` (proposed). It reuses the existing core vocabulary — it does **not** invent a new rotation enum:

```toml
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[outputs.orientation]
turn = "cw90"          # reuses core QuarterTurn: none | cw90 | cw180 | cw270
mode = "auto"          # auto | tag | pixels
# flip = "none"        # none | horizontal | vertical (reuses Cell.transform flip vocabulary)
```

- `turn` is a `QuarterTurn` (`crates/multiview-core/src/layout.rs:32`): `None`/`Cw90`/`Cw180`/`Cw270`, with the existing `degrees()` and `swaps_axes()` helpers (`layout.rs:47`, `:59`) — already unit-tested (`crates/multiview-core/tests/layout_broadcast.rs:159-170`). No new type.
- `mode`:
  - **`tag`** → emit the display-rotation tag only (mechanism a); **rejected at validation** if the transport carries no rotation tag (TS/RTSP/NDI) — explicit, never a silent no-op.
  - **`pixels`** → produce a rotated canvas (mechanism b); always available.
  - **`auto`** (default) → `tag` where the transport carries one (MP4/MOV/HLS-fMP4), else `pixels`. This makes the **tag-capable-player-correct** choice and stays **zero-cost** where supported; it is **pixel-correct only for transports without a robust rotation tag** (where it falls back to `pixels`). On a tag-transport a tag-blind player still sees landscape — that residual is surfaced in the preview overlay and Open question 1.
- `flip` reuses the `flip_h`/`flip_v` vocabulary already in the cell transform ([layout-and-config §4](../templates/layout-and-config.md): `[cells.transform] rotate/flip_h/flip_v`); flip is a pixel-only operation (no container "flip" tag exists), so a flip forces the `pixels` path.

`Orientation` (Landscape/Portrait on `Head`, `layout.rs:71`) is **not** this type — that is the scanout-head axis. We deliberately keep `OutputOrientation` (a `QuarterTurn` + mode) distinct from `Head.orientation` (a portrait/landscape scanout flag) because they mean different things and target different sinks; a future ADR could unify if they converge, but conflating them now would mis-model the display-out heads.

### 2.3 How each path is applied (the encode/mux seam)

- **Tag path (a).** No compositor change. At **mux init** the output sets the rotation:
  - MP4/MOV: write the `tkhd` 3×3 display matrix for θ (and, for remux/copy paths, attach `AV_FRAME_DATA_DISPLAYMATRIX`/`AV_PKT_DATA_DISPLAYMATRIX` side data) — ffmpeg's `-display_rotation`/`av_display_rotation_set` is the reference behaviour; players read it.
  - HLS-fMP4: same matrix in the init segment.
  - Optionally a H.264/HEVC SEI display-orientation message where a target player honors it (advisory; the container matrix is primary).
  - Verified post-mux with ffprobe (`side_data ... displaymatrix: rotation of -90.00 degrees`), the C006 gate extended.
- **Pixels path (b).** The output is a **rendition with a rotated geometry**. The compositor already owns lossless quarter-turn sampling for tiles (`QuarterTurn`, `layout.rs:28` — "a sampling transform, no resampling artefacts"); the output-orientation pixel path applies the **same** quarter-turn to the *whole canvas* into that rendition's encode target. For odd turns the encode target is H×W (swapped); the `EncodeProfile` ([ADR-M002](../decisions/ADR-M002.md)) for that output carries the swapped resolution. 180° is a same-geometry pixel flip (cheap, no swap). This is a distinct rendition ⇒ a separate composite/encode for that output (#7 honest exception, headline 6).

### 2.4 Invariant #8 (tag-not-convert) — explicit

The **tag path is the literal embodiment of #8/C006**: it labels the output's presentation without touching pixels, exactly as color tagging labels color without converting. The **pixels path** is a geometry transform that happens **inside the compositor's existing linear pipeline** — it changes *where samples land*, not the color order; the fixed color sequence (detect → expand → matrix → linearize → … → tag → verify, inv #8) is unchanged. Neither path reorders color. Both paths end at the same C006 verification gate.

---

## 3. Flow to program + preview

The operator's headline requirement: orientation set in the **layout manager** must flow to **program and preview**.

- **Source of truth = the layout/output manager.** Orientation is an output property edited where outputs are managed ([layout-and-config](../templates/layout-and-config.md) / the management surface). It is part of the config document, versioned and applied like any other output edit.
- **Program (the real output).** Program is the encoded output itself — applying the tag or producing the rotated rendition (§2.3) **is** "flowing to program". There is no separate program step.
- **Preview must reflect it — both taps.**
  - **Program-preview tap** ([preview-subsystem §1](preview-subsystem.md), the pre-encode canvas downscale): the *global program canvas* preview is a single shared, pre-encode tap and is **per-program, not per-output** — it cannot simultaneously reflect two outputs that chose different `OutputOrientation`s, so it shows the **unrotated program canvas** and per-output orientation honesty belongs to the per-output preview (next bullet). If/when an output-scoped pre-encode rendition preview is added, *that* per-output tap shows the rotated pixels on the **pixels** path, and on the **tag** path shows landscape pixels **with an explicit overlay label** "tagged θ — player-applied", because the pre-encode canvas is genuinely unrotated and a tag-blind player would see exactly that. Honesty over prettiness, per the [preview-subsystem](preview-subsystem.md) fidelity-label rule (`REAL ENCODED OUTPUT` vs `PRE-ENCODE CANVAS APPROX`).
  - **Per-output preview** ([preview-subsystem §1 OUTPUT scope](preview-subsystem.md), the real-encoded-packet tap decoded back): this **already** reflects reality — it decodes the real rendition, so a tagged output decoded by a tag-aware decoder shows rotated, and a pixel-rotated output shows rotated. The per-output preview is the truthful confirmation surface and needs no special-casing beyond carrying the orientation in its descriptor so the client can render the bounding box correctly.
- **Isolation (#10) preserved.** Preview reads existing frames/packets; nothing about orientation makes preview able to back-pressure the engine. The preview tap stays drop-oldest, read-only (preview-subsystem §2). A preview that cannot keep up freezes *its own* image; program orientation is unaffected.
- **Live-apply class.** Changing the **tag** (mode `tag`, or `auto` on a tag-transport) is a **mux-init** change: it is at worst a segment/keyframe-boundary reinit on that output — surfaced via the dry-run plan ([ADR-M005](../decisions/ADR-M005.md)); often Class-1-hot for a container that can re-emit an init segment, Class-2 where the muxer must restart. Changing **`turn` on the pixels path** changes the encode **geometry** (odd turns swap W↔H) → **Class-2** (a pinned-resolution change, [ADR-M002](../decisions/ADR-M002.md) — make-before-break). 180°/flip on the pixels path keep geometry → can be Class-1. The classifier states which before applying.

---

## 4. Capability gating — where transport + codec support it

The operator's qualifier — *"where transport and codec support"* — is load-bearing and is enforced structurally:

1. **Transport capability** (§1.2, §2.1 tables) is a static-but-honest matrix per `Output` kind: which metadata fields and which orientation mechanisms it can express.
2. **Codec/encoder capability** is the [ADR-M002](../decisions/ADR-M002.md) `CapabilityReport` (HAL-probed): e.g. a SEI display-orientation message is only meaningful for codecs/players that read it; a container display matrix is codec-agnostic. The plan never offers a field the negotiated encoder/muxer cannot write.
3. **The combined plan** (`OutputMetadataPlan` + an `OrientationPlan`) is returned from the dry-run plan endpoint **before** apply, listing `Applied`/`Dropped(reason)`/`Defaulted` per field and the chosen orientation mechanism + its live-apply class. The UI renders this as a per-output "what will land" panel (the [ADR-M002](../decisions/ADR-M002.md) session-budget-calculator precedent — show the operator the truth before commit).
4. **Post-apply verification** (the C006 ffprobe gate, extended) reads the **actual** bitstream/container and asserts the requested fields/matrix landed; a mismatch renders amber, not green, exactly as a color-tag mismatch does. "Requested ≠ delivered" is surfaced, never assumed away.

No field is ever silently dropped and no orientation is ever silently a no-op: an unsupported `mode = "tag"` on TS is a **validation error** (choose `pixels`), and an unsupported metadata field is a **visible `Dropped`** in the plan.

---

## 5. Efficiency budget (mem / cpu / gpu / io)

- **Metadata (all of §1):** negligible. SDT/PMT/`onMetaData`/container tags are bytes written once at mux init (or per-SDT-interval on TS — small, periodic, already part of the muxer's cadence). **No per-frame cost, no GPU, no extra encode** — fanned to all sinks of a rendition (#7 intact).
- **Timed metadata (§1.4):** one small ID3 PES / `EXT-X-DATERANGE` line per event, injected on the bake/segment thread from a bounded drop-capable queue — off the hot path; bounded memory; events drop under overload, never queue-grow (inv #10).
- **Orientation — tag path (§2.3a):** **zero** pixel/GPU/encode cost; one matrix written at mux init + side-data on copy paths.
- **Orientation — pixels path (§2.3b):** one **quarter-turn sampling pass** for that rendition. At 90°/270° it is a new geometry ⇒ a separate composite+encode for that output (counted against the [ADR-M002](../decisions/ADR-M002.md) session budget like any distinct rendition) — this is the one real cost and it is *only* paid when the operator chooses pixels on a transport that cannot tag (or explicitly forces pixels). 180° is a same-geometry pass (cheaper, no swap). The sampling itself is lossless and GPU-cheap (`layout.rs:28`), bandwidth-bound, not the binding constraint.
- **io:** metadata adds a few hundred bytes to mux init and a periodic SDT; timed metadata adds a small per-event stream. All bounded.

The efficiency standing review's answer: **prefer the tag path** (zero cost) wherever the transport carries one — which `mode = "auto"` does automatically — and only pay the pixel quarter-turn where a tag is impossible or the operator explicitly wants tag-blind-correct pixels.

---

## 6. Invariants honored (explicit)

- **#1 Output-clock — untouched.** Metadata and both orientation mechanisms are applied at **encode/mux**, never on the tick loop. The pixels path runs in the compositor's existing per-rendition render (already off the clock-emit path); the tag/metadata path is mux-init and a periodic SDT. Timed metadata is sampled from the control/cue stream and injected on the bake thread; a missing event never stalls a frame. `out_pts = f(tick)` is unchanged.
- **#10 Isolation — preserved.** The cue/now-playing producer feeding timed metadata is a best-effort, bounded, drop-oldest source (the [ADR-0059](../decisions/ADR-0059.md)/[ADR-0037](../decisions/ADR-0037.md) seam pattern); it never awaits or back-pressures the engine. Preview reflecting orientation stays read-only drop-oldest ([preview-subsystem §2](preview-subsystem.md)).
- **#7 Encode-once-mux-many — preserved, with the named exception.** Metadata + tag fan to all sinks of a rendition at zero extra encode. A 90°/270° **pixels** orientation is a different-geometry rendition (pixels genuinely differ) ⇒ a separate encode by the same rule that governs any res/bitrate-divergent output ([ADR-M002](../decisions/ADR-M002.md)/[ADR-E003](../decisions/ADR-E003.md)).
- **#8 Color order / tag-not-convert — honored.** The tag path *is* the tag-not-convert principle applied to orientation; the pixels path is a geometry transform inside the existing linear pipeline and reorders nothing. Both end at the C006 ffprobe verification gate.
- **#6 Decode-at-display-resolution — unaffected.** Output orientation is a post-composite output concern; ingest decode sizing is unchanged.
- **IPv6-first.** No new network surface here, but any examples of output endpoints lead IPv6 (`rtsp://[2001:db8::10]/multiview`), bind dual-stack `[::]`, bracket literals ([conventions §10](../architecture/conventions.md) / [ADR-0042](../decisions/ADR-0042.md)).

---

## 7. Dependency-ordered waves (each ships complete, no parked integration)

1. **Config + capability tables.** `OutputMetadata` + `OutputOrientation` on every `Output` variant (additive, `#[serde(tag="kind")]` round-trip); the per-transport capability matrices (§1.2/§2.1) as a typed function; validation (`service_id` range; `mode="tag"` rejected on TS/RTSP/NDI; `flip` forces pixels). Ship with the dry-run plan returning `OutputMetadataPlan`/`OrientationPlan`. *No runtime apply yet is allowed only if the same push wires the mux apply* — so this wave bundles with wave 2.
2. **Mux/tag apply + tag-path orientation.** Wire `OutputMetadata` into each muxer (TS SDT/PMT, HLS playlist/ID3, RTMP `onMetaData`, container tags); write the `tkhd` display matrix / `displaymatrix` side data for the tag path; extend the C006 ffprobe gate to assert the new fields/matrix.
3. **Pixels-path orientation.** Whole-canvas quarter-turn into a rotated rendition encode target; swapped `EncodeProfile` geometry for odd turns; Class-2 plan wiring (make-before-break) for geometry changes.
4. **Timed metadata.** ID3 PES + `EXT-X-DATERANGE` injection from the bounded cue stream on the bake/segment thread; chaos test (wedge the producer; segments keep closing).
5. **Preview + UI.** Per-output preview descriptor carries orientation; program-preview overlay label for the tag path; the "what will land" capability panel; SPA output editor controls (orientation picker + metadata fields), generated against the OpenAPI spec.

---

## Open questions

1. **`auto` default per transport — is "prefer tag where it exists" the right global default?** For MP4/HLS-fMP4 the tag is cheap and VLC-correct, but some embedded/STB players ignore the matrix and would show landscape. A per-deployment "assume tag-blind players" toggle that flips `auto` to `pixels` may be worth it. *Default proposed: prefer tag (zero cost), with the preview overlay warning; revisit if field players misbehave.*
2. **SEI display-orientation for TS/RTSP.** H.264/HEVC carry an optional display-orientation SEI. It *could* give MPEG-TS/RTSP a tag-like path, but player support is thin and inconsistent. *Proposed: ship container-matrix tags only; treat SEI as advisory/experimental, never the sole orientation carrier for TS/RTSP — those stay `pixels`. Needs a player-support survey before promotion.*
3. **HLS orientation when segments are MPEG-TS (not fMP4).** TS segments have no robust rotation tag, so a TS-segment HLS output's `auto` resolves to **pixels**, while an fMP4 HLS output's `auto` resolves to **tag** — a subtle per-variant difference. Is that acceptable, or should HLS always pick one mechanism for consistency? *Leaning: respect the segment container (fMP4→tag, TS→pixels) and state it in the plan.*
4. **NDI metadata depth.** NDI carries a sender name and (vendor-convention) connection/per-frame metadata via the runtime-loaded SDK. How much of `OutputMetadata` to map onto NDI metadata frames — and whether the operator's title/provider belong there — is deferred to the NDI integration owner; vendor-neutral, runtime-loaded, nominative only ([conventions §7](../architecture/conventions.md)). *Proposed v1: sender name only.*
5. **Free-text / arbitrary tag passthrough.** Should `OutputMetadata` carry an open `extra: map<string,string>` for container-specific tags (Matroska custom tags, extra `onMetaData` keys) the model doesn't name? Risk: it becomes an untyped escape hatch. *Proposed: a small, explicitly-named field set in v1; an `extra` map only if a concrete need appears, and only for file containers (never TS/RTSP).*
6. **Do output orientation and `Head.orientation` eventually unify?** A display-out head is portrait/landscape scanout; a network output is a `QuarterTurn`+mode. If display heads gain quarter-turn precision (or network outputs gain a portrait/landscape shorthand), one type could serve both. *Proposed: keep distinct now; flag for a future consolidation ADR if they converge.*

---

## Decision records

- [ADR-0088](../decisions/ADR-0088.md) — Output metadata model: per-output `OutputMetadata` intent projected onto per-transport carriers (TS SDT/PMT, HLS ID3/`EXT-X-DATERANGE`, RTMP `onMetaData`, container tags), capability-gated, verified at the extended C006 ffprobe gate. Extends [ADR-M002](../decisions/ADR-M002.md) + [ADR-C006](../decisions/ADR-C006.md).
- [ADR-0089](../decisions/ADR-0089.md) — Output orientation: per-output `QuarterTurn` + `mode` (canvas-rotate vs display-rotation tag), `auto` picks tag where the transport carries one else pixels, sourced from the layout manager, flows to program + preview, capability-gated and verified. Extends [ADR-C006](../decisions/ADR-C006.md) (#8 tag-not-convert) + [ADR-M002](../decisions/ADR-M002.md) (rendition geometry).
