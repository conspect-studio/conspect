# Output routing: route anything to any output

Design note for [ADR-0108](../decisions/ADR-0108-output-routing-matrix.md). Risk class R3: the change
touches invariants #1 (output clock) and #10 (isolation). Every claim about today's code cites
`file:line` on `main` at f5f2230e. Every claim about cost cites a command and a number. The spike
code ran in scratch and is not merged.

The operator, 2026-11-12:

> "how do I route a layout to an output? (and how do I route audio?)"

> "I asked for routing, in, around, through and out. I should be able to route anything to an output - preview, program, layout, VT, bars, source"

The operator, 2026-11-12 13:58 AEDT, on how routing must work (this governs the structure):

> "I'm still lost - we should be making this as absolutely efficient as possible.
> An encoder outputs to a bus, it might "end up" none, one, or more places.
> If an encoder output is routed to a HLS and an SRT output - then they are getting their input from the encoder outputs bus. If the output of the output has to change, it stops getting its input from the output encoders bus and starts getting it from somewhere else.
> Program will have a program bus - that probably will go to one or more encoders.
> Program has to be handled separately because if program is routed to an output, like HLS, that output isn't simply selecting the input to program, it's "take"ing program. And that might be a wipe, dissolve, or a straight input."

And earlier: "Why can't an output be routed seperately if it shares an encoder - I'm lost". The
answer in this design: it always can, because an output never owns an encoder.

The product contract: **every output has a selectable video feed and a selectable audio feed.**
A feed is one of these:

- the Program, Preview or Clean bus;
- any stored layout;
- any media player (VT);
- any generator (bars, solid, clock, timer);
- any single source.

The operator changes a feed live, from the UI and from the API. A change needs no restart and does
not disturb any other output.

---

## 1. What exists today

| Fact | Evidence |
| --- | --- |
| `OutputBus { Program, Clean, Preview }` and `OutputCrosspoint { output, program, bus }` exist in config | `crates/multiview-config/src/routing.rs:204-240` |
| A non-Program bus validates only when an M/E is declared | `crates/multiview-config/src/lib.rs:1635-1645` |
| Nothing at runtime reads `bus`: outside tests and config, no `OutputBus::Preview` or `OutputBus::Clean` appears in `multiview-cli`, `-engine`, `-output` or `-control` (grep). The desugar always writes `Program` | `crates/multiview-config/src/lib.rs:1596-1600` |
| Every output encodes the same composited, overlay-baked Program canvas. Encoders are keyed by codec name only (H.264 HLS and HEVC SRT get two encoders, one per codec) | `crates/multiview-cli/src/pipeline.rs:2820-2844`, `:7172`, `:7193-7206` |
| Program audio is ONE shared AAC encoder for all outputs. Discrete tracks are streams inside it | `crates/multiview-cli/src/pipeline.rs:5795`, `crates/multiview-output/src/sink.rs:724` |
| An output's `audio` selects the mixed bus, discrete tracks, or both (`OutputAudioMode::{Auto, Program, Tracks}`) | `crates/multiview-config/src/audio.rs:304-335` |
| Output CRUD is restart-class: every mutation is wrapped in `with_apply_restart` | `crates/multiview-control/src/routes/outputs.rs:185,254`, `crates/multiview-control/src/typed_resources.rs:57` |
| `apply-layout` swaps the ONE live canvas at the next frame boundary. It is Class-1 and targets no output | `crates/multiview-control/src/routes/mod.rs:756-942` |
| With `[switcher]` declared, startup rejects any canvas other than one full-canvas cell | `crates/multiview-cli/src/pipeline.rs:281-302`, `:2438-2440`; one M/E `me1` only, `crates/multiview-config/src/switcher.rs:48-55` |
| The PVW "bus" is a raw tile sample for the monitor. The code says a real PVW output "must instead own its own admitted compositor/rendition — this tap never composes or encodes" | `crates/multiview-engine/src/switcher_hook.rs:110-126` |
| Route takes exist for `video \| audio \| subtitle` (a cell, a bus channel, a discrete track, a layer). They have plan/take, an `Idempotency-Key`, BOLA checks on both ends, and Class-1/Reset-lite/Class-2 classes. There is no `output` kind and no `GET` of the routing state | `crates/multiview-control/src/routes/routing.rs:107-208`, `:225-252`, `:347-371`; `crates/multiview-control/src/routes/mod.rs:1418-1419` |
| Route takes have no correlated realtime outcome; the change rides the snapshot | `crates/multiview-control/src/realtime.rs:205-222` |
| `audio-routing` PUT applies program-bus gain, include and mute live (ramped). Discrete-track topology is `partial`/restart | `crates/multiview-control/src/live_apply.rs:100-150`, `crates/multiview-cli/src/audio.rs:863` |
| Every source with audio sits on the program bus as a strip: metered, but mixed at gain 0 unless included. So each source's clean per-tick block already exists | `crates/multiview-cli/src/pipeline.rs:17548-17572`, `:4431-4436`; `crates/multiview-audio/src/program.rs:386-395` |
| `AudioStore::read` advances the store's cursor, so a store cannot be read twice per tick | `crates/multiview-audio/src/store.rs:829-850` |
| Equal-power `GainRamp`, `ProgramBus::repoint_crossfade` and exact sample budgets from `SampleClock::total_at` exist | `crates/multiview-audio/src/mixer.rs:35`, `crates/multiview-audio/src/program.rs:233`, `crates/multiview-audio/src/cadence.rs:80`, `crates/multiview-cli/src/audio.rs:770-842` |
| A force-keyframe seam exists: `VideoEncoder::force_next_keyframe` makes the next frame an I picture (an IDR with `forced-idr=1`). Encoders run with B-frames off | `crates/multiview-ffmpeg/src/encode.rs:216,309-318`, `:149` |
| The HLS segmenter starts a new segment on every keyframe | `crates/multiview-output/src/sink.rs:1521-1527` |
| The take gate: `TAKE_PRIME_DEADLINE_TICKS = 50` and the `state_at(...).is_live()` freshness rule | `crates/multiview-engine/src/switcher_control.rs:57`, ADR-0104 §2 |
| The production output clock is a `tokio::spawn`ed task | `crates/multiview-engine/src/programset.rs:546` |
| The frame hook runs on the clock: `before_compose` and `after_compose` | `crates/multiview-engine/src/runtime.rs:244-251` |
| Decode size is the per-axis supremum over the cells that bind a source; an unbound source decodes at 640x360 | `crates/multiview-cli/src/display_size.rs:72-93` |
| `migration::OutputCrosspoint` / `PacketRouter::move_sink` have no production caller. `RestampAccumulator` is used only by `crates/multiview-output/src/guarded.rs` | `crates/multiview-engine/src/migration.rs:145-235`, `crates/multiview-output/src/restamp.rs:40` |

**What "route audio to an output" can do today.** An output can carry one source alone, but only
as a discrete track. That source's route must set `include_in_program_bus = false` and its own
`target_track`, and the output must set `audio.mode = "tracks"`. Changing an output's `audio` needs
a restart. No transport except TS/RTSP-class can carry several tracks
(`crates/multiview-audio/src/capability.rs`). There is no per-output audio-follow-video. The only
audio-follow-video is the switcher's program source (`crates/multiview-cli/src/audio.rs:820-842`).

**Live on mda001** (read-only, 2026-11-12):

- Hardware: Xeon W-2133 (10 threads), RTX 4060 (driver 580.178.04), Quadro P2000 idle.
- The compositor backend is GPU (wgpu), per the journal.
- `nvidia-smi encodersessions` shows 5 NVENC sessions on the 4060: 1080p30 H.264 and 1080p30 HEVC
  (the two outputs), 720p15 H.264 (the wall), and two 720p H.264 (WHEP).
- NVENC utilisation is 21 %, GPU 11 %, and the process uses about 2 of the 10 threads.
- The canvas is 1920x1080 at 30000/1001.

---

## 2. The model

### 2.1 Feeds

```text
Feed  = Bus(program | preview | clean)        -- of M/E me1 (the only M/E)
      | Layout(layout_id)                     -- any stored layout
      | Source(source_id)                     -- any configured source, which includes
                                              --   media players (VT), bars, solid, clock,
                                              --   timer, graphics: they are SourceKinds already
                                              --   (crates/multiview-config/src/schema.rs:653)
AudioFeed = Follow                            -- default
          | Program                           -- the mixed program bus
          | Source(source_id)                 -- that source's own audio
          | Tracks([track names])             -- today's discrete-track selection
          | None                              -- silence
```

**Two layers of bus.** This is the top-level structure:

```text
 sources ─► PICTURE BUSES (raw NV12, on the tick)     ENCODED BUSES                       OUTPUTS
            program   (the M/E's result: cut/AUTO,    one encoder per distinct
                       later wipe/dip)                (picture bus, encoder settings)
            preview                               ─►  encoder ─► encoded bus ─┬─► HLS
            clean                                                            ├─► SRT
            src:<source id>  (source, VT, bars…)                             └─► MoQ / WebRTC / …
            aux:<layout id>  (a routed layout)
```

- **A picture bus carries frames, one per tick.** Program is one of them, and it is the **output**
  of the switcher: a CUT or an AUTO dissolve happens *on* the program bus. Every encoder fed by
  Program, and every output on those encoders, sees the transition as pictures.
- **An encoder reads exactly one picture bus and writes one encoded bus.** It is keyed by
  `(picture bus, encoder settings)`. An encoded bus has zero, one or many subscribers. With zero
  subscribers it lingers for 10 s (so a quick re-take does not reopen it), then closes and frees
  its session. That is invariant #7 as a structure: one encode per key, whatever the subscriber
  count. Program "going to one or more encoders" is several keys on the same picture bus; on
  mda001 today that is H.264 for HLS and HEVC for SRT.
- **An output is a subscriber of one encoded bus.** **Routing an output means moving its
  subscription** to the encoded bus for `(new picture bus, its own settings)`, at a keyframe of the
  destination, with continuous timestamps (§4). The encoder it leaves, and every other subscriber
  of that encoder, are untouched. If the destination encoded bus already exists, the take costs
  **+0 encoders**. Otherwise the engine opens the encoder off the clock, and the take costs
  **+1 encoder**. The only refusal for this reason is capacity (§7).
- **"On Program" and "source X direct" are different routes.** `{bus: program}` follows the
  switcher. `{source: X}` shows X and ignores the switcher. If Program currently shows X, the two
  outputs show the same picture now and different pictures after the next take. Tally and the UI
  keep them distinct (§8).
- **A route take on an output is a clean cut at a keyframe.** An output-level dissolve is not in
  scope: it would need both pictures inside one encoder (a picture mixer per output), and the
  operator asked for Program's transitions to reach the output, which they do because the
  transition is on the program bus.

VT and generators need no feed kind of their own. They already are sources, with a `TileStore` and
an audio store. The UI lists them in their own groups ("Players", "Generators") because the operator
names them separately.

### 2.2 Canvases

Every video feed is produced by a **canvas**: a `CompositorDrive` (`crates/multiview-engine/src/drive.rs:216`)
over the **same** source `TileStore` map Program uses (`catalogue_stores()`, `crates/multiview-cli/src/pipeline.rs:3253`).
That reuse is what makes it decode-once-use-many: a routed feed never opens a decoder.

| Feed | Canvas | Thread | Extra compose |
| --- | --- | --- | --- |
| Program | The Program canvas (unchanged) | Output clock | none |
| Clean | The Program canvas **before** the overlay bake (`item.canvas`, `crates/multiview-cli/src/pipeline.rs:7172`) | Bake consumer | none |
| Preview | A one-cell aux canvas bound to the PVW source | `routing-aux` | one 1-cell (identity or CPU blit in slice 1) |
| Source / VT / generator | A one-cell aux canvas, `fit = contain` | `routing-aux` | one 1-cell (often identity, §2.4) |
| Layout | An aux canvas of that layout's cells | `routing-aux` | one N-cell |

**A routed layout is a second canvas of the same Program, on the Program's clock. It is not a
second Program.** It composes tick T at `T.pts`, from the stores Program reads at tick T. It is
encoded at the Program's cadence and stamped from the Program's tick counter. An ADR-0030 Program
owns its own `OutputClock` and its own seed. Two routed outputs on two clocks could never cut
frame-aligned between Program and a layout, and could not share one stamp sequence. The operator's
case is "the same show, another picture", so this design uses the shared clock. ADR-0030 is amended
(not superseded). Its separate-Program model stays the answer when an output needs a **different
cadence or canvas size** (for example a 25 fps or vertical output) and for passthrough programs.
Those are not the operator's request.

**The aux canvases do not run on the clock.** ADR-0103 proved that a second composite beside
Program costs the clock nothing when it runs on its own thread and shares only wait-free reads.
One difference matters here: the wall reads `store.slot().load()` (`crates/multiview-cli/src/monitor_wall.rs:246,492`), while
an aux canvas uses `read_at`, because it needs the cadence latch. `read_at` advances the latch
watermark, and §2.3 shows that this cannot change Program's selection. The single-reader wording
in the `crates/multiview-framestore/src/tile.rs` docs is updated in slice 1
(`crates/multiview-cli/src/monitor_wall.rs`, chaos tests `a_blocked_wall_worker_costs_the_output_clock_zero_ticks`
and `a_panicking_wall_worker_disables_only_the_wall`). This design generalises that pattern, with
three differences from the wall: aux canvases run at full cadence, they are tick-driven, and they
use the real compositor.

```text
                 decode threads (one per source, unchanged)
                          │ publish
                          ▼
              TileStore map (lock-free; shared)  ◄────────────── read_at(T.pts)
                │                    ▲                                   │
   output clock │ compose T          │ publish canvas T into            │
   (tokio task) ▼                    │ TileStore "layout:<id>" (§3)     │
   ┌────────────────────────┐        │                    ┌─────────────┴──────────────┐
   │ Program canvas T       │        │                    │ routing-aux OS thread      │
   │ after_compose:         │─try_send(Tick T)──────────► │ for each ACTIVE aux canvas:│
   │   1 bounded try_send   │   (cap 4, drop on full)     │   compose(T)               │
   └──────────┬─────────────┘                             │ adopt takes at T (gate)    │
              │ hot queue (existing)                      │ publish to picture buses   │
              ▼                                           └─────────────┬──────────────┘
   bake consumer (existing): bake overlays                              │
     Program frame ──► Program encoders (shared, keyed by settings)     │
     Clean frame   ──► Clean encoder (only if some output selects it)   │
                                                                        ▼
      picture-bus slots (program, preview, clean, src:*, aux:*)
           └─► encoders keyed by (picture bus, settings) ─► encoded buses ─► subscribed outputs
```

