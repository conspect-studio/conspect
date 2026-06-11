# ADR-W014: Control-plane access — bootstrap admin token from the environment

- **Status:** Accepted
- **Area:** Web/API stack · control-plane authentication
- **Date:** 2026-06-04
- **Source:** builds on ADR-W005 (API-key + RBAC auth); [web-api-stack](../research/web-api-stack.md)

## Decision

When `multiview run` serves the control plane (ADR-W013), it provisions a single bootstrap **admin** API key so the management API + web UI are usable out of the box without shipping a secret in the repo or config file:

- The admin **secret** is read from the `MULTIVIEW_CONTROL_TOKEN` environment variable (12-factor; never committed). It is registered as the `admin` key (`Role::Admin`) and is **stable across restarts**, so a token saved in the browser keeps working. The presented bearer token is `admin.<secret>`.
- If `MULTIVIEW_CONTROL_TOKEN` is unset, a cryptographically-random secret is generated and the full token `admin.<secret>` is **logged once** at startup (the Grafana/Jenkins first-run pattern) for first access. It regenerates each start until a stable secret is configured.

`provision_admin_keys(admin_secret: Option<String>) -> (ApiKeyStore, Option<String>)` is the pure, unit-tested core (returns the store and, when generated, the token to surface). The unauthenticated surface stays minimal and intentional: `/docs`, `/api/v1/openapi.json`, and the embedded web-UI shell are reachable so an operator can load the UI and read the API contract; **every `/api/v1` data route requires the admin token** (401 otherwise). Keys are stored only as HMAC-SHA256 digests under a per-process random pepper (ADR-W005), never in plaintext.

## Rationale

Secure-by-default without friction: the API is not wide open (every data route is authenticated), yet an operator can always get in (env secret, or the logged bootstrap token). Sourcing the secret from the environment honours the repo's secret hygiene (CLAUDE.md: secrets via the environment / 1Password, never the repo). A per-process pepper means no secret material is persisted by the server. Keeping `provision_admin_keys` a pure function makes the security-critical branch deterministically testable (env reading + logging stay at the CLI edge).

## Alternatives considered

- **Plaintext keys in the config file** (rejected: puts secrets in a file that lands in the repo/images — violates secret hygiene). Config-declared *roles* with secrets sourced from the environment remain a future option for multi-user/multi-role deployments.
- **No auth / a `--insecure` open mode** (rejected as the default: an unauthenticated management API on a reachable port is the classic foot-gun; the bootstrap token is barely more friction and is safe by default).
- **A full session/cookie + login UI now** (deferred: the API-key bearer path is the tested machine surface today; the browser cookie-session + CSRF path is a later SPA concern, per ADR-W005's scope note).

## Consequences

The web UI needs a place to enter/store the bearer token (a token field / login affordance) — a near-term SPA task. Only the `admin` role exists at provisioning time; finer-grained operator/viewer keys (config-declared, env-sourced secrets) are a follow-up. The generated bootstrap token appears in the logs, so operators on shared log sinks should set `MULTIVIEW_CONTROL_TOKEN` to a managed secret (the startup warning says so). Binding `0.0.0.0` exposes the authenticated API to the network — deployment docs must cover TLS termination / network policy (the engine isolation is unaffected either way, inv #10).
