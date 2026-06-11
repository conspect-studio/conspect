# CLAUDE.md — Agent Guide for the Multiview Repository

This is the working guide for Claude Code (and any coding agent) operating **in this repo**.
It tells you what Multiview is, where things live, how to build/test/lint, the conventions you must
follow, which deep brief to read before touching a subsystem, and the **safety rules you must never
break**.

> **Source of truth:** [`docs/architecture/conventions.md`](docs/architecture/conventions.md) pins
> the canonical crate names, API paths, feature flags, invariants, and licensing. Where any other
> doc disagrees, **conventions win**, and the Rust code is the ultimate source of truth. This file
> summarizes; it does not override conventions.

---

## 0. Ownership — you run this

You are the **engineering manager and owner of execution** for this repo, not an assistant waiting
for instructions. Decide, build, verify, ship. The operator sets direction and priorities; turning
that direction into shipped, gated, hardware-validated code is **your** job, and you carry it to
done without parking it.

- **"Pending operator feedback / review / approval" is not a valid state for buildable work.** If a
  thing is designed and unblocked, build it. The operator reviews via commits, PRs, and the running
  system — never via a pre-build approval gate you invent. Do **not** stop to ask "should I
  proceed?" — proceed.
- **Design-first is a quality step, not a hand-off.** For a non-trivial subsystem, write the
  brief/ADR first and adversarially verify it — then **implement it yourself**. Finishing the design
  does not return the work to the operator: a committed, verified design means *start coding*, not
  *wait*.
- **Default and move.** When a choice is reversible or has a sensible default, pick it, state it in
  one line, and continue. Reserve genuine questions for decisions that are (a) hard to reverse or
  outward-facing **and** (b) would materially change direction **and** cannot be sensibly defaulted —
  and even then prefer the reversible default over blocking. Each such question is a cost; spend it
  rarely.
- **Autonomy does NOT bypass quality.** The three guardrail pillars (absolute typing, TDD-first with
  real tests, adversarial review), the safety rules (§7), and confirmation for destructive or
  outward-facing actions (publishing public, deleting infrastructure, external comms) still hold.
  Autonomy is about **execution pace and decisiveness**, never about lowering the bar or weakening a
  test.
- **Drive the loop to the stated finish.** Hold the whole agenda, fan out independent work, then
  integrate + gate + hardware-validate it **yourself**. Report a thing as done only when it is green
  and verified. Keep going until the operator's goal is actually met.
- **NEVER defer, stub, scaffold, or partial-ship.** This is absolute. A thing is "done" only when it
  is **wired end-to-end and working** — not when a "core" lands with the integration parked for
  "a later wave", not a `todo!()`/placeholder, not "modelled but the real path isn't built", not
  "honestly documented as a follow-up". Splitting a unit of work for *parallelism* (e.g. a crate-core
  + its thin integration) is allowed **only if every part ships in the same push** — never "core now,
  wiring later". When something required is identified: **(a)** if it is **not documented**, fan out
  to write the brief/ADR + plan first, then ship it; **(b)** if it **is documented** and the pieces
  can fan out, parallel-ship **all** of them (core **and** integration) together; **(c)** otherwise
  ship it now, complete. Deferred/parked work is technical debt — we do not create it. If you
  genuinely cannot finish a thing this turn (a real external blocker), say so explicitly and name the
  blocker; "I'll wire it up later" is not a finish.

---

## Engineering guardrails (non-negotiable)

Full standard: [`docs/development/agent-guardrails.md`](docs/development/agent-guardrails.md). Conventions/naming source of truth: [`docs/architecture/conventions.md`](docs/architecture/conventions.md). All three pillars are blocking CI.

