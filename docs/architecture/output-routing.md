# Output routing: route anything to any output

Design note for [ADR-0108](../decisions/ADR-0108-output-routing-matrix.md). Risk class R3: the change
touches invariants #1 (output clock) and #10 (isolation). Every claim about today's code cites
`file:line` on `main` at f5f2230e. Every claim about cost cites a command and a number. The spike
code ran in scratch and is not merged.

The operator, 2026-11-12:

> "how do I route a layout to an output? (and how do I route audio?)"

> "I asked for routing, in, around, through and out. I should be able to route anything to an output - preview, program, layout, VT, bars, source"

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
| Nothing at runtime reads `bus`: outside tests and config, no `OutputBus::Preview` or `OutputBus::Clean` appears in `multiview-cli`, `-engine`, `-output` or `-control` (grep). The desugar always writes `Program` | `lib.rs:1596-1600` |
| Every output encodes the same composited, overlay-baked Program canvas. Encoders are keyed by codec name only (H.264 HLS and HEVC SRT get two encoders, one per codec) | `crates/multiview-cli/src/pipeline.rs:2820-2844`, `:7172`, `:7193-7206` |
| Program audio is ONE shared AAC encoder for all outputs. Discrete tracks are streams inside it | `pipeline.rs:5795`, `crates/multiview-output/src/sink.rs:724` |
| An output's `audio` selects the mixed bus, discrete tracks, or both (`OutputAudioMode::{Auto, Program, Tracks}`) | `crates/multiview-config/src/audio.rs:304-335` |
| Output CRUD is restart-class: every mutation is wrapped in `with_apply_restart` | `crates/multiview-control/src/routes/outputs.rs:185,254`, `crates/multiview-control/src/typed_resources.rs:57` |
| `apply-layout` swaps the ONE live canvas at the next frame boundary. It is Class-1 and targets no output | `crates/multiview-control/src/routes/mod.rs:756-942` |
| With `[switcher]` declared, startup rejects any canvas other than one full-canvas cell | `pipeline.rs:281-302`, `:2438-2440`; one M/E `me1` only, `crates/multiview-config/src/switcher.rs:48-55` |
| The PVW "bus" is a raw tile sample for the monitor. The code says a real PVW output "must instead own its own admitted compositor/rendition — this tap never composes or encodes" | `crates/multiview-engine/src/switcher_hook.rs:110-126` |
| Route takes exist for `video \| audio \| subtitle` (a cell, a bus channel, a discrete track, a layer). They have plan/take, an `Idempotency-Key`, BOLA checks on both ends, and Class-1/Reset-lite/Class-2 classes. There is no `output` kind and no `GET` of the routing state | `crates/multiview-control/src/routes/routing.rs:107-208`, `:225-252`, `:347-371`; `crates/multiview-control/src/routes/mod.rs:1418-1419` |
| Route takes have no correlated realtime outcome; the change rides the snapshot | `crates/multiview-control/src/realtime.rs:205-222` |
| `audio-routing` PUT applies program-bus gain, include and mute live (ramped). Discrete-track topology is `partial`/restart | `crates/multiview-control/src/live_apply.rs:100-150`, `crates/multiview-cli/src/audio.rs:863` |
| Every source with audio sits on the program bus as a strip: metered, but mixed at gain 0 unless included. So each source's clean per-tick block already exists | `pipeline.rs:17548-17572`, `:4431-4436`; `crates/multiview-audio/src/program.rs:386-395` |
| `AudioStore::read` advances the store's cursor, so a store cannot be read twice per tick | `crates/multiview-audio/src/store.rs:829-850` |
| Equal-power `GainRamp`, `ProgramBus::repoint_crossfade` and exact sample budgets from `SampleClock::total_at` exist | `crates/multiview-audio/src/mixer.rs:35`, `program.rs:233`, `crates/multiview-audio/src/cadence.rs:80`, `crates/multiview-cli/src/audio.rs:770-842` |
| A force-keyframe seam exists: `VideoEncoder::force_next_keyframe` makes the next frame an I picture (an IDR with `forced-idr=1`). Encoders run with B-frames off | `crates/multiview-ffmpeg/src/encode.rs:216,309-318`, `:149` |
| The HLS segmenter starts a new segment on every keyframe | `crates/multiview-output/src/sink.rs:1521-1527` |
| The take gate: `TAKE_PRIME_DEADLINE_TICKS = 50` and the `state_at(...).is_live()` freshness rule | `crates/multiview-engine/src/switcher_control.rs:57`, ADR-0104 §2 |
| The production output clock is a `tokio::spawn`ed task | `crates/multiview-engine/src/programset.rs:546` |
| The frame hook runs on the clock: `before_compose` and `after_compose` | `crates/multiview-engine/src/runtime.rs:244-251` |
| Decode size is the per-axis supremum over the cells that bind a source; an unbound source decodes at 640x360 | `crates/multiview-cli/src/display_size.rs:72-93` |
| `migration::OutputCrosspoint` / `PacketRouter::move_sink` have no production caller. `RestampAccumulator` is used only by `guarded.rs` | `crates/multiview-engine/src/migration.rs:145-235`, `crates/multiview-output/src/restamp.rs:40` |

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

