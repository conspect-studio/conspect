# ADR-0113: Upstream and downstream keyers in the Program drive: one compose pass, per-tile key alpha, tie/cut/auto, admission by covered area, tally from the render list

- **Status:** Accepted
- **Area:** Core engine / Compositor / Switcher / Web/API (invariants #1, #5, #8, #9, #10)
- **Date:** 2026-11-13
- **Source:** operator request of 14 Nov 2026 (AEDT) ("full production switching and routing (including fades, wipes, etc)"); lane `keyers`
- **Design note:** [switcher-keyers.md](../architecture/switcher-keyers.md) (data model, render list, cost, admission, API, desk, tally, tests, slices)
- **Refines:** [ADR-0056](ADR-0056.md) (keyer model; this ADR is the build decision for it on the as-built Program drive), [ADR-0104](ADR-0104.md) (AUTO in the one Program drive, fresh-frame gate), [ADR-R011](ADR-R011.md) §3 (keyer loss policy)
- **Amends:** [ADR-0056](ADR-0056.md) §3 (the flying box is a placement of any keyer, not a key type; shaped fills are refused in the first build), §5 (multi-box composition stays out of scope here)
- **Relates to:** [ADR-0058](ADR-0058.md) (NV12+A alpha media), [ADR-C007](ADR-C007.md) (key math domains), [ADR-0108](ADR-0108-output-routing-matrix.md) §2.5 and §10 (Clean, the Program protection bound), [ADR-0110](ADR-0110-patch-panel.md) (mixer inputs, persistence after `applied`), the DIP/WIPE work of lane `transitions` (per-tile wipe mask, dip weights)

## Context

The operator asked for full production switching. CUT and the AUTO dissolve run on main. DIP and
WIPE are being built in the same compose pass by lane `transitions`. Keyers are the remaining gap:
no issue tracks them against the code as it is today, and ADR-0056 predates the as-built Program
drive.

What is true on main (`1b59d254`), read from the code:

- **Program is one drive and one compositor call per tick.** The switcher frame hook
  (`crates/multiview-engine/src/switcher_hook.rs`) binds the one full-canvas Program cell to the PGM
  source. During an AUTO it adds one full-canvas tile for the incoming source at
  `elapsed / duration` (`CompositorDrive::set_mix_overlay`, `crates/multiview-engine/src/drive.rs:256`).
  Compose builds a flat, z-ordered `Tile` list from pooled scratch and makes one backend call
  (`drive.rs:558`).
- **Per-pixel alpha already exists in both kernels.** `Nv12Image` can carry a straight R8 alpha
  plane from a bounded `AlphaPlanePool` (`crates/multiview-compositor/src/pipeline.rs:172-261`).
  The CPU fold multiplies it into tile opacity at the single premultiply step
  (`pipeline.rs:1401`, `:1680`). The GPU shader does the same from a pooled R8 array
  (`crates/multiview-compositor/src/gpu/shaders/composite.wgsl:60`, `:151`), with
  `TileParams.dst_size.zw = [has_alpha, alpha_layer]`. Gates exist: `tests/nv12_alpha_gpu.rs`
  and `examples/alpha_composite_bench.rs`. On an M3 Ultra the bench measured a 1080p composite of
  9 tiles (640x360, together one canvas of area) at a GPU median of 1.60, 1.77 and 2.07 ms with
  0, 1 and 6 alpha tiles, and about 7 ms on the CPU (ADR-0105).
- **There are no key kernels.** There is no luma key, no key source, no mask and no key state in
  the engine. `multiview-core::switcher` has descriptor types for keyers (`KeySource`, `KeyType`
  {luma, linear, dve_pip}, `KeyerTransition` {cut, auto, tie}, `KeyerDescriptor`, `ArmedSet`
  {background, keys}) and `SwitcherState` keeps `keys_on_air`. Nothing renders them, and no
  config, API or UI reaches them. `[switcher]` keeps unknown keys in `extra` and
  "no runtime advertises those domains" (`crates/multiview-config/src/switcher.rs:91`).
- **Fills with alpha that exist today:** graphics sources (`kind = "graphic"` and
  `"graphics_channel"`) publish NV12 plus straight alpha. Their own mix is an opacity LUT on the
  generator thread (`docs/architecture/graphics.md`). Media players do not decode alpha. FFmpeg
  decode has no `yuva` path. NDI keeps only BGRA colour and drops alpha
  (`crates/multiview-input/src/ndi/convert.rs:369`). Any video source can be a key signal
  through its luma (the classic fill and key pair).
- **Clean** is the Program canvas before the monitoring bake (ADR-0108 §2.5, slice 2). It is
  published by the bake consumer, so today it contains everything Program composes.
- **Tally** on the desk is derived in the browser from `program_source` and `preview_source`
  (`web/src/components/broadcast/deskSafety.ts`). The engine tally arbiter is not wired into the
  run loop (#42).
- **Operator protection:** every Program change on mda001 is accepted against the Program
  protection bound (output-routing.md §10). That bound is lateness p50 +1 ms, p99 +2 ms and
  max +5 ms against a baseline, with 0 `frames_repeated` and an HLS PTS step of 3003.

## Decision

1. **Keys render in the Program drive's one compose pass. There is no second compositor and no
   overlay bake.** The per-tick tile list is fixed in this order (bottom to top):
   1. the Program cell (outgoing picture),
   2. the transition tile (AUTO, DIP or WIPE incoming picture, and the dip colour tile, as lane
      `transitions` builds them),
   3. upstream keyers (USK) of the M/E, in ascending `z`,
   4. downstream keyers (DSK) of the program, in ascending `z`,
   5. (reserved) the FTB cover. FTB is not part of this ADR.

   Each keyer is **one tile**, whatever its type. Its weight multiplies its key alpha. A keyer that
   is off and not ramping adds no tile and costs nothing. Every tile is composed in linear light
   with the existing premultiplied `over`, so overlapping keys and a flying key over a wipe are
   exact (no through-blend: each background is one full-canvas tile).

2. **Per-tile key alpha, computed in the kernel front half (ADR-0056 §4 and ADR-C007, unchanged
   domains).** `Tile` and `TileParams` gain a key block. The CPU reference lands first and the GPU
   follows under the existing SSIM ≥ 0.98 / PSNR ≥ 40 dB gate.
   - `luma`: `a = clamp((y_norm − clip) × gain, 0, 1)`, then optional `invert`. `y_norm` is the
     post-range-expand code-value Y of the key signal: the fill itself (self key) or the key
     source.
   - `linear`: `a` is the fill's own NV12+A alpha. With a key source, `a` is the key source's
     post-range-expand Y (a fill and key pair). If the fill has no alpha plane and there is no key
     source, `a = 1`: an opaque box. This is the picture-in-picture case.
   - `chroma`: ADR-C007's linear-RGB, tile-gamut distance. It is a later slice (K7), and its
     parameters are refused by the API until that slice lands.
   - **Mask:** a rectangle in fill-source pixels. Alpha is 0 outside it. Because it is in source
     space, it travels with a flying key.
   - **Weight:** the keyer's on-air weight in `[0, 1]`. A wipe-ridden key also takes the wipe's
     per-tile mask (item 4).
   - Final straight alpha = `opacity × fill_alpha_plane × key_alpha × mask × weight`. It is
     premultiplied once, at the existing step. The color pipeline order is untouched (#8).
   - A key source is one more texture-array layer on the GPU and a second plane read on the CPU.
     A keyed tile with a key source uses 2 of the `MAX_TILES = 64` layers.
   - **Shaped (premultiplied) fills are refused** (`premultiplied = true` returns 422 "shaped
     fills are not supported; deliver a straight fill"). This amends ADR-0056 §3. The existing
     alpha path is straight-only, and un-premultiplying gamma-coded NV12 correctly is not built.
     Refusing is honest, and a wrong edge would be silent.

3. **The flying (PiP) key is a placement of any keyer, not a key type.** It amends ADR-0056 §3's
   `dve_pip` key type. `fly = { x, y, w, h }` is an even-aligned canvas-pixel rectangle inside
   the canvas, at least 16 px wide and high. By default it is the full canvas. It is drawn with
   the existing scale-at-composite (nearest neighbour, as every tile today). The descriptor's
   `KeyType::DvePip { rect }` is read as `linear` plus `fly = rect` for compatibility. Borders,
   shadows, sub-pixel placement and bilinear scaling come with SW-25 (#51).

4. **USKs belong to the M/E scene and ride the background transition. DSKs belong to the program
   and survive it.**
   - USK state is `on_air` and `armed` (the next-transition set `{background, key[n]}` that
     `SwitcherState::arm` already validates). An on-air USK that is not armed stays at weight 1
     through a background transition, drawn above both pictures. An armed USK changes state with
     the next transition:
     - going on, its weight follows the incoming picture's weight (mix: `p`; dip: the incoming
       weight of `dip_weights`; wipe: weight 1 with the wipe's mask on its tile);
     - going off, it follows the outgoing picture's weight (the complement, or the inverted wipe
       mask).
     A key-only transition (background not armed) uses the selected transition type and rate. A
     DIP of a key with no background change is drawn as a MIX. A CUT flips armed keys on the
     same tick as the background.
   - Each DSK has its own `rate` and `tie` flag. DSK CUT flips its state at the next tick. DSK
     AUTO ramps its weight over `rate` frames. A **tied** DSK changes state with the next
     background transition and uses that transition's weight or mask, like an armed USK. `tie`
     stays set until it is cleared.
   - All progress is `f(tick.index − start)` in integer frames (ADR-T015). No wall clock enters
     keyer state.

5. **A keyer goes on air only with a fresh fill (and key): the ADR-0104 gate.** On the tick it
   would start, `TileStore::state_at(tick.pts).is_live()` must hold for the fill and for the key
   source. Otherwise the command is held in that keyer's own one-slot pending entry (one per
   keyer, fixed at attach, so a held key never blocks a background CUT) for up to
   `TAKE_PRIME_DEADLINE_TICKS` (50), then refused with a typed reason. A second command for the
   same keyer while one is held is refused. A background transition that carries armed or tied
   keys going on is gated on every one of them as well as on the incoming background. Assigning a fill or key
   takes a video demand lease on it, make-before-break as `select_preview` does, so an on-demand
   source starts before it is needed. A keyer that is on air or armed keeps its leases.

6. **Loss: ADR-R011 §3, default `drop_to_off_air`.** If the fill or the key reaches the trigger
   state (default `reconnecting`), the keyer goes off at the next tick boundary, cut by default.
   It emits `switcher.keyer` with `reason = "source-lost"` and stays off until an operator puts it
   on again. While a keyer is under its trigger, a held frame is drawn (inv #2). A slate is never
   keyed: if a key or fill sample is a slate, the keyer contributes nothing on that tick.
   `hold_last_good` is an explicit per-keyer opt-in.

7. **Clean is the pre-DSK picture.** When at least one output selects Clean and at least one DSK
   contributes on this tick, the drive makes a second backend call on the same tick. It uses the
   same tile list without the DSK tiles and publishes the result as Clean. Otherwise Clean stays
   the one-`Arc` alias it is today. The second call is admitted (item 8). The Program frame is
   composed first and never waits on the Clean result. If the Clean call fails, Clean repeats its
   last frame for that tick (counted). Program is unaffected.

8. **Admission by covered area, before the engine is asked (invariant #9).** Hard limits: 4 USKs
   per M/E and 2 DSKs per program. Each tile that a contributing keyer adds costs its destination
   area as a fraction of the canvas, plus 0.25 of that area when it has a key source. A
   pre-DSK Clean call costs the area of every non-DSK tile. `switcher.keyer_budget` caps the sum
   for keyers that are on air or ramping. The default is 4.0 on the GPU backend and 1.0 on the
   CPU backend. `/plan` and the command route answer 409 `capacity` and name the keyers holding
   the budget. The engine re-checks at the tick and refuses with the same reason, never by
   dropping a frame. On mda001 the default is accepted only after the protection-bound run in
   item 12.

9. **API: the existing switcher routes, new verbs and a larger state.** Every verb is a plan and
   command pair (`POST /api/v1/switcher/plan`, `POST /api/v1/switcher/commands/{kind}`). Each
   carries an `Idempotency-Key`, answers 202 with an `operation_id`, and reports its outcome on
   `switcher.operation`.
   - `keyer_cut` and `keyer_auto` (`{keyer, on}`; DSK only for `keyer_auto`, which uses the DSK's
     rate or a one-shot `duration_frames`).
   - `arm` (`{background, keys: [ids]}`): the USK next-transition set.
   - `keyer_tie` (`{keyer, tie}`): DSK only.
   - `keyer_set` (`{keyer, fill?, key?, type?, clip?, gain?, invert?, mask?, fly?, rate?, loss?}`):
     a Class-1 change at one tick. Changing the fill or key of a keyer that is on air is refused
     with 409 `on-air-protected` unless the request says `on_air = "allow"` (ADR-0110 §2).
   - `GET /api/v1/switcher/{me}` gains `keyers[]` (id, family, fill, key, type and params, fly,
     mask, `on_air`, `armed`/`tie`, `weight`, `in_flight`, `rate`, `loss`) and the engine-derived
     `tally` (`program[]`, `preview[]`, item 11).
   - **Authorization is per object on every id** (BOLA): `Control` on the M/E and on the keyer id
     (`me1/usk1`, `main/dsk1`), plus `Read` on every source the request names. Bodies redact
     every source id the caller cannot read. A scoped principal never learns that an unreadable
     source is on air; it sees `"redacted"`.
   - Keyer verbs share the switcher's 64-slot `try_send` mailbox and its bounded idempotency
     store. A full mailbox answers 503 and nothing grows. The switcher routes have no
     per-principal rate limit today. That limit is not added here; the mailbox bound is what
     protects the clock.

10. **Persistence: keyer definitions persist, on-air state does not.** Definitions live in the
    accepted state as typed config, `[[switcher.mix_effects.keyers]]` and
    `[[switcher.downstream_keyers]]`, moved out of `extra` with `deny_unknown_fields`. A definition
    is written only after the engine reports `applied` (ADR-0110 §9) and is validated at boot like
    the rest of `[switcher]`. On-air, armed and ramp state is runtime only: a restart comes up with
    every keyer off, as graphics channels do. The one exception is a DSK with
    `on_at_start = true`, the station bug. It goes on through the fresh-frame gate once its fill
    is live.

11. **Tally comes from the tile list Program composed.** At the end of each tick the hook derives
    `program` (every source with a contributing tile this tick: background, transition source,
    the fill and key of every keyer with weight > 0) and `preview` (the PVW source, plus fills
    and keys of USKs that will be on after the next transition and DSKs tied into it). This is
    bounded: at most 2 + 2×6 ids. It is published into the existing wait-free snapshot only when
    it changes. The desk shows it, and `GET /api/v1/tally` takes it when the arbiter is wired
    (#42). A source used only as a key signal is PROGRAM-tallied.

12. **Desk.** A keyer strip sits beside the transition group. It has a BKGD button and KEY 1–4
    buttons (the next-transition set, lit when armed), with an ON AIR lamp per USK. Each DSK has
    TIE, CUT and AUTO buttons with its rate. A per-keyer panel holds the fill and key pickers
    (sources the user can read), the type and its parameters, the mask, and fly x/y/w/h with
    corner presets. The wall shows key-source tally as `● PGM KEY` and `▲ PVW KEY`. Every disabled
    control says why ("no fresh fill", "capacity: keyers hold 3.8 of 4.0", "shaped fills are not
    supported").

## Rationale

- **One pass is the cheapest correct render.** A key is a tile with a per-pixel alpha. The
  kernels already multiply per-pixel alpha at the right step, and a full-canvas 1080p tile costs
  about one tile fold (~2.07 Mpx). A second canvas, an overlay bake or a chained program each
  adds a composite and an NV12 generation, and the bake is not tick-coordinated (ADR-0056 §2).
- **USK weights reuse the transition's own weights and masks.** "The key comes on with the wipe"
  is the same mask on one more tile. Nothing new is invented per transition type, and the result
  is exact.
- **Flying as placement**: an operator flies a luma-keyed logo or an alpha graphic as often as an
  opaque box. A separate key type would forbid that.
- **Area admission** counts what the kernels actually spend per keyer (destination pixels and one
  extra sample for a key source), and it is checked before any enqueue.
- **No persisted on-air state**: a restart must not put a stale graphic on air. The bug is the
  operator-declared exception.
- **Tally from the render list**: tally walks the same list the frame was drawn from, so a
  contributing source cannot go untallied. A key-only source is the known anti-pattern of other
  products.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| DSKs on the off-thread overlay bake | Not tick-coordinated, drop-oldest under load, after the Program taps, and no video-fill layer (ADR-0056, verified again on main). |
| Pre-render each M/E scene (background and USKs) to an intermediate canvas, then mix | Two extra composites and one NV12 generation per tick during every transition. The flat list is exact here because each background is a single full-canvas tile. |
| Clean from an aux canvas on the `routing-aux` thread | The aux thread cannot reproduce an in-flight transition at the same tick without publishing the full render plan every tick, which allocates per tick. The second call on the clock is bounded and admitted. |
| `dve_pip` as its own key type (ADR-0056 §3) | It forbids flying a keyed or alpha fill. |
| Support shaped fills now | An un-premultiply of gamma-coded NV12 is not built in either kernel. A wrong edge is silent. Refusing is honest, and the slice can add it later. |
| Persist on-air state | A restart or rollback could put a stale lower third on air. An explicit `on_at_start` bug covers the legitimate case. |

## Consequences

- **Invariant #1:** keyer state is fixed-size per drive (6 slots, allocated at attach), and the
  tile list comes from the pooled scratch, sized for the maximum keyer count at attach. Store
  reads are lock-free `read_at`. Nothing waits on a source, a client or a lock. The Clean call
  runs after Program is composed and cannot delay it past the bound measured in slice K2.
- **Invariant #10:** commands use `try_send` into the existing 64-slot mailbox, the pending take
  is one slot, tally and state snapshots are latest-wins `SwapCell`s, and `switcher.keyer`
  events go out on the existing drop-oldest event stream.
- **#5 / #8:** fills stay NV12 or NV12+A. Key math enters at ADR-C007's points, and alpha is
  premultiplied once.
- **#9:** keyers and the pre-DSK Clean call are admission-accounted. An in-flight keyer ramp is
  program-affecting and is never shed mid-flight.
- `TileParams` grows by one `vec4<f32>` (key: clip, gain, flags, key layer) and one `vec4<u32>`
  (mask). Every kernel change goes through the CPU oracle and the GPU parity gate.
- The `[switcher]` `extra` round-trip map loses `keyers` and `downstream_keyers` to typed fields.
  A document that already holds untyped keys there is validated against the new schema at boot.
  Tests pin the error for an invalid one.
- Owed by the build slices, not by this ADR: the chaos tests (command flood, fill death
  mid-ramp, GPU fallback, wedged Clean encoder) and the mda001 protection-bound run with 2 DSKs
  and 1 flying luma USK on air and a wipe every 5 s for 10 minutes (see the design note §11).
- Alpha media (media-player ProRes 4444 / qtrle / PNG decode to NV12+A) and NDI alpha are their
  own slice (K6). Until then, DSK fills with alpha are graphics sources, and other sources key by
  luma or through a fill and key pair.
