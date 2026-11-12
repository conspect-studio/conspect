# ADR-0108: Route anything to any output — per-output video and audio feeds produced by aux canvases of the one Program, raw switch with a forced IDR at a tick boundary, layout-as-a-source

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

What is true on main f5f2230e (the full table with citations is in §1 of the note):

- `OutputBus { Program, Clean, Preview }` exists in config (`crates/multiview-config/src/routing.rs:204-240`),
  but nothing at runtime reads it. Every output encodes the one overlay-baked Program canvas
  (`crates/multiview-cli/src/pipeline.rs:7172-7206`), and encoders are keyed only by codec
  (`pipeline.rs:2820-2844`).
- Output changes are restart-class (`crates/multiview-control/src/routes/outputs.rs:185,254`).
- `apply-layout` replaces the one live canvas (`crates/multiview-control/src/routes/mod.rs:756-942`).
- With `[switcher]` declared, startup admits only a one-cell canvas (`pipeline.rs:281-302,2438`), so
  a multiview and a switcher cannot both be on air (#243).
- Route takes exist for cells, bus channels, tracks and layers, but there is no `output` kind and no
  `GET` of the state (`crates/multiview-control/src/routes/routing.rs:107-208,347-371`).
- An output can carry one source's audio alone only as a discrete track, and changing it needs a
  restart. There is no per-output audio-follow-video.
- On mda001 (read-only): GPU (wgpu) compositor, RTX 4060 with 5 NVENC sessions at 21 % encoder load,
  10 CPU threads at about 20 %, canvas 1920x1080 at 30000/1001.

## Decision

1. **Feeds.** `Feed = Bus(program|preview|clean) | Layout(id) | Source(id)`. VT, bars, solid, clock,
   timer and graphics are already `SourceKind`s, so they are routed by source id; the UI lists them
   in their own groups. `AudioFeed = Follow | Program | Source(id) | Tracks([..]) | None`. In config:
   `[[routing.output]] { output, video, audio, route_group }`. An absent entry means Program and
   follow, exactly today's behaviour. The existing `bus` field is kept as an alias.

2. **Aux canvases of the one Program, on the Program's clock.** A routed layout, single source or
   Preview is produced by a `CompositorDrive` over the **same** `TileStore` map Program samples
   (decode once, use many). Its tick T is composed at `T.pts` on one off-clock `routing-aux` OS
   thread. The Program clock's only new work is one `try_send(Tick)` (capacity 4, drop on full) in
   `after_compose`.
   - A routed layout is a second canvas of the same Program, not a second ADR-0030 Program: the cut,
     the stamps and the cadence are all the Program's.
   - A single source is a **one-cell canvas** (`fit = contain`), not a tile pass-through. Scaling,
     the fixed colour order and the cadence latch live in the compositor. The identity case (tile
     equals the canvas in size and tag) skips drawing.
   - Clean is the Program canvas before the overlay bake, with no extra compose.
   - Preview is a one-cell canvas that follows the M/E's PVW selection through a `SwapCell`.
   - An aux canvas exists only while something selects it.

3. **Raw switch, never an encoded splice, for routing.** A routed output has a feed selector in
   front of its own encoder. Outputs in one `route_group` share the selector and the encoder (#7).
   Default outputs keep sharing the Program encoders as today.
   - A take is adopted at a tick boundary by the aux thread: per output, the latest take wins.
   - The output's encoder forces an IDR on the first frame of the new feed generation (keyed on the
     generation, so a dropped frame cannot lose it). PTS = DTS = the tick index. Missing ticks are
     gap-filled with the same generation's last frame.
   - An output's format is pinned and never changes on a take. A format change is RT-15's
     make-before-break migration, a separate operation.
   - The first take of a default output moves its binding once, from the shared Program encoder to
     its routed encoder, at the routed encoder's first (IDR) frame. Both encoders stamp from the same
     tick counter. The take is refused with `encoder-headers-differ` if the extradata differ on a
     transport that cannot carry in-band parameter sets (fMP4, FLV).

4. **Take gate (ADR-0104 reused).** A take applies only when its feed is fresh at that tick
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

   Routed audio is built on the bake consumer from the per-source blocks the program bus already
   pulls each tick (every source is already a strip, mixed at gain 0 unless included), and encoded
   by the output's own AAC encoder. A take crossfades old into new with the equal-power envelope
   over exactly `SampleClock::total_at(T+1) − total_at(T)` samples, starting on the video cut tick.
   `audio-routing` stays the source of gain, mute and membership. The existing per-output `tracks`
   keep their meaning.

6. **Layout-as-a-source (#243).**
   - An active layout canvas publishes into a `TileStore` named `layout:<id>`. Program and Preview
     publish into `bus:program` and `bus:preview`.
   - The M/E's PVW/PGM/AUTO and layout cells select these like any source. They are one tick behind
     their own sources, and their audio is delayed by exactly one tick of samples.
   - Feed cycles are refused at plan time and at adoption.
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

   Authorization is per object on both ends. Write on the output; Read on the source. A layout feed
   needs Read on the layout and on every source its cells bind. A bus feed needs the Program scope
   and the M/E. Takes are limited to 10 per second per output.

8. **Admission (invariant #9).**
   - At most `routing.max_aux_canvases` (default 2) aux canvases.
   - At most `min(routing.max_routed_encoders, probed free NVENC sessions − 2)` routed encoders.
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
  cores: p99 13.6-15.6 ms baseline vs 18.8-19.8 ms with routing (max 18-19 vs 31-34 ms). The
  shared host ran at load 25-90, so ±5 ms is noise.
- **Starved aux thread:** the routed outputs fell to 110/900 fresh frames, gap-filled at a constant
  PTS step, while Program stayed at 0 repeats.
- **Cost:**
  - The CPU reference compositor costs p50 198 ms per 720p frame on one core (44 ms on 10 cores).
  - The ADR-0103 integer blit costs 9.4 ms (720p) and 14.1 ms (1080p) on one core.
  - Stereo AAC costs about 2.7 % of a core.
- **Not proven:** a GPU aux compose beside Program on the RTX 4060. That is a measured acceptance
  gate of slice 3.

## Rationale

- **A raw switch.** It keeps one encoder and one tick counter per output, so continuity holds by
  construction and the consumer sees an ordinary IDR. An encoded splice (RT-13) would force IDRs on
  other outputs' encoders, or wait up to a GOP. It would also restamp DTS across two encoders.
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
| Encoded splice between per-feed encoders (RT-13) | Forces IDRs on encoders shared with other outputs, or waits for a GOP; a DTS restamp seam; no benefit when every feed is composed anyway |
| Compose aux canvases on the clock task | Puts N extra composites on invariant #1's path (ADR-0103's rejected alternative) |
| A tile pass-through for a single source | Breaks #6 (an extra scaler), #8 (a second colour conversion) and cadence conversion; the compositor already does all three in one pass |
| A new `/outputs/{id}/feed` resource | Duplicates plan/take, idempotency and BOLA, and splits the matrix over two APIs |
| The CPU reference compositor for aux canvases in the default build | Measured at 198 ms per 720p frame on one core |

## Consequences

- **Invariant #1.** The clock gains one non-blocking `try_send`. Chaos tests (take flood, failing
  feeds, wedged and panicking aux thread, wedged routed encoder) and a 1 h mda001 soak are owed by
  the implementing slices (note §9).
- **Invariant #10.** Every new channel is bounded and drops. Takes are admitted with `try_send`
  (503 when full).
- **Invariant #7.** Shared encodes are preserved for default outputs and inside a `route_group`. An
  output that leaves Program keeps its own encoder until restart.
- **Backward compatibility.** Absent config behaves exactly as today. Configs with a switcher and a
  multi-cell layout start instead of failing.
- **ADR-0030** stays Proposed, amended: aux canvases of one Program cover output routing, and
  separate Programs remain the model for other cadences or sizes and for passthrough.
- **ADR-0034 / decoupled-routing backlog:**
  - RT-12's `OutputRef` is reused, and its sink-mover is superseded for routing.
  - RT-13 is superseded for routing, and stays for passthrough.
  - RT-15 stays separate (format migration only).
  - RT-16's salvo shape is reused.
  - RT-17 is superseded by slice 5.
  - SW-27 is superseded.
- **Slices** (note §10):
  1. source, VT, bars or Preview to an output, live, from the Outputs page and the API;
  2. explicit audio feed and Clean;
  3. layout to an output;
  4. layout-as-a-source and removal of the first-slice restriction (#243);
  5. the matrix page and output salvos.
- **Interactions.** None with ADR-0107. A future dedicated clock thread carries the one `try_send`
  unchanged.