In slices 1-2, the clock's only new duty is one `try_send` of a `Tick` (16 bytes) into a capacity-4
channel, in `after_compose`. Slices 3-4 add one more: the existing O(1) frame-boundary
`insert_store`/`remove_store` (`crates/multiview-engine/src/drive.rs:431,454`) for a `layout:` or `bus:` store, once when such a
store is activated or retired. That send never blocks. A full channel drops the tick and increments a counter
(§4.4 handles the gap).

### 2.3 How many canvases, and on which device

- One `routing-aux` thread per run, and only while an aux canvas is active. It is an OS thread, not a
  tokio task, at lower scheduling priority than the clock (spike: `setpriority(+10)`).
- One-cell canvases (Preview, source, VT, generator) use the identity path or the CPU integer
  letterbox (§2.4), in every build. N-cell layout canvases (slice 3) use the run's compositor
  backend (`RunBackend::select`, `crates/multiview-cli/src/pipeline.rs:4140-4175`): wgpu on mda001 under slice 3's gate, the
  integer path in the default build (§7).
- At most `routing.max_aux_canvases` canvases are active at once (default 2; admission rule in §7).
- An aux canvas is **active** only while at least one output, or a layout-as-source bus (§3),
  selects it. An unselected feed costs nothing.
- Two outputs on the same feed share the canvas. Whether they share one encoder is decided in §4.3.

**Where aux work runs, relative to the Program clock.** Program protection is built into the thread
placement:

| Work | Runs on | Priority |
| --- | --- | --- |
| Program clock and compose | a `tokio::spawn`ed task on the main runtime, unchanged (`crates/multiview-engine/src/programset.rs:546`) | normal |
| Aux compose | one dedicated OS thread `routing-aux`, never a tokio task, never on the main runtime | `nice +10` (`setpriority` on the thread id) |
| Encode feed and take state machine | the existing bake consumer thread (`crates/multiview-cli/src/pipeline.rs:5953`) | unchanged |
| A newly opened encoder | its own `RenditionWorker` OS thread (`crates/multiview-cli/src/rendition_workers.rs`), opened off the clock and off the consumer | unchanged (the existing worker type) |

The aux thread never blocks the runtime the clock runs on. The planned move of the output clock to a
dedicated thread does not change any of this. A separate lane is also moving network work off the
main runtime; neither depends on the other.

What aux work shares with Program, and the bound on each:

| Shared resource | Bound |
| --- | --- |
| `TileStore` read path | Lock-free ring snapshot plus one relaxed `fetch_max` on the latch watermark (`crates/multiview-framestore/src/tile.rs:493-502`). The aux thread reads at the same `T.pts` Program read, so it selects the same frame. If Program has already advanced the watermark to T+1 and the producer pruned T's frame, the aux read falls back to the earliest retained frame (`crates/multiview-framestore/src/tile.rs:592-640`): one frame newer, never a wait and never a change to Program's selection (`fetch_max` to a lower seq is a no-op) |
| CPU | One thread at `nice +10`. Slice 1: about 0 (identity) to 42 % of one core (1080p letterbox blit, §7) per active canvas. mda001 has 10 threads at about 20 % |
| Memory bandwidth | A 1080p NV12 frame is 3.1 MB; one blit reads and writes about 6 MB per tick (190 MB/s at 30 fps) |
| GPU queue | **None in slices 1-2.** First used in slice 3, under that slice's hard gate (§7) |
| Encoder sessions | +1 only when a take needs an encoded bus that does not exist yet, admitted against the probed NVENC budget (§7); closed after 10 s without subscribers |
| Program audio bus | Read-only use of the blocks it already produced (§5.2) |

### 2.4 A single source is a one-cell canvas, not a tile pass-through

A source's tile is decoded at the size of the cell that binds it (`crates/multiview-cli/src/display_size.rs:72`). Its colour
tag is the source's own, and its frames arrive at the source's cadence. Passing the tile straight to
an encoder would break three invariants:

