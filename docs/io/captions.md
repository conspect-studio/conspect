# Multiview — Native Caption / Subtitle Ingest

Multiview ingests captions **from the source stream itself** — every way they arrive — and treats them
like an audio track: a **`CaptionTrack`** discovered during demux, decoded **in the same demux pass**
(no second open, no sidecar staging for stream captions), normalised onto the one internal nanosecond
timeline, **sampled** into a per-tile last-good **cue store**, and burned in by the existing overlay
renderer. An external SRT/WebVTT sidecar file stays as **one** of the supported forms, not the primary
path.

> **The one principle that governs everything (same as inputs):** captions are **sampled** into the
> output, never allowed to **pace** or **block** it. A stalled, absent, wrong-page, or corrupt caption
> stream degrades to *"no cue on this tile right now"* — it can **never** stall the output clock or
> back-pressure the engine. See [conventions §5 invariants 1, 2, 3, 10](../architecture/conventions.md),
> [io/inputs.md](inputs.md), and [ADR-0019](../decisions/ADR-0019.md) /
> [ADR-R007](../decisions/ADR-R007.md).

Related canonical references:

- Architecture: [conventions §3, §5](../architecture/conventions.md) (crate map, invariants).
- Deep briefs: [Resilience & A/V](../research/resilience-and-av.md) ·
  [Streaming Gotchas §5, §7](../research/streaming-gotchas.md) · [Core Engine](../research/core-engine.md).
- Sibling I/O docs: [io/inputs.md](inputs.md) (source/track/rendition selection) ·
  [io/outputs.md](outputs.md) (discrete caption passthrough).
- Decisions: [ADR-0019](../decisions/ADR-0019.md) (this design) · [ADR-R007](../decisions/ADR-R007.md)
  (libass burn-in + discrete passthrough) · [ADR-R008](../decisions/ADR-R008.md) (overlay stack).

---

## 1. The unified caption cue model

Every form of caption — broadcast teletext, DVB bitmap, embedded CEA-608/708, HLS WebVTT, a sidecar
file, MP4 timed-text, ASS — decodes down to **one of two cue shapes** on the shared media timeline.
This is the single model the cue store holds and the overlay renderer consumes; the decoder that
produced it is irrelevant downstream.

```text
CaptionCue =
  | Text   { start_ns, end_ns, lines: [String], region?: CueRegion }
  | Bitmap { start_ns, end_ns, rgba: ImageBuf, rect: CueRect }      // premultiplied alpha
```

- **Text cue** — markup-stripped display lines (top→bottom). This is the existing
  `multiview_overlay::subtitle::Cue { start, end, lines }`, which this design **reuses and extends** with
  an optional `region` (anchor + alignment) so 608/708 roll-up/pop-on placement survives where the
  source carried it. Teletext, CEA-608/708, WebVTT, SRT, ASS (text path), and mov_text all land here.
- **Bitmap cue** — a premultiplied-RGBA image plus its placement `rect` relative to the source frame.
  DVB subtitle (and, optionally, libass-rasterised ASS) land here. Premultiplied alpha matches the
  compositor's linear-light blend ([ADR-C003](../decisions/ADR-C003.md),
  [ADR-R008](../decisions/ADR-R008.md)); the rect is rebased to the tile's display rectangle by the
  renderer so a bitmap cue scales with its tile.
- **Times are absolute `MediaTime` (ns)** on the *source's normalised* timeline — the same timeline its
  video/audio rides (invariant #3). `end_ns > start_ns` is enforced at decode; an open-ended cue (608
  roll-up with no explicit clear) is closed by the next cue or by a bounded max-on-screen duration.

The model is a `#[non_exhaustive]` adjacently-tagged enum (`#[serde(tag = "kind")]`, never `untagged`)
so it round-trips TOML/JSON and a future cue shape (e.g. a styled-region 708 window) can be added
without breaking consumers. **No `dyn Any`, no untyped blobs.**

---

## 2. A `CaptionTrack` is discovered like an audio track

