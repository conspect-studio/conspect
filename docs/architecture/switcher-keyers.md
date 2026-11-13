# Design note: upstream and downstream keyers on the M/E (ADR-0113)

Decision record: [ADR-0113](../decisions/ADR-0113-keyers-on-the-program-drive.md). Model
background: [ADR-0056](../decisions/ADR-0056.md), [production-switcher.md §6](../research/production-switcher.md#6-keyers).
This note is the build plan on main as of `1b59d254`. It is design only: nothing here is built yet.

Vocabulary: **USK** is an upstream keyer. It belongs to the M/E scene and can change state with
a background transition. **DSK** is a downstream keyer. It belongs to the program and stays on
through background transitions. **Fill** is the picture a keyer shows. **Key** is the signal
that shapes it. **Mask** is a rectangle outside which the key is off. **Fly** is the keyer's
destination rectangle on the canvas.

## 1. What exists and what is reused

| Need | On main today | Reused as |
| --- | --- | --- |
| One compose pass for Program | `CompositorDrive::compose` builds one z-ordered `Tile` list from pooled scratch and makes one backend call (`crates/multiview-engine/src/drive.rs:558`). AUTO adds one full-canvas tile (`set_mix_overlay`, `drive.rs:256`). | Keyer tiles are appended to the same list (§3). |
| Per-pixel alpha in both kernels | `Nv12Image` with a straight R8 plane from `AlphaPlanePool` (`crates/multiview-compositor/src/pipeline.rs:172-261`). The CPU multiplies it at the premultiply step (`pipeline.rs:1401`, `:1680`). The GPU uses `a_planes` and `TileParams.dst_size.zw` (`composite.wgsl:60`, `:151`). | `linear` keys read this plane directly. Luma and mask math go next to it. |
| Fills with alpha | Graphics sources `graphic` and `graphics_channel` publish NV12+A and do their own opacity mix on their generator thread (`docs/architecture/graphics.md`). | DSK fills with alpha (lower thirds, bug, weather). |
| Key signal from any video | Every source is NV12. Its Y is the luma key. | Self luma key, or a fill and key pair through a second source. |
| Media with alpha | Not decoded. FFmpeg has no `yuva` path. NDI drops alpha (`crates/multiview-input/src/ndi/convert.rs:369`). | Slice K6. |
| Descriptor types | `KeySource`, `KeyType`, `KeyerTransition`, `KeyerDescriptor`, `ArmedSet` in `crates/multiview-core/src/switcher/descriptor.rs`. `SwitcherState.keys_on_air` and `arm()` in `crates/multiview-engine/src/switcher.rs`. | Kept and extended. They are not wired to anything today. |
| Fresh-frame gate | ADR-0104: `state_at(tick.pts).is_live()`, held up to 50 ticks, typed refusal. | Applied to fill and key before a keyer goes on. |
| Transition weights and masks | AUTO: `elapsed / duration`. DIP: `dip_weights` (lane `transitions`, in progress). WIPE: a per-tile mask (lane `transitions`, in progress). | An armed USK or tied DSK reuses the same weight or mask (§4). |
| Clean | Program canvas before the monitoring bake, published by the bake consumer (ADR-0108 §2.5). | Pre-DSK picture, only when needed (§6). |
| Tally | Browser-derived from `program_source` and `preview_source` (`web/src/components/broadcast/deskSafety.ts`). The engine arbiter is unwired (#42). | Engine-derived from the tile list (§9). |

## 2. Data model

Config (accepted state, typed, `deny_unknown_fields`; definitions persist only after the engine
reports `applied`):

```toml
[[switcher.mix_effects.keyers]]        # USK, at most 4 per M/E
id = "usk1"                            # unique within the M/E; object id "me1/usk1"
fill = "cam-3"                         # source id or "in:<n>" mixer input
key = "cam-3-matte"                    # optional; absent = self key
type = "luma"                          # luma | linear | (chroma, slice K7)
clip = 0.10                            # luma: [0,1] on post-range-expand Y
gain = 4.0                             # luma: >= 0
invert = false
mask = { x = 0, y = 0, w = 1920, h = 1080 }   # fill-source pixels; optional
fly = { x = 1280, y = 60, w = 576, h = 324 }  # canvas pixels, even, >= 16; optional = full canvas
z = 1                                  # order among USKs
loss = "drop_to_off_air"               # | "hold_last_good" (ADR-R011 §3)
loss_trigger = "reconnecting"          # | "stale" | "no_signal"

[[switcher.downstream_keyers]]         # DSK, at most 2 per program
id = "dsk1"                            # object id "main/dsk1"
program = "main"
fill = "gfx-channel-1"                 # e.g. a graphics channel with alpha
type = "linear"
rate = 15                              # AUTO frames, 1..=300
tie = false
on_at_start = false                    # the station bug: goes on at boot through the fresh gate
z = 1
```

`premultiplied = true` and `type = "chroma"` are refused at validation with typed messages
until their slices land. The `KeyType::DvePip { rect }` descriptor deserializes as
`type = "linear"` with `fly = rect`.

Runtime state, per keyer, in the switcher tick driver: a fixed array allocated at attach (at most
6 entries). There is no per-tick allocation.

```text
KeyerRuntime { on_air: bool, armed_or_tied: bool, ramp: Option<{start: i128, rate: u32, to_on: bool}>,
               pending: Option<(command, first_tick)>, held_since: Option<u64> }
weight(tick) = ramp ? f(tick.index - start) : (on_air ? 1 : 0)
```

On-air, armed and ramp state are not persisted. A restart comes up with every keyer off, except
DSKs with `on_at_start`.

## 3. Render: the tile list per tick

Bottom to top. Every entry is one `Tile`, and each is drawn only if it contributes:

1. Program cell (outgoing source), as today.
2. Transition tiles of lane `transitions`: the AUTO incoming tile, the DIP colour tile and incoming
   tile, the WIPE incoming tile with its mask.
3. USKs in ascending `z`.
4. DSKs in ascending `z`.
5. FTB cover (reserved; not part of this work).

`Tile` (CPU) and `TileParams` (GPU) gain:

```text
key:  { kind: none|luma|linear, clip: f32, gain: f32, invert: bool,
        key_image: Option<&Nv12Image> /* GPU: key layer index */ }
mask: Option<{x,y,w,h}>   // source pixels
weight: f32               // folded into `opacity` on the CPU; separate lane not needed
wipe: Option<mask params> // lane transitions' per-tile mask, reused for armed USK / tied DSK
```

Per pixel, in both kernels: sample the fill. Compute `ka` from the key signal: with `luma`, use
`clamp((y_norm - clip) * gain, 0, 1)` on the key image's post-range-expand Y (the fill when
there is no key image), then `invert`. With `linear`, use the key image's Y, else the fill alpha
plane, else 1. Apply `mask`, giving 0 outside it, then the wipe mask. Then
`a = opacity * weight * fill_alpha * ka * mask`. The existing steps 2–5 are unchanged.

The key image is sampled at the same position as the fill, in normalized source coordinates:
`kx = floor(sx × key_w / fill_w)` (nearest neighbour, as the scaler). A key source of another
size therefore lines up with its fill. Both images latch `read_at(tick.pts)` independently, and
the fresh gate and loss rule (§4) treat them as one unit.

GPU layout: the key image's Y plane goes into the existing `y_planes` array at its own layer
(only Y is sampled). `TileParams` grows by two 16-byte lanes: `key: vec4<f32>` (clip, gain,
kind | invert flags, key layer as float bits) and `mask: vec4<u32>`. With a key source, a keyed
tile uses 2 of `MAX_TILES = 64` layers. At most 6 keyers, plus 3 transition tiles and 1 Program
cell, makes 16 layers at most. The arm-time and admission checks keep the total under 64.

## 4. Behaviour

| Event | USK (armed) | USK (not armed, on air) | DSK tied | DSK not tied |
| --- | --- | --- | --- | --- |
| CUT | flips on the CUT tick | stays at weight 1 | flips on the CUT tick | unchanged |
| AUTO MIX `p` | on: `p`; off: `1 − p` | 1 | same as armed USK | unchanged |
| DIP | on: incoming weight of `dip_weights`; off: outgoing weight | 1 | same as armed USK | unchanged |
| WIPE | on: wipe mask; off: inverted wipe mask | 1 | same as armed USK | unchanged |
| Key-only transition (background not armed) | uses the selected type and rate; DIP is drawn as MIX | 1 | — | — |
| `keyer_cut` | — (USKs change via `arm` + CUT/AUTO) | — | flips at the next tick | flips at the next tick |
| `keyer_auto` | — | — | ramps over its rate | ramps over its rate |

USKs: `arm {background, keys}` sets the next-transition set (`SwitcherState::arm`, already
validated for duplicates and range). When a transition completes, the armed keys' `on_air` is
toggled and the background swaps as today. An AUTO aborted on its terminal tick (ADR-0104 §2)
also restores every armed key's state.

Fresh gate: the tick a keyer would start going on, its fill and key must be live. If not, the
command waits in that keyer's own pending slot, at most 50 ticks, then is refused with
`keyer source has no fresh frame before the bounded take deadline`. A background transition
carrying keys going on is gated on all of them and on the incoming background. Going off is
never gated.

Loss (ADR-R011 §3): on each tick, a contributing keyer whose fill or key state is at or past
`loss_trigger` goes off at that tick (drop, no ramp) under `drop_to_off_air`. It emits
`switcher.keyer {id, on_air:false, reason:"source-lost"}` and is not re-latched. A slate sample
is never keyed: a keyer whose sample is a slate contributes nothing on that tick. Under
`hold_last_good` the held frame keeps being drawn, and the state shows `stale: true`.

Demand leases: assigning a fill or key takes a video lease (make-before-break, as
`select_preview` does). Leases are released when the keyer's definition stops naming the source.
That means an off keyer keeps its sources warm, which is the operator's expectation for a ready
lower third. The cost is visible in `GET /api/v1/sources/status`.

## 5. Cost per key

All figures are 1920x1080 NV12 canvases. Measured figures carry their source. The rest are
estimates that slice K1 measures on mda001.

| Item | Cost | Source |
| --- | --- | --- |
| A tile covering one canvas of area, GPU (M3 Ultra, incl. upload + readback) | 9 tiles of 1/9 canvas: 1.60 ms median opaque, 1.77 ms with 1 alpha tile, 2.07 ms with 6 alpha tiles | ADR-0105 (`alpha_composite_bench`) |
| The same, CPU reference | ~7 ms median | ADR-0105 |
| Full-canvas keyed tile, GPU | ≈ one more canvas fold + one 3.1 MB NV12 upload (+2.1 MB alpha or key Y). Estimate 0.3–0.6 ms on the RTX 4060. | to measure in K1 |
| Key source | +1 Y-plane upload (2.1 MB/tick at 1080p, 62 MB/s at 30 fps) and one texture sample per covered pixel | design |
| Flying key at 30 % size | ≈ 0.09 canvas of fold work | area rule |
| Luma math | 3 ALU ops and a clamp per covered pixel | design |
| Idle keyer | 0: no tile, no upload | design |
| Pre-DSK Clean call (§6) | one more backend call over the non-DSK tiles while Clean is selected and a DSK contributes | design |
| Memory | no new pools. Fill and key images are store `Arc`s. Alpha comes from the producer's `AlphaPlanePool`. The GPU key layer uses the existing `y_planes` array. | code |

## 6. Clean (pre-DSK)

Clean is published by the bake consumer as the Program canvas (ADR-0108 §2.5). Once DSKs exist,
Program contains them, so Clean needs its own picture while a DSK contributes:

- The output router already knows whether any output selects Clean (`OutputRouter`, one cached
  bool). The hook reads that flag through a wait-free `AtomicBool` the router sets.
- When the flag is set and a DSK contributes on this tick, `compose` makes a second backend call
  after the Program call. It uses the same pooled tile list truncated before the first DSK tile.
  The result rides the `CompositedFrame` as `clean: Option<Arc<Nv12Image>>`. The bake consumer
  publishes that `Arc`, if present, instead of `item.canvas`.
- If the second call fails, Clean repeats its last frame for that tick
  (`clean_repeats` counter). Program has already been composed and is unaffected.
- Otherwise (no Clean subscriber, or no DSK on air) Clean stays the Program alias at zero cost.

## 7. Admission (invariant #9)

- Hard limits at validation: 4 USKs per M/E, 2 DSKs per program, 1 key source per keyer, and fly
  and mask inside bounds. Total layers ≤ 64 is checked at arm time.
- Budget: `switcher.keyer_budget`, in canvas areas. The default is 4.0 on the GPU backend and
  1.0 on the CPU backend (the CPU reference is about 7 ms per canvas area on a fast host and is
  not viable for more on mda001-class CPUs).
  - Each contributing keyer charges `area(fly) / area(canvas) × (1 + 0.25 × has_key_source)`.
  - The pre-DSK Clean call charges the area of every non-DSK tile while it runs.
- `/plan` returns the delta and the total ("+0.30 of 4.00; 1.10 in use: dsk1 0.30, usk1 0.80").
  The command is refused with 409 `capacity`, naming the holders the caller may read (others are
  counted, not named). The engine re-checks at the tick with the same rule. A refusal there is
  an outcome, never a dropped or late frame.
- An in-flight keyer ramp is never shed. Under an engine degradation the next keyer-on is refused;
  on-air keyers stay.

## 8. API

All on the existing switcher routes. Each command is a plan and command pair with an
`Idempotency-Key`, a 202 with an `operation_id`, and an outcome on `switcher.operation`.

| Verb | Body | Notes |
| --- | --- | --- |
| `arm` | `{me, program, background: bool, keys: ["usk1"]}` | USK next-transition set. Refused during a flight. |
| `keyer_cut` | `{me, program, keyer, on: bool}` | DSK, or a USK cut outside a transition |
| `keyer_auto` | `{me, program, keyer, on, duration_frames?}` | DSK only. 1..=300 frames. |
| `keyer_tie` | `{me, program, keyer, tie: bool}` | DSK only |
| `keyer_set` | `{me, program, keyer, fill?, key?, type?, clip?, gain?, invert?, mask?, fly?, rate?, loss?, on_air?: "allow"}` | Class-1 at one tick. Persisted after `applied`. Changing the fill or key of a keyer on air needs `on_air = "allow"`, else 409 `on-air-protected`. |

`GET /api/v1/switcher/{me}` adds:

```json
"keyers": [{"id":"dsk1","family":"dsk","fill":"gfx-channel-1","key":null,"type":"linear",
            "fly":null,"mask":null,"on_air":true,"tie":false,"weight":1.0,"in_flight":false,
            "rate":15,"loss":"drop_to_off_air","stale":false,"pending":false}],
"tally": {"program":["cam-1","gfx-channel-1"],"preview":["cam-2"]}
```

Authorization (BOLA, per object): `Control` on the M/E, `Program` scope, `Control` on the keyer
object id (`me1/usk1`, `main/dsk1`), and `Read` on every source a request names. Responses and
events replace every source id the caller cannot read with `"redacted"` and drop it from
`tally`. `switcher.keyer` events are scoped by the keyer object id in the realtime filter
(ADR-W025).

Abuse: commands go through the 64-slot `try_send` mailbox (503 when full) and the bounded
idempotency store. A flood cannot grow memory or reach the clock beyond one drain per tick.

## 9. Tally

At the end of each tick the hook derives two small sets from the tile list it composed:

- `program`: the source of every tile with an effective alpha > 0. That covers background,
  transition incoming, dip-through source (when built), and the fill and key of every
  contributing keyer.
- `preview`: the PVW source, plus fills and keys of armed USKs going on and tied DSKs going on.

These are at most 2 + 2×6 ids, kept in a fixed buffer and published to the switcher snapshot
`SwapCell` only when they change. The desk renders it, and #42 feeds it into the arbiter when
that lands. A key-signal-only source is PROGRAM-tallied.

## 10. Desk

- Next to MIX/WIPE/DIP there is a keyer strip: `BKGD` and `KEY 1`..`KEY 4` toggles (the arm set,
  lit when armed), each with an ON AIR lamp.
- Per DSK: `TIE`, `CUT` and `AUTO` buttons, its rate in frames, and an ON AIR lamp with the ramp
  in progress.
- A keyer panel per keyer: fill and key pickers (only sources the user can read), type with
  clip/gain/invert, mask, fly x/y/w/h with corner presets (TL, TR, BL, BR, centre, full), and the
  loss policy. Edits go through `keyer_set` with the plan shown first ("+0.30 canvas").
- Wall tiles show `● PGM KEY` and `▲ PVW KEY` for key sources, from the engine tally.
- Every disabled control has its reason as title and `aria-describedby`.

## 11. Tests, chaos and acceptance

Per slice; each item names its slice.

1. **Kernels (K1):** the CPU oracle first. Golden frames for luma (clip/gain/invert), linear
   (alpha plane, key source), mask, fly and weight. A property test that clip/gain is monotonic.
   An order test that alpha is premultiplied once, after primaries. GPU vs CPU: SSIM ≥ 0.98 and
   PSNR ≥ 40 dB on the existing gate. A bench on the RTX 4060 for 0, 1, 2 full-canvas and 2
   flying keyers, recorded in this note.
2. **Frame coordination (K2, K3):** with `ManualTimeSource`, a DSK CUT lands on the commanded
   tick. A DSK AUTO of N frames shows weights k/N at ticks k. A tied DSK and an armed USK track
   the transition weights exactly for MIX, DIP and WIPE. An AUTO abort restores keyer state.
3. **Fresh gate and loss (K2):** a keyer with a never-publishing fill is refused at 50 ticks with
   0 lost ticks. Killing the fill mid-ramp drops the keyer at the trigger, emits the event, and
   it stays off. A slate is never keyed.
4. **Invariant #1 / #10 chaos (K2):** a flood of 10 000 keyer commands from 8 tasks gives 0 lost
   ticks, the mailbox stays bounded, and 503s are counted. A wedged Clean encoder leaves Program
   with 0 repeats. A per-frame GPU failure with 2 keyers on air falls back to the CPU for that
   frame only. `scratch_reservations` stays constant over 10 000 ticks of ramping keyers (no
   per-tick allocation).
5. **Clean (K2):** with a DSK on and Clean selected, Clean has no DSK pixels and Program does.
   With no Clean subscriber, the second call never runs (counter 0).
6. **API (K4):** BOLA on every id, redaction in GET and events, 409 `capacity`, 409
   `on-air-protected`, idempotent replay, persistence after `applied`, and a restart that comes
   up with keyers off.
7. **Desk (K5):** component tests for every state and disabled reason, and the e2e switcher spec
   extended.
8. **mda001 acceptance (after K5; run by the lead):** the Program protection bound
   (output-routing.md §10) over 10 minutes with DSK1 (graphics lower third, alpha) on air, DSK2
   AUTO on/off every 10 s, USK1 a flying luma key (30 % box) on air, and a WIPE every 5 s,
   against the same 10 minutes with no keyers. That means lateness p50 +1 ms, p99 +2 ms,
   max +5 ms (< 16.7 ms), `frames_repeated` 0, and an HLS PTS step of 3003. `ffprobe` tags are
   unchanged.

## 12. Slices

Each slice is its own PR to the full bar. K1–K3 are R3 (data plane). K4 is R3 (API on the
data plane). K5 is R2.

| Slice | Scope | Depends on |
| --- | --- | --- |
| K1 (#386) | Compositor: key block in `Tile`/`TileParams` (luma, linear, key source, mask, weight), CPU oracle then GPU, bench | — |
| K2 (#387) | Engine: typed keyer config, the keyer runtime in the tick driver, DSK CUT/AUTO/TIE on MIX and CUT, fresh gate, loss policy, pre-DSK Clean, engine tally, chaos tests | K1 |
| K3 (#388) | USKs: arm set, keys riding MIX/CUT, then DIP/WIPE weights and masks from lane `transitions`, key-only transitions | K2, lane `transitions` WIPE |
| K4 (#389) | Control API: verbs, plan/cost, admission, BOLA and redaction, persistence after `applied`, OpenAPI/AsyncAPI | K2 (K3 for `arm`) |
| K5 (#390) | Desk: keyer strip, DSK controls, keyer panel, key tally on the wall | K4 |
| K6 (#391) | Alpha media: media-player decode of ProRes 4444, qtrle, PNG/TGA and VP9 alpha into NV12+A; NDI alpha; the SW-11 import gate | K2 |
| K7 (#392) | Chroma key (ADR-C007 domain) | K1 |

The backlog rows SW-9 (#35) and SW-10 (#36) are superseded by K1 and K2–K3. They predate the
as-built drive and assume a render-plan resolver that was not built.

## 13. Out of scope

FTB, multi-M/E, aux buses, compositions as keyer fills, shaped (premultiplied) fills, keyer
borders, shadows and bilinear or sub-pixel scaling (SW-25 #51), keyer audio, and a T-bar for
keyers.
