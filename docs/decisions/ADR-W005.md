# ADR-W005: API auth model: dual-credential (cookie sessions + API keys) with RBAC

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Web UI uses tower-sessions signed+encrypted cookies (HttpOnly/Secure/SameSite) + CSRF synchronizer tokens; machine access uses high-entropy API keys (Bearer) stored as SHA-256/HMAC hashes; RBAC admin/operator/viewer via axum-login with per-object (BOLA) authorization on every resource.

## Rationale

Cookies avoid token-in-localStorage XSS for the UI and are zero-config; API keys are the expected scriptable credential; both documented as distinct OpenAPI security schemes; BOLA is the #1 OWASP API risk so per-object checks are mandatory, not optional.

## Alternatives considered

JWT for the UI (hard to revoke, localStorage-exposed); mandatory OIDC (too heavy for the default self-hosted install); role-only checks (insufficient — gate the verb, not the object); Casbin (defer until policy outgrows static roles).

## Consequences

SameSite is not full CSRF defense so synchronizer tokens are required on state-changing cookie requests; API keys must be shown once and support revoke/rotate; source-URL credentials must be secrecy-wrapped, encrypted at rest, and write-only in the API; rate-limit auth endpoints (tower-governor) and lock CORS to the app origin.
