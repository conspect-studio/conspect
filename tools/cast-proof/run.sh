#!/bin/bash
# The cast acceptance run against the REAL engine (see README.md):
#   ./run.sh [--engine <multiview binary>] [--validator <cast_validate binary>] [--out <dir>] [--size WxH]
#            [--cores N] [--seconds 60] [--serial | --parallel]
#   ./run.sh --reference [run-all.sh options]   the ffmpeg reference stream + the harness's own server (fault tests)
# Starts `multiview run` on a generated config (two bars sources, one HLS output, one cast output), a TLS proxy in
# front of its control listener (Chrome needs an https receiver page), then checks (a)-(g). Exit code = number of
# failed required checks. Every wait is bounded; everything started is killed and verified with pgrep.
set -u
here="$(cd "$(dirname "$0")" && pwd)"; root="$(cd "$here/../.." && pwd)"
if [ "${1:-}" = "--reference" ]; then shift; exec "$here/run-all.sh" "$@"; fi
engine="$root/target/debug/multiview"; validator="$root/target/debug/examples/cast_validate"
out="$here/out"; size="1920x1080"; cores=""; seconds=60; serial=""
while [ $# -gt 0 ]; do case "$1" in
  --engine) engine="$2"; shift 2 ;; --validator) validator="$2"; shift 2 ;; --out) out="$2"; shift 2 ;;
  --size) size="$2"; shift 2 ;; --cores) cores="$2"; shift 2 ;; --seconds) seconds="$2"; shift 2 ;;
  --serial) serial=1; shift ;; --parallel) serial=0; shift ;;
  *) echo "unknown argument $1" >&2; exit 2 ;; esac; done
W="${size%x*}"; H="${size#*x}"
# (b) and (c) each decode in a software Chrome beside the engine's libx264 encode. On a small host (CI: 4 vCPU)
# two browsers at once starve the encoder, so serial is the default there; parallel saves ~70 s on a big host.
if [ -z "$serial" ]; then if [ -n "$cores" ] || [ -n "${CI:-}" ]; then serial=1; else serial=0; fi; fi
[[ "$W" =~ ^[0-9]+$ && "$H" =~ ^[0-9]+$ ]] || { echo "--size must be WxH" >&2; exit 2; }
[ -x "$engine" ] || { echo "engine binary not found: $engine (cargo build -p multiview-cli --features gpl-codecs)" >&2; exit 2; }
[ -x "$validator" ] || { echo "validator not found: $validator (cargo build -p multiview-output --example cast_validate)" >&2; exit 2; }
mkdir -p "$out"; out="$(cd "$out" && pwd)"
pin=(); if [ -n "$cores" ]; then pin=(taskset -c "0-$((cores - 1))"); fi
HARNESS_RUN_ID="engine$$-$(date +%s)"; export OUT_DIR="$out" HARNESS_RUN_ID
export CAST_PROOF_BEARER="admin.castproof$RANDOM$RANDOM"
run_dir="$(mktemp -d "$out/run-XXXXXX")"; pids=()

descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; descendants "$c"; done; }
ours() { pgrep -f -- "conspect-cast-harnes[s]=$HARNESS_RUN_ID" 2>/dev/null; pgrep -f -- "$run_dir/cast[.]toml" 2>/dev/null; }
cleanup() {
  trap - EXIT INT TERM HUP
  local all; all="$(descendants $$)"
  [ -n "$all" ] && kill $all 2>/dev/null
  for _ in $(seq 1 30); do [ -z "$(descendants $$)" ] && break; sleep 0.3; done
  all="$(descendants $$) $(ours)"
  [ -n "${all// /}" ] && kill -9 $all 2>/dev/null
  wait 2>/dev/null
  rm -rf -- "${out:?}/chrome-profiles"
}
trap cleanup EXIT; trap 'exit 130' INT TERM HUP

if (exec 3<>/dev/tcp/127.0.0.1/8008) 2>/dev/null; then echo "port 8008 is in use (another harness run?): the CAF SDK needs ws://localhost:8008" >&2; exit 99; fi
rm -f "$out"/*.log "$out"/summary.json "$out"/*.detail.json "$out"/*.png
timeout 60 node "$here/check-browser.mjs" || exit 93
"$here/make-cert.sh" "$out/tls" || exit 98
port="$(python3 -c 'import socket;s=socket.socket(socket.AF_INET6);s.bind(("::1",0));print(s.getsockname()[1])')"
api="http://[::1]:$port"

# --- (setup) the engine: canvas WxH at 30000/1001, two bars sources side by side on Program, HLS + cast outputs
cat > "$run_dir/cast.toml" <<EOF
schema_version = 1
[canvas]
width = $W
height = $H
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "a"
kind = "bars"
[[sources]]
id = "b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "b"
[[outputs]]
id = "program-hls"
kind = "hls"
path = "$run_dir/hls/index.m3u8"
codec = "h264"
[[outputs]]
kind = "cast"
id = "cast-1"
dir = "$run_dir/cast"
[control]
listen = "[::1]:$port"
EOF
mkdir -p "$run_dir/hls"
MULTIVIEW_CONTROL_TOKEN="${CAST_PROOF_BEARER#admin.}" "${pin[@]}" "$engine" run "$run_dir/cast.toml" --program-audio > "$out/engine.log" 2>&1 & pids+=($!)
up=0; for _ in $(seq 1 150); do curl -sf -o /dev/null -H "Authorization: Bearer $CAST_PROOF_BEARER" "$api/api/v1/cast/config" && { up=1; break; }; sleep 0.2; done
[ "$up" = 1 ] || { echo "the engine did not answer on $api within 30 s (see $out/engine.log)" >&2; exit 97; }

"${pin[@]}" node "$here/proxy.mjs" --upstream "$api" --tls-key "$out/tls/key.pem" --tls-cert "$out/tls/cert.pem" --ports 0,0 --log "$out/proxy.jsonl" > "$out/proxy.log" 2>&1 & pids+=($!)
for _ in $(seq 1 50); do grep -q '^LISTENING ' "$out/proxy.log" 2>/dev/null && break; sleep 0.2; done
S="$(sed -n 's/^LISTENING .*"origins":\["\([^"]*\)".*/\1/p' "$out/proxy.log")"
[ -n "$S" ] || { echo "the TLS proxy did not start: $(cat "$out/proxy.log")" >&2; exit 96; }

