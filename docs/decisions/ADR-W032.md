# ADR-W032: Start from the last state that started when the accepted state cannot boot

- **Status:** Accepted
- **Area:** Boot/Loaded/Running model ([ADR-W024](ADR-W024.md)) · invariant #1 (the output never stops because of a control action)
- **Date:** 2026-11-12
- **Source:** issue #317 (2 min outage on mda001: a persisted multi-cell canvas with a switcher M/E made every restart fail; a binary rollback could not help). Part 1 (one admission rule before persist) landed in PR #318.
- **Amended:** 2026-11-12, issue #324 (environment vs state errors, keep the accepted state in place, persisted and acknowledgeable warning) — see [Amendment (#324)](#amendment-324).

## Context

`multiview run` starts from the accepted state (`.multiview/desired.json`) when it exists.
PR #318 stops the control plane from persisting a state start-up refuses, but a state
persisted by an older build, an engine-side constraint that tightens in a newer build, or
a build failure (an encoder that went missing) can still make the saved state unstartable.
Before this change every such start failed, and systemd's restart loop turned that into an
outage that only a hand edit of `desired.json` could end.

## Decision

1. **Last-good copy.** `.multiview/last-good.json` is a serialized copy of the accepted
   envelope the engine last *started* from (by definition: later commits during the run did
   not start; each already passed the start-up admission rule before it was persisted, PR #318). "Started" = the pipeline built **and** the
   output clock emitted its first tick (the run's state slot sequence advanced). A
   control-plane task polls that wait-free counter (100 ms) and writes on the blocking pool
   with the shared atomic writer (tmp `O_EXCL` 0600 → fsync → rename → dir fsync). It never
   touches the clock; a run stopped before its first tick writes nothing.
2. **Fallback at start.** If the accepted state fails start-up admission
   (`multiview_config::boot_admission`, the same rule the control plane enforces before it
   persists) or does not parse/validate (an I/O error reading it is retried once and then fails as before:
   a blip is never a rollback), or `Pipeline::build` fails on it while a *different*
   last-good exists, the run: logs the exact reason; proves the last-good state admits (and,
   for a build failure, builds) without writing anything; builds the engine from it; and only
   then keeps a copy of the rejected `desired.json` under `.multiview/rejected/` and starts
   from last-good, raising the critical health warning `accepted-state-rejected` (UI health
   banner, `GET /api/v1/health`). *As amended by #324:* `desired.json` is no longer replaced
   by last-good, only a build failure of the **state** class falls back, and the warning is
   persisted until acknowledged — see the amendment below.
3. **No last-good copy** (first install, or a copy that is unreadable/invalid/also
   unstartable): fail exactly as before, with the clear reason. A first install never
   invents a state.
4. `multiview validate` reports whether a start would fail or fall back.

## Consequences

- A restart, crash or power cycle with an unstartable saved state is no longer an outage;
  the operator sees why and keeps the rejected file.
- The fallback changes what RUNS, never the operator's saved intent (#324): it is surfaced,
  never silent, and heals by itself when the cause goes away.
- Ordering is crash-safe: nothing is written before the last-good engine is built; the kept
  copy and the marker are each written with an atomic rename; a crash at any point leaves
  `desired.json` intact and the next start repeats the same decision.
- An alarm-store `AlarmKind` was considered; the health-warning catalog is the existing
  mechanism for configuration faults (`config-file-invalid` & co.) and the UI already
  renders it, so the new code lives there.

## Amendment (#324)

The original rule moved the operator's accepted `desired.json` to `rejected/` and overwrote it
with last-good on **any** engine build error. A host fault (a port still in use, an HLS
directory the process may not create, a thread that cannot be spawned) breaks every state alike,
so that rule silently replaced a good configuration with an older one, and the warning vanished
on the next restart. The amended rules:

1. **Two failure classes.** Every build error carries a class
   (`multiview_cli::boot::FailureClass`):
   - **Environment** — the host failed: `PipelineError::Environment` (an OS `io::Error` during
     `Pipeline::build`: HLS segment-directory creation, the source-event runtime's worker
     thread, the output-routing threads) and `RunError::LeaseExpired` (an entitlement refusal
     refuses every state).
   - **State** — everything else: the configuration itself cannot be built; a different one
     may.
2. **Environment errors are retried, never rolled back.** The same configuration is rebuilt
   after 250 ms, 1 s and 2 s (4 attempts). If it still fails, start-up fails with "building
   the engine failed with an environment error (not a configuration fault), after N attempts:
   …; the saved configuration was not changed". This applies to every build attempt — the
   accepted state, the boot document and the last-good copy. No fallback is planned and
   nothing is written. The retry sleeps on the start-up thread before any output clock exists
   (invariant #1 is not in play).
3. **State errors fall back, keeping the accepted state in place.** `desired.json` is never
   moved or overwritten. After the last-good engine is built the run keeps a **copy** of the
   rejected bytes as `.multiview/rejected/desired-<unix-ms>-<n>.json` (identical bytes already
   kept are reused, so repeated restarts do not churn the bounded directory; newest 5 kept),
   writes the marker (rule 4) and runs from last-good. The control plane's in-memory accepted
   snapshot is seeded with the last-good envelope (`BootModel::with_accepted_override`), so the
   stores, the `GET` routes, the layout library and the engine agree with what runs. The first
   operator commit through the API writes a new `desired.json` based on what runs; the
   original bytes stay in `rejected/`.
4. **Why keeping it in place does not loop.** Every restart tries the accepted state first. If
   it still cannot start, the same fallback repeats: deterministic, the kept copy is reused and
   the marker is rewritten — no growth, no outage. If it can (the host was fixed, a newer build
   accepts it), the run simply starts from it: a fault that goes away heals with no operator
   action and no hand edit. The equal-content guard stays: when last-good is byte-equal to the
   accepted state there is nothing different to fall back to, so the original error is
   returned.
5. **A persisted, acknowledgeable warning.** The fallback is recorded in
   `.multiview/boot-fallback.json` (0600, atomic writer; reason, kept copy, time, message) at
   commit time — after a successful build, before any run mode branches — so a `--ticks` run,
   a run without `[control]` and a crash right after the commit all leave it. Every start reads
   it and raises `accepted-state-rejected` from it until an operator acknowledges it:
   - API: `POST /api/v1/health/accepted-state-rejected/ack` (write role; any other code is
     `404`; `404` when nothing is pending). It removes the marker on the blocking pool, records
     the warning as cleared and publishes `health.warning.cleared` through the drop-oldest lane
     (invariant #10).
   - CLI: `multiview acknowledge-fallback <config>` removes the marker; a running engine
     clears its banner on the API acknowledgement or its next start.
   - `multiview validate` prints a `WARN` line while one is pending.
6. **If the copy or marker cannot be written** (`ENOSPC`, a read-only state directory) the run
   still starts from last-good: `desired.json` is untouched, so nothing is lost, and
   availability comes first. The in-memory warning says the record could not be written and
   why, so it shows for this run and in the log.
7. **`AfterRename`.** The kept copy and the marker are written with
   `boot_model::write_atomic_checked`. An `AfterRename` error (the rename is visible, only the
   directory fsync failed) counts as written when the file reads back byte-equal (logged as a
   warning); otherwise it is a write failure (rule 6).
8. **Unclassified errors default to State, and that is safe now.** A build error nobody has
   classified yet (or one behind a feature this lane could not compile, such as `display-kms`
   device opens) falls back as before. Under the original rule that default was dangerous: a
   misclassified host fault destroyed the operator's saved state. Under rules 3–6 the fallback
   is non-destructive, so the worst case of a misclassification is one run from last-good with
   a loud, persisted warning — while the opposite default (treating an unknown fault as
   Environment) would bring back the #317 outage for every unclassified state fault.
