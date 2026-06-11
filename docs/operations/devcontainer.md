# Multiview Dev Container

A single-image Dev Container that builds **and** lets you test hardware
decode/encode where the host supports it: NVIDIA (NVDEC/NVENC) and Intel/AMD
(VAAPI) on Linux, with **graceful degradation to CPU/software paths** on hosts
without a usable GPU — notably macOS Apple Silicon.

Files live in [`.devcontainer/`](../../.devcontainer):

| File | Runs | Purpose |
| --- | --- | --- |
| `devcontainer.json` | tooling | Config: image, GPU policy, env, mounts, ports, extensions. |
| `Dockerfile` | build | Rust + FFmpeg/libav\* + VAAPI userspace + 1Password CLI. |
| `initialize.sh` | **host** | Guarantees `.env` exists; seeds the 1Password token; prints VAAPI hints. |
| `post-create.sh` | container | Fixes cache ownership, verifies toolchain, prints a capability summary. |

---

## What you get on each platform

| Host | Build | NVENC/NVDEC | VAAPI | Native HW codecs |
| --- | --- | --- | --- | --- |
| **Linux + NVIDIA** (Container Toolkit configured) | yes | **yes** (auto) | if `/dev/dri` also added | n/a |
| **Linux + Intel/AMD** | yes | no | **yes** (local override) | n/a |
| **macOS Apple Silicon** (Docker Desktop) | yes | **no — impossible** | **no — impossible** | Metal/VideoToolbox via a **native** build only |
| **Any host, no GPU / CI** | yes | no | no | software paths |

### The macOS reality (read this)

Docker Desktop on Apple Silicon runs a Linux VM on Apple's
Hypervisor/Virtualization.framework, which **exposes no virtual GPU** to guests.
There is **no `--gpus` and no `/dev/dri`** inside the Linux VM, and no workaround
in stock Docker Desktop. Therefore, **inside the container on macOS you only get
CPU/software codec paths.** Real hardware decode/encode on a Mac must be tested
with a **native macOS build** (Metal / VideoToolbox) run *outside* the container,
or on a remote/native **Linux GPU host**.

---

## GPU access design

### NVIDIA — `hostRequirements.gpu: "optional"`

```jsonc
"hostRequirements": { "gpu": "optional" }
```

Spec wording: *"a GPU is used when available, but is not required."* On a Linux
host with an NVIDIA GPU **and** the NVIDIA Container Toolkit configured, the Dev
Containers tooling **auto-injects `--gpus all`**. Everywhere else it is silently
omitted, so the same config still builds.

We deliberately do **not** hardcode `runArgs: ["--gpus","all"]`: that flag is
unconditional and would make `devcontainer up` **fail** on every host without the
NVIDIA runtime (all macOS, GPU-less Linux).

`containerEnv` sets `NVIDIA_DRIVER_CAPABILITIES=compute,utility,video`. The
**`video`** capability is mandatory — without it CUDA works but NVENC/NVDEC
silently fail. This is the classic trap for a video project.

**Host prerequisites (NVIDIA, one-time):**

```bash
# Install the NVIDIA Container Toolkit (see NVIDIA docs for your distro), then:
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
# Verify on the host:
docker run --rm --gpus all nvidia/cuda:12.4.1-base-ubuntu22.04 nvidia-smi
```

> `hostRequirements.gpu: "optional"` **silently no-ops** (no GPU, no error) if the
> toolkit isn't configured. Always confirm with `nvidia-smi` *inside* the container.

### Intel/AMD — VAAPI via a local override

`/dev/dri` is **not** in the committed config because an absent device makes
`docker run` fail. On an Intel/AMD Linux host, `initialize.sh` detects
`/dev/dri/renderD128` and prints the exact override, including the **numeric**
host render-group GID (using the group *name* does not work — GIDs differ across
hosts). Create an **uncommitted** `.devcontainer/devcontainer.local.json`:

```jsonc
{
  // GID printed by initialize.sh, e.g. 104/106/44 — host-specific.
  "runArgs": ["--device", "/dev/dri", "--group-add", "104"]
}
```

