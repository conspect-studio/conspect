#!/usr/bin/env bash
# deploy/ffmpeg-fetch-source.sh — fetch and verify the pinned FFmpeg source.
#
# Split out of ffmpeg-build.sh so the Docker image build can run it in its own
# stage. That stage depends only on the version/pin build args and this file,
# so edits to the build/configure script reuse the cached, verified source
# instead of downloading FFmpeg again on every change.
#
# Usage: FFMPEG_SHA256=... ffmpeg-fetch-source.sh <dest-dir>
#   Leaves the verified source tree at <dest-dir>/ffmpeg-${FFMPEG_VERSION}.
set -euo pipefail

FFMPEG_VERSION="${FFMPEG_VERSION:-9.0.2}"
FFMPEG_GPG_KEY="${FFMPEG_GPG_KEY:-FCF986EA15E6E293A5644F10B4322F04D67658D8}"
DEST="${1:?usage: ffmpeg-fetch-source.sh <dest-dir>}"

if [ -z "${FFMPEG_SHA256:-}" ] || [ "${FFMPEG_SHA256}" = "TODO-pin" ]; then
  echo "ffmpeg-fetch: FFMPEG_SHA256 is not pinned — refusing to fetch unverified source." >&2
  exit 2
fi

mkdir -p "${DEST}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
cd "${WORK}"

# -----------------------------------------------------------------------------
# 1. Fetch + verify the pinned source tarball (GPG signature AND SHA-256).
# -----------------------------------------------------------------------------
TARBALL="ffmpeg-${FFMPEG_VERSION}.tar.xz"
BASE_URL="https://ffmpeg.org/releases"
SRC="${WORK}/ffmpeg-${FFMPEG_VERSION}"

# ffmpeg.org is reachable from most networks but has intermittently reset or
# timed out connections from CI runners (curl 35/28). Retry every error class
# with a bounded connect timeout before giving up on the release tarball.
fetch() {
  curl -fsSL --retry 6 --retry-delay 10 --retry-all-errors \
       --connect-timeout 30 --max-time 600 -o "$1" "$2"
}

# Primary source: the signed release tarball from ffmpeg.org, verified by
# SHA-256 pin AND the detached GPG signature against the pinned release key.
fetch_release_file() {
  local status=0
  fetch "$1" "$2" || status=$?
  case "${status}" in
    0) return 0 ;;
    # DNS, connect, timeout, TLS handshake and interrupted transport only.
    # HTTP errors (22), certificate failures and local I/O errors fail closed.
    5|6|7|28|35|52|55|56) return 1 ;;
    *)
      echo "ffmpeg-fetch: release download failed (curl ${status}); refusing fallback." >&2
      exit 3
      ;;
  esac
}

fetch_release_tarball() {
  fetch_release_file "${TARBALL}"     "${BASE_URL}/${TARBALL}"     || return 1
  fetch_release_file "${TARBALL}.asc" "${BASE_URL}/${TARBALL}.asc" || return 1

  # SHA-256 (defense-in-depth pin). A mismatch is fatal, never a fallback.
  echo "${FFMPEG_SHA256}  ${TARBALL}" | sha256sum -c - || exit 4

  export GNUPGHOME="${WORK}/gnupg"
  mkdir -p "${GNUPGHOME}"
  chmod 700 "${GNUPGHOME}"
  gpg_recv_ok=0
  for ks in hkps://keyserver.ubuntu.com hkps://keys.openpgp.org hkps://pgp.mit.edu; do
    if gpg --batch --keyserver "${ks}" --recv-keys "${FFMPEG_GPG_KEY}"; then
      gpg_recv_ok=1
      break
    fi
    echo "ffmpeg-build: keyserver ${ks} failed, trying next" >&2
  done
  if [ "${gpg_recv_ok}" -ne 1 ]; then
    echo "ffmpeg-build: could not retrieve FFmpeg signing key ${FFMPEG_GPG_KEY} from any keyserver." >&2
    exit 3
  fi
  gpg --batch --verify "${TARBALL}.asc" "${TARBALL}" || exit 3
  tar -xJf "${TARBALL}" || exit 4
}

# Fallback source (only when ffmpeg.org is unreachable): FFmpeg's own upstream
# git repository, mirrored by the FFmpeg project on GitHub, checked out at the
# release tag and content-pinned by its commit id. Git commit ids hash the full
# tree, so a matching FFMPEG_GIT_COMMIT is the same source the release tarball
# was cut from (the tarball adds only the generated VERSION file, recreated
# below). No third-party or self-hosted mirror is involved.
fetch_upstream_git() {
  [ -n "${FFMPEG_GIT_COMMIT:-}" ] || return 1
  rm -rf "${SRC}"
  git -c advice.detachedHead=false clone --quiet --depth 1 \
      --branch "n${FFMPEG_VERSION}" "${FFMPEG_GIT_URL}" "${SRC}" || return 1
  got="$(git -C "${SRC}" rev-parse HEAD)"
  if [ "${got}" != "${FFMPEG_GIT_COMMIT}" ]; then
    echo "ffmpeg-build: upstream tag n${FFMPEG_VERSION} is ${got}, pinned ${FFMPEG_GIT_COMMIT} — refusing." >&2
    exit 4
  fi
  rm -rf "${SRC}/.git"
  printf '%s\n' "${FFMPEG_VERSION}" > "${SRC}/VERSION"
}

FFMPEG_GIT_URL="${FFMPEG_GIT_URL:-https://github.com/FFmpeg/FFmpeg.git}"
if ! fetch_release_tarball; then
  echo "ffmpeg-build: ffmpeg.org unreachable; falling back to upstream git ${FFMPEG_GIT_URL} @ ${FFMPEG_GIT_COMMIT:-<unpinned>}" >&2
  if ! fetch_upstream_git; then
    echo "ffmpeg-build: could not fetch FFmpeg ${FFMPEG_VERSION} from ffmpeg.org or pinned upstream git." >&2
    exit 5
  fi
fi

rm -rf "${DEST:?}/ffmpeg-${FFMPEG_VERSION}"
mv "${SRC}" "${DEST}/ffmpeg-${FFMPEG_VERSION}"
echo "ffmpeg-fetch: verified FFmpeg ${FFMPEG_VERSION} source at ${DEST}/ffmpeg-${FFMPEG_VERSION}"
