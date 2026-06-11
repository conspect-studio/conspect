# ADR-G003: Mandatory adversarial cross-vendor review in a fresh context; human is the final approver

- **Status:** Proposed
- **Area:** Engineering Guardrails
- **Date:** 2026-06-02
- **Source:** [agent-guardrails.md](../development/agent-guardrails.md)

## Decision

Require that code authored primarily by one vendor/model be reviewed by a different vendor (Claude ↔ OpenAI Codex ↔ Gemini) before a human approves the PR. The reviewer must run in a fresh session/subagent seeing only the diff, the spec/PLAN, and a scoped checklist — never the author's chat history. Claude-authored code: review with Codex via the official codex-plugin-cc (`/codex:adversarial-review --base main`); Codex/other-authored code: review with Claude's bundled `/code-review` + `/security-review`. High-risk diffs (auth, concurrency, data migration, money) get a 3-reviewer panel with coordinator synthesis. The reviewer brief is scoped to correctness/security/spec/guardrail defects only (no style/speculative), and is handed the typing + TDD checklist. Branch protection requires the deterministic checks plus ≥1 human approval; AI review never auto-merges.

## Rationale

Context separation alone measurably beats same-session self-review even with the same model (CCR arXiv 2603.12123: F1 28.6% vs 24.6%, p=0.008; +4.7 F1 on code; +11pp critical), and different vendors have less-correlated blind spots — both stack. A different vendor in fresh context is more likely to flag a guardrail the author's own model rationalized away. Anthropic, Cloudflare, and the human-in-the-loop literature all keep a human as final approver and bias AI review to advisory.

## Alternatives considered

Same-model self-review in the author's session (rejected: correlated blind spots + sycophancy/leniency, amplifies confidence without adding information). AI review as the merge gate (rejected: residual blind spots in TOCTOU/timing/authz; auto-merge removes the key safeguard). Unscoped "find all problems" prompt (rejected: manufactures findings → over-engineering).

## Consequences

Adds latency/cost — use risk-based tiering (light pass for trivial diffs, full panel for high-risk). Unanimous AI approval is a yellow flag (require ≥1 substantive risk statement). AI review does not cover race/timing/authz — those still need property/concurrency tests + human review. codex-plugin-cc command surface can drift — commands verified current as of 2026-06-02; re-verify before pinning.
