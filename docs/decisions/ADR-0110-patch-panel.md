# ADR-0110: The patch panel: sources patched to mixer input slots and to typed output slots; each output owns its format; the engine picks the cheapest path per crosspoint (passthrough, shared encode, shared decode + convert + encode); salvos apply many crosspoints at one tick

- **Status:** Accepted
- **Area:** Core engine / Output / Switcher / Web/API (invariants #1, #3, #6, #7, #8, #9, #10, #11)
- **Date:** 2026-11-13
- **Source:** operator requirement of 13 Nov 2026 (quoted under Context); epic #303
- **Design note:** [output-routing.md §12](../architecture/output-routing.md#12-the-patch-panel-adr-0110) (objects, config, path selection, passthrough timing, per-output clocks, API, UI, cost, tests, slices)
- **Supersedes in part:** [ADR-0108](ADR-0108-output-routing-matrix.md) decisions 1 (one video and one audio feed per output), 2 (every output at the Program canvas size and cadence), 7 (the API shape, now a superset) and the salvo half of 9; everything else in ADR-0108 stays (encoded buses, the subscription move at an IDR, the take gate, the AAC grid, isolation, admission)
- **Amends:** [ADR-0030](ADR-0030.md) (per-output clocks for an output whose frame rate differs from Program; passthrough is a path of one crosspoint, not a Program kind), [ADR-0034](ADR-0034.md) (RT-13 restamp and the strict-IDR re-entry are used again, for passthrough)

## Context

The operator, 13 Nov 2026 (verbatim):

> I think we are still not aligning on "outputs"
> We need (two?) patch panel(s) audio and video
> A source (assume audio+video for all from here on) can be routed to none one or more outputs directly; and/or none one or more input sources for a switcher/mixer bus. ie "ABC News" might be routed to Source 1 on the primary mixer bus, and directly to Output HLS Stream 1.
> Sources can also be VT's, CG, etc
> Outputs are defined and finite/fixed.
> An output (for now) is a stream, but may in the future be a hardware output like and SDI port
> An output has fixed output settings dimensions, colour space, frame rate, audio channels, subtitle channels, etc
> These can be changed live - but they "restart" / or reset the output.
> Everything should be done as efficiently as possible. As intelligently as possible.
> That means - if we "match" an input to an ouput, for example "ABC News" comes in at 24 fps 1080i h.264 and out output is 24fps 1080i h.264 - then we don't transcode - we bus it over
> If we have three outputs - one is matching, one is h.265 but rest matching and one is same res but 9:16 aspect ratio set to centre zoom and clip - we should decode the input only once, the 264 should get that bus straight away, and the two other outputs should be fed as appropraite.
> So the config for an output should configure the output's "slots". If the medium supports multiplle video then the user can add more than one video stream, same for audio, subtitles, etc.
> Then the patch allows routing to those slots. (and obviously salvos can bulk change in an instant).
> For example, a "layout" may be direct routed to HLS output, then the salvo changes t to hust be ABC News.

Still binding from ADR-0108: "Program has to be handled separately because if program is routed to an output, like HLS, that output isn't simply selecting the input to program, it's "take"ing program. And that might be a wipe, dissolve, or a straight input." and "we should be making this as absolutely efficient as possible."

What is true on `main` 2c29aa8d (citations in note §12.1):

1. An output has one video feed and one audio feed: `OutputCrosspoint { output, program, bus, video, audio }` (`crates/multiview-config/src/routing.rs:224-244`); the take target is `OutputTarget { output, stream: video }` (`crates/multiview-control/src/routes/output_routing.rs:44-58`).
2. Every output encodes at the Program canvas size, cadence and colour; the one `Canvas` (`crates/multiview-config/src/schema.rs:159-172`) is the only format. ADR-0108 §2.2 called another cadence or size "not the operator's request". It is now.
3. A routed source is decoded and re-encoded through a one-cell canvas (`crates/multiview-cli/src/routing_aux.rs:300-356`). There is no compressed path from an ingest demuxer to an output; the passthrough primitives exist only as tested building blocks (`GuardedPacketSource`, `BakedSlate`, `RestampAccumulator`, `PacketLiveness`, `idr::is_self_contained_idr`; `crates/multiview-output/src/guarded.rs`, `slate.rs:184`, `restamp.rs:40`, `crates/multiview-framestore/src/liveness.rs:223`, `crates/multiview-ffmpeg/src/idr.rs:93`) with no production caller.
4. The M/E selects PGM and PVW from any source directly (`SwitcherVerb::SelectProgram(SourceRef)`, `crates/multiview-engine/src/switcher_control.rs:72-82`). There are no mixer input slots.
5. A salvo recalls a layout, cell sources, tally and UMD only (`crates/multiview-config/src/salvo.rs:59-77`).

## Decision

1. **One patch, three essences.** The engine has one patch panel with three independent planes: **video, audio and subtitle** (breakaway). A crosspoint connects one **source essence** to one **destination slot** of the same essence.
   - **Sources** of the patch: every configured source (cameras and network inputs, VT/media players, CG/graphics, web capture, generators such as bars), every stored layout (`layout:<id>`), and the mixer's buses `me1.program`, `me1.preview`, `me1.clean`. A source may be patched to zero, one or many destinations.
   - **Destinations** are slots, and they are finite and declared:
     - **mixer input slots** `me1.in<N>` (one video and one audio slot each; N declared per M/E);
     - **output slots**: each output declares typed slots `video[]`, `audio[]`, `subtitle[]`, as many as its medium allows.
   - A destination slot carries at most one source. An audio slot's default is `follow`: the audio of the source on the slot's paired video slot.

2. **Mixer input slots.** With `[[switcher.mix_effects.inputs]]` declared, PGM and PVW select **input numbers**, and the patch decides which source sits behind each input ("ABC News on Source 1"). Re-patching an input that is on PGM changes Program at that tick: it is refused unless the request says `on_air = "allow"` (409 `on-air-protected`), and it is refused during an AUTO on that input. Without the table, every source is implicitly an input and selection by source id works exactly as today.

3. **Outputs own their format.** An output declares `format` (width, height, frame rate as an exact rational, scan, colour profile, pixel format, aspect and fit such as `cover` + centre = zoom and clip) and its slots (per video slot the codec and its settings; per audio slot channels, rate, codec; per subtitle slot format and language). Absent `format` means the Program canvas; absent slots mean one video and one audio slot: exactly today. Slots are validated against a **capability table per medium** (HLS, LL-HLS, SRT/RIST/TS file, RTMP, RTSP, WebRTC/WHEP and MoQ preview, NDI, display, future SDI): count per essence, codecs, scan, frame rates. A format or slot change is live and **resets that output only** (Class-2 for that output: its sink, encoders and muxer restart, `EXT-X-DISCONTINUITY` or a new init, PAT/PMT version bump); Program and every other output are untouched.

4. **The engine picks the cheapest path per crosspoint and the plan shows it.** For each (source, output slot):
   - **P0 passthrough**: the source's compressed stream is "bussed over" without decode or encode, when it matches the slot exactly (codec, profile and level within the slot's limits, size, frame rate, scan, all four colour tags, bit depth and chroma, GOP within the medium's segment rule, bitrate within the slot's ceiling, B-frames allowed by the medium). Only a direct source can pass through; a bus, layout or generator is a picture and is never passthrough.
   - **P1 shared encode**: another slot already carries the same picture in the same format and codec settings: subscribe to its encoded bus (+0 encoders, ADR-0108).
   - **P2 shared decode + convert + encode**: one decode per source, whatever the number of consumers, at the supremum size its consumers need (never above native); one convert per distinct (picture, format) on the compositor path, which applies invariant #8's fixed order with the slot's fit; one encode per distinct (picture, format, codec settings).
   The operator's three-output example is therefore: one demux, zero decodes for the matching H.264 output (P0), one decode shared by the HEVC output (identity convert + HEVC encode) and the 9:16 output (one `cover` convert + encode).

5. **Passthrough keeps invariant #1 on the output's clock.** A passthrough slot has its output's clock. Each tick it emits exactly one access unit from a bounded jitter buffer (default depth 3 ticks), restamped from the tick counter (`dts = tick`, `pts = tick + reorder offset`, integers, never float). The ladder, decided on the clock tick and never waiting on the demuxer:
   - jitter within the buffer: invisible;
   - **drift** (a source clock a few ppm off the output's): re-centred only at a source IDR, by dropping the access unit just before the IDR (nothing after an IDR references it) or by inserting one synthesised **repeat AU** (a non-reference all-skip picture) before it; a source with B-frames cannot take a repeat AU and uses the bridge for an underrun instead;
   - **stall** (buffer empty), **parameter change** (new SPS/PPS/VPS, size, rate, scan or colour) or a non-strict RAP: switch **at that tick** to an encoded **bridge**: the pre-baked, parameter-matched slate (ADR-0030 GP-4, zero sessions) at once, replaced at its first IDR by a picture bridge (the source decoded, converted and encoded at the slot's format) when a session is admitted. Return to passthrough only at the next strict source IDR after the source has been healthy for 30 ticks (a parameter change: 30 s, and only if it matches again);
   - **take into passthrough**: a frame-accurate cut at tick T onto the picture bridge (an ADR-0108 move), then a seamless move bridge → passthrough at the next source IDR. If no session is free, the take waits for the next source IDR instead (at most one source GOP) and the plan says so;
   - **take out of passthrough**: a clean cut onto the destination's encoded bus at its IDR (ADR-0108 decision 3);
   - **muxing rules**: one persistent muxer per output, so TS continuity counters and PCR stay continuous through every move; a move whose parameter sets differ writes `EXT-X-DISCONTINUITY` (HLS TS), a new `EXT-X-MAP` (fMP4/CMAF), a new AVC/HEVC sequence header (FLV), a new MoQ group with an updated catalog, and in-band sets for TS/SRT/RIST and WebRTC.
   - Audio is never passed through: it is decoded and re-encoded (AAC costs about 2.7 % of a core; a compressed AAC splice clicks, ADR-0108 §5.2). Audio for a passthrough video slot is read at the source time of the emitted video access unit, so lip sync is kept by construction.

6. **Per-output clocks (amends ADR-0030).** An output whose frame rate equals Program's stays on Program's clock (ADR-0108, unchanged). An output with another frame rate gets its own output clock, derived from the one shared monotonic time source, with its own tick counter for PTS. Frame-rate conversion is the store latch (`read_at` at the output tick's time: nearest-not-after, whole frames, no blending in v1). Program reaches such an output through the `bus:program` store, so a Program cut lands on exactly one output frame and a dissolve is seen as the frames Program composed. A take on such an output is frame-accurate on that output's own tick grid. A/V sync: audio blocks follow the output's own tick sample budget on the global sample counter; the video latch error is bounded by one output tick and measured.

7. **Program stays special.** A slot patched to `me1.program` carries the mixer's result, so it receives every cut, dissolve and wipe as pictures (it "takes" Program). Patching an output slot away from Program is an output take (a clean cut). Program is never moved, reset or slowed by a patch, a format change or a passthrough event.

8. **API.** `GET /api/v1/patch` (the whole patch state from the engine's live readback, filtered per object), `POST /api/v1/patch/plan` and `POST /api/v1/patch/take` (an `Idempotency-Key`, `202`, outcomes on the realtime event `patch.take`). A take carries one or many crosspoints; **many crosspoints are applied at one tick or not at all**: every member is authorized and validated first; the engine primes every member (fresh feed, warmed encoder or bridge), then schedules one adoption tick and applies every member there. Refusals are typed: `capacity`, `feed-not-fresh`, `slot-medium-mismatch`, `slot-format-unsupported`, `on-air-protected`, `feed-cycle`, `unknown-source`, `unknown-slot`, `encoder-not-ready`. A stored salvo gains `patch = [crosspoints]` and is taken through the same path. The ADR-0108 routes (`/routing/output/take`, `/routing/plan` with an output target) stay as aliases of a one-crosspoint patch on the output's first video slot. Authorization is per object on both ends: the destination (output with the output axis `ObjectAndOutput`, #329; or the M/E plus Program scope for a mixer input) and the source (Read on the source; a layout's whole source closure; Program scope for a bus). Bodies redact what the caller cannot read.

9. **Persistence and restart truth.** The patch lives in the accepted state, store-backed (never spliced from the boot file, unlike today's `routing`, #329): mixer inputs under `switcher.mix_effects[].inputs`, output slots and their patch under the output and `patch`. It is written only after the engine reports `applied`. At boot the engine starts from it, and every readback comes from the engine's live state, never from the document.

10. **Admission and cost.** The plan states per crosspoint the path and the delta: decodes (and decoded MP/s), conversions, encoder sessions, aux canvases, CPU. Admission refuses with 409 `capacity` naming the resource. Passthrough costs 0 decodes and 0 sessions; a bridge is admitted like any encoder, and a passthrough slot with no admitted picture bridge runs on its slate bridge only (the plan says so).

11. **UI.** The Routing page becomes the patch matrix (#308): Video, Audio and Subtitle tabs; sources grouped (Buses, Layouts, Players, CG, Generators, Sources) against mixer inputs and output slots; each crosspoint shows its path (passthrough, shared, converted) and its state; salvo arming; take lock. The Outputs page edits an output's format and slots (with the reset warning). The Switcher page shows numbered inputs with their patched source.

## Rationale

- **It is the operator's model.** A patch panel with finite, typed destination slots and breakaway planes is how broadcast routers and mixers are operated; mixer input slots make "ABC News on Source 1" literal and keep the switcher panel stable when sources change.
- **Efficiency by construction.** Passthrough costs nothing but a demux and a restamp; a source is demuxed once and decoded at most once; an encode exists once per distinct (picture, format, settings). This extends ADR-0108's invariant-#7 structure to formats and to the compressed domain.
- **Invariant #1 holds without trusting the source.** A passthrough output is still clock-driven: the clock decides every tick what to emit (a buffered access unit, a repeat AU, or the bridge) and never waits for the demuxer. The slate bridge is always available at once and costs no session.
- **The cut rules stay ADR-0108's.** Every move between encoded streams is at an IDR with in-band parameter sets; passthrough only adds the source's own IDR as a move point.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Keep one feed per output and add "format" as encoder settings only | Cannot carry two audio languages or subtitles, and cannot express "this slot is 9:16 centre zoom"; the operator asked for slots |
| Passthrough with the source's own timestamps (ADR-0030 §4 offset restamp) | The output's cadence would follow the source clock; a take between a passthrough and an encoded feed could not be frame-accurate, and the output's PTS would not be `f(tick)` |
| Always transcode, cache nothing | Spends a decode and a session per output; the operator rejected it ("we don't transcode - we bus it over") |
| A warm picture bridge for every passthrough slot | Costs the decode and the NVENC session passthrough exists to save; kept as an option (`bridge = "warm"`) |
| Freeze on stall by repeating the last access unit | Not a valid stream: a repeated P picture double-applies its residual; a synthesised all-skip picture is the valid freeze |
| Pass AAC through as well | Splicing compressed AAC gives a decoder transient (ADR-0108 spike: 97-919 samples) and AAC re-encode is cheap |
| A separate ADR-0030 Program per output format | Separate stamp sequences and no frame-accurate cut against Program; per-output clocks share the time source and the patch instead |
| Frame-rate conversion by motion interpolation | GPU cost per output and new artefacts; latch conversion is the broadcast frame-sync behaviour; blending can come later |
| Mixer selecting sources directly, plus "labels" | A label cannot be re-patched under a live panel; the operator asked for patching to mixer inputs |

## Consequences

- **Invariant #1.** Per output clock, exactly one access unit or frame per tick. The new clock-side work is bounded: the switcher hook resolves an input number through a wait-free table; a passthrough clock pops one queued AU or emits the bridge; a per-output clock latches one frame. Chaos and soak tests per slice (note §12.10): source stall, parameter change, drift both ways, a wedged demuxer, a take flood, salvo storms, a format reset under load.
- **Invariant #10.** Every new queue is bounded and drops (the jitter buffer drops oldest; the patch mailbox is the switcher's 64-command mailbox or the router's 64-take mailbox).
- **Invariants #6, #7, #8.** One decode per source at the supremum consumer size; one encode per key; conversion only on the compositor path in invariant #8 order. Ingest's retag without conversion (#310) blocks a non-identity colour convert until it is fixed: until then a slot whose colour differs from the source's is refused `slot-format-unsupported` rather than mislabelled.
- **Invariant #11.** A crosspoint change is Class-1 (seamless at an IDR or a tick); an output format or slot change is Class-2 for that output only, and the API says which before applying.
- **Backward compatibility.** Absent mixer inputs, absent output format and slots, and an absent patch give exactly today's behaviour. `[[routing.output]]` is read as the patch of the output's first video and audio slot.
- **Superseded in ADR-0108:** decision 1's "an output subscribes to one encoded bus" becomes "each video slot subscribes to one encoded bus or a passthrough"; decision 2's "on the Program's clock" holds only for outputs at Program's frame rate; ADR-0108 §2.2's "not the operator's request" is withdrawn; decision 7's API becomes an alias of `/patch`; the output-salvo half of slice 5 becomes patch salvos. Everything merged for ADR-0108 slices 1-2 stays and is reused.
- **Slices** (note §12.11; epic #303): A mixer input slots and the patch API; B output slots and per-output format (same frame rate); C per-output clocks; D passthrough; E patch salvos and the patch matrix page; F subtitle slots. ADR-0108 slices 2-4 (#305-#307) continue as patch sources. Each slice carries the Program protection bound on mda001.
