#!/usr/bin/env bash
# Chaos test for the bounded exit (#309, docs/architecture/bounded-exit.md).
#
# Runs the REAL `multiview` binary (built with `_chaos-test-seams` and an
# H.264-capable `ffmpeg` feature set) N times on a two-source HLS config:
#   fatal: the bake consumer stalls once (the output falls behind the clock and
#          the run fails) while source teardown is held far past every grace;
#   stop:  SIGTERM while source teardown is held;
#   stopwedge: SIGTERM while the consumer is stalled (an output that cannot
#          finalise during the stop): exit code 1 within the bound.
# Asserts for every run: exit code (1 fatal, 0 stop), the time from the
# decision (the fatal log line / the signal) to the process exit is within the
# bound + slack, and the on-disk HLS manifest is valid and lists only complete
# segments.
#
# Usage: scripts/chaos-bounded-exit.sh <multiview-binary> [runs=20] [mode=both]
#   mode "both" runs all three modes (fatal, stop, stopwedge).
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
[control]
listen = "[::1]:0"
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
  grep -q '^#EXT-X-ENDLIST' "${m}" || { echo "manifest not finalised (no #EXT-X-ENDLIST)"; return 1; }
  local seg
  for seg in $(grep -v '^#' "${m}"); do
    [ -s "${WORK}/hls/${seg}" ] || { echo "listed segment ${seg} missing"; return 1; }
    case "${seg}" in *.tmp) echo "manifest lists a temp segment"; return 1 ;; esac
  done
}

run_once() {
  local mode="$1" i="$2" log="${WORK}/run.log" rc t_exit t_decide pid
  rm -rf "${WORK}/hls" "${WORK}/.multiview" && mkdir -p "${WORK}/hls"
  export MULTIVIEW_CHAOS_SLOW_TEARDOWN_MS="${HOLD_MS}"
  if [ "${mode}" = stopwedge ]; then
    # SIGTERM while the consumer is stalled 10 s (an output that cannot
    # finalise during the stop): the run must report failure (rc 1).
    export MULTIVIEW_CHAOS_CONSUMER_STALL_MS=10000 MULTIVIEW_CHAOS_CONSUMER_STALL_AT=60
  elif [ "${mode}" = fatal ]; then
    # 3 s at 29.97 fps is ~90 ticks: far past the 8-tick substitution envelope
    # (and the live queue), so the consumer must fail "fell N ticks behind".
    export MULTIVIEW_CHAOS_CONSUMER_STALL_MS=3000 MULTIVIEW_CHAOS_CONSUMER_STALL_AT=60
  else
    unset MULTIVIEW_CHAOS_CONSUMER_STALL_MS MULTIVIEW_CHAOS_CONSUMER_STALL_AT
  fi
  RUST_LOG=info "${BIN}" run "${WORK}/cfg.toml" > "${log}" 2>&1 &
  pid=$!
  if [ "${mode}" = stop ] || [ "${mode}" = stopwedge ]; then
    # Signal only once the pipeline runs (its first tick is logged), and only
    # if the process is still alive.
    local waited=0
    until grep -q "pipeline run: until" "${log}" 2>/dev/null; do
      sleep 0.1; waited=$((waited + 1))
      [ "${waited}" -lt 300 ] || { echo "run ${i} (stop): pipeline never started"; tail -20 "${log}"; kill -KILL "${pid}" 2>/dev/null || true; return 1; }
    done
    if [ "${mode}" = stopwedge ]; then
      local w2=0
      until grep -q "chaos: stalling the bake consumer" "${log}" 2>/dev/null; do
        sleep 0.05; w2=$((w2 + 1))
        [ "${w2}" -lt 400 ] || { echo "run ${i} (stopwedge): stall seam never reached"; tail -20 "${log}"; kill -KILL "${pid}"; return 1; }
      done
      sleep 0.2
    else
      sleep 2
    fi
    kill -0 "${pid}" || { echo "run ${i} (stop): process died before the signal"; tail -20 "${log}"; return 1; }
    t_decide="$(now_ms)"
    kill -TERM "${pid}"
  fi
  # Bounded wait: a run that never exits is a failure, never a hang.
  local polls=0
  while kill -0 "${pid}" 2>/dev/null; do
    sleep 0.02; polls=$((polls + 1))
    if [ "${polls}" -gt 2500 ]; then   # 50 s
      kill -KILL "${pid}" 2>/dev/null || true
      echo "run ${i} (${mode}): did not exit within 50 s"; tail -20 "${log}"; return 1
    fi
  done
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
    if "fatal encoded-output consumer failure" in line and "ticks behind the clock" in line:
        m = re.search(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)\.(\d+)Z", line)
        if m:
            base = datetime.datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
            frac = float("0." + m.group(2))
            print(int((base.timestamp() + frac) * 1000)); break
PY
)"
    [ -n "${t_decide}" ] || { echo "run ${i} (${mode}): no fatal line"; tail -20 "${log}"; return 1; }
  fi
  local took=$((t_exit - t_decide)) want=$([ "${mode}" = stop ] && echo 0 || echo 1)
  local extra=0
  echo "run ${i} (${mode}): rc=${rc} decision->exit=${took} ms"
  [ "${rc}" = "${want}" ] || { echo "  FAIL: exit code ${rc}, want ${want}"; tail -20 "${log}"; return 1; }
  # The bound must come from the watchdog (the held teardown cannot finish in
  # time on its own): require its armed line and its hard-exit message.
  grep -q "exit deadline armed" "${log}" || { echo "  FAIL: watchdog never armed"; return 1; }
  grep -q "teardown overran the" "${log}" || { echo "  FAIL: no watchdog hard exit (teardown not held?)"; return 1; }
  # The watchdog ended the process: nothing of the normal return path after it.
  if grep -q 'stage="run-tenants"' "${log}"; then echo "  FAIL: the run returned normally (not the watchdog)"; return 1; fi
  # The code the watchdog announced is the code the process returned.
  grep -q "exiting with code ${rc} " "${log}" || { echo "  FAIL: exit code ${rc} differs from the watchdog's"; return 1; }
  if [ "${mode}" != stopwedge ]; then
  grep -q "chaos: holding source teardown" "${log}" || { echo "  FAIL: teardown seam not reached"; return 1; }
  # Outputs are finalised BEFORE the held source teardown (egress first).
  python3 - "${log}" <<'PY' || { echo "  FAIL: egress not joined before source teardown"; return 1; }
