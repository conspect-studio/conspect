# ADR-0112: DIP and WIPE on the M/E, rendered in the one Program drive

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

### 5. Stinger media — PR B: alpha decode (#391 subset)

A media player whose asset has an alpha channel publishes NV12 plus a
straight, full-resolution R8 alpha plane on every frame (ADR-0058 NV12+A).
This is a general media-player output, not stinger-private: the plane is
the existing `Nv12Image::alpha_plane` in `multiview-compositor`, so the
stinger (PR A) and keyers K6 (ADR-0113 `fill_alpha`) consume the same
tile. No switcher or drive code changes.

- **Shared types.** The pure "where is the alpha sample" description and
  the copy/rescale routine live in `multiview-core::alpha`
  (`AlphaLayout`, `AlphaDepth`, `extract_alpha`; no libav, unit-tested in
  the default build). `multiview-ffmpeg::alpha` maps libav pixel formats
  to a layout (`alpha_layout`) and `DecodedVideoFrame::alpha` carries the
  decoder's own pre-NV12 frame (a moved `AVFrame` reference, not a copy)
  when the format has alpha, `None` otherwise.
- **Formats.** Planar `yuva420p/422p/444p` at 8/9/10/12/16 bits (ProRes
  4444 decodes to `yuva444p10le`/`yuva444p12le`, `libvpx-vp9` to
  `yuva420p`), `gbrap*`, packed `argb/rgba/abgr/bgra` (`qtrle` is `argb`,
  PNG `rgba`, TGA `bgra`), `rgba64/bgra64` (16-bit PNG) and `ya8/ya16`.
  Deeper samples round to nearest 8-bit (`(v*255 + max/2)/max`). VP9 with
  alpha (WebM `alpha_mode=1`) opens the BSD-licensed `libvpx-vp9` decoder
  for player sources only, because libav's native `vp9` drops the alpha
  side stream; if it is not linked or fails to open the asset plays opaque
  with a warning. Not covered: paletted `pal8` (PNG/GIF palette alpha),
  HEVC alpha (libav decodes it opaque; ADR-0058 §3 rejects it at import),
  and alpha in non-player sources (ADR-0058 §5: NDI and ordinary video
  stay opaque).
- **Colour.** Unchanged: the NV12 conversion is the same libswscale
  same-size conversion followed by the same `TileScaler`, so matrix, range
  and order are exactly today's. Alpha is coverage, so it is never
  range-mapped or linearized; the compositor premultiplies once in linear
  light (ADR-0058 §2). Every supported decoder emits straight alpha and
  none marks premultiplied output, so nothing is un-premultiplied; a
  premultiplied-authored asset must be normalized at import (ADR-0057).
- **Scale.** Alpha uses the luma's scaler geometry: decoded size to tile
  size with the same Lanczos + accurate-rounding filter, `GRAY8` to `GRAY8`
  (no colour step). Same-size frames copy straight into the pooled plane.
- **Memory.** One pooled plane per frame from the bounded
  `AlphaPlanePool` (`attach_with` writes it in place); the decoded-size and
  tile-size scratch frames and the alpha scaler are reused across frames
  (`Scaler::run_into`) and rebuilt only on a geometry change, so the steady
  state does not grow the heap. Opaque assets build none of it. Returning
  a plane never waits: the frame's `Drop` (possibly on the output clock
  thread) only `try_lock`s the pool and frees the plane on contention, and
  `attach_with` holds the lock only to take the spare, never across the
  copy/scale/extract (invariants #1/#10). The same rule now covers the CPU
  compositor's output-plane scratch pool (`PlaneReturn`/`ScratchPool`, a
  pre-existing blocking path fixed in this PR): a canvas's `Drop` only
  `try_lock`s the Y/UV spare caches and frees a plane on contention, and the
  clock's lease only `try_lock`s and allocates a fresh plane on contention
  (both counted; the caches never grow past their cap), so no frame holder
  can stall the output clock.
