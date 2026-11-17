# Design note: upstream and downstream keyers on the M/E (ADR-0113)

Decision record: [ADR-0113](../decisions/ADR-0113-keyers-on-the-program-drive.md). Model
background: [ADR-0056](../decisions/ADR-0056.md), [production-switcher.md §6](../research/production-switcher.md#6-keyers).
This note is the build plan on main as of `1b59d254`. §14 records what each slice built and where it
deviates.

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
fill = "cam-3"                         # a source id (mixer inputs "in:<n>" are refused)
key = "cam-3-matte"                    # optional; absent = self key
type = "luma"                          # luma | linear | (chroma, slice K7)
clip = 0.10                            # luma: [0,1] on post-range-expand Y
gain = 4.0                             # luma: >= 0
invert = false
mask = { x = 0, y = 0, w = 1920, h = 1080 }   # fill-source pixels; optional
fly = { x = 1280, y = 60, w = 576, h = 324 }  # canvas pixels, even, >= 16; optional = full canvas
max_source = { w = 1920, h = 1080 }    # admitted source envelope; default = canvas size
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
               pending: Option<(command, first_tick)>, last_good: (Option<Arc<Nv12Image>>, Option<Arc<Nv12Image>>),
               fill_idx: u16, key_idx: Option<u16> /* store indices, resolved at definition */ }
weight(tick) = ramp ? f(tick.index - start) : (on_air ? 1 : 0)
```

On-air, armed and ramp state are not persisted. A restart comes up with every keyer off, except
DSKs with `on_at_start`.

## 3. Render: the tile list per tick

Bottom to top. Every entry is one `Tile`, and each is drawn only if it contributes:

1. Program cell (outgoing background), as today.
2. Transition tiles of lane `transitions`: the AUTO incoming tile, the DIP colour tile and
   incoming tile, the WIPE incoming tile with its mask.
3. USKs in ascending `z`.
4. DSKs in ascending `z`.
5. FTB cover (reserved; not part of this work).

### 3.1 Coverage per tile (each alpha used once)

```text
a = opacity × weight × mask(sx, sy) × fill_alpha(sx, sy) × matte(kx, ky)
fill_alpha = fill NV12+A plane, else 1                    (already applied by both kernels)
matte      = linear, no key source : 1
             linear, key source    : clamp(Y_key_norm, 0, 1)
             luma                  : invert?(clamp((Y_norm − clip) × gain, 0, 1)),
                                     Y of the key source, else of the fill
```

Y is post-range-expand code-value Y (step 1 of the pipeline), so steps 2–5 are unchanged and the
alpha is premultiplied once (#8). Both kernels sample fill and key independently from the same
integer destination offset `(ddx, ddy)`:

```text
sx = floor(ddx × fill_w / dst_w)     kx = floor(ddx × key_w / dst_w)
sy = floor(ddy × fill_h / dst_h)     ky = floor(ddy × key_h / dst_h)
```

A key of a different size therefore never inherits the fill's rounding. Example: fill width 2,
key width 4, destination 4 samples key columns 0, 1, 2, 3.

### 3.2 Exact scene mixing (groups)

A background transition mixes the outgoing scene (A + its on-air USKs) with the incoming scene
(B + the USKs on after it). A flat `over` of a changing key at the transition weight is wrong: an
opaque incoming key K at p = 0.5 gives `0.5·K + 0.25·A + 0.25·B`, where the correct result is
`0.5·K + 0.5·A`. So each tile carries a group tag:

| Tag | Layers | Folded into |
| --- | --- | --- |
| `both` | a layer in both scenes at or below the highest changing layer (shared background of a key-only transition, an unarmed on-air USK below an armed one) | both accumulators |
| `out` | outgoing background, USKs going off | the outgoing accumulator |
| `in` | incoming background, USKs going on | the incoming accumulator |
| `top` | layers above the highest changing layer: unarmed on-air USKs above it, every DSK | once, onto the merged result |

Merge, per pixel, after the last grouped layer: MIX `lerp(out, in, p)`; WIPE
`lerp(out, in, m(x, y))` with the wipe mask of lane `transitions`; DIP
`lerp(lerp(out, colour, d), in, i)` with `dip_weights`. All of this is in premultiplied linear
light, so it is exact. Then the `top` layers fold with `over`, exactly as before (`over` is
linear in what lies beneath). A tied DSK is a `top` layer with a direction-aware envelope: going **on**,
MIX `p`, WIPE `m(x, y)` (canvas coordinates) and DIP `i` (0 until the switch point); going
**off**, MIX `1 − p`, WIPE `1 − m(x, y)` and DIP `1 − d`. Its state toggles on the terminal tick,
where the envelope already equals its end value.

When nothing but the background changes and B is opaque, the merge equals today's "incoming
tile at weight p". Lane `transitions` and the steady state therefore keep the single-accumulator
path, and the second accumulator exists only while a key-changing transition runs. CPU: a second
pooled `BandScratch` accumulator, sized at attach. GPU: a second `vec4` register per invocation.

### 3.3 GPU parameters

The key's Y plane is uploaded into the existing `y_planes` array at its own layer. A new storage
buffer `key_params` (binding 6, one entry per tile, parallel to `tiles`, pooled grow-only like
the tile buffer) carries:

```text
KeyParams { key_layer, key_w, key_h, kind|invert|has_table|group (u32 bits),
            table_base: u32,
            mask: vec4<u32>, merge: vec4<f32> /* kind, p | d, i */, dip_colour }
