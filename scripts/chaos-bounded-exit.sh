#!/usr/bin/env bash
# Chaos test for the bounded exit (#309, docs/architecture/bounded-exit.md).
#
# Runs the REAL `multiview` binary (built with `_chaos-test-seams` and an
# H.264-capable `ffmpeg` feature set) N times on a two-source HLS config:
#   fatal: the bake consumer stalls once (the output falls behind the clock and
#          the run fails) while source teardown is held far past every grace;
#   stop:  SIGTERM while source teardown is held;
#   stopwedge: SIGTERM while the consumer is stalled (an output that cannot
#          finalise during the stop): exit code 1 within the bound;
#   falterstop: a deterministic seam (MULTIVIEW_CHAOS_SHED_AT) makes the hot
#          queue refuse one tick, as a full queue does (no fatal); then SIGTERM:
#          the run faltered, so exit code 1 within the bound.
# Asserts for every run: exit code (1 fatal, 0 stop), the binary's own
# decision -> fire time, the time from the decision (the fatal log line / the
# signal) to the process exit, and the on-disk HLS manifest is valid and lists only complete
# segments.
#
# Usage: scripts/chaos-bounded-exit.sh <multiview-binary> [runs=20] [mode=both]
#   mode "both" (or "all") runs the four modes: fatal, stop, stopwedge, falterstop.
set -euo pipefail

BIN="${1:?usage: chaos-bounded-exit.sh <multiview-binary> [runs] [fatal|stop|both]}"
RUNS="${2:-20}"
MODE="${3:-both}"
BOUND_MS=2000
# The watchdog's own decision->fire time (monotonic). Every wait targets the
# absolute deadline, so the fire is late by at most the lateness of the one
# wait that crosses it. The watchdog reports:
#   max_wait_ms        - its longest single wait (the design: <= 20 ms);
#   worst_wake_late_ms - the worst lateness of a single wait, measured tightly
#                        around the park (the OS wake latency: the host's).
# Asserted: max_wait <= MAX_WAIT_MS (the watchdog's own logic, independent of
# the host); fire <= 2000 + worst_wake_late + LOGIC_SLACK_MS (one 1 ms step
# and ms rounding); worst_wake_late <= MAX_WAKE_LATE_MS, the host's bound.
# Measured worst single-wait lateness: <= 11 ms on a dedicated Apple-silicon
# host (idle and 28 busy loops; the default 50 ms bound); on the shared macOS
# CI VM 92 and 106 ms (one long wait), and with 20 ms waits 121-156 ms in
# every run of two 40-run series, so CI sets 200 ms.
MAX_WAIT_MS=20
LOGIC_SLACK_MS=5
MAX_WAKE_LATE_MS="${CHAOS_MAX_WAKE_LATE_MS:-50}"
# Fire -> process gone: the notice write plus the C runtime exit, cut by the
# 500 ms backstop (EXIT_PATH_GRACE); the observer adds its 1 ms poll and, for
# a fatal, the log write it waits for. Off CI: decision -> gone <= 2.6 s.
EXIT_PATH_MS=550
HOLD_MS=20000   # teardown held 10x past the bound
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

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
  local mode="$1" i="$2" log="${WORK}/run.log" rc pid
  rm -rf "${WORK}/hls" "${WORK}/.multiview" && mkdir -p "${WORK}/hls"
  export MULTIVIEW_CHAOS_SLOW_TEARDOWN_MS="${HOLD_MS}"
  unset MULTIVIEW_CHAOS_SHED_AT
  if [ "${mode}" = falterstop ]; then
    # A deterministic shed (#350): the hot queue refuses tick 60 exactly as a
    # full queue does, with no timing dependence (a 200 ms stall sheds
    # nothing on a fast host, and its outcome depended on the runner). The
    # run faltered, so it must exit 1.
    unset MULTIVIEW_CHAOS_CONSUMER_STALL_MS MULTIVIEW_CHAOS_CONSUMER_STALL_AT
    export MULTIVIEW_CHAOS_SHED_AT=60
  elif [ "${mode}" = stopwedge ]; then
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
  if [ "${mode}" = stop ] || [ "${mode}" = stopwedge ] || [ "${mode}" = falterstop ]; then
    # Signal only once the pipeline runs (its first tick is logged), and only
    # if the process is still alive.
    local waited=0
    until grep -q "pipeline run: until" "${log}" 2>/dev/null; do
      sleep 0.1; waited=$((waited + 1))
      if ! kill -0 "${pid}" 2>/dev/null && grep -q "building the pipeline" "${log}"; then
        # The host refused an encoder session (e.g. VideoToolbox busy):
        # an environment fault, not this run's scenario. Retry it once.
        echo "run ${i} (${mode}): pipeline build failed (encoder unavailable); retrying"
        return 3
      fi
      [ "${waited}" -lt 300 ] || { echo "run ${i} (${mode}): pipeline never started"; tail -20 "${log}"; kill -KILL "${pid}" 2>/dev/null || true; return 1; }
    done
    if [ "${mode}" = falterstop ]; then
      # Signal only after the forced shed happened (bounded wait).
      local w3=0
      until grep -q "chaos: forced shed" "${log}" 2>/dev/null; do
        sleep 0.05; w3=$((w3 + 1))
        [ "${w3}" -lt 400 ] || { echo "run ${i} (${mode}): forced shed never happened"; tail -20 "${log}"; kill -KILL "${pid}"; return 1; }
      done
      sleep 0.2
    elif [ "${mode}" = stopwedge ]; then
      local w2=0
      until grep -q "chaos: stalling the bake consumer" "${log}" 2>/dev/null; do
        sleep 0.05; w2=$((w2 + 1))
        if grep -q "ticks behind the clock" "${log}" 2>/dev/null; then
          # A genuine fault before the seam (e.g. a slow first hardware-encoder
          # open on a loaded host): not this run's scenario. Retry it once.
          kill -KILL "${pid}" 2>/dev/null || true; wait "${pid}" 2>/dev/null || true
          echo "run ${i} (${mode}): genuine early fatal before the seam; retrying"
          return 3
        fi
        [ "${w2}" -lt 400 ] || { echo "run ${i} (${mode}): stall seam never reached"; tail -20 "${log}"; kill -KILL "${pid}"; return 1; }
      done
      sleep 0.2
    else
      sleep 2
    fi
    kill -0 "${pid}" || { echo "run ${i} (stop): process died before the signal"; tail -20 "${log}"; return 1; }
  fi
  # One helper measures decision -> process gone on ONE monotonic clock, with a
  # 1 ms poll. Signal modes: the decision is the signal it sends. Fatal: the
  # decision is the moment it sees the fatal line in the log (the binary arms
  # the deadline before it logs, so this instant is at or after the decision;
  # the binary's own fired_after_decision_ms covers the decision side).
  # #350: separate python clock reads, a wall-clock log timestamp and 20 ms
  # shell polls added up to 0.5 s of observation error on a loaded host.
  local sig=0; [ "${mode}" = fatal ] || sig=1
  python3 - "${pid}" "${sig}" "${log}" > "${WORK}/gone" <<'PY'