A caption stream is **not** a special case bolted onto the side; it is a track the source advertises,
discovered the same way audio tracks are, and selectable per source. Discovery differs by container:

| Source shape | How the track is discovered | Track identity |
|--------------|-----------------------------|----------------|
| **MPEG-TS** (broadcast, SRT, UDP, `ts`) | Walk the **PMT**: subtitle elementary streams carry `stream_type` + descriptors — `teletext_descriptor` (0x56) → teletext (with page numbers), `subtitling_descriptor` (0x59) → DVB-sub, ISO-639 language descriptors give the language tag. | PID + page (teletext) / PID (DVB-sub) + language |
| **Embedded CEA-608/708** | **Not a PID** — carried in the H.264/HEVC SEI / MPEG-2 user-data of the *video* stream and surfaced by libav as `AV_FRAME_DATA_A53_CC` side data on decoded video frames. Discovered by sniffing the chosen video stream for A53 side data; CEA-608 has up to 4 fields (CC1–CC4), 708 has services. | video stream + field/service |
| **HLS** | Read the **master playlist** `#EXT-X-MEDIA:TYPE=SUBTITLES` group(s); the chosen video variant's `SUBTITLES="group-id"` attribute names the rendition. The rendition is its **own** segment URI list (WebVTT segments) that must be opened in addition to the video variant. | `GROUP-ID` + `NAME` + `LANGUAGE` |
| **MP4 / MOV** (`file`) | A `tx3g`/`mov_text` subtitle track in the moov box. | track id + language |
| **Sidecar** | An operator-supplied `.srt` / `.vtt` path — no stream discovery, the file *is* the track. | file path |

