# Features

The full capability matrix for Multiview: every planned feature, its current status, the milestone it
lands in ([ROADMAP.md](ROADMAP.md)), and the design doc that specifies it. This is the living
support matrix — update a row's status as it is implemented.

**Status:** 📐 Designed (spec complete, not yet implemented) · 📋 Backlog/planned (folded into a milestone, not started) · 🔵 In progress · ✅ Implemented

> **Foundation build-out (in progress).** The pure-Rust foundation is built and tested: the 16-crate
> workspace compiles, the default (GPU-free, native-dep-free) build is green across the full CI gate
> set (fmt/clippy `-D warnings`/test/deny/inclusive-language), and there are ~500 tests (unit +
> property + integration). What has landed beyond the M0 scaffold:
>
> - **`multiview-core`** — shared types/traits expanded (`Frame`, NV12 `PixelFormat`, 4-axis `ColorInfo`,
>   `MediaTime`/clock, layout model, error taxonomy, stage traits).
> - **The 10 leaf crates** (`hal`, `framestore`, `audio`, `overlay`, `input`, `output`, `config`,
>   `events`, `telemetry`, plus the color modules of `compositor`) — built with real unit/property
>   tests against the documented contracts.
> - **`multiview-engine`** — the protected output core: fixed-cadence output clock, compositor drive
>   loop, `EngineRuntime`, engine→outside isolation (arc-swap latest-state + bounded drop-oldest
>   broadcast), actor supervisor, and the degradation control loop. **Invariants #1 (output-clock)
>   and #10 (isolation) are exercised by tests** in this crate.
> - **Layer C** — `multiview-control` (axum REST/WS/SSE API, OpenAPI, SQLite, command bus, auth),
>   `multiview-preview` (isolated taps), and `multiview-cli` (`validate` + `run --headless`); `cargo xtask
>   gen-openapi` emits the OpenAPI document.
> - **Web SPA** (`web/`) — design system, react-konva + dnd-kit layout editor, realtime client,
>   i18n + accessibility scaffolding.
> - **Feature-gated paths (NOT in the default build, NOT yet CI-gated here):** the GPU **wgpu**
>   compositor (`wgpu` feature) and the **FFmpeg** media path (`ffmpeg` feature). These compile
>   behind off-by-default features and have not been exercised on real hardware in this environment.
>   **NDI** remains design-only.
>
> **Not yet verified on hardware here:** real HW decode/encode (NVDEC/NVENC, VideoToolbox, VAAPI/QSV),
> GPU compositing on a device, and any end-to-end run against live sources. The headless software
> engine runs and produces frames per the output clock, but a full software decode→compose→serve
> pipeline is not yet wired end-to-end. Rows below are 📐 *Designed* unless a crate has shipped
> working code for that feature; 🔵 marks partial/feature-gated work in progress. The repository
> baseline — docs, ADRs, agent-instruction system, dev container, CI — remains ✅ delivered (M0).
> See the [212-item management-completeness checklist](docs/development/completeness-checklist.md)
> for the fine-grained UI/API surface.

## Inputs

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| RTSP ingest (TCP/UDP, reconnect) | 📐 | M1/M4 | [io/inputs.md](docs/io/inputs.md) |
| HLS / M3U ingest (+ live-edge pacing, anti-burst) | 📐 | M4 | [io/inputs.md](docs/io/inputs.md), [streaming-gotchas](docs/research/streaming-gotchas.md) |
| MPEG-TS ingest | 📐 | M4 | [io/inputs.md](docs/io/inputs.md) |
| SRT ingest | 📐 | M4 | [io/inputs.md](docs/io/inputs.md) |
| RTMP ingest | 📐 | M4 | [io/inputs.md](docs/io/inputs.md) |
| NDI input (first-class; opt-in `ndi` feature + runtime license gate) | 📐 | M4 | [io/ndi.md](docs/io/ndi.md) |
| Live YouTube input (yt-dlp resolver → existing HLS ingest; opt-in `youtube` feature, runtime-discovered binary) | 📋 backlog | M4+ | [io/youtube-live.md](docs/io/youtube-live.md), [ADR-0015](docs/decisions/ADR-0015.md) |
| File / test-pattern sources | 🔵 | M1 | [io/inputs.md](docs/io/inputs.md) |
| Input pacer, jitter buffers, timestamp normalization | 🔵 | M3 | [timing-and-sync](docs/architecture/timing-and-sync.md) |
| Supervised reconnect / backoff / circuit breaker | 🔵 | M3 | [resilience](docs/architecture/resilience.md) |
| Per-source color override (4 axes) | 📐 | M5 | [color](docs/architecture/color.md) |
| Per-input audio/subtitle track selection | 📐 | M4/M5 | [media](docs/media/audio-subtitles-overlays.md) |

