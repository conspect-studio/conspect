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
KeyParams { key_layer, key_w, key_h, kind|invert|group (u32 bits),
            key_luma_scale, key_luma_off, clip, gain,
            mask: vec4<u32>, merge: vec4<f32> /* kind, p | d, i */ }
```

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
- **Deviation: WIPE merge and the GPU key path are not in K1.** `SceneMerge::Wipe` lands with K3 on
  top of lane `transitions`' mask. The WGSL key path (`key_params`, binding 6) is not merged before
  #395. Until it is, `GpuCompositor` refuses a frame with a keyed or grouped tile with a typed
  `GpuLimit`, and `RunBackend` routes a keyed or grouped frame (an allocation-free scan, before
  any GPU work) to a **persistent** CPU reference owned by the GPU compositor, whose LUT cache and
  band/scene scratch survive across ticks (counted in `gpu_cpu_fallback_frames`; its pool is what
  `scratch_alloc_count` reports on a GPU backend). It is never drawn without its key. A genuine
  GPU failure falls back through the same persistent backend. The LUT cache checks a frame's transfer set
  in place (no per-frame list is built unless the set changed). The parallel CPU path still spawns
  its scoped band threads each frame, as it did before K1: a fixed, bounded cost, tracked in #416
  (persistent band workers). #395 (the bounded GPU readback, PR #424) merged after K1 was written; the
  persistent fallback is now main's `cpu_fallback_backend`, and the WGSL key path is the next GPU
  step (owed, tracked on #386). ADR-0113 §9 already bounds this:
  the default budget is 1 full-canvas DSK until the mda001 run passes.
- Tests: `tests/keyers_cpu.rs` (20: goldens for linear self key with alpha used once, linear key
  source, luma clip/gain/invert, luma on a key source, mask with a flying key, unequal fill/key
  sizes up and down, mixed ranges, premultiply-once after primaries, weight; a monotonic and
  bounded clip/gain property; exact MIX and DIP scene merges against an independent per-pixel
  computation written out in the test (never the production helpers), both DIP legs; a NaN weight
  reads as 0; the tile-driven kernel equal to the oracle with keys and groups at 1-4 threads, on a
  small canvas (proptest) and on a 320x256 canvas above the parallel threshold with tiles across
  band edges; no scratch growth over 50 grouped composites) and `tests/keyers_gpu_fallback.rs`
  (`wgpu`: the refusal, the oracle-equal fallback, and no scratch growth or LUT rebuild over 20
  repeated keyed and grouped fallback frames).
- Bench: `examples/keyer_composite_bench.rs`. CPU figures from a shared 56-core devcontainer (not
  the target host; median ms at 1080p): 0 keyers 30.0, 1 full-canvas alpha DSK 41.1, 2 full-canvas
  keyers 54.5, 2 flying 576x324 31.2, 1080p source in a 128x72 fly 27.6, key-changing MIX + DSK
  58.9. The calibration of `u` and `fold` on the RTX 4060 is owed with the GPU key path.

### K2a (#387): DSK runtime in the Program drive

- **Config.** `[[switcher.downstream_keyers]]` and `switcher.keyer_budget` are typed fields of
  `SwitcherConfig` (named fields win over `flatten`, so they leave `extra`; other future keys still
  round-trip there). `DownstreamKeyerConfig` is `deny_unknown_fields`. `type = "chroma"` and
  `premultiplied = true` parse only so validation can refuse them by name. `validate_keyers` runs
  from the document validation, where the canvas is known. It checks: at most 2 DSKs; unique ids
  and z; a declared program; fill and key are declared sources, `in:` refused; rate in 1..=300;
  clip in [0, 1]; gain finite and >= 0; fly even, >= 16 and inside the canvas; `max_source` even
  and <= 8192; mask inside `max_source`; summed cost within the budget.
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
    - `source-lost`.
  - An AUTO toward the current state is an applied no-op.
- **Gate, pair, loss.**
  - **Going on.** Fill and key must be fresh (`read_at(pts)` Fresh) and inside `max_source`.
    Going off is never gated.
  - **Reads.** Each keyer that is on, ramping or riding a tied MIX does one `read_at` per source
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
    (#388).
  - **Gate.** A CUT or AUTO is also gated on the fill and key of every tied keyer it would put on
    (`take_fresh`).
  - **Abort.** An AUTO aborted by SW2-02 never reaches the terminal tick, so tied keyers keep their
    state.
  - **Boot.** `on_at_start` is seeded as an op-less pending going-on. It lands at the first fresh
    tick with an `OnAtStart` event and no outcome. An operator command supersedes it.
- **Reject atomicity.** `drain_at_tick` copies the `[KeyerFlags; 2]` (Copy, no heap).
  `reject_composed_tick` restores them, refuses slot-held ops that moved this tick, and discards
  queued events. Applied keyer ops sit in `staged_success` and are refused with everything else.
  `Staged` is untouched. `repeat_tick` expires held keyer commands past their deadline.
- **Compose.** The hook resolves keyers only on a frame that will commit, after the mix block. It
  calls `driver.resolve_keyers(tick, envelope, &mut keyer_buf)` and then `drive.set_keyer_draws`, a
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
- **CLI boot.** `attach_first_slice` resolves `KeyerDef`s from `pipeline.preview_stores()`; a
  missing store is a boot error. It then calls `keyer::admit` against the document's budget and
  `driver.set_keyers`.
- **Deviation: Clean is refused while a DSK is defined (until K2b).**
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
  - `program` holds the background, the PVW role while a MIX draws it, and the fill and key of
    every keyer tile drawn with weight > 0.
  - `preview` holds PVW plus the fill and key of tied keyers going on.
- **Deviation: GPU before #395.** A keyed tile makes `GpuCompositor` return `GpuLimit`, and
  `RunBackend` composes that frame on the CPU (`gpu_cpu_fallback_frames`). It is never drawn without
  its key. The ADR §7 rule "CPU fallback only if the reserved cost fits the CPU budget, else
  keyer-less frames and `gpu-fallback` after 3" is **not** built in K2a. It needs a per-frame
  fallback signal from `RunBackend` into the drive, and that is a follow-up with K2b / after #395.
  The default budget (1 DSK) bounds the CPU cost meanwhile.
- **Tests.**
  - `multiview-config` unit tests (8 keyer cases) and `tests/switcher_keyers.rs`.
  - `multiview-engine` `keyer` unit tests.
  - `multiview-control` `tests/output_routing_take.rs`: Clean is refused in the plan and as a 409
    `clean-unavailable-with-dsk` on take while a DSK is defined.
  - `tests/keyers_dsk.rs`: CUT, AUTO k/N, tie on CUT, MIX, DIP and WIPE (the CUT-at-end deviation), abort, reject, held background take,
    `on_at_start`, unknown index, z order and tally.
  - `tests/keyers_gate_loss.rs`: refusal at exactly +50 with 0 lost ticks, `keyer-pending`,
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