- a tile smaller than the output would need an extra scaler (#6);
- a source tag that differs from the canvas tag would need its own conversion (#8, which fixes the
  colour order and handles range exactly once);
- a 25 fps source on a 29.97 output would have no cadence latch.

The compositor already does all three in one pass: scale-at-composite, the fixed colour pipeline,
and the latch-on-tick `read_at` (`crates/multiview-framestore/src/tile.rs:592-640`). So a single
source is a one-cell canvas with `fit = contain` (letterbox or pillarbox on the canvas background).

The cost, stated plainly. Today the decode size follows **only** the active Program layout: an
unbound source decodes at 640x360 (`crates/multiview-cli/src/display_size.rs:72-93`, `crates/multiview-cli/src/pipeline.rs:12718-12724`). Slice 1
therefore adds **aux sizing**: the control drain publishes into the existing `DisplayLayoutSlot` a
*sizing layout*, which is the Program layout plus one full-canvas cell per source an active aux
canvas binds. So `cell_pixel_size` takes the per-axis supremum over both.

The resize uses the existing make-before-break handover (`crates/multiview-cli/src/display_size.rs:1-17`):

- it settles in 1.5 s;
- it waits at least 5 s between two resizes of the same decoder;
- at most 2 handovers run at once, each holding a second NVDEC session briefly.

A source that is also on Program decodes at the larger size, and Program scales it down at
composite as it already does for multi-size cells. The cost is more decode MP/s for that source.
Program's picture does not change.

The plan reports each resize in `decode_resizes`, with the extra MP/s. Admission counts them
(§7: at most `routing.max_decode_escalations`, default 2). Until a resize lands, the source is
blitted, not identity: a 640x360 tile is letterboxed to 1080p at 14.1 ms per frame on one core. The decode-side scaler already emits tiles at the planned size, tagged like
the canvas (`TileScaler`, `crates/multiview-cli/src/pipeline.rs:17574-17580`). So once the resize lands, the tile matches the
canvas in size and colour tag, and the one-cell compose is the identity. Implementation detail: the
drive returns the tile `Arc` without drawing. This makes the common case (a 16:9 source to a 16:9
output) free of compose cost. Until the resize settles (1.5 s, `crates/multiview-cli/src/display_size.rs:41`), the compose
scales the smaller tile. A tile with a different aspect ratio is letterboxed by the compositor.

**No GPU in slice 1.** Ingest tiles are already converted to NV12 and tagged with the canvas colour
(`CanvasColor::default().output_tag()`, `crates/multiview-cli/src/pipeline.rs:14229`, applied by `TileScaler`). Generators
that cannot resize decode at the full canvas (`crates/multiview-cli/src/display_size.rs:95-100`). So a routed single source
is either:

- **the identity**, when the tile is canvas-sized at the canvas aspect ratio (generators, and any
  16:9 source once its decode follows the routed size); or
- **a letterbox or pillarbox with a pure-integer CPU blit** (the ADR-0103 wall scaler,
  `crates/multiview-cli/src/monitor_wall.rs:179-229`), with colour samples copied unchanged (tag
  equal). Measured at p50 9.4 ms (720p) and 14.1 ms (1080p) per frame on one core.

Slice 1 therefore adds no second GPU compose. It does not use the CPU reference compositor either,
which is measured at 198 ms per 720p frame on one core.

The identity shortcut is taken **only** when all of these hold; otherwise the full compose runs:

- the tile and the canvas have equal width and height;
- the pixel format is NV12 on both sides. Stride does not matter: the encoder's frame converter
  repacks into libav planes (`crates/multiview-output/src/sink.rs:601,654`);
- the tile is progressive. An interlaced-flagged tile always takes the full compose;
- the tile carries no alpha plane;
- the cell has opacity 1 and no border or radius;
- the tile's full `ColorInfo` equals the canvas `ColorInfo`: primaries, transfer, matrix **and**
  range.

**What this colour rule does and does not prove.** Every ingest tile is tagged with the canvas
colour at decode (`crates/multiview-cli/src/pipeline.rs:14229`). `TileScaler` retags without converting
(`crates/multiview-cli/src/pipeline.rs:17593-17620`). So the tag comparison above always passes for an ingest tile. It does
not prove that the source's native colour matches. That retag already exists on **Program**:
Program composites the same tiles with the same tags.

So a routed source shows exactly the samples Program would show for that source in a full-canvas
cell. Invariant #8 is no worse on a routed output than on Program today. Fixing native colour
carry-through at ingest (for example a BT.601 or full-range source) is a pre-existing gap, filed
as #310. The routing path must not hide it: slice 1 tests that a routed
source and the same source on a full-canvas Program cell give byte-identical frames.

### 2.5 Preview and Clean

- **Preview is not composed today.** The switcher's PVW "bus" is a raw sample of the PVW source's
  tile `Arc`, at tile size, for the monitor (`crates/multiview-engine/src/switcher_hook.rs:110-126`). Nothing composes or encodes
  it. In this design, **Preview** is a one-cell aux canvas whose cell follows the M/E's PVW selection. The switcher hook
  already knows the selection every tick (`crates/multiview-engine/src/switcher_hook.rs:61`). It publishes it into a wait-free
  `SwapCell`, written only when it changes (the ADR-0104 §4 pattern). The aux thread rebinds its cell
  when the cell changes (`CompositorDrive::rebind_cell`, `crates/multiview-engine/src/drive.rs:480`). This is the "own admitted
  compositor/rendition" that `crates/multiview-engine/src/switcher_hook.rs:113-115` calls for.
- **Clean** = the Program canvas without the monitoring overlay bake (labels, tally borders, clocks,
  watermark: `BakeContext`, `crates/multiview-cli/src/pipeline.rs:5146-5195`). The bake consumer offers `item.canvas` instead
  of `overlaid` to the Clean encoder. It costs no compose, only one encoder while selected. When a DSK
  stage exists (ADR-0054), Clean is the tap before the DSK. The routing contract does not change.
- **Clean as built (slice 2, #305).** `{"bus":"clean"}` is a routed picture bus like Preview and
  needs an M/E (`switcher-required` otherwise). The bake consumer publishes `item.canvas` (the
  Program canvas before `baker.bake`) into the Clean slot with `OutputRouter::publish_clean`, just
  before the router's tick, so the Clean encoder gets frame T in tick T. It is one `Arc` clone into
  a wait-free `SwapCell`, no copy, and a single cached-bool check when nothing selects Clean. The
  aux thread never composes Clean and never wakes for it: routing only Clean leaves the clock's aux
  gate closed and sends no aux tick. Clean is always fresh for the take gate (§4.5): it is published
  every tick by the bake consumer itself, never by the aux thread, so an aux-thread failure never
  moves an output off Clean (only aux-composed buses fall back on that path). A Clean output still
  degrades and falls back (§4.4) when its OWN live encoder refuses frames (a full worker queue,
  counted per key) — a starved encoder, not a stale feed. A bake-consumer stall
  that sheds hot-queue ticks substitutes Clean with its own last pre-bake frame
  (`OutputRouter::substitute_clean`), exactly like Program's base-worker substitution, so a consumer
  gap alone never fails the Clean key either. `audio = follow` on Clean is the program mix.

---

## 3. In, around, through: layouts and buses inside each other (#243)

**Layout-as-a-source.** An active layout aux canvas publishes each composed frame `Arc` into a
`TileStore` with the reserved id `layout:<id>`, stamped at `T.pts`. That store goes into the catalogue
store map, so:

- the M/E's PVW and PGM can select `layout:NewsOne` like any source (ADR-0104 admission:
  `validate_source_configured` accepts stored layout ids as `layout:<id>`);
- the fresh-frame take gate works unchanged (`state_at(...).is_live()` on that store);
- the AUTO dissolve works unchanged (it is a full-canvas tile, `crates/multiview-engine/src/drive.rs:252`).

**Latency.** The layout composed for tick T is published, stamped `T.pts`, after tick T's Program
compose. Program at tick T+1 latches it (nearest-not-after, `crates/multiview-framestore/src/tile.rs:592`). So in steady state a
layout on Program is **one tick (33 ms) behind** its own sources.

Under load the aux thread can lag, and Program then latches an older layout frame. The **actual**
age is known per tick: `CompositedFrame::source_times` gives the stamp of the selected picture
(`crates/multiview-engine/src/drive.rs:71-73`). The audio delay follows that actual age (§5.4). A layout frame older than 4
ticks ages through the store's normal freshness ladder (`crates/multiview-framestore/src/tile.rs:521-530`), so the take gate and the
STALE handling apply to it like any other source.

The alternative is to compose the layout on the clock before Program. That puts an N-cell composite
on the clock task, which ADR-0103 rejected ("A second compositor pass on the clock thread … risks
invariant #1"). One tick of latency is the price of keeping the clock clean, and it matches what a
hardware multiviewer feeding a switcher input costs.

**Buses inside layouts.** A layout cell may bind `bus:program` or `bus:preview`, the classic
multiviewer with PGM/PVW tiles. The bake consumer publishes the composited Program canvas into
`TileStore` `bus:program`. That costs one `Arc` clone per tick, because the canvas `Arc` already
exists at `crates/multiview-cli/src/pipeline.rs:4764`. The Preview aux canvas publishes into `bus:preview`. Both are one tick
behind.

**Cycles are refused.** One example: Program shows `layout:A`, and A contains `bus:program`. Every
plan computes the feed graph. An edge goes from each feed to the feeds its cells bind, through the
M/E's current PGM/PVW selection. A take or a layout edit that closes a cycle gets 422 `feed-cycle`
with the path. The engine re-checks at adoption, because the M/E selection can change between plan
and tick. A cycle found at the tick refuses the take (typed outcome) and keeps the last-good feed.

**The first-slice restriction is removed.** `first_slice_me_layout_supported` (`crates/multiview-cli/src/pipeline.rs:281`)
existed because the Program canvas *was* the multiview, so a switcher would have replaced it. In the
new model, with `[switcher]` declared, the Program canvas is the M/E bus: a one-cell canvas, exactly
as today. The document's configured multi-cell layout is admitted as the stored layout `config`,
routable like any other. Startup no longer fails, and nothing is "replaced". Existing configs keep
working. The configs that fail today (switcher plus multi-cell) start, with Program = the M/E. By
default their outputs carry Program, and the operator routes `layout:config` to the outputs that
should keep the wall.

**`apply-layout` in the new model.** It means "route layout X to the feed `program`'s canvas".

- **No switcher declared.** It keeps today's meaning exactly: the Program canvas swaps to X at the
  next boundary (`crates/multiview-control/src/routes/mod.rs:756-942`, unchanged).
- **Switcher declared.** The Program canvas is the M/E bus, and replacing it would break the bus
  (`crates/multiview-engine/src/switcher_hook.rs:66` rebinds a named cell that a foreign layout does not have). The call answers
  422 `switcher-owns-program`. The detail names the two operations that do what the operator means:
  `POST /routing/output/take` (layout to an output) and `POST /switcher/commands/select_preview`
  with `layout:<id>`. It never silently changes something else.
- An optional body field `target: {output: id}` makes `apply-layout` a thin alias for an output
  take, so the Layouts page "Apply" can offer "Apply to output …".

---

## 4. Switching an output's video feed live

### 4.1 Decision: a take moves the output's subscription between encoded buses

A take moves the output's subscription between two encoded buses. Both encoders stamp PTS/DTS
from the same Program tick counter (`crates/multiview-cli/src/pipeline.rs:6518`; the encoder accepts only the next tick,
`crates/multiview-output/src/sink.rs:646-655`). So the move needs no restamp, and no RT-13
splicer. What it needs is a keyframe on the destination at the move tick and identical parameter
sets, and §4.6 proves both.

The output's sink never restarts. Its muxer keeps its streams and continuity counters; only the
source of its packets changes, at a window boundary of the dispatcher.

This replaces round 1's "a take never opens an encoder or moves a muxer binding". Moving the
binding *is* the route. What stays is the safety intent: nothing in a take runs on the clock, and
nothing in a take blocks the bake consumer (§4.4).

### 4.2 The cut

Tick T is the move tick (§4.5).

1. **Destination ready.**
   - If the encoded bus for `(new picture bus, the output's settings)` exists, the move waits for
     that bus's next IDR. It forces one only within the **per-encoded-bus force floor**:
     - at most one forced IDR per 2 s per encoded bus, for takes and the ADR-0049 viewer-join/PLI
       force path together (`crates/multiview-ffmpeg/src/encode.rs:208-216`), through one per-bus
       gate. There is one floor, not two: the caller-owned 2 s floor already governs that seam;
     - when the floor is not met, the move happens at the next natural or already-due IDR, at most
       one GOP later (2 s on mda001).

     So no principal can make another principal's output carry more than one extra IDR per 2 s.
   - If it does not exist, the engine opens it **off the clock** (§4.3). It starts at an absolute
     tick T0 (§4.3) and moves the output at T = T0 + 1 or later, at a forced IDR. The spike moved
     at T0 + 10.
2. **Keyframe with parameter sets (the move gate).** Each encoder is opened with `forced-idr=1`, so a forced I picture is a true IDR.
   `VideoRenditionEncoder` lacks the option today (`crates/multiview-output/src/sink.rs:578-595`), and it lacks a method that
   forwards to `VideoEncoder::force_next_keyframe` (`crates/multiview-ffmpeg/src/encode.rs:208-216`).
   Slice 1 adds both. The move happens only when the destination packet stamped T is a decoder-ready
   RAP (`StreamCodecParameters::is_decoder_ready_rap`, `crates/multiview-ffmpeg/src/packet.rs:384`).
   `is_decoder_ready_rap` is necessary, not sufficient: it is true on an IDR NAL alone. The gate
   also parses the access unit and requires its parameter sets in it: SPS and PPS for H.264,
   VPS, SPS and PPS for HEVC. They are there because no family is opened with
   `AV_CODEC_FLAG_GLOBAL_HEADER`, and libavcodec then sets NVENC `repeatSPSPPS`, x264
   `b_repeat_headers` and x265 `repeat-headers`. VAAPI and VideoToolbox outputs are not routable
   in slice 1 (`output-kind-unroutable`) until the same check exists for them.
   Otherwise the move is retried at the next tick, up to 3, then refused with `encoder-not-ready`
   while the output keeps its feed.
3. **Timestamps.** The output's packets are window T−1 from the old encoder, then window T from the
   new one. PTS = DTS = tick index in 1/cadence on both, so the step stays one tick. B-frames are
   off (`crates/multiview-output/src/sink.rs:582,589`).
4. **Parameter sets.** Encoders that share settings (and the same libav encoder) produce
   byte-identical SPS/PPS (VPS for HEVC). §4.6 measured this for libx264 and libx265, with one
   encoder opened 200 ticks late. They carry them in band at every IDR, because the crate never
   sets `AV_CODEC_FLAG_GLOBAL_HEADER` (`crates/multiview-ffmpeg/src/encode_options.rs:24-28`).
   So the output's muxer init (TS PMT, fMP4 `EXT-X-MAP`, FLV sequence header, MoQ catalog
   description) usually stays valid. **Identity is not assumed.** The engine compares the new
   encoder's in-band parameter sets with the moving output's current ones at the move:
   - **equal**: a plain move;
   - **different** (same codec, size and format): still correct for consumers that read in-band
     sets: MPEG-TS (HLS TS segments and SRT: the decoder re-reads SPS/PPS at the IDR, and the
     segmenter starts a segment there), FLV and MoQ (a new group starting at the IDR with its
     sets). Slice 1 routes those. For consumers with out-of-band init (fMP4/CMAF `EXT-X-MAP`, a
     MoQ catalog description, an FLV sequence header) a difference is signalled as a
     discontinuity on that output (`EXT-X-DISCONTINUITY`, a new init), and the event says "a brief
     decoder reset on this output"; it is never spliced silently;
   - a change of codec, size or pixel format is impossible by construction (the key includes the
     output's settings) and is an internal fault `encoder-format-mismatch`.

   **Slice 1 behaviour (no mux re-init is built).** Each dispatcher binding carries
   `inband_parameter_changes_ok`, set by the pipeline from the output's container: `true` for
   MPEG-TS muxers only (HLS TS segments, SRT and RIST push, a `.ts`/`.m2ts`/`.mts` file, the
   internal `program.ts` anchor); `false` for LL-HLS (fMP4/CMAF), RTMP/FLV, WebRTC/WHIP, MP4/MKV
   and other files, and the raw-frame sinks (NDI, AES67, where it does not apply). With `true`, a
   move or fallback onto a RAP whose parameter sets differ (or whose scan was rejected as
   malformed) splices at that RAP, which carries the new sets in band, so an MPEG-TS decoder
   reconfigures there. The dispatcher also bumps the output queue's generation (it does not arm the
   RAP gate or flush the backlog); in slice 1 no MPEG-TS muxer reads that generation, so no
   `EXT-X-DISCONTINUITY` and no continuity-counter reset is written, and the MoQ publisher treats
   the bump as one dropped window and resumes at the next RAP. With `false`, a take's move is
   **refused** (`Refused(FormatMismatch)`: "This feed's encoder settings differ from the output's;
   the take was refused to avoid a corrupt stream") and the output keeps its feed. A fallback cannot
   be refused (the routed feed is gone), so on such an output it splices at the RAP with the same
   generation bump; that output's out-of-band init is then stale until restart (the operator sees
   the fallback event). Equal sets always move. Writing a real discontinuity (HLS tag, TS continuity,
   new fMP4/FLV init) is owed by a later slice.

   **Slice 2:** HLS (TS segments) writes `EXT-X-DISCONTINUITY` before the segment that starts at
   the changed-set RAP. The queue marks the first window admitted after the
   `DecoderConfigChanged` discontinuity (`BatchLease::decoder_config_changed`); the HLS packet
   source reports it once at that window's keyframe, and the segmenter marks the segment starting
   there in the main playlist and every rendition playlist. A loss never sets it. SRT, RIST and
   `.ts` files still carry the new sets in band with no other change.

   Which case NVENC gives across a live open is measured on mda001 (slice 1 acceptance).
5. **Format.** An output's format (size, cadence, pixel format, colour tags, codec settings) never
   changes on a take, because the destination key includes the output's own settings. A format
   change is RT-15's migration, a separate operation.
6. **Lagging feeds.** A picture bus whose aux frame for tick T has not arrived is held: its encoder
   is fed the newest frame with that frame's own generation label. A live encoder that refuses
   the offered frame (a full worker queue) is counted per key, not per bus (§4.4): Clean included,
   whose picture is otherwise published fresh every tick, its outputs degrade at 60 refused offers
   and fall back at 150. After 60 held ticks the alarm
   `routing.degraded` is raised. After 150 held ticks (5 s), whether the aux thread is dead or
   wedged, every output subscribed to that bus moves to
   Program at an IDR, with the typed event `fallback:aux-failed`. An output is never frozen
   forever. The bake consumer's own substitution path (`crates/multiview-cli/src/pipeline.rs:7173-7195`)
   becomes per picture bus, so a consumer stall never feeds Program pixels into another bus's
   encoder.

What each consumer is expected to see at the move. The design relies on the facts below. Each is
verified per protocol in the slice that ships it (§9). §4.6 proved the MPEG-TS rows.

| Output | Sees |
| --- | --- |
| HLS (TS segments) | The packet-fed segmenter rotates on a video keyframe (`crates/multiview-output/src/sink.rs:2693-2715`), so a new segment starts at the moved IDR. Media sequence continuous, no `EXT-X-DISCONTINUITY` (identical SPS/PPS, continuous PTS) |
| LL-HLS / CMAF | A new part/segment at the IDR; the init segment is unchanged because the parameter sets are identical |
| SRT / RIST (MPEG-TS) | One muxer, so continuous PCR, continuity counters and PTS; an IDR with in-band SPS/PPS |
| RTMP / FLV | Continuous timestamps, a keyframe tag, no new sequence header (identical SPS/PPS) |
| WebRTC (WHEP output, WHIP push) | Continuous RTP timestamps; the IDR arrives, so no PLI round trip |
| MoQ | A new group at the IDR; the catalog description is unchanged |
| NDI, display (KMS) | No encoder: the output subscribes to the picture bus directly and switches at frame T |

### 4.3 Encoders: opened live, off the clock, keyed by (picture bus, settings)

- **Registry.** The control side keeps an encoder registry: key `(picture bus, settings)` to
  `{encoder worker, subscribers, linger deadline}`. It extends today's start-time keying by settings
  (`crates/multiview-cli/src/pipeline.rs:2830-2842`) with the picture bus. Absent config gives today's keys (`program`,
  settings), today's encoder count and today's pixels.
- **Opening is admitted and serialised.** Admission **reserves** the encoder session at plan
  acceptance, before the open, so two takes on adjacent ticks cannot both pass against the same
  NVML count. At most one live encoder open runs at a time per GPU; others queue, bounded at 4 and
  refused beyond that. A failed open releases the reservation and refuses the take with
  `encoder-not-ready`.
- **Opening is off the clock and off the consumer.** A take whose destination does not exist spawns
  a `RenditionWorker` on a new OS thread (the existing worker type, `crates/multiview-cli/src/pipeline.rs:6964-6984`). The
  encoder opens there; an NVENC open can take hundreds of ms.
  - **Open deadline.** 90 ticks (3 s at 30000/1001) from admission to the destination's first
    completed window that passes the move gate (§4.2 step 2). On overrun the take is refused
    `encoder-not-ready`, the session reservation is released, the worker is handed to the reaper,
    and the output keeps its previous feed. The operator sees "The encoder for this feed did not
    start in time".
  - `VideoRenditionEncoder` today always starts at tick 0 and rejects anything else
    (`crates/multiview-output/src/sink.rs:604,646-655`). Slice 1 adds `starting_at(T0)`: the first frame it accepts is T0, and
    that frame is an IDR.
  - T0 is the tick after the open completes.
  - The bake consumer offers frames to a new worker with `try_send` (drop when full), never with the
    bounded wait it uses for established workers (`offer_within`, `crates/multiview-cli/src/pipeline.rs:7193-7206`). So a
    slow open or a warming encoder cannot hold the consumer.
- **Rebinding.** The dispatcher's bindings are fixed for one generation today
  (`crates/multiview-cli/src/rendition_dispatch.rs:61-79`). Slice 1 adds two contracts, because
  video and audio move at different boundaries (§5.2):
  - `rebind_video(output, key, at_tick)`: windows before `at_tick` carry the old key's video, and
    windows from `at_tick` the new key's;
  - `rebind_audio(output, key, at_sample)`: audio packets that start before grid boundary G carry
    the old key, packets from G the new key. `key = None` is the shared program stage: slice 2
    uses it to return an output from its private bus (§5.2). An audio-only take's move is
    unconditional; a video take's audio move follows its video move and is dropped with it.

  Each swap is atomic per output at its own boundary.
- **Window closure with a live-opened stage (the rule that protects Program).** The dispatcher
  closes a window on the **base** stages only: the base (startup) video stages and the audio
  stages must be complete up to its end (`RenditionDispatcher::drain_base` in
  `crates/multiview-cli/src/rendition_dispatch.rs`). A stage opened live **never gates a base
  window**, not before its first completed window and not after it: its windows are not delivered
  to anyone until an output moves onto it, and a slow, wedged or failed live stage can delay no
  window of any other stage. This is stronger isolation than "every live stage is complete": the
  live stage's watchdog (`fail_stale_dynamic`) and per-binding admission fail it alone, and its
  subscribers fall back to Program. An output is rebound only to a window the destination stage has completed and whose first video
  packet passes the move gate (§4.2 step 2). Two tests pin this, and they fail without the rule: a
  live open whose encoder is slow (several ticks) or wedged (never produces) leaves every existing
  output's windows closing on time with none late or missing; and the moved output's first packet
  after the move is a keyframe carrying its parameter sets. The open-storm and wedged-live-open
  chaos tests (§9) assert the same.
