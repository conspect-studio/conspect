# Multiview — Canonical Conventions (Source of Truth)

This document pins the **canonical** naming, structure, and invariants for the Multiview project.
The deep design briefs in [`../research`](../research/) may use slightly varying names (they were
written in parallel); **where they differ, THIS document wins**, and the Rust code is the ultimate
source of truth. All other docs, the agent-instruction files, and the workspace must conform to this.

---

## 1. Project identity

- **Name:** Multiview — an efficient, hardware-accelerated, Rust live video multiview generator.
- **Binary / daemon:** `multiview`
- **Tagline:** Ingest many live sources → composite a templated multiview on the GPU → serve robustly. Built to run great on **commodity hardware**, with **bulletproof continuous output**.
- **Edition / toolchain:** Rust edition **2021**, pinned via `rust-toolchain.toml` (stable). MSRV documented in the README.
- **License:** Multiview's own code is **source-available, non-commercial** — the **Multiview Source-Available Non-Commercial License, Version 1.0** (© Aperim Pty Ltd). Free for genuine personal/home/non-commercial use plus three free exceptions; all other (commercial) use needs a paid Commercial License. **Not open source / not free software.** See §7 for the FFmpeg/NDI/codec dependency-linking model.
- **Platforms:** Linux (x86_64 + aarch64; NVIDIA via Container Toolkit, Intel/AMD via VAAPI) and macOS (Apple Silicon + Intel, native). **No Windows.**

---

## 2. Repository layout

```
multiview/
├── Cargo.toml                # workspace
├── rust-toolchain.toml
├── rustfmt.toml  .editorconfig  deny.toml  clippy.toml
├── LICENSE  LICENSE-COMMERCIAL.md  README.md  CLAUDE.md  AGENTS.md  CONTRIBUTING.md  SECURITY.md
├── crates/                   # all Rust crates (see §3)
│   └── multiview-*/ ...
├── web/                      # the management SPA (React + TS + Vite)
├── docs/                     # architecture, decisions (ADRs), research briefs, reference, ops
├── examples/                 # example multiview configs + layout templates
├── deploy/                   # Dockerfile, compose, container assets
├── xtask/                    # dev automation (cargo xtask ...)
└── .github/workflows/        # CI
```

> The build also uses a transient `.multiview-build/` working dir (git-ignored) — not part of the product.

---

## 3. Canonical crate map

All crates are prefixed `multiview-` and live under `crates/`. Hardware/FFI/GPU code is **behind
off-by-default Cargo features** so the default `cargo check` builds the pure-Rust trait/type layer.

| Crate | Responsibility | Notable optional features |
|-------|----------------|---------------------------|
| `multiview-core` | Shared types & traits: `Frame`, `PixelFormat` (NV12 canonical), `ColorInfo` (the 4 axes), clock/`MediaTime`, layout/template model, error taxonomy, the stage traits (`Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`, `Backend`). No FFI. | — |
| `multiview-hal` | Hardware capability detection, backend registry, per-stage **negotiation** + **cost model/planner** (admission + degradation inputs). | `cuda`, `vaapi`, `qsv`, `videotoolbox` (probing) |
| `multiview-ffmpeg` | Safe RAII wrappers over libav* (demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map). | `ffmpeg` (links libav), `gpl-codecs` |
| `multiview-compositor` | The **custom GPU compositor**: scale + place + per-tile color convert (range/matrix/linearize) + linear-light blend + overlay compositing. wgpu baseline; vendor fast paths. | `wgpu` (default backend), `cuda`, `metal`, `vaapi` |
| `multiview-framestore` | Per-tile **last-good-frame** stores (lock-free triple-buffer) + the tile **state machine** (LIVE/STALE/RECONNECTING/NO_SIGNAL). | — |
| `multiview-audio` | Per-input audio decode/resample/mix/route (program bus + discrete tracks) + **EBU R128** metering. | `ffmpeg` |
| `multiview-overlay` | Overlay layers + text rendering + **subtitle** ingest/render (libass) and passthrough. | `libass` |
| `multiview-input` | Ingest sources (rtsp/hls/ts/srt/rtmp/ndi/file/test), the **input pacer**, jitter buffers, **timestamp normalization**, supervised reconnect. | `ffmpeg`, `ndi` |
| `multiview-output` | Output sinks/servers: RTSP server, HLS/LL-HLS packager, NDI out, RTMP/SRT push; **encode-once-mux-many** fan-out. | `ffmpeg`, `ndi` |
| `multiview-engine` | The **protected output core**: the fixed-cadence output clock, compositor drive, supervisor/actors, **hot-reconfiguration**, admission/degradation control loop. | — |
| `multiview-config` | Config & template schema (serde), validation, **config-as-code** import/export. | — |
| `multiview-events` | Shared realtime **event types + versioned envelope** (used by engine, control, clients). | — |
| `multiview-control` | The **axum** REST + WebSocket + SSE API: OpenAPI (utoipa+Scalar), auth, SQLite (sqlx), the **command-bus shell**, embedded SPA serving. | `openapi` (default), `embed-web` |
| `multiview-preview` | Preview **taps** (input/program/output), the preview **encoder pool**, WHEP/MJPEG/snapshot endpoints. Strictly isolated from the program path. | `webrtc` |
| `multiview-webrtc` | The shared **native WebRTC transport endpoint** (str0m): one dual-stack `[::]` UDP socket muxing **all** sessions (WHIP ingest, WHEP preview + output viewers, WHIP push), full-ICE with IPv6-first candidates, per-run DTLS cert, one bounded driver task + session GC. Default build is a pure shell; the native transport relocated here from `multiview-preview` (ADR-0048). | `native` |
| `multiview-telemetry` | `tracing` + Prometheus metrics + health (`/livez`,`/readyz`). | — |
| `multiview-cli` | Binary **`multiview`**: wires the engine + control plane; config load; run/validate subcommands. | aggregates feature flags |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint, etc.). | — |

