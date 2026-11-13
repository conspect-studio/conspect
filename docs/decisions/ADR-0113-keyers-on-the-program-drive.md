# ADR-0113: Upstream and downstream keyers in the Program drive: one compose pass, per-tile key alpha, tie/cut/auto, exact scene mixing, Clean off the clock, admission reserved at definition

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

1. **Keys render in the Program drive's one compose pass. There is no second compositor on the
   clock and no overlay bake.** Each keyer is one tile. The per-tick tile list is, bottom to top:
   the Program cell (outgoing background), the transition tiles of lane `transitions` (AUTO, DIP
   or WIPE incoming background and the dip colour), the upstream keyers (USK) in ascending `z`,
   then the downstream keyers (DSK) in ascending `z`. A reserved slot follows for the FTB cover,
   which is not part of this ADR. A keyer that is off and not ramping adds no tile and costs no
   compose work.

2. **Exact scene mixing: two accumulators when a transition changes the key set.** A background
   transition mixes two *scenes*: the outgoing scene (background A and its on-air USKs) and the
   incoming scene (background B and the USKs on after the transition). The required picture is
   `merge(scene_out, scene_in)`:
   - MIX: `(1 − p)·out + p·in`.
   - WIPE: `(1 − m)·out + m·in`, where `m` is the wipe mask per pixel.
   - DIP: `lerp(out, colour, d)`, then `lerp(colour, in, i)`, using `dip_weights`.

   A flat `over` of a key at weight `p` above a mixed background is not this: an opaque incoming
   key at `p = 0.5` would leave `0.25·A + 0.25·B` under it instead of `0.5·A`. Both kernels
   therefore get a **group** per tile and a second per-pixel accumulator:
   - Find the highest *changing* layer: the background if it changes, or any armed USK.
   - Every layer at or below it is folded into the accumulator of each scene it belongs to. A
     layer in both scenes (shared background in a key-only transition, an unarmed on-air USK
     below a changing one) is folded into both.
   - The accumulators are merged with the transition's merge function right after that layer.
   - Every layer above it (unarmed on-air USKs above all changing layers, every DSK) is folded
     once onto the merged result. This is exact because `over` is linear in what lies beneath.

   When only the background changes and B is opaque, this reduces to today's
   incoming-tile-at-weight render, so lane `transitions` keeps its path. The second accumulator
   exists only while a key-changing transition is in flight. On the CPU it is a pooled band
   buffer sized at attach; on the GPU it is a register. Tied DSKs ride the merge weight or mask
   as a plain `over` of the DSK tile at that weight (a DSK is defined over the M/E output).