- **Gaps on a live-opened worker.** The consumer `try_send`s to a new worker and may drop a tick.
  The worker fills the missing tick with its last frame, so its encoder still sees consecutive
  ticks (`crates/multiview-output/src/sink.rs:646-655`) and its windows stay contiguous.
  `migration::OutputCrosspoint` / `PacketRouter::move_sink` (`crates/multiview-engine/src/migration.rs:145-235`)
  are the same idea over the unused fan-out carrier; production uses the dispatcher, so the
  dispatcher is extended and the carrier work (#144, #154-#156) stays separate.
- **Dispatcher topology is extended before the first frame.** Today the dispatcher's encoder set is
  fixed for a run. An event from an unknown encoder key is `InvalidEvent`, which fails the
  dispatcher and aborts delivery for **every** output, Program included
  (`crates/multiview-cli/src/rendition_dispatch.rs:145,188,319-344`). Slice 1 therefore:
  - registers a new key (`add_video_key`/`add_audio_key`, with its time base, which must equal the
    cadence exactly as `RenditionDispatcher::new` requires,
    `crates/multiview-cli/src/rendition_dispatch.rs:269-272`; a mismatch refuses the open) in the
    dispatcher
    **before** the worker's first offer. The add arrives as a control event on the same consumer
    thread, so it is ordered before any packet of that key;
  - makes a packet window from a registered key that has no bound output **dropped and counted**,
    never an error;
  - keeps `InvalidEvent` for genuinely malformed events only. A unit test proves that an unknown or
    late key never aborts Program delivery.
- **Linger and close.** An encoded bus whose last subscriber left closes after 10 s. The consumer
  only **marks** it for close. The EOF, flush (an NVENC flush can take a GOP) and join run on a
  separate `routing-reaper` thread, never on the consumer or the clock. The worker's event send to
  the consumer is `try_send` while it is being closed: a full channel drops tail packets, which no
  subscriber needs. A §9 chaos test asserts that a close never moves the consumer's tick. A re-take within 10 s reuses it (+0 encoders).
  - Every live and lingering bus is attributed to the principal whose take last subscribed an output
    to it.
  - A principal may hold at most **one** lingering bus besides its current subscriptions, and at
    most `routing.max_encoders_per_principal` (default 2) routed buses in total. Its oldest linger
    closes first.
  - A re-take does not refresh the linger of a bus the principal has already left.
  - Under admission pressure, 0-subscriber lingers close before any take is refused.

  So round-robin takes cannot pin the encoder pool.
- **Merging.** Two outputs that reach the same picture bus with the same settings are on the same
  encoded bus **by construction**: the key is the same, so there is nothing to merge and nothing to
  split.
- **Phases.** ADR-R010's make-before-break phases map onto this: VALIDATE is plan and admission;
  SPIN-UP is the open; WARM is encoding from T0; SWAP is the rebind at an IDR; DRAIN+STOP is linger
  and close. What it does not need from RT-15 / #146: a second Program, a format change, a container
  discontinuity, or a restamp.

### 4.4 Isolation of the data path

- **Clock to aux:** one `try_send(Tick)` (capacity 4). A full channel drops the tick: the clock
  never waits for the aux thread.
- **Aux to picture buses:** each composed frame is published, tagged with its tick and generation,
  into its picture bus's bounded slot (latest few, drop-oldest). The bake consumer reads it without
  waiting (§4.2 step 6).
- **Encoder open:** on its own OS thread (§4.3); the consumer only `try_send`s to a new worker.
- **Aux thread** reads only `TileStore::read_at` (lock-free, `crates/multiview-framestore/src/tile.rs:592`) and the selection
  `SwapCell`s. It takes no lock the clock holds.
- **Take admission:** `try_send` into a bounded mailbox (64, the `COMMAND_CAPACITY` pattern of
  `crates/multiview-engine/src/switcher_control.rs:53`). A full mailbox answers 503 `engine-busy`.
- **A wedged or panicking aux thread** affects only the outputs on aux picture buses. They hold their last-good
  frame: `routing.degraded` at 60 ticks, then a fallback to Program with a forced IDR
  (`fallback:aux-failed`) at 150 ticks. A drop guard publishes `routing.feed_state = failed` and an alarm.
  Program and every default output continue, which is the ADR-0103 wall pattern.
- **A wedged encoder** costs only the outputs subscribed to its encoded bus. The dispatcher already isolates bindings
  (`crates/multiview-cli/src/rendition_dispatch.rs:1-5`). Starvation is counted per encoder
  key, not per bus: each refused offer to a key's worker counts, an accepted one resets it, so a healthy sibling key on
  the same bus never hides a starved one. At 60 refused offers in a row the key's outputs show `degraded`; at 150 the
  key is closed and its outputs fall back to Program (`fallback:encoder-failed`), Clean included. A consumer stall
  substitutes a Clean key's own last frame for the missing ticks and stops at the first refused offer; the key's
  `GapFill` (bound 24 ticks: a full 8-tick substitution gap on top of a full 4-deep queue) closes the rest.

### 4.5 Take gating

A take names an output and a feed. **The take state machine runs on the bake consumer**, which
handles every Program tick (`crates/multiview-cli/src/pipeline.rs:7110`), **not on the aux thread**. So a wedged aux thread
cannot stop a take from completing, being refused, or raising an alarm. At each tick the consumer:

- drains the take mailbox, where the latest take per output wins and a replaced pending take gets
  the outcome `superseded`;
- tells the aux thread which canvases to keep active, through a `SwapCell`;
- applies the gate below, using only wait-free reads: `state_at` on the stores, and each picture
  bus's newest aux frame and its stamp;
- counts held ticks per picture bus. This is also how it detects an aux thread that is dead or wedged:
  no new aux frame for N ticks.

The gate:

| Feed | Ready when |
| --- | --- |
| Source / VT / generator | `store.state_at(T.pts).is_live()`: the compositor's own freshness rule, ADR-0104 §2 |
| Preview | the PVW source passes the source rule |
| Layout | the canvas exists (each cell holds or slates on its own), **and** for a layout not active yet, at least one composed frame exists |
| Program, Clean | always |

If the feed is not ready, the take is held, and the output keeps its current feed. A take still not
ready after `TAKE_PRIME_DEADLINE_TICKS` (50) is refused with `feed-not-fresh`. The output never shows
a stale, held or slate picture from a feed that was not ready, and never black. A take to an
on-demand source takes the demand lease (video and audio) on the control side before enqueueing. So
the source starts while the take waits, as `select_preview` does today (ADR-0104 §2).

### 4.6 Spike evidence

The spike ran in scratch and is deleted (§10). It used:

- the real `EngineRuntime` with `RealtimePacer` at 30000/1001 for 900 ticks;
- a Program one-cell 1280x720 canvas encoded with libx264;
- an off-clock aux thread driven by one `try_send(Tick)` from `after_compose`;
- from the **same** six `TileStore`s: a 6-cell layout canvas and a one-cell source canvas, each into
  its own libx264;
- output B taking b, then dead (never publishes), then c, then the layout, then d, under a flood of
  about 20,000 takes at 2,000 per second.

Results:

- **Routed output B:** 893-900 packets, every PTS step exactly 3003 (90 kHz), DTS = PTS (ffprobe).
  The forced IDRs sat exactly on the cut ticks; for example cuts at 67, 256, 364 and 457 gave
  keyframes at 67, 256, 364 and 457, besides the GOP-60 cadence.
- **Untouched output C** kept a pure GOP-60 keyframe cadence through all of B's cuts: other outputs
  are not disturbed.
- **The take to `dead`** was refused at the 50-tick deadline, and B stayed on b.
- **Program:** 900/900 ticks and 0 repeats in both arms. Program tick lateness with the aux thread
  limited to 4 cores was:

  | Arm | p99 | max |
  | --- | --- | --- |
  | Baseline | 13.6-15.6 ms | 18-19 ms |
  | With routing | 18.8-19.8 ms | 31-34 ms |

  That is **+4 to +5 ms at p99 and +13 to +15 ms at max**. The max with routing came within 2 ms of
  one whole tick (33.4 ms). Both arms ran on the same loaded host (load 25-90 on 56 cores), and the
  max difference is larger than the run-to-run spread of the baseline. An unpinned pair produced
  the opposite ordering (baseline p99 83 ms, routing 21 ms), which shows how noisy the host is, but
  it does not cancel the pinned result.

  **What the spike proves:** no lost and no repeated Program tick, continuous PTS on the routed
  output, and clean cuts. **What it does not prove:** that Program lateness is unaffected. That
  spike ran the CPU reference compositor for its aux canvases (about 100 ms per frame on 4 cores),
  a much heavier load than slice 1's identity and integer blit. Program protection is therefore
  measured per slice on mda001 against the bound in §10, not inferred from the spike.
- **When the aux thread was starved**, the routed outputs fell to 110 of 900 fresh frames. The
  spike's encoder thread gap-filled the rest at a constant PTS step. Program stayed at 900/900 and
  0 repeats. That shows the intended degradation order (§7). In production the hold lives in the
  picture bus slot (§4.2 step 6), not in a separate encoder gap-fill, and is bounded by the 60-tick
  degradation rule.
- **Second spike: subscription move between running encoders** (scratch, deleted; the operator's
  model). Three encoders ran on the same 30000/1001 tick counter, with the production
  `VideoRenditionEncoder` option set plus `forced-idr=1` and scene-cut off:
  - A: picture bus `program`, from tick 0;
  - B: picture bus `src:bars`, **opened live at tick 200** with absolute PTS 200;
  - C: `src:bars`, from tick 0, with other subscribers.

  Output X moved A → B at tick 210 and back B → A at 330 (a forced IDR on A, which A's other
  subscriber also received). Output Y moved A → C at 250. Each output was muxed to MPEG-TS from the
  subscribed packets, with no re-encode and no restamp. Results, identical for **libx264 and
  libx265**:

  - the parameter sets (SPS/PPS, plus VPS for HEVC) of B (opened 200 ticks late) and C were
    **byte-identical** to A's;
  - 450 packets per output, every PTS step exactly 3003, DTS = PTS;
  - the first packet after every move was a keyframe;
  - keyframes at 0/60/120/180, then at 210 and 330 for X, and at 250 for Y;
  - `ffmpeg -v error` decoded both outputs with **0 errors**;
  - the decoded luma average switched exactly at the move frame: X frame 209 = 69 and 210 = 183,
    329 = 183 and 330 = 69; Y frame 249 = 69 and 250 = 183.

  NVENC parameter-set identity and HLS/LL-HLS/FLV/MoQ consumers are verified in slice 1 on mda001
  (§9).
- **AAC move** (scratch): see §5.2. Two different-content streams spliced at a frame boundary
  gave a decoder transient 97-919 samples into the splice frame, with no click step. Hence the
  global frame grid and the same-signal boundary rule.
- **The first spike used its own libx264 wrapper** (with `forced-idr=1`), not `VideoRenditionEncoder`.
  The forced-IDR plumbing is slice 1 work (§4.2 step 2). Its claims are limited to MPEG-TS/H.264.
- **Not proven by the spike:** a GPU aux compose beside a GPU Program compose on the RTX 4060 (a
  shared device queue). That is a measured acceptance gate of slice 3 on mda001.

---

## 5. Audio

### 5.1 Rules

| Output's video feed | `audio = "follow"` (default) carries |
| --- | --- |
| Program | the program mix (today's behaviour) |
| Clean | the program mix |
| Preview | the PVW source's own audio |
| Source / VT / generator X | X's own audio. A source with no audio (solid, clock) gives silence; bars give the 1 kHz line-up tone (`crates/multiview-cli/src/pipeline.rs:4405-4410`) |
| Layout L | L's `audio` attribute: `program` (default, so routing a layout changes no one's audio versus today), `none`, or `source:<id>` of one of its cells |

An explicit `audio` overrides follow: `program`, `source:<id>`, `tracks:[...]` or `none`. Audio
follows the *feed*, so when a take changes the video feed, a `follow` output's audio changes on the
same tick.

### 5.2 Where routed audio comes from

There is no second decode and no second store read. Every source with audio is already a strip on
the program bus at gain 0 (`crates/multiview-cli/src/pipeline.rs:17548-17572`). Each tick, the bus pulls every strip's block
exactly once (`crates/multiview-audio/src/program.rs:386-395`). An `AudioStore` cannot be read twice (`crates/multiview-audio/src/store.rs:829`). A routed
output's audio block for tick T is therefore built on the bake consumer, right after `ProgramBus`
ticks, from:

- that tick's per-source clean blocks (`input_tracks()`); or
- the mixed program block.

**Audio uses the same two layers.**

- **Audio buses** carry PCM per tick. They are the program mix (`audio:program`), each source's
  own audio (`audio:src:<id>`), and a layout's audio (§5.1). The blocks come from the strips the
  program bus already pulls (above).
- **Encoded audio buses** are keyed by `(audio bus, AAC settings)`, with zero, one or many
  subscribers. Today one shared AAC encoder carries the program stream and every discrete track
  (`crates/multiview-cli/src/pipeline.rs:5795`, `crates/multiview-output/src/sink.rs:724`). That is the key `(audio:program, settings)` plus its tracks,
  unchanged with default config.
- **An output's audio subscription moves in the same take as its video, but in its own rebind**
  (`rebind_audio`, §4.3), keyed on the audio grid boundary G at or after the video move tick T
  (Rule 1 below), at most 21 ms later. Video is rebound at T, audio at G.

**Why audio needs its own rule.** Video moves at an IDR at tick T. Audio cannot move at the same
instant, for two reasons.

**AAC frames do not align with ticks.** An AAC frame is 1024 samples; a tick is 1601 or 1602
samples at 48 kHz and 30000/1001. A tick boundary and a frame boundary coincide only every 512
ticks (17 s).

**Two encoders have two frame grids.** Each encoder's frame grid starts at its own origin
(`audio_pts = 0` today, `crates/multiview-output/src/sink.rs:1141-1159`). Moving between two grids
mid-frame would duplicate or drop up to 1023 samples, and audio PTS would step unevenly.

**Rule 1: one global AAC frame grid.**

- Every encoded audio bus emits frames on frame boundaries `n × 1024` of the run's one sample
  counter (`SampleClock::total_at`).
- A bus opened live starts at the first grid boundary at or after `total_at(T0)`, via
  `starting_at(samples)` added to `AudioState`.
- So every bus has the same frame boundaries and the same PTS for the same samples.
- An output's audio moves at **grid boundary G**, the first frame boundary at or after
  `total_at(T)`. That is at most 1023 samples (21 ms) after the video move.
- Packets that start before G come from the old bus; packets from G come from the new one. The
  sample count and audio PTS are continuous by construction.
- The dispatcher's audio rebind keys on G, not on the video window.

**Rule 2: the same signal on both sides of G.** AAC overlaps consecutive frames (MDCT). A decoder
that receives frame G−1 from one encoder and frame G from another cancels the overlap cleanly only
if both encoders were coding the same signal around G. The spike (scratch) spliced two independent
AAC streams carrying different content: the decoded output was mixed from 97 to 919 samples into
the splice frame (no click step: largest sample step 379, against 381 in steady signal). So:

- **The output's audio moves onto a private encoded audio bus** for this take. It is fed the
  **old** audio up to G, so the boundary at G joins two encodings of the same signal. From G it
  crossfades old to new over one tick (§5.3), *inside that one stream*.
- When the take's destination is a **shared** encoded audio bus (another output already carries
  that audio with those settings), the private bus carries the new audio after the fade,
  sample-identical to the shared bus. The output then moves again, at the next grid boundary after
  the fade, from private to shared: the same signal, the same grid, so the boundary is clean. The
  private bus then closes. Subscribers of the shared bus never hear the fade.
  - **Encoder state, not just samples.** Two AAC encoders fed identical PCM can still differ at a
    frame boundary, through their MDCT history and bit-reservoir state. So the private-to-shared
    boundary at G′ is chosen at least 4 frames after the fade ends (85 ms), so both encoders have
    coded the same signal for longer than their history.
  - **As built (slice 2): a fixed rule plus a deterministic test, no runtime decoded check.** This
    deviates from the original plan of a decoded-identity check at run time. The rule: once a
    private bus carries the program mix again, no fade is pending or running, and it has been fed
    at least 4 AAC frames (4,096 samples) since its last fade ended, the output's program-audio
    slot moves back to the shared program stage at the next grid boundary G′ at or after
    everything fed (`rebind_audio(output, None, G′, None)`). When the dispatcher reports the move
    (`AudioApplied { key: None }`), the private bus closes: its stage retires and its job sender
    drops, never joined. Both encoders coded the identical post-loudnorm program PCM for at least
    4 frames before G′ (Rule 2). The acceptance criterion stays on the decoded output, but it is
    pinned by a test instead of measured per move
    (`a_private_bus_back_on_program_returns_to_the_shared_stream`): across G′, the maximum absolute
    difference between output 1 decoded and the shared stream decoded, over the 2,048 samples after
    G′, is at most twice their steady-state difference before G′. A bus that fails meanwhile
    returns through the dispatcher's `AudioFallback`. A take to a source and back therefore leaves
    no private bus after about 85 ms plus one fade.
  - Only the program mix returns to a shared stream: the shared program stage is the only shared
    encoded audio bus this build has. A private bus carrying another audio stays the output's own
    encoded audio bus (+1 audio encode, CPU) until the output's audio returns to the program mix.
