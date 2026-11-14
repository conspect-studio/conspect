# ADR-0112: DIP, WIPE and STINGER on the M/E, rendered in the one Program drive

- **Status:** Accepted
- **Area:** Core engine / Compositor / Web/API
- **Date:** 2026-11-14
- **Source:** operator request (14 Nov: "full production switching and routing, including fades, wipes"), lane ship-1111/transitions
- **Refines:** [ADR-0104](ADR-0104.md) §3 (AUTO mix in the Program drive), ADR-0055 §3 (transition taxonomy)

## Context

CUT and AUTO MIX work: the switcher frame hook draws the incoming source as one
extra full-canvas tile above the Program cell, at `elapsed / duration`, in the
same composite pass (ADR-0104 §3). `TransitionDescriptor` already has `Dip`,
`Wipe` and `Stinger`, and the value machine validates them, but nothing renders
them, the control API has no way to select them and the desk shows WIPE and DIP
disabled. Every output on Program receives what the Program bus shows, so a
rendered transition reaches HLS/SRT/MoQ with no routing change.

## Decision

1. **Selection is machine state.** A new verb `SetTransition(descriptor)` goes
   through the same bounded mailbox as every other verb (`try_send`, at most 16
   applied per tick). It is refused during a take (`InFlight`, the descriptor is
   frozen for the whole flight) and refused with `unsupported-transition` for a
   kind or option the drive does not render. `AUTO(rate)` keeps its own rate and
   uses the selected kind (`SwitcherState::transition_with_rate`); a selected
   `cut` makes AUTO a MIX, as before. The AUTO's descriptor is the flight's
   own (`SwitcherState::auto_with` / `active_transition`), cleared when the
   take completes, is cut or aborted: the selection itself never changes by
   an AUTO, and the drive renders the flight's descriptor.
