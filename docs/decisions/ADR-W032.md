# ADR-W032: Start from the last state that started when the accepted state cannot boot

- **Status:** Accepted
- **Area:** Boot/Loaded/Running model ([ADR-W024](ADR-W024.md)) · invariant #1 (the output never stops because of a control action)
- **Date:** 2026-11-12
- **Source:** issue #317 (2 min outage on mda001: a persisted multi-cell canvas with a switcher M/E made every restart fail; a binary rollback could not help). Part 1 (one admission rule before persist) landed in PR #318.

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
   then keeps the rejected `desired.json` as
   `.multiview/rejected/desired-<unix-ms>.json` (newest 5 kept); restores last-good as the
   accepted state (so control plane, stores and the next restart agree with what runs);
   starts; and raises the latched critical health warning `accepted-state-rejected` naming
   the reason and the kept file (UI health banner, `GET /api/v1/health`).
3. **No last-good copy** (first install, or a copy that is unreadable/invalid/also
   unstartable): fail exactly as before, with the clear reason. A first install never
   invents a state.
4. `multiview validate` reports whether a start would fail or fall back.

## Consequences

- A restart, crash or power cycle with an unstartable saved state is no longer an outage;
  the operator sees why and keeps the rejected file.
- The fallback is a Class-2 change of the operator's accepted intent; it is surfaced, never
  silent, and reversible (the rejected file is kept; re-apply through the API).
- Ordering is crash-safe: the rejected copy is written durably before `desired.json` is
  replaced, each with an atomic rename; a crash in between leaves both files intact and the
  next start repeats the decision.
- An alarm-store `AlarmKind` was considered; the health-warning catalog is the existing
  mechanism for configuration faults (`config-file-invalid` & co.) and the UI already
  renders it, so the new code lives there.
