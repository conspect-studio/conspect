# ADR-R012: Clock-servo telemetry model + a pure acceptance-soak verdict analyzer (DEV-C4)

- **Status:** Accepted
- **Area:** Resilience & A/V
- **Date:** 2026-06-19
- **Source:** operator request (DEV-C4) — implements the sync/timing acceptance gate of [ADR-M010](ADR-M010.md); testing posture from [ADR-R009](ADR-R009.md)

## Context

[ADR-M010](ADR-M010.md) gates the frame-accurate multi-output sync claim (published
Tiers S/A/B) on a **24 h acceptance soak**, and [ADR-T012](ADR-T012.md) fixes the
disciplined reference (PTP grandmaster / NTP-disciplined / free-run) as a *media-clock
reference only* — never a pacer. Two of ADR-M010's soak pass conditions are read off
the running node's servo state: the disciplined-reference **offset** (99th-percentile
`|offset|` ≤ 100 µs for PTP / ≤ 1 ms for chrony over the window) and a **converged**
(non-hunting) servo frequency correction. The display-audio buffer servo, which holds
the sink FIFO against scanout, has an analogous resample-ratio correction.

Until now none of this was observable or certifiable: there were no servo metrics, no
codified pass thresholds, and no harness to run the soak and render a verdict. The
constraints that bound the answer:

- **Invariant #3 (unified timing)** — timing comparisons are exact **integer
  nanoseconds**, never float seconds; the thresholds must be integer-ns constants.
- **Invariant #10 (isolation)** — telemetry is best-effort and must be *physically
  incapable* of back-pressuring the engine. `multiview-telemetry` is a **leaf** crate
  (no engine types, lock-free atomic gauges) and the servo publisher runs off the hot
  path at ~1 Hz.
- **GPU-less CI / rule-26** — the real servo discipline (a live `ptp4l`/`chronyd`
  p99 offset under load, a real audio resampler, a multi-day soak on ≥2 physical nodes)
  is on-hardware validation. CI cannot reproduce it, so the **pass/fail logic** must be
  pure data analysis runnable in the default software build (the ADR-R009 "run the suite
  on the software backend on every PR" posture).
- **Rule 27 (no aspirational docs)** — code and the operator runbook must not drift on
  the thresholds; one source of truth.

## Decision

Add two pure, dependency-free modules to `multiview-telemetry` plus an `xtask`
subcommand and an operator harness, all in the **default CI-green build, no hardware**:

1. **`multiview-telemetry::clock`** — the servo telemetry *model*: lock-free
   [`MetricsRegistry`] gauges for the disciplined-reference servo (offset ns + frequency
   ppb, labelled by a bounded `source` = `ptp`/`system`) and the display-audio buffer
   servo (resample ppm + FIFO fill fraction + sample-vs-scanout skew, labelled by
   `sink`); both legs registered up-front so a reference transition never makes a series
   disappear. The ADR-M010 pass thresholds are exported as **integer-nanosecond
   constants** (`PTP_OFFSET_P99_MAX_NS = 100_000`, `CHRONY_OFFSET_P99_MAX_NS =
   1_000_000`, `SOAK_WINDOW_SECS = 24 h`) with a `ClockSourceLabel` that maps each leg
   to its bound. The crate owns only the model — the off-hot-path 1 Hz publisher in
   `multiview-cli` (the same task that derives the outbound epoch) calls `record`.

2. **`multiview-telemetry::soak`** — the pure acceptance-soak **verdict** logic:
   nearest-rank `p99_abs_offset_ns`, a per-leg `evaluate_offset` (inclusive boundary),
   a `cadence_uninterrupted` invariant-#1 chaos assertion (output-tick counts must keep
   advancing across a deliberate PTP/WS kill window — a healthy node free-runs on the
   held epoch), and an aggregate `SoakReport` that passes only when the cadence held,
   ≥1 leg was measured, and every measured leg passed its threshold. No I/O, no
   side-effects — the same code is unit/proptest-tested in CI and invoked for real.

3. **`xtask soak-report <capture.json>`** — the capture-shape glue + human/CI-readable
   rendering over the `soak` analyzer (exit 0 = PASS, 1 = FAIL). Capture is a small
   JSON document (per-leg offset samples + chaos-window tick counts).

4. **`scripts/soak-acceptance.sh`** + **`deploy/sync-clock.md`** — the operator harness
   that drives a **≥2-node** sync group and gates on: each node's per-source offset
   p99 (every `--node-metrics` URL is scraped, not just the first); the **derived
   cross-node clock skew** (max pairwise `|offset_i − offset_j|` per source, per
   sample) checked against the same per-source bound via the analyzer; every node's
   cadence holding a floor that **defaults to `--sample × --expected-fps`** (never 1,
   so a near-stalled tick FAILS); and — when `--frame-ocr-hook` is supplied — the
   hook's captured cross-node burnt-in frame-counter skew (in ns) checked against one
   frame period at `--expected-fps` (a presentation skew, gated in the harness, not the
   clock analyzer). The OCR leg is **fail-closed**: a supplied hook that exits non-zero
   or produces no parseable reading is a **FAILURE** (`ocr_hook_failed` /
   `ocr_hook_no_output`), never coerced to skew 0 and passed — the operator asked for the
   visual check, so a check that did not happen fails. Omitting the hook is unchanged: the
   physical leg is reported "not run", which is not a failure. Plus the node-OS
   clock-discipline runbook (`ptp4l`/`chronyd` configs, exported-metric table, the
   `--frame-ocr-hook` ns contract, thresholds). `--dry-run` feeds bundled fixtures through
   `soak-report` and asserts a clean capture PASSes while a single-node offset breach, a
   cross-node-skew breach, and a cadence stall each FAIL; it also asserts the OCR leg FAILs
   for a hook that exits non-zero and for a hook that emits no value, and PASSes only for an
   in-bound reading.

