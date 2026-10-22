#!/usr/bin/env bash
# Resolve which pinned FFmpeg base image (ADR-0031 §5) a workflow should consume.
#
# Usage: scripts/resolve-ffmpeg-image.sh <owner> <tag>
#   e.g. scripts/resolve-ffmpeg-image.sh conspect-studio 8.1.1-lgpl
#
# Prefers this repository owner's own published base
# (ghcr.io/<owner>/multiview-ffmpeg:<tag>, written by ffmpeg-base.yml). Until
# that tag exists (before the owner's first ffmpeg-base publish), falls back to
# the public upstream base ghcr.io/aperim/multiview-ffmpeg:<tag> — the same
# default the deploy/Dockerfile* FFMPEG_IMAGE build-args carry. Prints the
# chosen image reference on stdout; the reason goes to stderr.
set -euo pipefail

owner="${1:?usage: resolve-ffmpeg-image.sh <owner> <tag>}"
tag="${2:?usage: resolve-ffmpeg-image.sh <owner> <tag>}"
upstream="ghcr.io/aperim/multiview-ffmpeg:${tag}"
# GHCR repository paths are lowercase-only.
own="ghcr.io/$(printf '%s' "${owner}" | tr '[:upper:]' '[:lower:]')/multiview-ffmpeg:${tag}"

if [ "${own}" != "${upstream}" ] && docker manifest inspect "${own}" >/dev/null 2>&1; then
  echo "using owner FFmpeg base ${own}" >&2
  printf '%s\n' "${own}"
else
  echo "owner FFmpeg base ${own} not published (or is upstream); using ${upstream}" >&2
  printf '%s\n' "${upstream}"
fi