```

A matte from a signal (luma, or linear with a key source) is a pure function of the signal's
8-bit Y code, its resolved range, clip, gain and invert. So the CPU builds it per keyed tile as
a 256-entry `f32` table with the oracle's own functions (`range::expand_luma`, then
`key::luma_matte` or `key::unit`, then invert), using the key source's range, or the fill's for
a luma self key. The tables live in a second storage buffer `mattes` (binding 7, pooled and
refilled in place like `key_params`); the shader rounds the texel to its integer code and looks
it up at `table_base + code`. This is exact by construction: a huge or infinite gain makes the
matte a step at the clip, and a shader evaluation of `(y − clip) × gain` could flip whole
pixels on float fuzz (review MEDIA-1).

Nothing is borrowed from the fill's `TileParams`, so fill and key can differ in size and range.
Layer count: at most 1 Program cell + 3 transition tiles + 6 keyers × 2 (fill, key) = 16 ≤ 64.

## 4. Behaviour

| Event | USK (armed) | USK (not armed, on air) | DSK tied | DSK not tied |
| --- | --- | --- | --- | --- |
| CUT | flips on the CUT tick | stays | flips on the CUT tick | unchanged |
| AUTO MIX / DIP / WIPE | group `in` (going on) or `out` (going off), merged exactly (§3.2) | `both` or `top` | `top`, direction-aware envelope (§3.2) | unchanged |
| Key-only transition (background not armed) | selected type and rate; DIP draws as MIX | `both` or `top` | — | — |
| `keyer_cut` | — (USKs change via `arm` + CUT or AUTO) | — | flips at the next tick | flips at the next tick |
| `keyer_auto` | — | — | ramps over its rate | ramps over its rate |

USKs: `arm {background, keys}` sets the next-transition set (`SwitcherState::arm` validates
duplicates and range). When a transition completes, the armed keys' `on_air` toggles and the
background swaps as today. An AUTO aborted on its terminal tick (ADR-0104 §2) restores every
armed key's state.

**Fresh gate.** The tick a keyer would start going on, both its fill and its key must be live.
If not, the command waits in that keyer's own pending slot for at most 50 ticks, then is refused
with `keyer source has no fresh frame before the bounded take deadline`. A second command for
the same keyer while one is held is refused. A background transition that carries keys going on
is gated on all of them and on the incoming background. Going off is never gated.

**Loss (ADR-R011 §3).** Each keyer holds a last-good pair, `(fill Arc, key Arc)`, in two slots
fixed at attach.

- The pair is replaced only on a tick where both reads are live.
- A slate is never stored or keyed.
- A tick where either read is not live draws the stored pair. Fill and key stay frozen together,
  and the keyer contributes nothing if it has no pair yet.
- At `loss_trigger` (default `reconnecting`) on either source:
  - `drop_to_off_air` takes the keyer off at that tick and emits
    `switcher.keyer {id, on_air: false, reason: "source-lost"}`. The keyer stays off until an
    operator puts it on again.
  - `hold_last_good` keeps drawing the pair and reports `stale: true`.
- The pair is released when the keyer goes off.

**Demand leases.** Keyers are a new demand consumer class, `keyer`, published by the hook only
when a keyer definition changes (the `LayoutCell` consumer-set pattern). A defined keyer keeps
its fill and key warm. That is the operator's expectation for a ready lower third, and it is why
admission reserves at definition (§7). Leases are attributed to the keyer object id and listed
in `GET /api/v1/sources/status`.

## 5. Cost per key

All figures are 1920x1080 NV12. Measured figures cite their source. The others are estimates that
K1 measures, and the default budget waits on the mda001 run (§11.8).

| Item | Cost | Source |
| --- | --- | --- |
| Composite of one canvas area, GPU (M3 Ultra, incl. upload + readback) | 9 tiles of 1/9 canvas: 1.60 ms median opaque, 1.77 ms with 1 alpha tile, 2.07 ms with 6 alpha tiles | ADR-0105 (`alpha_composite_bench`) |
| The same, CPU reference | ~7 ms median | ADR-0105 |
| Fold work of a keyer | `dst_area / canvas_area` canvas folds; ×2 for a USK during a key-changing transition (two accumulators) | design |
| Upload of a keyer (GPU) | whole fill: 3.1 MB NV12 (+2.1 MB alpha), plus key Y 2.1 MB, per tick at the source size, **independent of fly size** | `gpu/compositor.rs` uploads whole images |
| Luma / matte math | a few ALU ops per covered pixel | design |
| Merge during a key-changing transition | one lerp per covered pixel, plus a second accumulator | design |
| Idle keyer | 0 compose work; its leases keep its sources decoded (counted in admission) | design |
| Pre-DSK Clean | one aux canvas on `routing-aux` while selected and a DSK is defined | output-routing.md §2.3 |
| Memory | no new image pools; last-good pairs are 2 `Arc`s per keyer; the key params buffer is grow-only | design |

## 6. Clean (pre-DSK), composed off the clock from Program's own images

Clean is Program before the monitoring bake (ADR-0108 §2.5). Once a DSK is defined it needs its
own picture. Three facts rule out the obvious designs:

- A second backend call inside `compose` would delay Program. Compose finishes before Program is
  published (`crates/multiview-engine/src/runtime.rs:1083-1091`), and the GPU readback waits with
  no deadline (`gpu/compositor.rs:1211`, #395).
- Re-reading stores on another thread does not give Program's frames. Stores prune below the
  latest reader and fall back to a newer frame (output-routing.md §2.3). A frozen last-good pair
  or a slate cannot be reproduced at all.
- Program's GPU compositor holds its surface-pool lock through compose and readback
  (`gpu/compositor.rs:311-348`).

So:

- **Hand-off ring.** 4 slots, preallocated at attach. Each slot holds a fixed-size `Copy`
  descriptor of the non-DSK tiles (rects, opacity, weights, key parameters, groups, merge) and a
  fixed `[Option<Arc<Nv12Image>>; 16]` with the **exact images Program resolved** for them. That
  includes held frames, frozen pairs and slates. Each slot has an atomic state,
  `free → writing → ready → reading → free`, moved by compare-and-swap:
  - The clock thread claims a `free` slot, clones the `Arc`s (refcount increments, no
    allocation) and marks it `ready` with its tick index.
  - If no slot is free, it skips Clean for that tick (`clean_skipped`) and never waits.
  - The aux thread takes the oldest `ready` slot, composes, publishes Clean for that tick, and
    drops the `Arc`s as it frees the slot.
  - Image retention is bounded at 64 `Arc`s.
- **The ring is written only while needed:** a DSK is defined and Clean has a subscriber (one
  cached `AtomicBool`).
- **An independent GPU compositor on the aux thread.** It has its own surface pool, scratch and
  readback buffers on the admitted device, so Program's `RunBackend` and its locks are never
  shared. The integer aux path cannot draw alpha (`aux_layout.rs`), so pre-DSK Clean needs the
  GPU backend. Without it, the routing plan refuses Clean with `clean-needs-gpu` while a DSK is
  defined.
- **Under load** the aux rules apply: drop at the capacity-4 channel, hold last-good (counted),
  `routing.degraded` after 60 held ticks, and halved cadence when Program p99 passes half a tick.
- **No DSK defined:** Clean stays the zero-cost Program alias of today.
- **GPU queue sharing** with Program is covered by the existing slice-3 hard gate; the §11.8
  acceptance run includes Clean.

## 7. Admission (invariant #9): reserved at definition

- **Hard limits** at validation: 4 USKs per M/E, 2 DSKs per program, 1 key source per keyer, fly
  and mask in bounds, rate 1..=300, and the 64-layer bound.
- **Cost of a defined keyer** (whether on air or not), in canvas folds:
  `fold × dst_area / canvas_area + u × (fill_px + key_px) / canvas_px`, with `fold` = 2 for a
  USK and 1 for a DSK, and `fill_px`/`key_px` the `max_source` areas. `u` and `fold` are calibrated in K1. The sum is capped by
  `switcher.keyer_budget`.
- **Why reserve at definition:** off keyers keep decoders warm, and no later event (going on
  air, a key-changing transition, a Clean selection, an edit or a boot) may exceed the budget,
  since an on-air key cannot be shed.
- **What is checked:**
  - `keyer_set` checks raised costs against the remaining budget.
  - Boot validation refuses a document over budget.
  - A fill or key that is not running is charged +1 decode by the ADR-0110 decode rule.
  - **Clean's aux canvas.** One reservation is needed exactly while (a DSK is defined) and
    (Clean has a subscriber). Whichever change makes both true reserves it atomically or is
    refused with 409 `capacity`:
    - a routing take to Clean;
    - the first `keyer_set` that defines a DSK;
    - a config apply;
    - boot validation.
    All Clean subscribers share it. It is released when either fact stops being true. Tests
    cover both orders with capacity exhausted.
  - **Source size envelope.** Each keyer declares `max_source = {w, h}` (default: the canvas
    size, within device texture limits). Upload and texture memory are reserved for it. A fill
    or key frame above the envelope is not uploaded: the keyer treats the tick as a non-live
    read (last-good pair, then the loss policy) and counts `keyer_oversize_frames`. Raising
    `max_source` is an admitted `keyer_set`. Pools never grow past the reservation.
  - A refused `keyer_set` releases every reservation and lease it took (make-before-break,
    released on refusal or deadline).
- **Default budget:** until the mda001 run (§11.8) passes, it admits 1 DSK at full canvas only.
  It is raised in the slice that records the measured budget.
- **Visibility:** scoped callers see their own delta and a generic 409 `capacity`. Totals and
  holders go to unscoped readers only (as ADR-0108 decision 8 does).
- **GPU fallback:** a per-frame GPU failure falls back to the CPU only if the reserved cost fits
  the CPU budget. Otherwise that frame is composed without keyer tiles, and
  `keyer_fallback_frames` is counted. Three consecutive such frames take keyers off with
  `reason = "gpu-fallback"`.

## 8. API

All on the existing switcher routes. Each command is a plan and command pair with an
`Idempotency-Key`, a 202 with an `operation_id`, and an outcome on `switcher.operation`.

| Verb | Body | Notes |
| --- | --- | --- |
| `arm` | `{me, program, background: bool, keys: ["usk1"]}` | USK next-transition set. Refused during a flight. |
| `keyer_cut` | `{me, program, keyer, on: bool}` | DSK, or a USK outside a transition |
| `keyer_auto` | `{me, program, keyer, on, duration_frames?}` | DSK only, 1..=300 frames |
| `keyer_tie` | `{me, program, keyer, tie: bool}` | DSK only |
| `keyer_set` | `{me, program, keyer, fill?, key?, type?, clip?, gain?, invert?, mask?, fly?, max_source?, rate?, loss?, on_air?: "allow"}` | Class-1 at one tick; persisted after `applied`; changing the fill or key of an on-air keyer needs `on_air = "allow"`, else 409 `on-air-protected` |

Fill and key are **source ids only**. Mixer inputs (`in:<n>`) are refused (422), so a re-patch
can never change what a keyer shows behind its authorization.

`GET /api/v1/switcher/{me}` adds:

```json
"keyers": [{"id":"dsk1","family":"dsk","fill":"gfx-channel-1","key":null,"type":"linear",
            "fly":null,"mask":null,"on_air":true,"tie":false,
            "ramp":null,"rate":15,"loss":"drop_to_off_air","stale":false,"pending":false}],
