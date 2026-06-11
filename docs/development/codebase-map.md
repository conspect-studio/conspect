# Multiview codebase map (one screen)

A fast orientation for agents. Authoritative detail lives in
[`docs/architecture/conventions.md`](../architecture/conventions.md) (source of truth) and the
root [`CLAUDE.md`](../../CLAUDE.md). This page is the map, not the territory.

> **Build-out status (in progress).** The pure-Rust foundation is built and tested: all 16 crates
> compile, the default GPU-free / native-dep-free build is green across the full CI gate set, and
> there are ~500 tests. `multiview-engine` realizes invariants #1 (output clock) and #10 (isolation)
> with tests; `multiview-control`/`multiview-preview`/`multiview-cli` and the `web/` SPA have substantial
> partial implementations. The **GPU `wgpu` compositor** (`compositor/gpu/`) and the **FFmpeg media
> path** live behind **off-by-default features** — they are not in the default build. The pure-Rust
> off-by-default transport features (`st2110`, `webrtc`, `youtube`, `ptp`, `cluster`, `ntp`,
> `is07-mqtt`, `nmos`, `i915-pmu`) are CI-linted via a separate `feature-clippy` matrix job (IN-7).
> GPU/native-SDK features (`cuda`, `vaapi`, `ffmpeg`, `gpl-codecs`, `ndi`) are not in the shared-runner
> CI and not verified on real hardware in this environment. See
> [`ROADMAP.md`](../../ROADMAP.md) and [`FEATURES.md`](../../FEATURES.md) for per-milestone /
> per-feature status.

## Top-level layout

```
multiview/
  CLAUDE.md            # repo-wide agent rules + 11 invariants + crate map (Claude reads this)
  AGENTS.md            # tool-agnostic agent contract (non-Claude agents)
  Cargo.toml           # workspace; 16 crates + xtask
  crates/<crate>/CLAUDE.md   # per-crate orientation, loads on demand
  crates/              # the 16 multiview-* crates + xtask
  web/                 # React 19 + TS + Vite SPA (web/CLAUDE.md)
  xtask/               # dev automation (build-web, gen-openapi, packaging)
  examples/            # multiview configs + layout templates
  docs/                # architecture, research briefs, ADRs, dev docs
  deploy/ .devcontainer/ .github/   # container + CI
  .multiview-build/ target/            # git-ignored transient/build output (do NOT read or commit)
```

## Crate map + dependency direction

**`core` ← everything. No cycles.** Leaf crates depend on `core` (+ `hal`/`ffmpeg`/`events` as
needed); `engine` depends on the media crates; `control`/`preview` depend on `engine` + `events`;
`cli` depends on all.

```
          multiview-core  (Frame, PixelFormat=NV12, ColorInfo, MediaTime, stage traits) -- no FFI
             ^
   +---------+----------------------------------------------+
   |         |              |            |          |        |
 multiview-hal multiview-ffmpeg  multiview-       multiview-    multiview-  multiview-
 (caps,     (libav RAII,   framestore   compositor  audio   overlay
  planner)   FFI owner)    (last-good)  (GPU color) (mix)   (libass)
   |         |              |            |          |        |
   +----+----+------+-------+------------+----+-----+--------+
        |           |                        |
   multiview-input  multiview-output          multiview-engine  <-- PROTECTED OUTPUT CORE
   (pacer,       (RTSP/HLS/NDI/         (output clock, supervisor,
    jitter,       push, encode-          hot-reconfig, degradation loop)
    PTS norm)     once-mux-many)              ^
                                              |
                            multiview-config / multiview-events / multiview-telemetry
                                              ^
                                +-------------+-------------+
                                |                           |
                          multiview-control              multiview-preview
                          (axum REST/WS/SSE,           (taps, WHEP/MJPEG,
                           OpenAPI, SQLite, SPA)        strictly isolated)
                                              ^
                                         multiview-cli  (binary `multiview`; wires it all)
```