# --- create the stream and wait for `ready` plus a full 10-segment window (steady state, as run-all.sh)
curl -sS -X POST -H "Authorization: Bearer $CAST_PROOF_BEARER" -H 'content-type: application/json' -d '{"feed":{"bus":"program"}}' "$api/api/v1/cast/streams" > "$out/stream.json"
id="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["id"])' "$out/stream.json" 2>/dev/null)"
path="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["path"])' "$out/stream.json" 2>/dev/null)"
[ -n "$id" ] && [ -n "$path" ] || { echo "POST /api/v1/cast/streams failed: $(head -c 300 "$out/stream.json")" >&2; exit 95; }
media="${path%master.m3u8}video.m3u8"
ok=0; for _ in $(seq 1 300); do n="$(curl -sk "$S$media" | grep -c '#EXTINF')"; [ "${n:-0}" -ge 10 ] && { ok=1; break; }; sleep 0.2; done
[ "$ok" = 1 ] || { echo "the cast stream did not reach 10 segments within 60 s" >&2; exit 94; }
echo "engine $api / proxy $S   run id $HARNESS_RUN_ID   size ${W}x${H}${cores:+   cores $cores}   serial $serial   out $out"

run() {   # run <log name> <timeout s> <command...>: bounded, logged, never aborts the script
  local log="$out/$1.log" t="$2"; shift 2
  echo ">>> $(date +%H:%M:%S) $1 $(basename "${2:-}")"
  timeout -k 10 "$t" "${pin[@]}" "$@" > "$log" 2>&1 & wait $!; echo "exit=$?" >> "$log"
  grep -E '^(RESULT|exit=)' "$log" | cut -c1-200
}
specs=()
node "$here/stream-health.mjs" "$S$media" > "$out/stream-health-before.json" 2>/dev/null
# (a) the validator on the engine's cast directory
{ "$validator" "$run_dir/cast"; rc=$?; echo "RESULT {\"test\":\"validator\",\"pass\":$([ $rc = 0 ] && echo true || echo false),\"failed\":[$([ $rc = 0 ] || echo "\"cast_validate exit $rc\"")],\"parts\":[]}"; echo "exit=$rc"; } > "$out/validator.log" 2>&1
specs+=("a-validator=$out/validator.log")
# (b) Shaka 4.15.56, SDK default config, and (c) the real receiver SDK + the engine-served receiver: side by side
if [ "$serial" = 1 ]; then
  run test-shaka $((seconds * 2 + 180)) node "$here/test-shaka.mjs" "$S$path" --seconds "$seconds" --sequential 1
  run test-receiver $((seconds + 120)) node "$here/test-receiver.mjs" "$S/cast/receiver/" "$S$path" --seconds "$seconds" --width "$W" --engine "$api" --stream "$id"
else
  run test-shaka $((seconds * 2 + 180)) node "$here/test-shaka.mjs" "$S$path" --seconds "$seconds" & t1=$!
  run test-receiver $((seconds + 120)) node "$here/test-receiver.mjs" "$S/cast/receiver/" "$S$path" --seconds "$seconds" --width "$W" --engine "$api" --stream "$id"
  wait "$t1"
fi
specs+=("b-shaka-engine=$out/test-shaka.log" "c-receiver-engine=$out/test-receiver.log")
# (d) Default-Media-Receiver equivalent: the test serves its bare page on its own origin, so every media request
# is cross-origin to the engine (its CORS headers, for real)
run test-receiver-default 300 node "$here/test-receiver-default.mjs" "$S$path" --seconds 30 --variants fmp4
specs+=("d-default-receiver-engine=$out/test-receiver-default.log")
node "$here/stream-health.mjs" "$S$media" > "$out/stream-health-after.json" 2>/dev/null   # (e)-(f) revoke this token
# (e)-(g) takes during playback, revoke + restart, authz
run test-engine 300 node "$here/test-engine.mjs" --api "$api" --origin "$S" --stream "$id" --path "$path" --cast-dir "$run_dir/cast" --validator "$validator" --width "$W" --proxy-log "$out/proxy.jsonl"
specs+=("e-g-engine=$out/test-engine.log")
# Documented comparison (informational): Shaka with CAF defaults on the engine's existing HLS output
run hls-shaka 200 node "$here/test-shaka.mjs" "$S/hls/program-hls/master.m3u8" --seconds 40 --configs caf-default --label hls-shaka
specs+=("hls-output-shaka-caf-default=$out/hls-shaka.log:informational")

node "$here/summarize.mjs" "$out" "${specs[@]}"; failures=$?
cleanup
left="$(ours)"
if [ -n "$left" ]; then echo "LEFTOVER processes: $left" >&2; failures=$((failures + 1)); else echo "no engine, proxy or Chrome process left behind (pgrep)"; fi
rm -rf -- "${run_dir:?}"
exit "$failures"
