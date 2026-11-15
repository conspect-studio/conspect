#!/usr/bin/env bash
#
# deploy/libsrt-build.sh — build the pinned libsrt (Haivision SRT, MPL-2.0)
# that our FFmpeg and the multiview-srt-sys listener leaf use on Linux.
#
# Why: Debian trixie ships libsrt 1.5.4-1+deb13u1, which back-ports only the two
# 1.5.6 advisories (GHSA-6xg9-784j-24rm, GHSA-4mc6-qmpp-g7gw). The listener
# pre-authentication heap overflow GHSA-m96h-3rc9-xx6h is fixed only in 1.5.7,
# and multiview-srt-sys refuses to open a listener on anything older. So the
# images build 1.5.7 from the pinned upstream tarball into the FFmpeg prefix,
# FFmpeg's libsrt protocol links it, and no Debian libsrt package ships.
#
# Usage:
#   libsrt-build.sh fetch <dir>      download + SHA-256-verify the tarball into <dir>
#   libsrt-build.sh build <prefix>   build + install into <prefix>/{lib,include}
#                                    (uses LIBSRT_TARBALL when set, else fetches)
#   libsrt-build.sh assert <prefix>  check the installed library is the pin
#
# Inputs (environment): LIBSRT_VERSION (default 1.5.7), LIBSRT_SHA256 (default
# the pin below; a mismatch aborts), LIBSRT_TARBALL (pre-fetched tarball path).
# Build: cmake, a C++ toolchain, pkg-config and OpenSSL headers (libssl-dev).
# Encryption uses OpenSSL (Apache-2.0), the same flavour Debian's
# libsrt1.5-openssl used, so the runtime keeps needing only libssl3.
set -euo pipefail

LIBSRT_VERSION="${LIBSRT_VERSION:-1.5.7}"
# SHA-256 of https://github.com/Haivision/srt/archive/refs/tags/v1.5.7.tar.gz,
# verified at author time (2026-11-15) and identical to the tarball mda001's
# /opt/libsrt-1.5.7 was built from.
LIBSRT_SHA256="${LIBSRT_SHA256:-017cd1e437ef2073a4dd10ddf7b55e86bc3d6ebac0393d13bd22f6a57055d32b}"
LIBSRT_URL="https://github.com/Haivision/srt/archive/refs/tags/v${LIBSRT_VERSION}.tar.gz"

cpu_count() { getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4; }

fetch() {
  local dir="${1:?usage: libsrt-build.sh fetch <dir>}" out
  mkdir -p "${dir}"
  out="${dir}/srt-${LIBSRT_VERSION}.tar.gz"
  curl -fsSL --retry 5 --retry-all-errors --proto '=https' -o "${out}.part" "${LIBSRT_URL}"
  if ! printf '%s  %s\n' "${LIBSRT_SHA256}" "${out}.part" | sha256sum -c --quiet -; then
    echo "libsrt-build: SHA-256 mismatch for ${LIBSRT_URL} — refusing unverified source" >&2
    rm -f "${out}.part"
    return 1
  fi
  mv "${out}.part" "${out}"
  echo "libsrt-build: fetched + verified ${out}"
}

assert_installed() {
  local prefix="${1:?usage: libsrt-build.sh assert <prefix>}" got
  test -e "${prefix}/lib/libsrt.so.${LIBSRT_VERSION}" \
    || { echo "libsrt-build: ${prefix}/lib/libsrt.so.${LIBSRT_VERSION} missing" >&2; return 1; }
  got="$(PKG_CONFIG_PATH="${prefix}/lib/pkgconfig" PKG_CONFIG_LIBDIR="${prefix}/lib/pkgconfig" pkg-config --modversion srt)"
  if [ "${got}" != "${LIBSRT_VERSION}" ]; then
    echo "libsrt-build: pkg-config srt is ${got}, want ${LIBSRT_VERSION}" >&2
    return 1
  fi
  echo "libsrt-build: libsrt ${got} installed in ${prefix}/lib"
}

build() {
  local prefix="${1:?usage: libsrt-build.sh build <prefix>}" work tarball
  work="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: ${work} is fixed for this run
  trap "rm -rf -- '${work}'" EXIT
  tarball="${LIBSRT_TARBALL:-}"
  if [ -z "${tarball}" ]; then
    fetch "${work}"
    tarball="${work}/srt-${LIBSRT_VERSION}.tar.gz"
  elif ! printf '%s  %s\n' "${LIBSRT_SHA256}" "${tarball}" | sha256sum -c --quiet -; then
    echo "libsrt-build: SHA-256 mismatch for ${tarball}" >&2
    return 1
  fi
  tar -xzf "${tarball}" -C "${work}"
  cmake -S "${work}/srt-${LIBSRT_VERSION}" -B "${work}/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="${prefix}" \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DENABLE_APPS=OFF -DENABLE_STATIC=OFF -DENABLE_SHARED=ON \
    -DUSE_ENCLIB=openssl
  cmake --build "${work}/build" -j"$(cpu_count)"
  cmake --install "${work}/build"
  # ENABLE_APPS=OFF still installs the srt-ffplay helper script; the prefix
  # ships libraries only.
  rm -f -- "${prefix}/bin/srt-ffplay"
  install -d "${prefix}/share/licenses/libsrt"
  install -m 0644 "${work}/srt-${LIBSRT_VERSION}/LICENSE" "${prefix}/share/licenses/libsrt/LICENSE"
  assert_installed "${prefix}"
}

case "${1:-}" in
  fetch) fetch "${2:-}" ;;
  build) build "${2:-}" ;;
  assert) assert_installed "${2:-}" ;;
  *) echo "usage: libsrt-build.sh fetch <dir> | build <prefix> | assert <prefix>" >&2; exit 2 ;;
esac
