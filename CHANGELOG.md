# Changelog

All notable changes to **Conspect** — Rust software for live-media production, routing and
monitoring — are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Status:** pre-release scaffold. Until the first tagged `0.1.0`, everything lives under
> [Unreleased] and the public API is considered unstable.

---

## [Unreleased]

The repository has been bootstrapped: the canonical conventions, the full design-research
corpus, the Architecture Decision Records, and the workspace skeleton are in place. No runtime
functionality ships yet — this is the documentation-and-scaffolding foundation that the
implementation will build against.

### Changed

- **Conspect Source-Available Non-Commercial License, Version 2.0** now governs
  copies supplied under those terms. Free use is personal, home, hobby and learning
  use plus the named non-monetized exceptions; creators must have fewer than 55,000
  aggregate audience members. Religious use and monetized Conspect content require
  paid licensing, including sponsorships and promotional gifts. Free use excludes
  paid/metered features and has no warranty, subject to statutory rights. Source
  study and authorized-use copies do not grant general modification, reproduction
  or redistribution rights. See [`LICENSE`](LICENSE).
- **Historical Version 1.0 relicensing event:** the project, then named Multiview,
  moved from `MIT OR Apache-2.0` to the Multiview Source-Available Non-Commercial
  License (© Aperim Pty Ltd). That event's broader grants are not the current policy
  for Version 2.0 copies. Earlier valid grants remain subject to their original terms.
  Third-party rights remain intact; enabling GPL codecs does not relicense Conspect.

### Added

