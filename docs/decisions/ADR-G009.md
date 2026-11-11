# ADR-G009: No change class waits for the operator to merge — the agent owns PRs, CI and merge end to end (amends ADR-G005, ADR-G008)

- **Status:** Accepted
- **Area:** Engineering guardrails / governance
- **Date:** 2026-11-12
- **Source:** Operator directive (Troy Kelly, repository owner), 2026-11-12, after an agent asked him to confirm the merge of a docs-only R3 design note: "I need you to P0 and fix your agent documentation. If I have to tell you again this is your product to manage and you are responsible for PR's CI fixes and merging - I am going to loose it".
- **Amends:** [ADR-G005](ADR-G005.md) (scope of the delegation), [ADR-G008](ADR-G008.md) (the R3 row of the gate matrix)

## Context

[ADR-G005](ADR-G005.md) delegated review sign-off and merge to the agent for "routine changes" and
reserved a short list of outward-facing actions to the operator. [ADR-G008](ADR-G008.md) then wrote
the gate matrix, and its R3 row read "Human review: operator approval before the outward action"
and "Merge: operator confirms, then merge". `scripts/classify.sh` printed the same two lines, and
`codebase-map.md` and the orchestrate skill repeated them as "get explicit operator approval".

The effect was the opposite of G005's intent. Agents read the R3 row as "an R3 merge needs the
operator" and parked finished work as questions. On 2026-11-11 and 12 the operator was asked, among
other things, to confirm the merge of a docs-only design note, to approve a server restart on the
test host, and to choose between two fallback options the agent could measure itself. The estate
rules in `AGENTS.md` already say the opposite ("you merge your own work once the completion bar is
met"; "a run that stops to ask for permission it already has is a failed run"), so the repository's
own standard contradicted its always-loaded instructions.

## Decision

1. **The agent merges every change class, R3 included.** No class has a human review step and no
   class waits for an operator confirmation. The R3 row of the matrix now reads "agent merges on
   green, once every R3 gate above is met".
2. **R3 stays the highest bar; the bar is engineering work, not permission.** An R3 change still
   owes a governing issue, a plan, a design note or ADR, failing tests first, the 3-lens
   cross-vendor panel, a rehearsed recovery path, and the chaos/soak test for any invariant #1/#10
   risk. The chaos/soak test is mandatory for that risk and has no substitute.
   None of that is relaxed. The agent does that work and then merges.
3. **The agent owns everything around the merge:** fixing red CI (read the log, fix the cause, or
   rerun a tracked flaky job), keeping a PR up to date with `main`, closing superseded PRs, and
   deciding between engineering options it can measure.
4. **Report, do not ask.** Decisions are recorded in the PR (`## Concerns`) and in the final report.
   The operator may veto, revert or re-open anything afterwards (G005 §3 stands).
5. **What still goes to the operator is unchanged and is not a merge:** the outward-facing actions
   G005 lists (a public release, deleting infrastructure, a force-push of `main`, external
   communication, key handling) and the four stops in `AGENTS.md` "Obstacles" (destructive
   operations beyond the disposable environment, the git-irreversible, credentials or customer
   data, illegal or unauthorised targets). The rule "never alter legal content as engineering
   work" also stands. That list does not grow by analogy.
6. **Another team's infrastructure is not an operator question.** A change to the estate firewall
   or DNS goes through that team's own change process; the agent raises it there or works around it.

## What this ADR does NOT change

- **Deployment is not decided here.** The directive covers PRs, CI fixes and merging. Deploying to
  a test server is governed, as before, by the estate rules in `AGENTS.md` (a disposable,
  revertable environment) and by the operator's instructions for the run. This ADR neither grants
  nor removes that authority. A public release stays reserved to the operator (G005).
- The cross-vendor review of [ADR-G003](ADR-G003.md) stays mandatory, in a fresh context, never
  performed by the authoring vendor.
- Never merge on a failing check; never `--admin` or bypass; never weaken, skip or delete a test.
- The classifier still cannot be argued down: a class is a floor.
- `.github/CODEOWNERS` still routes governance paths to the operator for visibility. With no branch
  protection it routes and does not block, as [engineering.md](../standards/engineering.md) Part C
  already records.

## Alternatives considered

- **Keep "operator confirms" for R3 and tell agents to ask less.** Rejected: the text is what agents
  follow. While the matrix says "operator confirms", a careful agent asks, every time.
- **Remove R3's extra gates along with the confirmation.** Rejected: the panel and the chaos/soak
  gate are what protect invariants #1 and #10. Only the permission step is removed.
- **Also delegate public releases and key handling.** Not done here: the directive is about PRs, CI
  and merging. Those actions stay as G005 has them.

## Consequences

- An R3 change can reach `main` with no human having looked at it. The mitigation is the same as in
  G005, and it is now the whole of the mitigation: the 3-lens panel, the chaos/soak gate,
  property/concurrency tests, and the operator's ability to revert.
- A PR that waits for operator confirmation is, by definition, unfinished work that the agent owns.
- Files changed with this ADR: `docs/standards/engineering.md`, `scripts/classify.sh` (printed
  contract only), `AGENTS.md` (repository specifics), `docs/development/codebase-map.md`,
  `docs/development/agent-guardrails.md`, `.claude/skills/orchestrate/SKILL.md`,
  `.claude/workflows/README.md`.
