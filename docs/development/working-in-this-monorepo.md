# Working in this monorepo (for agents)

Orientation for any coding agent working in Conspect: a 23-crate Cargo workspace under `crates/`,
a React/TypeScript SPA under `web/`, dev automation in `xtask/`, and a large `docs/` tree
(architecture, 56 research briefs, 236 ADRs). This page is **how to move through it without
drowning your context window**, plus the operational hazards that have actually bitten here.

Which gates a change owes is not here — that is
[`docs/standards/engineering.md`](../standards/engineering.md), and it scales by class.

## Precedence, always

**[`conventions.md`](../architecture/conventions.md) → the Rust code → briefs/ADRs → this guide.**
Where a brief disagrees with the code, the code wins — flag the drift.

## The docs map

| You want… | Go to |
|-----------|-------|
| Canonical names, paths, feature flags, invariants, licensing | [`conventions.md`](../architecture/conventions.md) — **source of truth** |
| A one-screen map of the whole repo | [`codebase-map.md`](codebase-map.md) |
| Which gates this change owes | [`engineering.md`](../standards/engineering.md) + `scripts/classify.sh` |
| The engine/FFI safety rules (stable §1–§8 anchors) | [`data-plane-safety.md`](../architecture/data-plane-safety.md) |
| Lint traps, mutation exit codes, tool-version gotchas | [`agent-guardrails.md`](agent-guardrails.md) |
| *Why* a subsystem is built this way | the matching brief in [`docs/research/`](../research/README.md) |
| A specific decision + its alternatives | the ADR in [`docs/decisions/`](../decisions/README.md) |
| Conventions for the crate I'm in | that crate's `CLAUDE.md` (loads on demand) |

## Context discipline

Context is the fundamental constraint in a 23-crate workspace, and it is a **quality** constraint
before it is a cost one: oversized context buries the instructions that matter. Read a file because
a specific question requires it, not as precaution.

- **Work one crate/area at a time**; `/clear` between unrelated tasks.
- **Read a brief when the change needs its reasoning**, not as a standing toll on every edit. When
  you do need one, send a subagent and keep its conclusions, not its transcript.
- **Start Claude from the crate you are working in.** From `crates/multiview-input/` you get the
  root files plus `crates/multiview-input/CLAUDE.md`, and nothing from the other 22 crates.
- **Never open generated or build output.** Searches respect `.gitignore`, so `target/`,
  `node_modules/`, `dist/` and `.multiview-build/` stay out of results — do not open them manually
  (they are also read-denied in `.claude/settings.json`).
- Keep instruction files lean — under ~200 lines each — so adherence stays high.

### How the instruction files layer

- **Root [`AGENTS.md`](../../AGENTS.md)** — the router: invariants, commands, classes, where things
  are. Tool-agnostic. **Root [`CLAUDE.md`](../../CLAUDE.md)** imports it with `@AGENTS.md` and adds
  only Claude-Code specifics. Both are re-injected from disk after `/compact`.
- **Per-crate `CLAUDE.md`** (`crates/<crate>/CLAUDE.md`, `web/CLAUDE.md`, `docs/runbooks/CLAUDE.md`)
  — short and crate-scoped: what this area is, its load-bearing invariants, and the exact
  brief(s) + ADRs to read first. A subdirectory `CLAUDE.md` **loads the moment Claude reads a file
  in that directory**, so you pay context only for the crate you are in. This on-demand layer is
  where subsystem routing lives; the root files deliberately do not duplicate it.
- **Skills** (`.claude/skills/<name>/SKILL.md`) load only when invoked — for multi-step *procedures*,
  not orientation.

**After `/compact`:** the root files are re-injected from disk, but **nested crate `CLAUDE.md` files
are NOT** — they reload the next time Claude reads a file in that crate. If a crate rule seems to
disappear mid-session after a compaction, read any file in that crate to reload it.

### Use subagents for fan-out

The biggest context win in this repo. Spawn a subagent when a side task would flood your main
conversation with file contents you will not reference again — it runs in its own context window and
returns only the summary. Good uses here:

