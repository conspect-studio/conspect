# Multiview codebase map (one screen)

A fast orientation for agents. Authoritative detail lives in
[`docs/architecture/conventions.md`](../architecture/conventions.md) (source of truth); the working
contract is [`AGENTS.md`](../../AGENTS.md) + [`engineering.md`](../standards/engineering.md). This
page is the map, not the territory.

> **Build-out status (in progress).** The pure-Rust foundation is built and tested: all 23 crates
> compile, the default GPU-free / native-dep-free build is green across the full CI gate set, and
> the workspace carries **5,200+ `#[test]`/`#[tokio::test]` functions**
> (`rg -c '#\[(tokio::)?test\b' crates xtask` — a source count, not a run count).
> `multiview-engine` realizes invariants #1 (output clock) and #10 (isolation) with tests;
> `multiview-control`/`multiview-preview`/`multiview-cli` and the `web/` SPA have substantial
> partial implementations. The **GPU `wgpu` compositor** (`compositor/gpu/`) and the **FFmpeg media
> path** live behind **off-by-default features** — they are not in the default build. See
> [`ROADMAP.md`](../../ROADMAP.md) and [`FEATURES.md`](../../FEATURES.md) for per-milestone /
> per-feature status.
>
> **What CI actually covers.** The default (pure-Rust) build runs `check` + `test` on ubuntu +
> macOS. A `feature-clippy` matrix clippy+tests the off-by-default *pure-Rust* features one leg at a
> time (`st2110`, `webrtc`, `youtube`, `ptp`, `cluster`, `ntp`, `is07-mqtt`, `nmos`, `tls`,
> `i915-pmu`, `vaapi`, `heartbeat`, `overlay`, `wgpu`, `native`); two further jobs cover `libass`
> and the FFmpeg-linked `ffmpeg`/`aes67`/`webrtc-native` builds inside the FFmpeg base image.
> **The honest boundary:** GPU/native-SDK features (`cuda`, `metal`, `videotoolbox`, `ndi`,
> `gpl-codecs`) are **not in CI at all**, and **no** hardware path — including the
> `ffmpeg`/`vaapi`/`libass` legs that do compile and test on shared runners — **is verified on real
> hardware in this environment**. Every runner is GitHub-hosted: there is no GPU runner, no
> self-hosted runner, and no `cargo mutants` job in any workflow.

## Top-level layout

```
multiview/
  AGENTS.md            # the agent contract: non-negotiables, commands, change classes, routing
  CLAUDE.md            # Claude-Code specifics only (imports AGENTS.md)
  Cargo.toml           # workspace; 23 crates + xtask
  crates/<crate>/CLAUDE.md   # per-crate orientation where one exists; loads on demand
  crates/              # the 23 multiview-* crates
  xtask/               # dev automation (build-web, gen-openapi, packaging)
  scripts/             # classify.sh (change class) + the CI sanity checks (links, language, soak)
  web/                 # React 19 + TS + Vite SPA (web/CLAUDE.md)
  examples/            # multiview configs + layout templates
  docs/                # conventions, standards, research briefs, ADRs, runbooks, dev docs
  deploy/ .devcontainer/ .github/   # container + CI
  .multiview-build/ target/            # git-ignored transient/build output (do NOT read or commit)
```

## Crate map + dependency direction

**`core` ← everything. No cycles.** Leaf crates depend on `core` (+ `hal`/`ffmpeg`/`events` as
needed); `engine` depends on the media crates; `control`/`preview` depend on `engine` + `events`;
`webrtc` depends on `preview` + `input`; `cli` depends on all.

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

Off this spine sit the **control-plane leaves** (`multiview-licence`, `multiview-mesh` — neither has
an engine dependency, by charter), the **shared WebRTC endpoint** (`multiview-webrtc`, over
`preview` + `input`), and the four **`-sys`/FFI leaves** (`multiview-ndi-sys`,
`multiview-rist-sys`, `multiview-ntpsys`, `multiview-i915pmu`) that isolate `unsafe` so their
consumers stay `forbid(unsafe_code)`.

