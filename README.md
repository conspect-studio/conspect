# Multiview

**Turn many live video feeds into one dependable multiviewer.** Multiview ingests your
cameras, encoders and streams, composites them into a clean, templated wall on the GPU, and
serves that wall out in the formats your facility already uses — engineered to run well on
ordinary hardware and to **never drop a frame**.

<!-- badges: CI / license / container — kept minimal on purpose -->
[![License: Source-Available Non-Commercial](https://img.shields.io/badge/license-Source--Available%20Non--Commercial-blue.svg)](#licensing)
[![Container: GHCR](https://img.shields.io/badge/container-ghcr.io%2Faperim%2Fmultiview-2496ED.svg)](https://github.com/aperim/multiview/pkgs/container/multiview)

> [!NOTE]
> **Early stage, under active development.** The architecture, full API/UI design, ADRs and
> research are complete, and the Rust foundation is built and tested. The Docker quick-start
> below runs today (composite → encode → HLS + web UI/API); the live RTSP/NDI/RTMP/SRT output
> servers and hardware-accelerated paths are landing per the **[roadmap](ROADMAP.md)**. See
> **[FEATURES.md](FEATURES.md)** for the live, per-feature status matrix.

> [!NOTE]
> **New here?** [**WHY.md**](WHY.md) is the short story of why Multiview exists — a tool to help
> emerging artists and technicians get hands-on with live-production concepts on commodity hardware.

---

## Why Multiview

- **Output that never falters.** A single fixed-cadence clock emits one valid, correctly-timed
  frame every tick — *forever*, independent of any input. A dead camera shows a "no signal"
  tile; it never freezes, stalls or corrupts the wall.
- **Runs on commodity hardware.** NVIDIA, Intel, AMD or Apple Silicon — or pure software as a
  universal fallback. Multiview decodes at display size, stays NV12 end-to-end, keeps frames on
  the GPU, and sheds load tile-by-tile under pressure before the program output is ever touched.
- **Many sources in, many destinations out.** RTSP, HLS, MPEG-TS, SRT, RTMP, NDI, files and
  synthetic test patterns in; composite **once** and fan the same stream out to RTSP, HLS/LL-HLS,
  NDI, RTMP and SRT.
- **Built to be operated.** Declarative TOML config, an embedded web UI, and a REST/WebSocket/SSE
  API with interactive OpenAPI docs — scriptable and automatable end to end.
- **Source-available and license-clean.** Pure Rust under the **Multiview Source-Available
  Non-Commercial License** — free for genuine personal/home and other non-commercial use (a
  commercial licence is required otherwise) — with a default build that is LGPL-clean.

---

## Quick start

The fastest way to see it running. This brings up the engine plus a small companion that
publishes a **synthetic** test feed (no real or private sources), composites a 2×2 wall, encodes
it, serves the result as HLS, and serves the web UI + API on `:8080`.

```bash
git clone https://github.com/aperim/multiview.git
cd multiview
docker compose -f deploy/compose.yaml up -d
```

Then open:

- **Web UI** — <http://[::1]:8080/> (manage the engine)
- **API playground** — <http://[::1]:8080/docs> (interactive OpenAPI / Scalar)
- **The multiview** — `vlc http://[::1]:8888/multiview.m3u8` (or any HLS player)

Edit [`deploy/config/multiview.toml`](deploy/config/multiview.toml) to point a tile at your own
source and re-run `up -d`. Tear down with `docker compose -f deploy/compose.yaml down -v`.

> The default image is LGPL-clean and encodes **MPEG-2** (open it in VLC/ffplay). For
> browser-friendly **H.264**, use the `-gpl` image. GPU one-liners (NVIDIA / VAAPI) and the full
> configuration reference are in **[docs/operations](docs/operations/)**.

---

## Install

| Method | Get it |
|--------|--------|
| **Container (recommended)** | `docker pull ghcr.io/aperim/multiview:latest` (multi-arch `amd64`+`arm64`, SLSA provenance + cosign-signed). NVIDIA variant: `:latest-nvidia`. |
| **Prebuilt binaries** | Linux (`x86_64`/`aarch64`) and macOS (Apple Silicon/Intel, signed + notarized) on the [Releases page](https://github.com/aperim/multiview/releases). Requires FFmpeg 7.x on the host. |
| **From source** | `cargo build` (pure-Rust, LGPL-clean default). Feature presets and the FFmpeg/toolchain requirements are in **[docs/operations/building.md](docs/operations/building.md)**. |

---

## What it does

Multiview is a headless, scriptable compositor and router. It samples many independent live
inputs into a fixed, templated canvas and encodes that canvas **once per rendition** — then fans
the same packets to every transport. Inputs are *sampled*, never allowed to *pace* the output, so
one misbehaving source can never warp or stall the wall.

| Area | In brief |
|------|----------|
| **Inputs** | RTSP · HLS · MPEG-TS · SRT · RTMP · NDI · file · synthetic — supervised reconnect, jitter buffering, per-input timestamp normalization. |
| **Compositor** | A custom GPU-native pass: scale + place + per-tile colour-convert + linear-light blend + overlays. Hot-reconfigurable grid/PiP layouts. |
| **Outputs** | RTSP · HLS · Apple LL-HLS · NDI · RTMP · SRT via encode-once-mux-many. |
| **Audio** | Per-input decode/resample/mix, discrete tracks + an EBU R128 program bus, silence-fill on dropout. |
| **Subtitles & overlays** | CEA-608/708, DVB-sub, teletext, WebVTT/SRT/ASS; text, clocks, logos, tally, alert cards, audio meters. |
| **Control & preview** | Embedded React web UI, axum REST/WS/SSE API with OpenAPI 3.1, and sub-second WHEP/MJPEG preview — strictly isolated from the program path. |

The complete capability/status matrix is in **[FEATURES.md](FEATURES.md)**.

---

## Documentation

| Where | What |
|-------|------|
| **[docs/architecture](docs/architecture/)** | System [overview](docs/architecture/overview.md), [pipeline](docs/architecture/pipeline.md), [timing](docs/architecture/timing-and-sync.md), [resilience](docs/architecture/resilience.md), [color](docs/architecture/color.md) — and the [canonical conventions](docs/architecture/conventions.md) (source of truth for names, flags, invariants). |
| **[docs/research](docs/research/)** | Deep, verification-hardened design briefs ([index](docs/research/README.md)). |
| **[docs/decisions](docs/decisions/)** | Architecture Decision Records ([index](docs/decisions/README.md)) capturing every load-bearing choice. |
| **[ROADMAP.md](ROADMAP.md) · [FEATURES.md](FEATURES.md)** | The milestone plan and the per-feature status. |
| **API** | Live OpenAPI 3.1 spec at `/api/v1/openapi.json`, interactive Scalar docs at `/docs`. |

---

## Platforms

Linux (x86_64 + aarch64, containerised) and macOS (Apple Silicon + Intel, native). Hardware
acceleration via NVIDIA (NVDEC/NVENC/CUDA), Intel/AMD (VAAPI/QSV), and Apple (VideoToolbox/Metal),
with a universal software fallback. **No Windows.** Full matrix in
[docs/architecture/hardware-and-efficiency.md](docs/architecture/hardware-and-efficiency.md).

---

## Audio over IP — open standards, not Dante

Multiview transports audio over IP using **[AES67](https://en.wikipedia.org/wiki/AES67)** and
**SMPTE ST 2110-30** — open, royalty-free industry standards. Audio is announced and discovered
with SAP/SDP and synchronised with PTP (IEEE 1588), so any AES67-compatible device on the network
can send or receive a Multiview audio stream without a proprietary licence or controller.

**This still reaches Dante® networks.** Dante devices that support **Dante's own AES67 mode** can
discover (via SAP) and subscribe to Multiview's AES67 streams, and Multiview can receive theirs —
that interop path is exactly what AES67 was created to provide between otherwise-incompatible
audio-over-IP systems. So choosing open standards does not cut you off from a Dante facility; it
meets it on the common, standardised ground.

**Why we don't implement Dante natively — our position.** We originally hoped to support Dante and
its commercial variants natively, alongside the open standards. When we asked, Audinate® — the maker
of Dante, and, like us, an Australian company — told us that their SDKs are "intended for products
operating under commercial licensing agreements" and that they were therefore "not able to provide
guidance or support for integrating Dante functionality into open-source projects."

We'll let that speak for itself. We were disappointed — especially to see an Australian company close
the door on open, non-commercial, educational work. It's their call to make; ours is to build on open
standards anyone can learn. Multiview therefore implements the open AES67 / SMPTE ST 2110-30
standards, which are free for anyone to use and still reach Dante networks through Dante's own AES67
mode.

If Audinate's terms for open-source and community projects change, we would welcome revisiting native
Dante support. We will update this section if and when that happens.

> *Dante® and Audinate® are registered trademarks of Audinate Pty Ltd; other Dante-family names
> (e.g. Dante AV, Dante Domain Manager) are trademarks of Audinate Pty Ltd. Multiview is an
> independent project of Aperim Pty Ltd and is not affiliated with, endorsed by, or sponsored by
> Audinate — the marks are used nominatively, to identify Audinate's products and the Dante
> audio-networking standard only.*

---

## Licensing

Project code is **source-available**, licensed under the **Multiview Source-Available
Non-Commercial License** (see [`LICENSE`](LICENSE)) — © Aperim Pty Ltd. It is **free** for genuine
personal, home, and other non-commercial use, plus three free exceptions (First Nations Owned
Broadcasters; small Community Broadcasters; smaller Content Creators) defined in the License. All
other use — businesses, education, government, productization/appliances, and streamers/creators —
is Commercial Use and requires a paid Commercial License; see
[`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md) (licensing@aperim.com). This is a source-available
licence, **not** an open-source or free-software licence.

The **default build remains LGPL-clean** (FFmpeg linked LGPL; all scaling/compositing in-house).
Two capabilities are strictly opt-in and change the licensing of the resulting build:

- **`gpl-codecs`** — adds x264/x265 → the build becomes **GPL**.
- **`ndi`** — uses the proprietary, runtime-loaded NDI SDK (never vendored) under its EULA with
  mandatory attribution: *NDI® is a registered trademark of Vizrt NDI AB.*

Codec **patent** licensing (H.264/HEVC/AAC) is separate from software copyright and may apply to
your outputs regardless of build flags. Full model: [conventions.md §7](docs/architecture/conventions.md)
and [ADR-0012](docs/decisions/ADR-0012.md).

---

## Contributing

Issues and PRs are welcome — please read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md) first. Security reports: see [SECURITY.md](SECURITY.md).