5. **`.github/workflows/ci.yml` — the `soak harness self-test` job** runs `shellcheck`
   on the harness plus the `--dry-run` self-test on every relevant push, so the
   harness→analyzer wiring (including the cross-node-skew and cadence-floor gates) is
   exercised in CI with no hardware.

## Rationale

- **The verdict is the contract, so the verdict logic is pure and tested.** The same
  `multiview-telemetry::soak` code CI exercises is what a hardware soak is judged by —
  there is no second, untested copy of the pass/fail maths in a shell script. This is
  exactly ADR-R009's "you cannot prove *never falters* from inputs, only by consuming
  output and asserting numeric invariants", reduced to a CI-runnable core.
- **Integer-ns thresholds, one source of truth.** The bounds are `i64` constants in
  `clock.rs`; the runbook table and the analyzer both cite them, so code and docs cannot
  drift (rule 27, invariant #3). `CHRONY = PTP × 10` encodes the documented "PTP upgrades
  the tier" relationship as an asserted constant.
- **99th-percentile, not max.** The SLO tolerates the worst 1 % (a single GbE scheduling
  spike must not fail a 24 h run); nearest-rank `ceil(0.99·n)` matches ADR-M010's "99th-pct
  |offset|" wording exactly for any sample count, and the property tests pin the boundary.
- **Leaf-crate isolation preserved.** Gauges are latest-wins atomics; recording a sample
  is one relaxed store and references no engine type — the publisher can never
  back-pressure the engine (invariant #10).

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Pass/fail maths in the shell harness | An untested second copy of the contract; drifts from the runbook; can't be unit/proptest-tested in CI (violates ADR-R009 posture + rule 19). |
| A `max(|offset|)` bound instead of a percentile | A single legal GbE scheduling spike would fail a 24 h run; ADR-M010 specifies a percentile SLO. |
| Float-second thresholds / float fps in the analyzer | Violates invariant #3 (drift, non-exact comparisons); the soak compares an exact integer-ns percentile against an exact bound. |
| Put the servo gauges in `multiview-engine` | Breaks the leaf-crate boundary and the no-engine-type rule; risks coupling a telemetry write to the hot path. |
| Scrape only node A in the harness real-run | A 2-node sync soak that never reads node B (or its skew) can PASS a de-synced pair — the exact failure ADR-M010 exists to catch. The harness scrapes **every** node and derives the cross-node skew leg. |
| Hardcode the cadence floor at 1 tick/sample | A floor of 1 lets a 30 s sample PASS when the tick counter advanced a single tick — defeating the inv-#1 chaos assertion on hardware. The floor derives from `--sample × --expected-fps`. |
| Gate the frame-OCR skew with the clock-offset ns bound | A 1-frame presentation skew (20 ms at 50 fps) is ~200× the 100 µs PTP clock bound; the OCR leg needs a frame-period bound, checked in the harness, not the clock analyzer. |
| Let a supplied OCR hook that fails or emits nothing leave skew 0 and PASS (fail-open) | A soak-acceptance harness exists to *catch* desync; coercing a missing/failed reading to 0 silently passes the exact leg the operator added to verify presentation sync. The supplied-but-non-producing hook is fail-**closed** (`ocr_hook_failed` / `ocr_hook_no_output` ⇒ FAIL). Absence of a hook stays "not run", never a failure. |
| A new ADR family / numeric ADR | This is a telemetry/resilience implementation of an existing management decision (ADR-M010); the Resilience & A/V family (`R*`) is the correct home, next free number R012. |

## Consequences

- A node's clock discipline is now **observable** (5 Prometheus gauges) and a deployment
  is **certifiable** by a single repeatable command, with the pass/fail logic gated in
  every PR's `cargo test` — no GPU, no hardware.
- We are committed to keeping the ADR-M010 thresholds in `clock.rs` as the single source
  and to the capture-document JSON shape `soak-report` reads.
- **Hardware-validation seam (rule 26):** this change proves the **analyzer + thresholds +
  verdict logic** and the **harness orchestration** (multi-node scrape, cross-node-skew
  derivation, cadence floor, fail-closed OCR-hook capture+gate) — all exercised by the CI
  `--dry-run` self-test. It does **not** prove the real servo. The operator's on-hardware
  validation is: a live `ptp4l`/`chrony` p99 offset under production load; a real audio
  resampler's resample-ppm behaviour; and the physical 24 h soak across ≥2 nodes on a
  non-PTP GbE switch where the cameras + OCR of the burnt-in frame counter feed
  `--frame-ocr-hook` (the harness captures and gates that ns skew, failing closed if the
  hook errors or returns no reading, but the cameras/OCR themselves are physical) and the
  chaos is injected via `--chaos-hook`. The software path ships and is CI-exercised; the physical
  run is gated on real nodes.
- Touches invariant #1 (the `cadence_uninterrupted` chaos assertion *encodes* the
  output-never-falters guarantee as a soak pass condition) and invariant #10 (the servo
  gauges are leaf, lock-free, latest-wins) — neither is weakened.