Then **Dev Containers: Rebuild Container** and verify with `vainfo`.

---

## Secrets: 1Password `.env` / `op` flow

The container loads a **gitignored repo-root `.env`** via
`runArgs: ["--env-file", "${localWorkspaceFolder}/.env"]`. The token is injected
only at **runtime** — never as a Docker build ARG/ENV (which would leak into image
layers via `docker history`). The `op` CLI is installed at build time (no secret).

`initialize.sh` (host) does the bridge:

1. **Always creates `.env`** (empty if needed). This is critical:
   `docker run --env-file <missing>` **fails** with *"no such file or directory."*
   Creating the file unconditionally is what keeps startup robust.
2. If `.env` has no token and **`~/.onepassword_token`** exists, it writes
   `OP_SERVICE_ACCOUNT_TOKEN=<token>` (trailing newline stripped, `chmod 600`,
   never echoed). Idempotent: it won't clobber a token you already set.

Inside the container, `op` authenticates non-interactively from
`OP_SERVICE_ACCOUNT_TOKEN`:

```bash
op whoami
op read "op://Claude API Access/GitHub API - Personal/token"
```

If there is no token, op is simply disabled — **build and tests still work**.
Prefer vault/item **IDs** over names to reduce service-account rate-limit
pressure. Do **not** set `OP_CONNECT_HOST`/`OP_CONNECT_TOKEN`; they override the
service-account token.

> Rotating the token: edit `.env` (or re-run with a fresh `~/.onepassword_token`)
> and **restart** the container — `--env-file` is re-read on each `docker run`.

---

## Base image, toolchain & caching

- **Base:** `mcr.microsoft.com/devcontainers/rust:1-trixie`. Trixie is
  load-bearing — it ships **FFmpeg 7.1.x**; bookworm's 5.1 is too old for the
  FFmpeg FFI. Provides rustup/cargo and a non-root `vscode` user (uid 1000).
- **Native deps (Dockerfile):** `clang llvm-dev libclang-dev pkg-config nasm
  yasm`, the full `libav*-dev` set, `libass-dev`, plus VAAPI userspace
  (`libva-dev vainfo va-driver-all intel-media-va-driver-non-free
  mesa-va-drivers`). `clang`/`pkg-config` are required for the FFmpeg FFI bindgen
  + system-library discovery.
- **Node** via the official Feature (`node:1`, version 22) for `web/` (Vite SPA).
- **Caching:** named volumes for `/usr/local/cargo/registry`,
  `/usr/local/cargo/git`, and `target/`. Empty volumes mount as root;
  `post-create.sh` chowns them to `vscode`.

---

## Lifecycle, ports, usage

Order (spec-fixed): `initializeCommand` (host) -> `postCreateCommand` (once,
in container) -> `postStartCommand` (every start).

- **initialize.sh** — seed `.env`, VAAPI hints (host).
- **post-create.sh** — chown caches, verify toolchain, `cargo fetch`, print the
  capability summary.
- **postStartCommand** — quick per-start GPU/VAAPI probe.

**Forwarded ports:** `8080` (API/serving), `5173` (Vite dev).

### Quick start

1. (Optional) Ensure `~/.onepassword_token` exists on the host for `op` access.
2. Open the repo in VS Code -> **Dev Containers: Reopen in Container**
   (or `devcontainer up --workspace-folder .`).
3. Read the capability summary printed by `post-create.sh`.

### Verification matrix

| Host | Expect |
| --- | --- |
| Linux NVIDIA | `nvidia-smi` works in-container; `ffmpeg -hide_banner -encoders \| grep nvenc` non-empty |
| Linux Intel/AMD | `vainfo` lists VAAPI profiles (after the local `/dev/dri` override) |
| macOS / no GPU | neither; software paths only (expected) |

Application/test code should runtime-probe (`nvidia-smi`, `vainfo`,
`/dev/dri`, NVENC init) and fall back to software, guarding hardware tests behind
`#[ignore]` or an env/feature flag so the suite passes on no-GPU hosts and CI.
