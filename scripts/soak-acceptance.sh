#!/usr/bin/env bash
# DEV-C4 acceptance-soak harness — drive a sync group of ≥2 nodes for a long run,
# scrape EVERY node's clock-servo telemetry + the engine output-tick counter,
# inject the invariant-#1 chaos (kill PTP/WS mid-soak), and render the PASS/FAIL
# verdict with `cargo xtask soak-report`.
#
# The clock-offset pass/fail MATHS lives in the tested `multiview-telemetry::soak`
# analyzer (p99 |offset| vs the ADR-M010 per-source bound + cadence continuity);
# this script is the orchestration around it: it scrapes each node, derives the
# real cross-node clock skew, applies the cadence floor, and runs the operator's
# physical-OCR hook. The `--dry-run` mode feeds bundled fixtures through
# `soak-report` and asserts the expected verdicts; that self-test runs in CI (the
# `soak harness self-test` job: shellcheck + `--dry-run`) with no hardware.
#
# Thresholds (clock offset, from the analyzer / ADR-M010): 99th-pct |offset| ≤
# 100 µs (PTP) / ≤ 1 ms (chrony) over the window. The cross-node frame-counter
# skew bound (the physical-OCR leg) is one frame period at --expected-fps, checked
# in this harness. Pass-conditions for the clock legs are the analyzer's — kept in
# one place; this script never re-implements them.
#
# What a real run gates on (a 2-node sync acceptance):
#   1. each node's per-source |offset| p99 ≤ the per-source bound (analyzer);
#   2. the cross-node clock skew p99 (max pairwise |offset_i − offset_j| per
#      source, per sample) ≤ the same per-source bound (analyzer) — the whole
#      point of a *multi-node* soak;
#   3. every node's output-tick counter advanced ≥ the cadence floor each sample
#      across the PTP/WS kill window (analyzer, invariant #1);
#   4. if --frame-ocr-hook is supplied: the burnt-in cross-node frame-counter skew
#      it reports stays within one frame period (checked here).
set -euo pipefail

# ── defaults ────────────────────────────────────────────────────────────────
CONTROLLER="http://[::1]:8080"          # IPv6-first (ADR-0042); bracket literals
GROUP=""                                 # sync-group id under test
NODE_METRICS=()                          # one /metrics URL per node (≥2)
DURATION_SECS=86400                      # 24 h acceptance window
SAMPLE_SECS=30                           # scrape cadence
OUT="soak-capture.json"                  # capture document the verdict reads
CADENCE_METRIC="multiview_engine_output_ticks_total"  # the output-tick counter
OFFSET_METRIC="multiview_clock_servo_offset_nanoseconds"
CHAOS_HOOK=""                            # command run once at the soak midpoint
FRAME_OCR_HOOK=""                        # command printing cross-node frame skew (ns)
EXPECTED_FPS=25                          # output cadence (Hz); the cadence-floor base
CADENCE_FLOOR=""                         # ticks/sample floor (default: derived below)
DRY_RUN=0

