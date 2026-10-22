# ADR-G006: Governance bootstrap — committed `.claude/` machinery, worktree-lane discipline, local memory MCP

- **Status:** Accepted
- **Area:** Engineering guardrails / governance
- **Date:** 2026-06-16
- **Source:** Operator request — install the standard monorepo governance/tooling layer ("issues with discipline, deployments, coding"); the repository-agnostic bootstrap brief, translated to this Rust workspace.

## Context

The repository already carried strong governance prose (`AGENTS.md`, `CLAUDE.md`,
`docs/development/agent-guardrails.md`, 169 ADRs, the 11 invariants in
`docs/architecture/conventions.md`) but the **enforcement and tooling layer** was
thin and partly aspirational:

- All of `.claude/` was gitignored as "local scratch", so skills, hooks, and
  shared harness settings could not be committed, shared, or enforced.
- There were **no skills and no hooks** wired (`.claude/skills/`, `.claude/hooks/`
  did not exist), despite the guardrails doc referencing a planned PreToolUse hook.
- Secret scanning was documented (gitleaks) but **not actually wired into CI**, and
  no local pre-commit/pre-push gate existed.
- Worktree-lane discipline lived only as prose in `CLAUDE.md` §4 and
  `working-in-this-monorepo.md`; nothing surfaced a reminder at edit time.
- Cross-session memory was the per-user file-based Claude memory only; there was no
  repository-scoped, committed, team-shared memory.

The operator directed: adopt the standard rule-set as authoritative, preserve all
Multiview-specific knowledge, remove contradictory existing rules, and install the
machinery — selecting **warn-only** worktree enforcement and **installing** the
local qdrant memory MCP.

## Decision

Install a committed governance/tooling layer, adapted to the Rust workspace:

1. **Commit the `.claude/` governance bits.** `.gitignore` now ignores
   `.claude/*` but re-includes `.claude/settings.json`, `.claude/skills/`, and
   `.claude/hooks/`; worktrees, sessions, locks, and `settings.local.json` stay
   ignored. `.memory/` and `.worktrees/` are gitignored.
2. **Skills** under `.claude/skills/`: `worktree-lane`, `adr` (targets
   `docs/decisions/`, not a new `docs/adr/`), `memory`.
3. **Warn-only worktree hook** `.claude/hooks/enforce-worktree.mjs` wired as a
   `PreToolUse` matcher in committed `.claude/settings.json`. It emits a
   non-blocking `systemMessage` reminder for edits to the root checkout and treats
   `.claude/worktrees/**` and `.worktrees/**` as compliant lanes. It never blocks
   (operator choice).
4. **Local memory MCP** (`mcp-server-qdrant`, run via `uvx`) configured in
   `.mcp.json` with paths under `.memory/` (gitignored); fully local, free/OSS,
   offline after the first embedding-model fetch. Conventions in the `memory`
   skill; provisioning runbook in `docs/runbooks/memory-mcp.md`.
5. **Secret scanning + local gate**: `.github/workflows/gitleaks.yml` (pinned
   MIT binary v8.30.1, checksum-verified — not the registration-gated action) and
   an optional `lefthook.yml` mirroring the CI gate. A curated `.gitleaks.toml`
   allowlists 12 verified false positives surfaced by the first full-history scan
   (synthetic keys in auth/licence/OpenAPI test fixtures; public `FFMPEG_GPG_KEY`/
   `FFMPEG_SHA256`/ghcr image pins in `deploy/Dockerfile*`) — each justified inline;
   no real secret was found in history.
6. **Docs scaffolding**: `docs/stack.md` (toolchain/platform standards),
   `docs/runbooks/` (operational how, distinct from `docs/operations/` guides),
   `docs/decisions/TEMPLATE.md`.
7. **Rule-set refactor**: `AGENTS.md` adopts the full standard working rules as
   authoritative while preserving every Multiview-specific section (invariants,
   crate map, feature flags, licensing, API/realtime/frontend conventions,
   concurrency, IPv6-first). `CLAUDE.md` imports `AGENTS.md`, adds the persistent
   memory section, and keeps the Claude-specific orientation. Contradictory legacy
   rule sentences were removed (e.g. "commit/push only when explicitly requested",
   superseded by the PR-ownership rules and ADR-G005).

## Rationale

Instruction prose is followed only some of the time; deterministic hooks/CI and
committed, discoverable skills raise the floor. Committing the governance bits is
the only way to make them team-shared and enforced (Claude Code loads skills/hooks
only from `.claude/`). Warn-only worktree enforcement matches the operator's
preference for low friction while still surfacing the rule at the point of action.
A repository-scoped semantic memory captures decisions/feedback/gotchas that the
per-user file memory cannot share across the team. Keeping ADRs in `docs/decisions/`
(not a new `docs/adr/`) avoids fragmenting the 169-ADR corpus.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Keep `.claude/` fully gitignored; install skills/hooks locally only | Not committed, shared, or enforced for teammates/CI — defeats the purpose. |
| Hard-block worktree hook (exit 2) | Operator selected warn-only; a hard block adds friction during quick edits and the bootstrap itself. |
| New `docs/adr/` directory per the generic brief | Fragments the existing 169-ADR `docs/decisions/` corpus and its README/prefix taxonomy. |
| Replace `AGENTS.md` with the generic 42-rule file verbatim | Would lose Multiview-specific invariants, crate map, licensing, and IPv6-first knowledge. |
| Keep the per-user file memory only | No repository-scoped, committed, team-shared recall. |
| Duplicate CI gate/supply-chain workflows from the brief | `ci.yml` already runs fmt/clippy/test, cargo-deny (licenses+advisories), inclusive-language, docs-sanity, and mutation testing — only gitleaks + a local gate were genuinely missing. |

## Consequences

- Editing the root checkout now produces a warn-only reminder once the session
  reloads `.claude/settings.json`; all file-changing work should move into lanes.
- A new local dependency (`uv`/`uvx`) is required for the memory MCP; first use
  downloads a ~100 MB embedding model, then runs offline. The store is
  single-process (one session per clone).
- CI gains a gitleaks job (secret scan on every PR/push). Contributors are
  encouraged but not required to install the `lefthook` local gate.
- The governance rules in `AGENTS.md`/`CLAUDE.md` are now authoritative and
  internally consistent; this ADR does **not** touch the technical invariants
  (#1 output-clock, #10 isolation, …) which remain governed by
  `conventions.md` §5. ADR-G005 (agent owns routine review→merge) stays in force.