"tally": {"program":["cam-1","gfx-channel-1"],"preview":["cam-2"]}
```

`ramp` is `{start_tick, rate, to_on}` while ramping. Clients compute the weight. The snapshot
changes only on state changes, never per ramp tick.

**Authorization (BOLA, per object), on plan and command:**

- `Control` on the M/E, `Program` scope, `Control` on the keyer object id (`me1/usk1`,
  `main/dsk1`), and `Read` on every named source.
- `keyers[]` keeps only the rows whose keyer object id the caller may read (ADR-W025 row
  filtering), then replaces each unreadable source id in those rows with `"redacted"`.
- `tally` drops unreadable ids.
- `switcher.keyer` events are scoped by keyer object id and redacted the same way, in the
  control plane's per-client projection.
- A scoped-principal parity test covers GET against events.

**Abuse:**

- Commands go through the 64-slot `try_send` mailbox, which answers 503 when full.
- The switcher routes have no per-principal rate limit.
- The shared idempotency store (4096 keys, no per-principal namespace, no expiry of completed
  entries) bounds memory but is not fair between principals. That is tracked in #394 and is
  not claimed as fairness here.

## 9. Tally

Each tick the hook derives two sets from the tile list it composed:

- `program`: every source with a contributing tile. That covers background, transition source,
  and the fill and key of every keyer with weight > 0.
- `preview`: the PVW source, plus fills and keys of armed USKs and tied DSKs going on.

The sets are kept as store indices (u16) in two fixed 14-entry arrays. They are published to the
switcher snapshot only when they change. Ids are resolved to strings in the control plane, off
the clock. A key-signal-only source is PROGRAM-tallied. #42 feeds this into the arbiter.

## 10. Desk

- Next to MIX/WIPE/DIP, a keyer strip: `BKGD` and `KEY 1`..`KEY 4` toggles (the arm set, lit when
  armed), each with an ON AIR lamp.
- Per DSK: `TIE`, `CUT` and `AUTO` buttons, its rate in frames, and an ON AIR lamp with the ramp
  progress, computed from `ramp`.
- A keyer panel per keyer: fill and key pickers (sources the user can read; inputs are not
  offered), type with clip/gain/invert, mask, fly x/y/w/h with corner presets (TL, TR, BL, BR,
  centre, full), and the loss policy. Edits go through `keyer_set` with the plan shown first.
- Wall tiles show `● PGM KEY` and `▲ PVW KEY` for key sources, from the engine tally.
- Every disabled control has its reason as title and `aria-describedby`.

## 11. Tests, chaos and acceptance

Per slice; each item names its slice.

1. **Kernels (K1).** The CPU oracle comes first.
   - Golden frames for luma (clip/gain/invert), linear (own alpha used once: a 0.5 alpha gives
     0.5, never 0.25; key source), mask, fly, and weight.
   - Fill and key of unequal sizes, both up and down scaled (the 2/4/4 example in §3.1).
   - Key and fill in mixed full and limited range.
   - A property test that clip/gain is monotonic.
   - An order test that alpha is premultiplied once, after primaries.
   - GPU vs CPU: SSIM ≥ 0.98 and PSNR ≥ 40 dB.
   - A bench on the RTX 4060: 0–6 keyers, full canvas and flying, and large source with a tiny
     fly. It calibrates `u` and `fold`.
2. **Scene merge (K3, CPU and GPU).** For MIX, DIP and WIPE, the composite at frames 0, mid,
   switch point and end equals `merge(scene_out, scene_in)`, computed by composing each scene
   separately. Cases:
   - an opaque key going on, and one going off;
   - a key-only transition;
   - an unarmed on-air USK below and above an armed one;
   - a tied DSK.
3. **Frame coordination (K2, K3).** With `ManualTimeSource`:
   - a DSK CUT lands on the commanded tick;
   - a DSK AUTO of N frames shows weight k/N at tick k;
   - a tied DSK follows the transition exactly;
   - an AUTO abort restores keyer state.
4. **Fresh gate and loss (K2).**
   - A keyer whose fill never publishes is refused at 50 ticks, with 0 lost ticks.
   - Fill-only and key-only death mid-ramp keeps the pair frozen together, then drops the keyer
     at the trigger, emits the event, and the keyer stays off.
   - `hold_last_good` freezes the pair and marks it stale.
   - A slate is never keyed.
5. **Invariant #1 / #10 chaos (K2).** In every case Program ticks stay contiguous and correctly
   stamped.
   - A flood of 10 000 keyer commands from 8 tasks: 0 lost ticks, a bounded mailbox, 503s
     counted.
   - Stalled consumers for snapshots, `switcher.keyer` events and outcomes while sources die and
     commands flood: bounded retention, drops counted, Program unchanged.
   - A wedged and a panicking aux thread while Clean is selected: Clean holds, Program shows 0
     repeats.
   - An injected slow GPU frame (after #395) and failing GPU frames with keyers on air: fallback
     or keyer-less frames as §7 says, and the next tick's lateness is recorded.
   - A counting global allocator over 10 000 ticks of ramping keyers and key-changing
     transitions, compared with the no-keyer baseline: no new allocations per tick.
6. **Clean (K2).**
   - With a DSK on and Clean selected, Clean has no DSK pixels and Program does.
   - Clean at tick T equals Program's non-DSK composite at T, built from the same `Arc`s, under:
     - a late publication and a source reconnect between the Program tick and the aux compose;
     - an aux thread lagging 3 ticks;
     - fill-only and key-only loss with a frozen pair;
     - a slate.
   - A full ring skips Clean and Program never waits.
   - A stalled aux thread that holds its own compositor lock does not delay Program.
   - With no Clean subscriber or no DSK, the ring is never written.
   - Oversize fill and key frames are not uploaded and are counted.
7. **API (K4).**
   - Per-object authorization on every id, on plan and command.
   - Row filtering and redaction: GET against events for a scoped principal.
   - `in:<n>` refused.
   - Admission: reserved at definition; Clean, transition, edit and boot cannot exceed the
     budget; scoped refusal is generic.
   - 409 `on-air-protected`, idempotent replay, persistence after `applied`, and a restart with
     keyers off.
   - A repeated failed `keyer_set` leaves no lease and no reservation.
   - Clean's aux reservation in both activation orders with capacity exhausted, and `max_source`
     raise admission.
8. **mda001 acceptance (after K5; run by the lead).** The Program protection bound
   (output-routing.md §10) over 10 minutes at the admitted maximum budget, against the same 10
   minutes with no keyers:
   - DSK1 (alpha graphic) on air.
   - DSK2 AUTO on and off every 10 s.
   - USK1 a flying luma key armed on and off with a WIPE every 5 s.
   - Clean routed to an output.

   Pass: p50 within +1 ms, p99 within +2 ms, max within +5 ms and below 16.7 ms,
   `frames_repeated` 0, and every HLS packet's PTS step 3003. `ffprobe` colour tags are
   unchanged. The default budget is raised only after this run passes.

## 12. Slices

Each slice is its own PR to the full bar. K1–K3 are R3 (data plane). K4 is R3 (API on the
data plane). K5 is R2.

| Slice | Scope | Depends on |
| --- | --- | --- |
| K1 (#386) | Compositor: per-tile coverage (luma, linear, key source with its own params, mask, weight), independent fill and key sampling, the group tag and two-accumulator merge, CPU oracle then GPU, bench and calibration | — |
| K2 (#387) | Engine: typed DSK config, keyer runtime, DSK CUT/AUTO/TIE (TIE on CUT and MIX), fresh gate, last-good pair and loss policy, `keyer` demand class, engine-side admission reserved at definition (so K2 is safe on its own), pre-DSK Clean on the aux thread (hand-off ring, own GPU compositor), engine tally, chaos tests | K1, #395 for GPU keyers |
| K3 (#388) | USKs and transitions: arm set, groups for key-changing MIX/DIP/WIPE, key-only transitions, tied DSKs on DIP/WIPE | K2, lane `transitions` DIP and WIPE |
| K4 (#389) | Control API: verbs, plan with scoped visibility, BOLA row filtering and redaction, persistence after `applied`, OpenAPI/AsyncAPI | K2 (K3 for `arm`) |
| K5 (#390) | Desk: keyer strip, DSK controls, keyer panel, key tally on the wall | K4 |
| K6 (#391) | Alpha media: media-player decode of ProRes 4444, qtrle, PNG/TGA and VP9 alpha into NV12+A; NDI alpha; the SW-11 import gate | K2 |
| K7 (#392) | Chroma key (ADR-C007 domain) | K1 |

The backlog rows SW-9 (#35) and SW-10 (#36) are superseded by K1 and K2–K3. They predate the
as-built drive and assume a render-plan resolver that was not built.

## 13. Out of scope

FTB, multi-M/E, aux buses, compositions as keyer fills, shaped (premultiplied) fills, keyer
borders, shadows and bilinear or sub-pixel scaling (SW-25 #51), keyer audio, and a T-bar for
keyers.

## 14. As built

### K1 (#386): CPU oracle

- `multiview_compositor::key`: `TileKey` (`KeyKind::{Linear, Luma}`, optional key source, invert,
  `KeyMask` in fill-source pixels), `TileGroup::{Top, Scene {member, merge}}`, `SceneMember::{Both,
  Out, In}` and `SceneMerge::{Mix, Dip}`. `Tile` gains `key` and `group` (`with_key`, `in_group`);
  `Tile::placed`/`scaled` keep `key: None, group: Top`, so every existing caller is byte-for-byte
  unchanged (the oracle-equivalence and scratch-reuse suites pass unmodified).
- Coverage is `opacity × fill_alpha × mask × matte`, premultiplied once at the existing step. The
  key source is sampled with its own nearest-neighbour map, and its Y is range-expanded with its own
  range. A NaN or negative gain reads as 0 and a NaN weight as 0, so a bad parameter never poisons
  a pixel.
- Groups: a run of grouped tiles folds into two pooled per-band accumulators (`BandScratch`
  `scene_out`/`scene_in`, grow-only, counted in `scratch_alloc_count`) and merges at the next `Top`
  tile or the end of the list. While any tile is grouped the band's accumulator spans the whole
  band, because a DIP merge colours pixels no tile covers. Frames with no grouped tile keep the
  covered-span path unchanged.
- **Deviation: WIPE merge is not in K1.** `SceneMerge::Wipe` lands with K3 on top of lane
  `transitions`' mask (**resolved in K3**, on the CPU oracle and the GPU; see "K3 as built"). The GPU key path was first held back until #395 (the bounded GPU
  readback, PR #424). Until it landed, `GpuCompositor` refused a keyed or grouped frame with a
  typed `GpuLimit`, and `RunBackend` composed it on the persistent CPU fallback. That
  deviation is closed; see "K1 GPU key path as built" below. A genuine GPU failure, and a
  keyed frame the GPU cannot hold, still fall back through the same **persistent** CPU
  reference owned by the GPU backend (`cpu_fallback_backend`). Its LUT cache and band/scene
  scratch survive across ticks, its frames are counted in `gpu_cpu_fallback_frames`, its pool
  is what `scratch_alloc_count` reports on a GPU backend, and it never draws a keyer without
  its key. The LUT cache checks a frame's transfer set in place, so no per-frame list is
  built unless the set changed. The parallel CPU path still spawns its scoped band threads
  each frame, as it did before K1. That is a fixed, bounded cost, tracked in #416
  (persistent band workers). ADR-0113 §9 bounds this: the default budget is 1 full-canvas
  DSK until the mda001 run passes.
- Tests: `tests/keyers_cpu.rs` (20: goldens for linear self key with alpha used once, linear key
  source, luma clip/gain/invert, luma on a key source, mask with a flying key, unequal fill/key
  sizes up and down, mixed ranges, premultiply-once after primaries, weight; a monotonic and
  bounded clip/gain property; exact MIX and DIP scene merges against an independent per-pixel
  computation written out in the test (never the production helpers), both DIP legs; a NaN weight
  reads as 0; the tile-driven kernel equal to the oracle with keys and groups at 1-4 threads, on a
  small canvas (proptest) and on a 320x256 canvas above the parallel threshold with tiles across
  band edges; no scratch growth over 50 grouped composites) and `tests/keyers_gpu_fallback.rs`
  (`wgpu`: the layer-limit refusal, the oracle-equal fallback, and no scratch growth or LUT
  rebuild over 20 repeated keyed and grouped fallback frames).
- Bench: `examples/keyer_composite_bench.rs`. CPU figures from a shared 56-core devcontainer (not
  the target host; median ms at 1080p): 0 keyers 30.0, 1 full-canvas alpha DSK 41.1, 2 full-canvas
  keyers 54.5, 2 flying 576x324 31.2, 1080p source in a 128x72 fly 27.6, key-changing MIX + DSK
  58.9. The calibration of `u` and `fold` on the RTX 4060 is owed (lead), on the GPU key path below.

### K1 GPU key path as built (#386 part 2)

- **Where.** `composite.wgsl` draws keyed tiles and scene groups itself. `GpuComposer::draws_keys`
  is `true` for the wgpu compositor, so `OffClockGpu` no longer routes a keyed frame to the CPU
  before GPU work. The simulated chaos GPU keeps the CPU route, so the engine's slow-fallback
  epoch tests are unchanged.
- **Parameters (§3.3).** `KeyParams` (80 bytes) is binding 6, one entry per tile, parallel to
  `tiles`. It holds:
  - `head`: key layer, key w and h, and the flags (kind, invert, has-source, has-mask,
    has-table, and the 2-bit group).
  - `table`: the base of the tile's matte table in `mattes`.
  - `mask`: a `vec4<u32>` in fill-source pixels.
  - `merge`: kind, p or d, and i, plus the DIP colour. K3 adds kind ≥ 2, a WIPE merge:
    `2 + shape + 8·reverse + 16·complement`, then softness, extent and radius, with the origin
    and expand in the DIP-colour lanes (no struct growth, still 80 bytes).

  `mattes` is binding 7: one 256-entry `f32` table per signal-keyed tile (see §3.3). Both
  buffers are allocated once at `MAX_TILES` entries (tables) and refilled in place, like the
  tile buffer. The CPU-side `TileParams`/`KeyParams`/matte scratch lives in `SurfacePool` and is
  cleared and refilled, so a steady-state tick allocates no `Vec`.
- **Key sources.** Each key source uploads only its Y plane, into its own `y_planes` layer
  after the fill layers. It is sampled with its own nearest map, `kx = floor(ddx × key_w / dst_w)`,
  and its matte table is built with its own range. The matte reads the Y code only (the input
  of range expand), never the colour math. `mask × matte`
  multiplies into alpha before the single premultiply, so the #8 order is unchanged and nothing
  is materialised as RGBA (#5).
- **Matte math is the oracle's.**
  - A linear self key is 1 and ignores invert.
  - The mask test is half-open and compares offsets, so it cannot overflow.
  - Every signal matte is the oracle's own value for the Y code: the shader computes
    `round(texel × 255)` clamped to [0, 255] and reads the CPU-built table, so degenerate
    parameters (NaN, negative, zero or infinite gain; NaN, infinite or subnormal clip) and the
    step of a huge gain cannot diverge from the CPU, and the shader never clamps a NaN.
  - A NaN tile opacity reads as 0, as on the CPU.
- **Scene groups.** Two extra `vec4` registers (outgoing and incoming) hold the open segment.
  It merges (MIX, DIP, and since K3 WIPE `m(px, py)`) at the next `Top` tile or the end of the list, and the last grouped
  tile's merge wins. A grouped tile that covers nothing still opens its segment and sets its
  merge, matching the CPU oracle.
- **Wipe masks (K3).** Tile and merge masks share one `mask_coverage` (mirrors
  `WipeMask::coverage`), which honours the complement (`1 − m`): a tile mask carries it in the
  spare `TileParams::prim2.w` lane (still 160 bytes), a merge in its kind code. Endpoints and a
  mask of another canvas are encoded exactly on the CPU side (a pinned coverage, or a MIX at 0),
  as the oracle reads them. Nothing K3 builds is refused.
- **Still refused (typed `GpuLimit`, then the CPU fallback).**
  - Tiles plus key sources past the 64-layer tile array. §3.3's worst case is 16 layers.
  - A fill or key larger than the device's `max_texture_dimension_2d`.
- **Tests.**
  - `tests/keyers_gpu.rs` (`wgpu`, 6 tests since K3) compares GPU and CPU oracle output (SSIM ≥ 0.98,
    PSNR ≥ 40 dB on Y and UV) across 18 frames, plus the MEDIA-1 frames below. The cases cover:
    - a luma self key, and a luma key with a key source of another size and range (up- and
      downscaled tile);
    - a linear key with a key source, and a linear self key with fill alpha;
    - luma and linear invert, and a mask on a flying key;
    - two keyers at non-unit scale, and degenerate parameters;
    - MIX at p 0, 0.3, 0.5 and 1, and DIP at three (d, i);
    - two segments with an off-canvas grouped tile whose merge wins;
    - review MEDIA-1: a full-range black key source, clip −1e-40, gain +∞ (matte 1, and
      inverted 0) under a white fill, and a limited-range key whose codes sit one below, on and
      one above the clip with gain 1e30 and +∞. These also bound every pixel to |ΔY| ≤ 2, so a
      flipped step pixel fails.

    The suite also checks that a keyed or grouped frame on the GPU backend leaves
    `gpu_cpu_fallback_frames` at 0, that there is no GPU pool growth over 30 keyed and grouped
    composites, and that the layer-limit refusal falls back to the oracle.
  - On lavapipe (llvmpipe, Vulkan) the minima were SSIM 0.99998 and PSNR 60.9 dB, with a
    maximum |ΔY| of 2 codes.
  - K3 (`k3_wipe_merges_and_complemented_masks_match_the_cpu_oracle`): WIPE merges and tiles
    through a mask, each plain and complemented, for all five shapes, reverse, hard and soft
    edges, both endpoints, a border-expanded mask and a foreign-canvas mask (88 frames); the
    fixtures must differ from a MIX / the uncomplemented tile, and neither falls back on the GPU
    backend. On lavapipe the minima were SSIM 0.99999 and PSNR 61.05 dB, |ΔY| ≤ 2.
  - GPU-free `KeyParams` packing unit tests run in `cargo test` without an adapter, including
    a check that the matte table equals `key::luma_matte` for all 256 codes over NaN, negative
    and infinite gains and NaN, ±∞ and subnormal clips, in both ranges.
- **Bench.** `keyer_composite_bench` times the GPU key path. It prints the adapter and a
  per-case fallback delta, which must be 0. RTX 4060 figures and the calibration of `u` and
  `fold` are still owed by the lead; no numbers are recorded here. lavapipe timings measure
  the shader on the CPU and are not a calibration.

### K2a (#387): DSK runtime in the Program drive

- **Config.** `[[switcher.downstream_keyers]]` and `switcher.keyer_budget` are typed fields of
  `SwitcherConfig` (named fields win over `flatten`, so they leave `extra`; other future keys still
  round-trip there). `DownstreamKeyerConfig` is `deny_unknown_fields`. `type = "chroma"` and
  `premultiplied = true` parse only so validation can refuse them by name. `validate_keyers` runs
  from the document validation, where the canvas is known. It checks: at most 2 DSKs; unique ids
  and z; a declared program; fill and key are declared sources, `in:` refused; rate in 1..=300;
  clip in [0, 1]; gain finite and >= 0; fly even, >= 16 and inside the canvas; `max_source` even
  and <= 8192; mask inside `max_source`; summed cost within the budget. A DSK also needs a
  `[[switcher.mix_effects]]` on its program (`dsk-requires-mix-effects`, review F3): keyers run
  on the M/E's Program drive, and without one boot would install nothing while `on_at_start`,
  the demand leases and the Clean refusal still applied.
- **Cost and budget.** `keyer_cost` is `fold·dst/canvas + u·(fill + key)/canvas` on the
  `max_source` areas, shared by config and engine (`KeyerDef::cost`). `KEYER_UPLOAD_WEIGHT` (`u`) is
  1.0. That value is an uncalibrated placeholder until the RTX 4060 figure lands with the GPU key
  path. `DEFAULT_KEYER_BUDGET` is 3.0: one full-canvas DSK with a canvas-size fill **and** key. It
  admits exactly one DSK of any key type and refuses a second full-canvas one.
- **Runtime (`multiview_engine::keyer`).** `KeyerRuntime` lives in `SwitcherTickDriver` and is
  installed before the clock starts with `set_keyers(keyer::admit(defs, w, h, budget)?)`.
  - **Fixed state.** `[Option<KeyerSlot>; 2]`. Each slot holds `Copy` `KeyerFlags` (on air, tie,
    ramp, pending, stale), a last-good `(fill, key)` pair, and pending and ramp op ids. The op ids
    are moved in from the command and never cloned per tick.
  - **Events.** Up to 8 per tick, queued in a fixed array; overflow is counted.
  - **Tally.** Two 14-entry role sets.
  - **Stores.** `KeyerDef` holds the fill and key `Arc<TileStore>`s resolved at attach, so a tick
    never looks a source up by id.
- **Verbs.** `SwitcherVerb::{KeyerCut, KeyerAuto, KeyerTie}` take a keyer index and are
  engine-internal (K4 maps ids to indices and adds REST).
  - They use the existing 64-slot `try_send` mailbox. The drain dispatches them **before** the
    scene-version, held-take and take branches, so a held background take never blocks a DSK.
  - The ramp payload is a `FrameCount`.
  - **Typed refusals:**
    - `REFUSE_KEYER_NO_FRESH_FRAME` (at exactly tick + 50);
    - `keyer-pending` (a second command while one is held);
    - `unknown-keyer`;
    - `keyer-ramping` (AUTO during AUTO);
    - an AUTO interrupted by a keyer CUT;
    - `source-lost`;
    - `keyer-rate` (`REFUSE_KEYER_RATE`, review F5): a `KeyerAuto` rate override above
      `MAX_KEYER_RATE` (300, the config bound), refused before any state change. A
      `FrameCount` is already >= 1.
  - An AUTO toward the current state is an applied no-op.
- **Gate, pair, loss.**
  - **Going on.** Fill and key must be fresh (`read_at(pts)` Fresh) and inside `max_source`.
    Going off is never gated.
  - **Reads.** Each keyer that is on, ramping or taking part in a tied background AUTO (weight
    0 included) does one `read_at` per source
    per tick. State and picture come from that same read. A keyer that is off reads nothing.
  - **Pair.** A tick where both reads are live replaces the pair. Otherwise the stored pair is
    drawn, frozen together. With no pair the keyer draws nothing, and a slate is never keyed (a
    `NoSignal` read has no frame).
  - **Oversize.** A fresh frame larger than the envelope is a non-live read and counts
    `keyer_oversize_frames`.
  - **Loss.** A local `rank()` orders states, because `SourceState` is `non_exhaustive` and has no
    `Ord`. At the trigger on either source:
    - `drop_to_off_air` clears on-air, the ramp and the pair, refuses the ramp's op and emits
      `SourceLost`. The keyer stays off.
    - `hold_last_good` sets `stale` once, and recovery clears it (`SourceRecovered`).
- **Tie.**
  - **Participation (review F1).** `KeyerFlags::flight: Option<bool>` records a tied keyer's
    part in the background AUTO and its direction (`true` going on), fixed on the tick the AUTO
    starts (`begin_tied_transition`) and never re-derived from `on_air`. Only participants
    draw the envelope and flip on the terminal tick (`complete_tied`); a keyer tied during the
    flight waits for the next take. A participant is engaged for the whole flight, so its loss
    policy applies on every tick, including a DIP's weight-0 ticks before the switch point.
  - **Loss cancels participation.** `drop_to_off_air` during the flight clears `flight` with
    `on_air`: the keyer stays off through the recovery and the terminal tick, until an
    operator puts it on (the design is silent here; this is the conservative choice, off stays
    off). An operator keyer CUT, AUTO (the no-op included) or a held command that lands also
    takes the keyer out of the flight, and an untie mid-flight leaves it in its current state.
  - **CUT.** A tied keyer flips on the CUT tick (`stage_take`).
  - **MIX.** It draws `p` going on and `1 − p` going off. `p` is the same `MixOverlay::opacity`
    the dissolve tile uses. It flips on the terminal tick (`projection.completed` after `sample`).
  - **DIP (ADR-0112).** The same flip, with the DIP envelope: `i` going on (0 until the switch
    point) and `1 − d` going off, from the same `kernel::dip_weights` the transition tiles use.
    `MixOverlay::tie_envelope()` returns a `Copy` `TieEnvelope { going_on, going_off }` for the
    overlay's `TransitionStyle`, so the hook passes one value whatever the transition is.
  - **Deviation: WIPE (ADR-0112, rendered since the merge of main into K1).** §3.2 asks for the
    per-pixel envelope `m(x, y)` going on and `1 − m(x, y)` going off. A keyer tile carries a
    single `WipeMask` slot and the compositor has no complement mask, so in K2a a tied keyer on a
    WIPE behaves like a CUT at the transition end: `tie_envelope()` returns `{going_on: 0,
    going_off: 1}`, the keyer keeps its state through the flight and flips on the terminal tick
    (`keyers_dsk::a_tied_dsk_on_a_wipe_cuts_at_the_terminal_tick`). The exact WIPE envelope is K3
    (#388). **Resolved in K3:** the per-pixel envelope replaced this deviation, and the test
    became `a_tied_dsk_on_a_wipe_follows_the_mask_and_flips_on_the_terminal_tick` (see "K3 as
    built").
  - **Gate.** A CUT or AUTO is also gated on the fill and key of every tied keyer it would put on
    (`take_fresh`). For a new take that is every tied keyer that is off and not in its own AUTO.
    A CUT during a background AUTO completes that flight, so it waits only for the surviving
    going-on participants (`flight == Some(true)`, review round 2 G2): a keyer whose
    participation a loss cancelled is never waited for, the CUT completes at once, and a
    recovery after the terminal tick neither swaps the background again nor keys the DSK.
  - **Preview tally.** During the flight the same rule names the keyers coming on: a loss
    that cancels participation removes the keyer's fill and key from `preview` on that tick.
  - **Abort.** An AUTO aborted by SW2-02, interrupted by a punch or rolled back by a rejected
    frame cancels participation (`cancel_tied`), and tied keyers keep their
    state.
  - **Boot.** `on_at_start` is seeded as an op-less pending going-on. It lands at the first fresh
    tick with an `OnAtStart` event and no outcome. An operator command supersedes it.
- **Reject atomicity (review F2).** A rejected tick leaves the composed output and every
  keyer's last-good pair exactly as committed. `begin_tick` stages the flags, the last-good
  pairs (refcount increments only) and the tally; `reject` restores all three, so a staged off
  that released the pair gets it back. The hook never resolves keyers on a rejected frame and
  no longer clears the drive's draws: the last committed draws stay installed (an on-air DSK
  stays in the frame, with stale sources too). `on_compose_failed` swaps the committed draws
  back from `keyer_buf`. In both paths `settle_keyer_draws` rebuilds the installed draws as the
  committed resting state, in ascending z, in the same fixed array (review round 2 G1): a
  keyer still in its own AUTO keeps its committed draw; every other keyer draws its restored
  last-good pair at weight 1 on air and nothing off, whether or not the committed tick drew
  it. A tied keyer going off through a DIP draws weight 0 past the switch point, so its last
  committed draw is absent while it is still on air; the reject cancels the flight and the
  rejected frame shows it at rest. Pictures are the restored pair (`Arc` clones, no heap).
  The settle bumps the off-clock scene epoch (#415) only when the settled draws differ from
  the installed ones (keyer, rect, weight, key parameters or key presence; floats by bits), so
  a deadline miss that changes nothing keeps the epoch and a CPU fallback slower than the
  clock is still adopted. An unconditional bump froze Program (PR #443 review INV-MC-1).
  `reject_composed_tick` also refuses slot-held ops that moved this tick, and discards
  queued events. Applied keyer ops sit in `staged_success` and are refused with everything else.
  `Staged` is untouched. `repeat_tick` expires held keyer commands past their deadline.
- **Compose.** The hook resolves keyers only on a frame that will commit, after the mix block. It
  calls `driver.resolve_keyers(tick, envelope, incoming_drawn, &mut keyer_buf)` and then `drive.set_keyer_draws`, a
  `mem::swap` of a fixed `[Option<KeyerDraw>; 2]`. `KeyerDraw` is two `Arc`s plus `Copy` parameters.
  - `compose` reserves a constant `KEYER_TILES = 4` in `begin`, on top of the two transition tiles
    (DIP colour and incoming, or WIPE incoming and border band), so `scratch_reservations` does not depend on keyer state.
  - **Fix: `ComposeScratch::begin`.** It reserved `cell_count - capacity()` on a cleared vector.
    `Vec::reserve` is relative to `len()` (0 after `clear`), so the request was too small whenever
    the target exceeded twice the old capacity, and the odometer then counted a growth on every
    tick. It now reserves `cell_count`. A frame with keyers reaches this, because it adds 4 tiles to
    a one-cell layout.
  - `push_keyer_tiles` appends the fill (and key) to the pooled `held` vector, with one `Placement`
    per keyer: fly rect, opacity = weight, and a `PlacedKey`. They go in ascending z, above the
    transition tiles.
  - `tile_of(p, held)` builds every `Tile`, adding `with_key(TileKey { kind, source, invert, mask })`
    for keyer tiles. `CompositedFrame` is unchanged.
- **Snapshot and events.**
  - `SwitcherSnapshot` gains `keyers: [Option<KeyerSnapshot>; 2]` and `tally: KeyerTally`. Both
    are `Copy` and `Eq`, and they are part of the change test. A ramp is published as
    `{start_tick, rate, to_on}`, so the snapshot changes on state changes, never per ramp tick.
  - `SwitcherControl::keyer_events()` is a drop-oldest `EventStream<KeyerEvent>` (capacity 256),
    published only in `commit_composed_tick`.
  - Op outcomes reuse `SwitcherOutcome`, and therefore the `switcher.operation` relay. The
    `switcher.keyer` wire event and its redaction are K4.
- **Demand.** `ConsumerClass::Keyer` has the wire token `"keyer"`. The pipeline seeds one
  `consumer_set(Keyer, Rails::VIDEO)` with a `SetItem` per fill and key, labelled `main/dsk1`. It
  keeps the handle (`Pipeline::keyer_demand`) so K4 can `publish` on a definition change.
- **Run-future size (review F6).** `KeyerRuntime` in `SwitcherTickDriver` and the
  `[Option<KeyerDraw>; 2]` buffers in `CompositorDrive` and `SwitcherFrameHook` are boxed once
  at construction; nothing is boxed per tick. The `discrete_audio_tracks` run futures went from
  18 352 B (over clippy's 16 KiB `large_futures` threshold) to 13 536 B. Boxing the `Pipeline`
  keyer fields saved only 32 B and was not kept.
- **CLI boot.** `attach_first_slice` resolves `KeyerDef`s from `pipeline.preview_stores()`; a
  missing store is a boot error. It then calls `keyer::admit` against the document's budget and
  `driver.set_keyers`.
- **Deviation: Clean is refused while a DSK is defined (until K2b, #435).**
  - **Boot.** Validation refuses an output whose feed is Clean when a DSK is defined
    (`clean-unavailable-with-dsk`).
  - **Takes.** The output router's handle carries `dsk_defined`. The plan answers
    `PlanRefusal::CleanUnavailableWithDsk`: a 409 `clean-unavailable-with-dsk` on take, and a plan
    and patch refusal of the same kind. `try_take` refuses as `unroutable` if a caller skips the plan.
  - **Why.** Clean can never show DSK pixels, and the no-DSK Clean alias is unchanged. The hand-off
    ring, the aux GPU compositor and `clean-needs-gpu` are K2b.
- **Deviation: tally as roles.** The tally is `TallyRole::{Program, Preview, Fill(k), Key(k)}`
  rather than store indices: PGM and PVW have no stable store index in the drive. The control
  plane resolves roles to source ids off the clock (K4).
  - `program` holds the background, the PVW role only on a frame where the transition's
    incoming tile contributes (`CompositorDrive::transition_incoming_drawn`: incoming weight
    > 0 and a live frame, the test `push_transition_tiles` applies; never at MIX `p = 0` or a
    DIP's colour-only ticks, review F4), and the fill and key of
    every keyer tile drawn with weight > 0.
  - `preview` holds PVW plus the fill and key of tied keyers going on.
- **Deviation: GPU before #395.** A keyed tile makes `GpuCompositor` return `GpuLimit`, and
  `RunBackend` composes that frame on the CPU (`gpu_cpu_fallback_frames`). It is never drawn without
  its key. The ADR §7 rule "CPU fallback only if the reserved cost fits the CPU budget, else
  keyer-less frames and `gpu-fallback` after 3" is **not** built in K2a. It needs a per-frame
  fallback signal from `RunBackend` into the drive, and that is a follow-up with K2b / after #395.
  The default budget (1 DSK) bounds the CPU cost meanwhile.
- **Tests.**
  - `multiview-config` unit tests (8 keyer cases) and `tests/switcher_keyers.rs` (a DSK
    without an M/E is refused).
  - `multiview-engine` `keyer` unit tests.
  - `multiview-control` `tests/output_routing_take.rs`: Clean is refused in the plan and as a 409
    `clean-unavailable-with-dsk` on take while a DSK is defined.
  - `tests/keyers_dsk.rs`: CUT, AUTO k/N, tie on CUT, MIX, DIP and WIPE (the CUT-at-end deviation), abort, reject, held background take,
    PVW tally at MIX `p = 0` and before a DIP switch point, the AUTO rate bound (300, 301,
    `u32::MAX`),
    `on_at_start`, unknown index, z order and tally.
  - `tests/keyers_gate_loss.rs`: refusal at exactly +50 with 0 lost ticks, `keyer-pending`,
    tied loss and recovery during a MIX and a DIP in both directions (stays off), a rejected
    take with an on-air DSK, with stale sources and with a staged off, and a failed compose,
    both reject paths after a committed zero-weight tick of a tied DIP going off (pixels on
    the rejection tick, weight, pair identity, z order under a second DSK), a CUT during an
    AUTO after a tied loss (completes at once, one swap, stays off), the Preview tally
    dropping a lost participant,
    fill-only and key-only death mid-ramp (`Arc::ptr_eq` frozen pair, drop at the trigger, stays
    off), `hold_last_good`, slate never keyed, oversize, going off never gated.
  - `tests/keyers_chaos.rs`:
    - 10 000 commands from 8 tokio tasks, with contiguous ticks and PTS, a counted 503 total, one
      outcome per op and bounded work per tick;
    - stalled outcome, event and snapshot readers while sources die. The flood here never sends
      "off", so keyers stay engaged until their sources reach the loss trigger. A reader's
      retention is checked as the drain bound (at most the ring of 256, the rest a counted lag),
      because tokio's `Receiver::len` also counts overwritten messages;
    - `scratch_reservations` and `slate_builds` constant over 10 000 ramping ticks, with tally
      roles equal to the composed keyer tiles on every tick.
- **Concerns.**
  - **No counting allocator.** The workspace forbids `unsafe_code` on every target, and a
    `GlobalAlloc` needs `unsafe`. So the no-per-tick-allocation gate is the pooled odometers plus
    the runtime's fixed arrays. `keyer_events` publishes one `Arc` per state change (the existing
    `EventStream` cost), never per tick.
  - **Upload weight.** `u` is uncalibrated.
  - **Extra reads.** Each keyer on air adds 1–2 lock-free `read_at` per tick.

### K4 (#389): keyer control API

- **Routes.** No new route. `keyer_cut`, `keyer_auto`, `keyer_tie` and `keyer_set` are actions on
  `POST /api/v1/switcher/plan` and `POST /api/v1/switcher/commands/{kind}`. `SwitcherRequest`
  flattens `KeyerFields`: `keyer`, `on`, `tie`, and the `keyer_set` fields `fill`, `key`, `type`,
  `clip`, `gain`, `invert`, `mask`, `fly`, `max_source`, `rate`, `loss`, `loss_trigger`, `z`,
  `on_at_start` and `on_air`. `keyer_auto` takes its optional rate override in the existing
  `duration_frames` (1..=300). The handlers dispatch keyer verbs to `routes::switcher_keyers`
  right after BOLA. The background verbs are unchanged and refuse keyer fields. The plan's
  `200` is documented as `oneOf [SwitcherPlan, KeyerPlan]` (`SwitcherPlanResponseDoc`; review
  round 1 H5), since keyer plans answer a `KeyerPlan`, which has no `scene_version`.
  - Commands use the same `Idempotency-Key` store (a replay answers the first 202), resolved
    right after authorization and the stateless request shape and **before** any
    state-dependent admission (review round 1 H2): a retry of a `keyer_auto` whose ramp has
    started, or of a `keyer_set` that already applied, replays its 202 instead of answering
    409. A fresh request that then fails admission reserves nothing. They also use the same
    202 `SwitcherAccepted {operation_id, kind}` and the same 64-slot `try_send` (503
    `engine-busy`, key and leases released). Outcomes ride `switcher.operation` unchanged.
  - The engine verbs are K2a's `KeyerCut`/`KeyerAuto`/`KeyerTie`. The control plane maps
    `keyer` ids to indices through the committed definitions. K4 adds
    `SwitcherVerb::KeyerSet {keyer, def, allow_on_air}`.
- **BOLA (plan and command, before any existence answer or key).** The checks run in
  `authorize_refs` after the role gate. A keyer verb needs `Write` (Operator) on plan as well as
  on command: a keyer plan rehearses the command (cost, on-air state, the refusal it would
  answer), so a Viewer or ReadOnly key is 403 on `/plan` too (review round 1 H1; background
  verbs keep `Read` for plan). Then:
  `authorize_object(me)`, `Program(program)`, `authorize_object("<program>/<keyer>")` and
  `authorize_object` on every named `fill`/`key`. The control plane has one object axis, so
  `Control` and `Read` on an id are both that grant, and the role separates them (as for
  every other switcher verb). An unknown keyer that the caller may not see is a 403, never a
  404. A readable unknown keyer is 404 `unknown-keyer`. A denied request burns no key.
- **Shape.** `in:<n>` as fill or key is 422 `keyer-source-not-a-source`. Each verb accepts only
  its own fields. `duration_frames` outside 1..=300 is 422 `keyer-rate`.
- **`keyer_set` (class 1, one tick).**
  - **Merge.** The request is merged onto the committed definition. A free slot defines a new
    DSK: `fill` is required, z defaults to max + 1, and the other fields take the config
    defaults.
  - **Validation.** The merged entry is checked with the config's own per-definition rules
    (`DownstreamKeyerConfig::validate_definition`, the same checks boot validation runs: source
    exists, `chroma`/`premultiplied` refused by name, geometry, `max_source`, mask). The
    cross-keyer rules are then checked against the live set: unique z, at most 2 DSKs (409
    `capacity`), and the budget.
  - **Resolve.** The definition is resolved off the clock (`SwitcherControl::resolve_keyer`,
    against the stores and canvas published once at attach) into an `Arc<KeyerDef>`. The tick
    only moves that `Arc` in.
  - **At the tick.** `KeyerRuntime::define` rechecks the summed cost (`REFUSE_KEYER_CAPACITY`),
    a unique id and z, and that a slot keeps its id for life (`keyer-definition`). It also
    rechecks the on-air guard (`REFUSE_KEYER_ON_AIR_PROTECTED`). A fill or key change of a keyer
    on air or ramping (`allow`) needs the new sources fresh now, else it is refused and never
    held. The tick bumps `def_version` and releases a last-good pair that belonged to the old
    sources. It queues a `switcher.keyer` event with `reason = "definition"` and never changes
    on-air state.
  - **Rejected frames.** A rejected frame restores the previous definitions, the reserved cost,
    the draw order and the version (`DefUndo`, `Arc` refcounts only).
  - **Publication.** Definitions are published in a `LatestState<KeyerDefinitions>` at attach
    and then only on a committed `keyer_set` tick, before that tick's keyer events. The clock
    allocates nothing for it (review round 1 H6). The control plane builds the readback off
    the clock with the request (`SwitcherControl::keyer_set_verb`,
    `KeyerDefinitions::with_definition`: the admitted base, the slot replaced, version + 1)
    and the verb carries it as `KeyerReadback`. At the tick `define` checks that readback by
    pointer identity against the result (O(MAX_DSK) compares). The commit then publishes that
    `Arc` (`publish_arc`). A readback whose base changed meanwhile (two sets admitted on the
    same definitions) is refused `keyer-definitions-changed` and nothing changes. Odometer:
    `SwitcherTickDriver::definition_builds` stays at the attach build.
- **Admission (§7).** The plan reports `KeyerCost {delta, fits}` for every caller.
  `reserved`, `budget` and `holders` (keyer object ids) go to unscoped principals only
  (`Principal::is_global`, ADR-0108 decision 8). A scoped caller's 409 `capacity` detail is
  generic. A refused command takes no lease and keeps no key. Raising `max_source` is
  admitted like any other cost increase (tested).
- **On-air guard.** Changing `fill` or `key` of a keyer that is on air, ramping or holding a
  command needs `on_air = "allow"`, else 409 `on-air-protected` (plan refusal and command).
  Parameter changes that keep the sources are not protected. They take effect on the applying
  tick.
- **Make-before-break.** A `keyer_set` takes leases on its fill and key (label
  `keyer:<program>/<keyer>`) before the enqueue. A settle task subscribed before the enqueue
  waits for a terminal engine answer and then decides from the committed definitions, not the
  event, so a lagged ring cannot mislead it.
  - **Engine deadline (review round 1 H4).** The verb carries `deadline` = the clock's last
    composed tick at admission + `KEYER_SET_DEADLINE_TICKS` (= `TAKE_PRIME_DEADLINE_TICKS`,
    50). A `keyer_set` drained at or after that tick is refused `keyer-set-deadline` and never
    applies. Settlement has no wall-clock deadline any more. It ends only on the outcome,
    once `applied_tick >= deadline` (from then on the command cannot apply), or when the
    mailbox is closed (the run ended). Until then it re-reads the composed tick every 500 ms
    (`KEYER_SET_RECHECK`). It therefore never releases leases or the DSK reservation while
    the command could still apply. The old 10 s timeout released both and left an applied
    definition unpersisted, with its demand unpublished and Clean reopened.
  - **Applied.** It republishes the run's `keyer` demand set (`ConsumerSet::publish`, which
    leases the new set before it releases the old one), persists the definitions, and only
    then drops the request's leases.
  - **Serialized reconciliation (review round 2 J1).** Republish and persist run together
    under the config-mutation lock, and the definitions are read *inside* the lock
    (`reconcile_keyers`), never captured before it. Committed definitions only move
    forward, so the settlement that runs last writes the newest version. Before, a
    settlement captured version N, was descheduled while N+1 applied, published and
    persisted, and then republished N's demand (releasing sources the engine still keys) and
    persisted N last, so a restart came back with stale keyers.
  - **Refused or busy.** The leases drop at once.
- **Clean rule (review round 1 H3: one counted admission point).** One `CleanDskGate`
  (`multiview_engine::clean_gate`) is shared by the switcher (`SwitcherControl::clean_gate`),
  the routing handle (plan answers) and the router (drain admission). It replaces the old
  "DSK defined" `AtomicBool`, which raced: the router read `false` while the handler set it
  and read a routing snapshot that did not yet show the pending Clean take, so both could
  proceed. A failing request could also clear the bool under a queued sibling.
  - **One word.** The gate is one `AtomicU64` holding a committed "defined" bit, a count of
    DSK reservations and a count of Clean reservations. Every reservation is one CAS that
    checks the other side in the same step. A DSK reservation is refused while any Clean
    reservation is held. A Clean reservation is refused while a DSK is defined or any DSK
    reservation is held. Reservations are RAII guards that release exactly their own count,
    so a request that fails can never release another's. The operations are lock-free and
    allocation-free.
  - **Bounded on the clock (review round 2 J2).** A lock-free CAS loop is not wait-free:
    concurrent control-plane reservations could keep invalidating the clock's CAS. The clock
    therefore admits only through `tick_reserve_dsk` / `tick_reserve_clean`, which make at
    most `MAX_TICK_ATTEMPTS` (4) CAS attempts and then refuse as `TickRefusal::Contended`,
    counted on the gate (`contended()`). The switcher answers that refusal
    `clean-gate-contended` (`REFUSE_KEYER_GATE_CONTENDED`; nothing changes, the caller
    re-issues); the router refuses the Clean take `Unroutable`, as when excluded. Off-clock
    callers keep the retrying CAS. Every release is one non-retrying `fetch_sub` on its own
    count field (the field holds at least the guard's own count, so it never borrows into the
    neighbour), and `set_defined` is one `fetch_or`.
  - **Control plane.** A `keyer_set` that defines a new keyer (not only the first one) takes
    a DSK reservation before its leases and enqueue. A refusal is 409
    `clean-unavailable-with-dsk`. It holds the reservation in its settle task until a
    terminal engine answer (H4). A refused, lease-less or busy request drops only its own.
    The plan answers from the same gate (`clean_reservations() > 0`).
  - **Router.** A take to Clean is admitted at the drain only by taking a Clean reservation
    for its output (`hold_clean`). A take set with any Clean member that cannot reserve is
    refused whole, and a re-issued Clean boot take reserves again. The tick's
    `reconcile_clean` releases the reservation of every output that neither carries Clean
    nor has a take to it pending (O(outputs); the slots are sized at construction).
  - **Engine backstop.** At the tick, a `keyer_set` for a free slot takes its own DSK
    reservation (bounded, J2). If Clean is held it is refused `clean-unavailable-with-dsk`
    (`REFUSE_KEYER_CLEAN`), if contended `clean-gate-contended`, and nothing changes. The reservation is held across the compose.
    The commit sets the "defined" bit before the tick's outcomes, then drops it. A rejected
    frame drops it without setting the bit. Attach sets the bit for declared DSKs. Nothing
    clears it (no keyer removal verb).
  - **Every interleaving.** Each side admits only through its own CAS against the other side's
    count, so at most one wins. Whoever wins keeps the other excluded until its reservation
    settles (DSK) or its output leaves Clean (Clean). After a DSK commits, the bit keeps
    Clean out.
- **GET.** `GET /api/v1/switcher/{me}` adds:
  - `keyers[]` (design note §8 shape: `id`, `family = "dsk"`, `fill`, `key`, `type`, `fly`,
    `mask`, `on_air`, `tie`, `ramp {start_tick, rate, to_on}`, `rate`, `loss`, `stale`,
    `pending`), filtered by the keyer object id, with an unreadable fill or key replaced by
    `"redacted"`;
  - `tally {program, preview}`: the engine roles resolved to source ids off the clock. An
    unreadable id is dropped, and so is a keyer role whose keyer row is unreadable.

  The snapshot carries ramp descriptors, so it changes only on state changes.
- **`switcher.keyer` event.** A new `Event::SwitcherKeyer {me, program, keyer, tick, on_air,
  stale, reason, fill, key}` on topic `switcher`, with envelope `id` = the keyer object id.
  - **Scope.** It has its own `AuthzScope::Keyer {me, program, keyer}`: the M/E object, the
    `program:` grant and the keyer object, the same rule GET filters rows by. This is a new
    variant of the exhaustive enum, and `scope_permits` handles it.
  - **Redaction.** The per-client projection (`redact_embedded_objects`) replaces an
    unreadable fill or key with `"redacted"`, as GET does.
  - **Relay.** `switcher_keyers::forward_keyer_events` turns committed `KeyerEvent`s into this
    event off the clock. The CLI spawns it next to the outcome relay.
  - **AsyncAPI.** The message and schema are added.
- **Persistence.** After `applied`, the committed definitions are written as
  `switcher.downstream_keyers` of the accepted state, under the config-mutation lock, after a
  full `validate()`. Nothing is written before the engine applied the change (tested).
  On-air, tie, ramp and pending state are never written. A restart therefore comes up with
  every keyer off, except `on_at_start` DSKs, which return as the op-less boot intent. The
  test round-trips the persisted document through TOML, `validate()` and `keyer::admit`.
- **CLI wiring.**
  - `attach_first_slice` now always installs a keyer runtime when an M/E exists (an empty
    runtime is a no-op per tick), so the first DSK can be defined at run time.
  - The pipeline creates the `keyer` consumer set for every M/E run (seeded only with declared
    DSKs) and hands it to the switcher (`set_keyer_demand`).
  - The routing handle and the router get the switcher's `CleanDskGate`.
- **Deviations and choices.**
  - **`arm` is not built.** It needs USKs (K3, #388). USK `keyer_set` fields arrive with K3.
    (**Built in K3**, see "K3 as built".)
  - **z is fixed after definition.** The design is silent on re-ordering. Changing z would
    re-order draws mid-flight, so a `keyer_set` with a different z on an existing keyer is 422.
  - **No delete.** The design defines no verb that removes a keyer. A DSK is removed by
    editing the config.
  - **Fill/key change on an engaged keyer.** With `allow`, the new sources must be fresh at the
    applying tick, else the change is refused (`keyer source has no fresh frame…`), never
    held. The design is silent here. Refusing keeps the slot's one pending entry for operator
    CUT/AUTO.
  - **New-source leases.** These are video-only preview leases
    (`SourceDemandProvider::preview_lease`), held until settle. The durable lease is the
    `keyer` consumer set, as at boot.
  - **The +1 decode charge** for a fill or key that is not running (ADR-0110 §10) is not
    computed separately. The lease is refused if the demand manager cannot take it (409
    `capacity`).
  - **Every new keyer reserves, not only the first (H3).** The brief refuses "the first DSK"
    while Clean is routed. The gate makes any `keyer_set` that defines a new keyer reserve.
    While a DSK is already committed Clean cannot be held, so the result is the same. This
    also closes two first-DSK requests racing on an empty set.
  - **No per-principal fairness** on the switcher routes and the shared idempotency store. It
    is still tracked in #394.
- **Tests.**
  - `multiview-engine` `tests/keyers_set.rs`: define at one tick and publish on change,
    refusal over budget, on-air guard with protect and allow, a rejected frame restoring the
    definition, and id/z clashes; a definition-changing tick builds no readback on the clock,
    and a set admitted on a stale base is refused while the published readback is the
    prepared `Arc` (H6); a set drained at its deadline tick is refused and one drained the
    tick before applies (H4).
  - `multiview-control` `tests/switcher_keyers.rs` (15):
    - the BOLA matrix for each verb on plan and command, for the M/E, program, keyer and
      source grants and a viewer, with no key burned;
    - a Viewer and a ReadOnly key holding every grant are 403 on `/plan` for all four verbs
      (H1);
    - 202 plus outcome, replay and ramp refusal;
    - a `keyer_auto` retry during its own ramp replays the first 202, enqueuing nothing, while
      a new key is 409 `keyer-ramping` (H2);
    - 503 on a full mailbox, releasing the key and the leases;
    - `in:<n>` refused with 422;
    - `capacity` generic for scoped callers, totals for unscoped, a repeated refusal leaving
      nothing behind, and `max_source` raise admission;
    - `on-air-protected` and make-before-break leases;
    - a `keyer_set` held past any wall-clock deadline keeps its leases and Clean exclusion,
      is refused `keyer-set-deadline` when drained past its deadline tick, and leaves no
      definition, lease, reservation or persisted keyer (H4, paused tokio time);
    - the Clean rule;
    - a settlement paused after its outcome while a newer `keyer_set` applies and settles
      leaves both the durable document and the demand set at the newer version (J1,
      deterministic through the `_test-seams` `settle_seam`);
    - GET/events parity and redaction for scoped principals;
    - persistence and a restart.
  - Lib unit tests: tally resolution, row filtering and redaction, and the `Keyer` scope.
  - `multiview-events` `authz_scope` (switcher.keyer).
  - `multiview-control` `tests/openapi.rs`: the plan `200` is `SwitcherPlan | KeyerPlan` (H5).
  - `multiview-cli` `output_routing`: a queued Clean take refused at the drain once a DSK is
    reserved on the shared gate; a DSK reserved from inside the drain (after the Clean take's
    admission, before the snapshot shows it) loses; a reserved DSK refuses Clean at the
    drain, and a Clean take's reservation is released on refusal and on supersession (H3,
    `drain_hook` seam, no sleeps).
  - H3 control tests (`switcher_keyers`): A queued, sibling B refused 503, and a Clean
    reservation stays refused while A is queued and after A applies; both orders at the one
    admission point. Engine (`keyers_set`): the tick refuses a new keyer while Clean is held
    and a rejected definition releases the tick's reservation. Lib `clean_gate`: counting,
    and 2 000 barrier-synchronised races of both orders, never both winning.
  - J2 lib `clean_gate`: a concurrent reservation landing between every load and CAS of a
    clock admission (attempt seam, deterministic) is refused `Contended` within
    `MAX_TICK_ATTEMPTS` and counted (it spun 1 001 attempts before the fix); the bounded
    entry points keep the exclusion, and releases leave the other field and the defined bit
    intact.
- **Concern: tick refusals allocate once per command (review round 2 J3, investigated, not
  redesigned).** A tick-side `keyer_set` refusal (`keyer-set-deadline`,
  `clean-unavailable-with-dsk`, `clean-gate-contended`) builds its `String` reason and
  `EventStream::publish` wraps the outcome in an `Arc`. Every existing tick-side switcher
  refusal on main does the same through the same paths: `publish_outcome`
  (`switcher_control.rs` on main: 1539) for `REFUSE_LOST_DURING_AUTO.into()` (1003) and
  `REFUSE_NO_FRESH_FRAME.into()` (1024, 1048, and `refuse_armed` 1216 from 1246/1300), and the
  K2a keyer `refuse` (`keyer.rs` on main: 653 `reason.to_owned()`) for `keyer-rate` (945) and the
  repeat deadline (1144); `EventStream::publish` (`isolation.rs` 223, `Arc::new`). The K4
  refusals sit in the same mailbox drain and consume their command, so it is the existing
  per-command outcome pattern: bounded by the 64-slot mailbox and `MAX_COMMANDS_PER_TICK`,
  one event per refused command, never steady-state (no K4 verb is held across ticks).
  `keyers_set` asserts one outcome per refused command and nothing on later steady ticks.
  Making outcome publication allocation-free is a cross-switcher change, out of K4.
- **Concern.** The GET/events parity test drives the driver without the frame hook, so its
  `tally` is the background roles only. Keyer tally resolution is covered by the lib unit
  test on a constructed snapshot.

### K3 (#388): upstream keyers, the arm set and key-changing transitions

Built on K4 (`feat/keyers-k4`, PR #476), then merged with main after the K4 merge and the
WGSL key path (#386 part 2, PR #502). CPU oracle first; the GPU draws every K3 frame since the
merge (see "K1 GPU key path as built", wipe masks).

- **Compositor.**
  - `SceneMerge::Wipe { mask }`: `lerp(out, in, m(x, y))` per canvas pixel with lane
    `transitions`' `WipeMask` (no border band). `SceneMerge::apply` now takes the canvas pixel
    and size; `merge_scenes` derives each accumulator pixel's canvas row from the band geometry.
    A mask not valid for the canvas merges as the outgoing scene (never a poisoned pixel).
  - `WipeMask::complement` / `complemented()`: coverage `1 − m(x, y)`, endpoints included. It is
    the tied DSK's going-off envelope. The GPU draws both (merge kind ≥ 2 in `KeyParams`, the
    complement in `TileParams::prim2.w`; `KeyParams::build` now takes the canvas size). Main
    removed `key::needs_cpu_keyer_path`; `FrameJob::needs_cpu_keyer_path` (for a composer
    without a key path, the simulated chaos GPU) also counts a complemented mask.
- **Config.** `[[switcher.mix_effects.keyers]]` is `UpstreamKeyerConfig` (`deny_unknown_fields`):
  the DSK fields minus `program`, `rate`, `tie` and `on_at_start`. `validate_keyers` (run by the
  boot preflight `validate`) checks at most 4 per M/E, unique id and z per M/E, the DSK
  per-field rules (through `as_definition`, labelled "upstream keyer"), and charges
  `USK_FOLD (2) × dst/canvas + u × sources/canvas` to the **shared** `switcher.keyer_budget`.
  The default budget is unchanged (3.0): one full-canvas keyer of either family.
- **Runtime.** `KeyerRuntime` has `MAX_KEYERS = 6` fixed slots: DSKs at `0..2`, USKs at `2..6`
  (USK key index `k` is slot `2 + k − 1`, the declared order). Draw order is every USK in
  ascending z, then every DSK in ascending z. `KeyerDef::family`; a USK's object id is
  `<me>/<id>`. `SwitcherState` keeps key states in a fixed `[bool; 4]` and the armed set in an
  `Arc<ArmedSet>`, so the per-tick staging clone stays allocation-free; the driver mirrors the
  runtime's USK on-air states into the machine at commit (a loss or a key cut can differ from
  its toggle).
- **`arm`.** `SwitcherVerb::Arm(Arc<ArmedSet>)` (built off the clock) goes through
  `SwitcherState::arm_shared` (empty, duplicate, out-of-range and in-flight refused, fixed-array
  scan). A key that is not a defined USK is refused `unknown-keyer`. The runtime caches the set
  as a `Copy` `NextTake`; a rejected frame restores it with the machine.
- **Transitions.** Armed USKs reuse the tied-DSK flight bookkeeping (`flight: Some(direction)`,
  fixed at the AUTO start, `complete_tied` on the terminal tick or CUT, `cancel_tied` on an
  abort, punch or reject), so **an aborted AUTO (ADR-0104 §2) restores key state** and a USK that
  never went on releases its pair. During an AUTO an armed USK is drawn whole (weight 1) with
  `DrawRole::In` / `Out`; every USK below the highest changing one is `Both`; the rest stay
  `Top`. The drive turns a key-changing frame into scene groups: the Program cell is `Out` (or
  `Both` when the background does not change or the incoming frame is not fresh), the incoming
  background is one `In` tile at opacity 1, the WIPE border band is `Both`, and the merge is MIX
  `p`, DIP `dip_weights` in the fill colour (linear, decoded once per colour) or WIPE `m(x, y)`.
  A frame without a changing key keeps the single-accumulator path.
- **Key-only transitions** (background not armed): no incoming picture, no DIP colour, a DIP
  merges as a MIX and its keys follow `p` (`MixOverlay::envelope(false)`), the background never
  swaps, `CompositedFrame::mix` is `None` (Program audio does not crossfade), PVW is not Program-
  tallied, the SW2-02 terminal abort and the "same source on both buses" refusal do not apply.
- **Tied DSK on a WIPE** (replaces the K2a deviation): `TieEnvelope::wipe` carries the mask;
  the keyer is drawn through `m(x, y)` going on and its complement going off, at weight 1 while
  that side is visible at all (so the tally never names an invisible keyer), and flips on the
  terminal tick.
- **Fresh gate.** A CUT or AUTO waits for every armed USK going on (and every tied DSK, as
  before), then is refused at +50 ticks; a key-only take needs no fresh background; going off is
  never gated.
- **Verbs on a USK.** `keyer_cut` flips it outside a background transition
  (`REFUSE_KEYER_IN_TRANSITION` during one); `keyer_auto` and `keyer_tie` are refused
  (`REFUSE_KEYER_USK_VERB`).
- **Tally.** A USK participant is Program-tallied only while its side is visible (not at MIX
  `p = 0` going on, nor before a DIP's switch point); armed USKs going on are in `preview`.
- **Clean.** USKs are part of the M/E scene, so they never take the Clean / DSK exclusion: the
  gate's "defined" bit and the tick backstop count DSKs only (`any_dsk`).
- **CLI boot.** `admit_keyers` resolves the M/E's USKs after the DSKs; the `keyer` demand set
  seeds their fills and keys labelled `me1/<id>`.
- **API.**
  - `arm {background, keys}` on the existing plan/command routes: `Idempotency-Key`, 202 +
    `operation_id`, outcome on `switcher.operation`. Authorization exactly as K4: `Write` role
    on plan and command, `Control` on the M/E, `Program` scope, `Control` on every armed keyer
    object id (`me1/<key>`, the list bounded at 5 before it is walked), then `Read` on every
    armed key's fill and key, all before any existence answer (an unseen key is 403, a visible
    undefined one 404). 422 for an empty, duplicate or over-long set; 409 `in-flight` during a
    transition.
  - `keyer_set` / `keyer_cut` take `family = "usk"` (object id `<me>/<keyer>`). A USK
    `keyer_set` merges onto the definition, validates with the USK rules, admits its cost
    against the shared budget, never reserves the Clean gate, refuses `rate`/`on_at_start`
    (422 `keyer-usk`) and persists into `switcher.mix_effects[].keyers` after `applied`.
  - `GET /api/v1/switcher/{me}`: `keyers[]` lists USK rows (`family = "usk"`, `armed`), and
    `armed {background, keys}` lists the readable armed keys; rows, armed keys and
    `switcher.keyer` events are filtered by keyer object id and redacted alike (tested).
  - OpenAPI regenerated with `cargo run -p xtask -- gen-openapi` (`cargo xtask` has no alias in
    this checkout); AsyncAPI is unchanged (`switcher.keyer` already carries the object id).
- **Deviations and choices.**
  - **Request field `family`.** The design note's `keyer` names a DSK; K3 adds an optional
    `family` (`dsk` default) rather than inferring the family from the id, so a USK and a DSK
    may share an id and every request names the object it authorizes.
  - **USK `keyer_cut`** is allowed outside a transition (design note §8), refused during one.
  - **A key-only CUT/AUTO** uses the existing `cut` / `auto` verbs after an `arm` without the
    background; there is no separate key-transition verb.
  - **WIPE envelope weight.** A tied DSK on a WIPE carries weight 1 (0 at the invisible end) and
    the mask; the scalar envelope is a visibility flag, the pixels come from the mask.
- **Tests.**
  - `multiview-compositor` `tests/keyers_gpu.rs` K3 test (above) and `tests/keyers_wipe_merge.rs` (5): frames 0, mid, end of a WIPE with a
    key going on against `lerp(scene_out, scene_in, m)` with each scene composed separately and
    an independently written-out reveal; a key-only WIPE; a soft circle merge equal on the
    oracle and 1-4 threads; the complement is `1 − m` and a foreign mask keeps the outgoing
    scene; a tile through a complemented mask.
  - `multiview-config`: unit tests (3: limits, uniqueness, shared budget, round trip, DSK-only
    fields refused) and `tests/switcher_keyers.rs` (the preflight accepts USKs; budget shared).
  - `multiview-engine` `tests/keyers_usk.rs` (10): arm validation; CUT with fly and an unarmed
    key staying; MIX, DIP and WIPE frames 0, mid, end with a key going on, against
    `merge(scene_out, scene_in)` composed separately; a key going off; key-only MIX, DIP (as
    MIX) and WIPE; an unarmed USK below (`both`) and above (`top`) an armed one; an AUTO aborted
    on a stale incoming source restores key state; the fresh gate (+50, 0 lost ticks, going off
    never gated); USK verb refusals; USKs below DSKs. `tests/keyers_dsk.rs`: the WIPE tie test.
  - `tests/keyers_usk_chaos.rs` (2, invariants #1/#10): 1 500 ticks with armed-key sources
    stalling and one vanishing during AUTOs under an `arm`/AUTO/CUT flood: contiguous ticks,
    PTS from the tick counter, 0 repeats, one outcome per op, keys dropped at the trigger;
    6 000 ticks of key-changing MIX, DIP, WIPE-with-border and key-only AUTOs: the drive's
    scratch, slate and DIP-fill odometers and the definition builds never move after warm-up.
  - `multiview-control` `tests/switcher_keyers_usk.rs` (5): the `arm` BOLA matrix on plan and
    command (M/E, program, keyer object, sources, viewer; 403 before 404; no key burned); shape
    and `in-flight`; 202 + outcome + replay and the armed set in GET; USK `keyer_set` /
    `keyer_cut` and persistence to the M/E; GET/events parity and redaction for scoped
    principals.
- **Concerns.**
  - **No counting allocator** (as K2a: the workspace forbids `unsafe_code`); the gate is the
    pooled odometers plus fixed arrays. `arm` allocates only off the clock (the `Arc` set) and
    its refusals use the existing per-command outcome path.
  - **Cost of a key-changing frame:** two extra full-band accumulators and a merge per pixel on
    the CPU while a USK changes; the default budget (one full-canvas keyer) bounds it until the
    mda001 run.
  - **GPU timing:** the WIPE merge adds one mask evaluation per pixel of a key-changing frame;
    lavapipe proves equality only, and the RTX 4060 figures are still owed with the bench.

