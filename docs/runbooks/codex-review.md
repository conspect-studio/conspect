# Runbook — cross-vendor review via the `codex` CLI (rule 21)

## What it is and why

Rule 21 requires that code authored by one vendor be reviewed by a **different**
vendor in a fresh context. Under the single-orchestrator model (Claude is the
author), the second vendor is **OpenAI Codex** via the `codex` CLI, driven by the
[`review-wave`](../../.claude/workflows/review-wave.js) workflow and the
[`orchestrate`](../../.claude/skills/orchestrate/SKILL.md) skill (REVIEW step).
Decision: [ADR-G007](../decisions/ADR-G007.md); review standard:
[agent-guardrails §C](../development/agent-guardrails.md).

## Resource identity

- **CLI:** `codex` (codex-cli **0.140.0**, verified 2026-06-16), at
  `/usr/local/share/nvm/.../bin/codex`.
- **Non-interactive form:** `codex exec --sandbox read-only "<prompt>"` (diff is read
  from a file the prompt names, or piped on stdin).
- **Built-in reviewer:** `codex exec review` also exists; `review-wave` uses a plain
  `exec` with the adversarial-review prompt + the §C checklist so the scope matches.

## Auth — REQUIRED (current blocker, 2026-06-16)

`codex exec` calls the OpenAI Responses API and **fails closed without credentials**:

```
ERROR: unexpected status 401 Unauthorized: Missing bearer or basic authentication
```

Until auth is provided, the cross-vendor gate cannot run and `review-wave` records a
**labeled fallback** (`reviewer: claude-fallback`, `ranOk: false`) — which is a
fresh-context Claude review, **not** cross-vendor. **Do not merge on a fallback
verdict** (ADR-G005 gates merge on a *passing cross-vendor* review); hold the PR.

Provide auth one of two ways (operator action — credentials are 1Password-gated,
rule 34; never echo/commit the key):

```bash
# A) interactive sign-in (ChatGPT plan or API), persists to ~/.codex/
codex login            # run via the session `! codex login` so output lands here

# B) API key in the environment the Claude Code process sees
export OPENAI_API_KEY="$(op read 'op://<vault>/<item>/api_key')"   # 600 temp / op env
```

## Verify

```bash
timeout 90 codex exec --sandbox read-only "Reply with exactly the token CODEX_OK." 
# expect: CODEX_OK   (a 401 here means auth is still missing)
```

Then run the gate end-to-end on an open PR:
`Workflow({ name: 'review-wave', args: { items: [ { id:'pr-170', ref:'170', spec:'…', highRisk:true } ] } })`
and confirm the returned verdict has `reviewer: "codex"` / `ranOk: true`.

## Rotate / recreate / restore / roll back

- **Rotate the key:** mint a replacement in 1Password, update the env/`~/.codex`
  auth, re-run Verify, revoke the old.
- **Add Gemini as a third vendor:** install its CLI and extend `review-wave` to pick
  the reviewer vendor ≠ author; the 3-reviewer high-risk panel then spans 3 vendors.
- **CLI rollback:** pin/install the prior `codex-cli` version; the workflow prompt
  discovers the invocation via `codex exec --help`, so minor CLI changes are absorbed.
