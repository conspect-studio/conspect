#!/usr/bin/env bash
# Run the same image-build smoke check against a locally installed FFmpeg.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! grep -Eq '^RUN FF_PREFIX=/opt/multiview-ffmpeg /usr/local/bin/ffmpeg-build\.sh --smoke-srt$' "${script_dir}/Dockerfile.ffmpeg"; then
  echo 'Dockerfile.ffmpeg must run the SRT loopback smoke check' >&2
  exit 1
fi
FF_PREFIX="${FF_PREFIX:-/usr}" "${script_dir}/ffmpeg-build.sh" --smoke-srt
