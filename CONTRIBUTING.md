# Contributing to Multiview

Thanks for your interest in **Multiview** — an efficient, hardware-accelerated, Rust live
video multiview generator. This guide covers everything you need to get a dev environment
running, build and test the workspace, follow our coding/feature conventions, and propose
changes (including new architecture decisions).

> **Read this first:** the single source of truth for naming, structure, feature flags,
> invariants, and licensing is
> [`docs/architecture/conventions.md`](docs/architecture/conventions.md). Where any other
> doc disagrees, **conventions wins**, and the Rust code is the ultimate authority. Deep
> design rationale lives in the briefs under [`docs/research/`](docs/research/) and the
> decisions under [`docs/decisions/`](docs/decisions/).

---

## Table of contents

- [Code of conduct & ground rules](#code-of-conduct--ground-rules)
- [Development environment](#development-environment)
- [Workspace layout](#workspace-layout)
- [Build, test, lint](#build-test-lint)
- [Feature-flag conventions](#feature-flag-conventions)
- [Coding conventions](#coding-conventions)
- [Commit & PR conventions](#commit--pr-conventions)
- [Proposing an architecture decision (ADR)](#proposing-an-architecture-decision-adr)
- [Licensing & DCO](#licensing--dco)

---

## Code of conduct & ground rules

- Be respectful and constructive. Assume good faith.
- Keep changes focused; one logical change per PR.
- Don't break the **load-bearing invariants** in
 [conventions §5](docs/architecture/conventions.md). The output-clock guarantee,
 per-tile last-good-frame, NV12-throughout, the color pipeline order, and engine
 isolation are not negotiable without an ADR.
- Security issues: see [`SECURITY.md`](SECURITY.md) — do **not** open a public issue.

---

## Development environment

Multiview targets **Linux** (x86_64 + aarch64) and **macOS** (Apple Silicon + Intel). There
is **no Windows support**. The default build is **pure-Rust, LGPL-clean, and has no native
dependencies** — you can build and run the full software pipeline with nothing but a Rust
toolchain.

### Required: Rust toolchain

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (stable channel,
edition **2021**), with `rustfmt`, `clippy`, and the Linux/macOS targets preselected.
Install via [rustup](https://rustup.rs) and the pin is honoured automatically:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cd multiview
rustc --version # rustup auto-installs the pinned stable toolchain
cargo build # default features: builds the pure-Rust trait/type layer
```

You will also want `cargo-deny` for the license/advisory gate (see
[Licensing & DCO](#licensing--dco)):

```bash
cargo install --locked cargo-deny
```

### Optional: FFmpeg (libav) — the `ffmpeg` feature

Media crates that demux/decode/encode link **libav\*** behind the `ffmpeg` feature
(`multiview-ffmpeg` and the crates that use it). For any hardware path or real ingest/output
you need an FFmpeg with the right capabilities.

- The default build expects an **LGPL-clean FFmpeg** (no `--enable-gpl`,
 no `--enable-nonfree`). NVENC/NVDEC come via `nv-codec-headers` (MIT) — they need
 neither flag. We do **not** use libnpp / x264 / x265 in the default build (scaling and
 compositing are done in-house).
- Verify your FFmpeg with `ffmpeg -buildconf` (must **not** show
 `--enable-gpl`/`--enable-nonfree`/`--enable-libnpp`) and `ffmpeg -protocols`
 (should list `srt` if you test SRT — ensure `srt.pc` is on `PKG_CONFIG_PATH`).
- Prefer **dynamic linking** of FFmpeg so the LGPL relink right is preserved.

> `xtask` can fetch/compile an LGPL FFmpeg with the correct hardware flags for you — see
> [conventions §17](docs/architecture/conventions.md) and run `cargo xtask --help`.

### Optional: GPU backends

These are **off by default**; enable only what your hardware supports (they are
runtime-probed and fall back to software):

| Backend | Feature | Platform | Notes |
|---|---|---|---|
| NVIDIA NVDEC/NVENC + CUDA | `cuda` | Linux (NVIDIA) | Needs CUDA toolkit to build; driver injects runtime libs. Container runs require `NVIDIA_DRIVER_CAPABILITIES=compute,utility,video` — **`video` is mandatory** or HW silently disappears. |
| Intel/AMD VAAPI | `vaapi` | Linux | Pass `/dev/dri`; add host `render`/`video` GIDs. |
| Intel QuickSync (oneVPL) | `qsv` | Linux (Intel) | Derived from VAAPI. |
| Apple VideoToolbox / Metal | `videotoolbox`, `metal` | macOS | The only HW path on macOS (no CUDA/VAAPI). |
| Portable GPU | `wgpu` | all | Default compositor backend; cross-platform. |

See the per-platform backend matrix and zero-copy rules in the
[core-engine brief](docs/research/core-engine.md) and
[ADR-0003](docs/decisions/ADR-0003.md) / [ADR-0004](docs/decisions/ADR-0004.md).

### Optional: Node (for the web SPA)

The management UI lives in [`web/`](web/) (React 19 + TypeScript + Vite). You only need
Node if you are working on the frontend; backend-only contributors can skip it. Use a
current LTS Node and the lockfile in `web/`:

```bash
cd web
npm ci # install exactly per lockfile
npm run dev # Vite dev server (proxies to the running multiview API)
npm run build # production build (embedded into the binary via the embed-web feature)
```

The API client is generated from the OpenAPI spec; see
[conventions §8](docs/architecture/conventions.md).

### Optional: NDI

The NDI SDK is **proprietary** and is **never vendored**. The `ndi` feature uses a
runtime dynamic-load path; you must supply the SDK/runtime yourself and honour its EULA
and attribution requirements. See [conventions §7](docs/architecture/conventions.md) and
[ADR-0008](docs/decisions/ADR-0008.md).

---

## Workspace layout

Cargo workspace (`resolver = "2"`). All Rust crates are prefixed `multiview-` and live under
`crates/`. The full canonical map is in
[conventions §2–§3](docs/architecture/conventions.md); the headline structure:

```
multiview/
├── Cargo.toml # workspace
├── rust-toolchain.toml rustfmt.toml .editorconfig deny.toml clippy.toml
├── LICENSE-MIT LICENSE-APACHE README.md CONTRIBUTING.md SECURITY.md
├── crates/ # all multiview-* crates
├── web/ # management SPA (React + TS + Vite)
├── docs/ # architecture, decisions (ADRs), research briefs, ops
├── examples/ # example multiview configs + layout templates
├── deploy/ # Dockerfile, compose, container assets
├── xtask/ # dev automation (cargo xtask ...)
└── .github/workflows/ # CI
```

Key crates you'll touch most often:

| Crate | What it owns |
|---|---|
| `multiview-core` | Shared types & stage traits (`Frame`, `PixelFormat`, `Source`, `Sink`, `Compositor`, …). **No FFI.** |
| `multiview-engine` | The protected output core: output clock, compositor drive, supervisor, hot-reconfig. |
| `multiview-compositor` | The custom GPU compositor (wgpu baseline + vendor fast paths). |
| `multiview-ffmpeg` | Safe RAII wrappers over libav\*. |
| `multiview-control` | axum REST + WebSocket + SSE API, OpenAPI, SQLite. |
| `multiview-cli` | The `multiview` binary; aggregates feature flags. |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint). |

**Dependency direction:** `core` ← everything; `engine` depends on the media crates;
`control`/`preview` depend on `engine` + `events`; `cli` depends on all. **No cycles.**

---

## Build, test, lint

The default (no-feature) build is the **GPU-free, native-dep-free** path that CI runs on
free runners — keep it green.

```bash
# Type-check the pure-Rust layer (default features only)
cargo check

# Build / test the whole workspace, default features
cargo build --workspace
cargo test --workspace

# Format (config in rustfmt.toml) — CI fails on diffs
cargo fmt --all
cargo fmt --all -- --check

# Lint — CI runs clippy with -D warnings
cargo clippy --workspace --all-targets -- -D warnings

# License / advisory gate (must pass before merge)
cargo deny check
```

Building with hardware/native features (do this locally only when you have the toolchain
and hardware):

```bash
# A single feature
cargo build -p multiview-cli --features ffmpeg

# An umbrella preset from multiview-cli (see feature taxonomy below)
cargo build -p multiview-cli --features nvidia # cuda + ffmpeg + wgpu
cargo build -p multiview-cli --features apple # videotoolbox + metal + ffmpeg
cargo build -p multiview-cli --features linux-vaapi # vaapi + qsv + ffmpeg + wgpu
```

Dev automation lives in `xtask` — prefer it over ad-hoc scripts:

```bash
cargo xtask --help # build web, regenerate OpenAPI/AsyncAPI, lint, etc.
```

### Testing expectations

- New behaviour needs tests. The **software/CPU backend is the test enabler** — it runs in
 GPU-free CI with golden-frame (`framemd5`) checks for the deterministic compositor.
- GPU encode output is **not** bit-exact across drivers/versions → use SSIM/PSNR
 thresholds, not exact hashes, for HW paths.
- Add a **"stayed-on-GPU"** assertion (count host↔device copies) where relevant so silent
 CPU fallbacks fail the test. See the testing strategy in the
 [core-engine brief §19](docs/research/core-engine.md) and
 [ADR-R009](docs/decisions/ADR-R009.md).
- The engine **isolation** invariant is enforced by a CI **chaos gate** — changes that let
 the control/preview/realtime layers back-pressure the engine will fail.

---

## Feature-flag conventions

Feature flags are how Multiview stays LGPL-clean and GPU-free by default while still shipping
full hardware acceleration. The taxonomy is canonical in
[conventions §4](docs/architecture/conventions.md). Rules:

- **Default features build a pure-Rust, LGPL-clean, no-native-deps check.** CI is green
 without GPUs. Never make a hardware/FFI dependency a default feature.
- **Hardware/FFI/GPU code is always behind an off-by-default feature.** `software` is
 always on and is the universal fallback tier.
- **Features are additive and must never change the public API** — enabling `cuda` must not
 alter type signatures, only swap in a backend impl behind a trait.
- **Feature names are `kebab-case`** (e.g. `gpl-codecs`, `embed-web`).
- **License-escalating features are opt-in only:**
 - `gpl-codecs` (x264/x265) makes the resulting build **GPL** — off by default.
 - `ndi` pulls the proprietary, runtime-loaded NDI path — off by default.
- **Umbrella presets live in `multiview-cli`** (`nvidia`, `apple`, `linux-vaapi`, `full` =
 everything non-GPL). Don't scatter platform presets across leaf crates.

Quick reference:

| Category | Features |
|---|---|
| Codec backends | `cuda`, `videotoolbox`, `vaapi`, `qsv`, `software` (always on) |
| Compositor backends | `wgpu` (default), `metal`, `cuda` |
| Media engine | `ffmpeg` (links libav) |
| Codec licensing | `gpl-codecs` (→ GPL build) |
| NDI | `ndi` (proprietary, runtime-loaded) |
| Subtitles | `libass` |
| Web/API | `openapi` (default), `embed-web`, `webrtc` |

---

## Coding conventions

From [conventions §9](docs/architecture/conventions.md):

- **Crates:** `multiview-<area>` (kebab); the library target is `multiview_<area>` (snake).
- **Types:** `UpperCamel`; **functions/fields:** `snake_case`; **features:** `kebab-case`.
- **Errors:** a per-crate `Error` enum via `thiserror`; app boundaries (e.g. `multiview-cli`)
 may use `anyhow`.
- **Async** runtime is `tokio`; **logging/tracing** is `tracing`; **serialization** is
 `serde`.
- **Docs:** every public item is documented; library crates carry
 `#![warn(missing_docs)]`.
- **Formatting:** `rustfmt` (see [`rustfmt.toml`](rustfmt.toml) — edition 2021, max width
 100). Must be clippy-clean (`-D warnings` in CI).
- **Two-plane threading:** the data plane runs on dedicated OS threads (codec/CUDA/Metal
 calls must **never** run on Tokio workers); the control/IO plane is Tokio. Channels carry
 ref-counted frame **handles**, never pixels. See
 [ADR-0009](docs/decisions/ADR-0009.md).

Indentation and EOL are enforced by [`.editorconfig`](.editorconfig) (4-space Rust,
2-space for web/JSON/TOML/YAML, LF line endings).

---

## Commit & PR conventions

### Branches

- Branch off `main`. Never push directly to `main`.
- Use descriptive branch names, e.g. `feat/cuda-compositor-fit-modes`,
 `fix/hls-pts-rebase`.

### Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <short summary>

<body — what & why, not how>

<footer — DCO sign-off, issue refs>
```

- **type:** `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`.
- **scope:** the crate or area, e.g. `compositor`, `engine`, `control`, `ffmpeg`, `web`.
- Reference issues (`Closes #123`) in the footer.
- Every commit must be **signed off** (DCO — see below).

### Pull requests

A PR is ready to review when:

- [ ] `cargo fmt --all -- --check` is clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes (default features at minimum).
- [ ] `cargo deny check` passes.
- [ ] New public items are documented; new behaviour has tests.
- [ ] The default (GPU-free) build still compiles and runs the software pipeline.
- [ ] Relevant invariants ([conventions §5](docs/architecture/conventions.md)) are
 respected, or you've opened an ADR to change one.
- [ ] All commits are DCO-signed.

Keep PRs scoped and reviewable. If a change alters a load-bearing decision, link (or add)
the relevant ADR.

---

## Proposing an architecture decision (ADR)

Load-bearing decisions live in [`docs/decisions/`](docs/decisions/) as numbered ADRs (see
the index in [`docs/decisions/README.md`](docs/decisions/README.md)). Open an ADR when you
want to **change an invariant**, **add/replace a major dependency or backend**, or **set a
cross-crate policy**.

### ADR format

Every ADR follows the same shape (see [ADR-0001](docs/decisions/ADR-0001.md) as a
template):

```markdown
# ADR-XXXX: <short imperative title>

- **Status:** Proposed # → Accepted / Rejected / Superseded
- **Area:** <Core Engine | Resilience & A/V | Efficiency | Color | ... >
- **Date:** YYYY-MM-DD
- **Source brief:** [<name>.md](../research/<name>.md)

## Decision
<the decision, stated plainly>

## Rationale
<why — the load-bearing reasons>

## Alternatives considered
<what was rejected and why>

## Consequences
<costs, follow-on work, what this constrains>
```

### Process

1. **Pick the next id.** ADRs are grouped by area prefix (e.g. `ADR-00NN` core engine,
 `ADR-RT0NN` realtime API, `ADR-W0NN` web/API). Use the next free number in the matching
 series; the plain `ADR-00NN` series covers core-engine decisions.
2. **Write the ADR** under `docs/decisions/` using the template above, with `Status: Proposed`.
3. **Cite the brief.** Link the relevant deep brief in `docs/research/` — base the decision
 on it; don't invent facts that contradict
 [conventions.md](docs/architecture/conventions.md).
4. **Add it to the index** in [`docs/decisions/README.md`](docs/decisions/README.md) under
 the right area heading.
5. **Open a PR** for discussion. On approval, flip `Status` to `Accepted`. A decision that
 replaces an earlier one marks the old ADR `Superseded` and links forward.

> Conventions and code remain the source of truth: an `Accepted` ADR that changes an
> invariant must also update [`docs/architecture/conventions.md`](docs/architecture/conventions.md).

---

## Licensing & DCO

### Project license

Multiview is **source-available, not open source**: it is published under the
**Multiview Source-Available Non-Commercial License** (see [`LICENSE`](LICENSE)), with a
paid path for commercial use (see [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md)). The
Licensor and copyright holder is **Aperim Pty Ltd** (ABN 46 150 699 737).

### Contributor License Agreement (CLA) — required

Because Multiview is dual-licensed (a free source-available licence **plus** a paid
commercial/proprietary licence), every contribution must come with a **Contributor License
Agreement (CLA)** before it can be merged. You keep the copyright in your contribution; the
CLA grants Aperim Pty Ltd a broad, perpetual, irrevocable licence to use your contribution
**and to relicense it under any terms — including commercial and proprietary terms**. This
"inbound = outbound, plus the right to license under other terms" arrangement is what makes
the dual-license model work: without it, your contribution could not be offered to
commercial licensees.

Read and agree to [`CONTRIBUTOR-LICENSE-AGREEMENT.md`](CONTRIBUTOR-LICENSE-AGREEMENT.md). The
DCO sign-off below is still required on every commit, but it is **not** a substitute for the
CLA — both are needed.

### Build profiles & redistributability

The **default build is LGPL-clean and redistributable**. Some features change the effective
license of the resulting artifact — keep this in mind for any dependency you add:

| Profile | Effective status |
|---|---|
| **default** | Permissive / LGPL-clean, redistributable |
| **+`gpl-codecs`** | Whole build becomes **GPL-2.0-or-later** |
| **+`ndi`** | Permissive code + NDI EULA + mandatory attribution |
| **nonfree** (libnpp/FDK-AAC/OpenSSL) | **NOT redistributable** — internal use only |

Full matrix and rules: [conventions §7](docs/architecture/conventions.md) and
[ADR-0012](docs/decisions/ADR-0012.md).

### cargo-deny gate

CI runs `cargo-deny` against [`deny.toml`](deny.toml) to gate **licenses, advisories,
bans, and sources**. Any new dependency must pass. If you add a dependency that needs a
license exception or escalates the build profile, call it out explicitly in your PR and
update `deny.toml` (with justification) as part of the change.

### Developer Certificate of Origin (DCO)

We require a **DCO sign-off** on every commit. By signing off you certify the
[Developer Certificate of Origin](https://developercertificate.org/) — i.e. that you wrote
the code or have the right to submit it under the project license. Sign off by adding a
trailer to each commit:

```
Signed-off-by: Your Name <you@example.com>
```

The easy way is `-s`:

```bash
git commit -s -m "feat(compositor): add cover/crop fit modes"
```

Unsigned commits will be flagged in CI and must be amended (`git commit --amend -s`) or
rebased with sign-offs before merge.

---

Questions that aren't answered here usually are in
[`docs/architecture/conventions.md`](docs/architecture/conventions.md) (the source of
truth), the briefs in [`docs/research/`](docs/research/), or the decisions in
[`docs/decisions/`](docs/decisions/). Thanks for contributing to Multiview!