- When no shared destination exists, the private bus simply becomes the output's new audio bus.

The private bus must open early enough to be warm at G: it opens when the take is admitted, which
is at least the video open time before T, so it has encoded several frames of the old audio before
G.

**Cost.** AAC is software (`aac`), not an NVENC session: a private bus costs about 2.7 % of one
core while it exists (§5.2 cost line), at most a few seconds per take. It is never refused for
encoder sessions. The plan shows it as `+1 audio encode (transient, CPU)`.

**Config identity.** Encoded audio buses with the same AAC settings produce the same
AudioSpecificConfig (profile, rate and channel layout). Slice 1 checks it byte for byte when a bus
opens, and a mismatch is `encoder-format-mismatch` (§4.2 step 4), as for video. The muxer's audio
init (FLV sequence header, fMP4 and MoQ init) is therefore unchanged by a move.

**Priming.** A live-opened bus's priming packet is never delivered: an output moves to it at G,
which is at least one frame after its first frame. A primed packet is the only negative-DTS
packet the dispatcher admits, and only at stream start
(`crates/multiview-cli/src/rendition_dispatch.rs:146-157`).

Default config gives today's single program AAC encoder and today's samples. It also starts on the
global grid, because today's encoder starts at sample 0.

### 5.3 Crossfade at a take

When a take changes an output's audio feed, the output's private encoded audio bus (§5.2) carries
the old audio up to grid boundary G, then a mix of old and new from G:

```text
out = old · cos(θ) + new · sin(θ)
```

θ rises from 0 to π/2 over **exactly** `SampleClock::total_at(T+1) − total_at(T)` samples: one output
tick, 1601 or 1602 samples at 48 kHz and 30000/1001, never a float. This is the equal-power envelope
of `GainRamp` (`crates/multiview-audio/src/mixer.rs:35-109`) that ADR-0104 §4 uses for AUTO, applied per output.

The ramp starts at G, at most 21 ms after the video cut, and lasts one tick of samples. It is
sample-accurate on the global sample counter.

**Where the encoded bus is shared:** the fade runs on the private bus only (§5.2), so other subscribers never hear it.

**Audio-only takes (slice 2).** A take that changes only an output's audio (`audio` without a
video feed) is adopted at the drain tick T. There is no fresh gate: a source without audio is
silence (§5.1). The private bus opens if absent (at the grid boundary at or after `total_at(T)`),
and the crossfade starts at the first grid boundary G at or after `max(total_at(T), warm, fed)`,
exactly as for a video take. The slot move at G is unconditional (no video move to follow). If the
bus's `AudioSpecificConfig` is not known yet, the bus keeps carrying the old audio and the fade's G
moves to the next grid boundary after the configuration arrives, so the move never lands before the
bus is checked. The take reports `applied` with the tick whose block holds G, once the bus has been
fed past G and the dispatcher reported the slot move. It is refused `encoder-not-ready` when the bus
fails, the configuration differs, the dispatcher drops the move, or 50 ticks
(`TAKE_PRIME_DEADLINE_TICKS`) pass first; a refusal restores the old choice and fades back. An
audio take that changes nothing applies at once. A combined take (video and audio) installs the
audio choice when the video move is issued, so the fade at G goes to the explicit choice; a
refused or superseded video move restores the previous choice. A video-only take keeps the current
choice: an explicit choice stays, `follow` follows the new video. A fallback to Program keeps an
explicit choice; `follow` follows Program.

**One fade at a time.** A change requested while a fade runs never overlaps it and never steps: it
starts from the running fade's target at the first grid boundary at or after the end of the
tick block in which that fade ends. A change not
yet started is replaced by a newer one (latest wins). The per-output state is O(1), whatever the
take rate.

### 5.4 Lip-sync for one-tick-delayed feeds

A `layout:<id>` or `bus:*` used **as a source** (§3) is at least one tick behind its sources. When
that feed's audio rides with it (for example a layout on Program via the M/E with
`audio = source:x`), the audio is delayed by the **measured** picture age:

- the age is `k = max(0, tick.pts − selected frame stamp)` in whole ticks, clamped at 0, because
  `read_at`'s earliest-retained fallback can return a frame stamped after `now`
  (`crates/multiview-framestore/src/tile.rs:592-640`). The stamp is the layout
  canvas's own publish stamp (`T.pts` of the tick it was composed for), which `source_times`
  reports for the selected tile (`crates/multiview-engine/src/drive.rs:71-73`);
- the delay is `total_at(T) − total_at(T−k)` samples, read from a bounded, preallocated ring sized
  for 4 ticks (a ceiling of 4 × 1602 = 6,408 samples per channel at 48 kHz, 30000/1001; the exact
  span varies between 6,406 and 6,407);
- **the delay target changes only when a new k holds for 30 ticks** (hysteresis), so a transient
  aux lag does not move the audio;
- a delay change is a **sample slip by resampling**, not a crossfade. Crossfading a signal with a
  delayed copy of itself would comb-filter. The read position moves at a rate offset of at most
  0.5 % (about 8.6 cents, inaudible), using linear interpolation. One tick (1,601-1,602 samples)
  is absorbed in about 6.7 s;
- for k > 4, the picture is a held frame aging into STALE by the store's own rule (`crates/multiview-framestore/src/tile.rs:521-530`).
  A held picture has no lip sync to keep, so the audio stays at the 4-tick delay. When the picture
  is fresh again, the delay slews back to the new k as above.

A layout routed **directly** to an output is not delayed: it composes from tick T's stores and is
encoded as frame T.

### 5.5 How it composes with what exists

- **`audio-routing` stays the source of per-source gain, mute, program-bus membership and track
  naming.** An output's `audio = source:x` carries x's clean block with x's route `gain_db` and
  `mute` applied. It does not apply `include_in_program_bus`, which concerns the mix only.
- **An output's existing `audio.mode/tracks` stays valid.** `tracks` is now one value of the output
  audio feed. Default outputs keep exactly today's `auto | program | tracks` semantics and the one
  shared AAC encoder.
- **Any output gets any audio, independently.** Two outputs with identical settings can carry
  different audio: they subscribe to different encoded audio buses.
- **On a routed output**, the routed audio bus takes the program-audio stream slot. Discrete tracks
  stay on the encoded audio bus that carries them today.
- **Capability as built (slice 2).** The routed audio always rides the output's single
  program-audio slot (track 0 of its binding). So an HLS output carries exactly one routed track,
  and its discrete tracks are untouched. A `track:<name>` choice carries that track's input block
  without the route gain, exactly as the discrete-track encode does; a `source:<id>` choice applies
  the route's live `gain_db` and `mute`. The run publishes per output whether it has that slot
  (`RunOutput::program_audio`), so the control plane can refuse before enqueue. An output without
  it, and every audio take on a run without program audio, is refused `unroutable` by the engine.
  NDI, AES67 and display outputs have no packet binding with a program-audio slot in this build, so
  they are refused for audio: the control plane answers a typed `audio-unsupported` (409) from the
  plan before enqueue (§6.2). There is no NDI routed-audio path yet. Config validation keeps the per-transport rules
  (`Output::audio_capability`): a `{ tracks = [..] }` feed holds more than one track only where the
  transport carries tracks simultaneously (SRT, RIST, RTSP, multitrack RTMP); HLS/LL-HLS
  (select-one), NDI/AES67/display (no tracks), legacy RTMP and WebRTC (one) take at most one.
- **The rename propagation of ADR-R005 §4.3 extends to output audio feeds:**
  `propagate_routing_feed_change` (`crates/multiview-config/src/audio.rs`), the sibling of
  `propagate_routing_change`, rewrites `[[routing.output]] audio = { tracks = [..] }` to a renamed
  track and drops a removed one (an emptied feed becomes `program`). `PUT /api/v1/audio-routing`
  commits it in the same prospective document and reports it in `outputs_updated`.
- **Start-up re-applies a persisted audio feed.** An explicit audio feed in the config is admitted
  at start-up (`outputs_carry_program` no longer refuses it). The run starts every output on
  `follow` and, right after its routing handle is published, issues one **audio-only boot take**
  per output whose persisted audio is not `follow` (`Pipeline::issue_boot_audio_takes`, operation
  id `boot:audio:<output>`). An audio-only take has no fresh gate, so there is no retry: a boot take
  that cannot be enqueued or is refused (an output without a program-audio slot, a multi-track or
  undeclared-track feed) is logged, the output stays on `follow`, and the persisted value is kept.
  When the same row also persists a non-Program video feed, the audio rides that output's video
  **boot take** instead (one combined take, `boot:<output>`, see Restart below), so the two never
  supersede each other; if that boot take is given up, the router issues its audio alone as
  `boot:audio:<output>` (no fresh gate), so a refused video never drops the persisted audio.

---

## 6. API and config

### 6.1 Config (backward compatible)

```toml
# Absent [routing.output] entry for an output = Program, exactly today's behaviour.
[[routing.output]]
output = "restream"
video  = { layout = "NewsOne" }   # | { bus = "program" | "preview" | "clean" } | { source = "vt1" }
audio  = "follow"                 # | "program" | "none" | { source = "cnn-usa" } | { tracks = ["eng"] }

[[routing.output]]
output = "srt-multiview"
video  = { source = "bars" }      # bars carries the line-up tone under "follow"

[[layouts]]                       # stored layouts gain an optional audio attribute
id = "NewsOne"
audio = "program"                 # | "none" | { source = "<a cell's source>" }
```

The existing `bus` field (`crates/multiview-config/src/routing.rs:236`) stays as an alias: `bus = "preview"` means
`video = { bus = "preview" }`. When both are present they must agree, or validation fails. The
desugar (`crates/multiview-config/src/lib.rs:1590-1600`) keeps writing Program.

**Audio rows (slice 2).** `audio` in a `[[routing.output]]` row is accepted at start-up and
re-applied by a boot audio take (§5.5). A take persists its audio feed with its video: an applied
audio-only take writes `audio` and keeps the row's video; a row that ends on Program video and
`follow` audio is removed (absent = the default). A boot outcome (any id under `boot:`, `is_boot_take`) is never persisted, applied or not.

A take is persisted **only after its `applied` outcome**, never on `refused:*` or `superseded`, under
the same config-mutation lock `apply-layout` uses (`crates/multiview-control/src/routes/mod.rs:811`). This differs from
`apply-layout`, which commits before the engine adopts (`crates/multiview-control/src/routes/mod.rs:876-901`): a route take can
still be refused by the fresh gate after the 202. A take back to Program removes the output's row, unless the boot file
routes that output elsewhere: then an explicit Program row is kept, so a restart does not re-seed the boot row.

