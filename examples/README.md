# Example configurations

Ready-to-adapt Multiview configs. They conform to the schema documented in
[`../docs/templates/layout-and-config.md`](../docs/templates/layout-and-config.md) (canvas →
layout → cells → overlays → outputs, config-as-code). `fps` is **always** a rational string
(never a float); the canvas working color space is the single source of truth for the pipeline.

| File | Layout | Sources | Notes |
|------|--------|---------|-------|
| [`2x2.toml`](2x2.toml) | 2×2 grid | 4 × built-in `test` | Self-contained; no network needed. |
| [`3x3.toml`](3x3.toml) | 3×3 grid | 9 × built-in `test` | Density demo. |
| [`1plus5.toml`](1plus5.toml) | 1 large + 5 small | 6 × built-in `test` | Asymmetric grid via `grid-template-areas`. |
| [`pip.toml`](pip.toml) | Picture-in-picture | 2 × built-in `test` | Absolute normalized rect overlap. |
| [`synthetic-sources.toml`](synthetic-sources.toml) | 2×2 grid | `bars` + `solid` + 2 × `clock` | The first-class in-process synthetic sources (ADR-0027); no network, no GPL. |
| [`public-streams-2x2.toml`](public-streams-2x2.toml) | 2×2 grid | Mixed source kinds | Synthetic + a sample clip + an example RTSP camera; fps/codec/**color** heterogeneity test — see below. |
| [`webrtc.toml`](webrtc.toml) | 2×2 grid | 2 × `webrtc` (WHIP ingest) + `bars` + `clock` | WebRTC end-to-end: WHIP contribution sources (ADR-T014), a WHEP-serve `webrtc` output + a `whip_push` output (ADR-0049), and the shared `[webrtc]` endpoint section (ADR-0048 §9). IPv6-first examples. |

The synthetic source kinds — `bars` (75% colour bars; `test` is a back-compat alias), `solid`
(a `#RRGGBB` slate), and `clock` (a full-frame analog/digital clock) — render in-process in pure
Rust ([ADR-0027](../docs/decisions/ADR-0027.md)), so the grid/PiP examples run with no external
dependencies. Replace a source's `kind`/`url` to point at real `rtsp`/`hls`/`ts`/`srt`/`ndi` inputs.

## `public-streams-2x2.toml` — the heterogeneity test

A deliberately mixed-source grid: built-in `test` synthetic patterns, a public sample clip, and a
placeholder example RTSP camera (`rtsp://camera.example.net:8554/stream`). Between them the tiles
span different **fps**, **codecs**, and **color tagging** (untagged vs BT.709) — exactly the
heterogeneity the engine must normalize onto one canvas. Swap the example camera URL for your own
`rtsp`/`hls`/`ts`/`srt`/`ndi` source; for fully reproducible/offline runs use the `test` sources or
the synthetic recipes in [`../docs/reference/example-streams.md`](../docs/reference/example-streams.md).

```sh
# (once implemented) validate, then run:
multiview validate examples/public-streams-2x2.toml
multiview run      examples/public-streams-2x2.toml
```