import os, signal, sys, time
pid, sig, log = int(sys.argv[1]), sys.argv[2] == "1", sys.argv[3]
def alive():
    try:
        os.kill(pid, 0); return True
    except ProcessLookupError:
        return False
start = time.monotonic(); t0 = None
if sig:
    try:
        t0 = time.monotonic(); os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        print("gone-before-signal"); sys.exit(0)
else:
    pat = "fatal encoded-output consumer failure"
    with open(log, errors="replace") as f:
        buf = ""
        while t0 is None:
            buf = (buf + f.read())[-65536:]
            if pat in buf and "ticks behind the clock" in buf:
                t0 = time.monotonic(); break
            if not alive():
                print("no-fatal-line"); sys.exit(0)
            if time.monotonic() - start > 60:
                print("timeout"); sys.exit(0)
            time.sleep(0.001)
while alive():
    if time.monotonic() - t0 > 50:
        print("timeout"); sys.exit(0)
    time.sleep(0.001)
print(int((time.monotonic() - t0) * 1000))
PY
  local took
  took="$(cat "${WORK}/gone")"
  case "${took}" in
    timeout) kill -KILL "${pid}" 2>/dev/null || true
             echo "run ${i} (${mode}): did not exit within 50 s"; tail -20 "${log}"; return 1 ;;
    no-fatal-line) echo "run ${i} (${mode}): no fatal line"; tail -20 "${log}"; return 1 ;;
    gone-before-signal) echo "run ${i} (${mode}): process died before the signal"; tail -20 "${log}"; return 1 ;;
    ''|*[!0-9]*) kill -KILL "${pid}" 2>/dev/null || true
             echo "  FAIL: observer produced no measurement (${took})"; return 1 ;;
  esac
  set +e
  wait "${pid}"
  rc=$?
  set -e
  local want=$([ "${mode}" = stop ] && echo 0 || echo 1)
  # #350: the watchdog's own monotonic clock, read inside the binary:
  # decision (first arm) -> deadline fired. This is the bound the design
  # promises; it excludes the shell's own observation latency.
  local fire_ms
  fire_ms="$(grep -o 'fired_after_decision_ms=[0-9]*' "${log}" | head -1 | cut -d= -f2 || true)"
  local wake_ms wait_ms
  wake_ms="$(grep -o 'worst_wake_late_ms=[0-9]*' "${log}" | head -1 | cut -d= -f2 || true)"
  wait_ms="$(grep -o 'max_wait_ms=[0-9]*' "${log}" | head -1 | cut -d= -f2 || true)"
  local extra=0
  echo "run ${i} (${mode}): rc=${rc} decision->exit=${took} ms (armed->fired ${fire_ms} ms, worst wake late ${wake_ms} ms, max wait ${wait_ms} ms)"
  if [ "${mode}" = falterstop ] && grep -q "ticks behind the clock" "${log}"; then
    echo "  FAIL: falterstop hit the fatal envelope (stall too long for this host)"; return 1
  fi
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
  # The design bound: the watchdog fires at EXIT_DEADLINE after the decision
  # (its own clock), late only by the host's wake latency, never by itself.
  [ -n "${fire_ms}" ] && [ -n "${wake_ms}" ] && [ -n "${wait_ms}" ] || { echo "  FAIL: no fired_after_decision_ms / worst_wake_late_ms / max_wait_ms in the hard-exit line"; return 1; }
  [ "${wait_ms}" -le "${MAX_WAIT_MS}" ] || { echo "  FAIL: a watchdog wait of ${wait_ms} ms (design: <= ${MAX_WAIT_MS} ms)"; return 1; }
  [ "${fire_ms}" -ge "${BOUND_MS}" ] && [ "${fire_ms}" -le $((BOUND_MS + wake_ms + LOGIC_SLACK_MS)) ] \
    || { echo "  FAIL: watchdog fired ${fire_ms} ms after the decision (want ${BOUND_MS}..$((BOUND_MS + wake_ms + LOGIC_SLACK_MS)): its worst wait was ${wake_ms} ms late)"; grep -E "scheduling class|chaos|teardown overran" "${log}" | cut -c1-240 | tail -6; return 1; }
  [ "${wake_ms}" -le "${MAX_WAKE_LATE_MS}" ] \
    || { echo "  FAIL: a watchdog wait woke ${wake_ms} ms late (host bound ${MAX_WAKE_LATE_MS} ms)"; return 1; }
  grep -q "exit watchdog scheduling class" "${log}" || { echo "  FAIL: watchdog class not reported"; return 1; }
  if [ "${mode}" = falterstop ]; then
    grep -q "chaos: forced shed" "${log}" || { echo "  FAIL: forced shed not exercised"; return 1; }
  fi
  # And the process was gone within the exit path after the fire, as the
  # observer sees it.
  [ "${took}" -le $((fire_ms + EXIT_PATH_MS + extra)) ] || { echo "  FAIL: exit took ${took} ms > fire ${fire_ms} + ${EXIT_PATH_MS}"; grep -E "teardown|exit|chaos" "${log}" | tail -12; return 1; }
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
ran=0
for i in $(seq 1 "${RUNS}"); do
  for mode in fatal stop stopwedge falterstop; do
    if [ "${MODE}" = both ] || [ "${MODE}" = all ] || [ "${MODE}" = "${mode}" ]; then
      set +e
      run_once "${mode}" "${i}"
      r=$?
      if [ "${r}" = 3 ]; then run_once "${mode}" "${i}"; r=$?; fi
      set -e
      [ "${r}" = 0 ] || fails=$((fails + 1))
      ran=$((ran + 1))
    fi
  done
done
[ "${ran}" -gt 0 ] || { echo "chaos-bounded-exit: mode '${MODE}' matched no run"; exit 2; }
echo "chaos-bounded-exit: ${ran} run(s), ${fails} failure(s)"
[ "${fails}" = 0 ]
