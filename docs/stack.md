# Platform & toolchain standards

Binding toolchain/platform standards for this repository: the operator-set
constraints on package managers, language and runtime, platforms, deploy target,
secrets, and who owns the infrastructure. The canonical
naming/structure/invariants source of truth remains
[`docs/architecture/conventions.md`](architecture/conventions.md); the process
contract — change classes, gates, evidence — is
[`docs/standards/engineering.md`](standards/engineering.md). This file records the
*toolchain and platform* standards both of those assume.

## Stack parameters

| Parameter | Value for this repo |
|-----------|---------------------|
| **Package managers** | **Cargo** (Rust workspace) is the primary; **npm** for the `web/` SPA. No other Rust or JS package manager — never mix in yarn/pnpm/bun for `web/`. |
| **Frozen install** | Rust: `cargo build --locked` / `cargo test --workspace --locked` (cargo **ignores** the lockfile without `--locked`). Web: `npm ci` (from the committed `web/package-lock.json`). |
| **Language / runtime** | **Rust 2021**, stable channel, MSRV **1.85** (raised from 1.82 by ed25519-dalek 3.0 / curve25519-dalek 5.0 edition-2024 deps — ADR-I010), pinned via [`rust-toolchain.toml`](../rust-toolchain.toml). `web/` is **TypeScript** (React 19 + Vite) under `strict` + `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes`. |
| **Hosting / deploy** | Self-hosted **binary/daemon `multiview`** and **OCI container images** published to **GHCR** (`.github/workflows/docker.yml`, `ffmpeg-base.yml`, `release*.yml`). Linux (x86_64 + aarch64) and macOS (Apple Silicon + Intel). **No Windows.** No cloud SaaS hosting runtime. |
| **Secret manager** | **1Password** (`op read` → `chmod 600` temp file → `rm -f`, or `op ssh-agent`). Secrets never touch git or terminal history. |
| **AI co-author trailer** | `Co-Authored-By: <model> <noreply@anthropic.com>` — name the model that actually authored the commit, after a blank line, one per line. |
| **CI** | **GitHub Actions** (`.github/workflows/`): `ci.yml` (change-classification gate → fmt/clippy/test, feature-gated clippy, AsyncAPI validation, inclusive-language, docs-sanity, change-class report), `gitleaks.yml` (secret scan), and `cargo deny check` (licenses + advisories). **No mutation-testing job exists in any workflow** — `cargo mutants` is required by [ADR-G002](decisions/ADR-G002.md) but is not wired into CI today. |
| **Vulnerability / advisory gate** | `cargo deny check advisories` (config: [`deny.toml`](../deny.toml)). |
| **Licence gate** | `cargo deny check licenses` (config: [`deny.toml`](../deny.toml)). Default build is LGPL-clean; `gpl-codecs`/`ndi` are off-by-default and escalate licensing (conventions §7). |
| **Build-output dirs** (gitignored + read-denied) | `target/` (Rust), `web/node_modules/`, `web/dist/`, `node_modules/`, `dist/`, `.multiview-build/`, `.ndi-sdk/`, `.memory/`. |

## Determinism

`Cargo.lock` **and** `web/package-lock.json` are committed. The toolchain is
pinned via `rust-toolchain.toml`; the Node version follows the devcontainer.
CI installs from the committed lockfiles with frozen/locked installs. No floating
version ranges in product crates beyond the workspace catalog pins.

## Infrastructure & runbooks

**Infrastructure-as-code is owned by the agent.** Design, deploy and manage every
resource yourself — never ask the operator to click-create one. Credentials live
in 1Password: mint a least-privilege scoped token per consumer, store it in the
secret manager **and** deploy it where it is used, and never echo, log or commit
it. Rotation is mint the replacement → update everywhere → revoke the old one
([engineering.md Part C](standards/engineering.md#c-safety-invariants-unconditional-class-independent)).

**Runbooks are written as you work, never after.** The same commit that provisions
or changes a resource — a CI secret, a workflow, a deployed service, a scoped
token, a local dev service — creates or updates that resource's runbook under
[`docs/runbooks/`](runbooks/). Runbooks are the operational *how* and must stay
current; ADRs in [`docs/decisions/`](decisions/) are the *why*. Structure:
[`docs/runbooks/CLAUDE.md`](runbooks/CLAUDE.md); proportionality:
[engineering.md Part G](standards/engineering.md#g-documentation-proportionality).

Verified against the repository state on **2026-07-27**.
