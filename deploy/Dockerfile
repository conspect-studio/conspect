# syntax=docker/dockerfile:1
#
# Multiview runtime image (LGPL-clean default build).
#
# Multi-stage:
#   * ffmpeg   — pulled from the SEPARATELY-PUBLISHED FFmpeg base image
#                (ghcr.io/aperim/multiview-ffmpeg:8.1.1-lgpl, ADR-0031 §5). It
#                ships OUR OWN pinned, GPG+SHA-256-verified FFmpeg 8.1.1 under the
#                ADR-0031 LGPL-clean allowlist (VAAPI + Intel QSV via libvpl on
#                amd64; NO GPL/nonfree) at /opt/multiview-ffmpeg (libavcodec
#                .so.62). The ~10-min source build is amortised to a multi-second
#                COPY — see deploy/Dockerfile.ffmpeg + .github/workflows/
#                ffmpeg-base.yml. Pin FFMPEG_IMAGE by @sha256: for a release.
#   * web      — Node 22, builds the React SPA to `web/dist` (npm ci + npm run
#                build). MUST run before the rust build: the `web` feature embeds
#                `web/dist` into the binary at compile time (rust_embed).
#   * chef     — Debian trixie + Rust + cargo-chef + OUR FFmpeg 8.1.1 headers
#                (COPY --from=ffmpeg) on PKG_CONFIG_PATH. Shared base for planner
#                + builder so rustc/pkg-config are identical across the recipe.
#   * planner  — `cargo chef prepare` → recipe.json (manifests + Cargo.lock only).
#   * builder  — `cargo chef cook` pre-builds the dependency graph as a CACHED
#                layer keyed on recipe.json, THEN copies the source + web/dist and
#                builds the `multiview` binary with `--features
#                ${CARGO_FEATURES}` (default ffmpeg,linux-vaapi,web,ntp — the `web`
#                feature pulls multiview-control/embed-web, embedding the SPA so
#                the web UI + REST/WS API + OpenAPI/Scalar docs serve from the
#                binary on the [control] listener). ffmpeg-sys-next's build.rs
#                probes OUR pkg-config, so the ffmpeg headers are COPYed in BEFORE
#                cook.
#   * runtime  — debian:trixie-slim with OUR FFmpeg 8.1.1 runtime sonames
#                (libavcodec.so.62, …) + VAAPI userspace (Intel/AMD), a non-root
#                user, and the binary.
#
# FFmpeg provenance (ADR-0031 FF-0/FF-0b): the base image builds FFmpeg 8.1.1 from
# the pinned ffmpeg.org tarball, verified by BOTH the FFmpeg release GPG key
# (FCF986EA15E6E293A5644F10B4322F04D67658D8) AND a pinned SHA-256, under our
# `--disable-everything` allowlist — replacing the opaque apt libav* (provenance
# + soname we did not own). The default build is genuinely LGPL-clean: we
# DELIBERATELY do NOT enable `gpl-codecs` (x264/x265) here. A separate `-gpl`
# image tag (built with `--build-arg
# FFMPEG_IMAGE=ghcr.io/aperim/multiview-ffmpeg:8.1.1-gpl --build-arg
# CARGO_FEATURES=ffmpeg,linux-vaapi,web,ntp,gpl-codecs`) is the opt-in GPL variant.
#
# 8.1.1 → libavcodec.so.62 / libavutil.so.60 / libavformat.so.62, matching
# `ffmpeg-sys-next 8.1.0` (build.rs `ffmpeg_8_1` → libavcodec 62), the version
# the workspace lockfile resolves — so the binary links .so.62.
#
# Base-image pinning: the FFmpeg base (FFMPEG_IMAGE) and the `rust:1-trixie` /
# `debian:trixie-slim` tags below are floating lines. For a reproducible release
# pin each by digest, e.g.
#   ARG FFMPEG_IMAGE=ghcr.io/aperim/multiview-ffmpeg:8.1.1-lgpl@sha256:<digest>
#   FROM rust:1-trixie@sha256:<digest> AS chef
#   FROM debian:trixie-slim@sha256:<digest> AS runtime
# (Renovate adds + bumps these digests; CI provenance/SBOM record them anyway.)

# FFmpeg base image — the app build COPYs /opt/multiview-ffmpeg from here. Default
# is the published LGPL flavour so a plain `docker build` works; CI / the box
# override with `--build-arg FFMPEG_IMAGE=…@sha256:<digest>`.
ARG FFMPEG_IMAGE=ghcr.io/aperim/multiview-ffmpeg:8.1.1-lgpl