VT and generators need no feed kind of their own. They already are sources, with a `TileStore` and
an audio store. The UI lists them in their own groups ("Players", "Generators") because the operator
names them separately.

### 2.2 Canvases

Every video feed is produced by a **canvas**: a `CompositorDrive` (`crates/multiview-engine/src/drive.rs:216`)
over the **same** source `TileStore` map Program uses (`catalogue_stores()`, `pipeline.rs:3253`).
That reuse is what makes it decode-once-use-many: a routed feed never opens a decoder.

| Feed | Canvas | Thread | Extra compose |
| --- | --- | --- | --- |
| Program | The Program canvas (unchanged) | Output clock | none |
| Clean | The Program canvas **before** the overlay bake (`item.canvas`, `pipeline.rs:7172`) | Bake consumer | none |
| Preview | A one-cell aux canvas bound to the PVW source | `routing-aux` | one 1-cell |
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
Program costs the clock nothing when it runs on its own thread and shares only wait-free reads
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
              │ hot queue (existing)                      │ offer frame T to selectors │
              ▼                                           └─────────────┬──────────────┘
   bake consumer (existing): bake overlays                              │
     Program frame ──► Program encoders (shared, keyed by settings)     │
     Clean frame   ──► Clean encoder (only if some output selects it)   │
                                                                        ▼
                          per-output feed selector ─► routed encoder ─► that output's sink
