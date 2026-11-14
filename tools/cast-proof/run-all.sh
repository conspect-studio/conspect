#!/bin/bash
# Run the whole proof: reference stream generator + harness server, then tests 1-4 on the reference stream and the
# two documented-failure runs on the production-shaped TS stream. Writes <out>/summary.json and one log per test.
#   ./run-all.sh [--seconds 60] [--out <dir>] [--stream-base <dir or http(s) base URL of a live cast stream>]
#                [--skip-faults] [--skip-prod]
#   LIVE_URL=<base URL> ./run-all.sh   also runs tests 1 and 3 (CAF defaults) once, 40 s each, against that live
#                                      HLS base (through the server's proxy route; read-only GETs).
# --stream-base: test the real engine's cast output instead of the ffmpeg reference stream (tests 1-3; the fault
# test always generates its own stream because it restarts the generator).
# Exit code = number of failures. Only one harness can run per host: the CAF SDK hard-codes ws://localhost:8008.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
seconds=60; out="$here/../out"; stream_base=""; skip_faults=0; skip_prod=0
while [ $# -gt 0 ]; do case "$1" in
  --seconds) seconds="$2"; shift 2 ;; --out) out="$2"; shift 2 ;; --stream-base) stream_base="$2"; shift 2 ;;
  --skip-faults) skip_faults=1; shift ;; --skip-prod) skip_prod=1; shift ;;
  *) echo "unknown argument $1" >&2; exit 2 ;; esac; done
mkdir -p "$out"; out="$(cd "$out" && pwd)"
HARNESS_RUN_ID="run$$-$(date +%s)"; export OUT_DIR="$out" HARNESS_RUN_ID
REF=refAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; TS=tsAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; LIVE=liveAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
pids=()

ours() {   # PIDs this run may have left: Chrome with our marker, ffmpeg working in our stream directories
  pgrep -f -- "conspect-cast-harnes[s]=$HARNESS_RUN_ID" 2>/dev/null
  for p in $(pgrep -x ffmpeg 2>/dev/null); do
    case "$(readlink "/proc/$p/cwd" 2>/dev/null)" in "$out"/streams/*) echo "$p" ;; esac
  done
}
descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; descendants "$c"; done; }
cleanup() {
  trap - EXIT INT TERM HUP
  local all; all="$(descendants $$)"
  [ -n "$all" ] && kill $all 2>/dev/null      # tests stop their own Chrome and ffmpeg on SIGTERM
  for _ in 1 2 3 4 5 6 7 8 9 10; do [ -z "$(descendants $$)" ] && break; sleep 0.3; done
  all="$(descendants $$) $(ours)"
  [ -n "${all// /}" ] && kill -9 $all 2>/dev/null
  wait 2>/dev/null
  rm -rf -- "${out:?}/chrome-profiles"      # profiles of tests that were killed before they could remove their own
}
trap cleanup EXIT; trap 'exit 130' INT TERM HUP

if (exec 3<>/dev/tcp/127.0.0.1/8008) 2>/dev/null; then echo "port 8008 is in use (another harness run?): the CAF SDK needs ws://localhost:8008" >&2; exit 99; fi
rm -f "$out"/*.log "$out"/summary.json "$out"/*.detail.json "$out"/receiver-*.png
"$here/make-cert.sh" "$out/tls" || exit 98

# --- streams
if [ -z "$stream_base" ]; then
  "$here/gen-reference-stream.sh" "$out/streams/ref" 3600 2> "$out/gen-ref.log" & pids+=($!)
  stream_base="$out/streams/ref"
fi
if [ "$skip_prod" = 0 ]; then "$here/gen-stream.sh" prod-ts "$out/streams/ts" 3600 2> "$out/gen-ts.log" & pids+=($!); fi

# --- server (ports chosen by the OS, read back from its log)
maps=(--map "$REF=$stream_base" --map "$TS=$out/streams/ts")
[ -n "${LIVE_URL:-}" ] && maps+=(--map "$LIVE=$LIVE_URL")
node "$here/server.mjs" --port 0 --tls-port 0 --tls-key "$out/tls/key.pem" --tls-cert "$out/tls/cert.pem" "${maps[@]}" > "$out/server.log" 2>&1 & pids+=($!)
for _ in $(seq 1 50); do grep -q '^LISTENING ' "$out/server.log" 2>/dev/null && break; sleep 0.2; done
H="$(sed -n 's/^LISTENING .*"http":"\([^"]*\)".*/\1/p' "$out/server.log")"; S="$(sed -n 's/^LISTENING .*"https":"\([^"]*\)".*/\1/p' "$out/server.log")"
[ -n "$H" ] && [ -n "$S" ] || { echo "the harness server did not start: $(cat "$out/server.log")" >&2; exit 97; }
ready() { for _ in $(seq 1 250); do n="$(curl -s "$H/cast/$1/$2" | grep -c '#EXTINF')"; [ "${n:-0}" -ge "$3" ] && return 0; sleep 0.2; done; echo "stream $1 not ready" >&2; return 1; }
# Tests 1-3 measure the steady state: wait for the full 10-segment window (the cold start at 3 segments, which is
# what every real cast is, is case k of the fault test).
ready "$REF" video.m3u8 10 || exit 96
echo "server $H / $S   run id $HARNESS_RUN_ID   out $out"

