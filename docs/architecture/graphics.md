# Graphics (CG) — model and efficiency rules

Graphics is Conspect's character generator / graphics playout area (Vizrt,
Ross XPression, CasparCG and Chyron call it the same thing). Weather is the
first template family; lower thirds, tickers, full-screen pages and RSS
news plug into the same model.

## Model

```text
data sources ──▶ templates ──▶ items ──▶ channel rundown ──▶ switcher source
[graphics.        built-in      [graphics.   [graphics.          gfx-channel-1 (PGM)
 data_sources]    registry       items]       channels]           gfx-channel-1-preview (PVW)
```

| Layer | Where | Notes |
|---|---|---|
| Data source | `[[graphics.data_sources]]`, `kind = "bom_weather"` | Typed provider (`DataProviderKind`). Each kind yields a `DataKind` (`weather`). Adding RSS = one enum variant + `DataKind::News` + a management-plane fetcher; binding validation follows from the template's declared `data_kind`. Legacy `[weather] locations` still load and are effective `bom_weather` sources. |
| Template | `multiview_config::graphics::TEMPLATES` | Built-in registry: id (`family.variant`), declared data kind, fields and style options. `weather.seven_day`, `weather.today`. |
| Item | `[[graphics.items]]` | A prepared graphic = template + data binding (`data_source`, optional `key`) + style (`background`, `accent`, `theme`). |
| Channel | `[[graphics.channels]]` | Persisted rundown (ordered item ids) and transition preset (`cut`/`mix`, frames). `gfx-channel-1` always exists. PVW/PGM state is runtime only — a restart comes up clear. |
| Source | `kind = "graphics_channel"` (`bus = program\|preview`) or `kind = "graphic"` (one item) | Ordinary synthetic sources: layouts, previews and program use them like any other source. The program bus is transparent when clear, so you can layer it in a cell above video. |

Persistence goes through `boot_model::commit_desired` (desired.json). Graphics
mutations also write the `bom_weather` sources into `[weather] locations`, so a
rollback to a pre-Graphics build keeps the locations. `/api/v1/weather/locations`
is a thin alias over the `bom_weather` data sources.

Playout API (`/api/v1/graphics/channels/{id}`): `GET` status, `GET/PUT
/rundown`, `POST /cue {item}`, `/take`, `/clear`, `/next` (an optional one-shot
`transition` is accepted by take and clear). TAKE moves PVW to PGM and
auto-cues the next rundown entry. Every id is authorized per object (the
channel and each item it touches). Mutations are audited, and playout changes
publish `graphics.channel_state` on the `switcher` topic. Deleting an item
that is on air cuts the program to clear.

## Efficiency rules

1. **Render on change, off the clock.** Each graphics source renders on its
   own generator thread (a peer of a decode thread). It never renders on the
   output clock or the bake/encode thread. An item renders only when its
   definition or its bound data revision changes. Each tick otherwise
   republishes the same `Arc<Nv12Image>`. Data fetch and parse run on the
   management plane (the BOM poller), never in a generator.
2. **Opacity-only transitions on the clock.** A mix is a pure function of the
   generator's tick counter: `multiview_graphics::mix_at(from, to, kind,
   frames, ticks_since_change)`. Integer maths only, so it is deterministic.
   A settled frame is the prepared `Arc` itself (zero work). A mid-fade tick
   applies opacity once to one prepared image: a 256-entry LUT over the alpha
   plane plus a plane copy, memoized while the opacity is unchanged. Item →
   item mixes dip through clear (out over the first half, in over the second),
   so two full images are never blended per frame. Nothing re-renders during a
   transition.
3. **Bounded caches.** A channel generator keeps at most the transition's
   from/to items plus one transparent clear frame. It prunes everything else
   every tick. Alpha planes come from a one-slot `AlphaPlanePool`.
4. **Never block.** Generators read the hub and the weather store with
   `try_read` and hold their last frame on contention. The control plane
   mutates the hub under a short mutex and publishes immutable snapshots.

### Why per-generator opacity rather than per-cell opacity

The layout cell's opacity is static configuration. Animating it would need a
layout apply per frame, which is a control-plane round trip on the clock path.
Applying the ramp in the source generator keeps the change local to one
thread. It works in every layout, preview and program that uses the source,
and it costs O(1) plus at most one LUT pass per tick.

### Alpha and the compositor

A transparent graphic (the clear channel, a fade, or a `transparent`-style
item) carries an NV12 + straight-alpha plane. The GPU compositor rejects alpha
tiles, and that frame is composited on the CPU path (the existing behaviour for
alpha media). An opaque gradient item settles without an alpha plane. For a
full-screen opaque graphic, use a `gradient` item on a `graphic` source.