3. **Per-tile coverage, computed in the kernel front half (ADR-0056 §4 and ADR-C007 domains).**
   Each source's alpha is used once:

   `a = opacity × weight × mask × fill_alpha × matte`

   - `fill_alpha`: the fill's NV12+A plane if present, else 1. This is the factor both kernels
     already apply (`pipeline.rs:1401`, `:1680`, `composite.wgsl:151`).
   - `matte` depends on the key type:
     - `linear` with no key source: 1. The fill's own alpha already shapes it, and an opaque fill
       becomes a box (picture in picture).
     - `linear` with a key source: the key source's post-range-expand Y, clamped to `[0, 1]` (a
       fill and key pair).
     - `luma`: `clamp((y_norm − clip) × gain, 0, 1)`, then optional `invert`. `y_norm` is the
       post-range-expand Y of the key source, or of the fill when there is no key source.
     - `chroma`: ADR-C007's linear-RGB, tile-gamut distance. It is slice K7, and validation
       refuses it until then.
   - `mask`: a rectangle in fill-source pixels, 0 outside it. Because it is in source space, it
     travels with a flying key.
   - `weight`: the keyer's on-air weight. A key that rides a wipe as a group member gets the
     wipe through the merge (item 2), not through its own tile.

   The color pipeline order is untouched (#8), and alpha is premultiplied once at the existing
   step.
   - **Sampling:** fill and key are sampled independently from the same integer destination
     offset, each with the existing nearest-neighbour mapping: `sx = floor(ddx × fill_w / dst_w)`
     and `kx = floor(ddx × key_w / dst_w)`. A key of another size never inherits the fill's
     rounding.
   - **GPU:** the key's Y plane is uploaded into `y_planes` at its own layer. A new storage array
     `key_params` (binding 6, parallel to `tiles`) carries the key's own size, layer, range
     scale and offset, clip, gain, kind, invert, mask and group, so nothing is borrowed from the
     fill's `TileParams`.
   - **Shaped (premultiplied) fills are refused** with a 422 at validation until a slice builds
     un-premultiply. This amends ADR-0056 §3.

4. **The flying (PiP) key is a placement of any keyer** (amends ADR-0056's `dve_pip` key type).
   `fly = {x, y, w, h}` is even-aligned canvas pixels inside the canvas, at least 16 x 16. By
   default it is the full canvas. The descriptor's `DvePip { rect }` reads as `linear` with
   `fly = rect`. Borders, shadows and sub-pixel or bilinear scaling come with SW-25 (#51).

5. **USKs ride the background transition; DSKs survive it.**
   - USK: `armed` is the next-transition set `{background, keys}` (`SwitcherState::arm`). Armed
     keys change state with the next CUT or transition through item 2. A key-only transition uses
     the selected type and rate, and DIP draws as MIX when the background does not change. An
     aborted AUTO restores every armed key.
   - DSK: CUT flips at the next tick. AUTO ramps the weight over its own `rate` (1..=300 frames).
     A tied DSK changes with the next background transition.
   - Progress is `f(tick.index − start)` in integer frames (ADR-T015). No wall clock is used.

6. **Going on needs a fresh fill and key (the ADR-0104 gate).** The tick a keyer starts going on,
   both stores must be `state_at(tick.pts).is_live()`. Otherwise the command waits in that
   keyer's own one-slot pending entry (fixed at attach, so a held key never blocks a background
   CUT) for at most 50 ticks, then is refused with a typed reason. A second command for that
   keyer while one is held is refused. A transition that carries keys going on is gated on all of
   them and on the incoming background. Going off is never gated.

7. **Fill and key are one unit for loss (ADR-R011 §3).**
   - Each keyer holds a **last-good pair**: two `Arc` slots, fixed at attach. They are updated
     together, and only on a tick where both reads are live. A slate is never stored.
   - When either source is at or past `loss_trigger` (default `reconnecting`):
     - `drop_to_off_air` (default) takes the keyer off at that tick. It emits `switcher.keyer`
       with `reason = "source-lost"`, and the keyer stays off.
     - `hold_last_good` draws the stored pair, frozen together, and marks the keyer `stale`.
   - Between a non-live read and the trigger, the stored pair is drawn, so one plane never
     advances while the other is held.
   - The pair is released when the keyer goes off.

8. **Pre-DSK Clean is composed off the clock, on the `routing-aux` thread.**
   - While a DSK is defined and an output selects Clean, Clean becomes an aux-composed bus.
   - Each tick the hook writes a fixed-size, `Copy` render descriptor into a preallocated
     4-slot seqlock ring (no allocation, wait-free for the writer). The descriptor holds store
     indices, weights, rects, key parameters, group and merge parameters for every non-DSK tile.
   - The aux thread reads tick T's descriptor and the stores at `T.pts`: the same frames, by the
     rule in output-routing.md §2.3. It composes with the run backend at `nice +10`.
   - Under load the aux drop and hold rules apply (output-routing.md §2.3, §4.4): Clean holds
     last-good and is counted.
   - With no DSK defined, Clean stays the zero-cost Program alias of today.
   - The Program compose and publish never include Clean work. GPU queue sharing is covered by
     the existing slice-3 gate.

9. **Admission is reserved when a keyer is defined, at its worst case (invariant #9).**
   - Every *defined* keyer reserves its cost whether on air or not: off keyers keep their sources
     warm, and a reservation cannot be exceeded later by going on air, a transition, an edit or
     a boot.
   - Cost per keyer, in canvas folds:
     `fold × dst_area/canvas_area + u × (fill_px + key_px)/canvas_px`.
     - `fold` is 2 for USKs (folded twice during a key-changing transition) and 1 for DSKs.
     - `u` is the upload weight. Upload cost depends on source size, not fly size.
   - `switcher.keyer_budget` caps the sum. Hard limits: 4 USKs per M/E and 2 DSKs per program.
   - `keyer_set` that raises a cost is admitted against the remaining budget. Boot validation
     refuses a document whose keyers exceed it.
   - Clean reserves one aux canvas (`routing.max_aux_canvases`) at the routing take when the
     program has a DSK defined.
   - A fill or key that is not already running is charged as +1 decode under the patch panel's
     decode rule (ADR-0110 §10).
   - `u`, `fold` and the default budget come from the K1 bench and the mda001 run (item 12).
     Until that run passes, the default budget admits 1 DSK at full canvas and nothing else.
   - A per-frame GPU failure falls back to the CPU only if the reserved cost fits the CPU budget.
     Otherwise that frame is composed without keyer tiles (counted). Three consecutive failures
     take the keyers off with `reason = "gpu-fallback"`.
   - Keyers that compose on the GPU depend on #395 (a deadline on the GPU readback, which Program
     lacks today).

10. **API on the existing switcher routes; BOLA on every id.**
    - Verbs: `arm`, `keyer_cut`, `keyer_auto` (DSK), `keyer_tie` (DSK) and `keyer_set`. Each is a
      plan and command pair with an `Idempotency-Key`, a 202 and a `switcher.operation`
      outcome.
    - `keyer_set` is Class-1 at one tick. Changing the fill or key of a keyer on air needs
      `on_air = "allow"`, else 409 `on-air-protected`.
    - Fill and key name **sources only**, never mixer inputs (`in:<n>`). A patch therefore cannot
      substitute a source behind a keyer.
    - Every request needs `Control` on the M/E, `Program` scope, `Control` on the keyer object
      id (`me1/usk1`, `main/dsk1`) and `Read` on each named source, checked on plan and on
      command.
    - `GET /api/v1/switcher/{me}` lists only the keyer rows whose object id the caller may read
      (ADR-W025 row filtering). It then redacts unreadable source ids inside those rows and
      drops them from `tally`. Realtime `switcher.keyer` events follow the same rule.
    - Plan and 409 `capacity` answers give scoped callers only their own delta and a generic
      refusal. Totals and holders go only to unscoped readers, as ADR-0108 decision 8 does.
    - Commands share the 64-slot `try_send` mailbox (503 when full). There is no per-principal
      limit on the switcher routes. The shared idempotency store (4096 keys, no per-principal
      namespace) bounds memory but gives no fairness between principals; that is tracked in
      #394.

11. **Persistence, tally, desk.**
    - Keyer definitions are typed accepted state (`[[switcher.mix_effects.keyers]]`,
      `[[switcher.downstream_keyers]]`), written after the engine reports `applied`.
    - On-air, armed and ramp state is runtime only. The exception is `on_at_start` DSKs (the
      station bug), which go on through the fresh gate after boot.
    - Tally is derived from the composed tile list. `program` covers background, transition
      source and the fill and key of every contributing keyer; `preview` covers the PVW source
      and keys going on next. It is kept as store indices in fixed arrays and published only on
      change. A source used only as a key signal is PROGRAM-tallied.
    - The desk gets a keyer strip (BKGD, KEY 1–4, ON AIR lamps), DSK TIE/CUT/AUTO, a keyer
      panel, and KEY tally on wall tiles. Every disabled control states its reason.

12. **Acceptance on mda001 (after K5; run by the lead).** The Program protection bound
    (output-routing.md §10), measured over 10 minutes against a no-keyer baseline, at the
    admitted maximum budget:
    - DSK1 (alpha graphic) on air, DSK2 AUTO on and off every 10 s.
    - USK1 a flying luma key going on and off with a WIPE every 5 s (key-changing transitions).
    - Clean routed to an output.

    The bound: p50 within +1 ms, p99 within +2 ms, max within +5 ms and below 16.7 ms,
    `frames_repeated` 0, and an HLS PTS step of 3003. The default budget is raised only after
    this run passes.

## Rationale

- **One pass with scene groups is exact and cheap.** Per-pixel alpha already exists in both
  kernels. The group merge is a second accumulator and one lerp per pixel, and only during a
  key-changing transition. Pre-rendering each scene to a canvas would cost two extra composites
  and an NV12 generation every transition tick.
- **Clean off the clock** keeps Program's critical path to exactly the work Program shows. The
  aux thread already has drop, hold and admission rules and a gate for GPU sharing.
- **Reserving at definition** means no later event (Clean, transition, edit, boot) can push past
  the budget, so nothing ever has to be shed from Program.
- **Flying as placement**, **no on-air persistence** and **tally from the render list** are
  unchanged reasons: flying a keyed logo, no stale graphic after a restart, and no untallied
  contributor.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| DSKs on the off-thread overlay bake | Not tick-coordinated, drop-oldest, after the Program taps, no video fill (ADR-0056, verified on main). |
| Flat `over` of armed keys at the transition weight | Not exact: an opaque key at p = 0.5 leaves 0.25·A + 0.25·B under it (review finding). |
| Pre-render each scene to an intermediate canvas | Two extra composites and one NV12 generation per tick during every transition. The two-accumulator merge is exact at a fraction of the cost. |
| Pre-DSK Clean as a second backend call on the clock | Compose finishes before Program is published (`runtime.rs:1083-1091`), and the GPU readback has no deadline (#395). It would delay Program. |
| Admission when a keyer goes on air | Clean selection, key-changing transitions, edits and boot can raise the cost after admission, and an on-air key cannot be shed. |
| `in:<n>` as a keyer fill | A re-patch would change what is keyed without a keyer authorization. |
| `dve_pip` as its own key type | It forbids flying a keyed or alpha fill. |
| Shaped fills now, persisted on-air state | Wrong edges would be silent; a stale graphic could come up after a restart. |

## Consequences

- **#1:** keyer runtime, last-good pairs, pending slots, the descriptor ring and tally arrays are
  fixed-size and allocated at attach. The second accumulator is pooled. Store reads are
  lock-free. The existing per-tick `Vec<Tile>` (`drive.rs:645-652`) and the `LatestSlot` publish
  are today's baseline. K1–K3 add no new allocation per tick, measured with a counting
  allocator against that baseline. GPU keyers depend on #395.
- **#10:** commands use `try_send`. `switcher.keyer` events are emitted only on state changes,
  through the same bounded outcome path as `SwitcherOutcome`, and serialized and redacted in the
  control plane. Snapshots carry ramp descriptors (start, rate, direction), not per-tick weights,
  and are published only on change.
- **#5 / #8:** fills stay NV12 or NV12+A. Key math enters at ADR-C007's points, and alpha is
  premultiplied once.
- **#9:** reservation at definition (item 9), Clean as an admitted aux canvas, and the budget
  default gated by the mda001 run.
- Owed by the build slices: the chaos tests in the design note §11 (command flood, fill and key
  death mid-ramp, wedged event, snapshot and Clean consumers, slow and failing GPU frames) and
  the mda001 run of item 12.
- Alpha media and NDI alpha are slice K6. Until then, DSK fills with alpha are graphics sources,
  and other sources key by luma or as fill and key pairs.