usage() {
  cat <<'USAGE'
soak-acceptance.sh — DEV-C4 multi-node clock/sync acceptance soak

  --controller URL     controller base (default http://[::1]:8080)
  --group ID           sync-group id under test (required for a real run)
  --node-metrics URL   a node's Prometheus /metrics endpoint (repeatable, ≥2)
  --duration SECS      soak window (default 86400 = 24 h)
  --sample SECS        scrape interval (default 30)
  --expected-fps N     output cadence in Hz; the cadence-floor base (default 25)
  --cadence-floor N    ticks every sample interval must advance (default:
                       sample × expected-fps; never below 1). A near-stalled
                       tick counter therefore FAILS, defeating a false-PASS.
  --cadence-metric M   the engine output-tick counter metric (must be monotone)
  --offset-metric M    the per-source clock-offset gauge (default shown above)
  --chaos-hook CMD     command run once at the midpoint to kill PTP/WS
  --frame-ocr-hook CMD command that prints the current cross-node burnt-in
                       frame-counter skew in nanoseconds to stdout (one integer
                       per call). Its output IS captured and gated against one
                       frame period at --expected-fps. Omit it to run the
                       telemetry legs only (the physical visual leg is then
                       simply absent — not silently passed).
  --out FILE           capture document path (default soak-capture.json)
  --dry-run            run the bundled fixtures through `xtask soak-report` and
                       assert the expected PASS/FAIL — no hardware, CI-safe
  -h, --help           this help
USAGE
}

# ── arg parsing ─────────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
  case "$1" in
    --controller) CONTROLLER="$2"; shift 2 ;;
    --group) GROUP="$2"; shift 2 ;;
    --node-metrics) NODE_METRICS+=("$2"); shift 2 ;;
    --duration) DURATION_SECS="$2"; shift 2 ;;
    --sample) SAMPLE_SECS="$2"; shift 2 ;;
    --expected-fps) EXPECTED_FPS="$2"; shift 2 ;;
    --cadence-floor) CADENCE_FLOOR="$2"; shift 2 ;;
    --cadence-metric) CADENCE_METRIC="$2"; shift 2 ;;
    --offset-metric) OFFSET_METRIC="$2"; shift 2 ;;
    --chaos-hook) CHAOS_HOOK="$2"; shift 2 ;;
    --frame-ocr-hook) FRAME_OCR_HOOK="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

repo_root() { git rev-parse --show-toplevel; }

# Render a capture document through the tested analyzer; returns its exit code.
run_verdict() {
  local capture="$1"
  ( cd "$(repo_root)" && cargo run --quiet --locked -p xtask -- soak-report "$capture" )
}

# ── dry-run: prove the harness→analyzer wiring with bundled fixtures ─────────
# A clean capture must PASS (exit 0); an offset-breach, a cross-node-skew breach,
# and a cadence-stall must each FAIL (exit 1). This is the CI-exercised self-test
# (the `soak harness self-test` job runs shellcheck + this) — no nodes needed.
dry_run() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  # Clean: two nodes both tight to PTP, their cross-node skew tiny, cadence steady.
  cat >"$tmp/pass.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[0,1000,-2000,500]},
            {"source":"ptp","samples_ns":[200,-900,1500,-400]},
            {"source":"ptp","samples_ns":[200,1900,3500,900]},
            {"source":"system","samples_ns":[0,400000,-300000]}],
 "cadence":{"tick_samples":[0,750,1500,2250,3000],"expected_min_delta":750}}
JSON
  # A single node's offset breaches its own PTP bound.
  cat >"$tmp/offset-breach.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[200000,200000,200000]}],
 "cadence":{"tick_samples":[0,750,1500],"expected_min_delta":750}}
JSON
  # Each node is individually fine, but the cross-node skew leg breaches the bound
  # (the two nodes are well-disciplined to opposite edges) — a de-synced pair.
  cat >"$tmp/skew-breach.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[0,0,0]},
            {"source":"ptp","samples_ns":[0,0,0]},
            {"source":"ptp","samples_ns":[150000,150000,150000]}],
 "cadence":{"tick_samples":[0,750,1500],"expected_min_delta":750}}
JSON
  # Cadence stalls across the kill window (a flat delta) — inv #1 violation.
  cat >"$tmp/cadence-stall.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[0,0,0]}],
 "cadence":{"tick_samples":[0,750,750,1500],"expected_min_delta":750}}