2. **DIP to a colour.** `Dip` gains `color` (`"#rrggbb"`, default black);
   `dip_source` becomes optional and a dip through a source is refused until it
   is rendered. With `t = elapsed / duration` and the exact rational switch
   point `s` (default 1/2, strictly inside (0, 1), rate at least 2 frames): for
   `t < s` the colour is drawn over the outgoing picture at `t / s`; from `s` on
   the colour is opaque and the incoming picture is drawn over it at
   `(t - s) / (1 - s)`. The phase is decided in integers
   (`elapsed * den < num * duration`); only the two weights are floats
   (`switcher::kernel::dip_weights`). The colour is a 2x2 NV12 solid coded into
   the canvas colour space once per distinct colour (the `solid` source path)
   and scaled to the full canvas by the compositor, so a DIP is two ordinary
   full-canvas tiles in the existing pass: no compositor change, colour
   pipeline order unchanged (invariant #8). The incoming picture is drawn only
   from a fresh frame (ADR-0104 SW2-02); the terminal tick swaps the buses
   exactly as MIX does.
3. **WIPE** is one extra full-canvas draw of the incoming picture with a
   per-pixel mask, in the same pass, on the CPU reference and the GPU path.
   `Tile` carries an optional `mask::WipeMask` (shape, progress-derived edge
   radius, softness, origin, reverse, canvas size); its coverage is a
   closed-form distance field evaluated per canvas pixel, identical in Rust
   (`WipeMask::coverage`) and WGSL (`wipe_coverage`), and multiplies into the
   tile alpha at the one premultiply step (colour order unchanged, inv #8).
   The GPU carries the mask in the spare `.z/.w` lanes of `TileParams` (no
   struct growth, the ADR-0058 alpha-layer precedent). Patterns: horizontal
   (`linear_edge`), `vertical`, `diagonal`, `box` (centre out) and `circle`
   (iris, round on the canvas); `reverse` mirrors the field (other side, or
   closing in from the outside); `softness` and `border_width` are fractions
   of the canvas height in `[0, 1]`; `position_xy` in `[0, 1]` places the
   box/circle origin. An optional `border_color` band is the same 2x2 colour
   fill drawn through the edge grown by `border_width` times one minus the
   edge's own coverage (smooth on both sides of a soft edge), drawn above the
   incoming picture so it never depends on the incoming alpha: the band never
   covers the revealed interior, and it is
   drawn only while the incoming frame is fresh, so a stale or missing source
   shows the outgoing picture, never a colour fill; the travel is lengthened by the soft half-width and the border so
   progress 0 shows nothing of either and progress 1 shows only the incoming
   picture, and both endpoints are pinned exactly (coverage 0 / 1 for the whole
   canvas, whatever the origin or rounding). The signed distance to the edge is
   measured in canvas-height units for every shape, the box's sides included,
   so a soft edge or border band has the same pixel width on every side. `border_fill_source` and `flip_flop` are refused until rendered.
   Edge pixels may differ between CPU and GPU by f32 rounding; the GPU test
   bounds that to 1 % of luma samples by more than 3 code values.
4. **Control API, persistence and desk** (third PR): action `set_transition`
   on `/api/v1/switcher/plan` and `/commands/set_transition` carries the
   descriptor in `transition`. It has the same BOLA, Idempotency-Key, bounded
   `try_send` (`engine-busy`) and 202-means-queued rules as `auto`. Every
   other action that carries a `transition` is refused, and `set_transition`
   forbids `source`/`input`/`duration_frames`. A source named inside the
   descriptor (`dip_source`, `border_fill_source`, `player`) is authorized
   before the body is parsed. The route answers with typed 422s:
   `unsupported-transition` for what the drive does not render (stinger,
   DVE, a dip through a source, `flip_flop`, `border_fill_source`), and
   `invalid-transition` for a shape or number the machine refuses, an
   unknown field or a field of another kind (the descriptor and its switch
   point deny unknown fields, in the API and in a persisted config alike), or
   a rate above 1000 frames, including a number JSON cannot carry (`1e400`,
   `NaN`) inside `transition`; other malformed bodies keep the body
   extractor's rejection. A selection while a take is in flight is refused
   before enqueue, and the engine re-checks every rule at the tick. A retry
   with an already-reserved Idempotency-Key is answered with its original
   202 after authorization and static validation, before the transient
   bus/active-take/scene checks; a fresh key passes every check first. The
   checks are shared, not copied: `TransitionDescriptor::is_valid` /
   `is_rendered`. `GET /api/v1/switcher/me1` reports the engine snapshot's
   `transition`. The OpenAPI document publishes the descriptor as a
   `oneOf` tagged on `kind` (`TransitionDescriptorDoc`, a serde-equivalent
   mirror in `multiview-control`, pinned to the core type by a round-trip
   test); the route still parses the raw JSON so a malformed body is a typed
   422, and the web client uses the generated type. The snapshot compares the descriptor every committed tick
   and clones it only on a changed tick. The selection is machine state, so
   it is persisted as `switcher.mix_effects[0].transition`. Config
   validation applies the same two rules. The persist relay writes the
   selection off the clock, under the config-mutation lock, after the
   engine applied it and only while no take is in flight. The snapshot's
   `transition` is the selection, never the AUTO's flight descriptor, so a
   completed or interrupted AUTO persists the selection with its own rate.
   The first-slice
   launcher sets the persisted selection on the machine before the clock
   starts. The desk enables MIX/WIPE/DIP (pressed state from GET). Its
   parameter draft starts from the confirmed selection in GET; a later
   engine change updates every field without an unsaved local edit, and
   applying keeps the selection's fields the desk does not expose
   (`position_xy`, `switch_point`). A band without a `border_color` stays
   colourless until the operator sets the border colour or width. It has a
   picker with the five patterns as SVG thumbnails, reverse, softness,
   border width/colour and a dip colour, a 50 % preview thumbnail, and an
   AUTO title naming the selected kind. AUTO still sends only its
   `duration_frames`. T-bar remains out of scope.
5. **STINGER** (stinger PR A). A stinger is a media-player clip rolled over
   Program while the take happens beneath it. `TransitionDescriptor::Stinger`
   is rendered exactly when it is valid: `trigger_point + mix_rate <=
   clip_duration` and every count (`pre_roll`, `clip_duration`,
   `trigger_point`, `mix_rate`) at most `MAX_STINGER_FRAMES` = 1000 (the AUTO
   bound). Its timing is integer frames from the AUTO's tick
   (`StingerTiming`, elapsed `e`):
   - **Pre-roll decision.** The AUTO's tick cues and plays the player, and
     the clip window opens `pre_roll` frames later: `e` in `[0, pre_roll)` is
     the player's run-up, Program unchanged. We chose this over cueing at
     selection time because a selection is persisted and replayed at boot,
     while a cue parks a shared player that other operators may be using;
     rolling at AUTO keeps one operator action, one roll. The clip is drawn
     only from a fresh frame, so `pre_roll` is the operator's budget for the
     player's first frame to land; a late first frame only shortens the
     visible clip, never the take.
   - **Render.** The clip window is `[pre_roll, pre_roll + clip_duration)`;
     the player's fresh frame is drawn full-canvas above every other tile in
     the same composite pass, through its own alpha plane when it carries one
     (the compositor's per-pixel tile alpha, ADR-0113), else opaque (an
     opaque clip covers the frame). The underlying take is the existing MIX
     overlay over the mix window, clip frames `[trigger_point, trigger_point +
     mix_rate)`: mix frame `k` draws the incoming picture at `(k + 1) /
     mix_rate`, so `mix_rate = 1` is a CUT on `trigger_point` and the last mix
     frame is fully incoming. At most three full-canvas tiles above the cell
     (outgoing cell, incoming, clip); the scratch pool is sized once for them.
   - **Swap and end.** The take completes (Program/Preview swap, outcome
     published, completion counted once) on `swap_at = pre_roll +
     trigger_point + mix_rate`; the flight then stays in flight through the
     clip tail and ends silently at `pre_roll + clip_duration`. The SW2-02
     rule (never complete onto a source without a fresh frame) is checked at
     `swap_at`. A CUT before the swap completes the take at once; a CUT or
     T-bar in the clip tail ends the tail and is a new take. The machine's
     flight carries a `swapped` flag; `take_window()` replaces
     `auto_window()` for the take deadline.
   - **The player never paces the take.** A stale, missing or unregistered
     player frame is skipped (never a slate on Program) and the take
     completes on schedule.
   - **Roll.** `PlayerRollSender` (engine, `player_roll.rs`) is set on the
     tick driver at boot by the CLI, beside `set_prime_stores`. On the
     AUTO's tick the driver calls `roll(player)`: a lookup among the ids
     registered at boot and one `try_send` of an index on a 4-slot
     `sync_channel` — no allocation, no lock, no wait; a full or closed
     queue drops and counts. A dedicated `stinger-roll` thread runs the CLI
     callback, which submits `Cue { frame: None }` then `Play` to the
     player's existing bounded transport mailbox (ADR-0057/0097); a callback
     panic is contained per roll. A worker that blocks or panics cannot
     reach the clock.
   - **Validation.** The control API authorizes `player` with the other
     descriptor references (BOLA, before parsing), then refuses a player
     that is not a configured media player with a typed 422
     `unknown-media-player` on plan and command; an out-of-range stinger is
     `invalid-transition`. Config validation refuses a persisted stinger
     whose player is not in `media_players`. `dip_source`,
     `border_fill_source`, `flip_flop` and DVE stay refused.
   - **Desk.** A STING key beside MIX/WIPE/DIP; the "Transition parameters"
     popover gains a Stinger group (player from `GET /api/v1/media/players`,
     pre-roll, clip, trigger, mix). STING is disabled with a reason when no
     player is configured or the numbers are out of range. AUTO's title is
     "AUTO STINGER over N frames" with N = pre-roll + clip.
   - **PR B** adds alpha decode for clip assets (ProRes 4444 / VP9 alpha /
     PNG sequence into the alpha plane). Until then a decoded clip is opaque
     and covers Program for its window.
   - **Audio.** Program audio follows the take's mix (the existing AUTO
     crossfade over the mix window); the clip's own audio is not mixed in by
     this PR.

## Invariants

- **#1:** a DIP or a bordered WIPE adds at most one tile to a dissolve's
  tick (two full-canvas tiles instead of one); the mask is a handful of float
  operations per pixel of the masked tile, no allocation. The scratch pool is sized for both on the first
  transition and never grows after; the colour fill is built once per colour
  change, never per tick (`transition_fill_builds`, pinned by
  `tests/switcher_dip.rs`). Nothing waits.
- **#10:** `SetTransition` is one more verb in the 64-slot mailbox; a flood of
  selections and AUTOs cannot move the clock (chaos test
  `a_transition_command_flood_never_moves_the_output_clock`: 600 ticks, one
  frame per tick, zero repeats, full mailbox met, at most 16 commands a tick).
- **#1/#10 (stinger, §5):** the clip is one more full-canvas tile; the player
  roll is one `try_send` on a 4-slot queue to an off-clock worker. Chaos test
  `tests/switcher_stinger.rs::a_stinger_command_flood_never_moves_the_output_clock`
  floods stinger selections/AUTOs/CUTs for 600 ticks with a roll worker that
  never returns: one frame per tick, zero repeats, the roll queue fills and
  drops. `a_blocking_or_panicking_roll_never_affects_the_tick` pins the
  per-frame output with a blocked and a panicking worker.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| A dip as two chained AUTOs through a colour source | Two operations, two outcomes, a visible bus state in the middle and a colour source in every scene. |
| A per-transition full-canvas solid image | 8 MB per colour at 4K; the 2x2 scaled solid is the same picture for 6 bytes of planes. |
| A separate mask pass / second canvas for WIPE | Doubles the per-tick composite work; the mask is one more term in the existing fold. |

## Consequences

- `TransitionDescriptor::Dip.dip_source` is now `Option`; documents that named
  a source still parse, and are refused at selection until a source dip is
  rendered.
- Out of scope: T-bar on the desk, DVE push/squeeze, dip through a source,
  stinger clip alpha decode (§5, PR B) and stinger clip audio.
