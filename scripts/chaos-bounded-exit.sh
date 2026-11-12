#!/usr/bin/env bash
# Chaos test for the bounded exit (#309, docs/architecture/bounded-exit.md).
#
# Runs the REAL `multiview` binary (built with `_chaos-test-seams` and an
# H.264-capable `ffmpeg` feature set) N times on a two-source HLS config:
#   fatal: the bake consumer stalls once (the output falls behind the clock and
#          the run fails) while source teardown is held far past every grace;
#   stop:  SIGTERM while source teardown is held.
# Asserts for every run: exit code (1 fatal, 0 stop), the time from the
# decision (the fatal log line / the signal) to the process exit is within the
# bound + slack, and the on-disk HLS manifest is valid and lists only complete
# segments.
#
# Usage: scripts/chaos-bounded-exit.sh <multiview-binary> [runs=20] [mode=both]
set -euo pipefail

BIN="${1:?usage: chaos-bounded-exit.sh <multiview-binary> [runs] [fatal|stop|both]}"
RUNS="${2:-20}"
MODE="${3:-both}"
BOUND_MS=2000
SLACK_MS=500
HOLD_MS=20000   # teardown held 10x past the bound
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

write_config() {
  cat > "${WORK}/cfg.toml" <<TOML
schema_version = 1
[canvas]
width = 640
height = 360
fps = "30000/1001"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
gap = 0
areas = ["a b"]
[[sources]]
id = "a"
kind = "bars"
[[sources]]
id = "b"
kind = "bars"
[[cells]]
id = "c1"
area = "a"
fit = "contain"
[cells.source]
input_id = "a"
[[cells]]
id = "c2"
area = "b"
fit = "contain"
[cells.source]
input_id = "b"
[[outputs]]
kind = "hls"
path = "${WORK}/hls/index.m3u8"
codec = "${CODEC:-h264}"
TOML
}

check_hls() {
  local m="${WORK}/hls/index.m3u8"
  [ -s "${m}" ] || { echo "no manifest"; return 1; }
  head -1 "${m}" | grep -q '^#EXTM3U' || { echo "manifest not M3U"; return 1; }
  local seg
  for seg in $(grep -v '^#' "${m}"); do
    [ -s "${WORK}/hls/${seg}" ] || { echo "listed segment ${seg} missing"; return 1; }
    case "${seg}" in *.tmp) echo "manifest lists a temp segment"; return 1 ;; esac
  done
}

run_once() {
  local mode="$1" i="$2" log="${WORK}/run.log" rc t_exit t_decide pid
  rm -rf "${WORK}/hls" && mkdir -p "${WORK}/hls"
  export MULTIVIEW_CHAOS_SLOW_TEARDOWN_MS="${HOLD_MS}"
  if [ "${mode}" = fatal ]; then
    export MULTIVIEW_CHAOS_CONSUMER_STALL_MS=1500 MULTIVIEW_CHAOS_CONSUMER_STALL_AT=60
  else
    unset MULTIVIEW_CHAOS_CONSUMER_STALL_MS MULTIVIEW_CHAOS_CONSUMER_STALL_AT
  fi
  RUST_LOG=info "${BIN}" run "${WORK}/cfg.toml" > "${log}" 2>&1 &
  pid=$!
  if [ "${mode}" = stop ]; then
    sleep 4
    t_decide="$(now_ms)"
    kill -TERM "${pid}"
  fi
  set +e
  wait "${pid}"
  rc=$?
  set -e
  t_exit="$(now_ms)"
  if [ "${mode}" = fatal ]; then
    # The decision instant is the fatal log line's own timestamp (UTC).
    t_decide="$(python3 - "${log}" <<'PY'
import re,sys,datetime
for line in open(sys.argv[1], errors="replace"):
    if "fatal encoded-output consumer failure" in line:
        m = re.search(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z", line)
        if m:
            t = datetime.datetime.fromisoformat(m.group(1)[:26]).replace(tzinfo=datetime.timezone.utc)
            print(int(t.timestamp()*1000)); break
PY
)"
    [ -n "${t_decide}" ] || { echo "run ${i} (${mode}): no fatal line"; tail -20 "${log}"; return 1; }
  fi
  local took=$((t_exit - t_decide)) want=$([ "${mode}" = fatal ] && echo 1 || echo 0)
  echo "run ${i} (${mode}): rc=${rc} decision->exit=${took} ms"
  [ "${rc}" = "${want}" ] || { echo "  FAIL: exit code ${rc}, want ${want}"; tail -20 "${log}"; return 1; }
  [ "${took}" -le $((BOUND_MS + SLACK_MS)) ] || { echo "  FAIL: exit took ${took} ms > ${BOUND_MS}+${SLACK_MS}"; grep -E "teardown|exit|chaos" "${log}" | tail -12; return 1; }
  check_hls || { echo "  FAIL: HLS invalid"; return 1; }
}

write_config
fails=0
for i in $(seq 1 "${RUNS}"); do
  for mode in fatal stop; do
    [ "${MODE}" = both ] || [ "${MODE}" = "${mode}" ] || continue
    run_once "${mode}" "${i}" || fails=$((fails + 1))
  done
done
echo "chaos-bounded-exit: ${fails} failure(s)"
[ "${fails}" = 0 ]