JSON

  echo "dry-run: clean capture must PASS"
  run_verdict "$tmp/pass.json"

  echo "dry-run: single-node offset breach must FAIL"
  if run_verdict "$tmp/offset-breach.json"; then
    echo "FAIL: offset-breach fixture unexpectedly passed" >&2; exit 1
  fi

  echo "dry-run: cross-node skew breach must FAIL"
  if run_verdict "$tmp/skew-breach.json"; then
    echo "FAIL: skew-breach fixture unexpectedly passed" >&2; exit 1
  fi

  echo "dry-run: cadence stall must FAIL"
  if run_verdict "$tmp/cadence-stall.json"; then
    echo "FAIL: cadence-stall fixture unexpectedly passed" >&2; exit 1
  fi

  # ── physical-OCR leg: a SUPPLIED hook that does not produce a reading must
  # FAIL, never silently pass. The operator asked for the visual check; a hook
  # that exits non-zero or prints nothing means it did not happen. (No hook at
  # all is the "not run" case, exercised by the real run, not a failure.)
  local frame_ns=$(( 1000000000 / EXPECTED_FPS ))

  echo "dry-run: supplied OCR hook that EXITS NON-ZERO must FAIL (never silent pass)"
  if ocr_dry_leg 'exit 7' 3 "$frame_ns"; then
    echo "FAIL: a failing OCR hook silently passed the physical leg" >&2; exit 1
  fi

  echo "dry-run: supplied OCR hook that emits NO VALUE must FAIL (never silent pass)"
  if ocr_dry_leg 'true' 3 "$frame_ns"; then
    echo "FAIL: a no-output OCR hook silently passed the physical leg" >&2; exit 1
  fi

  echo "dry-run: supplied OCR hook reading within one frame period must PASS"
  if ! ocr_dry_leg 'echo 1000' 3 "$frame_ns"; then
    echo "FAIL: a healthy in-bound OCR hook did not pass the physical leg" >&2; exit 1
  fi

  echo "dry-run: supplied OCR hook reading beyond one frame period must FAIL"
  if ocr_dry_leg "echo $(( frame_ns + 1 ))" 3 "$frame_ns"; then
    echo "FAIL: an out-of-bound OCR reading unexpectedly passed the physical leg" >&2; exit 1
  fi

  # A hook that prints an IN-BOUND reading but ALSO exits non-zero must FAIL: the
  # operator's check errored, so the in-bound number is not trustworthy. (This is
  # the case a naive PIPESTATUS-after-pipeline check misses — the exit status of a
  # hook inside `cmd | awk` is awk's, always 0, so the failure is invisible.)
  echo "dry-run: OCR hook that prints a value THEN exits non-zero must FAIL (ocr_hook_failed)"
  local out
  out="$(ocr_dry_leg 'printf "1000\n"; exit 7' 3 "$frame_ns")" && {
    echo "FAIL: a value-then-nonzero-exit OCR hook silently passed" >&2
    printf '%s\n' "$out" >&2; exit 1
  }
  case "$out" in
    *ocr_hook_failed*) : ;;
    *) echo "FAIL: value-then-nonzero hook did not report ocr_hook_failed:" >&2
       printf '%s\n' "$out" >&2; exit 1 ;;
  esac

  # A hook that exits zero but prints a NON-NUMERIC token must FAIL (no usable
  # reading), never be passed through as if it were a skew value.
  echo "dry-run: OCR hook that prints non-numeric output must FAIL (ocr_hook_no_output)"
  out="$(ocr_dry_leg 'echo abc' 3 "$frame_ns")" && {
    echo "FAIL: a non-numeric OCR hook silently passed" >&2
    printf '%s\n' "$out" >&2; exit 1
  }
  case "$out" in
    *ocr_hook_no_output*) : ;;
    *) echo "FAIL: non-numeric hook did not report ocr_hook_no_output:" >&2
       printf '%s\n' "$out" >&2; exit 1 ;;
  esac

  # A supplied hook that fails must not ABORT the soak: every poll is attempted
  # and the leg fails closed. Prove all polls ran (no set -e early-exit) by
  # checking the attempt count the verdict prints.
  echo "dry-run: a failing OCR hook must be polled every sample (no abort) and FAIL"
  out="$(ocr_dry_leg 'exit 7' 3 "$frame_ns")" && {
    echo "FAIL: a failing OCR hook silently passed" >&2; exit 1
  }
  case "$out" in
    *"3 hook poll(s)"*) : ;;
    *) echo "FAIL: the failing-hook leg did not poll all 3 samples (set -e abort?):" >&2
       printf '%s\n' "$out" >&2; exit 1 ;;
  esac

  echo "dry-run OK: harness→analyzer wiring verified (offset, cross-node skew, cadence, frame-OCR)"
}