# -----------------------------------------------------------------------------
# Stage 0: ffmpeg — the published FFmpeg 8.1.1 base (VAAPI/QSV, LGPL-clean).
# Ships /opt/multiview-ffmpeg/{lib,include,lib/pkgconfig}. Built by
# deploy/Dockerfile.ffmpeg; here it is just a pulled, digest-pinnable layer.
# -----------------------------------------------------------------------------
FROM ${FFMPEG_IMAGE} AS ffmpeg

# -----------------------------------------------------------------------------
# Stage 1: web — build the React SPA to web/dist (Node 22, matches the
# dev-container). The `web` cargo feature embeds web/dist into the binary at
# compile time, so this stage MUST complete before the rust build. `npm ci`
# installs from the committed web/package-lock.json; `npm run build` runs
# `lingui compile && tsc --noEmit && vite build` and writes to web/dist.
# -----------------------------------------------------------------------------
FROM node:22-trixie-slim AS web

WORKDIR /web

# Install from the lockfile first (cache-friendly), then build the SPA.
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci
COPY web/ ./
RUN npm run build

# -----------------------------------------------------------------------------
# Stage 2a: chef — Rust toolchain + native build deps + OUR FFmpeg 8.1.1 headers
# + cargo-chef. Shared base for `planner` and `builder` so rustc + pkg-config are
# byte-identical across the recipe (a toolchain mismatch silently invalidates the
# cooked dependency layer). Links against OUR FFmpeg 8.1.1 (libavcodec.so.62).
# -----------------------------------------------------------------------------
FROM rust:1-trixie AS chef

ENV DEBIAN_FRONTEND=noninteractive

