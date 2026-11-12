# ADR-0108: Route anything to any output — picture buses and encoded buses; an output subscribes to one encoded bus and a take moves the subscription at an IDR; layout-as-a-source

- **Status:** Accepted
- **Area:** Core engine / Output / Audio / Web/API (invariants #1, #10, #9, #7)
- **Date:** 2026-11-12
- **Source:** operator requirement of 2026-11-12 (quoted under Context); issue #243
- **Design note:** [output-routing.md](../architecture/output-routing.md) (model, cut and audio rules, API and config with examples, UI states, cost table, test plan, slices)
- **Amends:** [ADR-0030](ADR-0030.md) (multi-program engine: a routed feed is an aux canvas of the same Program, not a second Program), [ADR-0034](ADR-0034.md) (RT-12/13/15/16/17 mapping)
- **Refines:** [ADR-0103](ADR-0103.md) (the off-clock second composite generalised), [ADR-0104](ADR-0104.md) (the fresh-frame take gate and equal-power crossfade reused per output), [ADR-0054](ADR-0054.md) (bus taps program/clean/preview)

## Context

The operator asked "how do I route a layout to an output? (and how do I route audio?)". He was told
that an output can only carry Program, and answered:

> "I asked for routing, in, around, through and out. I should be able to route anything to an output - preview, program, layout, VT, bars, source"

The product contract: every output has a selectable video feed and a selectable audio feed. A feed
is one of these:

- the Program, Preview or Clean bus;
- any stored layout;
- any media player (VT);
- any generator;
- any single source.

The operator changes a feed live, from the UI and the API, with no restart and without disturbing
other outputs.

When asked why an output that shares an encoder could not be routed on its own, the operator
said: "Why can't an output be routed seperately if it shares an encoder - I'm lost". He then
stated the model this ADR adopts:

> "I'm still lost - we should be making this as absolutely efficient as possible.
> An encoder outputs to a bus, it might "end up" none, one, or more places.
> If an encoder output is routed to a HLS and an SRT output - then they are getting their input from the encoder outputs bus. If the output of the output has to change, it stops getting its input from the output encoders bus and starts getting it from somewhere else.
> Program will have a program bus - that probably will go to one or more encoders.
> Program has to be handled separately because if program is routed to an output, like HLS, that output isn't simply selecting the input to program, it's "take"ing program. And that might be a wipe, dissolve, or a straight input."

What is true on main f5f2230e (the full table with citations is in §1 of the note):

- `OutputBus { Program, Clean, Preview }` exists in config (`crates/multiview-config/src/routing.rs:204-240`),
  but nothing at runtime reads it. Every output encodes the one overlay-baked Program canvas
  (`crates/multiview-cli/src/pipeline.rs:7172-7206`), and encoders are keyed only by codec
  (`crates/multiview-cli/src/pipeline.rs:2820-2844`).
- Output changes are restart-class (`crates/multiview-control/src/routes/outputs.rs:185,254`).
- `apply-layout` replaces the one live canvas (`crates/multiview-control/src/routes/mod.rs:756-942`).
- With `[switcher]` declared, startup admits only a one-cell canvas (`crates/multiview-cli/src/pipeline.rs:281-302,2438`), so
  a multiview and a switcher cannot both be on air (#243).
- Route takes exist for cells, bus channels, tracks and layers, but there is no `output` kind and no
  `GET` of the state (`crates/multiview-control/src/routes/routing.rs:107-208,347-371`).
- An output can carry one source's audio alone only as a discrete track, and changing it needs a
  restart. There is no per-output audio-follow-video.
- On mda001 (read-only): GPU (wgpu) compositor, RTX 4060 with 5 NVENC sessions at 21 % encoder load,
  10 CPU threads at about 20 %, canvas 1920x1080 at 30000/1001.

## Decision

1. **Two layers of bus.**
   - **Picture buses** carry raw frames on the tick: `program` (the M/E's result, so a CUT or AUTO
     happens on it), `preview`, `clean`, `src:<id>` (any source; VT, bars and the other
     generators are sources) and `aux:<layout id>`.
   - **Encoders** are keyed by `(picture bus, encoder settings)`. Each writes one **encoded bus**
     with zero, one or many subscribers. A bus with no subscriber closes after a 10 s linger.
   - **An output is a subscriber of one encoded bus.** It never owns an encoder. So every output
     can always be routed on its own.
   - Audio has the same two layers: audio buses (`program` mix, `src:<id>`, layout audio) and
     encoded audio buses keyed by `(audio bus, AAC settings)`.
   - `{bus: program}` (follows the switcher) and `{source: X}` (direct, ignores the switcher) are
     distinct routes everywhere: config, API, UI and tally.
   - Config: `[[routing.output]] { output, video, audio }`. An absent entry means Program and
     follow, exactly today's behaviour, encoders and pixels. The existing `bus` field is kept as an
     alias. No operator-facing string names an encoder grouping.

2. **Aux canvases of the one Program, on the Program's clock.** A routed layout, single source or
   Preview is produced by a `CompositorDrive` over the **same** `TileStore` map Program samples
   (decode once, use many). Its tick T is composed at `T.pts` on one off-clock `routing-aux` OS
   thread. The Program clock's only new work is one `try_send(Tick)` (capacity 4, drop on full) in
   `after_compose`.
   - A routed layout is a second canvas of the same Program, not a second ADR-0030 Program: the cut,
     the stamps and the cadence are all the Program's.
   - A single source is a **one-cell canvas** (`fit = contain`), not a tile pass-through. Scaling,
     the fixed colour order and the cadence latch live in the compositor. The identity case (tile
     equals the canvas in size, format and full `ColorInfo`) skips drawing.
   - Ingest tiles already carry the canvas colour tag, so in slices 1-2 a non-identity single source
     is a pure-integer CPU letterbox (the ADR-0103 scaler, 14.1 ms per 1080p frame on one core).
     **No GPU compose is added before slice 3.** Slice 3 starts on the integer path, and wgpu aux is
     an upgrade gated by measurement.
   - Routed sources join the decode-size plan (a *sizing layout*: Program cells plus one full-canvas
     cell per routed source), through the existing make-before-break handover. Each resize is
     costed in the plan and capped by admission.
   - The routed path shows exactly the samples Program shows for that source. Ingest retags tiles
     to the canvas colour today (`crates/multiview-cli/src/pipeline.rs:14229`), and native-colour carry-through is a
     pre-existing gap (#310).
   - The aux thread is a dedicated OS thread at `nice +10`, never on the tokio runtime that hosts
     the output clock.
   - Clean is the Program canvas before the overlay bake, with no extra compose.
   - Preview is a one-cell canvas that follows the M/E's PVW selection through a `SwapCell`.
   - An aux canvas exists only while something selects it.

3. **A take moves the output's subscription at an IDR. It is a clean cut, never a restamp.**
   - **Destination exists** (another output already carries that picture bus with those settings):
     **+0 encoders**. The engine rebinds the output at an IDR of that bus. It forces one only within one
     per-encoded-bus floor of 2 s, shared with the ADR-0049 PLI path. Otherwise it waits for the
     next natural IDR, at most one GOP.
   - **Destination does not exist:** **+1 encoder**. The engine opens it off the clock on its own
     worker thread, starting at an absolute tick T0 (`starting_at(T0)`, added to
     `VideoRenditionEncoder` and the AAC state, which start at 0 today, `crates/multiview-output/src/sink.rs:604,1141-1159`). It
     moves the output at a forced IDR at T ≥ T0 + 1 (T ≥ T0 + 2 for audio, after priming).
   - All encoders stamp PTS = DTS = the tick index, so the step is one tick across the move.
   - Every encoder is opened with `forced-idr=1`. The move happens only on a decoder-ready RAP
     (`is_decoder_ready_rap`).
   - Encoders with the same settings produce byte-identical parameter sets, carried in band at
     every IDR (no `GLOBAL_HEADER`). A mismatch is an internal fault (`encoder-format-mismatch`),
     never silent.
   - The dispatcher gains `rebind(output, video_key, audio_key, at_tick)`, atomic per output at a
     window boundary. The consumer `try_send`s to a new worker and never waits for it.
   - The dispatcher registers a new encoder key before its first packet. A window from a key with
     no bound output is dropped, never an error, because today an unknown key aborts delivery for
     every output (`crates/multiview-cli/src/rendition_dispatch.rs:319-344`).
   - Closing an encoder (EOF, flush, join) runs on a reaper thread, never on the consumer.
   - Opens are serialised per GPU, and the session is reserved at admission.
   - Lagging aux buses hold last-good with their own generation label: `routing.degraded` at 60
     ticks, then a move to Program at an IDR at 150 ticks, whether the aux thread is dead or wedged.
   - An output's format never changes on a take. A format change is RT-15.
   - An output-level dissolve is out of scope: it needs both pictures in one encoder. Program's
     transitions reach the outputs because they happen on the program bus.

4. **Take gate (ADR-0104 reused), run on the bake consumer.** The take state machine runs on the
   bake consumer, which handles every tick. The aux thread only composes. So a wedged aux thread
   cannot stop takes from completing or being refused. A take applies only when its feed is fresh at that tick
   (`state_at(T.pts).is_live()` for sources and Preview; a composed frame for a layout). It is held
   up to 50 ticks, then refused with `feed-not-fresh`. Until then the output keeps its last-good
   feed: never a stale, slate or black frame from a feed that was not ready. On-demand sources are
   leased before enqueue.

5. **Audio follows the feed by default.**
   - Program and Clean carry the program mix.
   - A source, VT or generator carries its own audio.
   - Preview carries the PVW source's audio.
   - A layout carries its `audio` attribute (default `program`).
   - An explicit selection overrides follow.

   Audio buses are built on the bake consumer from the per-source blocks the program bus already
   pulls each tick (every source is already a strip, mixed at gain 0 unless included). An output's
   audio subscription moves with its video, in the same rebind.

   Audio moves on one **global AAC frame grid**: every encoded audio bus emits frames on multiples
   of 1024 of the run's sample counter, and a live-opened bus starts on that grid. An output's
   audio moves at the first grid boundary G at or after the video move (at most 21 ms later), so
   there are no duplicated or missing samples. The boundary at G always joins two encodings of the
   **same** signal:
   - the output moves onto a private (CPU, transient) encoded audio bus fed the old audio up to G;
   - it crossfades old to new from G over one tick, `SampleClock::total_at` exact, inside that
     stream;
   - if the destination is shared, the output moves on to the shared bus at least 4 frames after
     the fade, and only when the decoded output across that boundary matches the shared bus alone
     (an encoder-state check, not just equal samples). The private bus lives at most 2 s, and then
     becomes the output's own audio encode. Other subscribers never hear the fade.
   The rule is needed because a spike measured a decoder transient of 97-919 samples when two
   different-content AAC streams are spliced. AudioSpecificConfig identity is checked at open.
   `audio-routing` stays the source of gain, mute and membership. The existing per-output `tracks`
   keep their meaning.

6. **Layout-as-a-source (#243).**
   - An active layout canvas publishes into a `TileStore` named `layout:<id>`. Program and Preview
     publish into `bus:program` and `bus:preview`.
   - The M/E's PVW/PGM/AUTO and layout cells select these like any source. They are one tick behind
     their own sources in steady state. Their audio is delayed by the measured picture age
     (`source_times`): a bounded 4-tick ring, with 30-tick hysteresis and a resampled sample slip
     (at most a 0.5 % rate offset) when the age changes. Never a crossfade of the signal with a
     delayed copy of itself.
   - Feed cycles are refused at plan time and at adoption.
   - A PVW/PGM/AUTO select of `layout:L` requires Read on every source L shows, transitively.
   - `first_slice_me_layout_supported` is removed: with a switcher, the Program canvas is the M/E
     bus, and the configured multi-cell layout becomes the routable stored layout `config`.
   - `apply-layout` keeps its meaning without a switcher. With a switcher it answers 422
     `switcher-owns-program` and names the two operations that do what is meant. It can take
     `target: {output}` as an alias of an output take.

7. **API.** The routing resource is extended rather than adding a new one:
   - `GET /api/v1/routing` (the whole state, filtered per object);
   - `POST /routing/plan` with output targets (class, cost, typed refusals);
   - `POST /routing/output/take` (`Idempotency-Key`, `202`, adoption at the next tick);
   - `POST /routing/salvo/{plan,take}` (atomic at one tick or not at all; the stored `Salvo` gains
     RT-16's `output` recall);
   - a correlated realtime event `route.output` with typed outcomes.

   Authorization is per object on both ends: role Write plus `authorize_object` on the output (the
   axis the output routes use), and Read on the source. A layout feed needs Read on the layout and
   on every source its cells bind. A bus feed needs the Program scope and the M/E; that deliberately
   replaces per-source Read and is confined to Program-scope holders.
   - The rule holds after the take: editing a layout that is an output's feed requires Write on that
     output and Read on any added source, or it is refused with 403 `routed-layout`.
   - Plan, 409 and event bodies redact what the caller cannot read.
   - Takes are persisted only after `applied`.
   - Idempotency keys are body-blind (a new key per distinct take).
   - Takes are limited to 2 per second per output and 10 per second per principal; forced IDRs to
     one per 2 s per encoded bus.
   - `apply-layout` with a switcher becomes 422, a breaking change documented in the note.

8. **Admission (invariant #9).**
   - At most `routing.max_aux_canvases` (default 2) aux canvases.
   - A take that needs a new encoder is admitted against the probed free NVENC sessions minus the
     sessions the config's wall, WHEP and preview ladder need (3 on mda001; 12 per system on the
     current driver, so 7 remain for routed encoded buses), and `routing.max_encoders` (default 6).
   - Lingering and live routed buses are attributed per principal: at most one linger besides its
     subscriptions, and `routing.max_encoders_per_principal` (default 2) in total. Lingers close
     first under pressure.
   - The plan states "+0 encoders" or "+1 encoder". The only refusal for this reason reads "This
     output needs its own encoder and the GPU has no free encoder session (N of M in use)" for
     whole-system principals, and "No free encoder session for this output" for scoped principals,
     which never see fleet counts or unreadable bus ids.
   - At most `routing.max_decode_escalations` (default 2) decode resizes are forced by routing.
   - In the default GPU-free build, aux canvases use the identity path or the ADR-0103 integer box
     scaler, never the CPU reference compositor (measured: not viable at full cadence).
   - A refused take answers 409 `capacity`, naming the resource and its holders.
   - Program is never degraded: the aux thread runs at lower priority and drops ticks first, routed
     outputs repeat last-good, and the aux thread halves its cadence if Program's lateness p99 passes
     half a tick.

9. **UI.** The Routing page becomes a crosspoint matrix (video and audio tabs; feeds grouped as
   Buses / Layouts / Players / Generators / Sources against outputs, PGM/PVW and layout cells), with
   plan/take, the take lock, tally and named disabled reasons. Each Outputs page card shows and
   changes its feed live.

## Spike (scratch, deleted)

The spike used:

- the real `EngineRuntime` with `RealtimePacer` at 30000/1001 for 900 ticks, Program a one-cell
  720p canvas encoded with libx264;
- an off-clock thread fed by one `try_send(Tick)` from `after_compose`, composing a 6-cell layout
  and a one-cell source from the same `TileStore`s, each into its own libx264;
- live takes on output B: b, then never-fresh `dead`, then c, then the layout, then d, under a flood
  of about 20,000 takes at 2,000 per second.

Results (ffprobe and the run log):

- **Routed output:** every PTS step exactly 3003 (90 kHz), DTS = PTS. IDRs exactly on the cut ticks;
  one run cut at 67, 256, 364 and 457 and keyed at those ticks, besides GOP 60.
- **The untouched routed output** kept a pure GOP-60 cadence.
- **The `dead` take** was refused at 50 ticks with b held.
- **Program:** 900/900 ticks and 0 repeats in both arms. Tick lateness with the aux thread on 4
  cores: p99 13.6-15.6 ms baseline vs 18.8-19.8 ms with routing, and max 18-19 vs 31-34 ms.
  That is +4 to +5 ms at p99 and +13 to +15 ms at max; the max came within 2 ms of a whole tick.
  The spike's aux canvases used the CPU reference compositor, far heavier than slice 1's path.
  **The spike proves** no lost or repeated Program tick, continuous PTS on the routed output and
  clean cuts. **It does not prove** that Program lateness is unaffected. That is measured per slice
  on mda001 against a stated bound (p50 +1 ms, p99 +2 ms, max +5 ms and below half a tick, 0
  repeats, HLS PTS delta 3003 on every packet; note §10).
- **Starved aux thread:** the routed outputs fell to 110/900 fresh frames, gap-filled at a constant
  PTS step, while Program stayed at 0 repeats.
- **Cost:**
  - The CPU reference compositor costs p50 198 ms per 720p frame on one core (44 ms on 10 cores).
  - The ADR-0103 integer blit costs 9.4 ms (720p) and 14.1 ms (1080p) on one core.
  - Stereo AAC costs about 2.7 % of a core.
- **Second spike (the operator's model): a subscription move between running encoders.**
  - Setup: encoders A (`program`, from tick 0), B (`src:bars`, **opened live at tick 200** with
    absolute PTS) and C (`src:bars`, from tick 0), all with the production option set plus
    `forced-idr=1`. Output X moved A → B at 210 and B → A at 330; output Y moved A → C at 250.
    Packets were muxed to TS, with no re-encode.
  - Results, for libx264 and libx265 alike:
    - SPS/PPS (and VPS) of B and C byte-identical to A's;
    - 450 packets per output, every PTS step 3003, DTS = PTS;
    - the first packet after every move a keyframe;
    - 0 decode errors;
    - the decoded luma switched exactly at the move frame (209 → 210: 69 → 183).
- **AAC move:** splicing two different-content AAC streams at a frame boundary showed a decoder
  transient 97-919 samples into the frame, with no click step. Hence the global frame grid and the
  same-signal boundary rule (decision 5).
- **Not proven:**
  - A GPU aux compose beside Program on the RTX 4060 (a shared queue). That is a hard acceptance
    gate of slice 3; the CPU integer path is the fallback.
  - NVENC parameter-set identity across a live open. That is slice 1 acceptance on mda001.
  - The spike used its own libx264 wrapper with `forced-idr=1`, and proved only MPEG-TS/H.264.
    Every other protocol's cut is verified in the slice that ships it.

## Rationale

- **Subscription move.** It is the operator's model and the most efficient one: an output never
  owns an encoder, so any output routes on its own; outputs that carry the same bus with the same
  settings share one encode by construction; and an unneeded encoder closes. All encoders stamp
  from the one tick counter, so the move needs no restamp. The spike shows identical parameter
  sets, so it needs no muxer re-initialisation either.
- **The shared clock.** It gives frame-aligned cuts between Program and a layout, and one stamp
  sequence. ADR-0030's separate clocks cannot do either.
- **Off-clock aux composition.** ADR-0103 proved this costs the clock nothing. Doing it on the clock
  is what ADR-0103 rejected.
- **Audio from the existing bus strips.** It adds no decode and no second store read (an
  `AudioStore` read advances its cursor).

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Each routed layout as an ADR-0030 Program (its own clock) | No frame-aligned cut against Program, separate stamp sequences, and a second seed per feed. Kept for different cadence or size outputs only |
| RT-13 restamp splicer | Not needed: all encoders share the tick counter, so DTS is already continuous across a move |
| Forcing an IDR on a shared destination at every take | Gives other principals' outputs extra IDRs; bounded instead by the per-encoded-bus floor (decision 3) |
| Switching the frames fed into an output's own encoder (round 1 of this ADR) | Outputs that share an encoder could not be routed on their own; an encoder per output defeats #7. Withdrawn after the operator's model |
| Compose aux canvases on the clock task | Puts N extra composites on invariant #1's path (ADR-0103's rejected alternative) |
| A tile pass-through for a single source | Breaks #6 (an extra scaler), #8 (a second colour conversion) and cadence conversion; the compositor already does all three in one pass |
| A new `/outputs/{id}/feed` resource | Duplicates plan/take, idempotency and BOLA, and splits the matrix over two APIs |
| The CPU reference compositor for aux canvases in the default build | Measured at 198 ms per 720p frame on one core |

## Consequences

- **Invariant #1.** The clock gains one non-blocking `try_send`. Chaos tests (take flood, failing
  feeds, ongoing authorization, wedged and panicking aux thread, wedged encoder, a wedged live encoder open) and a 1 h mda001 soak are owed by
  the implementing slices (note §9).
- **Invariant #10.** Every new channel is bounded and drops. Takes are admitted with `try_send`
  (503 when full).
- **Invariant #7** becomes a structure: one encode per (picture or audio bus, settings), whatever
  the subscriber count. Default config keeps today's encoders. A routed output costs a new encoder
  only while no other output carries its bus with its settings.
- **Backward compatibility.** Absent config gives today's pixels, samples, encoders and outputs.
  Configs with a switcher and a multi-cell layout start instead of failing.
- **ADR-0030** stays Proposed, amended: aux canvases of one Program cover output routing, and
  separate Programs remain the model for other cadences or sizes and for passthrough.
- **ADR-0034 / decoupled-routing backlog:**
  - RT-12's `OutputRef` is reused. Its sink-mover is reused as the idea and built on the production
    dispatcher (`rebind`).
  - RT-13 is superseded for routing, and stays for passthrough.
  - RT-15 stays separate (format migration only).
  - RT-16's salvo shape is reused.
  - RT-17 is superseded by slice 5.
  - SW-27 is superseded.
- **Slices** (note §10; epic #303, slices #304-#308):
  1. preview, program, VT, bars and source to any output, live and independently (also outputs with
     identical settings), from the Outputs page and the API. This is what the operator named,
     except layout, and it needs no GPU compose;
  2. explicit audio feed and Clean;
  3. layout to an output;
  4. layout-as-a-source and removal of the first-slice restriction (#243);
  5. the matrix page and output salvos.
- **Interactions.** None with ADR-0107. A future dedicated clock thread carries the one `try_send`
  unchanged.
