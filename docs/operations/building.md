# Building Multiview from Source

This guide covers building the `multiview` binary (and its web UI) from source on **Linux** and
**macOS** — the only supported platforms. **There is no Windows build.**

Multiview is a Cargo workspace. The **default build is pure-Rust, LGPL-clean, and needs no native
libraries** (`cargo check` is green in GPU-free CI). Everything that links FFmpeg/libav, touches a
GPU, or loads the NDI SDK is **behind off-by-default Cargo features**. You opt into hardware and
codecs with umbrella feature presets at build time, and the engine then **auto-negotiates** the
best available backend at runtime.

> Read first: the [conventions](../architecture/conventions.md) are the source of truth for crate
> names, feature flags, and the licensing model. The build/licensing rationale lives in
> [ADR-0011 (platforms)](../decisions/ADR-0011.md) and
> [ADR-0012 (LGPL-clean default)](../decisions/ADR-0012.md); deep design is in the
> [core-engine brief](../research/core-engine.md) and the
> [efficiency brief](../research/efficiency.md).

---

## 1. Quick start

```bash
# 1. Clone
git clone https://github.com/aperim/multiview.git
cd multiview

# 2. Default build — pure Rust, no native deps, LGPL-clean, no GPU/FFmpeg/NDI
cargo build --release

# 3. Validate a config and run the daemon (software/CPU path)
cargo run --release -p multiview-cli -- validate examples/multiview.toml
cargo run --release -p multiview-cli -- run      examples/multiview.toml
```