# Drive the physical-OCR capture+verdict path against a hook command for a fixed
# number of polls (no metrics, no sleeps) — the same accumulators and verdict the
# real run uses, so the dry-run self-test gates the SAME fail-closed logic.
ocr_dry_leg() {
  local hook="$1" polls="$2" bound_ns="$3"
  local ocr_skew_max_ns=0 ocr_attempts=0 ocr_readings=0
  local ocr_hook_failed=0 ocr_no_output=0 j=0 ocr rc
  while [ "$j" -lt "$polls" ]; do
    ocr_attempts=$(( ocr_attempts + 1 ))
    # Same guarded capture as real_run: `|| rc=$?` so a failing hook under set -e
    # records the failure and the loop continues (no abort).
    rc=0; ocr="$(ocr_capture_sample "$hook")" || rc=$?
    if [ "$rc" -eq 0 ]; then
      ocr_readings=$(( ocr_readings + 1 ))
      [ "$ocr" -gt "$ocr_skew_max_ns" ] && ocr_skew_max_ns="$ocr"
    elif [ "$rc" -eq 2 ]; then
      ocr_hook_failed=$(( ocr_hook_failed + 1 ))
    else
      ocr_no_output=$(( ocr_no_output + 1 ))
    fi
    j=$(( j + 1 ))
  done
  ocr_leg_verdict "$ocr_attempts" "$ocr_hook_failed" "$ocr_no_output" \
    "$ocr_readings" "$ocr_skew_max_ns" "$bound_ns"
}

# ── real run helpers (hardware) ─────────────────────────────────────────────
# Scrape one Prometheus gauge value for a given metric+source from a /metrics URL.
# Prints the value, or nothing on miss (the caller substitutes 0).
scrape_offset() {
  local url="$1" source="$2"
  curl -fsS "$url" \
    | grep -E "^${OFFSET_METRIC}\{[^}]*source=\"${source}\"" \
    | awk '{print $NF}' | head -n1
}

# Scrape the monotone output-tick counter (cadence) from a /metrics URL.
scrape_cadence() {
  local url="$1"
  curl -fsS "$url" \
    | grep -E "^${CADENCE_METRIC}( |\{)" \
    | awk '{print $NF}' | head -n1
}