```

The clock's only new duty is one `try_send` of a `Tick` (16 bytes) into a capacity-4 channel, in
`after_compose`. That send never blocks. A full channel drops the tick and increments a counter
(§4.4 handles the gap).

### 2.3 How many canvases, and on which device

- One `routing-aux` thread per run, and only while an aux canvas is active. It is an OS thread, not a
  tokio task, at lower scheduling priority than the clock (spike: `setpriority(+10)`).
- Each aux canvas uses the run's compositor backend (`RunBackend::select`, `pipeline.rs:4140-4175`):
  the GPU (wgpu) on mda001, the CPU reference in the default build.
- At most `routing.max_aux_canvases` canvases are active at once (default 2; admission rule in §7).
- An aux canvas is **active** only while at least one output, or a layout-as-source bus (§3),
  selects it. An unselected feed costs nothing.
- Two outputs on the same feed share the canvas. Whether they share one encoder is decided in §4.3.

### 2.4 A single source is a one-cell canvas, not a tile pass-through

A source's tile is decoded at the size of the cell that binds it (`display_size.rs:72`). Its colour
tag is the source's own, and its frames arrive at the source's cadence. Passing the tile straight to
an encoder would break three invariants:

- a tile smaller than the output would need an extra scaler (#6);
- a source tag that differs from the canvas tag would need its own conversion (#8, which fixes the
  colour order and handles range exactly once);
- a 25 fps source on a 29.97 output would have no cadence latch.

The compositor already does all three in one pass: scale-at-composite, the fixed colour pipeline,
and the latch-on-tick `read_at` (`crates/multiview-framestore/src/tile.rs:592-640`). So a single
source is a one-cell canvas with `fit = contain` (letterbox or pillarbox on the canvas background).

The cost is honest. A routed source is registered as a consumer at the full canvas size, so its
decode size rises to the per-axis supremum (`display_size.rs:72`, make-before-break handover,
`display_size.rs:1-17`). The decode-side scaler already emits tiles at the planned size, tagged like
the canvas (`TileScaler`, `pipeline.rs:17574-17580`). So once the resize lands, the tile matches the
canvas in size and colour tag, and the one-cell compose is the identity. Implementation detail: the
drive returns the tile `Arc` without drawing. This makes the common case (a 16:9 source to a 16:9
output) free of compose cost. Until the resize settles (1.5 s, `display_size.rs:41`), the compose
scales the smaller tile. A tile with a different aspect ratio is letterboxed by the compositor.

### 2.5 Preview and Clean

- **Preview** = a one-cell aux canvas whose cell follows the M/E's PVW selection. The switcher hook
  already knows the selection every tick (`switcher_hook.rs:61`). It publishes it into a wait-free
  `SwapCell`, written only when it changes (the ADR-0104 §4 pattern). The aux thread rebinds its cell
  when the cell changes (`CompositorDrive::rebind_cell`, `drive.rs:480`). This is the "own admitted
  compositor/rendition" that `switcher_hook.rs:113-115` calls for.
- **Clean** = the Program canvas without the monitoring overlay bake (labels, tally borders, clocks,
  watermark: `BakeContext`, `pipeline.rs:5146-5195`). The bake consumer offers `item.canvas` instead
  of `overlaid` to the Clean encoder. It costs no compose, only one encoder while selected. When a DSK
  stage exists (ADR-0054), Clean is the tap before the DSK. The routing contract does not change.

---

## 3. In, around, through: layouts and buses inside each other (#243)

**Layout-as-a-source.** An active layout aux canvas publishes each composed frame `Arc` into a
`TileStore` with the reserved id `layout:<id>`, stamped at `T.pts`. That store goes into the catalogue
store map, so:

- the M/E's PVW and PGM can select `layout:NewsOne` like any source (ADR-0104 admission:
  `validate_source_configured` accepts stored layout ids as `layout:<id>`);
- the fresh-frame take gate works unchanged (`state_at(...).is_live()` on that store);
- the AUTO dissolve works unchanged (it is a full-canvas tile, `drive.rs:252`).

**Latency.** The layout composed for tick T is published after tick T's Program compose. Program at
tick T+1 latches it (nearest-not-after, `tile.rs:592`). So a layout on Program is **one tick (33 ms)
behind** its own sources. Its audio is delayed by exactly the same `SampleClock` span (§5.4) so it
stays lip-synced.

The alternative is to compose the layout on the clock before Program. That puts an N-cell composite
on the clock task, which ADR-0103 rejected ("A second compositor pass on the clock thread … risks
invariant #1"). One tick of latency is the price of keeping the clock clean, and it matches what a
hardware multiviewer feeding a switcher input costs.

**Buses inside layouts.** A layout cell may bind `bus:program` or `bus:preview`, the classic
multiviewer with PGM/PVW tiles. The bake consumer publishes the composited Program canvas into
`TileStore` `bus:program`. That costs one `Arc` clone per tick, because the canvas `Arc` already
exists at `pipeline.rs:4764`. The Preview aux canvas publishes into `bus:preview`. Both are one tick
behind.

**Cycles are refused.** One example: Program shows `layout:A`, and A contains `bus:program`. Every
plan computes the feed graph. An edge goes from each feed to the feeds its cells bind, through the
M/E's current PGM/PVW selection. A take or a layout edit that closes a cycle gets 422 `feed-cycle`
with the path. The engine re-checks at adoption, because the M/E selection can change between plan
and tick. A cycle found at the tick refuses the take (typed outcome) and keeps the last-good feed.

**The first-slice restriction is removed.** `first_slice_me_layout_supported` (`pipeline.rs:281`)
existed because the Program canvas *was* the multiview, so a switcher would have replaced it. In the
new model, with `[switcher]` declared, the Program canvas is the M/E bus: a one-cell canvas, exactly
as today. The document's configured multi-cell layout is admitted as the stored layout `config`,
routable like any other. Startup no longer fails, and nothing is "replaced". Existing configs keep
working. The configs that fail today (switcher plus multi-cell) start, with Program = the M/E. By
default their outputs carry Program, and the operator routes `layout:config` to the outputs that
should keep the wall.

**`apply-layout` in the new model.** It means "route layout X to the feed `program`'s canvas".

- **No switcher declared.** It keeps today's meaning exactly: the Program canvas swaps to X at the
  next boundary (`routes/mod.rs:756-942`, unchanged).
- **Switcher declared.** The Program canvas is the M/E bus, and replacing it would break the bus
  (`switcher_hook.rs:66` rebinds a named cell that a foreign layout does not have). The call answers
  422 `switcher-owns-program`. The detail names the two operations that do what the operator means:
  `POST /routing/output/take` (layout to an output) and `POST /switcher/commands/select_preview`
  with `layout:<id>`. It never silently changes something else.
- An optional body field `target: {output: id}` makes `apply-layout` a thin alias for an output
  take, so the Layouts page "Apply" can offer "Apply to output …".

---

## 4. Switching an output's video feed live

### 4.1 Decision: raw frame switch in front of the output's own encoder

There are two ways to switch:

- **Encoded splice (RT-13):** move the sink from encoder A's packets to encoder B's packets at an
  IDR, restamping the DTS.
- **Raw switch:** the output's encoder keeps running, and the frame fed into it changes at a tick
  boundary, with a forced IDR on that frame.

**Raw switch, for every case this design covers.**

- PTS/DTS stay continuous by construction: the same encoder, the same tick counter, no restamp.
- The SPS/PPS and the init segment stay identical: the same encoder, the same pinned format. So no
  consumer sees a discontinuity tag, a new `EXT-X-MAP`, a new FLV sequence header or a new PMT.
- The cut lands exactly on the requested tick, with no wait for B's next IDR.
- No other output's stream changes. An encoded splice would force an IDR on encoder B for every
  other consumer of B, or wait up to a GOP.

RT-13's splicer stays the right tool for passthrough programs (ADR-0030 kind B). Output routing
never needs it.

### 4.2 The cut

Tick T is the first tick at which the take is adopted (§4.5).

1. **Video.** Frame T is the first frame of the new feed. Frame T-1 was the last frame of the old
   feed. There is no blend and no black frame between them.
2. **Keyframe.** The routed encoder's generation counter increments at T. The encoder thread forces
   an IDR on the first frame whose generation differs from the last one it encoded
   (`force_next_keyframe`, `encode.rs:216`). It keys on a generation change, not on a per-frame
   flag, so a dropped cut frame cannot lose the keyframe: the first new-feed frame that does arrive
   is the IDR.
3. **Timestamps.** PTS = DTS = the tick index in the encoder time base 1/cadence
   (`pipeline.rs:6518`, B-frames off at `encode.rs:149`). Ticks never received are filled by the
   encoder thread with the last frame of the **same** generation, exactly as
   `missing_ticks_before` does today (`pipeline.rs:6561`). So the packet stream has one frame per
   tick, forever.
4. **Format.** An output's format (size, cadence, pixel format, colour tags, codec settings) is
   pinned at output start and **never changes on a take**. The new feed is composed at the output's
   pinned canvas size and colour. Scaling, letterboxing and tag conversion happen in the compositor
   in the fixed colour order. A source at another frame rate is latched per tick (repeat or drop).
   A format change, such as resolution or codec, is RT-15's make-before-break migration. It is a
   separate operation and is never triggered by a route take.

What each consumer sees at the cut:

| Output | Sees |
| --- | --- |
| HLS (TS segments) | A new segment starts at the IDR (`sink.rs:1525`); that segment and the one before are shorter. Media sequence continuous; **no** `EXT-X-DISCONTINUITY` (same encoder, same SPS) |
| LL-HLS / CMAF | A new part/segment starts at the IDR; same init segment, so no new `EXT-X-MAP` |
| SRT / RIST (MPEG-TS) | Continuous PTS/PCR and continuity counters; an IDR with in-band SPS/PPS. Decoders cut cleanly |
| RTMP / FLV | Continuous timestamps, a keyframe tag, no new sequence header |
| WebRTC (WHEP output, WHIP push) | Continuous RTP timestamps; the IDR arrives, so no PLI round trip is needed |
| MoQ | A new group starts at the IDR (groups align to keyframes); a joining viewer starts at the cut |
| NDI, display (KMS) | Raw frames: the new picture at frame T. There is no encoder, so no keyframe |

### 4.3 Encoders: who shares what (#7)

- **Default outputs**, those with no `video` feed configured and never taken, share the Program
  encoders exactly as today: keyed by settings (`pipeline.rs:2830-2842`). Absent config means
  Program, byte-for-byte today's behaviour.
- **Routed outputs.** An output becomes routed when its config sets `video`, or at its first live
  take. A routed output has a **feed selector** and its own encoder, keyed by
  `(route group, settings)`. Outputs in the same `route_group` share one selector and one encoder,
  and always take together. That covers "the same routed feed to HLS and SRT with the same
  settings", one encode (#7). Outputs on different groups never share an encoder, even when they
  happen to carry the same feed at a given moment. Sharing them would turn every take into an
  encoded splice between encoders (§4.1). The `plan` response shows that cost explicitly.
- **The first take of a default output** is the only binding move in the design. At adoption tick
  T0, the dispatcher binding of that output switches from the shared Program encoder to its new
  routed encoder. The routed encoder was opened at plan time on the control side. Its first frame
  is encoded at T0, so it is an IDR by definition, with PTS = T0 from the same tick counter. The
  stream is therefore continuous and the cut lands on an IDR.
  - The new encoder's extradata (SPS/PPS) must be byte-identical to the shared encoder's. Identical
    settings with the same libav encoder give identical extradata, and slice 1 tests this.
  - TS-based transports carry headers in band at every IDR, so a difference is legal there. For
    fMP4/LL-HLS and FLV, a difference makes the take refuse with `encoder-headers-differ`, never
    silently corrupt.
  - Returning an output to Program keeps it routed: its selector simply feeds Program. Rejoining
    the shared encoder would save one encode and cost a second binding move. It happens at the next
    restart (the persisted feed is `program`, and an output whose feed equals the default and which
    has no `route_group` starts unrouted).

### 4.4 Isolation of the data path

- **Clock to aux:** one `try_send(Tick)` (capacity 4). A full channel drops the tick: the clock
  never waits for the aux thread.
- **Aux to encoders:** bounded `try_send` per encoder (capacity 8). A full channel drops the frame;
  the encoder thread gap-fills (§4.2 step 3).
- **Aux thread** reads only `TileStore::read_at` (lock-free, `tile.rs:592`) and the selection
  `SwapCell`s. It takes no lock the clock holds.
- **Take admission:** `try_send` into a bounded mailbox (64, the `COMMAND_CAPACITY` pattern of
  `switcher_control.rs:53`). A full mailbox answers 503 `engine-busy`.
- **A wedged or panicking aux thread** freezes only the routed outputs (on their last-good frame,
  gap-filled). A drop guard publishes `routing.feed_state = failed` and an alarm. Program and every
  default output continue, which is the ADR-0103 wall pattern.
- **A wedged routed encoder** costs only its own output. The dispatcher already isolates bindings
  (`crates/multiview-cli/src/rendition_dispatch.rs:1-5`).

### 4.5 Take gating

A take names an output and a feed. The aux thread drains the mailbox at the start of each tick it
processes. Per output, the latest take wins; a replaced pending take gets the outcome `superseded`.
The thread then applies the gate:

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
  limited to 4 cores:

  | Arm | p99 | max |
  | --- | --- | --- |
  | Baseline | 13.6-15.6 ms | 18-19 ms |
  | With routing | 18.8-19.8 ms | 31-34 ms |

  The shared build host ran at load 25-90 on 56 cores, so differences of about 5 ms are noise. A
  later unpinned run produced the opposite ordering (baseline p99 83 ms, routing 21 ms).
- **When the aux thread was starved**, the routed outputs fell to 110 of 900 fresh frames (the
  encoder gap-filled the rest at a constant PTS step). Program stayed at 900/900 and 0 repeats.
  That is the intended degradation order (§7).
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
| Source / VT / generator X | X's own audio. A source with no audio (solid, clock) gives silence; bars give the 1 kHz line-up tone (`pipeline.rs:4405-4410`) |
| Layout L | L's `audio` attribute: `program` (default, so routing a layout changes no one's audio versus today), `none`, or `source:<id>` of one of its cells |

An explicit `audio` overrides follow: `program`, `source:<id>`, `tracks:[...]` or `none`. Audio
follows the *feed*, so when a take changes the video feed, a `follow` output's audio changes on the
same tick.

### 5.2 Where routed audio comes from

There is no second decode and no second store read. Every source with audio is already a strip on
the program bus at gain 0 (`pipeline.rs:17548-17572`). Each tick, the bus pulls every strip's block
exactly once (`program.rs:386-395`). An `AudioStore` cannot be read twice (`store.rs:829`). A routed
output's audio block for tick T is therefore built on the bake consumer, right after `ProgramBus`
ticks, from:

- that tick's per-source clean blocks (`input_tracks()`); or
- the mixed program block.

It is then encoded by the output's own AAC encoder. The shared program AAC encoder
(`pipeline.rs:5795`) keeps serving default outputs.

**Cost.** One stereo AAC encode at 128 kbit/s used 3.2 s CPU per 120 s of audio:
`ffmpeg -benchmark -f lavfi -i sine=r=48000:d=120 -ac 2 -c:a aac -b:a 128k -f null -`. That is about
2.7 % of one core per routed audio feed.

### 5.3 Crossfade at a take

When a take changes an output's audio feed, the output's audio mixes old and new at tick T:

```text
out = old · cos(θ) + new · sin(θ)
```

θ rises from 0 to π/2 over **exactly** `SampleClock::total_at(T+1) − total_at(T)` samples: one output
tick, 1601 or 1602 samples at 48 kHz and 30000/1001, never a float. This is the equal-power envelope
of `GainRamp` (`mixer.rs:35-109`) that ADR-0104 §4 uses for AUTO, applied per output.

The ramp starts on the first sample of tick T, the same tick as the video cut, so it is
sample-accurate against the tick. Silence to a feed is a ramp up, and a feed to silence a ramp down,
so there is no pop at either end. A take whose old and new audio feed are equal (for example
switching between two layouts that both carry `program`) does nothing to the audio.

### 5.4 Lip-sync for one-tick-delayed feeds

A `layout:<id>` or `bus:*` used **as a source** (§3) is one tick behind its sources. When that feed's
audio rides with it, for example a layout on Program via the M/E with `audio = source:x`, the audio
is delayed by exactly `total_at(T) − total_at(T−1)` samples: a one-tick, bounded, allocation-free
ring per such feed. A layout routed **directly** to an output is not delayed: it composes from tick
T's stores and is encoded as frame T.

### 5.5 How it composes with what exists

- **`audio-routing` stays the source of per-source gain, mute, program-bus membership and track
  naming.** An output's `audio = source:x` carries x's clean block with x's route `gain_db` and
  `mute` applied. It does not apply `include_in_program_bus`, which concerns the mix only.
- **An output's existing `audio.mode/tracks` stays valid.** `tracks` is now one value of the output
  audio feed. Default outputs keep exactly today's `auto | program | tracks` semantics and the
  shared encoder.
- **On a routed output**, the routed audio feed takes the program-audio stream slot. Its selected
  discrete tracks still come from the shared encoder (the dispatcher already muxes several
  encoders' streams per binding, `rendition_dispatch.rs:61-66`).
- **Per-transport capability is unchanged** (`crates/multiview-audio/src/capability.rs`). An HLS
  output can still select one track. NDI carries the routed audio as its channel map.
- **The rename propagation of ADR-R005 §4.3 extends to output audio feeds:** `propagate_routing_change`
  (`crates/multiview-config/src/audio.rs:498`) also rewrites a feed naming a renamed track.

---

## 6. API and config

### 6.1 Config (backward compatible)

```toml
# Absent [routing.output] entry for an output = Program, exactly today's behaviour.
[[routing.output]]
output = "restream"
video  = { layout = "NewsOne" }   # | { bus = "program" | "preview" | "clean" } | { source = "vt1" }
audio  = "follow"                 # | "program" | "none" | { source = "cnn-usa" } | { tracks = ["eng"] }
route_group = "wall"              # optional: outputs in a group share one selector + encoder

