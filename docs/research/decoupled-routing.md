# Per-stream decoupled routing + instant switching (the crosspoint model)

> Status: design brief feeding **ADR-0034**. Verification-hardened against the
> as-built code (file:line cited inline) and reviewed adversarially. Where the
> text *proposes* new surface it says so; where the code already does the thing,
> the code wins. **This brief builds on, and does not duplicate, ADR-0030**
> (multi-program / `ProgramSet` + `SourceRegistry`), **ADR-R004** (pin-params +
> seamless scene-graph swap / make-before-break), **ADR-R005/R006** (program bus
> + discrete tracks + R128), **ADR-0019/0024** (unified `CaptionCue`), and
> **invariant #11** (Class-1/Class-2 surfaced before apply). Read those first.

## 0. The problem the operator stated

> *Completely decouple inputs, layouts and outputs and allow instant switching
> of (1) which input feeds each cell of a layout and (2) which layout/input an
> output carries — the broadcast router + multiviewer crosspoint model.*

with the load-bearing refinement:

> *An input is **not** a single stream. It is a bundle of elementary streams:
> 1+ video, multiple audio tracks (each with a BCP-47 language + channel
> layout), subtitle/caption tracks, data (SCTE-35, KLV), timecode. Every one of
> these must be (a) **discovered**, (b) **visible** in model + API + UI, and (c)
> **independently routable** — audio/subtitle breakaway: take video from input A,
> audio track 2 from input B, subtitles from input C, simultaneously.*

So crosspoints are **per elementary stream** (`input.stream -> destination`),
not per input. The job is to lift the existing per-program binding key from
`input_id` to a stable `StreamEndpoint = (input_id, StreamKind, selector)`, and
make the three already-existing crosspoint maps (video cells, audio bus/tracks,
subtitle layers) **independently keyed** so breakaway falls out structurally.

## 1. The one mental model

- A **layout IS an ADR-0030 Multiview program**: one composite + one
  `OutputClock` fed by **three independently-keyed maps** — VIDEO
  (`cells[].source`), AUDIO (program bus + named tracks, ADR-R005), SUBTITLE
  (per-layer cue source). Inputs, layouts and outputs become **independent
  resources wired by per-stream crosspoints**.
- **Three bus tiers**, all already present in code:
  - **TIER-1 — input-stream bus.** The `SourceRegistry` facets (ADR-0030 §3/§4),
    but now **one store per elementary stream** (V1, A1..An, S1..Sn, D, TC) — not
    one per input. Each facet is independently `Arc`-clonable.
  - **TIER-2 — program bus.** Composite ⊕ `ProgramBus` mix ⊕ cue layer.
  - **TIER-3 — output bus.** Each `Output` selecting a program (all-streams) or
    per-kind streams (output-level breakaway).
  A **crosspoint** joins TIER-1 → TIER-2; an **OutputRef** joins TIER-2 → TIER-3.

Breakaway is **structural**: the three TIER-1→TIER-2 maps are keyed
independently, so nothing forces the audio/subtitle key to equal the video
cell's key. `video <- A`, `audio-track2 <- B`, `subs <- C` coexist on one
program with no special case.

## 2. As-built vs missing (what the code actually does today)

This section is the verified ground truth; the design only claims what survived
review. The `unwrap`/`expect`-free, file:line-cited facts:

| Capability | As-built today | Verified | Missing / to build |
|---|---|---|---|
| Per-stream demux enumeration | `Demuxer::streams()` returns **all** container streams with `language` from container metadata (`crates/multiview-ffmpeg/src/demux.rs:301`, lang at `:318`). | yes | The result is **discarded**: `multiview-input/src/libav.rs` consumes only `best_stream(Video)` and `.find()`s the one video row; audio/subtitle/data rows are dropped. The inventory is computed then thrown away. |
| Routable kinds | `MediaKind` is exactly `Video\|Audio\|Subtitle\|Other` (`crates/multiview-ffmpeg/src/convert.rs:29-41`); `best_stream` returns `None` for `Other` (`demux.rs:350-357`). | yes | No `Data\|Timecode` routing kind. SCTE-35 **is** already discovered as PIDs (`multiview-input/src/mpegts/selection.rs:42 scte35_pids`, full `scte/` module) — but **not as a routing kind**. **KLV has zero matches repo-wide.** `StreamKind` must SUPERSET `MediaKind`. |
| Demux snapshot lifetime | Open-time-only; **not refreshed mid-stream** (`multiview-input/src/param_probe.rs:7-11`). Container `index` is a one-time snapshot. | yes | Binding by `index` is fragile across re-probe / PMT-version bump / rendition reorder. Needs a stable kind-scoped id. |
| Frame-boundary apply seam | `run_inner` drains `control(&mut self.drive)` **between `clock.tick()` and `drive.compose()`** (`crates/multiview-engine/src/runtime.rs:411/420/424`). Bounded non-back-pressuring bus: `try_submit`→`Full`(503), `try_drain` (`command.rs:242/264`). | yes | The seam is real and never stalls the clock (inv #1/#10). But the **apply is not yet an O(1) re-point** — see below. |
| Video swap mechanism | Production `SwapSource` mutates the working config's `cell.source.input_id`, **re-solves the whole layout** (`config.solve_layout()` allocates, O(cells)), then `drive.set_layout()` revalidates (`multiview-cli/src/control.rs:152/170`, `multiview-config/src/lib.rs:341`). | yes | The claimed *O(1) map swap* is a **design target, not as-built**. Small N it is sub-µs; it degrades with large layouts/salvo storms (K commands = K re-solves/tick, no coalescing). **Build `rebind_cell` (O(1)) + coalesce + cap-per-tick.** |
| Video geometry on swap | Each source's `TileStore` frames are decoded/scaled **once** to the pixel size of the cell that bound it at build time (`pipeline.rs:554 cell_pixel_size`); the reference compositor places tiles 1:1 with **no resample** (`drive.rs:331-333`, dst rect from `tile.image.height()`). | yes | **Cross-geometry swap is visually broken**: a source decoded at cell-A's size painted into a different-sized cell-B clips/overdraws neighbours; an unbound spare falls through to **full-canvas** size (`pipeline.rs:555`) and smears the whole multiview. **Decode at a canonical size + scale-at-composite, or re-scale on swap.** |
| Warm target | Every declared `config.sources` entry gets a `TileStore` + decode thread at startup (`pipeline.rs:553-585`, `IngestRunner::start :2548-2566`); a swap to a declared source spins **no new decoder** (decode-once, inv #5/#7). `NoSignalPolicy::HoldForever` + `read_at` last-good means no black flash **for a primed target**. | yes | Startup prime-wait only gates **cell-bound** sources (`pipeline.rs:1217-1240`). An **unbound spare** held for a future take is decoding but never prime-gated; a reconnecting source returns `NoSignal` → slate/black flash on the take frame. **As-built `SwapSource` has no arm/prime-gate.** Build WARM-ON-ARM. |
| Audio program bus | `ProgramBus::tick()` pulls `samples_per_tick` on the output `SampleClock` and silence-fills (`crates/multiview-audio/src/program.rs:81`); `route_to_program(point, gain)` + `mix_program` sum gains (`mixer.rs:90/148-177`). | yes | **Lip-sync is NOT structural in the current wiring:** `bus.tick()` is called once per **dequeued** video frame in `consumer_main` (`pipeline.rs:1915-1916`), and the live policy is `DropOnOverload` (`pipeline.rs:151-156`) — a dropped video frame skips a `bus.tick()` → audio trails video by the dropped samples. **Drive `bus.tick()` from the output tick index, not surviving frames.** |
| Audio re-point primitive | `add_source` only **appends** a route (`program.rs:62-66`); `route_to_program` only mutates gain/bool (`mixer.rs:90`). | yes | **There is no method to swap which `Arc<AudioStore>` a channel reads.** Build `ProgramBus::repoint(RoutePoint, Arc<AudioStore>)`. Also: a warm-but-unread store sits at `read_frame=0` while drop-oldest pushes `base_frame` ahead (`store.rs:160-168`) → on take it returns silence and climbs from frame 0. **Make the cursor absolute-tick-indexed / seek to the live edge on re-point.** |
| Audio cross-fade | Expressible *in principle* (per-strip gain + summed mix). | yes | The transition-ramp state machine does **not** exist (zero `cross.?fade`/`gain_ramp` matches in audio routing). A single per-tick **scalar** gain over a ~20-40 ms tick block cannot render a 10 ms equal-power fade — it needs an **intra-block (per-sample) gain envelope**. Build it. |
| Subtitle cue store | Unified `CaptionCue` decoded in-demux; per-tile sampled cue store keyed by source id (`multiview-cli/src/captions.rs`, ADR-0019/0024). | yes | Generalise the per-source `CaptionSelector` to a **per-layer** `Arc<CueStore>` re-point. |
| Egress restamp/splice primitives | `RestampAccumulator` is real, pure, default-build (`crates/multiview-output/src/restamp.rs:40/82/106`): `dts'=max(raw+offset,last_dts+1)`, `rebase` re-anchors offset at the seam. `is_idr` is a pure header parser gating a **true RAP** (H.264 nal==5; HEVC 19/20 rejecting CRA21; AV1 KEY+show_frame), **not** `AV_PKT_FLAG_KEY` (`crates/multiview-ffmpeg/src/idr.rs:77`). Property/unit tests pass. | yes | **Reusable verbatim** for an egress program→program splice — but they have **zero non-test callers** today, and there is **no `GuardedPacketSource`** and **no egress splicer**. Build GP-7's sibling. |
| Encode-once fan-out | The pipeline encodes the canvas **once** then fans to N sinks (inv #7 holds for a **static** output set). | yes | **Two `EncodedPacket` types.** `fanout.rs` (`EncodedPacket{data:Arc<[u8]>}`, `PacketRouter`) is **dead/unwired** — its sole consumer is `rtsp_server`, which is **skipped** (`pipeline.rs:3053-3057`). The **production** fan-out is `Vec<SyncSender<multiview_ffmpeg::EncodedPacket>>` doing a **per-sink `packet.clone()`** (ref-counted AVPacket buf, `pipeline.rs:1947`), **not** one shared `Arc`. There is **no runtime sink-mover** (`register`/`deregister` are test-only) and **no `ProgramSet`** in the engine. **Converge the types, land ProgramSet, build the mover.** |
| SCTE-35 continuity | `RestampAccumulator` handles **video/audio only** (`restamp.rs:14`); `fanout::PacketKind` is `{VideoKeyframe,VideoDelta,Audio}` (`fanout.rs:53-67`); `scte/splice35.rs` is a **one-way decoder** (no re-serialiser); `pts_adjustment` is read at `splice35.rs:197` only for monitoring. | yes | A re-stamping passthrough that shifts the video `offset` **must shift the SCTE-35 `pts_adjustment` by the same offset** (SCTE-35 2023r1, below) or ad cues misfire. **No SCTE restamp target exists.** Build the data-PID sibling + a `pts_adjustment` re-serialiser (CRC-32/MPEG-2 helper already at `mpegts/crc.rs`). |
| Salvo (atomic take) | `Command` is `#[non_exhaustive]` with `ArmSalvo`/`TakeSalvo`/`CancelSalvo` (`command.rs`); the engine salvo arm/take/cancel value machine exists (`multiview-engine/src/salvo.rs`, all-or-nothing, idempotent). Config `Salvo` carries `sources:Vec<SourceRecall{cell,input_id}>` + `tally` + `umd` (`multiview-config/src/salvo.rs:59`); `SourceRecall` is **per-input**, not per-stream. | yes | Extend **three** surfaces in lockstep: config `Salvo`, engine `SalvoChange`/`SalvoBatch`, and the OpenAPI mirror (`SalvoDoc`/`*RecallDoc`). All `#[non_exhaustive]` + serde-tagged → additive. |
| Realtime | `Topic` is `#[non_exhaustive]` with `Inputs/Tiles/Outputs/Tally/Layout` etc. (`multiview-events/src/topic.rs:16`); wait-free `LatestState::latest()` snapshot + drop-oldest broadcast (`multiview-engine/src/isolation.rs`). | yes | No `Routing`/`Streams` topic, no `input.streams` event, no `StreamInventory` type. Add `Topic::Routing` + deltas on the **same** wait-free path. Snapshot is one conflated blob (`state.rs:50`) — fold routing/inventory **into it**, don't invent a per-topic typed snapshot. |
| Output identity | Outputs are nested in a program and carry **no operator id** (addressed by mount/path/url, `schema.rs label()`). | yes | Add a stable operator `id` + `OutputRef{output->program}` (ADR-0030 §5.2 adds `program:String` to commands). |

**Cross-feature dependency (the one):** `output <- program` routing **depends on
ADR-0030 (`ProgramSet` + `SourceRegistry`) landing.** Today the engine is
single-program and `ProgramSet`/`SourceRegistry` are **designed, not coded** (zero
Rust matches; ADR-0030 is *Proposed*). Until they land, the output crosspoint
degenerates to the single `"main"` program (the desugar default) — exactly
backward-compatible. Everything else (inventory, per-stream `RouteMatrix`,
audio/subtitle breakaway, GP-6/GP-1/GP-7 primitives) builds on already-coded
surfaces.

## 3. Stream discovery — `StreamInventory` (new model surface)

Converge the **three** existing discovery surfaces into one persisted, typed,
stably-keyed list. None of these is re-derived here — each already exists and is
referenced:

1. **Generalise the libav path.** Stop discarding non-video `StreamParams`
   (`demux.rs:301`); emit **all** rows. Map `kind=Other` through
   `codec_id_name` (already at `demux.rs:307`) to disambiguate `Data(Scte35)` vs
   `Data(Klv)` vs `Timecode` — `MediaKind` alone cannot.
2. **TS/SRT.** Fold in `mpegts::pmt::ElementaryStream` (stable `pid`) +
   `SelectedProgram{video_pid,audio_pids,scte35_pids}` (gives SCTE-35 PIDs) +
   PMT version. Add typed ES-descriptor decoders to `mpegts/descriptor.rs`
   (ISO_639 `0x0A`, subtitling `0x59`, teletext `0x56`, AC-3 component) to fill
   audio/subtitle language+role that libav metadata misses. **Reconcile** the two
   SCTE-35 discovery surfaces (general demux `codec_id` vs PSI `scte35_pids`) so a
   TS input neither double-lists nor misses SCTE-35.
3. **HLS.** Fold in `MasterPlaylist`'s AUDIO + SUBTITLES `MediaRendition{group_id,
   name,language,default,forced}`. The audio side needs the same resolver the
   subtitle side (`hls.rs pick_subtitle`) already has.

```text
StreamInventory{ input_id, probed_at, streams: Vec<InventoryStream> }
InventoryStream{ stream_id: StableStreamId, kind: StreamKind,
                 codec, language: Option<Bcp47>, default: bool, payload }
```

`StreamKind` **SUPERSETS** `MediaKind`:

```text
StreamKind =
  | Video    { w, h, fps, color }
  | Audio    { layout, sample_rate, lang, title, default }
  | Subtitle { family, lang, forced, default }
  | Data     { Scte35 | Klv }          // KLV (ST 0601) is NEW — discovery + passthrough
  | Timecode { TcSource }              // Ltc | Vitc | AtcRp188 | Generated + ST2110-40 RP188
```

`StableStreamId` is **kind-scoped** so a crosspoint survives re-probe /
PMT-version bump / rendition reorder (container `index` is volatile,
`param_probe.rs:7-11`):

- **Hard keys** (genuinely stable): TS = PID; HLS = `group_id + name`; declared
  unique language.
- **Soft key** (heuristic fallback): general = `kind + ordinal + codec + lang`
  hash. **Fold the container `title` tag into the hash** (libav exposes it the
  same way `language` is read) so same-codec/same-language tracks get a real
  discriminator. **Enforce HLS `NAME` non-empty** by synthesising
  `group_id + ordinal` when absent (`hls.rs:206-207` currently `unwrap_or_default`
  to `""`, which can collide; RFC 8216 NAME-uniqueness is not enforced by the
  parser).
- **Tag each `InventoryStream` with a key-stability TIER** (hard/soft) and surface
  it in the API + UI, so a soft-keyed crosspoint is a **known, operator-visible
  reorder risk** rather than a silent mis-route after a re-probe.

The new `language: Option<Bcp47>` must **parse/validate** the raw container string
(`demux.rs:318` reads whatever the container stored — it does **not** normalise to
BCP-47/ISO-639) and fall through to the ES-descriptor / `MediaRendition.language`
sources when libav's is absent.

> NB this is distinct from `multiview-config/src/probe.rs`, which is
> **content-fault** probing (black/freeze/silence/loudness on decoded essence),
> not a stream inventory.

## 4. Per-stream routing model — `RouteMatrix` + `StreamEndpoint`

A first-class per-program **`RouteMatrix`**:

```text
RouteMatrix {
  video:    Map<CellId,                     StreamEndpoint>,   // IS today's cells[].source
  audio:    Map<BusChannel | TrackName,     StreamEndpoint>,   // IS AudioRouting.routes + OutputAudio
  subtitle: Map<LayerId | OutputTrackId,    StreamEndpoint>,   // IS the per-source CaptionSelector, per-layer
  data:     Vec<(StreamEndpoint, OutputId)>,                   // passthrough, no decode
  timecode: Option<(StreamEndpoint, OutputId)>,                // carried, not composited
}
StreamEndpoint = (input_id, StreamKind, selector)
```

**Breakaway falls out structurally** — the three maps are independently keyed.

**Config** (new):

```text
StreamRef     { input_id, kind: StreamKind, selector: StreamSelector }
StreamSelector = internally-tagged by = Index{index} | Language{language:Bcp47}
                 | Best | StreamId{id}        // NEVER untagged (ADR-0010)
RoutingTable  { video: Vec<VideoCrosspoint{cell, source: StreamRef}>,
                audio: Vec<AudioCrosspoint{bus_channel|track, source, gain_db, mute}>,
                subtitle: Vec<SubtitleCrosspoint{layer, source}>,
                output: Vec<OutputCrosspoint{output, program}> }
```

Add a top-level `#[serde(default)] routing: Option<RoutingTable>` mirroring
`audio: Option<AudioRouting>`. **Audio routing must key per-STREAM, not per-input:**
today `AudioRoute` keys `input_id` assuming one audio stream per input — change to
key `(input_id, selector)` for true breakaway. `OutputAudio{mode:Program|Tracks}`
already selects named tracks per output (the output-side audio breakaway half);
transport capability is preserved (`multiview-audio/src/capability.rs
OutputCapability`: MpegTs/Rtsp=`Multiple`, Hls=`SelectOne`, Rtmp=`SingleProgramOnly`,
Ndi=`ChannelMap`).

**Backward-compat desugar** (bump `schema_version` to **3**, stacked on
ADR-0030's v2 programs precedent): when `routing` is absent, derive it — each
`Cell.source.input_id` → `VideoCrosspoint{cell, StreamRef{input_id, Video, Best}}`;
each `[audio].route` → `AudioCrosspoint`; each `CaptionSelector` →
`SubtitleCrosspoint`; each `Output` → `OutputCrosspoint{output, program:"main"}`.
**A v1/v2 doc routes identically** — the matrix is a sugar-superset. The selector
is resolved against the input's probed inventory at **admission** (config-time
validates `input_id` + destination existence; Language/Index resolution is
admission-time).

**`SourceRegistry` keys per elementary stream, not per input** (ADR-0030 §3/§4):
one decode actor per `(physical source, selected stream)` publishing into the
right store type — `TileStore<Nv12Image>` per video, `AudioStore` per audio track,
`CueStore` per subtitle track. Ref-count is **per-stream**, so an unreferenced
A2/S1 spins a decoder only on **first reference** (warm-on-first-reference);
breakaway from a *different* input than the video is expressible because each
facet is independently `Arc`-clonable.

## 5. Instant switch #1 — per-stream re-point **within** a program (Class-1)

All apply at the frame-boundary control hook (`runtime.rs:420`), drained from the
bounded bus. The engine **never awaits** the new source. To make the re-points
genuinely O(1) (not the as-built re-solve), the build must:

- give `CompositorDrive` a **`rebind_cell(cell_id, source_id)`** that mutates an
  O(1) cell→source map (`RouteMatrix.video`) and **skips `solve_layout()` /
  `validate()`** on a pure source re-point (geometry is unchanged);
- **pre-validate + pre-resolve** layout presets at ARM time (off the engine
  thread) so a TAKE swaps a pre-built `Arc<Layout>` (pointer swap = O(1));
- **coalesce** a drained batch: apply all re-points, then re-solve/validate **at
  most once per tick**, not once per command;
- **cap commands drained per tick**, shedding excess back to the bus, so even a
  pathological salvo storm cannot blow the tick budget.

### VIDEO → cell

Re-point the cell binding so the next `sample_cell` (`drive.rs:295`) calls
`read_at(now)` (`drive.rs:313`) on the target's **already-decoded**
`Arc<TileStore>` (`drive.rs:79` stores map). Decode-once (inv #5/#7) — **zero
redundant decode** if the target is already running; a cold target spins a
decoder (ref-count up). **Two correctness conditions the feature must satisfy or
the swap is broken:**

1. **Geometry.** Decode each routable source at a canonical size and
   **scale-at-composite** to the destination cell (the GPU compositor already
   scales; the CPU reference must gain a per-tile resample), OR re-scale on swap.
   Without this, a cross-geometry crosspoint clips/overdraws or smears the canvas.
2. **Warmth.** Gate the take behind **WARM-ON-ARM**: raise the target ref-count,
   wait **control-side** (bounded, off-engine) on `TileStore::is_primed()` /
   `buffered_frames > 0` before the boundary take. Otherwise an unbound spare or a
   reconnecting source returns `NoSignal` → slate/black flash on the take frame.

The output-clock frame boundary IS the RP-168 vertical-interval clean-switch line.

### AUDIO-track breakaway → program bus

Re-point the `ProgramBus` channel's `Arc<AudioStore>`. **This primitive does not
exist** — build `ProgramBus::repoint(RoutePoint, Arc<AudioStore>)` (today
`add_source` only appends; `route_to_program` only changes gain). Make the store
cursor **absolute-tick-indexed** (or, on re-point, seek `read_frame` to the live
edge) so the switch is sample-aligned at the seam, not climbing from frame 0
through evicted history.

**Lip-sync (inv #3) is only structural if `bus.tick()` is driven by the output
tick index.** Carry the tick number with each `StreamItem` and have the consumer
call `bus.tick()` once per tick — catching up `n` times across any
`DropOnOverload` gap — or mix audio on the engine loop alongside `compose`. Then
the `SampleClock` stays a pure function of the tick counter even under overload,
and the re-pointed audio is re-stamped to the same tick PTS timeline as video.

### AUDIO pop-avoidance (the one genuinely new engine mechanism)

A hard cut at the buffer edge is sample-accurate but waveform-discontinuous →
click. Add a per-channel **equal-power cross-fade** (~10 ms): keep the old strip
routed, run both strips from their warm, cursor-aligned stores, and apply an
**intra-block (per-sample) gain envelope** inside `mix_program`'s sample loop —
extend `InputStrip` with `gain_ramp: Option<GainRamp{from,to,frames_total,
frames_done}>`, advance `frames_done` by the tick's sample budget each
`ProgramBus::tick`, then `unroute_from_program` when complete. A single per-tick
**scalar** gain over a 20-40 ms tick block would step in 1-2 coarse jumps and
still click — the envelope must be sub-block. This lives in the off-hot-path bake
consumer where `bus.tick()` already runs (`pipeline.rs:1915`), so inv #1/#10 and
#3 are preserved.

**Two tiers, surfaced as a badge** (ADR-0030 §4 contract):

- **SOFT-STEP** — coded passthrough; ≥2 leading silence/overlap AUs, AU-aligned,
  video-clean. The AAC IMDCT/TDAC seam transient is **uncancellable** in the coded
  domain — Class-1 video-clean / audio soft-step, **not** guaranteed pop-free.
- **CLICK-FREE** — decode → `ProgramBus` cross-fade → re-encode; the transcode
  floor (ADR-0030 rung 3), **Class-2**. The operator opts in.

### SUBTITLE breakaway

Re-point which `Arc<CueStore>` the layer samples (`active_at(now)` per tick).
**Default CLEAR-on-switch** for the boundary frame to avoid a stale/wide cue
flashing; hard cut, no fade. Class-1, instant.

### DATA / SCTE-35

Passthrough, no decode. Discovered from TS PMT `scte35_pids`. The SCTE-35 data PID
has **no IDR of its own**, so it **rides the video seam**. KLV (ST 0601) is a
sibling Data kind (discovery + passthrough). See §6 for the continuity rule.

### TIMECODE

Catalogued as an inventory entry; routed as passthrough (carried, not composited).
Modelled from `multiview-overlay/src/timecode.rs TcSource` + ST2110-40 ANC (RP188)
recognition.

## 6. Instant switch #2 — output → program (crosses the output session)

Lift outputs to a stable id + `OutputRef{output -> program}`. An output→program
switch is a packet-domain splice **identical in kind** to the guarded-passthrough
input→slate splice — **reuse the primitives, but note they have no wiring yet**:

- **GP-6 `RestampAccumulator`** (`restamp.rs:40/82/106`): **one persistent
  accumulator per egress coded stream** straddles the seam; `restamp` keeps
  `dts'=max(raw+offset,last_dts+1)` so `av_interleaved_write_frame` never aborts on
  non-monotonic DTS; `rebase(raw_dts_at_boundary)` re-anchors `offset` at the
  switch instant — computed from the **outgoing** program's last emitted DTS and
  the **incoming** program's first-IDR raw DTS. B-frame reorder survives. **Do not
  reset the accumulator across the switch.**
- **GP-1 `is_idr`** (`idr.rs:77`): the video seam lands on a **true RAP**, not
  `AV_PKT_FLAG_KEY`.
- **GP-7 `GuardedPacketSource` sibling** (the egress splicer): **UNBUILT** — the
  new piece. Also: converge the two `EncodedPacket` types (back the production
  `multiview_ffmpeg::EncodedPacket` with `fanout::EncodedPacket{Arc<[u8]>}`, or
  honestly restate inv #7 as "encode once, ref-counted-AVPacket-clone per muxer" —
  pick one and make the `fanout.rs` doc match reality). Build the **runtime
  sink-mover** (`PacketRouter::register/deregister` driven by a `RouteOutput`
  command) and the producer of `OutputRunState::Migrating` (today only an event
  enum variant, `multiview-events/src/event.rs:209-210`).

**Decision matrix** (seamless | MBB) × (all-streams | breakaway):

- **SEAMLESS** = move the sink's registration to the target program's
  `PacketRouter`, rebase per stream (video=IDR seam; audio=AU boundary, with the
  audio accumulator's rebase landing **coincident with the video IDR** to keep A/V
  sync; subs=cue). **Encode-once preserved (inv #7)** — a switch is a routing-table
  move → **zero new encode** if the target is already encoding; a new encode is
  paid only for a cold target or the MBB params-differ case.
- **Class-2 / MBB** (any pinned-param mismatch — codec/profile/level/res/bitdepth/
  chroma/GOP-structure/track-set/HDR per capability-matrix Class-2 set) = ADR-R004
  make-before-break: pre-warm the target program actor to steady IDR cadence,
  atomic sink cutover at an IDR (`OutputRunState::Migrating`), signal a container
  discontinuity (HLS `EXT-X-DISCONTINUITY` + new `EXT-X-MAP` / TS `disc_indicator`
  + PMT version++ + PCR rebase), stop old if unreferenced. **Admission-gated**
  (NVENC session / Mpix budget).
- **Output-level breakaway** = feed the sink's per-kind muxer streams
  (`PacketMuxSink`/`MuxStream`, `sink.rs` preserves `StreamKind`) from **different**
  `PacketRouter`s: video←progX, audio←progY, subs←progZ.

**SCTE-35 continuity (load-bearing, cited spec).** When GP-6 shifts `offset` at a
switch, any passed-through SCTE-35 section's **`pts_adjustment` must shift by the
same offset.** SCTE-35 2023r1: *"Any device that re-stamps PCR/PTS/DTS and that
passes these cue messages … should modify the pts_time field or the
pts_adjustment field … Modifying the pts_adjustment field is preferred."* As-built
there is **no SCTE restamp target** (`restamp.rs` is video/audio only;
`fanout::PacketKind` has no Data variant; `splice35.rs` is decode-only). Build the
data-PID sibling: add `Data{Scte35}` to the egress packet model, add
`SpliceInfoSection::reserialize_with_pts_adjustment(new_adj)` that rewrites the
33-bit field in place and recomputes the trailing CRC-32/MPEG-2 (helper already at
`mpegts/crc.rs`), and at the seam apply
`new_adj = (old + offset_90k) & ((1<<33)-1)` from the **same** offset GP-6 computed
for the video PID (the section rides the video seam, mod 2^33 wrap). An *immediate*
splice (no `pts_time`) needs no shift — passthrough verbatim. On the HLS tail,
re-derive cues as `EXT-X-DATERANGE` from the re-stamped tick timeline.

## 7. Take / Cut / SALVO — atomic mixed multi-crosspoint

The atomic take already exists: `ArmSalvo`/`TakeSalvo` drained in **one
frame-boundary pass** = one mixed audio tick → a coherent mixed take. **Extend
three surfaces in lockstep** (all `#[non_exhaustive]` + serde-tagged → additive,
forward-compatible):

1. **config `Salvo`** (`multiview-config/src/salvo.rs:59`): add
   `audio:Vec<AudioRecall>`, `subtitle:Vec<SubtitleRecall>`,
   `output:Vec<OutputRecall>`; lift `SourceRecall` to carry a
   `StreamRef{input_id,kind,selector}` (or add a parallel per-stream recall) so
   video routing is per-stream too; extend `Salvo::validate()` to range/duplicate-
   check the new vectors (no bus-channel rebound twice; output→program references
   resolve). Keep `SourceRecall{cell,input_id}` as the **back-compat desugar
   target** (→ video crosspoint, selector=Best).
2. **engine `SalvoChange`/`SalvoBatch`** (`multiview-engine/src/salvo.rs`): add
   matching variants (`AudioRoute{dest,StreamRef}`, `SubtitleRoute`,
   `OutputRoute`) so the batch — already applied as one frame-boundary transaction
   — carries them; wire the apply to re-point the `ProgramBus` channel / `CueStore`
   / `PacketRouter`. This is where the audio cross-fade and IDR-aligned output
   splice must be honoured so the atomic take stays seamless (inv #1) and
   lip-synced (inv #3).
3. **OpenAPI mirror** (`openapi_schemas.rs SalvoDoc` + new `*RecallDoc` types) plus
   the `SalvoDoc` schema list in `openapi.rs`.

`Command` (`#[non_exhaustive]`) gains `RouteVideo{cell,source:StreamRef}` /
`RouteAudio{dest:BusChannel|Track,source,gain_db,mute}` /
`RouteSubtitle{layer,source}` / `RouteOutput{output,program|per-stream-map,class}`;
**`SwapSource{tile,source}` becomes the desugared alias for
`RouteVideo{cell, StreamRef{source,Video,Best}}`** (extend `operation_id()`/`kind()`
match arms additively).

## 8. #11 classification per crosspoint type

The classifier must be **honest at the edges**, not a universal "all in-program =
Class-1". The dominant case is sound and code-backed; the carve-outs come from the
authoritative capability matrix:

| Crosspoint | Class | Source of truth |
|---|---|---|
| VIDEO re-point onto an existing cell | **Class-1** | atomic scene-graph swap (ADR-R004; matrix Class-1). |
| VIDEO re-point requiring a single IDR (cold-target spin-up) | **Reset-lite** | matrix defines a 3rd tier (single IDR/discontinuity within pre-allocated max). |
| AUDIO re-point onto the **program bus** (incl. cross-fade, gain, mute) | **Class-1** | matrix L277/L280 "Hot (mixer weights)"; bus path resamples to the working layout, so the source layout is absorbed. |
| AUDIO breakaway onto a **discrete output track** whose pinned layout/codec/track-set would change | **Class-2** (or Class-1-with-degradation "coerced: downmix/upmix to pinned `<layout>`" the operator confirms) | matrix L276 "track set CRUD → Class-2 (layout)"; mux pins the layout for the session (`sink.rs:136-138/402`). |
| SUBTITLE re-point onto an existing layer/track | **Class-1** | matrix L292 "Hot (re-route)". |
| SUBTITLE breakaway requiring a new passthrough **track set** | **Class-2** | matrix L291 "Class-2 (set)". |
| DATA/SCTE-35 / TC passthrough re-point | **Class-1** | rides the video seam; no encoder reset. |
| OUTPUT ← program, **params match** | **Class-1** (seamless splice) | routing-table move at an IDR/AU. |
| OUTPUT ← program, **pinned params differ** | **Class-2** (MBB migration) | ADR-R004; matrix Class-2 set. |

`/routing/plan` must inspect the **destination's pinned `StreamCodecParameters`**
(the `sink.rs` snapshot), not merely whether the op stays in-program, and return
one of **`{class1, reset_lite, class2}`** (+ the "coerced" degradation flag where
applicable). A property test must assert a breakaway whose source layout ≠ pinned
track layout is **not** reported as plain Class-1.

## 9. API + realtime + UI

**API.**
- `GET /api/v1/inputs/{id}/streams` → `StreamInventory` (pure read off the ingest
  actor's demuxer, **off-engine**, inv #10; emit `input.streams` deltas on
  re-probe / PMT-version bump).
- `POST /api/v1/routing/{video|audio|subtitle|output}/take` returns the **#11 class
  first**: `200 {class:class1, applied:true}` for a hot re-point (params match) vs
  `202 {operation_id}` for a Class-2 migration (params differ).
- `POST /api/v1/routing/plan` (no submit) classifies without applying.
- Reuse `submit_accepted` (Idempotency-Key, RFC 9457 `problem+json`, BOLA
  `authorize_object`/`authorize_output`, shed-to-503).

**Realtime.** Add `Topic::Routing` (`#[non_exhaustive]`, non-breaking) +
`routing.{video,audio,subtitle,output}` delta events (tagged on `t`, never
untagged) + connect-time `$snapshot`. Both are **read-only from the engine's view**
(inv #10) — publish through the **existing wait-free `LatestState` slot +
drop-oldest broadcast**, never a new engine-awaited channel. The snapshot is one
conflated `EngineStateSnapshot` blob today — **fold routing/inventory into it (or
emit as post-snapshot deltas)**, not a per-topic typed snapshot the wire model
lacks. Live inventory rides `Topic::Inputs` as `input.streams`. Salvo arm/take
events already exist on `Topic::Tally`.

**UI.** A **Router crosspoint MATRIX** page (`web/src/pages/RouterPage.tsx`), tabs
per stream-type (Video | Audio | Subtitles | Data), rows = every input STREAM
(badges: `CAM-A · A2 · spa · 5.1`, plus the key-stability tier), cols =
destinations (Video=cells; Audio=bus channels + named tracks; Subtitle=layers;
Output=outputs←programs), **Cut + Take** (salvo arm-then-take), **breakaway badge**
when a destination's V/A/S come from different inputs. A per-input
**stream-inventory inspector** drawer (the operator's `V1 1080p, A1 eng stereo, A2
spa, A3 eng 5.1, S1 eng-CC, S2 fra, SCTE-35, TC` view). A per-output
**now-carrying** panel. Every apply is gated behind a `/routing/plan` Class-1/Class-2
confirm; reuse `SalvosPage`/`TallyPage` for staged mixed takes (tally follows the
resolved crosspoint).

## 10. Invariants preserved (and how)

- **#1 output never stalls during a switch** — all re-points are O(1)
  map/pointer swaps at the frame-boundary hook (once `rebind_cell` + coalesce +
  cap-per-tick land); the engine never awaits the new source.
- **#3 PTS re-stamped from the tick, A/V stay in sync** — video re-stamped from the
  tick counter; audio `bus.tick()` driven by the **tick index** (not surviving
  frames) so the `SampleClock` can't drift under `DropOnOverload`; the egress splice
  lands the audio rebase coincident with the video IDR; SCTE-35 `pts_adjustment`
  shifts by the same offset.
- **#5/#7 NV12 / encode-once** — a switch is a routing-table move → zero redundant
  decode/encode if the target is already running; cold/MBB cases pay exactly one.
- **#10 isolation** — control/preview/realtime read the engine through wait-free
  slots + bounded drop-oldest broadcast; no new engine-awaited channel.
- **#11 Class-1/2 surfaced before apply** — `/routing/plan` returns
  `{class1, reset_lite, class2}` per destination's pinned params, honest at the
  discrete-track-layout carve-out.

## 11. References (build on, do not duplicate)

- ADR-0030 + [multi-program.md](multi-program.md) — `ProgramSet` + ref-counted
  `SourceRegistry`, decode-once-use-many, per-program OutputClock+EngineRuntime.
- ADR-R004 — pin output session params; seamless atomic scene-graph swap; Class-2
  make-before-break parallel-output migration.
- ADR-R005/R006 — program bus + discrete tracks + transport capability + EBU R128.
- ADR-0019/0024 — unified `CaptionCue` + per-tile cue store + HLS SUBTITLES
  discovery.
- [management-capability-matrix.md](management-capability-matrix.md) — the
  authoritative Class-1 / Reset-lite / Class-2 table.
- [realtime-api.md](realtime-api.md) — snapshot-then-delta wire model, isolation.
- SCTE-35 2023r1 §pts_adjustment; SMPTE RP-168 (vertical-interval clean switch);
  SMPTE ST 0601 (KLV); ST 2110-40 / RP 188 (ANC timecode); RFC 8216 (HLS NAME).