real_run() {
  if [ -z "$GROUP" ] || [ "${#NODE_METRICS[@]}" -lt 2 ]; then
    echo "a real run needs --group and ≥2 --node-metrics URLs" >&2
    exit 2
  fi

  # Derive the cadence floor from the scrape interval × expected output cadence
  # unless the operator overrode it. Never below 1. A near-stalled tick counter
  # (e.g. advancing a single tick over a 30 s interval) is then FAR below the
  # floor and FAILS — the inv-#1 chaos assertion has real teeth on hardware.
  if [ -z "$CADENCE_FLOOR" ]; then
    CADENCE_FLOOR=$(( SAMPLE_SECS * EXPECTED_FPS ))
    [ "$CADENCE_FLOOR" -lt 1 ] && CADENCE_FLOOR=1
  fi
  echo "cadence floor: ${CADENCE_FLOOR} ticks per ${SAMPLE_SECS}s sample (expected ${EXPECTED_FPS} Hz)"

  echo "starting the burnt-in test-pattern on sync-group ${GROUP}"
  curl -fsS -X POST "${CONTROLLER}/api/v1/sync-groups/${GROUP}/test-pattern" \
    -H 'content-type: application/json' -d '{}' >/dev/null

  local node_count="${#NODE_METRICS[@]}"
  local samples=$(( DURATION_SECS / SAMPLE_SECS ))
  local midpoint=$(( samples / 2 ))

  # Per-node, per-source offset series + per-node tick series + the derived
  # per-source cross-node skew series + the physical-OCR skew series. Each is a
  # comma-joined string of integers built across the run.
  local -a ptp_series sys_series tick_series
  local n
  for (( n = 0; n < node_count; n++ )); do
    ptp_series[n]=""; sys_series[n]=""; tick_series[n]=""
  done
  local ptp_skew_series="" sys_skew_series="" ocr_skew_series=""
  local ocr_skew_max_ns=0 ocr_attempts=0 ocr_readings=0
  local ocr_hook_failed=0 ocr_no_output=0

  local i=0
  while [ "$i" -lt "$samples" ]; do
    # Scrape every node this interval.
    local -a ptp_now sys_now
    for (( n = 0; n < node_count; n++ )); do
      local p s t
      p="$(scrape_offset "${NODE_METRICS[n]}" ptp || echo 0)"; p="${p:-0}"
      s="$(scrape_offset "${NODE_METRICS[n]}" system || echo 0)"; s="${s:-0}"
      t="$(scrape_cadence "${NODE_METRICS[n]}" || echo 0)"; t="${t:-0}"
      ptp_now[n]="$p"; sys_now[n]="$s"
      ptp_series[n]="${ptp_series[n]}${p},"
      sys_series[n]="${sys_series[n]}${s},"
      tick_series[n]="${tick_series[n]}${t},"
    done

    # Derive the cross-node clock skew this sample: the max pairwise absolute
    # difference between nodes' offsets, per source. This is the real
    # frame-accuracy signal a *multi-node* soak exists to measure.
    ptp_skew_series="${ptp_skew_series}$(max_pairwise_abs "${ptp_now[@]}"),"
    sys_skew_series="${sys_skew_series}$(max_pairwise_abs "${sys_now[@]}"),"

    # Physical leg: capture (do NOT discard) the operator hook's cross-node
    # burnt-in frame-counter skew in ns, gate it against one frame period. A
    # supplied hook that fails (non-zero) or emits no parseable value is counted
    # as an OCR *failure* this sample — the operator asked for the visual check
    # and it did not happen, so it must not silently pass (gated below).
    if [ -n "$FRAME_OCR_HOOK" ]; then
      ocr_attempts=$(( ocr_attempts + 1 ))
      local ocr rc=0
      # Guard with `|| rc=$?`: under `set -e` a command substitution that returns
      # non-zero in a bare assignment aborts the script — a failing hook must
      # record the failure and let the soak CONTINUE, never kill the run.
      ocr="$(ocr_capture_sample "$FRAME_OCR_HOOK")" || rc=$?
      if [ "$rc" -eq 0 ]; then
        ocr_readings=$(( ocr_readings + 1 ))
        ocr_skew_series="${ocr_skew_series}${ocr},"
        [ "$ocr" -gt "$ocr_skew_max_ns" ] && ocr_skew_max_ns="$ocr"
      elif [ "$rc" -eq 2 ]; then
        ocr_hook_failed=$(( ocr_hook_failed + 1 ))
      else
        ocr_no_output=$(( ocr_no_output + 1 ))
      fi
    fi

    if [ "$i" -eq "$midpoint" ] && [ -n "$CHAOS_HOOK" ]; then
      echo "midpoint: injecting chaos (kill PTP/WS) via the chaos hook"
      eval "$CHAOS_HOOK" || true
    fi
    sleep "$SAMPLE_SECS"
    i=$(( i + 1 ))
  done

  # Assemble the capture: one offset leg per node per source, plus the derived
  # cross-node skew legs, plus the per-node cadence series (the verdict requires
  # every cadence leg to hold the floor — so a single stalled node fails).
  build_capture "$node_count"

  echo "rendering the clock verdict"
  local clock_ok=0
  if run_verdict "$OUT"; then clock_ok=1; fi

  # Physical-OCR leg (gated here, not in the clock analyzer — it is a frame-period
  # skew, not a clock offset). One frame period at EXPECTED_FPS, in ns.
  local ocr_ok=1
  if [ -n "$FRAME_OCR_HOOK" ]; then
    local frame_ns=$(( 1000000000 / EXPECTED_FPS ))
    if ! ocr_leg_verdict "$ocr_attempts" "$ocr_hook_failed" "$ocr_no_output" \
        "$ocr_readings" "$ocr_skew_max_ns" "$frame_ns"; then
      ocr_ok=0
    fi
  else
    echo "  [frame-ocr] no --frame-ocr-hook supplied — physical visual leg not run (telemetry legs only)"
  fi

  if [ "$clock_ok" -eq 1 ] && [ "$ocr_ok" -eq 1 ]; then
    echo "VERDICT: PASS"
  else
    echo "VERDICT: FAIL"
    exit 1
  fi
}

