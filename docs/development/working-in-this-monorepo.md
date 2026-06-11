# Working in this monorepo (for agents)

This is the orientation guide for Claude Code and any coding agent working in the Multiview
repository. Multiview is a complex greenfield monorepo: a 16-crate Cargo workspace under
`crates/`, a React/TypeScript SPA under `web/`, dev automation in `xtask/`, and a large
`docs/` tree (architecture, 10 research briefs, 89 ADRs). This page tells you **how to move
through it efficiently without drowning your context window** — applying the official Claude
Code guidance for large/complex codebases:
[Set up Claude Code in a monorepo or large codebase](https://code.claude.com/docs/en/large-codebases)
and [How Claude remembers your project](https://code.claude.com/docs/en/memory).

## TL;DR — the five habits

1. **Start Claude from the crate you're working in**, not always the repo root. From
   `crates/multiview-input/` you get the root `CLAUDE.md` **plus** `crates/multiview-input/CLAUDE.md`,
   and *nothing* from the other 15 crates. ([why](https://code.claude.com/docs/en/large-codebases#choose-where-to-start-claude))
2. **Read the brief before you touch the subsystem.** Each crate's nested `CLAUDE.md` names the
   exact research brief(s) and ADRs to read first. The briefs are verification-hardened — they
   capture footguns you will otherwise rediscover the hard way.
3. **One crate / one area per task.** Finish, then `/clear` before the next. A fresh context
   window per task keeps adherence high.
4. **Fan out searches into subagents.** When a question needs reading many files (where is a
   symbol used, what does this brief say, does this diff break an invariant), delegate to a
   subagent so the file reads stay out of your main context and you get back only the summary.
   ([why](https://code.claude.com/docs/en/best-practices#use-subagents-for-investigation))
5. **Navigate with `rg` and the crate map, not exhaustive reads.** See
   [`docs/development/codebase-map.md`](codebase-map.md).

## The docs map — where to start

| You want… | Go to |
|-----------|-------|
| Canonical names, paths, feature flags, invariants, licensing | [`docs/architecture/conventions.md`](../architecture/conventions.md) — **source of truth** |
| A one-screen map of the whole repo | [`docs/development/codebase-map.md`](codebase-map.md) |
| Repo-wide rules, the 11 invariants, crate map, build commands | root [`CLAUDE.md`](../../CLAUDE.md) |
| Tool-agnostic agent contract (for non-Claude agents) | [`AGENTS.md`](../../AGENTS.md) |
| *Why* a subsystem is built this way | the matching brief in [`docs/research/`](../research/README.md) |
| A specific decision + its alternatives | the ADR in [`docs/decisions/`](../decisions/README.md) |
| Conventions/invariants for the crate I'm in | that crate's `CLAUDE.md` (loads on demand) |

Precedence, always: **conventions.md → the Rust code → briefs/ADRs → this guide.** Where a
brief disagrees with the code, the code wins; flag the drift.

## How CLAUDE.md / AGENTS.md are organized here

Claude Code reads `CLAUDE.md`, not `AGENTS.md`
([docs](https://code.claude.com/docs/en/memory#agents-md)). In this repo they are deliberately
**separate, non-duplicated** files:

- **Root `CLAUDE.md`** — repo-wide rules, the 11 canonical invariants, the crate map, build/test
  commands, safety rules. Loaded at launch from any starting directory; survives `/compact`
  (re-injected from disk).
- **`AGENTS.md`** — the tool-agnostic contract for any agent/new contributor. CLAUDE.md is its
  Claude-specific companion. Do not merge them.
- **Per-crate `CLAUDE.md`** (`crates/<crate>/CLAUDE.md`) and **`web/CLAUDE.md`** — short,
  crate-scoped: what this area is, its load-bearing invariants, and the exact brief(s)+ADRs to
  read first. Per the docs, a subdirectory `CLAUDE.md` **loads on demand the moment Claude reads
  a file in that directory** — so you pay context only for the crate you're actually in.
  ([mechanism](https://code.claude.com/docs/en/large-codebases#layer-claude-md-files-by-directory))

### Why nested CLAUDE.md (not `.claude/rules/` or skills) here

- **Nested `CLAUDE.md`** loads automatically when you read a file in that crate — zero ceremony,
  versioned next to the code, owned by whoever owns the crate. Best fit for "orient me to this
  crate." This is what we added.
- `.claude/rules/*.md` (with a `paths:` glob in frontmatter) load when Claude touches matching
  files anywhere. Useful later for cross-cutting rules that span scattered paths (e.g. a color
  pipeline rule over both `multiview-compositor` and `multiview-ffmpeg`). Optional follow-up.
- **Skills** (`.claude/skills/<name>/SKILL.md`) load only when invoked — for multi-step
  *procedures* (run the invariant audit, generate the OpenAPI client), not for orientation.
  ([compare](https://code.claude.com/docs/en/large-codebases#choose-between-per-directory-claude-md-and-path-scoped-rules))

We chose nested `CLAUDE.md` as the primary mechanism because the highest-value need is
*per-subsystem orientation that loads automatically* — exactly what the large-codebases guide
recommends as the starting point.

### Note on `/compact`

Root `CLAUDE.md` is re-read from disk and re-injected after `/compact`. **Nested crate
`CLAUDE.md` files are NOT auto-re-injected** — they reload the next time Claude reads a file in
that crate. If a crate rule "disappears" mid-session after a compaction, read any file in that
crate to reload it.
([docs](https://code.claude.com/docs/en/memory#instructions-seem-lost-after-compact))

## Context discipline

Context is the fundamental constraint in a 16-crate workspace. Treat it as a budget:

- **Work one crate/area at a time**; `/clear` between unrelated tasks.
- **Don't read whole briefs into the main thread** to answer a narrow question. Send a subagent.
- **Don't read generated/build output.** Searches respect `.gitignore`, so `target/`,
  `node_modules/`, `.multiview-build/` stay out of results. Don't open them manually.
- **Prefer `rg` over broad file reads** to locate symbols/usages (see Navigation below).
- Per the docs, keep instruction files lean: target **under ~200 lines per CLAUDE.md** so
  adherence stays high. The per-crate files are intentionally a few dozen lines each.
  ([why](https://code.claude.com/docs/en/memory#write-effective-instructions))

## Build-dir hygiene — never fill /tmp

Per-lane build artifacts have filled `/tmp` with terabytes. The rules, for every agent and
every parallel lane:

- **Do not set `CARGO_TARGET_DIR` to a `/tmp` path.** Use the worktree-local default `target/`
  (i.e. don't override it at all). Each worktree's `target/` is already isolated from every
  other lane — the same isolation a `/tmp` dir gave — and it is deleted automatically with the
  worktree.
- **Remove your worktree when the lane is done** (the PR is pushed; the branch lives on the
  remote): `git -C /workspaces/mosaic worktree remove --force <path>`. Orphaned worktrees and
  their `target/` dirs are the debt.
- If a `/tmp` scratch dir is genuinely unavoidable (non-cargo tooling), `rm -rf` it before
  finishing.
- Sweep rule: any `/tmp/*-target*` directory not written to in the last ~3 hours is orphaned —
  delete it (check mtimes first; never delete a dir an active lane is writing).

### Use subagents for fan-out

The biggest context win in this repo. Spawn a subagent when a side task would flood your main
conversation with file contents you won't reference again — it runs in its own context window
and returns only the summary
([docs](https://code.claude.com/docs/en/subagents)). Good uses here:

- "Read `streaming-gotchas.md` §1–§3 and summarize the PTS-normalization rules" before editing
  `multiview-input`.
- "Find every place `out_pts` / the tick counter is computed across the workspace."
- "Review this diff against invariants #1 and #10 and report violations with ADR refs."

## Navigation — ripgrep + the crate map

The repo is large; use targeted search, not tree-walking.

```bash
rg -n "out_pts|tick"                 # find the output-clock timing logic
rg -n "trait Source|trait Sink"      # locate stage trait definitions (multiview-core)
rg --type rust -l "AVHWFramesContext" crates/multiview-ffmpeg   # FFI hwframe lifecycle
rg -n "ADR-T003" docs/                # everywhere a decision is referenced
fd CLAUDE.md crates web              # list all nested agent docs
```

Crate map and dependency direction live in the root `CLAUDE.md` §3 and in
[`codebase-map.md`](codebase-map.md): **`core` ← everything; no cycles.** Knowing the direction
tells you which crate a change belongs in before you start reading.

A **Rust code-intelligence plugin** (language server) is the recommended next step for symbol
navigation across 16 crates — it replaces many file reads with jump-to-definition / find-
references. Install with `/plugin install rust-lsp@claude-plugins-official` (requires
`rust-analyzer` on the machine), or enable it repo-wide via the `enabledPlugins` setting.
([docs](https://code.claude.com/docs/en/large-codebases#reduce-file-reads-with-code-intelligence))

## The "read the brief before touching subsystem X" workflow

This is the single most important workflow in this repo:

1. Identify the crate (root `CLAUDE.md` §3 / the crate map).
2. Open that crate's `CLAUDE.md` (or just start Claude there) — it names the brief(s) + ADRs.
3. **Have a subagent read the brief and the ADRs** and report the invariants and footguns.
4. Re-check the relevant invariant(s) in root `CLAUDE.md` §2 (especially **#1 output-clock**
   and **#10 isolation** — if a change risks either, stop and write a design note).
5. Implement in that one crate; keep `cargo check --workspace` green GPU-free.
6. Before proposing a PR: `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and
   `cargo deny check` if deps changed.

The per-crate `CLAUDE.md` files are the entry point to step 2 for every subsystem. Skipping the
brief is how invariants get broken.

## References

- [Set up Claude Code in a monorepo or large codebase](https://code.claude.com/docs/en/large-codebases)
- [How Claude remembers your project (CLAUDE.md, nested files, @-imports, rules)](https://code.claude.com/docs/en/memory)
- [Subagents](https://code.claude.com/docs/en/subagents) · [Best practices: use subagents for investigation](https://code.claude.com/docs/en/best-practices#use-subagents-for-investigation)
- [Skills](https://code.claude.com/docs/en/skills) · [Context window / what survives compaction](https://code.claude.com/docs/en/context-window)
