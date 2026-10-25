#!/usr/bin/env bash
# Exercise delayed UDP bind and failed-caller recovery without SRT timing races.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "${script_dir}/../target"
work="$(mktemp -d "${script_dir}/../target/ffmpeg-srt-retry.XXXXXXXX")"
if [[ "$(realpath -- "$work")" != "$(realpath -- "${script_dir}/../target")/ffmpeg-srt-retry."* ]]; then
  echo 'unexpected SRT test directory' >&2
  exit 1
fi
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/prefix/bin" "$work/prefix/lib"
cat > "$work/prefix/bin/ffmpeg" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
mode='' port='' host='' input=''
for arg in "$@"; do
  case "$arg" in
    *mode=listener*|*mode=caller*)
      if [[ "$arg" =~ ^srt://(\[::1\]|127\.0\.0\.1):([0-9]+)\?mode=(listener|caller) ]]; then
        host="${BASH_REMATCH[1]}"
        port="${BASH_REMATCH[2]}"
        mode="${BASH_REMATCH[3]}"
      else
        echo "unexpected SRT URL: $arg" >&2
        exit 1
      fi
      ;;
    */in.bin) input="$arg" ;;
  esac
done
if [[ -z "$mode" ]]; then echo 'missing SRT mode' >&2; exit 1; fi
port_hex="$(printf '%04X' "$port")"
if [[ "$host" == '[::1]' ]]; then udp_table=/proc/net/udp6; else udp_table=/proc/net/udp; fi
if [[ "$mode" == listener ]]; then
  printf '%s\n' "$port" >> "$SMOKE_TRACE/listeners"
  # Bind after the delay, then keep the socket open until the caller completes.
  if [[ "$SMOKE_MODE" == slow ]]; then sleep 5; fi
  exec python3 - "$host" "$port" "$SMOKE_TRACE/done-$port" <<'PY'
import os
import socket
import sys
import time

host, port, done = sys.argv[1:]
family = socket.AF_INET6 if host == '[::1]' else socket.AF_INET
address = '::1' if family == socket.AF_INET6 else host
with socket.socket(family, socket.SOCK_DGRAM) as sock:
    sock.bind((address, int(port)))
    while not os.path.exists(done):
        time.sleep(0.05)
PY
fi
printf '%s\n' "$port" >> "$SMOKE_TRACE/callers"
if awk -v p=":$port_hex" '$2 ~ (p "$") && $4 == "07" { found=1; exit } END { exit !found }' "$udp_table"; then
  printf '%s\n' "$port" >> "$SMOKE_TRACE/bound-callers"
else
  echo 'caller ran before listener UDP bind' >&2
  exit 1
fi
if [[ "$SMOKE_MODE" == fail && ! -e "$SMOKE_TRACE/first-caller" ]]; then
  touch "$SMOKE_TRACE/first-caller"
  exit 1
fi
if [[ -z "$input" ]]; then echo 'missing caller input' >&2; exit 1; fi
cp -- "$input" "${input%/in.bin}/out.bin"
touch "$SMOKE_TRACE/done-$port"
SHIM
chmod +x "$work/prefix/bin/ffmpeg"
for mode in slow fail; do
  mkdir "$work/$mode"
  SMOKE_TRACE="$work/$mode" SMOKE_MODE="$mode" \
    FF_PREFIX="$work/prefix" bash "$script_dir/ffmpeg-build.sh" --smoke-srt
  if [[ "$mode" == slow ]]; then
    if [[ "$(wc -l < "$work/$mode/callers")" -ne 1 ]] ||
       [[ "$(wc -l < "$work/$mode/bound-callers")" -ne 1 ]]; then
      echo 'slow startup must bind before its sole caller attempt' >&2
      exit 1
    fi
  else
    if [[ "$(wc -l < "$work/$mode/callers")" -ne 2 ]] ||
       [[ "$(wc -l < "$work/$mode/bound-callers")" -ne 2 ]] ||
       [[ "$(wc -l < "$work/$mode/listeners")" -ne 2 ]] ||
       [[ "$(sort -u "$work/$mode/listeners" | wc -l)" -ne 2 ]]; then
      echo 'failed caller must retry with a freshly bound listener and port' >&2
      exit 1
    fi
  fi
done
