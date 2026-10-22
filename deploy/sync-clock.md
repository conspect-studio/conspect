# Node clock discipline + acceptance soak (DEV-C4)

Display nodes in a **sync group** present frame-accurately against a common wall
timeline (ADR-M010). That only holds if each node's system clock is disciplined
to a shared reference. This guide covers the two supported disciplines, the
telemetry that proves they hold, and the acceptance-soak harness that certifies a
deployment.

> Clock discipline is a **node-OS** concern (`ptp4l`/`chronyd` run on the node,
> not inside Multiview). Multiview *reads* the disciplined clock and exports the
> servo offset/frequency so you can verify it — it never steps the system clock.

## Pass thresholds

The acceptance condition is the 99th-percentile of `|offset|` over a 24 h window,
per reference class (these are the constants in `multiview-telemetry::clock`, so
code and docs cannot drift):

| Reference | 99th-pct `|offset|` bound | Notes |
|-----------|---------------------------|-------|
| **PTP** (`ptp4l` + `phc2sys`) | **≤ 100 µs** | preferred; LAN hardware/software timestamping |
| **chrony/NTP** | **≤ 1 ms** | fallback on a non-PTP GbE switch |

The worst 1 % of samples may exceed the bound (it's a percentile, not a max) —
the analyzer tolerates the tail and fails only when the 99th-rank sample is over.

## Exported telemetry

Multiview exports these Prometheus gauges (scrape `/metrics`); the soak harness
reads them:

| Metric | Labels | Meaning |
|--------|--------|---------|
| `multiview_clock_servo_offset_nanoseconds` | `source="ptp"\|"system"` | disciplined-reference offset (ns) |
| `multiview_clock_servo_frequency_ppb` | `source` | servo frequency correction (ppb) |
| `multiview_audio_servo_resample_ppm` | `sink` | display-audio buffer-servo resample (ppm) |
| `multiview_audio_servo_fifo_fill_fraction` | `sink` | audio FIFO fill fraction |
| `multiview_audio_servo_skew_milliseconds` | `sink` | sample-vs-scanout skew (ms) |

## PTP (preferred) — `/etc/linuxptp/ptp4l.conf`

```ini
[global]
# Software timestamping is the portable default; set to hardware on a NIC + switch
# that support it for the tightest offsets.
time_stamping       software
# A single domain shared by every node in the sync group.
domainNumber        0
# IPv6-first transport (ADR-0042): PTP over UDP/IPv6.
network_transport   UDPv6
# Slave-only nodes never become master.
slaveOnly           1
logSyncInterval     -3
logMinDelayReqInterval -3

[eth0]
```

Discipline the *system* clock from the PTP hardware clock with `phc2sys`:

```ini
# /etc/linuxptp/phc2sys: -s <phc> -O 0 -w  (sync CLOCK_REALTIME to the PHC)
```

A `phc2sys` running with `-w` (wait for ptp4l) keeps `CLOCK_REALTIME` slaved; the
PHC = TAI, so the `[timing] ptp_utc_offset_s` (default 37) in the Multiview
config converts to UTC for the outbound epoch — see DEV-C1.

## chrony (fallback) — `/etc/chrony/chrony.conf`

```conf
# Two upstream sources minimum; iburst for fast initial sync.
pool        ntp.example.net iburst
# Step the clock only on the first few large corrections, then slew.
makestep    1.0 3
# Record drift so a restart re-converges quickly.
driftfile   /var/lib/chrony/drift
# IPv6-first: prefer the IPv6 addresses of the pool.
```

chrony on a non-PTP GbE switch typically holds well within the 1 ms bound.

## Running the acceptance soak

The harness (`scripts/soak-acceptance.sh`) drives a sync group of **≥2 nodes** and
gates a run on four things:

1. **each node's** per-source `|offset|` p99 ≤ the per-source bound (the
   `multiview-telemetry::soak` analyzer) — every `--node-metrics` URL is scraped,
   not just the first;
2. the **cross-node clock skew** p99 — the max pairwise `|offset_i − offset_j|`
   per source, per sample — ≤ the same per-source bound (the analyzer). This is
   the whole point of a *multi-node* soak: two nodes can each track the reference
   yet be skewed relative to each other, which this leg catches;