[[routing.output]]
output = "srt-multiview"
video  = { source = "bars" }      # bars carries the line-up tone under "follow"

[[layouts]]                       # stored layouts gain an optional audio attribute
id = "NewsOne"
audio = "program"                 # | "none" | { source = "<a cell's source>" }
```

The existing `bus` field (`routing.rs:236`) stays as an alias: `bus = "preview"` means
`video = { bus = "preview" }`. When both are present they must agree, or validation fails. The
desugar (`lib.rs:1590-1600`) keeps writing Program. A take persists the new feed into the running
routing table, through the same config-mutation lock `apply-layout` uses (`routes/mod.rs:811`), so a
restart comes back on the operator's last feed.

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

`GET /api/v1/routing`, filtered per object for scoped principals:

```json
{ "tick": 1843021,
  "outputs": [
    { "output": "restream", "video": {"layout": "NewsOne"}, "audio": "follow",
      "effective_audio": "program", "route_group": null, "state": "on_air",
      "since_tick": 1841990, "pending": null },
    { "output": "srt-multiview", "video": {"bus": "program"}, "audio": "follow",
      "effective_audio": "program", "state": "on_air", "since_tick": 0,
      "pending": {"video": {"source": "vt1"}, "held_ticks": 12, "operation_id": "…"} } ],
  "cells": [ … existing video/audio/subtitle crosspoints … ],
  "aux": { "active_canvases": 1, "max_canvases": 2, "routed_encoders": 1,
           "encoder_sessions_free": 7, "dropped_ticks": 0 } }
