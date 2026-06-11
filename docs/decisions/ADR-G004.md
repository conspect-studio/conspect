# ADR-G004: Scope discipline, no-silent-suppression, secrets, and supply-chain guardrails for agents

- **Status:** Proposed
- **Area:** Engineering Guardrails
- **Date:** 2026-06-02
- **Source:** [agent-guardrails.md](../development/agent-guardrails.md)

## Decision

Mandate: explore→plan→implement→commit (plan/spec for multi-file or ambiguous work; skip only for one-sentence diffs); minimal diffs where every changed line traces to the request, with a stated out-of-scope boundary. No silent suppression — disabling/weakening any lint, test, or type-check requires an inline justification and is a reviewable event; fix root cause not symptom. Show evidence (command + output) not assertions. Determinism: commit Cargo.lock + JS lockfile, build `--locked`/`npm ci`, pin toolchain/Node, no floating versions. Secrets: never commit/echo; use the 1Password op flow (read→chmod 600 temp→rm -f, or op ssh-agent); layered gitleaks (pre-commit `gitleaks git --pre-commit --staged --redact` + CI). Supply chain: `cargo deny check` via cargo-deny-action@v2 (already wired) plus cargo audit/vet; combine pinning with auditing + provenance. Run agents sandboxed/least-privilege; beware indirect prompt injection.

## Rationale

Autonomy amplifies bad habits: scope creep, symptom-suppression, leaked secrets, unpinned deps. Cargo ignores the lockfile without --locked, so non-deterministic CI is the default failure. AI-generated code is a documented secret-leak and vulnerability risk, so layered scanning + a different model reviewing the diff is the defense. Verified: gitleaks `protect`/`detect` are deprecated/hidden since v8.19.0 — use `gitleaks git --pre-commit`; cargo-deny's four checks (advisories/bans/licenses/sources) and `@v2` (not `@v1`) confirmed.

## Alternatives considered

Trusting agents to self-limit scope (rejected: 400-line diffs for 20-line asks). Allowing inline suppressions silently (rejected: hides the very bugs lints catch). Pinning deps only (rejected: 'pinning is futile' — a pinned-malicious version is still malicious; combine with audit + provenance). Committing secrets to .env in-repo (rejected: 12M+ secrets/year leak to public GitHub).

## Consequences

Slightly more process overhead per PR. Pin cargo-deny-action to a tag/SHA, not a moving major. gitleaks subcommands/flags drift between versions — verify against the installed release before scripting. AGENTS.md/CLAUDE.md must stay concise (bloat causes agents to ignore instructions), so deterministic must-happen actions live in hooks/CI, not prose.
