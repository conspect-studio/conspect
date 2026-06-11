# ADR-DC004: Install the 1Password CLI (op) in the image; authenticate via service-account token

- **Status:** Proposed
- **Area:** Dev Container
- **Date:** 2026-06-02
- **Source brief:** [devcontainer-design.md](../research/devcontainer-design.md)

## Decision

Install `1password-cli` at build time via the official 1Password apt repo, using `$(dpkg --print-architecture)` for the sources line and importing both the archive keyring and the debsig policy/keyring for signature verification. No secret is passed at build time. At runtime, `OP_SERVICE_ACCOUNT_TOKEN` (from .env) authenticates op non-interactively; app/tests read credentials with `op read "op://Vault/Item/field"`, preferring vault/item IDs.

## Rationale

Installing op at build time keeps it out of the secret path entirely (the token arrives only at runtime). `$(dpkg --print-architecture)` makes the same Dockerfile build on amd64 and arm64 (Apple Silicon Linux container). Service-account auth (op >= 2.18.0; current releases far exceed this) needs no interactive signin and no desktop-app socket (unavailable in containers). Using IDs over names reduces service-account rate-limit pressure.

## Alternatives considered

(a) Hardcode arch=amd64 — rejected: breaks on Apple Silicon arm64. (b) A third-party 1Password devcontainer Feature — viable but adds an external dependency; the official apt repo is authoritative and minimal. (c) 1Password Connect (OP_CONNECT_HOST/TOKEN) — rejected: overrides the service-account token and adds infra; explicitly warned against in .env.example. (d) Skip op, pass each secret via .env — rejected: doesn't satisfy the 'read other credentials via op' requirement.

## Consequences

Any developer/test with a valid token can fetch the team's 1Password secrets in-container without interactive login; CI and tokenless hosts degrade gracefully (op disabled, build/test still work). The token must have access to the specific vaults the tests need, and heavy `op read` usage can be throttled by service-account rate limits.