Discovery produces a list of `CaptionTrackInfo { id, kind, language?, pages?, codec }` per source, the
caption analogue of the audio track list in [io/inputs.md §7](inputs.md#7-track--rendition-selection).
It is surfaced read-only (`GET /api/v1/sources/{id}/captions`) so the UI can populate a selector, and
it is **advisory**: teletext pages and 608 fields are often only *known to exist* once data flows, so
discovery reports "expected/declared" and the live cue store reports "actually seen".

---

## 3. The six input forms → which libav decoder handles each

This FFmpeg 7.1 build links **libzvbi + libass + dvbsub** and exposes every decoder needed. The
caption decode lives in **`multiview-ffmpeg`** behind the `ffmpeg` feature (the decoders are already
linked — **no new `cargo deny`-relevant dependency**), wrapped in safe RAII like the rest of the libav
surface.

| # | Form | Arrival | libav decoder | Cue shape | Notes |
|---|------|---------|---------------|-----------|-------|
| 1 | **DVB teletext** | broadcast TS PID | `libzvbi_teletextdec` (`dvb_teletext`) | Text | Page-addressed (commonly 801; intermittent). Selector picks the page; `auto` follows the PMT's teletext descriptor / first subtitle page. |
| 2 | **DVB subtitle** | broadcast TS PID | `dvbsub` (`dvb_subtitle`) | **Bitmap** (RGBA + rect) | Cues are pre-rendered regions; carry a `clear` (empty) segment that ends the on-screen cue. |
| 3 | **CEA-608 / 708** | H.264/MPEG-2 video **SEI / user-data** | `cc_dec` fed from `AV_FRAME_DATA_A53_CC` side data (or the `lavfi` `subcc` path) | Text | **Not a separate PID.** Common on HLS/TS/SDI-origin. 608 **field** selectable (CC1–CC4); roll-up/pop-on handled, open-ended cues bounded. **708 service text is _not_ decodable by the linked `cc_dec`** (it parses line-21 608 and discards DTVCC service blocks), so a 708-*service* selector is **refused at decoder construction** (`FfmpegError::UnsupportedCaptionChannel`) rather than yielding a silently cue-less decoder — fall back to a 608 field, teletext, or a sidecar. |
| 4 | **HLS WebVTT rendition** | master `SUBTITLES` group | `webvtt` | Text | Must **resolve the master's SUBTITLES group**, then decode the rendition's own WebVTT segments — not the default video variant. |
| 5 | **Sidecar SRT / WebVTT** | operator file | parsed natively (`subtitle::CueTrack`) or `subrip`/`webvtt` | Text | The existing `--subtitles` path. Pure-Rust parse already exists; kept as one form. |
| 6 | **mov_text / tx3g · ASS/SSA** (bonus) | MP4 box / TS or file | `mov_text`; `ass` (+ libass for styled rendering) | Text (or Bitmap via libass) | Cheap to add since the decoders are linked; ASS styled rendering is behind the `libass` feature ([ADR-R007](../decisions/ADR-R007.md)). |

Forms **1–4** are *stream* captions decoded in-pipeline; **5** is the sidecar; **6** is opportunistic.
Forms 1, 2, 3, 4, and 6-from-stream all decode inside the **same demux** that already feeds the tile's
video — **one open, no second pass, no sidecar staging** — except the HLS WebVTT rendition (form 4),
which is a *separate* media playlist by HLS design and so needs a second, isolated reader for that
rendition (still one extra demux for the *track*, not a re-open of the program).

---

## 4. In-pipeline decode in the same demux

The per-source decode thread ([io/inputs.md §1](inputs.md#1-per-source-pipeline),
`multiview-cli/src/pipeline.rs`) already pulls packets and decodes video. Caption decode rides alongside
it on that same thread's demux, so there is **one `av_read_frame` loop per source**:

```text
av_read_frame ─┬─▶ video packet  ─▶ video decoder ─▶ NV12 frame ─▶ (A53_CC side data? → cc_dec)
               ├─▶ teletext pkt  ─▶ libzvbi_teletextdec ─┐
               ├─▶ dvbsub pkt    ─▶ dvbsub             ──┤
               └─▶ (other PIDs ignored)                  ├─▶ CaptionCue (text|bitmap)
                                                         │      rebased to source ns timeline
   HLS only: SUBTITLES rendition reader ─▶ webvtt ───────┘      │
                                                                ▼
                                                   per-tile CueStore (last-good, bounded)
   OutputClock ─▶ compositor samples CueStore at out_pts ◀──────┘  (sampled, never pacing)
```

- **Selected captions only.** A caption decoder is instantiated **only when a tile actually shows
  captions** for that source (selector ≠ `off` and the tile is on-canvas). Subtitle packets for a
  deselected PID are dropped at demux; no decoder is created. This is the efficiency lever (§7).
- **Off the hot path.** Caption decode happens on the **input** thread, never on the output clock
  thread. The compositor only ever *reads* the cue store at `out_pts` (a non-blocking
  `arc_swap`-style load); it never waits on a decoder (invariants #1, #10).
- **CEA-608/708 special case.** Because A53 captions are side data on *video* frames, they cost
  effectively nothing extra to extract during the video decode that already happens — `cc_dec` runs on
  the side-data bytes only when the embedded-CC selector is active.

---

## 5. Cue timing: rebased to ns, held last-good, intermittency handled

Caption timestamps go through the **same normalisation and rebasing** as the source's video/audio
(invariant #3, [io/inputs.md §4](inputs.md#4-timestamp-normalisation--rebasing),
[ADR-T003](../decisions/ADR-T003.md)): unwrap 33-bit TS PTS, genpts fallback, monotonic guard,
re-anchor on discontinuity, then add the source's rebase offset so a cue's `[start_ns, end_ns)` lands on
the *same* timeline as the frame it belongs to. **Never** float fps; carry ns / exact rationals.

The cue store is the caption analogue of the per-tile **last-good-frame store**
([multiview-framestore](../research/resilience-and-av.md)), with the cases captions actually present:

| Case | Behaviour |
|------|-----------|
| **Current cue** | `CueStore::active_cue(out_pts)` returns the cue whose window contains the tick; renderer burns it in. |
| **No current cue** (gap between cues) | Returns `None`; the tile shows **no caption** — the normal, common state. Not an error, not a stall. |
| **Track absent / wrong page** | Decoder produces nothing (e.g. teletext page 801 selected but the source never sends it). Store stays empty; tile shows no caption; the [caption-presence probe](../research/resilience-and-av.md) may raise a *caption-loss* alarm after a timeout, but **output never falters**. |
| **Intermittent** (broadcast captions come and go) | The store holds the **last** cue only while its window is open; once `out_pts >= end_ns` it expires to `None`. Hold-last-good is bounded by the cue's own end time, not indefinitely, so a stale caption never "sticks". |
| **Open-ended cue** (608 roll-up, no explicit clear) | Closed by the next cue or a bounded max-on-screen duration so it cannot linger forever. |
| **Late / out-of-order cue** | Dropped if its window already ended before the current tick (drop-too-late, like the jitter buffer). |

Because "now" is an **injected `MediaTime`**, the whole store + expiry logic is a pure state machine —
property/golden-vector testable with no real clock and no live broadcast (see §9).

---

## 6. The per-source caption selector

Selection is a per-source attribute owned by the **Source** (like audio-track selection;
[ADR-M004](../decisions/ADR-M004.md), [io/inputs.md §7](inputs.md#7-track--rendition-selection)). It is
an adjacently-tagged enum (`#[serde(tag = "mode")]`, never `untagged`) so it is robust across TOML/JSON
and unambiguous:

```toml
[cells.source.captions]
mode = "auto"            # auto | off | teletext_page | track | embedded_cc | sidecar
burn_in = true           # render into this tile (default true when a cue source resolves)
language = "eng"         # optional preference used by `auto`

# mode = "teletext_page"
# page = 801

# mode = "track"         # a discovered CaptionTrack id (DVB-sub PID, HLS rendition NAME, mov_text track)
# id = "pid:0x812"

# mode = "embedded_cc"   # CEA-608/708 from the video SEI/user-data
# field = "cc1"          # cc1..cc4 (608, decodable to text); 708 service:N is
#                        # refused — the linked cc_dec discards DTVCC service blocks

# mode = "sidecar"
# path = "/media/news.srt"
```

| `mode` | Resolves to | Form |
|--------|-------------|------|
| `auto` | First usable track by preference: embedded-CC → teletext (first page) → DVB-sub → HLS WebVTT, honouring `language` when set. | 1–4 |
| `off` | No caption decoder is created; no burn-in. The cheap default for tiles that don't need captions. | — |
| `teletext_page` | `libzvbi_teletextdec` on the teletext PID, decoding the given `page`. | 1 |
| `track` | A discovered `CaptionTrack` id (DVB-sub PID, HLS rendition, mov_text track). | 2, 4, 6 |
| `embedded_cc` | `cc_dec` over the video A53 side data, the given 608 `field` (CC1–CC4). A 708 `service:N` is refused (the linked `cc_dec` has no 708 service text decode). | 3 |
| `sidecar` | An external SRT/WebVTT file (existing `--subtitles`). | 5 |

`auto` is intentionally conservative: it never promotes a *declared-but-silent* track over one actually
producing cues, and a tile with no resolvable caption source simply shows none.

---

## 7. Where it lives & efficiency

| Concern | Crate / module | Feature |
|---------|----------------|---------|
| Caption **decode** (FFI: libzvbi/dvbsub/cc_dec/webvtt/mov_text/ass wrappers) | `multiview-ffmpeg` (owns all raw libav FFI; `unsafe = deny` + `// SAFETY:`) | `ffmpeg` |
| `CaptionCue` / `CaptionTrack` **types** | `multiview-core` (or reuse/extend `multiview-overlay::subtitle::Cue`) — pure, no FFI | — (always built) |
| **Discovery** (PMT walk, HLS SUBTITLES-group resolve, A53 sniff) + per-tile **cue store** + selector wiring | `multiview-input` (`unsafe = forbid`) | `ffmpeg` for stream forms; sidecar/parse is pure |
| **Burn-in** (active cue → text runs / bitmap quad) | `multiview-overlay` renderer → `multiview-compositor` overlay sub-pass (existing) | `overlay` (+ `libass` for styled ASS) |
| Sidecar **parse** (SRT/VTT) | `multiview-overlay::subtitle::CueTrack` (already committed, pure) | — |
| Caption-presence **alarm** | `multiview-overlay::caption_probe` (already committed, pure) | — |

Efficiency rules (consistent with [conventions invariants 6, 9](../architecture/conventions.md)):

- **Decode only when shown.** No caption decoder exists for a tile whose selector is `off` or that is
  off-canvas; subtitle packets for unselected PIDs are dropped at demux. A 64-tile wall with captions
  on the 2 PGM tiles decodes 2 caption streams, not 64.
- **Bounded cue store.** The store holds at most the current cue (text) or current bitmap per tile;
  bitmap cues are size-bounded and freed on expiry (drop-oldest, never grow — [ADR-E005](../decisions/ADR-E005.md)).
- **Pure model always compiled.** `CaptionCue`, the cue store state machine, the selector schema, the
  SRT/VTT parser, and the HLS-master `SUBTITLES`-group parser are pure Rust, built and tested with **no
  FFmpeg and no GPU** — the FFI decoders are the only feature-gated part.
- **Discrete passthrough** to outputs (where the format allows) is governed separately by
  [ADR-R007](../decisions/ADR-R007.md) / [io/outputs.md](outputs.md); this doc is the **ingest** side.

---

## 8. Invariants upheld

- **#1 output-clock** — caption decode runs on input threads; the output clock samples a non-blocking
  cue store and emits a frame every tick whether or not a cue is present.
- **#2 last-good** — the cue store is the caption sibling of the last-good-frame store; "no cue" is a
  first-class, graceful state.
- **#3 timing** — cue PTS is normalised/unwrapped/rebased onto the source's ns timeline; never float fps.
- **#10 isolation** — a stalled, absent, wrong-page, or flooding caption stream cannot back-pressure or
  stall the engine; bounded drop-oldest, the engine never awaits a caption decoder.

---

## 9. Testing (TDD on controlled inputs — never live broadcast)

All caption tests run against **synthesised** media built with the FFmpeg CLI, so they are
deterministic and never depend on a live broadcast caption being present:

- **DVB-sub / teletext in TS** — mux a known SRT into an MPEG-TS subtitle stream (`-c:s dvbsub` /
  teletext), then assert the decoded `CaptionCue`s match the known text/windows.
- **CEA-608** — burn known captions into H.264 with `-a53cc 1` and assert `cc_dec` over the A53 side
  data recovers them on the right field.
- **HLS WebVTT** — generate a master playlist with a `SUBTITLES` group + WebVTT segments and assert the
  group resolves and the cues decode.
- **Sidecar SRT/VTT** — the existing pure parser tests (`multiview-overlay/tests/subtitle.rs`).
- **Pure state machine** — property/golden tests for cue-store expiry, no-cue gaps, wrong-page (empty),
  intermittency, and timeline rebasing, driven by an injected `MediaTime` (no FFmpeg, no clock).

Tests assert on real decoded values (no tautologies); the cue-store/expiry logic carries property tests
per [ADR-G002](../decisions/ADR-G002.md). The held-out acceptance suite uses additional synthesised
fixtures.

---

## 10. Related ADRs

| ADR | Topic |
|-----|-------|
| [ADR-0019](../decisions/ADR-0019.md) | Native multi-form caption ingest: unified cue model + per-tile sampled cue store (this design) |
| [ADR-R007](../decisions/ADR-R007.md) | Subtitle ingest → libass burn-in (off hot path) + discrete passthrough |
| [ADR-R008](../decisions/ADR-R008.md) | Overlay rendering: layer stack, premultiplied alpha, input-decoupled |
| [ADR-T003](../decisions/ADR-T003.md) | Per-input timestamp normalisation (applies to cue PTS) |
| [ADR-M004](../decisions/ADR-M004.md) | Track-mapping ownership (Source owns selection) |
| [ADR-E005](../decisions/ADR-E005.md) | Bounded, drop-oldest working set (applies to the cue store) |
| [ADR-C003](../decisions/ADR-C003.md) | Composite in linear light with premultiplied alpha (bitmap cues) |
