---
name: orchestrate
description: Run the single-orchestrator "Conductor" loop — plan a wave of dependency-ready work, fan it out across disjoint file territories via workflows and agent teams, integrate as the sole integrator, gate each diff through cross-vendor (Codex) review, merge, clean up, record, and reschedule. Use when driving the Multiview backlog forward as one orchestrator instead of many independent terminals (ADR-G007).
---

# The Conductor loop

One long-lived orchestrator session owns the work loop (ADR-G007). It is the **sole
board writer, sole integrator, sole `memory` client, and owns every PR to merge +
cleanup**. It fans out broadly but never lets two concurrent lanes touch the same file
territory. The *why* is [ADR-G007](../../../docs/decisions/ADR-G007.md); this skill is
the *how*. Lane mechanics are the [`worktree-lane`](../worktree-lane/SKILL.md) skill;
recall/record is the [`memory`](../memory/SKILL.md) skill.

> **Golden rule:** collisions can't surface at merge if colliding lanes are never
> *assigned* at once. Disjoint territory per concurrent lane; **hot shared files are
> serial** (one owner). Everything else follows.

## One iteration (a "wave")

### ① PLAN
- Recall: `qdrant-find` the topic; read `docs/development/work-schedule.md` (Part 2
  checklist + Part 3 items; 400 KB — **search with `rg`, never read whole**) and
  `gh pr list --state open`.
- Pick the next set of **dependency-ready** items (`deps:` satisfied, status `[ ]`/`[~]`),
  each mappable to **one disjoint territory** (see table below). Cap the wave at the
  concurrency you can integrate + review well (start ~3–5 lanes).
- Any item touching a **serial hot file** (`pipeline.rs`, `engine/{runtime,clock,drive}.rs`,
  `control/{routes/mod,openapi,state}.rs`) goes to that file's single owner lane; other
  lanes in the wave file the body and hand wiring to the owner.

### ② ASSIGN
- Record each lane as `territory → item(s) → owner → class` on the board (you are the
  only writer). The class comes from `scripts/classify.sh` and decides which gates the
  lane owes; it is a floor you may raise, never lower. Note the **authoring vendor** per
  lane so REVIEW can pick a different one.

### ③ FAN OUT — two modes
- **Workflow mode** (`Workflow` tool; scripts in [`.claude/workflows/`](../../workflows/)):
  for sub-steps that are themselves decompose→verify→synthesize fan-outs. Reusable named
  workflows: `orient`, `wave-fanout`, `review-wave`, `cleanup-sweep`. Each agent that
  mutates files runs `isolation: 'worktree'`.
- **Team mode** (background `Agent` + shared `TaskList` + `SendMessage`): for lane-length
  stateful implementation. Each teammate owns exactly one territory's worktree, commits
  the failing test first (required at R2/R3, and at R1 for a behavioural change), and
  returns its commit SHA(s).
- **Every lane bases on current HEAD.** If a pre-existing lane is on a stale
  base (common — every in-flight lane on 2026-06-16 was), **rebase onto current `main`
  before integrating** or cherry-picks conflict.

### ④ INTEGRATE (sole integrator)
- `git log origin/main..<lane-HEAD>` to find **all** of a lane's commits; cherry-pick as
  **individual single commits**, not ranges.
- **Rebuild from a clean, isolated `target/` before trusting green** — a shared cache can
  link a sibling's stale artifacts and fake a pass. Never set `CARGO_TARGET_DIR` to `/tmp`
  (operator directive: per-lane `/tmp` targets once filled the disk with terabytes).
- **The local gate is class-scaled** (`scripts/classify.sh`;
  [engineering.md](../../../docs/standards/engineering.md) Part A). R0/R1 lean on CI plus
  the focused crate suite (`cargo test -p multiview-<crate>`); **R2/R3 run the full gate
  before the PR**: `cargo fmt --all -- --check`,
  `cargo clippy --locked --workspace --all-targets -- -D warnings`,
  `cargo test --locked --workspace`, `cargo deny check` if deps changed (+ `web/`
  lint/typecheck/build if `web/` changed).

### ⑤ REVIEW — adversarial, cross-vendor, fresh context
- **Depth is the class; the gate itself is fail-closed.** R0 skips review entirely. R1
  gets a single **diff-only** pass. R2 gets a scoped-context cross-vendor pass. R3 keeps
  the **3-lens panel**. Above R0 the review is mandatory, is never self-performed by the
  authoring vendor, and no lane merges without it.
- Dispatch the lane's diff to a **different vendor** than authored it, seeing **only**
  diff + spec/PLAN + the checklist — never the author's chat history. Default here:
  Claude-authored → **Codex** reviews. Invocation pattern:

  ```bash
  git diff origin/main...<lane-HEAD> > /tmp/review.diff
  codex exec --sandbox read-only \
    "Adversarial review for correctness/security/spec/guardrail defects ONLY (see \
     docs/development/agent-guardrails.md §C). Here is the diff and the item spec. \
     Report concrete defects with file:line; if none, name the single highest-residual \
     risk. Do not comment on style." < /tmp/review.diff
  ```
