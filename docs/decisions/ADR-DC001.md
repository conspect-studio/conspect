# ADR-DC001: GPU passthrough via hostRequirements.gpu "optional" (no hardcoded --gpus / --device)

- **Status:** Proposed
- **Area:** Dev Container
- **Date:** 2026-06-02
- **Source brief:** [devcontainer-design.md](../research/devcontainer-design.md)

## Decision

Express NVIDIA GPU access declaratively with `hostRequirements.gpu: "optional"` and set `NVIDIA_DRIVER_CAPABILITIES=compute,utility,video` (plus `NVIDIA_VISIBLE_DEVICES=all`) in containerEnv. Do NOT put `--gpus all` in runArgs. Do NOT put `--device /dev/dri` in the committed config; VAAPI is enabled per-host via an uncommitted `.devcontainer/devcontainer.local.json` whose runArgs add `--device /dev/dri` and `--group-add <numeric host render GID>` (initialize.sh prints the exact line).

## Rationale

"optional" is the spec-defined graceful-degradation value ('a GPU is used when available, but is not required'); on capable Linux hosts the Dev Containers tooling auto-injects `--gpus all`, and elsewhere it is omitted so the same config still builds. Hardcoding `--gpus all` makes `devcontainer up` fail on every host without the NVIDIA runtime (all macOS, GPU-less Linux); an unconditional `--device /dev/dri` likewise fails container creation when the device is absent. The `video` capability is mandatory or NVENC/NVDEC silently fail while CUDA works. Render-group access needs the NUMERIC host GID because the group name's GID differs across hosts.

## Alternatives considered

(a) `runArgs:["--gpus","all"]` — rejected: breaks macOS/GPU-less startup. (b) `gpu: true` — rejected: makes a GPU REQUIRED, fails cross-platform. (c) docker-compose with deploy.resources reservations — rejected: spec issue reports a malformed GPU override for compose-based dev containers, and Multiview needs no sidecar services. (d) Unconditional `--device /dev/dri` committed — rejected: fails where the device is absent.

## Consequences

Same config works on Linux NVIDIA, Linux Intel/AMD, macOS, and CI. NVIDIA needs the host Container Toolkit configured (documented); 'optional' silently no-ops without it, so docs require verifying with nvidia-smi in-container. VAAPI requires a one-time per-host uncommitted override file. Auto-injection is reliable on Linux but reportedly inconsistent on Windows+WSL2 (out of current scope; would need an isolated opt-in override).
