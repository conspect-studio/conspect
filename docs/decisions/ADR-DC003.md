# ADR-DC003: Base on mcr.microsoft.com/devcontainers/rust:1-trixie + thin Dockerfile + official Features

- **Status:** Proposed
- **Area:** Dev Container
- **Date:** 2026-06-02
- **Source brief:** [devcontainer-design.md](../research/devcontainer-design.md)

## Decision

Use the Microsoft Rust dev container pinned to the Debian TRIXIE variant (`rust:1-trixie`), layered with a thin Dockerfile that apt-installs (single layer) the FFmpeg/libav*-dev stack, clang/llvm/pkg-config/nasm/yasm, libass, and VAAPI userspace. Add Node and GitHub CLI via official ghcr.io Features. Cache cargo registry/git and target/ in named volumes, chowned to vscode in post-create. Run as non-root vscode with updateRemoteUserUID.

## Rationale

Trixie ships FFmpeg 7.1.x; Debian bookworm's 5.1 is too old for the FFmpeg FFI and would force a from-source FFmpeg build. The MS base image provides a maintained, patched Rust toolchain and a non-root vscode user, keeping the Dockerfile to just native deps no Feature provides. clang/pkg-config are mandatory for FFmpeg-sys bindgen/discovery. Named volumes avoid host<->VM filesystem penalties (important on macOS Docker Desktop) and survive rebuilds; empty volumes mount as root so they must be chowned. The trixie tag is confirmed published on the MS Artifact Registry.

## Alternatives considered

(a) bookworm variant — rejected: FFmpeg 5.1 too old. (b) Fully custom Debian/Ubuntu Dockerfile — rejected: re-implements rustup/user/Node the base image standardizes. (c) Build FFmpeg from source — rejected: unnecessary on trixie. (d) Bind-mount caches or bake them into layers — rejected: slow IO / fat image.

## Consequences

Multi-arch (amd64 + arm64) so it builds on Linux GPU hosts and Apple Silicon. Fast incremental builds via volume caches. Tied to Debian trixie's FFmpeg major (7.x) — the FFmpeg-binding crate version must match that major; an FFmpeg 8 bump would require re-pinning. The image carries no NVIDIA driver/toolkit (host prerequisite, documented).
