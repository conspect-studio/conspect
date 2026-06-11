# ADR-W001: Rust web/API framework: axum 0.8.x

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Use axum 0.8.x as the control-plane HTTP/API framework, running on the engine's existing tokio runtime, with shared state via Arc<AppState> holding the engine's channels/actor handles.

## Rationale

Built by the Tokio team on tokio/tower/hyper, so API and engine share one runtime with no second runtime or actor abstraction; first-class WebSocket/SSE/streaming bodies and the full tower-http middleware ecosystem; largest ecosystem and active maintenance.

## Alternatives considered

poem+poem-openapi (verified OpenAPI 3.0-only, disqualified by the 3.1 requirement); actix-web (faster under saturation but own runtime + actor model adds friction sharing tokio state); salvo (smaller ecosystem); warp (stagnant mindshare).

## Consequences

axum 0.8 breaking changes (/{name} path syntax, Bytes-based ws Message) mean pre-0.8 snippets won't compile; the whole product must standardize on tokio for the shared-runtime benefit to hold.