- **Codex must be authenticated** or `codex exec` fails 401 and `review-wave` records a
  labeled `claude-fallback` (fresh-context Claude, **not** cross-vendor). **Never merge on
  a fallback verdict** — hold the PR until Codex auth lands. Setup + verify:
  [codex-review runbook](../../../docs/runbooks/codex-review.md).
- Require **≥1 substantive risk statement** — unanimous bland approval is a yellow flag.
  A finding blocks the merge when it carries a reproduction, a failing test, or a cited
  line demonstrably violating a stated invariant; unreproducible findings are advisory
  ([engineering.md](../../../docs/standards/engineering.md) Part D). Never argue a
  reproduced finding away, and re-review only **the delta** after a fix.
- **R3 diffs** (invariant #1/#10 risk, keys, trust boundaries — authn/authz/egress/TLS,
  licence enforcement, legal or public action, release publishing) → **3-lens panel**,
  chaos/soak, a rehearsed recovery path, and explicit operator approval.

### ⑥ MERGE (ADR-G005)
- Merge only on **green deterministic checks** + a passing cross-vendor review. Report
  failures and skips honestly — a green summary over a skipped suite is a defect. Never
  `--admin`/bypass branch protection; never weaken, skip, `#[ignore]` or delete a test to
  go green — stop and ask the operator instead.

### ⑦ CLEAN
- `git worktree remove` the lane (+ `git worktree prune`), delete its branch, then
  `git fetch origin && git pull --ff-only origin main` in the root so the **next wave
  bases on current HEAD**. Never force-remove a `locked` worktree of a *live* session;
  if a `locked` worktree's owning pid is dead, salvage its WIP to a `salvage/*` branch
  first (see Salvage below).

### ⑧ RECORD
- Flip the board checkbox + set the Part-3 Status; add the red→green commit SHAs + PR
  number inline. `qdrant-store` every non-obvious decision, operator correction, and
  hard-won gotcha — proactively, not on request. Write/refresh the resource runbook in
  the **same** change that provisioned or altered the infrastructure. An ADR when the
  decision constrains future changes (required at R3), not for every wave.

### ⑨ RESCHEDULE
- `ScheduleWakeup` the next iteration (fully self-paced per operator directive
  2026-06-16). Keep the agenda durable on the board so a fresh wake can resume it.
  Operator can interrupt at any time and retains override (ADR-G005).

## Territory map (disjoint; refines work-schedule.md §1c)

Serial **one-owner-only** territories (never two concurrent lanes here):
- **LANE-CORE** — `multiview-cli/src/{pipeline,sink,run,control}.rs`,
  `multiview-engine/src/{runtime,drive,clock}.rs`, `multiview-events/src/event.rs`,
  `multiview-config/src/schema.rs`.
- **LANE-API** — `multiview-control/src/{routes/mod,openapi,openapi_schemas,asyncapi,state,lib}.rs`,
  `docs/api/openapi.json`, auth/session/RBAC.

One-owner territories (parallelizable across the wave):
LANE-WRTC (`multiview-webrtc/**`, `preview/src/whep*`, `control/routes/{whip,whep_serve}.rs`)
· LANE-IN (`multiview-input/**`, `multiview-rist-sys/**`) · LANE-PRV (`multiview-preview/**`
minus WHEP transport) · LANE-ENG (`multiview-engine/**` minus runtime/clock/drive;
`hal/src/load.rs`) · LANE-GPU (`multiview-compositor/**`, `multiview-framestore/**`,
`multiview-ffmpeg/**`, `hal/src/select.rs`) · LANE-AUDIO (`multiview-audio/**`,
`multiview-overlay/**`) · LANE-BCAST (`control/src/{nmos,is07}*`, `multiview-output/**`)
· LANE-WEB (`web/**`) · LANE-DEVICES (zowietek/display-kms/sync/cast/node-enroll,
`deploy/**`) · LANE-CONSPECT (`multiview-licence/**`, `multiview-mesh/**`) ·
LANE-GOV (`.claude/**`, `docs/{decisions,research,runbooks}`, `.github/workflows/**`).

When two items genuinely need the same territory in one wave, **serialize them under one
owner** — do not split the territory.

## Salvage (orphaned `locked` lane with a dead owning pid)

```bash
# work is preserved as a readable, recoverable branch before the worktree is removed
git -C <lane> add -A && git -C <lane> commit -m "wip(salvage): <what> — recovered by Conductor"
git branch salvage/<descriptive-name> $(git -C <lane> rev-parse HEAD)
git worktree unlock <lane> && git worktree remove --force <lane>
```
Then queue the salvage branch for rebase + completion in the owning territory's lane.

## Non-negotiables (never relax under self-pacing)

- Invariants **#1 (output-clock)** and **#10 (isolation)** are blocking for any
  engine/data-plane wave — a change that risks either is R3: stop, write a design note,
  add a chaos/soak test.
- **The class matrix binds every wave** ([engineering.md](../../../docs/standards/engineering.md)
  Parts A–C): each lane meets the gates its class owes, and the unconditional safety
  invariants (secrets, authorization, supply chain, injection, licensing) hold at every
  class. Autonomy is pace, never a lower bar.
- Confirm genuinely destructive/outward-facing actions with the operator (force-push
  `main`, delete infra, public release, external comms) — the loop does not do these
  silently.