**Restart (#329).** `routing.output` rows are **store-backed**: start-up takes them from the accepted
state (`desired.json`, where `persist_feed` committed them), not from the boot file. The other routing
tables (video, audio and subtitle crosspoints) stay restart-only and are spliced from the boot file as
before. Per output, the accepted row wins; a boot-file `[[routing.output]]` row seeds an output the
accepted state has no row for. The same rule applies to the accepted state, an `active.toml` resume and
a last-good fallback (`crates/multiview-cli/src/boot.rs`, `splice_output_routing`).

The engine itself always starts every output on Program. Right after the run's routing handle is
published, before the clock starts, it enqueues one **boot take** per output whose persisted feed is not
Program: `operation_id = "boot:<output config id>"`, principal `system:boot`. A boot take goes through
the normal gate (fresh within 50 ticks, capacity), so a source that is not up yet is refused
`feed-not-fresh`. The router on the bake consumer then re-issues it at most once every 2 s, for up to
30 s after the run's first tick. This state is O(outputs), lives off the clock on the consumer that
already owns the take state machine, and needs no extra thread or timer. An operator take on the same
output ends the boot take for good. After the window, or on a refusal that cannot clear (unroutable,
format mismatch), the output stays on Program, a warning is logged, and the routing snapshot counts it
(`boot_takes_given_up`). A refused or expired boot take never deletes the persisted feed: the operator's
intent stays. An applied boot take is not persisted either: every boot id (`boot:<id>`, `boot:audio:<id>`) shares the one
`BOOT_TAKE_PREFIX` in `multiview-control`, and the outcome relay skips persistence for `is_boot_take` ids.

**What the operator sees when a boot take is refused.** `GET /routing` shows `video: {"bus":
"program"}` with tally `program` (what is on air) and `configured_video` with the persisted feed, each
redacted by its own read rule. The Outputs page shows the live feed (and the live `effective_audio` in the
Audio row) in the select and a "configured: X (not on air)" note. Start-up
admission (`outputs_carry_program`, the single start-up rule) refuses only a feed the engine cannot
carry at all: a layout, which validation already refuses until slice 3.

### 6.2 HTTP surface

The routing resource is extended rather than adding a new one. `/routing/{kind}/take` already
carries plan/take, the `Idempotency-Key`, BOLA on both ends, live-apply classes and the UI's
plan/take flow. An output crosspoint is a crosspoint. A separate `/outputs/{id}/feed` resource would
duplicate all of that and split the matrix UI over two APIs. Output CRUD stays restart-class for
*format*; the feed is routing.

```text
GET  /api/v1/routing                      whole routing state (new)
POST /api/v1/routing/plan                 now also accepts output targets; returns cost
POST /api/v1/routing/output/take          new kind "output"
POST /api/v1/routing/salvo/plan|take      several crosspoints, one tick (new)
```

`GET /api/v1/routing`, filtered per object for scoped principals. When the engine publishes a live
snapshot for an output, `video`, `tally` and `effective_audio` describe what is **on air** (the live
bus), and `configured_video` is the persisted feed. They differ after a refused boot take or a
fallback. Each field is redacted by its own per-object rule. Without a live snapshot (no run), both
fields are the configured feed and `state` is `configured`.

```json
{ "tick": 1843021,
  "outputs": [
    { "output": "restream", "video": {"layout": "NewsOne"}, "configured_video": {"layout": "NewsOne"}, "audio": "follow",
      "effective_audio": "program", "encoded_bus": "aux:NewsOne/h264-1080p", "state": "on_air",
      "since_tick": 1841990, "pending": null },
    { "output": "srt-multiview", "video": {"bus": "program"}, "configured_video": {"bus": "program"}, "audio": "follow",
      "effective_audio": "program", "state": "on_air", "since_tick": 0,
      "pending": {"video": {"source": "vt1"}, "held_ticks": 12, "operation_id": "…"} } ],
  "cells": [ … existing video/audio/subtitle crosspoints … ],
  "buses": { "picture": ["program", "aux:NewsOne"], "active_aux_canvases": 1, "max_aux_canvases": 2,
             "encoders": 3, "encoder_sessions": "6 of 12 in use", "dropped_ticks": 0 } }
```

**`POST /api/v1/routing/plan`** with an output target is a dry run:

```json
{ "target": {"kind": "output", "output": "restream", "stream": "video"},
  "feed": {"layout": "NewsOne"} }
```

The response gives the class, the cost and the refusals. `new_encoders` is `0` when the feed is
Program, when another output with the same settings already carries the bus, or when a live-opened
encoder for (bus, settings) still runs, a lingering one (§4.3) included: the take reuses it. The
engine publishes those keys in its readback (`live_keys`; failed keys excluded).

```json
{ "class": "class1",
  "cost": { "new_aux_canvases": 1, "new_encoders": 1, "encoder_sessions": "6 of 12 in use",
            "decode_resizes": ["cnn-usa"] },
  "refusals": [] }
```

Possible refusals (typed, RFC 9457 `type` suffixes):

- `capacity` (with `needed` and `available` per resource);
- `feed-cycle` (with `path`);
- `unknown-feed`;
- `encoder-format-mismatch` (internal fault, §4.2 step 4);
- `switcher-required` (Preview or Clean without an M/E);
- `output-kind-unroutable`;
- `audio-unsupported` (no program-audio slot, a transport without discrete tracks, or more than
  one track).

The class is `class1` for every feed change, because the format never changes (§4.2). A format
change is not offered here.

**Audio in plan and take (slice 2, as built).** `target.stream` is `video` or `audio`. `feed` is the
video feed; `audio` is the full audio feed, externally tagged like the config `audio`:
`"follow" | "program" | "none" | {"source": "<id>"} | {"tracks": ["<name>"]}`.

- `stream: "video"` + `feed` (+ optional `audio`): a video take, with the audio taken together.
  Absent `audio` keeps the output's audio choice (slice-1 bodies, `audio: "follow"` included,
  behave as before).
- `stream: "audio"` + `audio`, no `feed`: an **audio-only take**; the video is untouched.
- Any other combination is 422 with a detail naming the missing or extra field.

```json
{ "target": {"kind": "output", "output": "restream", "stream": "audio"},
  "audio": {"source": "cnn-usa"} }
```

