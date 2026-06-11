# ADR-W007: SPA build/serve: embed in the single binary (rust-embed / axum-embed)

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Embed the built Vite SPA into the Rust binary at compile time with rust-embed/axum-embed and serve it from the same axum server as the API/SSE, with a fallback handler returning index.html for client-side routes.

## Rationale

Delivers the single-deployable, low-friction constraint: one process, one port, no separate static host or CDN; sub-1MB bundles add negligible binary size and are served in-memory; same-origin serving also simplifies CORS and Scalar try-it-out.

## Alternatives considered

tower-http ServeDir from disk (cannot serve embedded files; breaks single-deployable if used alone); separate static host/CDN (more cache flexibility but breaks single-deployable simplicity).

## Consequences

Every frontend change requires rebuilding the binary — wire the Vite build into the cargo build (build script/CI step) so embedded assets stay current; optionally a dev feature flag serving from disk for fast iteration.