## Outputs

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| RTSP server endpoint | 📐 | M1/M4 | [io/outputs.md](docs/io/outputs.md) |
| HLS + Low-Latency HLS | 🔵 | M4 | [io/outputs.md](docs/io/outputs.md) |
| NDI output (opt-in `ndi` feature + runtime license gate) | 📐 | M4 | [io/ndi.md](docs/io/ndi.md) |
| RTMP / SRT push | 📐 | M4 | [io/outputs.md](docs/io/outputs.md) |
| Encode-once, mux-many fan-out | 🔵 | M4 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| Per-output transcode profiles (codec/bitrate/GOP/rate-control) | 📐 | M6 | [management matrix](docs/research/management-capability-matrix.md) |
| Output color tagging (CICP) + ffprobe verify | 📐 | M5 | [color](docs/architecture/color.md) |
| Multistream discrete audio tracks + program bus | 📐 | M4 | [media](docs/media/audio-subtitles-overlays.md) |

## GPU, decode/encode & HAL

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| Custom GPU compositor (wgpu baseline) | 🔵 | M2 | [pipeline](docs/architecture/pipeline.md) |
| Vendor fast paths (CUDA, Metal) | 📐 | M2/M8 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| HW decode (NVDEC, VideoToolbox, VAAPI, QSV) | 📐 | M2 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| HW encode (NVENC, VideoToolbox, VAAPI, QSV) | 📐 | M2 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| Per-stage backend auto-negotiation + cost model | 🔵 | M2/M8 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| Zero-copy within a vendor island; NV12-throughout | 📐 | M2 | [overview](docs/architecture/overview.md), [ADR-0004](docs/decisions/ADR-0004.md) |
| Decode-at-display-resolution | 📐 | M8 | [ADR-E001](docs/decisions/ADR-E001.md) |

## Layout & templates

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| Presets: 2x2, 3x3, 1+5, PiP | 🔵 | M3 | [templates/layout-and-config.md](docs/templates/layout-and-config.md) |
| Grid + absolute (free-form / overlap) layouts | 🔵 | M3 | [templates/layout-and-config.md](docs/templates/layout-and-config.md) |
| Fit modes, borders, gaps, corner radius, z-order | 🔵 | M3 | [templates/layout-and-config.md](docs/templates/layout-and-config.md) |
| Live hot-swap of a cell's source (no black flash) | 📐 | M3 | [templates/layout-and-config.md](docs/templates/layout-and-config.md) |
| Transitions (cut / crossfade) | 📐 | M3/M13 | [production-switcher §5](docs/research/production-switcher.md), [ADR-0055](docs/decisions/ADR-0055.md) — supersedes the cut/crossfade sketch in [templates/layout-and-config.md](docs/templates/layout-and-config.md) §7.1; generalized to the full transition family (mix/dip/wipe/DVE/stinger) with integer-frame rates |
| Config-as-code (TOML/JSON, validate, import/export, rollback) | 🔵 | M6 | [ADR-M006](docs/decisions/ADR-M006.md) |