| Crate | Touch it when… | Key modules (`src/`) | Optional features | Brief(s) / ADRs to read first |
|-------|----------------|----------------------|-------------------|-------------------------------|
| `multiview-core` | Shared types/traits, clock, layout model, error taxonomy. No FFI. | `frame` `pixel` `color` `time` `layout` `traits` `error` | — | core-engine §3–§5 |
| `multiview-hal` | Capability detect, backend negotiation, cost model/planner. | `capability` `probe` `registry` `cost` `planner` `degradation` | `cuda` `vaapi` `qsv` `videotoolbox` `i915-pmu` `display-kms` | core-engine §6, efficiency; ADR-0003/0004/E008 |
| `multiview-ffmpeg` | Safe RAII over libav*, hwframe lifecycle, all raw FFI. **Feature-gated (`ffmpeg`).** | `demux` `decode` `decode_stream` `encode` `mux` `scale` `resample` `convert` `hwframe` `audio_file` | `ffmpeg` `gpl-codecs` `cuda` | core-engine §7,§8.1,§12; ADR-0002/0004 |
| `multiview-compositor` | Color convert, linear-light blend, overlays; **GPU path in `gpu/` (feature-gated `wgpu`)**. | `range` `matrix` `transfer` `primaries` `blend` `pipeline` (CPU reference); `gpu/` (`device` `compositor` `shader` `uniforms` `shaders/`) | `wgpu` (baseline GPU backend) `cuda` `metal` `vaapi` `overlay` | color-management, core-engine §8.2,§13; ADR-C001..C006, E002 |
| `multiview-framestore` | Lock-free last-good-frame stores + tile state machine. | `latest` (triple-buffer slot) `state` (LIVE→STALE→RECONNECTING→NO_SIGNAL) `tile` `liveness` | — | resilience-and-av, streaming-gotchas §1,§7; ADR-T002 |
| `multiview-audio` | Decode/resample/mix/route + EBU R128. | `format` `decode` `filter` (K-weighting) `loudness` `truepeak` `mixer` | `ffmpeg` | resilience-and-av, streaming-gotchas §5,§7; ADR-R005/R006/T006 |
| `multiview-overlay` | Overlay layers, text, subtitles (libass). | `geometry` `layer` `resolve` `alert` `subtitle` `timecode` `umd` | `libass` | resilience-and-av; ADR-R007/R008 |
| `multiview-input` | Ingest, **input pacer**, jitter, PTS normalization, reconnect. libav adapter feature-gated (`ffmpeg`). | `source` (ingest core) `normalize` `jitter` `pacer` `reconnect` `libav` | `ffmpeg` `ndi` `st2110` `webrtc` `youtube` | streaming-gotchas §1–§3,§5–§7, core-engine §9.1; ADR-T003/T004/T006/T007/T008 |
| `multiview-output` | RTSP/HLS·LL-HLS/NDI/push; encode-once-mux-many. Transport/encode feature-gated (`ffmpeg`). | `sink` `fanout` (encode-once routing model) `hls/` (`master` `media` playlist text) | `ffmpeg` `ndi` `rtsp-server` `aes67` `rist-stats` `gpl-codecs` `display-kms` | streaming-gotchas §4, core-engine §9.2; ADR-0006/0007/T005 |
| `multiview-engine` | **Protected core**: output clock, supervisor, hot-reconfig, degradation. | `clock` (inv #1) `drive` `runtime` (`EngineRuntime`) `isolation` (inv #10) `supervisor` `degrade` `sysref` | `ffmpeg` `ptp` `ntp` `cluster` | core-engine §4–§12, resilience-and-av, streaming-gotchas §0; ADR-T001/R001/R004 |
| `multiview-config` | Config/template schema, validation, config-as-code. | `schema` `grid` (grid solver) `layout_doc` `diff` `limits` | — | core-engine §13,§14, management-capability-matrix; ADR-0010 |
| `multiview-events` | Realtime event types + versioned envelope. | `event` `envelope` `seq` `ordering` `topic` `subscription` | — | realtime-api; ADR-RT002/RT003 |
| `multiview-control` | axum REST/WS/SSE, OpenAPI, auth, SQLite, command bus, embedded SPA. | `routes` `problem` (RFC 9457) `realtime` (WS/SSE) `auth` `command` `repository` `sqlite` `openapi` `state` `concurrency` | `openapi` (**on by default**) `embed-web` `tls` `nmos` `is07-mqtt` `discovery` `devices-net` | web-api-stack, realtime-api, management-capability-matrix; ADR-RT001..RT006, W001..W008 |
| `multiview-preview` | Preview taps, WHEP/MJPEG, cue/pre-warm. Strictly isolated. | `tap` (registry, refcounted lazy-start) `framing` `token` `focus` `whep` | `webrtc` (the pure seam; the native transport moved to `multiview-webrtc`) | preview-subsystem; ADR-P001..P005 |
| `multiview-webrtc` | The **shared native WebRTC endpoint** (str0m): one dual-stack `[::]` UDP socket muxing every session (WHIP ingest, WHEP preview + output, WHIP push), full-ICE IPv6-first + an in-crate sans-IO TURN client. | `session` `sdp` `config` `egress` `whep_egress` `transport/` (`ingest` `unified` `whep_serve` `whip_endpoint` `whip_push`) `turn/` `signalling/` | `native` (str0m ICE/DTLS/SRTP; everything else is socket-free) | webrtc; ADR-0048/0049/T014 |
| `multiview-telemetry` | `tracing` + Prometheus + health (`/livez`,`/readyz`). | `metrics` `health` `tracing_init` `availability` `retention` | `syslog` `snmp` | core-engine §15, resilience-and-av; ADR-R009 |
| `multiview-licence` | Conspect **entitlement plane**: signed lease/entitlement verify, the enforcement ladder as **pure data**, fingerprint scoring, heartbeat client. **No engine dep, no I/O, spawns nothing** — every computed state stays on air (inv #1). | `entitlement` `lease` `verify` `ladder` `fingerprint` `store` `status` `watcher` `heartbeat` `challenge` | `heartbeat` (key-trust + lease verifier + client loop; cli supplies the HTTP transport) | conspect-account-architecture §2,§6,§8,§12, licensing-runtime; ADR-0050/0096/I006 |
| `multiview-mesh` | Conspect **local mesh**: always-on mDNS announce/browse of salted, signed summaries; **untrusted** peer inventory; role determination; end-to-end-signed relay carrier. Control-plane actor — bounded drop-oldest, never back-pressures (inv #10). | `announce` `peer` `role` `relay` `state` `service` `driver` `transport` | `mdns` (the socket announce/browse task; the payload + peer table compile without it) | conspect-account-architecture §8,§9,§11; ADR-0051/0042/0050 |
| `multiview-ndi-sys` | FFI leaf: runtime `dlopen`/`dlsym` of the **proprietary NDI® SDK** (`NDIlib_v6_load`). The SDK is **never vendored and never linked at build time**; this crate isolates the `unsafe` so `multiview-output` stays `forbid(unsafe_code)`. | `find` `table` `recv` `send` `error` | `bindings` (bindgen over the licensed header at build time; consumers' `ndi` pulls it) | ndi-integration; ADR-0008/0028 |
| `multiview-rist-sys` | FFI leaf: runtime `dlopen` of **librist** for RIST **link statistics** (FFmpeg's `rist://` exposes no stats callback). The callback runs on librist's thread and only `try_send`s on a bounded drop-oldest channel. | `raw` (hand-mirrored C ABI, size-checked) `session` | `session` (the dlopen + stats path; the pure C-ABI decode compiles without it) | rist-transport; ADR-0095 |
| `multiview-ntpsys` | FFI leaf: minimal safe wrapper over Linux `adjtimex(2)` — the NTP clock-discipline read behind the wall-clock **reference** badge (a media-clock reference only, **never** a pacer). | `lib` (single-module leaf) | — (reached via `multiview-engine/ntp`) | wall-clock-sync; ADR-T012/0038 |
| `multiview-i915pmu` | FFI leaf: minimal safe wrapper over the Linux i915 PMU per-engine busy-ns counters (`perf_event_open(2)`) for the Intel GPU-load probe. | `lib` (single-module leaf) | — (reached via `multiview-hal/i915-pmu`) | gpu-monitoring-and-scheduling, system-stats-attribution; ADR-0017 |
| `multiview-cli` | Binary `multiview`: wires engine + control, config load, run/validate. | `cli` (arg parse) `validate` `run` (`SoftwareEngine`, `run --software`) `pipeline` `licence` | aggregates every crate flag; umbrella presets `nvidia` / `apple` / `linux-vaapi` / `full` (everything non-GPL) | core-engine; conventions §7 (licensing) |
| `web/` | React 19 SPA: shadcn/ui, TanStack, react-konva, dnd-kit, OpenAPI client. | `app` `pages` `components/ui` `layout/` (konva canvas + accessible `CellsForm`) `realtime/` `api` `i18n` + `locales/` (en/ar/pseudo) `theme` | n/a (npm only — `npm --prefix web …`) | web-api-stack, conventions §8; ADR-W001..W008 |

Every listed feature is **off by default** except `multiview-control/openapi` (and the cli's
`software` backend, which is always on). This column names the load-bearing ones; the full taxonomy
is [conventions §4](../architecture/conventions.md) and each crate's `Cargo.toml` `[features]` block
is authoritative.

## Routing by topic (not by crate)

| Working on… | Read first |
|-------------|------------|
| Any management surface (API ↔ UI ↔ engine) | [management-capability-matrix](../research/management-capability-matrix.md) — the authoritative capability table: every controllable engine parameter needs a versioned API resource **and** a named UI control |
| Licensing / build profiles — what a feature does to the effective licence | [core-engine](../research/core-engine.md) §17–§18, [conventions §7](../architecture/conventions.md), ADR-0011/0012 |
| Anything on the data plane, or any FFI / `unsafe` | [data-plane-safety](../architecture/data-plane-safety.md) — the 8 engine + FFI rules, stable `§1`–`§8` anchors |
| Which gates a change owes before it can merge | [engineering.md](../standards/engineering.md) Part A; `scripts/classify.sh` prints the class (R0–R3) and the gates it owes |

## Docs tree

```
docs/
  architecture/conventions.md   # SOURCE OF TRUTH (names, flags, invariants, licensing)
  architecture/                 # overview, pipeline, color, timing-and-sync, resilience,
                                #   hardware-and-efficiency, efficiency-budget,
                                #   feature-dependency-map, data-plane-safety (the 8 safety rules)
  standards/engineering.md      # the process contract: gate matrix, classes, evidence, exceptions
  research/                     # 56 verification-hardened design briefs (the "why") + README index
  decisions/                    # 236 ADRs, grouped by prefix (below) + TEMPLATE.md
  development/                  # this map, working-in-this-monorepo, agent-guardrails,
                                #   completeness-checklist, work-schedule + the per-push backlogs
  runbooks/                     # the operational "how" for provisioned resources
  api/ io/ media/ web/ templates/     # OpenAPI/AsyncAPI + REST/realtime prose; input & output;
                                #   audio/subtitles/overlays; SPA (a11y, i18n, preview); layouts
  licensing/ operations/ reference/   # licence posture; building/container/testing; bibliography
  README.md  glossary.md  roadmap.md  stack.md
```

**ADR prefixes:** `0001+` core engine · `C*` color · `DC*` devcontainer · `E*` efficiency ·
`G*` guardrails/governance · `I*` impl build-out · `M*` management · `MV*` broadcast multiviewer ·
`P*` preview · `R*` resilience/AV · `RT*` realtime API · `T*` streaming/timing · `W*` web/API stack.
Indexes: [`research/README.md`](../research/README.md), [`decisions/README.md`](../decisions/README.md).

## The 11 invariants (one line; full text in conventions.md §5)

1 output-clock · 2 last-good-frame + state machine · 3 unified timing (never float fps) ·
4 HLS pacing to wall-clock · 5 NV12-throughout · 6 decode-at-display-resolution ·
7 encode-once-mux-many · 8 fixed color pipeline order · 9 resource-adaptive degradation ·
10 isolation (control/preview never back-pressure the engine) · 11 live-apply classification.

**#1 and #10 are the heart of the product. A change that risks either is R3** — stop, write a design
note, add a chaos/soak test, and get explicit operator approval
([engineering.md](../standards/engineering.md) Part A).

How to work here without exhausting context:
[`working-in-this-monorepo.md`](working-in-this-monorepo.md).
