# `.claude/workflows/` — committed Conductor workflow scripts

Reusable workflow scripts the Conductor invokes by name (`Workflow({ name: '<file>' })`)
or by `scriptPath`. They are the loop's fan-out tools (ADR-G007; the
[`orchestrate`](../skills/orchestrate/SKILL.md) skill). Committed via the
`!/.claude/workflows/` negation in [`.gitignore`](../../.gitignore).

## Loader contract (read before "fixing" a top-level `return`)

Each script is **not** a plain ES module. The Workflow runtime **wraps the file body in
an `async` function** before executing it and injects the orchestration globals
(`agent`, `parallel`, `pipeline`, `phase`, `log`, `args`, `budget`, `workflow`). So in
these files:

- a **top-level `return`** (to hand a value back to the orchestrator) is **intended and
  valid**, and
- **top-level `await`** is **intended and valid**.

Any checker that parses these as ESM — `node --input-type=module --check`, or the same file
renamed `.mjs` — **false-positives** with `SyntaxError: Illegal return statement` on the
top-level `return`; that does **not** mean the script is broken. (Plain `node --check
<file>.js` parses as CommonJS on Node 22, where the top-level `return` is legal, so it
catches real brace/token errors only. Empirical proof the `return` is intended: `orient`
and `review-wave` execute successfully with it.) **Do not delete the `return` to silence a
checker** — that breaks the script's contract with the orchestrator.

### Correct local syntax check

Validate by reproducing the runtime's wrapping, which exposes any *real* syntax error
(unbalanced braces, stray tokens) while allowing the legal top-level `return`/`await`:

```bash
for f in .claude/workflows/*.js; do
  { echo "async function __wf(args){"; sed 's/^export const /const /' "$f"; echo "}"; } > /tmp/_chk.mjs
  node --check /tmp/_chk.mjs && echo "OK $f" || echo "FAIL $f"
done
```

## Authoring conventions

- Begin with `export const meta = { name, description, whenToUse?, phases }` — a **pure
  literal** (no variables/among/calls). `name` is how the script is invoked.
- Prefer **named, multi-line schema consts** (e.g. an `obj(required, props)` helper) over
  dense one-line inline JSON Schema — the dense form is where brace-count bugs hide.
- `Date.now()` / `Math.random()` / argless `new Date()` are **unavailable** (they break
  resume) — vary agents by index, pass timestamps via `args`, stamp results after return.
- Read-only analysis workflows must say so and must not mutate; destructive actions stay
  with the orchestrator (e.g. `cleanup-sweep` returns lists, it never deletes).
- **Classes gate the work; the caller decides dispatch.** A lane classifies its own diff with
  [`scripts/classify.sh`](../../scripts/classify.sh) to learn which gates it owes; the
  orchestrator decides what to *dispatch* — an R0 prose item owes no review at all, R1/R2 one
  cross-vendor pass, R3 the 3-lens panel plus chaos/soak and operator approval. No script
  lowers a class. Contract: [engineering.md](../../docs/standards/engineering.md).

## Current scripts

| Script | Purpose |
| --- | --- |
| `orient.js` | Read-only state-of-the-world map (lanes, branches, PRs, collisions, board) → synthesis for PLAN. |
| `wave-fanout.js` | Run one wave of lane implementation across disjoint territories in isolated worktrees, each lane meeting the gates its class owes, returning committed work + opened PRs. |
| `review-wave.js` | Adversarial cross-vendor (Codex) review of diffs/PRs — the mandatory pre-merge gate for R1+; R3/high-risk → 3-lens panel; fail-closed on fallback. |
| `cleanup-sweep.js` | Read-only triage of branch/worktree sprawl → exact prune/remove/salvage lists for the orchestrator to execute. |