## Audio, subtitles & overlays

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| EBU R128 / true-peak metering | 🔵 | M4 | [media](docs/media/audio-subtitles-overlays.md) |
| Subtitle ingest (CEA-608/708, DVB, teletext, WebVTT) | 🔵 | M5 | [media](docs/media/audio-subtitles-overlays.md) — native in-pipeline **WebVTT** (HLS rendition) + **DVB-sub** ingest implemented + demoed; CEA-608/708 + teletext pending (#36) |
| Subtitle burn-in (libass) + passthrough tracks | 🔵 | M5 | [media](docs/media/audio-subtitles-overlays.md) — per-tile burn-in (text + bitmap) implemented + demoed; libass styling + passthrough pending |
| Overlays: labels, clocks, logos, audio meters, alert cards | 🔵 | M5 | [media](docs/media/audio-subtitles-overlays.md) |
| Color: 4-axis detect/convert, linear-light, HDR/SDR tone-map | 🔵 | M5 | [color](docs/architecture/color.md) |

## Resilience & efficiency

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| Output-clock invariant (never-falters) | 🔵 | M3 | [resilience](docs/architecture/resilience.md) |
| Last-good-frame stores + tile state machine + "no signal" cards | 🔵 | M3 | [resilience](docs/architecture/resilience.md) |
| Hot reconfiguration (live-apply vs controlled reset) | 📐 | M3/M6 | [ADR-M005](docs/decisions/ADR-M005.md) |
| GPU device-loss recovery; encoder hot-standby | 📐 | M3 | [resilience](docs/architecture/resilience.md) |
| Resource-adaptive degradation + admission control | 🔵 | M8 | [hardware-and-efficiency](docs/architecture/hardware-and-efficiency.md) |
| Commodity-tier density targets + perf-regression CI | 📐 | M8 | [efficiency](docs/research/efficiency.md) |

## Management API, realtime & web UI

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| REST API `/api/v1` (RFC 9457, ETag, idempotency) | 🔵 | M6 | [api/rest.md](docs/api/rest.md) |
| OpenAPI 3.1 + Scalar "try-it-out" test env | 🔵 | M6 | [api/rest.md](docs/api/rest.md) |
| Auth (sessions + API keys), RBAC, per-object authz | 🔵 | M6 | [web-api-stack](docs/research/web-api-stack.md) |
| Realtime: WebSocket (primary) + SSE + AsyncAPI | 🔵 | M7 | [api/realtime.md](docs/api/realtime.md) |
| Web app: dashboard, sources, outputs, settings/users | 🔵 | M7 | [web/management-app.md](docs/web/management-app.md) |
| Drag-and-drop visual layout editor (react-konva + dnd-kit) | 🔵 | M7 | [web/management-app.md](docs/web/management-app.md) |
| Preview: input (incl. off-air cue), program, output | 🔵 | M7 | [web/preview.md](docs/web/preview.md) |

## Operations & tooling

| Feature | Status | Target | Design |
|---------|--------|--------|--------|
| Dev container (GPU passthrough, 1Password, multi-arch) | ✅ | M0 | [operations/devcontainer.md](docs/operations/devcontainer.md) |
| CI (fmt/clippy/test/deny/inclusive-language/web) | ✅ | M0 | `.github/workflows/ci.yml` |
| Agent guardrails (typing, TDD, adversarial review) | ✅ (docs) | M0 | [agent-guardrails](docs/development/agent-guardrails.md) |
| Linux container (NVIDIA Toolkit / VAAPI) | 📐 | M9 | [operations/containerization.md](docs/operations/containerization.md) |
| Observability (tracing, Prometheus, health) | 🔵 | M3/M8 | [operations/observability.md](docs/operations/observability.md) |
| Testing: synthetic sources, chaos/soak, mutation, density | 📐 | M3/M9 | [operations/testing-and-benchmarking.md](docs/operations/testing-and-benchmarking.md) |

## Broadcast multiviewer (proposed — established, standards-based capabilities)

Standards-based broadcast-multiviewer capabilities mapped to Multiview — see [research/broadcast-multiviewer-features.md](docs/research/broadcast-multiviewer-features.md) and milestones **M10–M12** in [ROADMAP.md](ROADMAP.md). Every capability is anchored in an open industry standard/protocol. Legend: 📋 planned · ✅ already designed (enhance).

### Layouts & display modes

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Layout presets (quad / grid / 1+5 / PiP / PoP / 2+8 / 4x4) | ✅ have (enhance) | core | M3 | De-facto industry standard |
| Free-form absolute tile placement (any size/aspect/position, overlap) | ✅ have (enhance) | core | M3 | De-facto industry standard |
| Per-tile source crop / zoom (region-of-interest) | 📋 | valuable | M3 | De-facto industry standard |
| Round-robin / cycling tile mode | 📋 | valuable | M11 | De-facto industry standard |
| Freeze / reference-image still tiles | 📋 | niche | M11 | De-facto (freeze-frame detection also a QC metric); reference stills ship via the media-library still store — decode-once + hold-forever ([ADR-0057](docs/decisions/ADR-0057.md)) |
| Output orientation (portrait/landscape) + per-tile rotation/flip | 📋 | valuable | M11 | De-facto industry standard |

### Layouts & control

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Instant layout preset recall / salvo (multi-trigger) | 📋 | core | M11 | Router/automation salvo concept; SW-P-08 presets/salvos; extended by switcher memories = salvo + recall-scope masks ([ADR-M012](docs/decisions/ADR-M012.md), [production-switcher §10](docs/research/production-switcher.md)) — supersedes [ADR-MV004](docs/decisions/ADR-MV004.md)'s salvo-only automation story |

### Layouts & monitoring

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Penalty-box / auto-promote-on-fault | 📋 | core | M10 | Generic industry pattern |

### Layouts & metadata

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| AFD-driven aspect handling + pillarbox/letterbox | 📋 | valuable | M10 | SMPTE ST 2016-1 (format), ST 2016-3 (VANC mapping); carriage ETSI TS 101 154 / ATSC A/53 Part 4 |

### Overlays

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Safe-area / title-safe / action-safe / center-cross markers | 📋 | valuable | M5 | SMPTE ST 2046-1 (93%/90% of Production Aperture); RP 2046-2 (alt aspect 90/90); legacy RP 218; ITU-R BT.1848 |

### Outputs & video walls

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Multi-head output (independent per-head layout + resolution) | 📋 | core | M12 | De-facto; IP head transport SMPTE ST 2110-20/-22, ST 2022-6 |
| Video-wall spanning + bezel compensation | 📋 | niche | M12 | De-facto (AV wall processing) |

### UMD & tally

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| TSL UMD protocol ingest/egress (v3.1 / v4.0 / v5.0) | 📋 | core | M11 | TSL UMD v3.1 / v4.0 / v5.0 — on-wire codec spec-reconciliation (v5.0 DLE, CONTROL bit order, v4.0 rebuild, golden on-wire vectors) pinned in [ADR-MV006](docs/decisions/ADR-MV006.md) |
| Dynamic UMD text + multi-field UMD/OMD | ✅ have (enhance) | core | M11 | TSL UMD; generic dynamic UMD |
| Tally states + multi-region/multi-level tally | ✅ have (enhance) | core | M11 | TSL v4.0/v5.0 (0=off/1=red/2=green/3=amber); SW-P-08 multi-level; internally-derived switcher tally (PGM/PVW/ISO-amber from live bus state) per [ADR-MV006](docs/decisions/ADR-MV006.md) |
| External tally-bus integration (switcher/router) + arbiter | 📋 | core | M11 | TSL tally; NMOS IS-07; GPI; switcher-native buses; internally-derived facts from the built-in switcher join the same arbiter under one conflict policy ([ADR-MV006](docs/decisions/ADR-MV006.md)) |
| Router/switcher name-following UMD labels | 📋 | valuable | M12 | SW-P-08 labels; Ember+; NMOS IS-04 |

### Control & integration

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| GPI/GPO contact-closure I/O (+ virtual GPI over IP) | 📋 | valuable | M11 | GPI/GPO contact closure; AMWA NMOS IS-07 (event/tally over WebSocket/MQTT) |
| NMOS IS-12 / MS-05 control + monitoring model | 📋 | niche | M12 | AMWA IS-12, MS-05-01/02; Control Feature Sets: Monitoring; IS-14 config |
| Salvos + scheduled/event-triggered layout automation | 📋 | valuable | M11 | SCTE 35/104; SMPTE 2021/BXF; UTC/VITC; GPI |
| Router control + route-follow (SW-P-08, Ember+) | 📋 | valuable | M12 | SW-P-08/SW-P-88; Ember+ (Glow/EmBER/S101); |
| Ember+ control/monitoring gateway | 📋 | niche | M12 | Ember+ (open de-facto; Glow/EmBER/S101) |

### Monitoring & alarms

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Content-aware fault probes: black / freeze (zone + dwell) | 🔵 | core | M10 | Generic industry-standard probes — black + freeze (dwell/hysteresis) probes wired into the CLI per-tile fault badges + demoed; full M10 alarm roll-up/REST/SNMP pending |
| Audio fault probes: silence / over-level / clip / phase / imbalance | 🔵 | core | M10 | Generic; true-peak ceiling per ITU-R BS.1770 (R128 -1 dBTP / A/85 -2 dBTP) — silence/audio-loss wired into the CLI fault badge + demoed; over-level/clip/phase/imbalance designed |
| Loudness-violation + dialnorm-mismatch alarm | 📋 | valuable | M10 | ITU-R BS.1770 vs EBU R128 / ATSC A/85; dialnorm per ATSC A/52, A/85 |
| Caption presence-loss + validity monitoring | 📋 | valuable | M10 | CEA-608/708; DVB EN 300 743; teletext EN 300 706 / OP-47 (SMPTE RDD 8); WebVTT |
| Format/standard-change + AFD-mismatch + colorimetry/HDR monitoring | 📋 | valuable | M10 | SMPTE ST 2016 (AFD); ITU-R BT.709/2020/2100; SMPTE ST 2086 / CTA-861 |
| Compression-artifact / QoE scoring (no-reference) | 📋 | niche | M10 | OPEN metrics ITU-T P.1203/P.1204, ITU-T J.247/J.341 |
| MPEG-TS error monitoring (ETSI TR 101 290 P1/P2/P3) | 📋 | valuable | M10 | ETSI TR 101 290 v1.3.1 (groupings verified; Buffer/Empty_buffer/Data_delay are P3 in all editions) |
| IP transport health: Media Delivery Index + ST 2022-7 path health | 📋 | valuable | M10 | IETF RFC 4445 (MDI); SMPTE ST 2022-7; RTP RFC 3550 |
| Alarm state machine: severity, dwell/hysteresis, latch, ack, roll-up | 📋 | core | M10 | ITU-T X.733 (alarm reporting) |

### Monitoring & metadata

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| SCTE-35 / SCTE-104 splice/ad-marker monitoring | 📋 | valuable | M10 | SCTE 35 (in-TS); SCTE 104 (ANC via SMPTE ST 2010 / ST 2038) |

### Monitoring & overlays

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Color-coded status borders + on-tile alarm cards | ✅ have (enhance) | core | M10 | Generic; severity colour aligns with ITU-T X.733 |
| Confidence scopes (waveform/vectorscope/histogram/parade) | 📋 | niche | M12 | Generic scopes; gamut per ITU-R BT.709/2020; HDR per BT.2100 |

### Monitoring & integration

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Northbound alerting: SNMP traps, syslog, email, webhook | 📋 | core | M10 | SNMP (IETF RFC 1157/3411 + MIB); syslog RFC 5424; SMTP; HTTP webhooks; severities map to X.733 |

### Monitoring & operations

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Alarm logging, history & availability reporting | 📋 | valuable | M10 | Generic QoS reporting |

### Audio metering

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Selectable meter ballistics & scales (PPM/VU/sample-peak/true-peak) | 📋 | valuable | M4 | IEC 60268-10 (analog PPM); IEC TR 60268-18 (digital sample-peak, a Technical Report); true-peak ITU-R BS.1770 |
| Phase/correlation + goniometer + surround sound-field metering | 📋 | valuable | M4 | ITU-R BS.775 (downmix/layout); generic correlation/goniometer |
| Loudness sub-meters (M/S/I/LRA/max-TP) + A/85 profile | ✅ have (enhance) | core | M4 | ITU-R BS.1770-4/-5; EBU R128 + Tech 3341/3342; ATSC A/85 |
| Multi-channel display + channel mapping/shuffle/de-embed | 📋 | valuable | M4 | Embedded model SMPTE ST 299-1 / AES3; IP SMPTE ST 2110-30 / AES67; NMOS IS-08 |

### Audio metering & compliance

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Loudness logging + exportable compliance reports | 📋 | core | M10 | Driven by CALM Act/FCC + ATSC A/85 + EBU R128 compliance; logging generic |

### Audio metering & operations

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Audio-follow-video monitor / PFL bus | 📋 | valuable | M4 | Generic operational feature; distinct from switcher audio-follow-video on transitions — that is [ADR-0059](docs/decisions/ADR-0059.md) (production switcher) |

### Audio metering & metadata

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| AC-3 / E-AC-3 audio metadata read (dialnorm) | 📋 | niche | M12 | ATSC A/52 (AC-3); SMPTE ST 337/340 (non-PCM data in AES3); ST 2110-31 over IP |

### Overlays & timing

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| NTP/PTP-disciplined clocks (analog+digital) + multi-timezone | ✅ have (enhance) | valuable | M5 | NTP RFC 5905; PTP IEEE 1588 / SMPTE ST 2059-2 |

### Overlays & control

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Count-up / count-down / down-then-up timers | 📋 | valuable | M11 | Generic; GPI/REST triggered |
| IDENTIFY (flash/highlight a tile) | 📋 | niche | M11 | Generic; control-API triggered |

### Overlays & metadata

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| On-tile format / codec / bitrate / colorimetry readout | 📋 | valuable | M10 | Readout of signal params; TS PSI (PMT/PCR) per ISO/IEC 13818-1 |

### Overlays & integration

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Data-bound custom text/data widgets | 📋 | niche | M11 | Generic; REST/JSON/SNMP/XML bindings |

### Inputs

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| SMPTE ST 2110 uncompressed IP ingest (-20/-30/-40) | 📋 | valuable | M12 | SMPTE ST 2110-10/-20/-30/-40; -21 shaping; -31 AES3 |
| ST 2110-22 (JPEG XS) lightly-compressed IP ingest | 📋 | niche | M12 | SMPTE ST 2110-22 (codec-agnostic, JPEG XS common); VSF TR-08; ISO/IEC 21122 |
| ST 2022-6 uncompressed SDI-over-IP ingest | 📋 | niche | M12 | SMPTE ST 2022-6 (HBRMT) |
| MPEG-TS ingest with full PSI/SI + program selection | ✅ have (enhance) | core | M4 | ISO/IEC 13818-1; RTP RFC 3550; VSF TR-01 |
| SRT ingest modes + encryption + stream-id | ✅ have (enhance) | core | M4 | SRT (carries MPEG-TS payload) |
| NDI ingest (High Bandwidth + HX) with HDR | ✅ have (enhance) | valuable | M4 | NDI (interop transport, runtime-loaded; license-gated) |
| OTT/ABR ingest: HLS/LL-HLS/DASH (+ RTSP/M3U) | ✅ have (enhance) | valuable | M4 | HLS RFC 8216; LL-HLS; MPEG-DASH ISO/IEC 23009-1; RTSP |
| WebRTC / RTMP contribution ingest | ✅ have (enhance) | valuable | M4 | RTMP (Adobe); WebRTC (W3C/IETF) |

### Inputs & resilience

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| ST 2022-7 hitless seamless protection (dual red/blue) | 📋 | valuable | M12 | SMPTE ST 2022-7:2019 (RTP-datagram level; applies to any RTP incl. ST 2110) |

### Inputs & timing

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| PTP / ST 2059-2 reference-clock timing | 📋 | valuable | M12 | SMPTE ST 2059-1/-2; IEEE 1588-2008; mandated (shall support) by ST 2110-10 |

### Inputs & control

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| NMOS IS-04/05 discovery & connection management | 📋 | valuable | M12 | AMWA IS-04, IS-05, BCP-002-01; JT-NM TR-1001 |
| NMOS IS-08 audio channel mapping | 📋 | niche | M12 | AMWA NMOS IS-08 |

### Control & security

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| NMOS IS-10 authorization (OAuth2/JWT) | 📋 | niche | M12 | AMWA IS-10; OAuth 2.0 RFC 6749; JWT RFC 7519 |

### Inputs & metadata

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Ancillary/VANC data extraction (captions/AFD/timecode/SCTE) | 📋 | valuable | M10 | SMPTE ST 2110-40 (ANC over IP), ST 2038 (VANC in TS), ST 291 |
| Embedded timecode extraction (display + alignment) | ✅ have (enhance) | niche | M5 | SMPTE ST 12-1/-2/-3; RP 188; ST 2110-40 |

### Inputs & color

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Per-input HDR/WCG recognition + mixed HDR/SDR on one wall | ✅ have (enhance) | valuable | M5 | ITU-R BT.2100 (PQ/HLG), BT.2020, S-Log3, BT.2446 |

### Outputs

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Multiview-as-a-stream output (ST 2110/-22, RIST, WebRTC additions) | ✅ have (enhance) | core | M12 | SMPTE ST 2110-20/-22, ST 2022-6; SRT, RIST, RTP/UDP, HLS/LL-HLS, NDI, WebRTC; MPEG-TS |

### Outputs & resilience

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| ST 2022-7 hitless redundancy on outputs (dual-NIC) | 📋 | valuable | M12 | SMPTE ST 2022-7 |
| System redundancy: hot-standby / N+1 / failover | ✅ have (enhance) | valuable | M9 | Generic HA (also dual PSU/NIC on hardware) |

### Control & operations

| Capability | Status | Relevance | Target | Standard |
|---|---|---|---|---|
| Soft/hardware control panels + browser/mobile remote | ✅ have (enhance) | valuable | M11 | Panels over Ember+/HTTP; HTML5/WebSocket/WebRTC monitor view |
| RBAC multi-user + audit log + config versioning | ✅ have (enhance) | valuable | M6 | RBAC; OAuth 2.0/JWT; align AMWA NMOS IS-10 |

## Production switcher (proposed — designed 2026-06-11)

A live production-switcher layer on the multiviewer engine, in generic industry vocabulary
(M/E stage, PGM/PVW, T-bar, dip, wipe, stinger, upstream/downstream keyer, clean feed, media
library, macro, memory). Designed in [production-switcher](docs/research/production-switcher.md)
and [media-playout](docs/research/media-playout.md); decided in
[ADR-0054](docs/decisions/ADR-0054.md)–[ADR-0059](docs/decisions/ADR-0059.md),
[ADR-M012](docs/decisions/ADR-M012.md), [ADR-RT008](docs/decisions/ADR-RT008.md),
[ADR-W021](docs/decisions/ADR-W021.md), [ADR-P007](docs/decisions/ADR-P007.md),
[ADR-T015](docs/decisions/ADR-T015.md), [ADR-C007](docs/decisions/ADR-C007.md),
[ADR-MV006](docs/decisions/ADR-MV006.md), [ADR-R011](docs/decisions/ADR-R011.md); PR-sized
slices in [production-switcher-backlog.md](docs/development/production-switcher-backlog.md)
(`SW-`). **Invariant posture:** switcher state is *sampled* by the output clock once per tick at
the frame-boundary control seam — transitions are pure `f(tick)`, never pacing (inv #1); the
control surface, macros, and monitors are physically incapable of back-pressuring the engine
(inv #10); every duration is an integer frame count at the exact rational output cadence, never
float fps/seconds (inv #3, [ADR-T015](docs/decisions/ADR-T015.md)). All rows 📐 (docs-only design
pass; no code), except the native-graphics row — explicitly deferred and **not** designed, so 📋.
**Target** follows the MVP boundary in
[production-switcher §18](docs/research/production-switcher.md): *M13* = the one-M/E MVP
milestone, *M13+* = the post-MVP waves (wipes, DVE, stinger, chroma/pattern keys, aux via the
output crosspoint, multi-M/E) — milestone definition lands in [ROADMAP.md](ROADMAP.md) with this
design pass.

| Capability | Status | Target | Design | Standard |
|---|---|---|---|---|
| M/E program/preview buses (PGM/PVW inside ONE program; same-tick second composite; frame-aligned take) | 📐 | M13 | [production-switcher §4](docs/research/production-switcher.md), [ADR-0054](docs/decisions/ADR-0054.md), [ADR-P007](docs/decisions/ADR-P007.md) | De-facto industry practice |
| Cut / auto / T-bar (idempotent absolute progress setter, conflated latest-wins; integer-frame rates; flip-flop; abort semantics) | 📐 | M13 | [ADR-0055](docs/decisions/ADR-0055.md), [ADR-T015](docs/decisions/ADR-T015.md) | De-facto industry practice |
| Mix / dip transitions (dip via any bus-selectable dip source; pinned linear-light dissolve law + optional perceptual progress curve) | 📐 | M13 | [ADR-0055](docs/decisions/ADR-0055.md), [ADR-C007](docs/decisions/ADR-C007.md) | De-facto industry practice (gamma-domain dissolve midpoint documented vs the linear-light law) |
| Wipes (pattern / softness / border fill / position / reverse / flip-flop; SDF masks) | 📐 | M13+ | [ADR-0055](docs/decisions/ADR-0055.md) Wave 2 | De-facto industry practice |
| DVE transitions (push/squeeze; per-tile transform + sub-pixel placement; wires the dormant fit/crop/rotation model) | 📐 | M13+ | [ADR-0055](docs/decisions/ADR-0055.md) Wave 2 | De-facto industry practice |
| Stinger / media transitions (pre-roll, trigger point, mix window; validated `trigger+mix ≤ duration`) | 📐 | M13+ | [ADR-0055](docs/decisions/ADR-0055.md) Wave 3, [ADR-0058](docs/decisions/ADR-0058.md), [media-playout §9](docs/research/media-playout.md) | De-facto industry practice |
| Fade to black (master stage after the DSKs; own rate; optional audio-follow master ramp) | 📐 | M13 | [ADR-0056](docs/decisions/ADR-0056.md), [ADR-0059](docs/decisions/ADR-0059.md) | De-facto industry practice |
| Upstream keyers (luma / linear fill+key / chroma / pattern / DVE PiP; clip/gain/invert; garbage matte; explicit key priority) | 📐 | M13 (linear/luma) · M13+ (chroma/pattern) | [ADR-0056](docs/decisions/ADR-0056.md), [ADR-C007](docs/decisions/ADR-C007.md) | De-facto industry practice |
| Downstream keyers (canvas-level, post-M/E pre-FTB; TIE / CUT / AUTO; on-clock render-plan stage, not the monitoring overlay bake) | 📐 | M13 | [ADR-0056](docs/decisions/ADR-0056.md), [ADR-0054](docs/decisions/ADR-0054.md) | De-facto industry practice |
| Multi-box composition source (background + z-ordered boxes as a bus-selectable source; drive-internal same-tick pre-pass) | 📐 | M13+ | [ADR-0056](docs/decisions/ADR-0056.md) | De-facto industry practice |
| Media library + media players (stills/clips/audio assets; cue/play/pause/seek; loop; play-on-take; EOF policy `hold_last_frame\|loop\|black\|auto_off`; frame-accurate start) | 📐 | M13 | [media-playout](docs/research/media-playout.md), [ADR-0057](docs/decisions/ADR-0057.md) | De-facto industry practice; transport semantics follow the open CasparCG AMCP model (cue/play/auto-chain) |
| Alpha media (NV12+A frame payload, 2.5 B/px — straight alpha in the payload, premultiplied exactly once in-kernel in linear light; bounded stinger mezzanine; import probe + reject-or-transcode) | 📐 | M13 (stills with alpha) · M13+ (alpha clips) | [ADR-0058](docs/decisions/ADR-0058.md), [media-playout §6](docs/research/media-playout.md) | FFmpeg-documented alpha decode set (ProRes 4444, qtrle, PNG/TGA sequences, VP9-alpha); HEVC-alpha rejected/import-transcoded |
| Native graphics renderer (templates, lower thirds, data-bound text, clock/score widgets, font management, preview-before-air, take-coupled graphics automation) | 📋 | M13+ | Explicitly deferred — **not designed in this pass** ([production-switcher §18](docs/research/production-switcher.md)); MVP production graphics are media-based alpha fills (NV12+A stills/clips, [ADR-0058](docs/decisions/ADR-0058.md)); the existing overlay text stack stays monitoring-only | De-facto industry practice |
| Audio-follow-video + master gain (equal-power AFV crossfades from the same state-machine window; FTB audio fade; gain-preserving mute; live meters ~30 Hz) | 📐 | M13 | [ADR-0059](docs/decisions/ADR-0059.md) | EBU R128 / ITU-R BS.1770 (loudness-normaliser interaction); else de-facto industry practice |
| Internally-derived tally (recursive contributes-to-program over the live composition graph; merged with external facts in one arbiter) | 📐 | M13 | [ADR-MV006](docs/decisions/ADR-MV006.md), [production-switcher §9](docs/research/production-switcher.md) | TSL UMD v3.1/v4.0/v5.0; AMWA NMOS IS-07 |
| Aux buses + clean feeds (bus-tap taxonomy `{program, clean, preview, me[n], aux[j]}`; clean = pre-DSK tap; aux = independent encode via the output←program crosspoint) | 📐 | M13 (clean tap) · M13+ (aux buses) | [production-switcher §4.4/§4.6](docs/research/production-switcher.md), [ADR-0054](docs/decisions/ADR-0054.md) | De-facto industry practice |
| Macros + memories (control-plane command sequencer with `wait_frames`/`wait_ms`; memories = salvo + recall-scope masks; multi-op frame-boundary batches) | 📐 | M13 | [production-switcher §10](docs/research/production-switcher.md), [ADR-M012](docs/decisions/ADR-M012.md), [ADR-W021](docs/decisions/ADR-W021.md) | Batch-at-one-frame semantic per the open obs-websocket protocol (`SERIAL_FRAME`); else de-facto industry practice |
| Switcher REST/WS surface + operator panel + shortcuts (`/api/v1/switcher/*` bare verbs, plan/take classification, correlated events; SPA panel, keyboard CUT/AUTO/bus keys, no-color-alone bus state) | 📐 | M13 | [ADR-W021](docs/decisions/ADR-W021.md), [ADR-RT008](docs/decisions/ADR-RT008.md), [ADR-M012](docs/decisions/ADR-M012.md), [ADR-P007](docs/decisions/ADR-P007.md) | OpenAPI 3.1 + AsyncAPI; RFC 9457 errors (house API conventions) |
| External-control-friendly contract (long-lived WS snapshot+deltas; stable string ids; idempotent single-shot commands; boolean state feedback) | 📐 | M13 (contract) · M13+ (official gateway module, OSC) | [ADR-W021](docs/decisions/ADR-W021.md) §13 | De-facto industry practice (control-surface gateways) |

## Accessibility & internationalization

WCAG 2.2 AA + i18n for the management web app — see [accessibility](docs/web/accessibility.md) and [internationalization](docs/web/internationalization.md). All 📐 designed for **M7**.

| Capability | Status | Target | Notes |
|---|---|---|---|
| WCAG 2.2 AA conformance + CI a11y gate (jsx-a11y, jest-axe, @axe-core/playwright) | 📐 | M7 | Per-area plan in docs/web/accessibility.md (ADR-W009). Layered lint + axe component/E2E scans fail the build on new violations; manual SR matrix (NVDA, JAWS, VoiceOver macOS+iOS) per milestone. Adds 2.2 AA SCs 2.4.11, 2.5.7, 2.5.8, 3.3.8. |
| Accessible layout-editor equivalent path (Cells list + Inspector, keyboard grab/move/drop) | 🔵 | M7 | ADR-W010. Non-canvas DOM editor drives the same layout model as react-konva; numeric x/y/w/h/z/rotation + steppers + z-order buttons satisfy SC 2.5.7 by single pointer; arrow-nudge/resize satisfies 2.1.1; drawn pseudo focus ring on canvas. dnd-kit KeyboardSensor (custom 1px/grid coordinateGetter) for DOM reorder/palette-drop. |
| No-color-alone realtime status: triple-encoded tally/alarm severity (color+icon/shape+text) | 📐 | M7 | ADR-W011. SC 1.4.1; CVD-safe Wong/Okabe-Ito palette contrast-verified both themes (1.4.11 3:1, 1.4.3 4.5:1). |
| aria-live announcement strategy (pre-mounted status/alert + role=log, debounced) | 📐 | M7 | SC 4.1.3. Global announcer/message-bus; polite for ~90%, assertive only for critical alarm raises; per-tile debounce/coalesce (~1-2s tunable) + aria-busy; alarm history role=log. |
| Accessible audio meters (silent <meter>/role=meter gauges, threshold-only announcements) | 📐 | M7 | Never stream meter values over aria-live; focusable wrapper + aria-valuetext for on-demand read; announce only clip (assertive) / silence (polite). |
| prefers-reduced-motion across meters, alarm pulses, tile + drag transitions | 📐 | M7 | matchMedia/CSS gate to static state; pulse/blink never the sole signal; nothing flashes >3x/s (SC 2.3.1). |
| Target size, focus-visible, focus-not-obscured, contrast in dark+light themes | 📐 | M7 | SC 2.5.8 (24x24 CSS px or spacing exception), 2.4.7, 2.4.11 (scroll-margin), 1.4.3/1.4.11/1.4.10 verified per theme; accessible TanStack tables (th[scope], single moving aria-sort, sort announcements). |
| i18n framework: Lingui v5 + ICU MessageFormat + extraction/pseudolocale/TMS in CI | 🔵 | M7 | ADR-W012. SWC macro + Vite plugin; auto content-hash IDs; <Trans> rich-text; CLDR plural/select; lazy per-locale catalogs; pseudoLocale + fail-on-new-untranslated in CI. |
| Intl-based locale formatting (date/time/number/relative) + multi-timezone clock + timecode | 🔵 | M7 | ECMAScript Intl owns all value formatting, memoized per (locale,options); one cached Intl.DateTimeFormat per displayed timezone (locale orthogonal to zone); SMPTE timecode structure app-controlled; dB/fps formatted as number + literal (not Intl units). |
| RTL support incl. logical properties, Tailwind logical utilities, and konva canvas mirroring | 📐 | M7 | dir on <html> via feature-detected getTextInfo() + static fallback map; canvas mirrored by explicit mirroredX=stageWidth-(x+width); selective mirroring (nav/chevrons yes; video/transport/timecode/numbers no); SC 3.1.1/3.1.2. |
| Locale negotiation + client-localized RFC 9457 API errors from stable code/type | 📐 | M7 | navigator.languages via RFC 4647 lookup + persisted override + Accept-Language; OpenAPI 3.1 error schema exposes machine code/type the client maps to localized ICU; server title/detail fallback only. |
| Localization boundary enforcement (chrome localized; user/operator content verbatim) | 📐 | M7 | Lint guidance + reviewer checklist: never wrap source names/overlay text/template names/IDs in t/<Trans>; never hardcode chrome; user content rendered with lang/dir=auto. |