A track resolves to its `[audio].routes[].target_track` route `input_id`. Only one track is
takeable: the routed audio rides the output's single program-audio slot (§5.5), so
`{"tracks": [a, b]}` is 422 `audio-unsupported` ("This output carries one routed audio feed;
choose one track"). An unknown source or track is 422 `unknown-feed`. An output without a
program-audio slot (`RunOutput::program_audio == false`: NDI, AES67, display, or a run without
program audio) is 409 `audio-unsupported` ("This output cannot carry a routed audio feed"); a
`tracks` feed on a transport whose `Output::audio_capability()` delivery is `None` is 409
`audio-unsupported` too. The plan reports the same refusals in `refusals`.

The plan's cost gains `audio_encoders` (`0` or `1`) and `audio_cost`: `1` and
"+1 audio encode (transient, CPU)" when the take changes what the output carries and the output has
no private audio bus yet; `0` (and no `audio_cost`; the UI says "No extra encoder") when the target
is what it already carries or a private bus already runs. The engine readback carries per output
the carried audio, the pending take's audio and whether a private bus runs. `GET /routing` keeps
`audio` as the persisted choice; `effective_audio` comes from the live readback when present, and
`pending.audio` shows a pending take's audio; all redacted per §6.3. `route.output` is unchanged
in shape and covers audio-only takes.

**`POST /api/v1/routing/output/take`.** The body is the same as plan: a take carries a video feed,
an audio choice, or both (`stream: "audio"` is an audio-only take). It requires an
`Idempotency-Key`. The response is
`202 {operation_id, status: "accepted"|"pending"|"settled", adopt?: "next_tick", outcome?, tick?}`.
`accepted` (with `adopt: "next_tick"`) answers only the request that enqueued the take. A replay of
the key answers the same `operation_id` with `pending` (no outcome yet) or `settled` (plus the
recorded `outcome` and its `tick`); it never carries `adopt` and never enqueues again.

The take's idempotency record is body-blind. It is keyed by principal (API key id), method and path
plus the raw key (the `IdempotencyKey` extractor in `crates/multiview-control/src/concurrency.rs`),
so two principals that send the same raw key get two separate takes. A reused key with a
**different** body is still a replay of the first operation: nothing is applied a second time, and
clients must mint a new key for every distinct take. A replay returns the recorded `operation_id`
and, once known, its terminal outcome (`applied`, `superseded`, `refused:*`), never a fresh 202 that
implies the feed is on air.

The replay decision is owned by a **take-scoped** store
(`OutputRoutingControl` in `crates/multiview-control/src/output_routing.rs`), not by the
process-global `IdempotencyStore`. Each record holds `{operation_id, take, outcome}`. The route does
not use the global store at all: other routes `release`/`rebind` keys there, and a take's replay
must not depend on them. The take store is bounded at 4096 records in total and 256 **per principal**.
A **pending** record (one with no outcome yet) is never evicted, so a key answered `accepted` always
replays its own operation while that operation is pending. Under pressure, **settled** records are
evicted oldest first. When a principal reaches its own cap, its own oldest settled record goes first. If
all of that principal's records are pending, that principal gets `503 idempotency-capacity` and other
principals are unaffected. At the global cap, the oldest settled record of any principal goes. Only
when all 4096 records are pending does the store fail closed for everyone. This is the documented
trade-off: **a replay of an evicted (settled) key is a new take**, because the store no longer knows
the key. Clients already must mint a new key per distinct take, and a replay long after its settlement
re-applies a feed the operator already chose. A record is also removed when its own enqueue failed
(`engine-busy`, `output-kind-unroutable`), which frees the key for a retry. The control-side
`route.output` subscriber writes the outcome when the engine publishes it. A salvo reserves its key
only after every member is authorized and validated. So a denied or invalid salvo burns no key, and
its replay can never return a 202 for an operation that never enqueued. The outcome arrives as the realtime event
`route.output {output, operation_id, tick, outcome}`, with `outcome` one of:

- `applied`
- `superseded`
- `refused:feed-not-fresh`
- `refused:feed-cycle`
- `refused:capacity`
- `refused:encoder-not-ready`
- `fallback:aux-failed`

The event is correlated by a new `CorrKey::Route { output }`. Today's video, audio and subtitle
takes stay uncorrelated (`crates/multiview-control/src/realtime.rs:209-222`) until they move onto the same event.

**`POST /api/v1/routing/salvo/take`** takes a list of crosspoints: output feeds plus the existing
cell, audio and subtitle crosspoints. It is validated as a set: all authorized, no cycle, the total
cost admitted. Every member is authorized **before** the idempotency reservation and the enqueue,
and any denial is a 403 that enqueues nothing. The salvo is adopted **at one tick or not at all**:

- if any output feed in it is not fresh at the deadline, the whole salvo is refused and nothing
  changes;
- if a single take for one of its members is adopted before the salvo's tick, the salvo is refused
  as `superseded` as a whole;
- admission (capacity and cycles) is re-checked at adoption, not only at plan. A salvo with no output
crosspoints keeps today's salvo semantics. The stored `Salvo` document (`crates/multiview-config/src/salvo.rs:59`)
gains `output: Vec<OutputRecall>`, the RT-16 shape reused.

### 6.3 Authorization (per object, both ends)

A take or plan requires:

| Object | Requirement |
| --- | --- |
| Destination | Role `Write` (`principal.role.require(Action::Write)`, as `crates/multiview-control/src/routes/routing.rs:151`) **and** both axes of the output: `authorize_scope(principal, AuthzScope::ObjectAndOutput { object: output, output })` (#329). The object axis (`scoped_object_ids`) is the one the output routes use (`crates/multiview-control/src/routes/outputs.rs:77,110`). The output axis (`scoped_output_ids`, ADR-W026) is checked as well, so a key confined to some outputs can neither plan nor take another, nor read its row or its events |
| Feed `source:x` | `authorize_object(principal, x)` with Read (`crates/multiview-control/src/auth.rs:336`) |
| Feed `layout:L` | Read on L **and on every source its cells bind**. A layout picture shows those sources' pixels, so a principal limited to L could otherwise route a source it cannot read |
| Feed `bus:*` (Program, Preview, Clean) and `layout:*` cells binding `bus:*` | `authorize_scope(Program("main"))` plus `authorize_object(me1)`, as `authorize_refs` does (`crates/multiview-control/src/routes/switcher.rs:99-112`). **This deliberately replaces per-source Read**: a bus shows whatever the M/E selects, now or later, so the grant is the Program scope itself. Only principals that already hold Program scope can route a bus |
| Audio feed `source:x` | `authorize_object(principal, x)` |
| Audio feed `program`, and `follow` of a bus video | the bus rule above: `authorize_scope(Program("main"))` plus `authorize_object(me1)` (`follow` is covered by the video end) |
| Audio feed `tracks:[t]` | `authorize_object(principal, t)` **and** `authorize_object(principal, <t's route input_id>)`: the track carries that source's audio |
| Audio feed `none`, `follow` | nothing beyond the output (and the video end, when present) |
| Salvo | every crosspoint independently, all before enqueue |

The audio-feed rules are the same as `may_read_audio` in `GET /routing` (plus the track's route
input), so readback never discloses an audio feed a take would refuse. They run before existence:
an audio source or track the caller may not read is 403 whether or not it exists.

**The rule is checked on every change, not only at the take.** A layout routed to an output can be
edited after the take. The principal making a change is checked against every output the change
affects:

- **A layout update or apply that adds a cell source** (`update_layout`, today authorized only on
  the layout id, `crates/multiview-control/src/routes/mod.rs:331-332`) also requires, when that layout is currently an output's
  feed (directly, or through `layout:` cells), that the editor has Write on each such output and
  Read on the added source. Otherwise the edit gets 403 `routed-layout`, naming the affected outputs
  the editor may read and a count of the others. The editor who can read the source and publishes
  it onto an output is accountable for that exposure.
- **One source-closure function** computes every source a feed shows, transitively through
  `layout:` cells and `bus:` cells (through the M/E's current selection). It is used by:
  - output takes;
  - **PVW/PGM/AUTO selects.** A select of `layout:L` requires Read on L's whole closure. Today
    `authorize_refs` checks only the selected id (`crates/multiview-control/src/routes/switcher.rs:102-104`), and slice 4 extends it;
  - layout edits. An edit of a layout that is in any output feed's closure **or** in the current
    PVW/PGM selection's closure requires Read on the added sources and Write on the affected
    outputs.

  So whatever reaches a bus was selected by a principal who could read all of it. An output
  carrying a bus shows whatever the M/E selects; that is the deliberate Program-scope grant in the
  table above.
- **The engine records the taking principal's id** with each output's current feed (in `GET /routing`
  for auditors with whole-system access), so any later exposure can be traced.

**The same redaction applies to every body:** `GET /routing`, plan responses, 409 bodies and
`route.output` events.

- Outputs and cells the principal may not read are omitted.
- A feed, a `buses.picture` entry, a `decode_resizes` entry, a capacity `holders` entry or a
  `feed-cycle` path node the principal may not read becomes `{"redacted": true}` or a count, never
  an id.
- Encoder-session occupancy (`"N of M in use"`) is shown only to whole-system principals
  (`require_unscoped_for_whole_system`). A scoped principal gets its own cost (`+0` or `+1`
  encoder) and a boolean `fits`. Its capacity refusal reads "No free encoder session for this
  output", without counts.
- `route.output` is delivered with `AuthzScope::ObjectAndOutput { object: output, output }`, the same two axes
  as the REST check. `GET /routing` omits the rows that fail this rule.
- Authorization runs before existence, as the existing routes do (`crates/multiview-control/src/routes/routing.rs:164-169`,
  `crates/multiview-control/src/routes/switcher.rs:288-289`). So an id the caller may not read gets 403 whether or not it exists.

### 6.4 Abuse: take floods

The engine takes at most one effective cut per output per tick, because the latest take wins at the
drain (spike: about 20,000 takes caused 4 cuts and no clock effect). There is one pending take per
output **across audio and video**: an audio-only take supersedes a pending video take and vice
versa (the older one reports `superseded`). An audio change that arrives while a fade runs waits
for that fade's end (§5.3), so a flood of audio takes is at most one effective audio change per
output per tick and never two overlapping fades. Forced IDRs are a bitrate cost,
so the control plane limits takes to 2 per second **per output** and 10 per second **per
principal**. Forced IDRs are limited **per encoded bus** (§4.2 step 1): one per 2 s, shared with the
viewer-join/PLI path.

`crates/multiview-ffmpeg/src/encode.rs:208-216` documents a caller-owned floor of at least 2 s for forced keyframes. That floor
is for viewer-join and PLI requests. A take uses the same 2 s per-bus floor, enforced at adoption. A
take within it moves at the bus's next natural IDR (at most one GOP). Each forced IDR costs one deep copy of the frame
(`crates/multiview-ffmpeg/src/encode.rs:310-318`), at most 2 per second per output. A take over either limit gets 429 with `Retry-After`. The existing per-IP limiter
(`crates/multiview-control/src/limits.rs`) still applies first. Idempotency keys bound replays.

The take mailbox is bounded (64) and **shared by all principals**: when it is full, every
principal's take gets 503 `engine-busy`. The per-principal limit keeps one principal from filling
it, because 10 takes per second drain in under a tick.

**Breaking change.** `POST /commands/apply-layout` on a run with `[switcher]` now answers 422
`switcher-owns-program`. Today it answers 202 and breaks the M/E (`crates/multiview-control/src/routes/mod.rs:756-786`,
`crates/multiview-engine/src/switcher_hook.rs:66`). Its optional `target: {output}` alias runs the output take's full path: both
authorization ends, idempotency, the limits and persistence only after `applied`. It does not use
`apply-layout`'s layout-only check (`crates/multiview-control/src/routes/mod.rs:802`) or its commit-before-adopt
(`crates/multiview-control/src/routes/mod.rs:876-901`). The 422 `switcher-owns-program` is returned **before** the idempotency
reservation (today the route reserves first, `crates/multiview-control/src/routes/mod.rs:815`), so a refused call does not burn
the key.

---

## 7. Cost, limits, admission (invariant #9)

Measured on the build host (Xeon Gold 6132, under load). Compose numbers come from the spike's
`bench` mode: 60 composes, the first 10 discarded.

| Work | Per frame | At 30 fps |
| --- | --- | --- |
| CPU reference compositor, 1280x720, 6 cells, 1 core | p50 198 ms | not viable (about 6 cores) |
| same, 10 cores | p50 44 ms (6-cell), 42 ms (1-cell) | over the 33.4 ms budget |
| same, 1920x1080, 10 cores | p50 85 ms | not viable |
| Integer box blit (ADR-0103 wall algorithm), 1280x720, 6 cells, 1 core | p50 9.4 ms | 28 % of one core |
| same, 1920x1080 | p50 14.1 ms | 42 % of one core |
| One-cell identity (tile = canvas size and tag) | about 0 | about 0 |
| Stereo AAC 128k encode | about 0.9 ms | 2.7 % of one core |
| NVENC 1080p30 H.264 session (mda001, existing) | 3.3-3.8 ms latency | +1 of the driver's session cap |

**Consequences:**

- **The CPU reference compositor cannot run aux canvases at full cadence.** It is a correctness
  reference, and it costs about 200 ms per 720p frame on one core. In the default (GPU-free) build,
  aux canvases use the GPU-free integer path in **every** build, including mda001, until slice 3's
  measurement says otherwise. The identity fast path covers same-size sources. A layout or a
  letterboxed source uses the wall's integer box scaler; samples are copied, because every tile
  already carries the canvas tag (§2.4). A 6-cell 1080p layout costs 14.1 ms per frame on one core.
  wgpu aux composition (with borders, fit modes and the full colour pipeline) is an **optional
  upgrade in slice 3**, enabled only if it passes the Program protection bound (§10).
- **On mda001** the 4060 runs 5 NVENC sessions at 21 % encoder load. The driver limits sessions
  per system to a moving number (12 on current drivers; `docs/architecture/hardware-and-efficiency.md:155`),
  probed through `nvmlDeviceGetEncoderSessions`. A take adds an encoder session only when its
  destination encoded bus does not exist yet ("+1 encoder"). A lingering bus closes after 10 s and
  frees its session.

**Admission.** A take, salvo or config apply is admitted only if all of these hold:

1. `active_aux_canvases + new ≤ routing.max_aux_canvases` (default 2; 0 disables aux canvases);
2. on the CPU path, each new aux canvas passes the CPU rule above;
3. a take that needs a new encoder fits `probed_free_encoder_sessions − non_routed_sessions`,
   checked at plan and again at the open. Here `non_routed_sessions` is computed from
   the config, not assumed: 1 for the wall when `MULTIVIEW_MOQ_WALL=1`, plus
   `webrtc.preview_max_focus` WHEP encoders, plus the optional preview ladder's rungs. On mda001
   that is 1 + 2 = 3. With today's 2 program encoders, and 12 sessions per system on the current
   driver, 7 are left for routed encoded buses. `routing.max_encoders` (default 6) caps the total.
   The refusal reads, in the operator's terms: "This output needs its own encoder and the GPU has
   no free encoder session (N of M in use)";
4. the number of decode resizes forced by routing stays within `routing.max_decode_escalations`
   (default 2), with the plan showing their MP/s.

The plan shows the arithmetic. A refused take returns 409 `capacity` with
`{resource, needed, available, holders: [outputs whose feeds hold aux canvases]}`, redacted per
§6.3, so the operator sees what to free. The UI renders that as the disabled reason (§8).

**Never degrade Program.**

- Program is composed on the clock task, before the aux thread is even woken (`after_compose`).
- The aux thread runs at lower OS priority.
- On the GPU, aux work is submitted only after Program's tick has been submitted.
- If the aux thread falls behind, its ticks are dropped at the capacity-4 channel, and aux picture buses
  hold last-good on their picture bus (counted as `routing.dropped_ticks`). After 60 consecutive held
  ticks the alarm `routing.degraded` is raised (§4.2 step 3).
- If Program's own lateness p99 over a 10 s window exceeds 50 % of the tick, the aux thread halves
  its cadence (composes every second tick; picture buses hold) until the window recovers. It never
  touches Program.
- **On the GPU this is a scheduling argument, not yet a proof.** A wgpu queue is shared, and aux
  submissions can delay a later Program submission. Slice 3, the first slice that composes on the
  GPU off-clock, has a hard gate on mda001: the Program protection bound of §10, with two aux
  layout canvases active for an hour. If the gate fails, slice 3 ships on the
  CPU integer path, which slice 3 measures and gates first, and wgpu aux stays off. A second-GPU
  placement is not offered: one `AdmissionPick` pins compositor and decode per run
  (`crates/multiview-cli/src/pipeline.rs:4130-4180`). Slices 1-2 compose no GPU aux work.
- The spike shows this order holds: routed outputs at 110/900 fresh while Program had 0 repeats.

---

## 8. UI

The existing Routing page (`web/src/pages/RoutingPage.tsx`, built on `CrosspointGrid` in
`web/src/pages/approved/RoutingSurface.tsx:122-175`, which already shows real destinations with
"state Unknown, no readback") becomes a real matrix. It reads `GET /routing` for readback, which
removes the "Unknown". It follows the console conventions in `web/src/components/broadcast/`:

- `TouchAction` keys of at least 48 px with a visible `disabledReason`;
- `OperationFeedback`, where accepted is not applied and a timeout is not a failure;
- `ProducerStatus`, where the tick is the only truth;
- the global take lock, `useTakeLock` (`web/src/components/broadcast/BroadcastShell.tsx:19`).

Desktop sketch:

```text
┌ Routing ─────────────────────────────────────────────── [Video|Audio]  🔒 Take lock ┐
│ Feeds ↓  /  Destinations →  │ OUTPUTS               │ M/E me1   │ LAYOUT CELLS (NewsOne)│
│                             │ restream │ srt-multi  │ PGM │ PVW │ c1 │ c2 │ … │ c6     │
├ BUSES ──────────────────────┼──────────┼────────────┼─────┼─────┼────┼────┼───┼────────┤
│ Program                     │    ●     │            │  —  │  —  │ ○  │    │   │        │
│ Preview                     │          │            │  —  │  —  │    │    │   │        │
│ Clean                       │          │            │  —  │  —  │    │    │   │        │
├ LAYOUTS ────────────────────┼──────────┼────────────┼─────┼─────┼────┼────┼───┼────────┤
│ NewsOne                     │          │     ◐      │     │     │ ⊘  │    │   │        │
├ PLAYERS (VT) ───────────────┼──────────┼────────────┼─────┼─────┼────┼────┼───┼────────┤
│ vt1                         │          │            │     │     │    │    │   │        │
├ GENERATORS ─────────────────┼──────────┼────────────┼─────┼─────┼────┼────┼───┼────────┤
│ bars · solid · clock        │          │            │     │     │    │    │   │        │
├ SOURCES ────────────────────┼──────────┼────────────┼─────┼─────┼────┼────┼───┼────────┤
│ cnn-usa                     │          │            │ PGM │     │ ●  │    │   │        │
│ times-square (failed)       │    ⊘     │     ⊘      │  ⊘  │  ⊘  │ ⊘  │    │   │        │
└─────────────────────────────┴──────────┴────────────┴─────┴─────┴────┴────┴───┴────────┘
 Plan: NewsOne → srt-multiview   class1 · +1 canvas (1 of 2) · +0 encoders
 [ Plan ]  [ TAKE ]   Last: restream ← Program at tick 1841990 (applied)
```

**Crosspoint states** (each with an accessible name and a tooltip):

| Glyph | State | Meaning |
| --- | --- | --- |
| ● | `on-air` | the destination carries this feed (readback tick advancing) |
| ◐ | `planned` | selected and planned, not taken |
| ◌ | `pending` | taken, held by the fresh gate; shows held ticks out of 50 |
| ○ | `available` | a take is allowed |
| ⊘ | `disabled` | with the reason: `not-fresh` · `would-cycle` · `capacity (needs 1 encoder, 0 free)` · `not-authorized` · `switcher-required` · `unroutable-output (ndi build missing)` |
| — | `n/a` | the destination cannot carry this feed kind |
| ? | `unknown` | no readback, or the tick stopped |

**Page states:**

- `loading`;
- `live` (tick advancing);
- `stale`: tick frozen for more than 2 s. Takes stay enabled, with an "engine not confirming" warning;
- `disconnected`: management connection lost. Takes are disabled;
- `read-only`: the principal lacks Write;
- `salvo-arming`: multi-select, then "Take all".

**Tally.** Each destination column header shows what it carries now, as **"PGM"** (follows the
switcher) or **"direct: cnn-usa"** (ignores the switcher). These stay distinct even when Program
currently shows cnn-usa. Each feed row shows a chip for
every output it is on (red for Program and the outputs carrying it, green for Preview).

The **Audio tab** has the same matrix, with audio feeds (`follow`, the program mix, each source, the
discrete tracks, `none`) against outputs, the program bus channel and discrete tracks. `follow` shows
what it currently resolves to.

**Take lock.** When locked, a take that changes a destination currently carrying Program opens the
existing confirm dialog (`web/src/pages/RoutingPage.tsx:300-342`).

**Phone.** Use the existing two-step `DestinationPicker`, then `SourcePicker`.

**Outputs page shortcut** (`web/src/pages/OutputsPage.tsx`). Each output card shows two always-visible
rows:

```text
Video  [ Program (follows the switcher) ▾ ]  on air since 12:04:31     Audio  [ Follow (program mix) ▾ ]
       Plan: bars direct → +1 encoder (6 of 12 encoder sessions in use)   [ Take ]
```

The dropdown is grouped Buses / Layouts / Players / Generators / Sources, with disabled items and
their reasons. Choosing an item runs plan, shows the cost line, and offers [Take]. The outcome uses
`OperationFeedback`.

**Audio row as built (slice 2, `web/src/pages/OutputAudioRow.tsx`).** The select starts on the
persisted choice (`GET /routing` `audio`); `follow` reads "Follow (<effective audio>)". Groups:
Follow; Program mix; Silence; Sources (the sources the page already loads for the video row);
Tracks (`GET /api/v1/audio-routing` `selectable_tracks` without `prog`; while that is unknown the
group is omitted and the row says so). Choosing an item runs plan with `stream: "audio"`, shows
the `audio_cost` line or "No extra encoder", shows a refusal as the Take key's disabled reason,
and [Take] posts the audio-only take with an `Idempotency-Key`; the outcome uses the same
`OperationFeedback` pattern as the video row. Output CRUD stays restart-class for format, and the callout says so; the feed
row is live.

---

## 9. Test plan

**Unit tests:**

- config: absent `[routing.output]` desugars to Program;
- `bus` alias agreement;
- feed validation (unknown ids, Preview without an M/E);
- `audio` follow resolution for every feed kind;
- cycle detection over buses and layouts, including through the M/E selection;
- plan cost arithmetic and admission;
- rename propagation into output feeds;
- the per-output take limiter.

**Engine integration** (`crates/multiview-engine/tests`, `ManualTimeSource` + `CooperativePacer`, the
`switcher_slice2.rs` harness style):

- a take adopts on exactly the next processed tick;
- latest-wins and the `superseded` outcome;
- the fresh gate holds and then refuses at 50 ticks, with the last-good feed kept;
- the generation-keyed IDR lands on the first new-feed frame even when the cut frame is dropped;
- gap fill keeps one frame per tick;
- a layout-as-source is one tick behind with a fresh gate;
- a cycle found at the tick is refused.

**Media checks** (`multiview-cli`, a real libav encode, as `actual_h264_pipeline_http` does). For an
H.264 and an HEVC output, ffprobe asserts:

- a constant PTS step and DTS = PTS across 5 cuts;
- `K` exactly at the cut ticks;
- identical SPS/PPS bytes across the cuts;
- no `EXT-X-DISCONTINUITY` in the HLS playlist;
- colour tags unchanged.

Also checked:

- a subscription move: the destination encoder (existing or opened live at T0) has byte-identical
  parameter sets, the output's first packet after the move is a decoder-ready RAP, the PTS step is
  constant, and the stream count is unchanged;
- a live open never holds the bake consumer (an injected 2 s encoder open: Program unaffected,
  consumer offers dropped for the new worker only);
- the dispatcher registers a new key before its first packet; an unbound or unknown key's window
  is dropped and counted, and Program delivery is never aborted; a key whose time base differs from
  the cadence is refused at registration;
- AAC private-to-shared move: the decoded-identity criterion across G′, and the 2 s lifetime
  fallback;
- a close (EOF, flush, join) runs on the reaper; an injected 3 s flush does not move the
  consumer's tick;
- two takes on adjacent ticks against one free session: one is admitted, the other refused, never
  both;
- linger-close after 10 s and reuse within 10 s;
- a held aux frame keeps its old generation, so the IDR lands on the first produced new-feed frame
  (drops injected at the seam);
- more than 8 dropped aux ticks do not trip the consumer's `FRAME_SUBSTITUTION_LIMIT`;
- **consumer stall:** a stalled bake consumer substitutes each picture bus's own last frame and
  generation, never Program pixels into another bus's encoder;
- **aux sizing:** routing a 640x360-planned source raises its decode to canvas size through the
  handover, and the plan's `decode_resizes` and MP/s match;
- **colour parity:** a routed source is byte-identical to the same source on a full-canvas Program
  cell;
- a dead aux thread falls back to Program with an IDR;
- `forced-idr=1` gives a true IDR (`is_idr`) on libx264 and NVENC, and a decode of the cut GOP
  references nothing before the cut;
- per protocol, as each slice ships it: HLS segment rotation at the IDR and no discontinuity;
  LL-HLS init and part stability; TS PCR and continuity counters; FLV with no new sequence header;
  WebRTC RTP timestamp continuity; MoQ group start at the IDR.

**Audio checks:**

- the crossfade spans exactly `total_at(T+1) − total_at(T)` samples at 30000/1001;
- no sample over the old or new peak;
- `follow` switches on the video tick;
- the measured-age delay for layout-as-source (sample-exact at k = 1, ramped when k changes under
  injected aux lag);
- AAC move: audio PTS step constant (1024 samples per packet) across every move, including
  private-to-shared; no duplicated or missing samples (sample-exact compare against the PCM fed);
  no sample step above the steady-signal maximum; AudioSpecificConfig byte-identical;
  every encoded audio bus's frame boundaries are multiples of 1024 on the global counter.

**Chaos and soak** (owed by an invariant #1/#10 change; none may be skipped):

1. **Take flood:** 2,000 takes per second across all outputs for 10 minutes (almost all of them
   get 429). Program emits exactly
   the expected tick count with contiguous PTS. Program lateness meets the Program protection
   bound of §10 against the same run without routing (on the CI host for the test, on mda001 for
   acceptance).
2. **Failing feeds:** routed sources die, reconnect and flap every 2 s during takes. Never a stale
   or black frame on a routed output: pixel-probe each frame against the expected feed.
2b. **Cross-principal effects:**
   - one principal flapping takes onto a bus that another principal's output subscribes to adds at
     most one extra IDR per 2 s to that output;
   - round-robin takes cannot hold more than the per-principal encoder cap;
   - scoped principals never see session counts or unreadable bus ids.
2c. **Ongoing authorization:**
   - a layout edit that adds an unreadable source to a routed layout, or to the current PVW/PGM
     layout, is refused (403 `routed-layout`);
   - a principal with Program scope but no Read on S cannot select a `layout:` that shows S onto
     PVW or PGM, directly or through nested `layout:` or `bus:` cells;
   - redaction holds on plan, 409 and event bodies.
3. **Wedged aux thread:** blocked forever on the first frame. Program gets every tick with
   contiguous PTS. Routed outputs hold last-good, raise `routing.degraded` at 60 ticks and fall back
   to Program with an IDR at 150 ticks. A take pending at the wedge gets its terminal
   `refused:feed-not-fresh` at 50 ticks, driven by the bake consumer (the
   `a_blocked_wall_worker_costs_the_output_clock_zero_ticks` pattern).
4. **Panicking aux thread:** the same, plus a drop guard marks the feeds failed.
4b. **Open storm and GPU interference (mda001):** 1 take per second for 10 minutes, each needing a
   new encoder, alternating with moves back (each open serialised, each bus lingering and closing).
   It passes when the Program protection bound holds, Program's encoders report no
   `OfferRejection::Overloaded`, and the session count never exceeds the reservation.
5. **Wedged encoder:** only the outputs on its encoded bus lose windows (dispatcher isolation). A
   wedged live open: the take is refused at its deadline and the output keeps its feed.
6. **mda001 soak:** 1 h with the HLS output on a layout and SRT on a source, a take every 30 s.
   ffprobe continuity on both. Program tick lateness meets the Program protection bound of §10. NVENC
   sessions match the plan's count.

---

## 10. Slices

Epic: #303.

Each slice is one PR, shippable alone and visible to the operator. Acceptance is measured on mda001
after deploy (deployment by the lead under RULES). Risk class: R3 for slices touching the
clock-adjacent path, otherwise as `scripts/classify.sh` says.

| # | Slice | Gives the operator | Risk / crates | Does NOT yet do |
| --- | --- | --- | --- | --- |
| **1** (#304) | **Route preview, program, VT, bars or a source to any output, live, from the Outputs page and the API**: each output on its own, also when outputs share settings | The operator's named items except layout. Audio follows (`follow`), with a crossfade. "On Program" and "source direct" are distinct | R3. `multiview-config` (feed fields, `bus` alias), `multiview-engine` (aux thread with one-cell canvases: identity or CPU letterbox; picture-bus slots; take state machine on the bake consumer), `multiview-output` (`forced-idr=1`, force-keyframe, `starting_at(T0)` for video and AAC), `multiview-cli` (encoder registry keyed by picture bus + settings, live open off the clock, linger-close, dispatcher `rebind` at a window, per-bus substitution, routed audio buses), `multiview-control` (`GET /routing`, `output` take kind, `route.output` event, BOLA on the object axis, limits), `web` (Outputs page feed rows) | Layouts as feeds, Clean, explicit audio selection, salvos, the matrix page |
| 2 (#305) | Explicit audio feed per output (`program`, `source`, `tracks`, `none`) live; Clean bus to an output (part 1 built: Clean bus, plan counts lingering encoders as +0, HLS discontinuity at a changed-set move; part 2 built: the engine's explicit audio choice, audio-only and combined takes, one fade at a time, return to the shared program stream, config validation and rename propagation; part 3 built: the HTTP `audio` feed and `stream: "audio"`, audio-feed authorization, `audio-unsupported`, the plan's audio cost, audio persistence, boot audio takes from config, and the Outputs page Audio row) | "how do I route audio?": any source's audio alone on any output, live | R2-R3. `multiview-cli`, `-config`, `-control`, `web` (Audio row) | Layout audio attribute (slice 3) |
| 3 (#306) | Route a stored layout to an output (GPU aux canvas + CPU integer path), admission and cost in plan, the `routing.degraded` path | "layout": NewsOne on the HLS output while the switcher stays on | R3. `multiview-engine`, `-cli`, `-control`, `web` | Layout on the M/E, buses inside layouts |
| 4 (#307) | Layout-as-a-source on the M/E (PVW/PGM/AUTO); `bus:program` and `bus:preview` cells; cycle refusal; first-slice restriction removed; `apply-layout` semantics | "in, around, through" (#243) | R3. `multiview-engine`, `-cli`, `-config`, `-control` | More than one M/E |
| 5 (#308) | Crosspoint matrix Routing page (video + audio, tally, all states), output salvos (`/routing/salvo/*`, `Salvo.output`) | One screen for everything, atomic multi-output takes | R2. `web`, `multiview-control`, `-config`, `-engine` (salvo adoption) | — |

**Program protection bound: acceptance for every slice.** Measured on mda001 over at least 10
minutes with N routed feeds active, against the same 10-minute measurement with none:

| Measure | Bound |
| --- | --- |
| Program tick lateness p50 | within +1 ms |
| Program tick lateness p99 | within +2 ms |
| Program tick lateness max | within +5 ms, and below half a tick (16.7 ms) |
| `frames_repeated` (ticks behind) | 0 |
| HLS video | every packet's PTS delta is 3003 |

Program tick lateness is not exported today. Slice 1 adds the measurement: a histogram of emit time
minus deadline, at the point `state_of` runs, and a `frames_repeated` counter
(`crates/multiview-engine/src/runtime.rs:354-361`) on the metrics endpoint. This one bound is the
Program protection bound everywhere in this note (§7, §9). Every R3 slice also owes its chaos/soak
tests (§9). Slice 1 runs: the take flood (1), failing and flapping feeds (2), cross-principal
effects (2b), ongoing authorization (2c), a wedged aux thread (3), a panicking aux thread (4), the
open storm (4b), a wedged encoder and a wedged live open (5), and the mda001 soak (6).

**Slice 1 acceptance on mda001:**

- Two outputs with identical settings, both on Program. Take one to bars: the other keeps Program,
  with no restart. PTS is continuous on both, the moved output's first packet after the move is an
  IDR, and the plan said "+1 encoder" beforehand. Take it back to Program: "+0 encoders", and the
  bars encoder closes after 10 s.
- From the Outputs page, take the SRT output to bars, then to `vt1`, then Preview, then Program,
  and take the HLS output to `cnn-usa`.
- ffprobe on each stream: PTS step constant, `K` at each cut tick, SPS/PPS unchanged, no HLS
  discontinuity.
- The SRT and HLS audio follow (1 kHz tone on bars).
- The Program protection bound above, with both outputs routed (N = 2).
- NVENC sessions are +1 while an output is on a picture bus no other encoder carries, as the plan
  said; they return to 5 within 10 s of the move back.
- Measure whether NVENC parameter sets are byte-identical across a live-opened encoder and the
  running one, and show that the move is correct in either case (§4.2 step 4).
- A take to the failed `times-square` is refused at 50 ticks while the output keeps its feed.

**What slice 1 gives the operator, by his own words:** **preview, program, VT, bars, source**, on
any output, live. **Not layout.** Layout needs an N-cell compose, which on mda001 means a second
GPU compose beside Program, and that is unproven. It ships in slice 3 under its own gate.

Slice 1 is the least engine risk:

- no GPU work is added (identity or a CPU integer blit, §2.4);
- no encoder or binding changes (§4.3);
- audio uses blocks the bus already produces (§5.2).

**Mapping of old backlogs and issues:**

| Item | Fate |
| --- | --- |
| RT-12 `OutputRef` / output identity | **Reused** (`crates/multiview-config/src/routing.rs:280-298`; the stable output id addresses the feed) |
| RT-12 sink-mover through `PacketRouter::move_sink` | **Reused as the idea, rebuilt on the production dispatcher**: a take is a dispatcher rebind at a window boundary (§4.3) |
| RT-13 seamless output←program splice (GP-7) | **Superseded for routing** by the raw switch (§4.1); still separate for ADR-0030 passthrough |
| RT-15 make-before-break output migration | **Still separate**: format changes only, never a route take |
| RT-16 mixed salvo | **Reused**: shape of `OutputRecall` in slice 5 |
| RT-17 matrix UI | **Superseded** by slice 5 (same intent, on `GET /routing` readback) |
| SW-27 (aux buses via RT-12) | **Superseded**: aux = an aux canvas of the same Program (slices 1-4) |
| GP-1 `is_idr`, GP-6 `RestampAccumulator` | Not needed for routing; unchanged |
| #144, #154, #155, #156 (shared encoded-packet carrier) | **Still separate**; routing does not depend on them |
| #145 (IDR egress splicer) | **Superseded for routing**; stays for passthrough |
| #146-#149 (Class-2 MBB, migrate route) | **Still separate** (format migration) |
| #150, #151 (audio rendition registry, per-output selection) | Routed audio encoders (slices 1-2) use #150's keying if it has landed; otherwise slice 2 builds the minimal per-output form. The registry's other renditions stay in #150 |
| #152, #153 | Still separate |
| #203 (HEVC SRT carries H.264) | Encoders are now keyed per codec (`crates/multiview-cli/src/pipeline.rs:2830-2842`); verifying and closing #203 belongs to its owner. Route groups are keyed by each output's own settings, so routing cannot reintroduce it |
| #216 (per-output encoder settings) | Still separate; encoded buses key on its settings once #216 lands |
| #243 | **Delivered by slice 4** (slice 3 for the wall-on-output half) |

---

## 11. Interactions and open points

- **Network front door (ADR-0107).** No interaction: routing changes what an output carries, not
  how sockets are bound. WHEP and MoQ output viewers see the cut as in §4.2.
- **Output clock on its own thread** (planned). The only clock-side code this design adds is one
  `try_send` in `after_compose`. It moves with the clock unchanged. The aux thread is already an OS
  thread.
- **Not resolved:**
  - The GPU cost of an aux canvas beside Program on the RTX 4060 is unmeasured. Slice 3 has a hard
    acceptance gate: Program lateness p99 is not worse.
  - Multiple M/Es and outputs at a different cadence or size remain ADR-0030 work.
  - Rejoining the shared encoder after returning an output to Program happens only at restart
    (§4.3).
