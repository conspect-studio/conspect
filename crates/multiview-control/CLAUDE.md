# multiview-control — agent notes

Management plane: axum REST + WebSocket + SSE, OpenAPI 3.1 (utoipa + Scalar at `/docs`), auth, SQLite (sqlx), command-bus shell, embedded SPA. Best-effort — physically incapable of back-pressuring the engine (inv #10): watch/broadcast + bounded drop-oldest.
API conventions: REST base `/api/v1`; OpenAPI generated, not hand-written. Long-running ops return `202` + operation id, result arrives on the realtime stream.
Errors are RFC 9457 `application/problem+json`. Optimistic concurrency: `ETag`/`If-Match` → `412`; `Idempotency-Key` on start/stop/swap.
WebSocket primary at `/api/v1/ws`; SSE fallback at `/api/v1/events`.
Every management change is Class-1 (hot/seamless) or Class-2 (controlled reset) — surface which before applying (inv #11).
Read first: [management-capability-matrix](../../docs/research/management-capability-matrix.md).
