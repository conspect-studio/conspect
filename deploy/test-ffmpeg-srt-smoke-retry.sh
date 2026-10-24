#!/usr/bin/env bash
# Exercise startup delay and failed-caller recovery against a real SRT-capable FFmpeg.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
real_prefix="${FF_PREFIX:-/opt/multiview-ffmpeg}"
real_ffmpeg="${real_prefix}/bin/ffmpeg"
mkdir -p "${script_dir}/../target"
work="$(mktemp -d "${script_dir}/../target/ffmpeg-srt-retry.XXXXXXXX")"
if [[ "$(realpath -- "$work")" != "$(realpath -- "${script_dir}/../target")/ffmpeg-srt-retry."* ]]; then
  echo 'unexpected SRT test directory' >&2
  exit 1
fi
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/prefix/bin"
ln -s "${real_prefix}/lib" "$work/prefix/lib"
cat > "$work/prefix/bin/ffmpeg" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
for arg in "$@"; do
  case "$arg" in
    *mode=listener*)
      printf '%s\n' "$arg" >> "$SMOKE_TRACE/listeners"
      # Delaying the listener's bind makes the first caller depend on readiness.
      if [[ "$SMOKE_MODE" == slow ]]; then sleep 5; fi
      ;;
    *mode=caller*)
      printf '%s\n' "$arg" >> "$SMOKE_TRACE/callers"
      if [[ ! -e "$SMOKE_TRACE/first-caller" ]]; then
        touch "$SMOKE_TRACE/first-caller"
        case "$SMOKE_MODE" in
          fail) exit 1 ;;
        esac
      fi
      ;;
  esac
done
exec "$SMOKE_REAL_FFMPEG" "$@"
SHIM
chmod +x "$work/prefix/bin/ffmpeg"
for mode in slow fail; do
  mkdir "$work/$mode"
  SMOKE_TRACE="$work/$mode" SMOKE_MODE="$mode" SMOKE_REAL_FFMPEG="$real_ffmpeg" \
    FF_PREFIX="$work/prefix" bash "$script_dir/ffmpeg-build.sh" --smoke-srt
  if [[ "$mode" == slow ]]; then
    if [[ "$(wc -l < "$work/$mode/callers")" -ne 1 ]]; then
      echo "slow startup must succeed on its first caller attempt (callers: $(wc -l < "$work/$mode/callers"))" >&2
      exit 1
    fi
  else
    if [[ "$(wc -l < "$work/$mode/listeners")" -lt 2 ]] ||
       [[ "$(sort -u "$work/$mode/listeners" | wc -l)" -lt 2 ]]; then
      echo 'failed caller must retry with a fresh listener and port' >&2
      exit 1
    fi
  fi
done
