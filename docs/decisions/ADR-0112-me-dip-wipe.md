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
   `cut` makes AUTO a MIX, as before.
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
4. **Control API and desk** (third PR): `/api/v1/switcher/plan` and
   `/commands/set_transition` carry the descriptor, with the same BOLA,
   idempotency and 202-means-queued rules as `auto`; `GET /api/v1/switcher/me1`
   reports the selected transition; the desk enables MIX/WIPE/DIP with a pattern
   picker and the AUTO rate.

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
- **FFI safety.** Scratch frames are `multiview_ffmpeg::frame::ScratchFrame`s,
  which only the checked `alloc_video` (null `AVFrame`, dimension cap, CPU
  format, `av_frame_get_buffer` result) creates — never the unchecked
  `Video::new`/`Video::empty`. `Scaler::run_into` reuses only a
  `ScratchFrame` (its frame is never exposed mutably, so its geometry cannot
  be raised over a small allocation) and re-validates every destination
  plane against the frame's own buffers, and that the buffers are writable,
  before libswscale or the identity copy writes. Decoded
  alpha is read through `frame::plane_rows`, which validates the plane,
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
  destination is an error, a null frame allocation never becomes a
  `ScratchFrame`) and `tests/scale_resample.rs` (bottom-up identity
  `run_into`). Player path
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
