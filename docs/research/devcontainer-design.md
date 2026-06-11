> **Design brief — Dev Container.** Authoritative research/design record backing the implementation. Produced by a verification-hardened multi-agent research workflow (2026-06-02). Canonical crate/API naming lives in [docs/architecture](../architecture/). ADRs derived from this brief are in [docs/decisions](../decisions/).

---

## Multiview Dev Container — Design Brief

A **single-image** Dev Container (Dockerfile-based, no docker-compose: there are no sidecar services — the API and Vite SPA run as processes inside one container) that builds **and** tests hardware decode/encode where the host allows it, with honest graceful degradation everywhere else.

### GPU access (verified against the spec)
- **NVIDIA:** `hostRequirements.gpu: "optional"` — spec wording is *"a GPU is used when available, but is not required."* On a Linux host with an NVIDIA GPU **and** the NVIDIA Container Toolkit configured, the tooling **auto-injects `--gpus all`**; everywhere else it is silently omitted. We deliberately do **not** hardcode `runArgs:["--gpus","all"]` — that flag is unconditional and breaks `devcontainer up` on every host without the NVIDIA runtime (all macOS, GPU-less Linux). `containerEnv` sets `NVIDIA_DRIVER_CAPABILITIES=compute,utility,video`; the **`video`** cap is mandatory or NVENC/NVDEC silently fail while CUDA still works.
- **Intel/AMD VAAPI:** `/dev/dri` is **not** in the committed config because an absent device fails `docker run`. `initialize.sh` detects `/dev/dri/renderD128` on the host and prints the exact uncommitted `.devcontainer/devcontainer.local.json` override including the **numeric** host render-group GID (the group *name* fails — GIDs differ per host).
- **macOS Apple Silicon:** Docker Desktop runs a Linux VM on Hypervisor.framework with **no virtual GPU** — there is no `--gpus` and no `/dev/dri` inside the VM, no workaround. In-container = CPU/software only; native Metal/VideoToolbox must be tested with a native macOS build outside the container.

### Secrets (1Password)
The container loads a **gitignored repo-root `.env`** via `runArgs:["--env-file","${localWorkspaceFolder}/.env"]`. The token is injected only at **runtime**, never as a build ARG/ENV (which would leak into image layers via `docker history`). `op` is installed at build time (no secret). Host-side `initialize.sh`:
1. **Always creates `.env`** (empty if needed) — `docker run --env-file <missing>` **fails** with *"no such file or directory"*, so unconditional creation is what keeps startup robust. (Verified live in research on current Docker.)
2. If `.env` lacks a token and `~/.onepassword_token` exists, writes `OP_SERVICE_ACCOUNT_TOKEN` (newline stripped, `chmod 600`, never echoed). Idempotent — won't clobber a token you set. **Dry-run tested:** empty-`.env` path, seed path, idempotent re-run, user-token preservation, and no-leak-to-stdout all pass.

Inside the container `op` authenticates non-interactively (`op whoami`, `op read "op://Vault/Item/field"`). No token => op disabled, build/test still work. `${localEnv:OP_SERVICE_ACCOUNT_TOKEN}` is surfaced in `remoteEnv` only as a harmless convenience (it reads a host *env var*, not the file — unreliable for GUI-launched VS Code, hence the `.env` bridge is primary).

### Base image, toolchain, caching
- **`mcr.microsoft.com/devcontainers/rust:1-trixie`** — trixie is load-bearing: ships **FFmpeg 7.1.x** (bookworm's 5.1 is too old for the FFmpeg FFI). The `2.x-trixie`/`1-trixie` tags are published (verified on MS Artifact Registry; tracking issue #1269 closed). Provides rustup/cargo + non-root `vscode` (uid 1000).
- **Dockerfile native deps:** `clang llvm-dev libclang-dev pkg-config nasm yasm`, full `libav*-dev` set + `libass-dev`, VAAPI userspace (`libva-dev vainfo va-driver-all intel-media-va-driver-non-free mesa-va-drivers`), and `op` via the official apt repo with `$(dpkg --print-architecture)` so it builds on amd64 **and** arm64.
- **Node** via official Feature (`node:1`@22) for `web/`. **Caching:** named volumes for `/usr/local/cargo/{registry,git}` + `target/`; `post-create.sh` chowns them (empty volumes mount as root).

### Lifecycle, ports, usage
`initializeCommand` (host: seed `.env`, VAAPI hints) -> `postCreateCommand` (chown caches, verify toolchain, `cargo fetch`, print a capability summary) -> `postStartCommand` (per-start GPU/VAAPI probe). Forwarded: **8080** (API), **5173** (Vite).

**What a dev gets:** Linux NVIDIA host -> `nvidia-smi` + `*_nvenc` encoders in-container. Linux Intel/AMD -> `vainfo` after the local `/dev/dri` override. macOS / no-GPU / CI -> build + software paths only (expected). Full doc at `docs/operations/devcontainer.md`. JSONC validated; both scripts pass `sh -n`/`bash -n`.