**1. Absolute typing — no untyped, no escape hatches.**
- Rust: lint policy is centralized in root `[workspace.lints]`; every crate uses `[lints]` `workspace = true`. **Denied in non-test code:** `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`, `get_unwrap`, `indexing_slicing`, `as_conversions`, `dbg_macro`, `print_stdout/stderr`, `str_to_string`, `exit`, `mem_forget`. `unsafe_code = forbid` (FFI crates: `deny` + `// SAFETY:`). Prefer `?`/`match`/`unwrap_or`/`let-else` over unwrap/expect. Use newtype+`TryFrom`, typestate, `#[non_exhaustive]`, exhaustive `match`. **Ban `dyn Any`.** Tests relaxed via `clippy.toml allow-*-in-tests`; **every `tests/` file needs `#![allow(clippy::unwrap_used, …)]`** (those options don't cover integration tests).
- TS: `tsconfig` `strict` **+** `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes` (+ override/returns/switch flags). ESLint `strictTypeChecked` (type-aware) bans `any` + `no-unsafe-*`; `ban-ts-comment` (no `@ts-ignore`/`@ts-nocheck`, `@ts-expect-error` allow-with-description) and `no-non-null-assertion` (no `!`).
- Gates: `cargo clippy --all-targets --all-features -- -D warnings`, `tsc --noEmit`, `eslint . --max-warnings=0`.

**2. TDD-first with REAL tests.** Write the failing test FIRST; run it and paste the failing output; **commit failing tests separately**; then implement to green WITHOUT touching tests. **NEVER weaken/delete/skip/`#[ignore]` a test, weaken an assertion, or edit code-under-test to fit a weak test — STOP and ask a human.** No tautological/assertion-free tests. Coverage is a floor; **mutation score is the target**: `cargo mutants --in-diff` on PRs (a MISSED mutant in changed code fails the PR), full run nightly. Property tests required for pure/stateful logic (`proptest`/`proptest-state-machine`, commit `proptest-regressions/`; `fast-check` for TS). Keep a held-out acceptance suite the author never sees.

**3. Adversarial cross-vendor review (required).** Code authored by one vendor is reviewed by a **different** vendor (Claude ↔ Codex ↔ Gemini) in a **fresh context** seeing only diff + spec + checklist. Reviewer scope: correctness/security/spec/guardrail defects only. Reviewer checks the typing + TDD rules above and that no test was weakened. Unanimous approval is a yellow flag. **A human is always the final approver.**

**Baseline:** explore→plan→implement→commit; minimal in-scope diffs with a stated out-of-scope boundary; **no silent suppression** (any `#[allow]`/`eslint-disable`/`.skip` needs an inline justification + review; fix root cause); show evidence not assertions; propagate errors with `?`, never swallow; build `--locked` + `npm ci`, commit lockfiles; secrets via 1Password (`op read`→`chmod 600`→`rm -f`), gitleaks pre-commit + CI; `cargo deny check`; Conventional Commits + `Co-Authored-By:` trailer; ADRs in `docs/decisions/` for non-trivial decisions; **no copying proprietary/competitor features, designs, or trademarked terms — build from open standards + original work, keep docs vendor-neutral** ([CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)). Definition of Done in the full doc.

---

## Working in this monorepo — navigation & context discipline

Multiview is a complex monorepo (16-crate Cargo workspace + `web/` SPA + a large `docs/` tree with
10 research briefs and 89 ADRs). To work efficiently here without exhausting your context
window, follow the official Claude Code guidance for large/complex codebases:
[code.claude.com/docs/en/large-codebases](https://code.claude.com/docs/en/large-codebases).
The full agent playbook is [`docs/development/working-in-this-monorepo.md`](docs/development/working-in-this-monorepo.md);
the one-screen layout is [`docs/development/codebase-map.md`](docs/development/codebase-map.md).

**Layered, on-demand instructions.** Each crate has a short `crates/<crate>/CLAUDE.md` (and the
SPA has `web/CLAUDE.md`) with that area's invariants and the exact brief(s)+ADRs to read first.
Per the docs, a subdirectory `CLAUDE.md` **loads automatically when Claude reads a file in that
directory** — so you pay context only for the crate you're in, not all 16. **Start Claude from
the crate you're working in** to load just the root file plus that crate's file. (This root
`CLAUDE.md` is re-injected after `/compact`; nested crate files reload on the next read in that
crate.)

**Context discipline.**
- Work **one crate/area per task**; `/clear` between unrelated tasks.
- **Fan out searches into subagents** — when a side task means reading many files (find a
 symbol's callers, summarize a brief, audit a diff against invariants), delegate it so the file
 reads stay out of your main thread and you get back only the summary.
- Navigate with `rg` and the crate map, not exhaustive reads. Never open `target/`,
 `node_modules/`, or `.multiview-build/` (git-ignored, excluded from search).
- Recommended: install a Rust code-intelligence plugin for symbol navigation across the workspace.

**The core workflow: read the brief before touching subsystem X.** The per-crate `CLAUDE.md`
files and §6 below name the brief and ADRs for each subsystem — read them (via a subagent) before
writing code. Always re-check invariants **#1 (output-clock)** and **#10 (isolation)**; a change
that risks either means stop and write a design note.

---

## 1. What Multiview is

Multiview is an efficient, hardware-accelerated, **Rust live video multiview generator**. It ingests many
live sources (RTSP, HLS/M3U, MPEG-TS, SRT, RTMP, NDI, file/test), composites them into a templated
multiview **on the GPU**, and serves the result robustly (RTSP, HLS/LL-HLS, NDI, RTMP/SRT push). The
binary/daemon is **`multiview`**.

Design pillars (see the briefs for depth):
- **Hybrid engine:** FFmpeg/libav for demux/decode/encode; **custom Rust + GPU** for compositing and
 serving. There is no FFmpeg `xstack_cuda`/LL-HLS muxer to lean on — those are ours.
- **Zero-copy islands per GPU vendor.** Cross-vendor on-GPU zero-copy does not exist on desktop; we
 budget an explicit copy at every vendor/NDI/CPU boundary.
- **Bulletproof continuous output** on commodity hardware: the output never stalls, never falters.
- **Edition:** Rust 2021 (pinned via `rust-toolchain.toml`). **License:** source-available under the **Multiview Source-Available Non-Commercial License** (© Aperim Pty Ltd) — free for non-commercial/home use, commercial licence otherwise.
- **Platforms:** Linux (x86_64 + aarch64; NVIDIA via Container Toolkit, Intel/AMD via VAAPI) and
 macOS (Apple Silicon + Intel, native). **No Windows.**

> **Repo status:** early stage. The architecture, ADRs, and research briefs are written, and the
> `crates/`, `web/`, `xtask/`, and workspace `Cargo.toml` exist as a **compiling scaffold**
> (`cargo check`/`clippy`/`fmt` green) of trait/type stubs being built out against the documented
> contracts. Match the canonical names and structure below exactly; do not invent alternatives.

---

## 2. The canonical invariants (do not violate)

These are load-bearing across every doc and every line of code. Full text:
[`conventions.md` §5](docs/architecture/conventions.md). One-line each, with the deep source:

| # | Invariant | One-liner | Deep brief / ADR |
|---|-----------|-----------|------------------|
| 1 | **Output-clock** | One fixed-cadence monotonic clock emits exactly one valid frame per tick, forever, independent of any input. Inputs are *sampled*, never *pacing*. `out_pts = f(tick)`. | [streaming-gotchas](docs/research/streaming-gotchas.md), [ADR-T001](docs/decisions/ADR-T001.md), [ADR-R001](docs/decisions/ADR-R001.md) |
| 2 | **Last-good-frame + state machine** | Inputs write lock-free single-slot stores; compositor reads latest (or placeholder), never blocks. Tiles ride LIVE→STALE→RECONNECTING→NO_SIGNAL. | [resilience-and-av](docs/research/resilience-and-av.md), [ADR-T002](docs/decisions/ADR-T002.md) |
| 3 | **Unified timing model** | Per-input PTS normalized (unwrap 33-bit, genpts fallback, monotonic guard) and rebased to one ns timeline; output re-stamps all PTS/DTS from the tick counter. NTSC `1001` as exact rationals — **never float fps**. | [streaming-gotchas §0,§2](docs/research/streaming-gotchas.md), [ADR-T003](docs/decisions/ADR-T003.md) |
| 4 | **HLS ingest pacing** | Live/VOD-as-live inputs paced to wall-clock by PTS (custom pacer). `-re` is for files, **not** live ingest. | [streaming-gotchas §3](docs/research/streaming-gotchas.md), [ADR-T004](docs/decisions/ADR-T004.md) |
| 5 | **NV12-throughout** | Frames stay NV12 (1.5 B/px); never materialize RGBA per tile. YUV→RGB happens in-shader at tile size. | [efficiency](docs/research/efficiency.md), [ADR-E002](docs/decisions/ADR-E002.md) |
| 6 | **Decode-at-display-resolution** | Decode each source near its displayed size where the backend supports it; budget decode in megapixels/sec. | [efficiency](docs/research/efficiency.md), [ADR-E001](docs/decisions/ADR-E001.md) |
| 7 | **Encode-once-mux-many** | Composite once, encode the canvas once per rendition, fan the *same* packets to all transports. Separate encode only when codec/res/bitrate differ. | [efficiency](docs/research/efficiency.md), [ADR-E003](docs/decisions/ADR-E003.md), [ADR-E004](docs/decisions/ADR-E004.md) |
| 8 | **Color pipeline order (never reorder)** | detect 4 axes → range-expand → YUV→RGB matrix → linearize (EOTF) → primaries convert in linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV + range compress → **tag output** → verify with ffprobe. | [color-management](docs/research/color-management.md), [ADR-C003](docs/decisions/ADR-C003.md), [ADR-C006](docs/decisions/ADR-C006.md) |
| 9 | **Resource-adaptive degradation** | Closed control loop (sense→estimate→plan→apply, with hysteresis) sheds load tile-by-tile cheapest-impact-first **before** program output is touched. Bounded queues drop, never grow. | [efficiency](docs/research/efficiency.md), [ADR-E007](docs/decisions/ADR-E007.md) |
| 10 | **Isolation** | Control plane, preview, realtime are best-effort and **physically incapable of back-pressuring the engine** (watch/broadcast channels; bounded drop-oldest; the engine never awaits a client). CI chaos gate enforces this. | [realtime-api](docs/research/realtime-api.md), [ADR-RT004](docs/decisions/ADR-RT004.md), [ADR-P001](docs/decisions/ADR-P001.md) |
| 11 | **Live-apply classification** | Every management change is Class-1 (hot/seamless at a frame boundary) vs Class-2 (controlled reset via make-before-break parallel-output migration); the API surfaces which before applying. | [management-capability-matrix](docs/research/management-capability-matrix.md), [ADR-R004](docs/decisions/ADR-R004.md) |

**If a change would break invariant #1 or #10, stop.** Those two — output never falters, control/preview
never back-pressures the engine — are the heart of the product. Any PR that risks them needs an explicit
design note and a chaos/soak test.

---

## 3. Crate map — where things live

All crates are prefixed `multiview-` under `crates/`; the library target is `multiview_<area>` (snake).
Hardware/FFI/GPU code sits **behind off-by-default Cargo features** so the default `cargo check`
builds the pure-Rust trait/type layer (LGPL-clean, no native deps, GPU-free CI). Canonical list:
[`conventions.md` §3](docs/architecture/conventions.md).

| Crate | You touch it when… | Optional features |
|-------|--------------------|-------------------|
| `multiview-core` | Editing shared types/traits: `Frame`, `PixelFormat` (NV12 canonical), `ColorInfo` (4 axes), clock/`MediaTime`, layout/template model, error taxonomy, stage traits (`Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`, `Backend`). **No FFI.** | — |
| `multiview-hal` | Capability detection, backend registry, per-stage negotiation + cost model/planner (admission/degradation inputs). | `cuda`, `vaapi`, `qsv`, `videotoolbox` |
| `multiview-ffmpeg` | Safe RAII wrappers over libav\* (demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map). All raw FFI is owned here. | `ffmpeg`, `gpl-codecs` |
| `multiview-compositor` | The custom GPU compositor: scale + place + per-tile color convert + linear-light blend + overlay compositing. wgpu baseline; vendor fast paths. | `wgpu` (default), `cuda`, `metal`, `vaapi` |
| `multiview-framestore` | Per-tile last-good-frame stores (lock-free triple-buffer) + the tile state machine. | — |
| `multiview-audio` | Per-input audio decode/resample/mix/route (program bus + discrete tracks) + EBU R128 metering. | `ffmpeg` |
| `multiview-overlay` | Overlay layers + text rendering + subtitle ingest/render (libass) and passthrough. | `libass` |
| `multiview-input` | Ingest sources, the **input pacer**, jitter buffers, timestamp normalization, supervised reconnect. | `ffmpeg`, `ndi` |
| `multiview-output` | Output sinks/servers: RTSP server, HLS/LL-HLS packager, NDI out, RTMP/SRT push; encode-once-mux-many fan-out. | `ffmpeg`, `ndi` |
| `multiview-engine` | The **protected output core**: fixed-cadence output clock, compositor drive, supervisor/actors, hot-reconfiguration, admission/degradation loop. | — |
| `multiview-config` | Config & template schema (serde), validation, config-as-code import/export. | — |
| `multiview-events` | Shared realtime event types + versioned envelope. | — |
| `multiview-control` | axum REST + WebSocket + SSE API: OpenAPI (utoipa+Scalar), auth, SQLite (sqlx), command-bus shell, embedded SPA. | `openapi` (default), `embed-web` |
| `multiview-preview` | Preview taps (input/program/output), preview encoder pool, WHEP/MJPEG/snapshot. **Strictly isolated** from the program path. | `webrtc` |
| `multiview-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). | — |
| `multiview-cli` | Binary **`multiview`**: wires engine + control plane; config load; run/validate. | aggregates feature flags |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint, package). | — |

**Dependency direction:** `core` ← everything; leaf crates depend on `core` (+ `hal`/`ffmpeg`/`events`
as needed); `engine` depends on the media crates; `control`/`preview` depend on `engine` + `events`;
`cli` depends on all. **No cycles.**

> The deep briefs ([core-engine](docs/research/core-engine.md),
> [management-capability-matrix](docs/research/management-capability-matrix.md)) sometimes use older
> crate names (`multiview-sys`, `multiview-io`, `multiview-server`). **Use the canonical names above**, not the
> brief's working names.

Other top-level dirs: `web/` (the React SPA), `examples/` (multiview configs + layout templates),
`deploy/` (Dockerfile, compose, container assets), `.github/workflows/` (CI). `.multiview-build/` is a
**git-ignored transient working dir — not part of the product**; do not commit anything from it.

---

## 4. Build / test / lint commands

The **default** feature set is pure-Rust and builds GPU-free in CI. Add hardware features explicitly.

```bash
# Pure-Rust, no native deps — this is the CI-green baseline
cargo check --workspace
cargo build --workspace
cargo test --workspace

# Lint + format (CI runs clippy with -D warnings)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all # write
cargo fmt --all -- --check # verify (CI)

# Licenses / advisories gate (uses deny.toml)
cargo deny check

# Feature builds (opt-in native deps; need the toolchains/SDKs installed)
cargo check -p multiview-cli --features nvidia # cuda + ffmpeg + wgpu
cargo check -p multiview-cli --features apple # videotoolbox + metal + ffmpeg
cargo check -p multiview-cli --features linux-vaapi # vaapi + qsv + ffmpeg + wgpu
cargo check -p multiview-cli --features full # everything non-GPL

# Dev automation (web build, OpenAPI/AsyncAPI codegen, packaging)
cargo xtask --help
cargo xtask build-web # build the SPA and stage it for rust-embed
cargo xtask gen-openapi # regenerate the OpenAPI/AsyncAPI specs

# Web SPA (in web/) — React 19 + TS + Vite
npm --prefix web ci
npm --prefix web run dev # Vite dev server (proxies to the API)
npm --prefix web run build # production build (embedded via rust-embed)
npm --prefix web run lint
```

Notes:
- **Default `cargo check` must stay green without GPUs.** Never make a hardware feature default-on.
- Run a single crate's tests with `cargo test -p multiview-<area>`.
- GPU decode/composite/encode + SSIM/PSNR run only on GPU-tagged self-hosted runners; the software
 backend is the CI enabler. See [efficiency](docs/research/efficiency.md) / core-engine §19 for the
 testing tiers (golden-frame on CPU only; GPU output uses SSIM/PSNR thresholds, never bit-exact).
- **Feature flags are canonical** — see [`conventions.md` §4](docs/architecture/conventions.md). Codec
 backends: `cuda`, `videotoolbox`, `vaapi`, `qsv`, `software` (always on). Compositor: `wgpu`
 (default), `metal`, `cuda`. Media: `ffmpeg`. License-escalating (off by default): `gpl-codecs`
 (→ GPL), `ndi` (proprietary, runtime-loaded). Web: `openapi` (default), `embed-web`, `webrtc`.

---

## 5. Coding conventions

Full text: [`conventions.md` §9](docs/architecture/conventions.md). The essentials:

- **Naming:** crates `multiview-<area>` (kebab); public types `UpperCamel`; functions/fields `snake_case`;
 features `kebab-case`.
- **Errors:** per-crate `Error` enum via `thiserror`; app boundaries (`multiview-cli`) may use `anyhow`.
- **Async:** `tokio`. **Logging/tracing:** `tracing`. **Serialization:** `serde`.
- **Serde enums:** use **adjacently/internally-tagged** (`#[serde(tag = "kind")]`) for source/overlay/fit
 unions — robust across TOML and JSON. **Never `untagged`.**
- **Networking — IPv6-first** (see [`conventions.md` §10](docs/architecture/conventions.md), [ADR-0042](docs/decisions/ADR-0042.md)):
 all network-facing surfaces are **IPv6-first**; IPv4 is **legacy-only and on a deprecation path** —
 **never design/document IPv4-only or IPv4-first.** Bind **dual-stack `[::]`** (`IPV6_V6ONLY=false`),
 not `0.0.0.0`; loopback `[::1]`; bracket IPv6 URL literals; SDP `c=IN IP6` (no TTL); IPv6 multicast
 `ff00::/8`+SSM `FF3x::/32` via MLDv2 primary; examples lead IPv6.
- **Docs:** every public item documented; library crates carry `#![warn(missing_docs)]`.
- **Formatting:** `rustfmt` (`rustfmt.toml`); clippy-clean under `-D warnings`.
- **API conventions** (when touching `multiview-control`, see [`conventions.md` §6](docs/architecture/conventions.md)):
 REST base `/api/v1`; long-running ops return `202 Accepted` + operation id (result on the realtime
 stream); RFC 9457 `application/problem+json` errors; `ETag`/`If-Match` + `412`; `Idempotency-Key` on
 start/stop/swap; OpenAPI 3.1 via utoipa (Scalar at `/docs`); WebSocket primary at `/api/v1/ws`, SSE
 fallback at `/api/v1/events`.
- **Frontend** (`web/`, see [`conventions.md` §8](docs/architecture/conventions.md)): React 19 + TS +
 Vite; shadcn/ui (Radix + Tailwind v4); TanStack Query/Table; **react-konva** + **dnd-kit** for the
 layout editor; API client generated from the OpenAPI spec (`openapi-typescript` + `openapi-fetch`);
 WCAG 2.1 AA.

---

## 6. Read these briefs BEFORE touching subsystem X

Don't guess at a subsystem's design — the briefs are verification-hardened and capture the footguns.
Read the relevant one (and its ADRs) **before** writing code:

| Working on… | Read first |
|-------------|------------|
| Output clock, compositor drive, supervisor, hot-reconfig (`multiview-engine`) | [core-engine](docs/research/core-engine.md) §4–§12, [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §0](docs/research/streaming-gotchas.md), ADR-T001/R001/R004 |
| HAL, capability detection, backend negotiation/planner (`multiview-hal`) | [core-engine §6](docs/research/core-engine.md), [efficiency](docs/research/efficiency.md), ADR-0003/0004/E008 |
| libav wrappers, hwaccel, frame pools, FFI (`multiview-ffmpeg`) | [core-engine §7,§8.1,§12](docs/research/core-engine.md), ADR-0002/0004 |
| GPU compositor, color math, fit/crop/overlays (`multiview-compositor`) | [color-management](docs/research/color-management.md), [core-engine §8.2,§13](docs/research/core-engine.md), ADR-C001..C006, ADR-E002 |
| Frame stores + tile state machine (`multiview-framestore`) | [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §1,§7](docs/research/streaming-gotchas.md), ADR-T002 |
| Ingest, input pacer, jitter, PTS normalization, reconnect (`multiview-input`) | [streaming-gotchas §1–§3,§5–§7](docs/research/streaming-gotchas.md), [core-engine §9.1](docs/research/core-engine.md), ADR-T003/T004/T006/T007/T008 |
| Output servers, RTSP/HLS·LL-HLS/NDI/push (`multiview-output`) | [streaming-gotchas §4](docs/research/streaming-gotchas.md), [core-engine §9.2](docs/research/core-engine.md), ADR-0006/0007/T005 |
| Audio decode/mix/route + R128 (`multiview-audio`) | [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §5,§7](docs/research/streaming-gotchas.md), ADR-R005/R006/T006 |
| Overlays + subtitles (`multiview-overlay`) | [resilience-and-av](docs/research/resilience-and-av.md), ADR-R007/R008 |
| Config/template schema, validation, config-as-code (`multiview-config`) | [core-engine §13,§14](docs/research/core-engine.md), [management-capability-matrix](docs/research/management-capability-matrix.md), ADR-0010 |
| Control API: REST/WS/SSE, auth, command bus (`multiview-control`) | [web-api-stack](docs/research/web-api-stack.md), [realtime-api](docs/research/realtime-api.md), [management-capability-matrix](docs/research/management-capability-matrix.md), ADR-RT001..RT006, ADR-W001..W008 |
| Events/envelope (`multiview-events`) | [realtime-api](docs/research/realtime-api.md), ADR-RT002/RT003 |
| Preview taps, WHEP/MJPEG, cue/pre-warm (`multiview-preview`) | [preview-subsystem](docs/research/preview-subsystem.md), ADR-P001..P005 |
| Telemetry, metrics, health (`multiview-telemetry`) | [core-engine §15](docs/research/core-engine.md), [resilience-and-av](docs/research/resilience-and-av.md), ADR-R009 |
| Any management surface (API ↔ UI ↔ engine) | [management-capability-matrix](docs/research/management-capability-matrix.md) — the authoritative capability table |
| Licensing / build profiles | [core-engine §17,§18](docs/research/core-engine.md), [`conventions.md` §7](docs/architecture/conventions.md), ADR-0011/0012 |

Indexes: all briefs in [`docs/research/`](docs/research/README.md); all decisions in
[`docs/decisions/`](docs/decisions/README.md) (89 ADRs, grouped by area).

---

## 7. Safety rules (engine + FFI + process)

These are non-negotiable for code in this repo.

1. **Never break the output-clock invariant.** The output stage emits one valid, correctly-timestamped
 frame per tick forever. No code path on the data plane may block waiting for an input, a client, or a
 lock that an input/client holds. Inputs are sampled; they never pace.
2. **Preview / control / realtime must never back-pressure the engine.** Use watch/broadcast channels
 and bounded **drop-oldest** queues. The engine **never `.await`s a client** and never sends on a
 channel that a slow consumer can fill. Conflate high-rate telemetry (audio meters ~10–30 Hz). A CI
 chaos gate enforces this — if you add a channel from engine→outside, prove it can't stall the engine.
3. **No `unwrap`/`expect`/`panic!` on the hot path** (decode→composite→encode→mux, the output clock,
 frame stores). Hot-path code returns/handles errors and **holds last-good** rather than crashing.
 `unwrap` is acceptable only in tests and in clearly non-hot startup/config code with a justification.
4. **FFI safety:** all raw libav/CUDA/Metal/Vulkan/NDI FFI lives behind safe wrappers (FFI is owned by
 `multiview-ffmpeg` and the feature-gated backend modules). `unsafe` blocks carry a `// SAFETY:` comment
 stating the invariant upheld. libav context wrappers are `Send + !Sync` (or Mutex-guarded); never
 share one context across threads unsynchronized. `get_format` and other `extern "C"` callbacks run on
 foreign/decoder threads — keep them allocation-light and never let a Rust panic unwind across the FFI
 boundary. Return buffers to pools via `Drop`; never run that `Drop` inside a Tokio async destructor.
5. **Bounded memory everywhere on the data plane.** Queues drop, never grow. No unbounded channels into
 the engine. Frame buffers come from per-device pools allocated at start, never per-frame.
6. **Timestamps:** never feed raw input PTS to the encoder/muxer — re-stamp from the tick counter. Carry
 internal time as i64 ns / exact rationals; **never float fps** (drifts ~3.6 s/hour).
7. **Color order is fixed** (invariant #8). Range is handled in-shader exactly once. Tagging ≠
 converting — always tag the output and verify with ffprobe.
8. **Licensing discipline:** keep the default build LGPL-clean. Do scaling/compositing in-house
 (`scale_cuda`, not `scale_npp`). `gpl-codecs` (x264/x265) makes the whole build GPL — opt-in only.
 Never vendor the proprietary NDI SDK; the `ndi` feature is runtime-loaded (`NDIlib_v6_load()`) with
 mandatory attribution. CI `cargo deny` gates this. See [`conventions.md` §7](docs/architecture/conventions.md).

---

## 8. Navigating `docs/`

```
docs/
├── architecture/conventions.md # SOURCE OF TRUTH — read this first, always
├── research/ # deep, verification-hardened design briefs (the "why")
├── decisions/ # ADRs (the load-bearing decisions; 72, grouped by area)
├── reference/ # bibliography, example streams
└── development/ # completeness checklist, process docs
```

- Start at [`conventions.md`](docs/architecture/conventions.md) for canonical names/paths/flags.
- For *why* a subsystem is built a certain way, read its brief in
 [`docs/research/`](docs/research/README.md) (see §6 above for the mapping).
- For a specific decision and its alternatives/consequences, find the ADR in
 [`docs/decisions/`](docs/decisions/README.md). ADR prefixes track area: `0001`+ core engine,
 `R*` resilience/AV, `E*` efficiency, `C*` color, `T*` streaming/timing, `P*` preview, `RT*`
 realtime API, `M*` management, `W*` web/API stack.
- Prefer linking to a brief over duplicating it. When code and a brief disagree, the code wins and the
 brief should be updated — flag the drift.

---

## 9. Git & workflow expectations

- Commit or push **only when the user asks**. If on `main`, branch first.
- End commit messages with: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- End PR bodies with: `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Before proposing a PR that touches the engine, run `cargo fmt --all -- --check`,
 `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and (if
 dependencies changed) `cargo deny check`.
- Don't add Windows support, per-tile re-encode/ABR-per-tile, or cross-vendor on-GPU zero-copy — these
 are explicit **non-goals** ([core-engine §2](docs/research/core-engine.md)).