- "Read `streaming-gotchas.md` §1–§3 and summarize the PTS-normalization rules" before editing
  `multiview-input`.
- "Find every place `out_pts` / the tick counter is computed across the workspace."
- "Review this diff against invariants #1 and #10 and report violations with ADR refs."

**Tell every subagent its lane path explicitly, and have it `cd` there in every Bash call.** A
background agent's shell can otherwise execute against the root checkout while its file reads go to
the lane — it will then misreport git state with total confidence.

## Navigation — ripgrep + the crate map

```bash
rg -n "out_pts|tick"                 # the output-clock timing logic
rg -n "trait Source|trait Sink"      # stage trait definitions (multiview-core)
rg --type rust -l "AVHWFramesContext" crates/multiview-ffmpeg   # FFI hwframe lifecycle
rg -n "ADR-T003" docs/               # everywhere a decision is referenced
fd CLAUDE.md crates web              # list all nested agent docs
```

Crate map and dependency direction: [`codebase-map.md`](codebase-map.md) and
[`conventions.md` §3](../architecture/conventions.md) — **`core` ← everything; no cycles.** Knowing
the direction tells you which crate a change belongs in before you start reading.

A Rust language-server plugin replaces many file reads with jump-to-definition across 23 crates;
install with `/plugin install rust-lsp@claude-plugins-official` (needs `rust-analyzer` present).

## Workspace disk hygiene

**Each agent owns the disk footprint of its task, including delegated lanes.** Isolation is not
permission to retain unlimited caches. In November 2026 this checkout reached 875 GiB across
132 worktrees; 67 idle Rust build caches alone consumed 652 GiB. Keep disk use bounded throughout
the task, not just after an ENOSPC failure.

### Before work and before each large build or fan-out

- Work in a task-specific Git worktree; never edit or build in the root checkout. Integrate through
  a reviewed omnibus PR. Only sync the root after merge, without overwriting existing changes.
- Check `df -h .` and `df -i .`, then inspect `git worktree list --porcelain` and the size of your
  own worktree, `target/`, dependency caches and scratch outputs with `du -sh <exact-path>`.
- Estimate and record peak additional bytes and inodes for **all** concurrent lanes in the PR or
  task handoff, not only final artifact size. Require free bytes to be at least
  `max(20% of filesystem capacity, estimated peak additional bytes + 50 GiB)`, and free inodes
  to cover estimated peak additional inodes plus 20% of total inodes (where inode limits apply).
  Otherwise do not start another large build, dependency install or fan-out. Reclaim verified
  disposable task artifacts, reduce concurrency, or arrange capacity first. Recheck during long
  runs and between waves.
- The local push gate (`bin/check`) enforces its own hard floor, `MIN_FREE` = 150 GiB free, and a
  15 GiB cap on its own targets. That floor is deliberately lower than the planning rule above
  (owner decision, 2026-11-11; lowered again from 300 to 150 GiB on 2026-11-13, when free space sat at 277-313 GiB for hours and blocked every gate while each gate needs about 15 GiB): the build host disk is shared with other repositories, whose
  activity moved free space by more than 60 GiB within an hour and aborted gates mid-run. The
  planning rule still applies before any fan-out or several concurrent build lanes; run gates
  one at a time.
- Keep Cargo's per-worktree `target/` isolation. Never override `CARGO_TARGET_DIR`, share a sibling's
  cache, or move build output to `/tmp` or another filesystem to evade the capacity check. See
  [`devcontainer-cargo-target.md`](../runbooks/devcontainer-cargo-target.md) for the I/O-storm history.
- Reuse your current lane for successive steps rather than create a checkout for every command.
  Bound parallel build lanes to available disk and memory. Documentation-only changes need no
  Rust or web build artifacts. Keep logs, recordings, coverage and test output bounded too.

### During work and at handoff

- Stop your own completed background jobs and release disposable build outputs before the next
  wave. Do not remove an artifact still needed for review, integration, deployment or evidence.