**Dependency direction:** `core` ← everything; leaf crates depend on `core` (+ `hal`, `ffmpeg`,
`events` as needed); `engine` depends on the media crates; `control`/`preview` depend on `engine` +
`events`; `webrtc` depends on `preview` + `input` (+ `core`); `cli` depends on all. No cycles.
Documented exception: `config` depends on `overlay` for the shared IANA-timezone model
(`clock::{parse_tz, resolve_offset, WallTime}`) that both config validation (clock/timer sources,
ADR-0047) and render-time consume — one source of truth for DST math; acyclic. If a third consumer
appears, the tz module's long-term home is `core`.

---

## 4. Feature-flag taxonomy (canonical)

Default features build a **pure-Rust, LGPL-clean, no-native-deps** check (CI green without GPUs).

- **Codec backends (per stage, auto-negotiated at runtime):** `cuda` (NVDEC/NVENC), `videotoolbox`, `vaapi`, `qsv`, `software` (always on).
- **Compositor backends:** `wgpu` (default, cross-platform), `metal`, `cuda`.
- **Media engine:** `ffmpeg` (links libav for demux/decode/encode).
- **Codecs licensing:** `gpl-codecs` (x264/x265 → makes the build GPL; **off by default**).
- **NDI:** `ndi` (proprietary SDK; **off by default**, runtime-loaded; see §7).
- **Subtitles:** `libass`.
- **Web/API:** `openapi` (default), `embed-web` (embed the SPA), `webrtc` (the pure WHEP/WHIP seams; native-free).
- **WebRTC native transport:** `native` on `multiview-webrtc` (str0m ICE/DTLS/SRTP; off by default); in `multiview-cli`: `webrtc` (pure seams) and `webrtc-native` (= `webrtc` + `multiview-webrtc/native` + `ffmpeg`). `multiview-preview`'s former `webrtc-native` flag is **removed** — the native transport relocated to `multiview-webrtc` (ADR-0048).
- **Umbrella presets (in `multiview-cli`):** `nvidia` = cuda+ffmpeg+wgpu+webrtc-native; `apple` = videotoolbox+metal+ffmpeg+webrtc-native; `linux-vaapi` = vaapi+qsv+ffmpeg+wgpu+webrtc-native; `full` = everything non-GPL (incl. webrtc-native).

---

## 5. Canonical technical invariants

These are load-bearing; every doc and implementation must respect them (see the briefs for depth).

