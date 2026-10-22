---
name: worktree-lane
description: Create, work in, integrate, and clean up an isolated git worktree lane. File-changing work at R1 and above happens in a lane — solo tasks and delegated subagents alike; the root checkout is a pristine mirror of main. Use at the start of any task that changes files (R0 prose batches may be edited in place).
---

# Worktree lane lifecycle

A "lane" is one unit of work: one worktree, one file territory, one deliverable
(a PR for solo work; a returned commit SHA for a delegated lane). The root
checkout is a pristine, current mirror of `main` — never edit or commit product
changes there.

**Lanes scale with the change class.** Run `scripts/classify.sh`; the contract is
[engineering.md](../../../docs/standards/engineering.md). R1 and above take a
lane. An R0 prose batch — documentation with no executable or spec effect — may
be edited in place and batched into one PR; a lane there is ceremony, not safety.

A WARN-ONLY `PreToolUse` hook (`.claude/hooks/enforce-worktree.mjs`) reminds you
when an edit targets the root checkout. It does **not** block (operator choice;
[ADR-G006](../../../docs/decisions/ADR-G006.md)) — discipline is on you. Lanes
under `.claude/worktrees/**` and `.worktrees/**` are compliant.

## 1. Create — always from current HEAD

```bash
# Derive the root checkout; never hard-code it — a pinned path rots when the clone moves.
ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
LANE=<short-task-name>
git -C "$ROOT" worktree add --detach ".claude/worktrees/$LANE" HEAD
# Solo (PR-bound) lanes name their branch immediately:
git -C "$ROOT/.claude/worktrees/$LANE" switch -c <type>/<topic>  # e.g. feat/ndi-egress
git -C "$ROOT/.claude/worktrees/$LANE" rev-parse HEAD            # record the base SHA
```

- `--detach` from `HEAD`, never from a branch name or an older SHA — a stale base
  produces cherry-pick conflicts at integration.
- `.claude/worktrees/` (the harness `EnterWorktree` default) is gitignored, as is
  `.worktrees/`. **Never** place a worktree in `/tmp`.
- The harness `EnterWorktree` tool creates an equivalent lane under
  `.claude/worktrees/` — either path is fine; this skill is the manual form.
- Do not launch a standalone session from inside a worktree expecting the memory
  MCP — the embedded qdrant store's single-process lock and `.mcp.json` point at
  the **root** checkout's `.memory/`.

## 2. Work — only inside the lane, absolute paths

- Every file the lane touches lives under `.claude/worktrees/$LANE/...`.
- **Build artifacts stay worktree-local. NEVER set `CARGO_TARGET_DIR` to a `/tmp`
  path** (operator directive 2026-06-10: per-lane `/tmp/*-target` dirs filled the
  disk with TiB of artifacts). Do not override `CARGO_TARGET_DIR` at all — each
  worktree's own `target/` is already isolated and is removed with the worktree.
  Cargo's registry/git caches are shared and safe (downloads stay fast).
- Commit in the lane with Conventional Commits + the AI co-author trailer
  (`Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`). **Test-first**: the
  failing test is committed on its own, before the implementation commit —
  required at R2/R3, and at R1 for a behavioural change.
- Git-hooks caveat: `.git/hooks` is shared across all worktrees. Keep the root
  checkout's tooling installed so any hook manager keeps resolving; CI is the
  authoritative gate either way.

## 3. Deliver

- **Solo lane:** push the branch, open the PR, own it to green CI + merge. A PR is
  not done when it is opened; routine merge is delegated to the agent
  ([ADR-G005](../../../docs/decisions/ADR-G005.md)) and never happens on a failing
  gate. Which gates the PR owes is the change's class — `scripts/classify.sh`.
- **Delegated lane:** report the final commit SHA (and base SHA) to the integrator.
  A lane that did not commit produced nothing integrable.

## 4. Integrate delegated lanes — pick by pick, in the integrator's lane

```bash
git -C "$ROOT/.claude/worktrees/$LANE" log --oneline <base-sha>..HEAD  # find ALL commits
git -C "$ROOT/.claude/worktrees/<integrator-lane>" cherry-pick <sha>   # one at a time
```

- List `base..HEAD` first — lanes sometimes make more commits than they report.
- Individual single-commit picks, not multi-commit ranges.
- After integrating, **rebuild from a clean worktree-local `target/`** and re-run
  the gate. A delegated lane's in-worktree green is not integration evidence: a
  shared/sibling build cache can link stale artefacts and fake a green run, so any
  binary offered as evidence is built from a clean, isolated `target/`.

## 5. Clean up — the lane dies with its work, root catches up

```bash
# Re-derive $ROOT here if this is a fresh shell (see §1) — never a literal path.
git -C "$ROOT" worktree remove --force ".claude/worktrees/$LANE"
git -C "$ROOT" worktree prune
# After a PR merge, the merging agent refreshes the root mirror:
git -C "$ROOT" fetch origin && git -C "$ROOT" pull --ff-only origin main
```

Run cleanup as soon as the lane's work is merged/integrated, so the next lane
bases on current HEAD. Sweep any orphaned worktrees idle for more than a few
hours: `git worktree list`. Never force-remove a worktree that is `locked` or
holds another active session's unmerged work — that disrupts a sibling lane.