- A paused task must state its worktree owner, reason for retention, retained size and cleanup
  condition in the PR or handoff. A pushed branch is not, by itself, permission to delete a lane.
- Before removal, resolve and verify the exact absolute path. Check owner/session and process
  activity, worktree locks and nested worktrees. Run
  `git status --short --ignored --untracked-files=all` inside the lane to list ignored and untracked
  paths together. A collapsed `!! <dir>/` line remains unclassified, never disposable; inventory
  its contents before any deletion. Do not read secret contents.
- Preserve uncommitted, untracked and non-rebuildable ignored data, open-PR work and unique commits.
  Preserve detached commits with a durable branch ref before removing their checkout. A clean Git
  status, old mtime, dead PID or merged branch alone does **not** prove a directory disposable.
- Coordinate with the owner before touching a sibling lane or shared cache. Never use a broad
  `git clean -fdx`, recursive workspace deletion, Docker/volume prune, toolchain purge or age-only
  `/tmp` sweep as a shortcut. Protect scratch data and unrelated sessions.

### After merge or explicit abandonment

Explicit abandonment requires a recorded owner decision and durable refs for every unmerged commit;
a handoff alone is not abandonment. Before a root sync, inspect root tracked, untracked and ignored
paths without reading secrets. Refuse the update if any local work lacks an explicit preservation
plan. Use a safe sync that preserves all local paths and commits; abort on collision. Never reset,
clean, stash or force the root to make a sync pass. `pull --ff-only` alone is not a preservation plan.

1. Verify the merged commit on `origin/main`, or record a deliberate handoff for any unmerged work.
2. Stop your own processes and leave the worktree before removal. Recheck the path and protections
   above immediately before deletion; do not rely on an old inventory.
3. Inventory **all ignored paths** before removal, including nested directories; do not infer
   safety from a clean tracked status. Positively classify each as rebuildable or preserve it
   through an owner-approved secure handoff. Refuse removal while any path is unclassified.
   Plain `git worktree remove` deletes ignored data without warning, even without `--force`.
4. Remove only verified disposable outputs; never follow symlinks or cross mount boundaries.
   Before a Cargo clean, use `env -u CARGO_TARGET_DIR cargo metadata --no-deps --format-version 1`
   in the inactive lane and verify `target_directory` resolves exactly to `<lane>/target`, not a
   root/shared/mounted cache. Only then use `env -u CARGO_TARGET_DIR cargo clean` there. Remove
   only your disposable dependency installs, logs and scratch files. Never clean the root cache.
5. Refuse removal if nested worktrees remain. Remove your finished checkout with
   `git worktree remove <verified-absolute-path>` from outside it, only after the inventory and
   preservation above. Do not use `--force` to bypass dirty/untracked data or a lock. Retain
   branches until merge or durable handoff is verified; do not force-delete unique commits.
6. Check `git worktree list --porcelain`, `df -h .` and `df -i .` again. Report reclaimed space and
   any retained lane, its owner and cleanup condition. Cleanup is part of completion, not optional
   follow-up. If safe cleanup cannot restore headroom, report the capacity blocker instead of
   starting more disk-heavy work.

## Build-cache honesty

A build cache shared across worktrees can link a **sibling's stale artefacts and fake a green run**.
Any binary you offer as evidence must be built from a clean, isolated `target/`; after integrating
cherry-picks, rebuild fresh. A delegated lane's in-worktree green is not integration evidence.

## Process safety

Prefer surgical, targeted commands. **Never kill processes broadly by port**; never disrupt shared
infrastructure — containers, other sessions, sibling worktrees. When stopping a service, use its own
stop mechanism.

## References

- [Set up Claude Code in a monorepo or large codebase](https://code.claude.com/docs/en/large-codebases)
- [How Claude remembers your project (CLAUDE.md, nested files, @-imports, rules)](https://code.claude.com/docs/en/memory)
- [Subagents](https://code.claude.com/docs/en/subagents) · [Context window / what survives compaction](https://code.claude.com/docs/en/context-window)
