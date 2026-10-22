# Working in this monorepo (for agents)

Orientation for any coding agent working in Multiview: a 23-crate Cargo workspace under `crates/`,
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

## Build-dir hygiene — never fill `/tmp`

Per-lane build artifacts have filled `/tmp` with **terabytes** (operator directive, 2026-06-10;
the incident is written up in
[`devcontainer-cargo-target.md`](../runbooks/devcontainer-cargo-target.md) — loadavg 675 with ~627
processes stuck in D-state). The rules, for every agent and every parallel lane:

- **Do not set `CARGO_TARGET_DIR` to a `/tmp` path** — do not override it at all. Each worktree's
  `target/` is already isolated from every other lane, and it is deleted with the worktree.
- **Remove your worktree when the lane is done** (PR pushed, branch on the remote):
  `git worktree remove --force <path> && git worktree prune`, run from anywhere inside the repo.
  Orphaned worktrees and their `target/` dirs are the debt.
- If a `/tmp` scratch dir is genuinely unavoidable (non-cargo tooling), `rm -rf` it before finishing.
- **Sweep rule:** any `/tmp/*-target*` directory not written to in the last ~3 hours is orphaned —
  delete it. Check mtimes first; never delete a dir an active lane is writing.

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