3. **every node's** output-tick counter advanced ≥ the cadence floor each sample
   across the PTP/WS kill window (the analyzer, invariant #1 — the nodes free-run
   on the held epoch). The floor defaults to `--sample × --expected-fps` (never 1),
   so a near-stalled tick counter FAILS;
4. if `--frame-ocr-hook` is supplied, the burnt-in **cross-node frame-counter
   skew** it prints (in ns) stays within one frame period at `--expected-fps`
   (checked in the harness — it is a presentation skew, not a clock offset).
   This leg is **fail-closed**: a supplied hook that exits non-zero or prints no
   parseable value FAILS the run (`ocr_hook_failed` / `ocr_hook_no_output`) — a
   missing reading is never coerced to skew 0 and passed.

The clock pass/fail maths lives only in the tested `cargo xtask soak-report`
analyzer; the harness scrapes, derives the cross-node skew, applies the cadence
floor, and runs the OCR hook around it.

**Verify the wiring (no hardware, CI-safe):**

```bash
scripts/soak-acceptance.sh --dry-run
```

This feeds bundled fixtures through `soak-report` and asserts a clean capture
passes while a **single-node offset breach**, a **cross-node skew breach**, and a
**cadence stall** each fail. It also drives the physical-OCR leg with sample hooks
and asserts that a hook which **exits non-zero** (with or without output), one that
**emits no value**, and one that **prints a non-numeric token** each FAIL, that a
hook printing an in-bound value **but also exiting non-zero** FAILs (its reading is
not trusted), and that a failing hook is **polled every sample** (the soak does not
abort) — while a valid in-bound reading passes. The fail-closed behaviour is gated,
not just asserted. The CI `soak harness self-test` job runs `shellcheck` plus this
`--dry-run`, so the harness→analyzer wiring is exercised on every relevant push.

**Real 24 h run (needs ≥2 nodes):**

```bash
scripts/soak-acceptance.sh \
  --controller 'http://[node-controller]:8080' \
  --group sg-livingroom \
  --node-metrics 'http://[node-a]:9090/metrics' \
  --node-metrics 'http://[node-b]:9090/metrics' \
  --expected-fps 50 \
  --chaos-hook 'ssh node-a systemctl stop ptp4l' \
  --frame-ocr-hook './capture-and-ocr.sh' \
  --duration 86400 --out soak-capture.json
```

`--expected-fps` sets the output cadence (the cadence-floor base **and** the
frame-skew bound); override the derived floor directly with `--cadence-floor N` if
needed. `cargo xtask soak-report soak-capture.json` re-renders the clock verdict
from a capture at any time (exit 0 = PASS, 1 = FAIL).

### The `--frame-ocr-hook` contract

The hook is the operator's physical capture+compare: a command that, each time it
is called, **prints to stdout one integer** — the current cross-node burnt-in
frame-counter skew in **nanoseconds** (e.g. from two cameras pointed at the node
heads + OCR of the DEV-C3 test-pattern counter). The harness **captures** that
output (it is not discarded) and fails the run if the worst skew exceeds one frame
period at `--expected-fps`.

The leg is **fail-closed**. Because you supplied the hook to verify presentation
sync, a hook that **exits non-zero** (`ocr_hook_failed`) or that **prints no
parseable integer** (`ocr_hook_no_output`) on any poll is a **FAILURE** — the
verification you asked for did not happen, so the harness never coerces the missing
reading to skew 0 and passes. Omit the hook entirely to run the telemetry legs
only; the physical visual leg is then simply **absent** (reported as "not run"),
which is *not* a failure. The contract is: supply the hook and it must produce a
reading every poll, or do not supply it at all.

### Hardware-gated

The **physical 24 h execution** — two real nodes on a non-PTP GbE switch, the live
`ptp4l`/`chronyd` discipline under load, and the cameras + OCR feeding
`--frame-ocr-hook` — is the operator's hardware-validation step (it needs ≥2
physical nodes). The harness, the cross-node-skew derivation, the chaos extension,
the cadence floor, and the verdict analyzer ship and are exercised in CI via
`--dry-run` (the `soak harness self-test` job). The burnt-in counter itself comes
from the sync-group **test-pattern** (DEV-C3), which the harness starts over the
API.
