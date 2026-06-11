# ADR-W002: OpenAPI 3.1 tooling + interactive try-it-out: utoipa + utoipa-axum + Scalar

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Generate OpenAPI 3.1 code-first with utoipa 5.x + utoipa-axum 0.2 (OpenApiRouter, split_for_parts), serve interactive try-it-out via utoipa-scalar same-origin (Swagger UI vendored as secondary), and export the spec JSON for typed-client generation.

## Rationale

utoipa emits 3.1 only (verified: OpenApiVersion has just Version31) from the same handler annotations that define routes, eliminating doc drift; Scalar embeds assets with no build-time download and authenticates against the documented schemes.

## Alternatives considered

aide (axum-only, macro-free 3.1, smaller ecosystem); poem-openapi (3.0 only); hand-written openapi.yaml (drifts); Swagger UI without vendored (build-time download breaks offline builds).

## Consequences

utoipa-axum 0.2 pins axum ^0.8/utoipa ^5.0 — keep versions in lockstep; polymorphic Source/Output needs untagged+discriminator for clean oneOf; Scalar must use relative servers URLs + a same-origin/disabled proxyUrl to avoid leaking try-it-out traffic to proxy.scalar.com; validate the spec in CI.