# Native build deps:
#   * pkg-config + clang/libclang  — FFI bindgen + locating our libav* via .pc.
#   * libva-dev                    — VAAPI (Intel/AMD) for the `linux-vaapi` path.
#   * libopus0/libsrt/gnutls/libdrm2 — OUR FFmpeg's transitive runtime libs: the
#     `builder` stage's `multiview --version` smoke test dynamically loads
#     libavcodec.so.62 which NEEDs them; without them the loader exits 127.
# The libav* themselves come from the published ffmpeg base (COPY --from=ffmpeg
# below) — NOT from apt (no libav*-dev here; that opaque apt source is exactly
# what we replaced).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config clang llvm-dev libclang-dev nasm \
      libva-dev \
      libopus0 libsrt1.5-openssl libgnutls30t64 libdrm2 \
 && rm -rf /var/lib/apt/lists/*

# Our pinned FFmpeg 8.1.1 (headers + .so + .pc) from the published base image.
# Point pkg-config + the loader at it so `ffmpeg-sys-next`'s build.rs discovers
# FFmpeg 8.1.1 (detects ffmpeg_8_1 → libavcodec 62) and the build links .so.62.
# This MUST precede `cargo chef cook`: build.rs probes pkg-config at dep-compile
# time, so the headers have to be present for the cooked layer to be valid.
COPY --from=ffmpeg /opt/multiview-ffmpeg /opt/multiview-ffmpeg
ENV PKG_CONFIG_PATH=/opt/multiview-ffmpeg/lib/pkgconfig \
    LD_LIBRARY_PATH=/opt/multiview-ffmpeg/lib

# cargo-chef turns the dependency graph into a cacheable layer keyed on
# recipe.json (manifests + Cargo.lock), which `type=gha`/registry cache persists
# across runs — unlike a bare `--mount=type=cache,target=target` (builder-local
# scratch that the cache backends do NOT export).
RUN cargo install cargo-chef --locked

WORKDIR /build

# -----------------------------------------------------------------------------
# Stage 2b: planner — distil the whole workspace down to recipe.json (the
# manifests + Cargo.lock). Only THIS file feeds the cook layer, so app-source
# edits that don't touch Cargo.{toml,lock} reuse the cooked deps.
# -----------------------------------------------------------------------------
FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# -----------------------------------------------------------------------------
# Stage 2c: builder — cook the deps (cached), then build the binary.
# -----------------------------------------------------------------------------
FROM chef AS builder

# Build-time feature set. Default is the LGPL-clean `ffmpeg,linux-vaapi,web,ntp`
# (`web` embeds the SPA + serves the control API; `ntp` measures the kernel
# NTP/chrony discipline for the outbound presentation epoch instead of assuming
# it — the linux-vaapi preset already implies it, named here so an override list
# keeps it deliberately; no GPL deps). Pass
# `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,web,ntp,gpl-codecs` (with
# `--build-arg FFMPEG_IMAGE=…:8.1.1-gpl`) to produce the GPL `-gpl` image variant.
# CARGO_FEATURES is threaded IDENTICALLY into `cook` and `build` — a mismatch
# pre-builds the wrong dependency closure and silently triggers a near-full
# rebuild while appearing to hit the cache.
ARG CARGO_FEATURES=ffmpeg,linux-vaapi,web,ntp

# Cook the dependency graph from recipe.json ONLY. This is the expensive,
# cacheable layer: it busts only when Cargo.{toml,lock} (→ recipe.json) change,
# not on first-party source edits. `--locked` enforces the committed Cargo.lock.
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --release --locked -p multiview-cli \
      --features "${CARGO_FEATURES}" --recipe-path recipe.json

# Now the source. With deps already cooked, this rebuilds only first-party crates.
COPY . .

# Bring in the SPA built by the `web` stage. The `web` feature's rust_embed
# folder is `web/dist` (resolved relative to multiview-control at compile time),
# so it must exist before `cargo build`. This COPY overwrites any web/dist that
# rode in with `COPY . .`, guaranteeing a fresh, in-image build.
COPY --from=web /web/dist ./web/dist

# Build the release binary. The same cargo registry/git mounts plus the target
# mount accelerate the intra-build step; the cross-run dep cache is carried by
# the cooked layer above. `--locked` enforces the committed Cargo.lock.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked -p multiview-cli --features "${CARGO_FEATURES}" \
 && cp /build/target/release/multiview /usr/local/bin/multiview \
 && /usr/local/bin/multiview --version

# -----------------------------------------------------------------------------
# Stage 3: runtime — debian:trixie-slim + OUR FFmpeg 8.1.1 sonames + VAAPI
# -----------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps for OUR FFmpeg 8.1.1 shared libs + VAAPI userspace + Intel/AMD
# drivers. We build FFmpeg --enable-shared against libopus/libsrt/gnutls/libdrm
# (+ libvpl on amd64), so the runtime needs those shared libs present (the loader
# resolves them when the binary pulls in libavcodec.so.62 et al). libva2 +
# mesa-va-drivers provide VAAPI; intel-media-va-driver is x86_64-only (no arm64
# package), installed only on amd64; libvpl2 (oneVPL runtime) likewise amd64-only.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      libopus0 libsrt1.5-openssl libgnutls30t64 libdrm2 \
      libva2 mesa-va-drivers \
 && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      apt-get install -y --no-install-recommends intel-media-va-driver libvpl2; \
    fi \
 && rm -rf /var/lib/apt/lists/*

# Our FFmpeg 8.1.1 shared libraries (libavcodec.so.62, libavutil.so.60,
# libavformat.so.62, libavfilter.so.11, libswscale.so.9, libswresample.so.6, and
# their symlinks). Install into /usr/local/lib and ldconfig so the binary finds
# them with NO LD_LIBRARY_PATH override. Assert the 8.1.1 soname (.so.62, NOT
# .61) so a version/layout drift fails the BUILD, not the deploy.
COPY --from=ffmpeg /opt/multiview-ffmpeg/lib /usr/local/lib
RUN ldconfig \
 && test -e /usr/local/lib/libavcodec.so.62 \
 && test -e /usr/local/lib/libavutil.so.60

# Non-root runtime user (fixed uid/gid 10001). The render group is added at
# `docker run` time (e.g. compose `group_add`) for VAAPI /dev/dri access.
RUN groupadd --gid 10001 multiview \
 && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin multiview \
 && mkdir -p /etc/multiview /var/lib/multiview/hls \
 && chown -R 10001:10001 /etc/multiview /var/lib/multiview

COPY --from=builder /usr/local/bin/multiview /usr/local/bin/multiview

USER 10001:10001
WORKDIR /var/lib/multiview

# The control/serving HTTP surface: web UI at `/`, REST under `/api/v1`,
# WebSocket at `/api/v1/ws`, SSE at `/api/v1/events`, OpenAPI at
# `/api/v1/openapi.json`, and Scalar docs at `/docs`. `multiview run` binds this
# when the config has a [control] section (the example config sets
# `listen = "[::]:8080"` — IPv6-first, dual-stacks to IPv4-mapped clients on
# Linux). The SPA serves because this image is built with the
# `web` feature (see the builder stage); set MULTIVIEW_CONTROL_TOKEN to fix the
# admin bearer secret (`admin.<secret>`), else one is generated + logged once.
EXPOSE 8080

# `multiview` is config-driven: `run <CONFIG>` (TOML). Override CMD to pass a
# config path; the default points at the in-image example config volume mount.
ENTRYPOINT ["/usr/local/bin/multiview"]
CMD ["run", "/etc/multiview/multiview.toml"]