The default binary builds and runs anywhere a Rust toolchain works. To do real media work you must
add a media/GPU **feature preset** (see [§5](#5-feature-flag-build-profiles)), which requires the
system prerequisites below.

---

## 2. Prerequisites

### 2.1 Rust toolchain (all platforms)

The toolchain is **pinned** via `rust-toolchain.toml` (stable, edition 2021). Install
[rustup](https://rustup.rs); it picks up the pinned channel automatically on first build. The MSRV
is documented in the README.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup show                      # confirms the pinned stable toolchain is active
rustup component add clippy rustfmt
```

For media/GPU builds you also need a C toolchain and **`libclang`** (bindgen generates the libav
bindings); macOS additionally requires `bindgen ≥ 0.70` for the aarch64 clang target (already
pinned).

### 2.2 FFmpeg / libav (only for `ffmpeg`-feature builds)

The media crates ([`multiview-ffmpeg`](../architecture/conventions.md#3-canonical-crate-map),
`multiview-input`, `multiview-output`, `multiview-audio`) link the system **libav\*** via `rsmpeg`. The
default build does **not** link FFmpeg — only feature presets that enable `ffmpeg` do.

> [!IMPORTANT]
> **The default Multiview build is LGPL-clean.** That means FFmpeg must be built **`--enable-shared`
> without `--enable-gpl` and without `--enable-nonfree`** (NVENC/NVDEC come from MIT
> `nv-codec-headers`; scaling is in-house via `scale_cuda`, never `scale_npp`). A distro/Homebrew
> FFmpeg may already be GPL or nonfree — verify before relying on the LGPL-clean guarantee
> ([§6](#6-licensing--build-profiles)).

#### macOS (Homebrew)

```bash
brew install ffmpeg pkg-config
ffmpeg -version          # this dev box ships FFmpeg 8.1.1 (Homebrew)
ffmpeg -hwaccels         # expect videotoolbox
```

> [!WARNING]
> **Dev-box caveat:** the Homebrew FFmpeg on this machine is **8.1.1** built with
> `--enable-gpl --enable-version3 --enable-libx264 --enable-libx265 --enable-openssl`. That is
> **fine for local development and testing**, but a binary linked against it is **GPL** and is
> **not** the LGPL-clean redistributable artifact. For release/redistribution, build FFmpeg
> yourself with the LGPL flags (see [§6](#6-licensing--build-profiles)) and point Multiview at it via
> `PKG_CONFIG_PATH`. VideoToolbox HW accel is present in this Homebrew build.

#### Linux (system libav)

Install the FFmpeg dev packages, then verify protocols/hwaccels:

```bash
# Debian/Ubuntu (note: distro FFmpeg is often GPL — see §6)
sudo apt-get install -y \
  libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev \
  libavdevice-dev libswscale-dev libswresample-dev \
  pkg-config clang libclang-dev

ffmpeg -protocols | grep -E 'srt|rtsp|rtmp|hls'   # SRT 404s silently if libsrt isn't linked
ffmpeg -hwaccels                                  # vaapi / cuda depending on build
```

`pkg-config` must find the `libav*.pc` files (and `srt.pc` for SRT). If you build a custom FFmpeg,
export its location:

```bash
export PKG_CONFIG_PATH=/opt/multiview-ffmpeg/lib/pkgconfig:$PKG_CONFIG_PATH
```

### 2.3 Optional hardware acceleration (Linux)

Enable only the backend your hardware has; the planner falls back to CPU if absent at runtime.

| Backend | Feature | System prerequisites |
|---------|---------|----------------------|
| **NVIDIA** (NVDEC/NVENC, CUDA compositor) | `cuda` | CUDA toolkit + `nv-codec-headers` to build; at runtime the host NVIDIA driver provides `libcuda` / `libnvidia-encode` / `libnvidia-decode`. Driver must satisfy the targeted Video Codec SDK (e.g. **SDK 13.0 → driver ≥ 570**). |
| **Intel/AMD** (VAAPI) | `vaapi` | `libva-dev` + a VAAPI driver (`intel-media-va-driver` / `mesa-va-drivers`); access to `/dev/dri`. |
| **Intel QSV** (oneVPL) | `qsv` | oneVPL runtime + dispatcher (`libvpl-dev`); Gen12+ recommended. |

> macOS has **no** CUDA/NVENC/VAAPI/QSV — VideoToolbox + Metal is the only hardware path
> (the `apple` preset). See the [per-platform backend matrix](../architecture/conventions.md#5-canonical-technical-invariants)
> and the [core-engine brief §6.4](../research/core-engine.md).

### 2.4 NDI SDK (optional, proprietary — never vendored)

NDI is **feature-gated (`ndi`) and off by default**. The SDK is **proprietary (royalty-free,
attribution required, redistribution restricted)** and is **never vendored into the repo**. Multiview
uses a **runtime dynamic-load** path (`NDIlib_v6_load()`), so you build *with* the `ndi` feature but
provide the runtime yourself.

1. Download the **NDI 6 SDK** from <https://ndi.video> and accept its EULA.
2. Make the runtime discoverable (Linux `LD_LIBRARY_PATH` / standard NDI runtime path; macOS
   bundled dylib with `@rpath`).
3. Honor the attribution obligations: link to ndi.video near NDI uses, the About-box notice
   **"NDI® is a registered trademark of Vizrt NDI AB"**, and contact NDI before putting "NDI" in a
   product name. HX (compressed) NDI needs the separate **paid** Advanced SDK (`ndi-advanced`).

See [io/ndi.md](../io/ndi.md), [ADR-0008](../decisions/ADR-0008.md), and the licensing detail in
[§6](#6-licensing--build-profiles).

### 2.5 Node.js (only to rebuild the web UI)

The management SPA (`web/`, React 19 + TypeScript + Vite) is embedded into the binary via
`rust-embed`. You only need Node when changing the UI; release builds embed a prebuilt SPA.

```bash
# Node 20+ recommended
cd web && npm install && npm run build      # emits the embeddable static bundle
# or, from the repo root via dev automation:
cargo xtask build-web
```

The `embed-web` feature ([§5](#5-feature-flag-build-profiles)) bakes that bundle into the binary;
during UI development use the Vite dev server with the API proxy instead.

---

## 3. Building

### 3.1 Default (LGPL-clean, no native deps)

```bash
cargo build --release            # the whole workspace, default features
cargo build --release -p multiview-cli   # just the `multiview` binary
```

### 3.2 Workspace checks (what CI runs GPU-free)

```bash
cargo check --workspace          # pure-Rust trait/type layer compiles without GPUs/FFmpeg
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace           # software/CPU pipeline + golden-frame tests
cargo deny check                 # license + advisory gate (deny.toml)
```

### 3.3 Dev automation (`xtask`)

Project-specific build orchestration lives in `xtask` (build the web bundle, generate the
OpenAPI/AsyncAPI specs, lint, packaging helpers):

```bash
cargo xtask --help
cargo xtask build-web            # build + stage the SPA for embedding
cargo xtask gen-openapi          # regenerate the API spec
```

---

## 4. Running

```bash
# Validate a config without starting the engine
cargo run --release -p multiview-cli -- validate examples/multiview.toml

# Run the daemon (loads config, wires engine + control plane)
cargo run --release -p multiview-cli -- run examples/multiview.toml

# Or run the built binary directly
./target/release/multiview run examples/multiview.toml
```

The control plane (REST `/api/v1`, WebSocket `/api/v1/ws`, interactive docs at `/docs`) and the
embedded SPA are served by `multiview-control`. Health probes (`/livez`, `/readyz`) run on a separate
lightweight runtime so they never contend with the video threads. See the
[example configs](../../examples/) and [io/inputs.md](../io/inputs.md).

---

## 5. Feature-flag build profiles

The canonical feature taxonomy is in
[conventions §4](../architecture/conventions.md#4-feature-flag-taxonomy-canonical). `multiview-cli`
exposes **umbrella presets** that roll up the right per-stage backends for a platform. Build with a
preset via `--features` (disable defaults only if you want a minimal codec-less build):

```bash
# NVIDIA Linux box (NVDEC/NVENC + CUDA compositor + wgpu baseline + FFmpeg)
cargo build --release -p multiview-cli --features nvidia

# macOS native (VideoToolbox + Metal + FFmpeg)
cargo build --release -p multiview-cli --features apple

# Linux Intel/AMD (VAAPI + QSV + wgpu + FFmpeg)
cargo build --release -p multiview-cli --features linux-vaapi

# Everything non-GPL (still LGPL-clean / redistributable)
cargo build --release -p multiview-cli --features full

# Add proprietary NDI (runtime-loaded SDK) on top of any preset
cargo build --release -p multiview-cli --features "nvidia,ndi"

# Opt into GPL codecs (x264/x265) — makes the whole build GPL
cargo build --release -p multiview-cli --features "linux-vaapi,gpl-codecs"

# Embed the prebuilt web UI into the binary (single deployable)
cargo build --release -p multiview-cli --features "apple,embed-web"
```

### Preset → backend map

| Preset | Expands to | Platform / use |
|--------|-----------|----------------|
| *(default)* | `software` + `wgpu` + `openapi` (no `ffmpeg`, no GPU codec, no NDI) | LGPL-clean, no-native-deps CI / portable |
| `nvidia` | `cuda` + `ffmpeg` + `wgpu` | Linux + NVIDIA (NVDEC→CUDA→NVENC island) |
| `apple` | `videotoolbox` + `metal` + `ffmpeg` | macOS native (VT→Metal→VT island) |
| `linux-vaapi` | `vaapi` + `qsv` + `ffmpeg` + `wgpu` | Linux Intel/AMD (dma-buf island) |
| `full` | everything non-GPL | Cross-platform max non-GPL build |

### Standalone feature flags (combine with presets)

| Feature | Effect | Default |
|---------|--------|---------|
| `software` | CPU codec/compositor tier (universal fallback) | **on** |
| `ffmpeg` | Link libav for demux/decode/encode | off |
| `cuda` / `vaapi` / `qsv` / `videotoolbox` | Per-stage HW codec backends (runtime-negotiated) | off |
| `wgpu` | Default cross-platform compositor backend | on (default) |
| `metal` / `cuda` | Vendor fast-path compositor backends | off |
| `gpl-codecs` | x264/x265 → **build becomes GPL** | off |
| `ndi` | NDI in/out via runtime `NDIlib_v6_load()` (proprietary SDK) | off |
| `ndi-advanced` | NDI HX (compressed) — separate **paid** SDK | off |
| `libass` | Subtitle ingest/render | off |
| `openapi` | OpenAPI spec + Scalar docs | **on** |
| `embed-web` | Embed the SPA into the binary | off |
| `webrtc` | WHEP preview transport | off |

> Hardware/FFI/codec backends are **additive** and **never change the public API** — they only add
> available runtime paths. The planner ([HAL negotiation](../architecture/conventions.md#5-canonical-technical-invariants))
> picks the cheapest path that actually exists on the host and falls back to CPU. Building a backend
> in is necessary but not sufficient: the corresponding driver/SDK must be present at runtime.

---

## 6. Licensing & build profiles

The **effective license of the artifact depends on the features you enable.** Project code is under the
**Multiview Source-Available Non-Commercial License, Version 1.0** (source-available, **not** open
source); the feature presets determine what third-party code gets linked. Full rationale:
[ADR-0012](../decisions/ADR-0012.md) and [conventions §7](../architecture/conventions.md#7-licensing-model-build-profiles).

| Profile / features | Composition | Effective status |
|--------------------|-------------|------------------|
| **default** (no `gpl-codecs`/`ndi-advanced`) | Source-available project code + dynamically-linked **LGPL-2.1** FFmpeg (no `--enable-gpl`, no `--enable-nonfree`; NVENC/NVDEC via MIT `nv-codec-headers`; `scale_cuda` not `scale_npp`; native AAC + GnuTLS) | **Redistributable, LGPL-clean** |
| **+ `gpl-codecs`** | + x264/x265 (GPL FFmpeg) | Whole product **GPL-2.0-or-later** |
| **+ nonfree** (libnpp / FDK-AAC / OpenSSL) | nonfree FFmpeg | **NOT redistributable** — internal/personal only |
| **+ `ndi`** | + proprietary NDI runtime (royalty-free) | Source-available project code **+ NDI EULA + mandatory attribution/branding** |
| **+ `ndi-advanced`** | + NDI Advanced SDK (HX H.264/HEVC) | Separate **paid** commercial license + codec royalties |

**Build-your-own LGPL FFmpeg (release path).** Because distro/Homebrew FFmpeg is frequently GPL,
compile an LGPL-clean FFmpeg yourself and link Multiview against it. Key flags and verification:

```bash
# Illustrative LGPL-clean configure (NVIDIA example):
./configure --prefix=/opt/multiview-ffmpeg --enable-shared \
  --enable-ffnvcodec --enable-nvenc --enable-nvdec \
  # NO --enable-gpl, NO --enable-nonfree, NO --enable-libnpp, NO --enable-libx264/x265

# Verify the build is LGPL-clean before trusting it:
ffmpeg -buildconf | grep -E '\-\-enable-(gpl|nonfree|libnpp|libx264|libx265)' \
  && echo "NOT LGPL-clean" || echo "LGPL-clean"
```

CI gates licenses/advisories with **`cargo-deny`** (`deny.toml`) and reports the **effective license
per built artifact**. Prefer **dynamic linking** of FFmpeg everywhere (the LGPL relink right),
including inside containers and macOS bundles. Codec **patent** licensing (H.264/HEVC/AAC pools) is
a separate obligation from software copyright and may apply regardless of build flags.

---

## 7. Platform notes

### 7.1 Linux

- **Runtime HW probe.** On startup the engine `dlopen`s `libnvcuvid`/`libnvidia-encode` (or opens a
  throwaway VAAPI/QSV session) and **fails loudly or falls back** to VAAPI/CPU if a backend is
  absent. Compiling a backend in does not guarantee it at runtime.
- **NVIDIA containers.** The image ships **no** driver libs — the **NVIDIA Container Toolkit injects
  the host driver** at runtime. You **must** set
  `NVIDIA_DRIVER_CAPABILITIES=compute,utility,video` — the `video` capability is **mandatory** for
  NVENC/NVDEC and is **not** in the default `utility,compute`; omitting it silently disables
  encode/decode while CUDA still appears to work.
- **VAAPI.** Pass `/dev/dri` and add the host `render`/`video` GIDs dynamically
  (`--group-add $(getent group render | cut -d: -f3)`); GIDs vary per host — never hardcode.
- **Workspace `resolver = "2"`** is mandatory so a Linux-only build/test dep can't leak into a
  macOS build.

### 7.2 macOS

- **Native only** — there is no container GPU path on macOS (no Metal/VideoToolbox in containers).
- **Universal2 release.** Build each target (`aarch64-apple-darwin`, `x86_64-apple-darwin`), `lipo`
  into a single universal2 binary, bundle the LGPL `libav*` (and optional `libndi`) dylibs with
  `@rpath`/`@loader_path` install names, then **inside-out codesign** every dylib + the executable
  (hardened runtime, secure timestamp, consistent Team ID — `codesign --deep` is unreliable), and
  finally `notarytool` + `stapler`.
- Verify HW: `ffmpeg -hwaccels` lists `videotoolbox`; the local Homebrew FFmpeg 8.1.1 has it.

See [ADR-0011](../decisions/ADR-0011.md) for the full platform decision and consequences.

---

## 8. Troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| `pkg-config` can't find libav | FFmpeg dev packages missing, or custom build not on `PKG_CONFIG_PATH`. |
| SRT inputs silently 404 | `srt` not linked in FFmpeg — check `ffmpeg -protocols \| grep srt` and that `srt.pc` is on `PKG_CONFIG_PATH`. |
| NVENC/NVDEC "work but no HW" in a container | `NVIDIA_DRIVER_CAPABILITIES` missing `video`; or host driver older than the targeted SDK (e.g. SDK 13.0 needs driver ≥ 570). |
| VAAPI permission denied | Missing `/dev/dri` passthrough or host `render`/`video` GID not added. |
| NDI feature won't build | The `ndi` feature still needs the NDI 6 SDK headers/runtime present; the SDK is never vendored — install it and accept the EULA. |
| bindgen / libclang errors (macOS) | Install `libclang` (or use prebuilt bindings); needs `bindgen ≥ 0.70` for the aarch64 clang target. |
| "build is GPL" surprise on release | Your linked FFmpeg has `--enable-gpl`/`--enable-nonfree` (common in Homebrew/distro). Build an LGPL-clean FFmpeg ([§6](#6-licensing--build-profiles)). |

---

## See also

- [Conventions](../architecture/conventions.md) — canonical crate map, features, invariants (source of truth).
- [ADR-0011](../decisions/ADR-0011.md) — cross-platform targets (Linux containers + macOS universal2).
- [ADR-0012](../decisions/ADR-0012.md) — LGPL-clean default build; GPL/nonfree/NDI opt-in.
- [core-engine brief](../research/core-engine.md) — §6 backend matrix, §17 build & deployment.
- [efficiency brief](../research/efficiency.md) — §6 smallest-footprint build & runtime.
- [io/ndi.md](../io/ndi.md) — NDI integration and licensing obligations.