```

**`POST /api/v1/routing/plan`** with an output target is a dry run:

```json
{ "target": {"kind": "output", "output": "restream", "stream": "video"},
  "feed": {"layout": "NewsOne"} }
```

The response gives the class, the cost and the refusals:

```json
{ "class": "class1",
  "cost": { "new_aux_canvases": 1, "new_encoders": 1, "new_audio_encoders": 0,
            "decode_resizes": ["cnn-usa"], "binding_move": true },
  "refusals": [] }
```

Possible refusals (typed, RFC 9457 `type` suffixes):

- `capacity` (with `needed` and `available` per resource);
- `feed-cycle` (with `path`);
- `unknown-feed`;
- `encoder-headers-differ`;
- `switcher-required` (Preview or Clean without an M/E);
- `output-kind-unroutable`.

The class is `class1` for every feed change, because the format never changes (§4.2). A format
change is not offered here.

**`POST /api/v1/routing/output/take`.** The body is the same as plan plus an optional `audio`. It
requires an `Idempotency-Key`, and a replay returns the original `operation_id`. The response is
`202 {operation_id, adopt: "next_tick"}`. The outcome arrives as the realtime event
`route.output {output, operation_id, tick, outcome}`, with `outcome` one of:

- `applied`
- `superseded`
- `refused:feed-not-fresh`
- `refused:feed-cycle`
- `refused:encoder-headers-differ`
- `refused:capacity`

The event is correlated by a new `CorrKey::Route { output }`. Today's video, audio and subtitle
takes stay uncorrelated (`realtime.rs:209-222`) until they move onto the same event.

**`POST /api/v1/routing/salvo/take`** takes a list of crosspoints: output feeds plus the existing
cell, audio and subtitle crosspoints. It is validated as a set: all authorized, no cycle, the total
cost admitted. It is adopted **at one tick or not at all**. If any output feed in the salvo is not
fresh at the deadline, the whole salvo is refused and nothing changes. A salvo with no output
crosspoints keeps today's salvo semantics. The stored `Salvo` document (`crates/multiview-config/src/salvo.rs:59`)
gains `output: Vec<OutputRecall>`, the RT-16 shape reused.

### 6.3 Authorization (per object, both ends)

A take or plan requires:

| Object | Requirement |
| --- | --- |
| Destination | `authorize_output(principal, output)` with Write (`crates/multiview-control/src/auth.rs:349`) |
| Feed `source:x` | `authorize_object(principal, x)` with Read (`auth.rs:336`) |
| Feed `layout:L` | Read on L **and on every source its cells bind**. A layout picture shows those sources' pixels, so a principal limited to L could otherwise route a source it cannot read |
| Feed `bus:*` | `authorize_scope(Program("main"))` plus `authorize_object(me1)`, as `authorize_refs` does (`crates/multiview-control/src/routes/switcher.rs:99-112`). Program shows any source |
| Salvo | every crosspoint independently |

`GET /routing` returns only the outputs and cells the principal may read, and redacts a feed the
principal may not read to `{"redacted": true}` (never its id). Denials use the existing 403 shape,
and an unknown id the existing 404/422 shape. Neither leaks another object's existence beyond what
the existing routes leak.

### 6.4 Abuse: take floods

The engine takes at most one effective cut per output per tick, because the latest take wins at the
drain (spike: about 20,000 takes caused 4 cuts and no clock effect). Forced IDRs are a bitrate cost,
so the control plane limits takes **per output** to 10 per second. A take over the limit gets 429
with `Retry-After`. The existing per-IP limiter (`crates/multiview-control/src/limits.rs`) still
applies first. Idempotency keys bound replays. The mailbox is bounded (503 when full).

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
  aux canvases therefore use the GPU-free integer path. The identity fast path covers same-size
  sources. A layout or a letterboxed source uses the wall's integer box scaler with a BT.709 tag
  pass (no conversion). It is admitted only when all its tiles carry the canvas tag; otherwise the
  plan refuses with `capacity: aux-compose-unsupported-on-cpu-for-mixed-colour`. With the `gpu`
  feature (mda001), aux canvases use the wgpu compositor.
- **On mda001** the 4060 runs 5 NVENC sessions at 21 % encoder load. The driver limits sessions
  per system to a moving number (12 on current drivers; `docs/architecture/hardware-and-efficiency.md:155`),
  probed through `nvmlDeviceGetEncoderSessions`. Each routed output adds one NVENC session at
  canvas size. Each routed audio feed adds one AAC encode (CPU).

**Admission.** A take, salvo or config apply is admitted only if all of these hold:

1. `active_aux_canvases + new ≤ routing.max_aux_canvases` (default 2; 0 disables aux canvases);
2. `routed_encoders + new ≤ min(routing.max_routed_encoders, probed_free_encoder_sessions − 2)`.
   The 2 sessions are reserved for WHEP and the wall. Without NVML the probe value is
   `routing.max_routed_encoders`;
3. on the CPU path, each new aux canvas passes the CPU rule above.

The plan shows the arithmetic. A refused take returns 409 `capacity` with
`{resource, needed, available, holders: [outputs currently using routed encoders]}`, so the
operator sees what to free. The UI renders that as the disabled reason (§8).

**Never degrade Program.**

- Program is composed on the clock task, before the aux thread is even woken (`after_compose`).
- The aux thread runs at lower OS priority.
- On the GPU, aux work is submitted only after Program's tick has been submitted.
- If the aux thread falls behind, its ticks are dropped at the capacity-4 channel. Routed outputs
  repeat last-good (counted as `routing.dropped_ticks`). After 30 consecutive dropped ticks the alarm
  `routing.degraded` is raised.
- If Program's own lateness p99 over a 10 s window exceeds 50 % of the tick, the aux thread halves
  its cadence (composes every second tick; encoders gap-fill) until the window recovers. It never
  touches Program.
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
 Plan: NewsOne → srt-multiview   class1 · +1 canvas · +1 encoder (6 of 12 sessions free)
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

**Tally.** Each destination column header shows what it carries now. Each feed row shows a chip for
every output it is on (red for Program and the outputs carrying it, green for Preview).

The **Audio tab** has the same matrix, with audio feeds (`follow`, the program mix, each source, the
discrete tracks, `none`) against outputs, the program bus channel and discrete tracks. `follow` shows
what it currently resolves to.

**Take lock.** When locked, a take that changes a destination currently carrying Program opens the
existing confirm dialog (`RoutingPage.tsx:300-342`).

**Phone.** Use the existing two-step `DestinationPicker`, then `SourcePicker`.

**Outputs page shortcut** (`web/src/pages/OutputsPage.tsx`). Each output card shows two always-visible
rows:

```text
Video  [ Program ▾ ]  on air since 12:04:31     Audio  [ Follow (program mix) ▾ ]
```

The dropdown is grouped Buses / Layouts / Players / Generators / Sources, with disabled items and
their reasons. Choosing an item runs plan, shows the cost line, and offers [Take]. The outcome uses
`OperationFeedback`. Output CRUD stays restart-class for format, and the callout says so; the feed
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

Also checked: the first-take binding move with byte-identical extradata, and the refusal when it is
not identical (fMP4).

**Audio checks:**

- the crossfade spans exactly `total_at(T+1) − total_at(T)` samples at 30000/1001;
- no sample over the old or new peak;
- `follow` switches on the video tick;
- the one-tick delay for layout-as-source (sample-exact).

**Chaos and soak** (owed by an invariant #1/#10 change; none may be skipped):

1. **Take flood:** 2,000 takes per second across all outputs for 10 minutes. Program emits exactly
   the expected tick count with contiguous PTS. Program lateness p99 stays within 2 ms of the same
   run without routing (CI host); the ratio is checked on mda001.
2. **Failing feeds:** routed sources die, reconnect and flap every 2 s during takes. Never a stale
   or black frame on a routed output: pixel-probe each frame against the expected feed.
3. **Wedged aux thread:** blocked forever on the first frame. Program gets 40/40 ticks with
   contiguous PTS; routed outputs hold last-good; `routing.degraded` is raised (the
   `a_blocked_wall_worker_costs_the_output_clock_zero_ticks` pattern).
4. **Panicking aux thread:** the same, plus a drop guard marks the feeds failed.
5. **Wedged routed encoder:** only its output loses windows (dispatcher isolation).
6. **mda001 soak:** 1 h with the HLS output on a layout and SRT on a source, a take every 30 s.
   ffprobe continuity on both. Program tick lateness p99 is not worse than the hour before. NVENC
   sessions match the plan's count.

---

## 10. Slices

Each slice is one PR, shippable alone and visible to the operator. Acceptance is measured on mda001
after deploy (deployment by the lead under RULES). Risk class: R3 for slices touching the
clock-adjacent path, otherwise as `scripts/classify.sh` says.

| # | Slice | Gives the operator | Risk / crates | Does NOT yet do |
| --- | --- | --- | --- | --- |
| **1** | **Route a single source, VT, bars or Preview to an output, live, from the Outputs page and the API** | Named items: "preview", "VT", "bars", "source", "program" (back). Audio follows (`follow`), crossfaded | R3. `multiview-config` (feed fields, alias), `multiview-engine` (aux thread with one-cell canvases + identity path, take mailbox, gate), `multiview-cli` (selector, routed encoder, first-take binding move, routed audio from bus strips, per-output AAC), `multiview-control` (`GET /routing`, `output` kind on plan/take, `route.output` event, BOLA, per-output limiter), `web` (Outputs page feed row) | Layouts as feeds, Clean, explicit audio selection, salvos, the matrix page |
| 2 | Explicit audio feed per output (`program`, `source`, `tracks`, `none`) live; Clean bus to an output | "how do I route audio?": any source's audio alone on any output, live | R2-R3. `multiview-cli`, `-config`, `-control`, `web` (Audio row) | Layout audio attribute (slice 3) |
| 3 | Route a stored layout to an output (GPU aux canvas + CPU integer path), admission and cost in plan, the `routing.degraded` path | "layout": NewsOne on the HLS output while the switcher stays on | R3. `multiview-engine`, `-cli`, `-control`, `web` | Layout on the M/E, buses inside layouts |
| 4 | Layout-as-a-source on the M/E (PVW/PGM/AUTO); `bus:program` and `bus:preview` cells; cycle refusal; first-slice restriction removed; `apply-layout` semantics | "in, around, through" (#243) | R3. `multiview-engine`, `-cli`, `-config`, `-control` | More than one M/E |
| 5 | Crosspoint matrix Routing page (video + audio, tally, all states), output salvos (`/routing/salvo/*`, `Salvo.output`) | One screen for everything, atomic multi-output takes | R2. `web`, `multiview-control`, `-config`, `-engine` (salvo adoption) | — |

**Slice 1 acceptance on mda001:**

- From the Outputs page, take the SRT output to bars, then to `vt1`, then Preview, then Program,
  and take the HLS output to `cnn-usa`.
- ffprobe on each stream: PTS step constant, `K` at each cut tick, SPS/PPS unchanged, no HLS
  discontinuity.
- The SRT and HLS audio follow (1 kHz tone on bars).
- Program tick lateness p99 is within 2 ms of the previous hour.
- NVENC sessions are +1 per routed output, as planned.
- A take to the failed `times-square` is refused at 50 ticks while the output keeps its feed.

Slice 1 is the least engine risk: one-cell canvases are mostly the identity, no GPU composite is
added, and audio uses blocks the bus already produces.

**Mapping of old backlogs and issues:**

| Item | Fate |
| --- | --- |
| RT-12 `OutputRef` / output identity | **Reused** (`routing.rs:280-298`; the stable output id addresses the feed) |
| RT-12 sink-mover through `PacketRouter::move_sink` | **Superseded** for routing: the one-time first-take binding move is a dispatcher rebind (§4.3) |
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
| #203 (HEVC SRT carries H.264) | Encoders are now keyed per codec (`pipeline.rs:2830-2842`); verifying and closing #203 belongs to its owner. Routed encoders take each output's own codec, so routing cannot reintroduce it |
| #216 (per-output encoder settings) | Still separate; a routed encoder opens with the output's settings once #216 lands |
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
