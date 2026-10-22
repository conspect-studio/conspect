---
name: memory
description: Store and recall persistent project memory via the local qdrant memory MCP (qdrant-store / qdrant-find). Recall at the start of non-trivial tasks; store non-obvious decisions, operator feedback, and hard-won lessons when you learn them.
---

# Project memory conventions

The `memory` MCP server (`mcp-server-qdrant`, configured in
[`.mcp.json`](../../../.mcp.json)) provides two tools backed by a local vector
store under `.memory/` (gitignored). Runbook:
[`docs/runbooks/memory-mcp.md`](../../../docs/runbooks/memory-mcp.md).

- `qdrant-find` — semantic search; phrase the query as a natural-language question.
- `qdrant-store` — persist one memory (`information` + optional `metadata` JSON).

## Recall

At the start of any non-trivial task, run one or two `qdrant-find` queries about
the area you're touching (e.g. "decisions about the output clock", "gotchas
deploying NDI egress"). Do this before re-deriving anything a past session may
have settled, and alongside reading the area's brief/ADR and crate `CLAUDE.md`.

## Store

Store when you (a) make a non-trivial decision not worth a full ADR, (b) receive
operator feedback or a correction, (c) discover a gotcha that cost real time, or
(d) finish a milestone whose state a future session needs.

Entry format:

- `information`: 1–3 self-contained sentences, present tense, absolute dates
  (never "today"/"recently"). A future session sees only this text — include the
  why.
- `metadata`: `{"type": "project|feedback|user|reference", "topic": "<kebab-case>"}`.

## Do NOT store

- Secrets, tokens, keys — ever. They live in 1Password and never touch git, logs,
  terminal history or model-visible output; this store is none of those, and is
  still not where a credential goes.
- Anything already canonical in the repo: `AGENTS.md`/`CLAUDE.md`,
  `docs/standards/engineering.md`, `docs/architecture/conventions.md`, ADRs,
  briefs, or code. Repo files are the source of truth; memory is for what the repo
  doesn't record.
- Conversation-local trivia with no future value.

## Relationship to the other memory systems

- This MCP store is the **repository-scoped, committed-config, team-shared**
  memory. It is the primary store for project decisions/feedback/gotchas.
- Claude Code also keeps a per-user file-based memory under
  `~/.claude/.../memory/` (`MEMORY.md` index) that it manages automatically. That
  is complementary and personal; prefer the MCP store for anything a teammate or
  future session in this repo should see.

## Operational notes

- Local after first run: embedded Qdrant DB + ONNX embedding model under
  `.memory/`. The embedding model downloads from the HuggingFace Hub on first use
  only, then runs fully offline — nothing leaves the machine.
- Single-process lock: only one session per repo clone can use the store at a
  time. A second concurrent session's memory server fails to connect — that is the
  lock, not corruption. The lock belongs to the **root** session (`.mcp.json`
  points at the root checkout's `.memory/`), so worktree sessions don't get it.
- No delete tool: if a memory turns out wrong, store a correcting entry stating
  both the old claim and the correction.