- **Front-door certificates: setup, boot wiring and `multiview tls` (ACME, slice 5)**:
  `network.public_host` + `[network.tls]` now start the ACME service at boot. While it waits for a
  certificate, the front door serves a self-generated bootstrap certificate and stays in setup
  (fingerprint and a one-time setup code in the log). In setup it serves only the setup page and
  `/api/v1/tls/*`; everything else is a `503 /problems/tls-setup-required`. QUIC/MoQ on the
  front door starts once the certificate is installed, and renewals apply without a restart.
  Failing renewals raise the `tls-certificate` health warning. On the host, `multiview tls status`,
  `setup-code`, `set-credential` (value from stdin only, stored 0600) and `issue` give full
  recovery without a browser. The `acme` feature is now in the shipped presets; a `[network.tls]`
  config in a build without it refuses to start. Runbook: `docs/runbooks/acme-certificates.md` (#291).
- **The TCP 443 door serves the ACME certificate (slices 4 + 5)**: with `network.mode` and
  `[network.tls]`, the engine's TCP door serves the bootstrap, then the issued certificate (renewals
  on the next handshake), answers TLS-ALPN-01 (ALPN `acme-tls/1`) and gates every route behind the
  setup state; `[network.tls]` wins over `[control.tls]` for the door. Behind a proxy, setup-code
  limits key on the PROXY v2 client address and `multiview tls status` shows the TLS-ALPN-01 hint
  (#291).
- **ACME certificate engine for the network front door**: the
  `acme` feature of `multiview-control` issues and renews the front-door certificate over
  DNS-01 (Cloudflare token or global key) or TLS-ALPN-01 from Let's Encrypt, Google or a custom
  directory, with SSRF-screened egress, a private 0700 state directory, crash-safe TXT cleanup,
  ARI renewal and expiry alarms, on its own thread and runtime away from the output clock. New
  `[network.tls] allow_private_directory` gates a private custom directory. Tested against Pebble
  in the new `acme-pebble` CI job (#291).

- **Event journal.** Every raw SCTE-35, timed ID3 and HLS cue observation is recorded off-clock
  with its full decode, predicted wall-clock and on-air times, per source and day, with bounded
  retention (`[events]`). Read it in the Events dialog's Journal tab, or through
  `GET /api/v1/inputs/{id}/events/journal` and `.../journal.jsonl`. The Events panel now shows
  decoded markers with times, and the source's embedded clock instead of ID3 timestamp rows.

#### Repository scaffold
- Initial repository structure following the canonical [repository layout][layout]:
  `crates/`, `web/`, `docs/`, `examples/`, `deploy/`, `xtask/`, and `.github/workflows/`.
- Dual licensing under **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`).
- Toolchain pin via `rust-toolchain.toml` (Rust **edition 2021**, stable channel).
- Baseline tooling configs: `rustfmt.toml`, `.editorconfig` (with `clippy.toml` and
  `deny.toml` to follow as the workspace is wired up).

#### Architecture & conventions
- **[Canonical conventions][conventions]** — the single source of truth pinning project
  identity, repository layout, the crate map, the feature-flag taxonomy, the load-bearing
  technical invariants, the API/realtime conventions, the licensing model, and frontend +
  naming/style rules.

#### Research / design briefs
The verification-hardened deep briefs that back the implementation
(see the [research index][research]):

| Area | Brief |
|------|-------|
| Core Engine | [core-engine.md][b-core] |
| Resilience & A/V | [resilience-and-av.md][b-resilience] |
| Efficiency | [efficiency.md][b-efficiency] |
| Color | [color-management.md][b-color] |
| Streaming / Timing | [streaming-gotchas.md][b-streaming] |
| Preview | [preview-subsystem.md][b-preview] |
| Realtime API | [realtime-api.md][b-realtime] |
| Management | [management-capability-matrix.md][b-mgmt] |
| Web / API Stack | [web-api-stack.md][b-web] |

- Supporting reference material: a [bibliography][bib] and an
  [example-streams catalogue][streams].

#### Architecture Decision Records
- **89 ADRs** (status: *Proposed*) derived from the design briefs — see the
  [ADR index][adr]. Grouped by area:
  - Core Engine — `ADR-0001`…`ADR-0014`
  - Resilience & A/V — `ADR-R001`…`ADR-R009`
  - Efficiency — `ADR-E001`…`ADR-E009`
  - Color — `ADR-C001`…`ADR-C006`
  - Streaming / Timing — `ADR-T001`…`ADR-T008`
  - Preview — `ADR-P001`…`ADR-P005`
  - Realtime API — `ADR-RT001`…`ADR-RT006`
  - Management — `ADR-M001`…`ADR-M007`
  - Web / API Stack — `ADR-W001`…`ADR-W008`

  Load-bearing examples: the continuous-output guarantee ([ADR-R001][adr-r001]), the
  single internal timeline + fixed-cadence output clock ([ADR-T001][adr-t001]), the
  LGPL-clean default build ([ADR-0012][adr-0012]), and the axum web framework choice
  ([ADR-W001][adr-w001]).

#### Crate workspace skeleton
- Cargo workspace skeleton enumerating the canonical [crate map][cratemap]. All crates are
  prefixed `multiview-` and live under `crates/`; hardware/FFI/GPU code sits behind
  **off-by-default** Cargo features so the default `cargo check` builds the pure-Rust,
  LGPL-clean trait/type layer:

  | Crate | Responsibility |
  |-------|----------------|
  | `multiview-core` | Shared types & traits (`Frame`, `PixelFormat`, `ColorInfo`, `MediaTime`, stage traits). No FFI. |
  | `multiview-hal` | Hardware capability detection, backend registry, negotiation + cost model/planner. |
  | `multiview-ffmpeg` | Safe RAII wrappers over libav* (demux/decode/encode, hwframe lifecycle). |
  | `multiview-compositor` | Custom GPU compositor (scale + place + color convert + linear-light blend + overlay). |
  | `multiview-framestore` | Per-tile last-good-frame stores + tile state machine. |
  | `multiview-audio` | Per-input audio decode/resample/mix/route + EBU R128 metering. |
  | `multiview-overlay` | Overlay layers, text rendering, subtitle ingest/render. |
  | `multiview-input` | Ingest sources, input pacer, jitter buffers, timestamp normalization, reconnect. |
  | `multiview-output` | Output sinks/servers; encode-once-mux-many fan-out. |
  | `multiview-engine` | Protected output core: output clock, compositor drive, supervisor, hot-reconfig. |
  | `multiview-config` | Config & template schema, validation, config-as-code import/export. |
  | `multiview-events` | Shared realtime event types + versioned envelope. |
  | `multiview-control` | axum REST + WebSocket + SSE API, OpenAPI, auth, SQLite, embedded SPA. |
  | `multiview-preview` | Preview taps, preview encoder pool, WHEP/MJPEG/snapshot endpoints. |
  | `multiview-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). |
  | `multiview-cli` | Binary **`multiview`**: wires engine + control plane; run/validate subcommands. |
  | `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint). |

  **Dependency direction:** `core` ← everything; `engine` depends on the media crates;
  `control`/`preview` depend on `engine` + `events`; `cli` depends on all. No cycles.

### Notes

- **Platforms:** Linux (x86_64 + aarch64) and macOS (Apple Silicon + Intel). **No Windows.**
- **Default dependencies are LGPL-clean; this is not a Conspect redistribution grant.**
  The opt-in `gpl-codecs` feature adds GPL dependencies and combined-work obligations,
  not permission to relicense or distribute Conspect. The proprietary `ndi` feature
  is off by default and runtime-loaded (never vendored). See the [licensing model][conventions].

---

## Versioning policy

Conspect follows **[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)**
(`MAJOR.MINOR.PATCH`):

- **MAJOR** — incompatible API changes (Rust public API across crates, and the
  REST/WebSocket surface under `/api/v1`).
- **MINOR** — backwards-compatible functionality.
- **PATCH** — backwards-compatible bug fixes.

Additional rules for this project:

- **Pre-1.0 (`0.y.z`):** the API is unstable; **minor** bumps may carry breaking changes,
  per the SemVer pre-release allowance. Pin exact versions if you depend on the crates.
- **Cargo feature flags are part of the contract.** Removing or repurposing a feature
  (e.g. `cuda`, `vaapi`, `ndi`, `gpl-codecs`, `webrtc`) is a breaking change.
- **REST/WebSocket API** versioning is independent of the crate versions and is carried in
  the `/api/v1` path and the realtime envelope version; bumping it is a breaking change.
- ADR status transitions (Proposed → Accepted/Superseded) are recorded in the ADRs
  themselves, not as version bumps.

---

[Unreleased]: https://github.com/conspect-studio/conspect/compare/HEAD

<!-- Architecture & docs -->
[conventions]: docs/architecture/conventions.md
[layout]: docs/architecture/conventions.md#2-repository-layout
[cratemap]: docs/architecture/conventions.md#3-canonical-crate-map
[research]: docs/research/README.md
[adr]: docs/decisions/README.md
[bib]: docs/reference/bibliography.md
[streams]: docs/reference/example-streams.md

<!-- Research briefs -->
[b-core]: docs/research/core-engine.md
[b-resilience]: docs/research/resilience-and-av.md
[b-efficiency]: docs/research/efficiency.md
[b-color]: docs/research/color-management.md
[b-streaming]: docs/research/streaming-gotchas.md
[b-preview]: docs/research/preview-subsystem.md
[b-realtime]: docs/research/realtime-api.md
[b-mgmt]: docs/research/management-capability-matrix.md
[b-web]: docs/research/web-api-stack.md

<!-- Selected ADRs -->
[adr-r001]: docs/decisions/ADR-R001.md
[adr-t001]: docs/decisions/ADR-T001.md
[adr-0012]: docs/decisions/ADR-0012.md
[adr-w001]: docs/decisions/ADR-W001.md
