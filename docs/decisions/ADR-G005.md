# ADR-G005: Operator delegates routine review sign-off + merge to the agent (amends ADR-G003)

- **Status:** Accepted
- **Area:** Engineering Guardrails
- **Date:** 2026-06-14
- **Source:** operator directive 2026-06-14 — "You are responsible for all reviews and merges"
- **Amends:** [ADR-G003](ADR-G003.md)

## Context

[ADR-G003](ADR-G003.md) mandated adversarial cross-vendor review and named **a human as the final
approver** (branch protection: deterministic checks + ≥1 human approval), keeping AI review advisory
because it under-covers TOCTOU/race/timing/authz. For this single-operator source-available project
the per-PR human-approval step became the bottleneck that left finished, reviewed work parked in open
PRs — the exact "pending approval" anti-pattern [§0 ownership](../../CLAUDE.md) forbids. On 2026-06-14
the operator (the human approver of record) explicitly delegated routine review sign-off and merge to
the agent.

## Decision

The agent (Claude Code) is the **delegated approver** for routine changes in this repo and **owns the
review→merge pipeline end-to-end**:

1. **The adversarial cross-vendor review of ADR-G003 remains mandatory and unchanged** — a *different*
   vendor, fresh context, scoped checklist; never self-performed by the authoring vendor; unanimous
   approval is still a yellow flag. This is the quality core and is **not** relaxed.
2. The agent runs that review, triages/fixes findings, confirms the **required deterministic gates are
   green**, and **merges** — same turn, no separate per-PR human sign-off, no parking.
3. **The operator retains ultimate authority** and may veto, revert, or re-open any merge at any time.

### Bars that do NOT relax

- Never merge on a **failing required gate**; never `--admin`/bypass branch protection; never
  weaken/skip/delete a test ([ADR-G002](ADR-G002.md) holds).
- **High-risk diffs** (auth, concurrency, data migration, money) still get the 3-reviewer panel;
  **engine / hot-path / invariant #1·#10** changes still clear their chaos·soak + mutation bars
  **before** the agent merges.
- Genuinely irreversible / outward-facing actions beyond merging (force-pushing `main`, deleting
  infrastructure, public releases, external comms) are **surfaced to the operator**, not done silently.

## Rationale

The G003 safeguard has two separable parts: (a) **independent adversarial review** and (b) **a human
merge gate**. Part (a) carries most of the defect-catching value (CCR fresh-context + vendor-diversity
evidence) and is kept intact. Part (b) traded latency for a human catch on AI blind spots; for a
single-operator project that catch was rarely exercised and routinely became a parking bottleneck.
Delegating (b) to the agent restores execution velocity (§0) while the operator's standing override
preserves human ultimate authority.

## Alternatives considered

- **Keep ≥1 human approval per PR** — rejected by the operator: it is the parking bottleneck §0
  forbids and rarely caught what a green cross-vendor review + tests did not.
- **Drop the cross-vendor review too** — rejected: that is the quality core; removing it would lower
  the bar, which autonomy never does.
- **Carve out a *blocking* human gate for high-risk diffs** — rejected as a hard gate: instead
  high-risk diffs get the heightened multi-reviewer panel + required tests and are surfaced to the
  operator, who can override. Comprehensive delegation per the directive, with visibility.

## Consequences

- **Honest residual risk:** G003's noted AI blind spots (TOCTOU/race/timing/authz) no longer have a
  mandatory independent *human* reviewer. Mitigated by — and these are now hard requirements —
  property/concurrency tests for those classes ([ADR-G002](ADR-G002.md)), the heightened panel on
  high-risk diffs, and the operator's retained override. The operator accepted this trade for velocity.
- Branch protection should require the **deterministic checks**; the routine ≥1-human-approval rule is
  delegated to the agent (configure branch protection / the agent service account accordingly — do
  **not** use per-PR admin-bypass to mask a failing required check).
- ADR-G003 stays as the record of the original decision, marked **amended by this ADR**.