import sys
lines = open(sys.argv[1], errors="replace").read().splitlines()
def first(s):
    return next((i for i, l in enumerate(lines) if s in l), None)
e, h = first('stage="egress-joined"'), first("chaos: holding source teardown")
sys.exit(0 if e is not None and h is not None and e < h else 1)
PY
  fi
  # The journal flush was applied at the exit decision.
  if grep -q "exit-time journal flush was not applied" "${log}"; then echo "  FAIL: journal flush not applied"; return 1; fi
  # The durable Running state was written at the exit decision.
  grep -q "exit-time running-state persist written" "${log}" && [ -s "${WORK}/.multiview/active.toml" ] \
    || { echo "  FAIL: the exit-time Running persist did not land"; grep -E "persist|armed|overran|control plane|stage=" "${log}" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-200 | tail -12; ls -la "${WORK}/.multiview" 2>&1 | head -5; return 1; }
  [ "${took}" -le $((BOUND_MS + SLACK_MS + extra)) ] || { echo "  FAIL: exit took ${took} ms > ${BOUND_MS}+${SLACK_MS}"; grep -E "teardown|exit|chaos" "${log}" | tail -12; return 1; }
  if [ "${mode}" = stopwedge ]; then
    # The output could not finalise: only require that what is on disk is valid.
    local m="${WORK}/hls/index.m3u8"
    if [ -s "${m}" ]; then
      for seg in $(grep -v '^#' "${m}"); do [ -s "${WORK}/hls/${seg}" ] || { echo "  FAIL: listed segment missing"; return 1; }; done
    fi
    return 0
  fi
  check_hls || { echo "  FAIL: HLS invalid"; return 1; }
}

write_config
fails=0
for i in $(seq 1 "${RUNS}"); do
  for mode in fatal stop stopwedge; do
    [ "${MODE}" = both ] || [ "${MODE}" = "${mode}" ] || continue
    run_once "${mode}" "${i}" || fails=$((fails + 1))
  done
done
echo "chaos-bounded-exit: ${fails} failure(s)"
[ "${fails}" = 0 ]