run() {   # run <log name> <timeout s> <command...>: bounded, logged, never aborts the script (the test runs in
          # the background and is waited for, so that a signal to this script is handled at once)
  local log="$out/$1.log" t="$2"; shift 2
  echo ">>> $(date +%H:%M:%S) $*" | sed "s#/cast/\([A-Za-z0-9_-]\{6\}\)[A-Za-z0-9_-]*#/cast/\1...#g"
  timeout -k 10 "$t" "$@" > "$log" 2>&1 & wait $!; echo "exit=$?" >> "$log"
  grep -E '^(RESULT|exit=)' "$log" | cut -c1-200
}
specs=()
# Is the stream itself on time on this host? (Recorded in the summary: see stream-health.mjs.)
node "$here/stream-health.mjs" "$H/cast/$REF/video.m3u8" > "$out/stream-health-before.json" 2>/dev/null
# Tests 1 (Shaka alone) and 2 (receiver) do not share anything but the stream: run them side by side.
run test-shaka $((seconds * 2 + 180)) node "$here/test-shaka.mjs" "$H/cast/$REF/master.m3u8" --seconds "$seconds" & t1=$!
run test-receiver $((seconds + 120)) node "$here/test-receiver.mjs" "$S/cast/receiver/" "$S/cast/$REF/master.m3u8" --seconds "$seconds"
wait "$t1"
specs+=("1-shaka-reference=$out/test-shaka.log" "2-receiver-reference=$out/test-receiver.log")
run test-receiver-default 400 node "$here/test-receiver-default.mjs" "$S/cast/$REF/master.m3u8" --seconds 30
specs+=("3-default-receiver-reference=$out/test-receiver-default.log")
if [ "$skip_prod" = 0 ] && ready "$TS" video.m3u8 6; then
  run prod-ts-shaka 300 node "$here/test-shaka.mjs" "$H/cast/$TS/master.m3u8" --seconds 40 --label prod-ts-shaka & t1=$!
  run prod-ts-default-receiver 300 node "$here/test-receiver-default.mjs" "$S/cast/$TS/master.m3u8" --seconds 40 --variants ts,none --require ts,none --label prod-ts-default
  wait "$t1"
  specs+=("prod-ts-shaka-caf-default=$out/prod-ts-shaka.log:expect-fail=caf-default" "prod-ts-default-receiver=$out/prod-ts-default-receiver.log:expect-fail")
fi
if [ -n "${LIVE_URL:-}" ]; then
  run live-shaka 200 node "$here/test-shaka.mjs" "$H/cast/$LIVE/master.m3u8" --seconds 40 --configs caf-default --label live-shaka
  run live-default-receiver 200 node "$here/test-receiver-default.mjs" "$S/cast/$LIVE/master.m3u8" --seconds 40 --variants none --require none --label live-default
  specs+=("live-shaka-caf-default=$out/live-shaka.log:expect-fail" "live-default-receiver=$out/live-default-receiver.log:expect-fail")
fi
if [ "$skip_faults" = 0 ]; then
  run test-receiver-faults 1500 node "$here/test-receiver-faults.mjs"
  specs+=("4-receiver-faults=$out/test-receiver-faults.log")
fi

node "$here/stream-health.mjs" "$H/cast/$REF/video.m3u8" > "$out/stream-health-after.json" 2>/dev/null
node "$here/summarize.mjs" "$out" "${specs[@]}"; failures=$?
cleanup
left="$(ours)"
if [ -n "$left" ]; then echo "LEFTOVER processes: $left" >&2; failures=$((failures + 1)); else echo "no ffmpeg or Chrome process left behind (pgrep)"; fi
exit "$failures"
