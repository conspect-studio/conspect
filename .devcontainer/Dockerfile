# syntax=docker/dockerfile:1
#
# Multiview dev container image.
#
# Base: Microsoft Rust dev container on Debian TRIXIE. Trixie is load-bearing:
#   * bookworm ships FFmpeg 5.1 (libav* too old for modern rsmpeg/FFI: needs 6.x/7.x)
#   * trixie  ships FFmpeg 7.1.x -> apt libav* satisfy the build with no source build.
# Provides: rustup/cargo, non-root `vscode` user (uid 1000), git, common-utils.
# Multi-arch: builds on amd64 (Linux GPU hosts) AND arm64 (Apple Silicon Linux container).
#
# What this image does NOT contain (by design):
#   * NVIDIA kernel driver / Container Toolkit -> HOST prerequisites; the toolkit
#     injects host driver libs at runtime. Image carries only userspace + env.
FROM mcr.microsoft.com/devcontainers/rust:1-trixie

# Avoid interactive apt during build.
ENV DEBIAN_FRONTEND=noninteractive

# -----------------------------------------------------------------------------
# Native build deps + FFmpeg/libav* dev stack + VAAPI userspace, in a single
# apt layer. clang/libclang are mandatory for FFI bindgen; pkg-config is how the
# FFmpeg-sys crates locate the system libraries.
# -----------------------------------------------------------------------------
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      # toolchain / build
      build-essential clang llvm-dev libclang-dev pkg-config nasm yasm \
      ca-certificates curl gnupg git-lfs \
      # developer CLIs (multi-arch via apt): ripgrep, fd (fdfind), bat (batcat), jq
      ripgrep fd-find bat jq \
      # FFmpeg / libav* dev (full set used across decode/encode/filter/scale)
      libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev \
      libavdevice-dev libswscale-dev libswresample-dev libpostproc-dev \
      libass-dev \
      ffmpeg \
      # VAAPI (Intel/AMD) userspace + verification + arch-portable runtime drivers
      libva-dev vainfo va-driver-all mesa-va-drivers \
 # The Intel media driver is x86_64-only (no arm64 package) AND lives in Debian's
 # `non-free` component, which the trixie base image does NOT enable (Components:
 # main only) — so a plain install fails with "has no installation candidate"
 # (apt exit 100). Enable non-free and refresh the index before pulling it, and
 # only on amd64 where Intel VAAPI hardware can exist (installing it unconditionally
 # also breaks `docker buildx build` on Apple Silicon / arm64).
 && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      sed -i -E 's/^Components:.*/Components: main contrib non-free non-free-firmware/' \
        /etc/apt/sources.list.d/debian.sources \
      && apt-get update \
      && apt-get install -y --no-install-recommends intel-media-va-driver-non-free; \
    fi \
 && rm -rf /var/lib/apt/lists/*

# -----------------------------------------------------------------------------
# 1Password CLI (op). Installed at BUILD time (no secret involved); the service
# account token is supplied only at RUNTIME via --env-file (.env). Architecture
# is auto-detected with `dpkg --print-architecture`, so this works on amd64 and
# arm64. Includes the debsig policy/keyring so package signatures are verified.
# Service accounts require op >= 2.18.0 (current releases are well past that).
# -----------------------------------------------------------------------------
RUN curl -sS https://downloads.1password.com/linux/keys/1password.asc \
      | gpg --dearmor --output /usr/share/keyrings/1password-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/1password-archive-keyring.gpg] https://downloads.1password.com/linux/debian/$(dpkg --print-architecture) stable main" \
      > /etc/apt/sources.list.d/1password.list \
 && mkdir -p /etc/debsig/policies/AC2D62742012EA22/ \
 && curl -sS https://downloads.1password.com/linux/debian/debsig/1password.pol \
      > /etc/debsig/policies/AC2D62742012EA22/1password.pol \
 && mkdir -p /usr/share/debsig/keyrings/AC2D62742012EA22 \
 && curl -sS https://downloads.1password.com/linux/keys/1password.asc \
      | gpg --dearmor --output /usr/share/debsig/keyrings/AC2D62742012EA22/debsig.gpg \
 && apt-get update \
 && apt-get install -y --no-install-recommends 1password-cli \
 && rm -rf /var/lib/apt/lists/* \
 && op --version

# Help the FFmpeg-sys crates find pkg-config metadata (system FFmpeg).
ENV PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/lib/pkgconfig

# CARGO_HOME is /usr/local/cargo on the MS image; ensure the registry/git dirs
# exist so the named-volume mounts have a target (chown happens in post-create).
RUN mkdir -p /usr/local/cargo/registry /usr/local/cargo/git

# Debian renames some tools (fd-find -> fdfind, bat -> batcat). Provide the
# conventional names so docs/scripts/muscle-memory work on amd64 and arm64.
RUN ln -sf "$(command -v fdfind)" /usr/local/bin/fd \
 && ln -sf "$(command -v batcat)" /usr/local/bin/bat

# -----------------------------------------------------------------------------
# OPTIONAL, OPERATOR-LICENSED NDI SDK (OFF by default; ADR-0008 / ADR-T010).
#
# Native NDI (the off-by-default `ndi` feature) runtime-LOADS libndi via
# NDIlib_v6_load at run time and is NEVER build-linked or vendored into the repo
# (ADR-0008). Two things still come from the NDI SDK (NDI is a NewTek/Vizrt
# product — Vizrt NDI AB; NOT Audinate, which makes Dante): the NDIlib v6 HEADER
# (to declare the FFI ABI in `multiview-ndi-sys` as original binding work) and
# the libndi RUNTIME (for the gated loopback test). Downloading the SDK REQUIRES
# ACCEPTING THE NDI SDK (NewTek/Vizrt) LICENCE, so this stage is gated on an
# explicit operator opt-in and installs into the IMAGE ONLY (never the git repo).
#
# Default OFF: with NDI_SDK_ACCEPT_LICENSE unset this is a no-op and the image
# stays proprietary-free / LGPL-clean. To enable, an operator who attests they
# hold an NDI SDK licence rebuilds with:
#   --build-arg NDI_SDK_ACCEPT_LICENSE=1 \
#   --build-arg NDI_SDK_URL=<URL of their NDI SDK for Linux tarball>
# (Vizrt gates the SDK download behind a registration form, so there is no
# stable public URL to hardcode — the operator supplies it. Self-extracting
# `.sh` SDK installers need their own EULA-accept flags; the tarball path below
# is the clean case and may need tuning to the exact SDK package layout.)
USER root
ARG NDI_SDK_ACCEPT_LICENSE=
ARG NDI_SDK_URL=
ENV NDI_RUNTIME_DIR_V6=/usr/local/lib
RUN if [ -n "${NDI_SDK_ACCEPT_LICENSE}" ]; then \
      set -eu; \
      if [ -z "${NDI_SDK_URL}" ]; then \
        echo "NDI_SDK_ACCEPT_LICENSE set but NDI_SDK_URL empty — supply the SDK URL" >&2; exit 1; \
      fi; \
      echo "Installing operator-licensed NDI SDK (image only, never the repo)"; \
      mkdir -p /tmp/ndi /opt/ndi/include; \
      curl -fSL "${NDI_SDK_URL}" -o /tmp/ndi/ndi-sdk.tar.gz; \
      tar -xzf /tmp/ndi/ndi-sdk.tar.gz -C /tmp/ndi; \
      inc="$(find /tmp/ndi -type f -name 'Processing.NDI.Lib.h' -printf '%h\n' | head -1)"; \
      [ -n "${inc}" ] || { echo "NDIlib headers not found in the SDK tarball" >&2; exit 1; }; \
      cp -a "${inc}/." /opt/ndi/include/; \
      find /tmp/ndi -type f -name 'libndi.so*' -exec cp -a {} /usr/local/lib/ \; ; \
      ldconfig; \
      rm -rf /tmp/ndi; \
      echo "NDI SDK installed: headers in /opt/ndi/include, libndi in /usr/local/lib"; \
    else \
      echo "NDI SDK NOT installed (NDI_SDK_ACCEPT_LICENSE unset) — open build, native NDI inert"; \
    fi
# Header search prefix for the (off-by-default) `ndi` FFI binding; harmless when
# the SDK is absent.
ENV NDI_SDK_INCLUDE_DIR=/opt/ndi/include

USER vscode