| Crate | Touch it when… | Key modules (`src/`) | Brief(s) / ADRs to read first |
|-------|----------------|----------------------|-------------------------------|
| `multiview-core` | Shared types/traits, clock, layout model, error taxonomy. No FFI. | `frame` `pixel` `color` `time` `layout` `traits` `error` | core-engine §3–§5 |
| `multiview-hal` | Capability detect, backend negotiation, cost model/planner. | `capability` `probe` `registry` `cost` `planner` `degradation` | core-engine §6, efficiency; ADR-0003/0004/E008 |
| `multiview-ffmpeg` | Safe RAII over libav*, hwframe lifecycle, all raw FFI. **Feature-gated (`ffmpeg`).** | `demux` `decode` `decode_stream` `encode` `mux` `scale` `resample` `convert` `hwframe` `audio_file` | core-engine §7,§8.1,§12; ADR-0002/0004 |
| `multiview-compositor` | Color convert, linear-light blend, overlays; **GPU path in `gpu/` (feature-gated `wgpu`)**. | `range` `matrix` `transfer` `primaries` `blend` `pipeline` (CPU reference); `gpu/` (`device` `compositor` `shader` `uniforms` `shaders/`) | color-management, core-engine §8.2,§13; ADR-C001..C006, E002 |
| `multiview-framestore` | Lock-free last-good-frame stores + tile state machine. | `latest` (triple-buffer slot) `state` (LIVE→STALE→RECONNECTING→NO_SIGNAL) `tile` | resilience-and-av, streaming-gotchas §1,§7; ADR-T002 |
| `multiview-audio` | Decode/resample/mix/route + EBU R128. | `format` `decode` `filter` (K-weighting) `loudness` `truepeak` `mixer` | resilience-and-av, streaming-gotchas §5,§7; ADR-R005/R006/T006 |
| `multiview-overlay` | Overlay layers, text, subtitles (libass). | `geometry` `layer` `resolve` `alert` | resilience-and-av; ADR-R007/R008 |
| `multiview-input` | Ingest, **input pacer**, jitter, PTS normalization, reconnect. libav adapter feature-gated (`ffmpeg`). | `source` (ingest core) `normalize` `jitter` `pacer` `reconnect` `libav` | streaming-gotchas §1–§3,§5–§7, core-engine §9.1; ADR-T003/T004/T006/T007/T008 |
| `multiview-output` | RTSP/HLS·LL-HLS/NDI/push; encode-once-mux-many. Transport/encode feature-gated (`ffmpeg`). | `sink` `fanout` (encode-once routing model) `hls/` (`master` `media` playlist text) | streaming-gotchas §4, core-engine §9.2; ADR-0006/0007/T005 |
| `multiview-engine` | **Protected core**: output clock, supervisor, hot-reconfig, degradation. | `clock` (inv #1) `drive` `runtime` (`EngineRuntime`) `isolation` (inv #10) `supervisor` `degrade` | core-engine §4–§12, resilience-and-av, streaming-gotchas §0; ADR-T001/R001/R004 |
| `multiview-config` | Config/template schema, validation, config-as-code. | `schema` `grid` (grid solver) | core-engine §13,§14, management-capability-matrix; ADR-0010 |
| `multiview-events` | Realtime event types + versioned envelope. | `event` `envelope` `seq` `ordering` `topic` `subscription` | realtime-api; ADR-RT002/RT003 |
| `multiview-control` | axum REST/WS/SSE, OpenAPI, auth, SQLite, command bus, embedded SPA. | `routes` `problem` (RFC 9457) `realtime` (WS/SSE) `auth` `command` `repository` `sqlite` `openapi` `state` `concurrency` | web-api-stack, realtime-api, management-capability-matrix; ADR-RT001..RT006, W001..W008 |
| `multiview-preview` | Preview taps, WHEP/MJPEG, cue/pre-warm. Strictly isolated. | `tap` (registry, refcounted lazy-start) `framing` `token` | preview-subsystem; ADR-P001..P005 |
| `multiview-telemetry` | `tracing` + Prometheus + health (`/livez`,`/readyz`). | `metrics` `health` `tracing_init` | core-engine §15, resilience-and-av; ADR-R009 |
| `multiview-cli` | Binary `multiview`: wires engine + control, config load, run/validate. | `cli` (arg parse) `validate` `run` (`SoftwareEngine`, `run --software`) | core-engine; conventions §7 (licensing) |
| `web/` | React 19 SPA: shadcn/ui, TanStack, react-konva, dnd-kit, OpenAPI client. | `app` `pages` `components/ui` `layout/` (konva canvas + accessible `CellsForm`) `realtime/` `api` `i18n` + `locales/` (en/ar/pseudo) `theme` | web-api-stack, conventions §8; ADR-W001..W008 |

## Docs tree

```
docs/
  architecture/conventions.md   # SOURCE OF TRUTH (names, flags, invariants, licensing)
  architecture/                 # overview, pipeline, color, timing-and-sync, resilience, hardware
  research/                     # 11 verification-hardened design briefs (the "why")
  decisions/                    # 89 ADRs, grouped by prefix (below)
  development/                  # this map, working-in-this-monorepo, completeness checklist
  reference/  io/  media/  templates/  operations/  glossary.md  roadmap.md
```

**ADR prefixes:** `0001+` core engine · `R*` resilience/AV · `E*` efficiency · `C*` color ·
`T*` streaming/timing · `P*` preview · `RT*` realtime API · `M*` management · `W*` web/API stack ·
`DC*` devcontainer. Indexes: [`research/README.md`](../research/README.md),
[`decisions/README.md`](../decisions/README.md).

## The 11 invariants (one line; full text in conventions.md §5, root CLAUDE.md §2)

1 output-clock · 2 last-good-frame + state machine · 3 unified timing (never float fps) ·
4 HLS pacing to wall-clock · 5 NV12-throughout · 6 decode-at-display-resolution ·
7 encode-once-mux-many · 8 fixed color pipeline order · 9 resource-adaptive degradation ·
10 isolation (control/preview never back-pressure the engine) · 11 live-apply classification.

**#1 and #10 are the heart of the product. A change that risks either: stop, write a design note.**

How to work here without exhausting context:
[`working-in-this-monorepo.md`](working-in-this-monorepo.md).
