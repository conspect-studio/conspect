# Runbook — local persistent memory MCP (`memory` / mcp-server-qdrant)

## What it is and why

A repository-scoped, fully-local vector-RAG memory for coding-agent sessions:
decisions, operator feedback, gotchas, milestone state that future sessions must
recall. It is an embedded Qdrant DB + a local ONNX embedding model (FastEmbed) —
free, no API keys, no paid tier, nothing leaves the machine after the first model
fetch. It is the **repository-scoped, committed-config, team-shared** memory;
conventions live in the [`memory` skill](../../.claude/skills/memory/SKILL.md).
Decision: [ADR-G006](../decisions/ADR-G006.md).

## Resource identity

- **MCP server name:** `memory` (stdio), configured in [`/.mcp.json`](../../.mcp.json).
- **Server package:** `mcp-server-qdrant@0.8.1`, run via `uvx` (no global install).
- **Runner:** `uv` / `uvx` (Astral) at `~/.local/bin` — installed **0.11.21**, verified 2026-06-16.
- **Store path:** `.memory/qdrant` (collection `memory`) — a **relative** path the
  qdrant client resolves against the MCP server's CWD, which Claude Code sets to the
  project root. (Was `${CLAUDE_PROJECT_DIR}/.memory/qdrant`; see the substrate-fix
  note below for why that broke — ADR-G007.)
- **Embedding-model cache:** `.memory/fastembed_cache` (same relative-path rule).
- **Tools exposed:** `qdrant-find`, `qdrant-store` (descriptions overridden via
  `TOOL_FIND_DESCRIPTION` / `TOOL_STORE_DESCRIPTION` so the agent recalls/stores
  proactively).
- `.memory/` is gitignored — **never commit it**.

## Create / install (already done)

```bash
# 1. Install the uvx runner (idempotent; ~/.local/bin must be on PATH)
command -v uvx || curl -LsSf https://astral.sh/uv/install.sh | sh

# 2. Warm the package cache (downloads ~68 deps incl. onnxruntime/fastembed)
uvx mcp-server-qdrant@0.8.1 --help

# 3. .mcp.json + .claude/settings.json (enableAllProjectMcpServers: true) are
#    committed. The server loads on the next Claude Code session start.
```

The HuggingFace embedding model (~100 MB) downloads on the **first** `qdrant-store`
/`qdrant-find` call, then the server is fully offline. On a locked-down/offline box,
warm it once with network access first.

## Verify

1. Restart the Claude Code session (MCP servers load at startup).
2. `qdrant-store` a test entry, then `qdrant-find` for it — the find returns it:
   - store: `"Memory MCP verified on 2026-06-16."`, metadata `{"type":"reference","topic":"memory-mcp"}`
   - find: `"is the memory mcp working?"` → returns the entry.
3. `uvx --version` and `command -v uvx` resolve (PATH includes `~/.local/bin`).

## Rotate / recreate / restore / roll back

- **Recreate the store:** stop all sessions, `rm -rf .memory/qdrant`, restart — a
  fresh empty collection is created. (This discards all stored memories.)
- **Re-fetch the model:** `rm -rf .memory/fastembed_cache`; the next call
  re-downloads it.
- **Bump the server:** edit the `mcp-server-qdrant@<version>` pin in `.mcp.json`,
  `uvx mcp-server-qdrant@<version> --help` to warm, restart. Roll back by
  restoring the previous pin.
- **No delete tool** for individual memories: store a correcting entry stating both
  the old claim and the correction.

## Substrate fix — relative store path (2026-06-16, ADR-G007)

`.mcp.json` previously set `QDRANT_LOCAL_PATH=${CLAUDE_PROJECT_DIR}/.memory/qdrant`.
This Claude Code build (2.1.178) does **not** interpolate `${CLAUDE_PROJECT_DIR}` in
`.mcp.json` `env` values, so the qdrant client received the **literal** string and
created its store in a directory literally named `${CLAUDE_PROJECT_DIR}/` at the repo
root — 97 MB, **untracked and not gitignored** (at risk of being committed), while the
canonical `.memory/` never existed and `qdrant-find` returned empty. Fix: use a
**relative** path (`.memory/qdrant`), which the client resolves against the server's
CWD (the project root) with no variable expansion required. `.gitignore` now also
guards `/${CLAUDE_PROJECT_DIR}/` so the stray dir can never be committed.

**Migration (one-time, per clone that hit the bug):** with all sessions stopped (so
nothing holds the qdrant lock), copy any real collection data across and delete the
stray dir:

```bash
# stop every Claude Code session on this clone first (releases the qdrant lock)
mkdir -p .memory
[ -d '${CLAUDE_PROJECT_DIR}/.memory' ] && cp -an '${CLAUDE_PROJECT_DIR}/.memory/.' .memory/ || true
rm -rf '${CLAUDE_PROJECT_DIR}'      # the literal-named dir, NOT the env var
```

Then restart and run the Verify steps. The 97 MB is mostly the re-downloadable
FastEmbed model; the actual `memory` collection is small, so a clean re-init is also
acceptable if the copy is awkward.

## Gotchas

- **Single-process lock:** only one process per repo clone can hold the embedded
  qdrant store; a second concurrent `memory` server fails to connect (that is the
  lock, not corruption). Under the single-orchestrator model (ADR-G007) the
  **Conductor is the sole `memory` client**, so this contention does not arise in
  normal operation — but a stray second session (or a leftover process) will still
  block. If `qdrant-find` errors with a lock/connection failure, find and stop the
  other holder; do not delete the store.
- `~/.local/bin` must be on the PATH the Claude Code process sees, or the `uvx`
  command in `.mcp.json` fails to launch the server.
- The store path is **relative to the server CWD**. Claude Code launches stdio MCP
  servers with CWD = project root, so `.memory/qdrant` lands at the repo root. If a
  future host launches the server with a different CWD, the store would land
  elsewhere — pin an absolute repo path in `.mcp.json` for that host rather than
  reintroducing `${CLAUDE_PROJECT_DIR}` (which this client does not expand).