- **FFI safety.** Every `ScratchFrame` has all its owned buffer bytes (rows,
  row padding, tail) initialised once at allocation, since
  `av_frame_get_buffer` storage is not zeroed (review PB-12); plain
  `alloc_video` frames (e.g. `Scaler::run`'s per-frame output) are not
  filled, because this crate never reads them in Rust (review PB-16). `plane_rows` never yields
  more than a row's active bytes (`av_image_get_linesize`), so a decoder
  frame's unwritten stride padding is never a Rust slice (review PB-13). Scratch frames are `multiview_ffmpeg::frame::ScratchFrame`s,
  which only the checked `alloc_video` (null `AVFrame`, dimension cap, CPU
  format, `av_frame_get_buffer` result) creates — never the unchecked
  `Video::new`/`Video::empty`. `Scaler::run_into` reuses only a
  `ScratchFrame` (its frame is never exposed mutably, so its geometry cannot
  be raised over a small allocation) and re-validates every destination
  plane against the frame's own buffers, and that the buffers are writable,
  before libswscale or the identity copy writes. Both `Scaler::run` and
  `run_into` validate every source plane the same way before libswscale
  or `av_frame_copy` (both identity paths) reads it (`frame::check_source`:
  row bytes from the format and width, subsampled plane height, signed
  linesize, every row inside one of the frame's own refcounted buffers; a
  frame without such buffers is an error), so a source whose geometry was
  raised over a small allocation is an error, not an out-of-bounds read.
  Before `sws_scale` (not the identity copies, which read only active
  bytes) both sides must also have the 16 bytes `libavutil/frame.h` says
  libswscale may read beyond a plane, after the plane's highest active
  byte (the last row, or row 0 of a bottom-up plane) in the same owning
  buffer (`frame::check_swscale_source`/`check_swscale_destination`;
  checked even for `alloc_video` outputs, not assumed), so an exact-size
  allocation whose padding became active rows is an error. The buffer scan
  walks `buf[]`/`extended_buf` in place, allocation-free.
  No public path builds a Rust slice over a caller-supplied `Video`, whose
  geometry the safe `Video::set_width`/`set_height`/`set_format` could widen
  over unwritten padding inside its allocation (review PB-14), nor over a
  libav copy of one (review PB-15): `Scaler::run_into` takes its *source*
  as a `ScratchFrame` too, so a `ScratchFrame`'s bytes come only from the
  zero fill, checked `plane_mut` writes and `run_into` from another
  `ScratchFrame` (its documented provenance invariant); `Scaler::run`
  still takes any `&Video` but returns a plain `Video`, which no safe API
  turns into a `ScratchFrame` or `AlphaSource`; `plane_rows` is
  crate-private and only sees frames with trusted metadata, and
  `alpha::AlphaSource` has private fields — it is built only by the decoder
  in this crate (its untouched output frame) or from a `ScratchFrame`
  (`AlphaSource::from_checked`), and hands out only `&Video`.
  Decoded alpha is read through `frame::plane_rows`, which validates the plane,
  its signed linesize, its subsampled height and that every row lies in
  one of the frame's own buffers before yielding single-row slices, so a
  bottom-up (negative linesize) frame copies in display order instead of
  forming an out-of-bounds slice.
- **Tests.** Pure: `multiview-core` `alpha::tests` (odd sizes, padded
  strides, 10/16-bit rescale, packed channels, bad input as errors).
  Compositor: `tests/nv12_alpha.rs` pool reuse with no growth, and
  `pipeline::alpha_pool_tests` (dropping an alpha frame while another thread
  holds the pool lock completes and frees the plane), and
  `pipeline::plane_return_tests` (a canvas drop and a plane lease each
  complete while another thread holds the Y or UV cache lock). FFI:
  `multiview-ffmpeg` `frame::tests`, `alpha::tests` (negative linesize,
  mismatched layout), `scale::run_into_tests` (an undersized or shared
  destination is an error, an undersized or unbuffered source is an error
  in the conversion and identity paths of `run`, a source or destination
  without the 16-byte swscale padding — exact-end or 15-byte tail,
  top-down or bottom-up — is refused before `sws_scale` while a padded or
  normally allocated one converts, a bottom-up source still converts, a
  null frame allocation never becomes a `ScratchFrame`, a widened decoder
  frame through `run` stays a plain `Video`), `compile_fail` doctests on
  `Scaler::run_into` / `Scaler::run` (a `Video` source, undersized,
  unbuffered or widened, does not compile in `run_into`; `run`'s output
  cannot become an `AlphaSource` or `ScratchFrame`) and
  `tests/scale_resample.rs` (`ScratchFrame`-sourced `run_into` reuse,
  bottom-up identity copy). Player path
  (ffmpeg): `pipeline::media_alpha::tests` (yuva → NV12+A, rgba/argb/bgra
  → NV12+A, odd-size scaling, steady-state reuse, an unallocatable scratch
  frame as an error, the no-alpha-decoder warning) and
  `pipeline::media_alpha::player_tests`, which encode tiny qtrle, ProRes
  4444, PNG-in-MOV and VP9-alpha clips with the `ffmpeg` CLI at test time
  (printing `SKIP` when an encoder is missing), play them through the real
  player executor and check the alpha at band centres, plus an opaque
  MPEG-2 asset that publishes no alpha plane.

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
- Out of scope: T-bar on the desk, DVE push/squeeze, stinger rendering (the API
  may accept and validate a stinger later), dip through a source.
