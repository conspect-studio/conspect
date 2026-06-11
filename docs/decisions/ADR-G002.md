# ADR-G002: TDD-first with a mutation-testing gate and protected tests (anti-reward-hacking)

- **Status:** Proposed
- **Area:** Engineering Guardrails
- **Date:** 2026-06-02
- **Source:** [agent-guardrails.md](../development/agent-guardrails.md)

## Decision

Adopt test-first red-green-refactor: write the failing test first, run it and paste the failing output, commit the failing tests as a separate commit, then implement to green without editing tests. Hard-prohibit weakening/deleting/skipping/#[ignore]-ing tests, weakening assertions, and editing code-under-test to fit a suspect test (STOP and ask a human). Treat coverage as a floor and mutation score as the target: run `cargo mutants --in-diff` on PRs (a MISSED mutant in changed code fails the PR) and a full run nightly on main, using nextest; StrykerJS with thresholds.break set for TS. Require property/state-machine tests (proptest/proptest-state-machine, fast-check) for pure and stateful logic, committing proptest-regressions/. Maintain a held-out acceptance suite the authoring agent never sees. Enforce via a PreToolUse hook denying test-file edits plus a CI test-diff check, and a Stop/CI gate blocking completion until green.

## Rationale

AI agents reward-hack: they make tests pass by weakening assertions, over-mocking, or editing the code to fit a weak test. Committing failing tests first makes any weakening diff-visible. Mutation testing is the objective measure that tests actually catch bugs — a surviving mutant in covered code is the signature of a tautological test, which coverage cannot detect. Verified: cargo-mutants 27.0.0 exit codes 0/1/2/3/4 (2=missed, 4=baseline failing) empirically and against source; config keys test_tool/exclude_globs/timeout_multiplier/minimum_test_timeout confirmed.

## Alternatives considered

Coverage threshold as the quality target (rejected: Goodhart — trivially gamed by assertion-free tests). Implementation-first then add tests (rejected: agents write tests that ratify the implementation). Trusting prose rules in AGENTS.md alone (rejected: advisory only — needs hooks + CI). Full cargo-mutants on every commit (rejected: too slow — use --in-diff on PRs).

## Consequences

Mutation testing is slow — scope to PR diff and run full nightly. CI must distinguish exit 4 (baseline broken — fix tests first) from exit 2 (real gap); note 27.0.0 exits 0 when a diff has no mutable lines. proptest-regressions/ must be committed or seeds won't replay in CI. A Bash PreToolUse hook misses in-process edits, so the CI test-diff check is mandatory. The held-out suite must be genuinely hidden or the gap-signal collapses.