1. **Output-clock invariant (bulletproof output):** at every tick of a single fixed-cadence internal monotonic clock, the output stage emits exactly one valid, correctly-timestamped frame (+ matching audio), **forever**, independent of any input. Inputs are *sampled*, never *pacing*. Output PTS = `f(tick)`.
2. **Per-tile last-good-frame + state machine:** inputs write into lock-free single-slot stores; the compositor always reads the latest (or a placeholder card) and never blocks. Tiles ride LIVE→STALE→RECONNECTING→NO_SIGNAL.
3. **Unified timing model:** per-input PTS is normalized (unwrap 33-bit wrap, genpts fallback, monotonic guard) and rebased onto one internal ns timeline; the output re-stamps all PTS/DTS from the tick counter. NTSC `1001` rates carried as exact rationals/ns — never float fps.
4. **HLS ingest pacing:** live/VOD-as-live inputs are paced to wall-clock by PTS (a custom input pacer); `-re` is for files, not live ingest.
5. **NV12-throughout:** frames stay NV12 (1.5 B/px); never materialize RGBA per tile. YUV→RGB happens in-shader at tile size.
6. **Decode-at-display-resolution:** decode each source near its displayed size where the backend supports it (NVDEC `-resize` fused; VideoToolbox/VAAPI/QSV per the capability matrix); prefer a smaller source rendition/substream. Budget decode in megapixels/sec.
7. **Encode-once-mux-many:** composite once, encode the canvas once per rendition, fan the *same* packets to all transports; separate encode only when codec/res/bitrate differ.
8. **Color pipeline order (never reorder):** detect 4 axes (with untagged-default policy matching players, not swscale) → range-expand → YUV→RGB matrix (gamma-encoded) → linearize (EOTF) → primaries convert in linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV + range compress → **tag the output** (primaries/TRC/matrix/range + HDR) → verify with ffprobe.
9. **Resource-adaptive degradation:** a closed control loop (sense→estimate→plan→apply, with hysteresis) sheds load tile-by-tile in a defined cheapest-impact-first order **before** the program output is ever touched. Bounded queues drop, never grow.
10. **Isolation:** the control plane, preview, and realtime layers are best-effort and **physically incapable of back-pressuring the engine** (watch/broadcast channels; bounded drop-oldest queues; the engine never awaits a client). A CI chaos gate enforces this.
11. **Live-apply classification:** every management change is classified Class-1 (hot/seamless at a frame boundary) vs Class-2 (controlled reset via make-before-break parallel-output migration); the API surfaces which before applying.

---

## 6. API & realtime conventions