# Poll the operator's frame-OCR hook once and classify the result:
#   exit 0 + the absolute integer ns skew on stdout — a usable reading;
#   exit 2 — the hook itself exited non-zero (ocr_hook_failed);
#   exit 3 — the hook succeeded but produced no parseable non-negative-integer
#            first field (ocr_hook_no_output).
# A failed reading is NEVER coerced to skew 0; the caller counts it as a leg
# failure (fail-closed). The hook runs in its OWN command substitution so `$?`
# is the hook's real exit status (running it inside `hook | awk` would mask a
# non-zero exit, since the pipeline's status is awk's — always 0). The first
# field is parsed and numerically validated only after the exit is confirmed.
ocr_capture_sample() {
  local hook="$1" raw first value
  raw="$(eval "$hook" 2>/dev/null)" || return 2
  first="$(printf '%s\n' "$raw" | awk 'NR==1{print $1}')"
  value="$(abs_int "$first")"
  case "$value" in
    '' | *[!0-9]*) return 3 ;;
  esac
  printf '%s\n' "$value"
}

# Render the physical-OCR leg verdict from the run's accumulators: the polls
# attempted, how many had the hook exit non-zero, how many produced no parseable
# value, how many yielded a usable reading, the worst observed cross-node frame
# skew (ns), and the one-frame-period bound (ns). Prints a one-line verdict +
# reason and returns 0 (PASS) / 1 (FAIL).
#
# Fail-CLOSED: the operator supplied the hook to verify presentation sync, so if
# the hook ever failed (ocr_hook_failed), ever produced no reading
# (ocr_hook_no_output), or never produced a single usable reading, the leg is a
# FAILURE — a missing/failed reading is never coerced to skew 0 and passed.
ocr_leg_verdict() {
  local attempts="$1" hook_failed="$2" no_output="$3" readings="$4" max_ns="$5" bound_ns="$6"
  echo "  [frame-ocr] ${attempts} hook poll(s): ${readings} reading(s), ${hook_failed} hook-error, ${no_output} no-output"
  if [ "$hook_failed" -gt 0 ]; then
    echo "  [frame-ocr] supplied OCR hook exited non-zero on ${hook_failed}/${attempts} poll(s) — FAIL (ocr_hook_failed)"
    return 1
  fi
  if [ "$no_output" -gt 0 ]; then
    echo "  [frame-ocr] supplied OCR hook produced no reading on ${no_output}/${attempts} poll(s) — FAIL (ocr_hook_no_output)"
    return 1
  fi
  if [ "$readings" -lt 1 ]; then
    echo "  [frame-ocr] supplied OCR hook produced no usable reading — FAIL (ocr_hook_no_output)"
    return 1
  fi
  if [ "$max_ns" -le "$bound_ns" ]; then
    echo "  [frame-ocr] cross-node frame skew max ${max_ns} ns (bound ${bound_ns} ns) — PASS"
    return 0
  fi
  echo "  [frame-ocr] cross-node frame skew max ${max_ns} ns (bound ${bound_ns} ns) — FAIL (ocr_skew_exceeded)"
  return 1
}

