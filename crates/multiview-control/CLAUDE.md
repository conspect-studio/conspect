# multiview-control — agent notes

The management plane: **axum** REST + WebSocket + SSE, OpenAPI 3.1 (utoipa + Scalar at `/docs`),
auth, SQLite (sqlx), a command-bus shell, and the embedded SPA (`embed-web`). Best-effort —
**physically incapable of back-pressuring the engine** (inv #10): watch/broadcast + bounded
drop-oldest; never make the engine `.await` a client.

API conventions (conventions §6 — canonical):
- REST base **`/api/v1`**; OpenAPI is generated, not hand-written.
- Long-running ops return **`202 Accepted` + an operation id**; the result arrives on the
  realtime stream, not the HTTP response.
- Errors are **RFC 9457** `application/problem+json`.
- Optimistic concurrency: **`ETag` / `If-Match` → `412`**. **`Idempotency-Key`** on start/stop/swap.
- WebSocket primary at **`/api/v1/ws`**; SSE fallback at **`/api/v1/events`**.
- Every management change is **Class-1 (hot/seamless)** or **Class-2 (controlled reset)** — the
  API surfaces which **before** applying (inv #11). The capability table is authoritative.

Read first: [web-api-stack](../../docs/research/web-api-stack.md) ·
[realtime-api](../../docs/research/realtime-api.md) ·
[management-capability-matrix](../../docs/research/management-capability-matrix.md) ·
ADR-RT001..RT006, ADR-W001..W008. Map: [codebase-map](../../docs/development/codebase-map.md).