- **REST base path:** `/api/v1`. Commands/CRUD only. Long-running ops return `202 Accepted` + an operation id; the result arrives on the realtime stream.
- **Error model:** RFC 9457 `application/problem+json`.
- **Concurrency:** per-resource version → `ETag`/`If-Match`, `412` on mismatch.
- **Idempotency:** `Idempotency-Key` on start/stop/swap.
- **OpenAPI:** 3.1 via **utoipa**; interactive docs (**Scalar**) at `/docs`; spec at `/api/v1/openapi.json`.
- **Realtime:** **WebSocket** primary at `/api/v1/ws` (versioned envelope, snapshot+delta, subscriptions, resume via `seq`); **SSE** fallback at `/api/v1/events`; documented with **AsyncAPI** at `/docs/events`. High-rate audio meters are sampled/conflated (~10–30 Hz).
- **Auth:** UI = `tower-sessions` cookie + CSRF; machine = hashed API keys (Bearer). RBAC admin/operator/viewer via `axum-login`. Per-object authorization on every resource id (BOLA is the #1 risk).
- **Preview:** WHEP (sub-second focus) + MJPEG/JPEG (cheap grid); preview access gated by short-lived signed tokens; auto-stop with no subscribers.

---

## 7. Licensing model (build profiles)

- **Project code:** the **Multiview Source-Available Non-Commercial License, Version 1.0** (© Aperim Pty Ltd; see `LICENSE`) — source-available, **not** open source / free software. Free for genuine personal/home/non-commercial use plus three free exceptions (First Nations owned broadcasters; community broadcasters under USD $1M sponsorship/yr; content creators under 55,000 subscribers summed across all channels); all other (commercial) use — business, education, government, productization/appliance, streamers/creators — requires a paid Commercial License (`licensing@aperim.com`, see `LICENSE-COMMERCIAL.md`). This governs **Multiview's own code only**; the FFmpeg/NDI/codec build-profile obligations below are unchanged.
- **Default build = LGPL-clean & redistributable:** FFmpeg linked LGPL; NVENC/NVDEC via `nv-codec-headers` (MIT) need neither `--enable-gpl` nor `--enable-nonfree`; **no** libnpp/x264/x265 in the default build (compositing/scaling done in-house).
- **`gpl-codecs` feature:** pulls x264/x265 etc. → the resulting build is **GPL**. Opt-in only.
- **NDI:** the NDI SDK is **proprietary** (royalty-free, attribution required, redistribution restricted). It is **never vendored**; the `ndi` feature uses a runtime dynamic-load path, and the SDK/runtime is the user's responsibility. Document the EULA + attribution. NDI I/O is additionally **gated at runtime**: it stays inert until the operator explicitly confirms NDI license acceptance (`[system.ndi] accept_license`, audited), so even `ndi`-enabled builds carry no NDI obligations until a user accepts. Required trademark attribution is always preserved. See [io/ndi.md §7.5](../io/ndi.md).
- CI uses `cargo-deny` (`deny.toml`) to gate licenses and advisories.

---

## 8. Frontend conventions

- **Stack:** React 19 + TypeScript + Vite; **shadcn/ui** (Radix + Tailwind v4) design system; **TanStack Query** for server state; **TanStack Table** for lists.
- **Layout editor:** **react-konva** (free-form canvas: drag/resize/rotate/z-order) + **dnd-kit** (accessible palette drag & reorderable lists).
- **API client:** generated from the OpenAPI spec (`openapi-typescript` + `openapi-fetch`).
- **A11y & i18n:** target **WCAG 2.2 AA** (full keyboard + focus management via Radix; status never by color alone; an accessible non-canvas editing path for the layout editor) and internationalize the UI (Lingui + ECMAScript `Intl`, RTL). Light/dark via Tailwind tokens. See [web/accessibility.md](../web/accessibility.md) and [web/internationalization.md](../web/internationalization.md).
- **Build:** embedded into the `multiview` binary via `rust-embed` (single deployable); dev via Vite proxy.

---

## 9. Naming & style

- Crates: `multiview-<area>` (kebab); the library target is `multiview_<area>` (snake, automatic).
- Public types: `UpperCamel`; functions/fields: `snake_case`; features: `kebab-case`.
- Errors: per-crate `Error` enum via `thiserror`; app boundaries may use `anyhow`.
- Async: `tokio`. Logging/tracing: `tracing`. Serialization: `serde`.
- Docs: every public item documented; `#![warn(missing_docs)]` on library crates.
- Formatting: `rustfmt` (config in `rustfmt.toml`); lint clean under `clippy` (`-D warnings` in CI).
- **Inclusive language is required everywhere, always** — code, identifiers, comments, docs, commit messages, branches, config, logs, and UI copy. Prefer `allowlist`/`blocklist`, `primary`/`replica`, `main`, and gender-neutral wording. See [`CODE_OF_CONDUCT.md`](../../CODE_OF_CONDUCT.md). This may be enforced in review and CI.

---

## 10. Networking & addressing (IPv6-first)

**All network-facing surfaces are IPv6-first.** IPv4 is supported for **legacy interop only** and is
on a **deprecation path** — it will be removed from this product. New designs, code, config, and docs
MUST be IPv6-first: **never IPv4-only, never IPv4-first.** A network surface that cannot do IPv6 is a
defect. Full rationale + the remediation plan: [ADR-0042](../decisions/ADR-0042.md) /
[ipv6-first](../research/ipv6-first.md) (backlog: [ipv6-first-backlog](../development/ipv6-first-backlog.md)).

- **Bind / listen:** default to **dual-stack on `[::]`** (`IPV6_V6ONLY=false`), never `0.0.0.0`;
  loopback is `[::1]`, never `127.0.0.1`. Control plane, telemetry, preview, RTSP, HLS, SRT, and
  multicast all bind IPv6/dual-stack by default.
- **Addresses & URLs:** accept and prefer IPv6 wherever an address is parsed; **bracket IPv6 literals**
  in URLs (`udp://@[ff3e::1]:5004`, `rtp://[…]`, `[::1]:8080`). Examples and defaults lead with IPv6;
  an IPv4 form, if shown, is explicitly labelled *legacy*.
- **SDP:** handle `c=IN IP6` as a first-class form alongside `IN IP4`. The IPv6 multicast connection
  line carries **no TTL** (`c=IN IP6 <addr>[/<count>]` — the slash is an address *count*); only the
  IPv4 form takes `/ttl` (RFC 8866 §5.7).
- **Multicast:** IPv6 multicast `ff00::/8` (flags + scope nibbles) with IPv6 SSM `FF3x::/32` via
  **MLDv2** is the primary path; IPv4 `239/8` (admin-scoped) / `232/8` (SSM) + IGMPv3 is the legacy
  peer. A `join_multicast_v6` / protocol-agnostic `MCAST_JOIN_(SOURCE_)GROUP` path is required, not a
  follow-up.