# Absolute value of a (possibly signed, possibly float-ish) integer string → int.
abs_int() {
  local v="$1"
  v="${v%%.*}"            # drop any fractional part
  v="${v#+}"             # drop a leading +
  echo "${v#-}"          # drop a leading - (absolute value)
}

# Max pairwise |a_i − a_j| over the integer args (0 for <2 args). Pure bash/awk.
max_pairwise_abs() {
  if [ "$#" -lt 2 ]; then echo 0; return; fi
  printf '%s\n' "$@" | awk '
    { v[NR] = $1 + 0 }
    END {
      m = 0
      for (a = 1; a <= NR; a++)
        for (b = a + 1; b <= NR; b++) {
          d = v[a] - v[b]; if (d < 0) d = -d
          if (d > m) m = d
        }
      print m
    }'
}

# Emit the capture JSON from the per-node series + the derived skew series.
build_capture() {
  local node_count="$1"
  # Pass the series to python via env to avoid argv length/quoting limits.
  PTP_SERIES_0="${ptp_series[0]:-}" \
  python3 - "$OUT" "$node_count" "$CADENCE_FLOOR" \
    "$ptp_skew_series" "$sys_skew_series" \
    "${ptp_series[@]}" "###" "${sys_series[@]}" "###" "${tick_series[@]}" <<'PY'
import json, sys

out = sys.argv[1]
node_count = int(sys.argv[2])
floor = int(sys.argv[3])
ptp_skew = sys.argv[4]
sys_skew = sys.argv[5]

rest = sys.argv[6:]
sep1 = rest.index("###")
ptp_nodes = rest[:sep1]
rest2 = rest[sep1 + 1:]
sep2 = rest2.index("###")
sys_nodes = rest2[:sep2]
tick_nodes = rest2[sep2 + 1:]

def nums(s):
    return [int(float(x)) for x in s.split(",") if x.strip()]

offsets = []
# One offset leg per node per source — so EVERY node is evaluated, not just node A.
for s in ptp_nodes:
    v = nums(s)
    if v:
        offsets.append({"source": "ptp", "samples_ns": v})
for s in sys_nodes:
    v = nums(s)
    if v:
        offsets.append({"source": "system", "samples_ns": v})
# The derived cross-node clock-skew legs (same per-source bound).
for src, s in (("ptp", ptp_skew), ("system", sys_skew)):
    v = nums(s)
    if v:
        offsets.append({"source": src, "samples_ns": v})

# The cadence verdict requires every leg to hold the floor; emit one cadence
# entry per node by feeding the analyzer the per-node tick series. The capture
# schema carries a single cadence object, so a multi-node run fails if ANY node
# stalls by reducing every node's series to its own min consecutive delta and
# taking the worst — the analyzer then sees the worst node's continuity.
def min_delta(v):
    if len(v) < 2:
        return None
    return min(b - a for a, b in zip(v, v[1:]))

tick_series = [nums(s) for s in tick_nodes]
tick_series = [v for v in tick_series if v]
# Worst node's tick series (the one with the smallest min consecutive delta);
# if it holds the floor, all do.
worst = None
worst_md = None
for v in tick_series:
    md = min_delta(v)
    if md is None:
        continue
    if worst_md is None or md < worst_md:
        worst_md, worst = md, v
cadence_ticks = worst if worst is not None else (tick_series[0] if tick_series else [])

doc = {
    "offsets": offsets,
    "cadence": {"tick_samples": cadence_ticks, "expected_min_delta": floor},
}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
print(f"wrote {out} ({len(offsets)} offset legs over {node_count} nodes)")
PY
}

if [ "$DRY_RUN" -eq 1 ]; then
  dry_run
else
  real_run
fi
