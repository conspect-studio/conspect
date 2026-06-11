# ADR-W013: Serving the management plane from `multiview run` — control↔engine integration

- **Status:** Proposed
- **Area:** Web/API stack · control↔engine integration
- **Date:** 2026-06-04
- **Source:** [web-api-stack](../research/web-api-stack.md), [realtime-api](../research/realtime-api.md), [management-capability-matrix](../research/management-capability-matrix.md); builds on ADR-RT004 (isolation), ADR-W001/W008 (state sharing), ADR-R004 (live-apply classification)

## Decision

Wire the existing `multiview-control` library into the running daemon so `multiview run` exposes the management API + embedded web UI **without ever being able to back-pressure the engine** (invariant #10). The control plane stays a pure library: `multiview_control::serve(listener, AppState, shutdown)` binds a caller-provided `tokio::net::TcpListener`, drives `axum::serve(router(state))` with graceful shutdown, and returns when `shutdown` resolves. `multiview run` binds the configured `[control]` address (off when absent → today's headless behaviour), constructs `AppState` from the engine's **existing isolation channels**, and spawns the server as a sibling task that shuts down on the same `StopSignal` the engine uses.

Three data paths, all using primitives that already exist and enforce isolation structurally:

1. **Engine → control (state/events out).** The engine publishes into `EnginePublisher<EngineStateSnapshot, Event>` — a wait-free latest-state slot (`arc_swap`) plus a drop-oldest `broadcast` (cap 64). `EngineStateSnapshot` is an opaque `serde_json::Value`, so the control plane never couples to the engine's internal shape. The CLI's `state_of`/`event_of` closures map each `CompositedFrame` to a **compact** snapshot + typed `multiview_events::Event`s; mirror tasks (alarms, tally) subscribe to the event stream off the hot path and feed the control-plane mirror stores.
2. **Control → engine (commands in).** The bounded, non-blocking command bus (`command_bus(cap)`): handlers `try_submit` (shed to `503` when full, `202 Accepted` + operation id otherwise); the engine `try_drain`s once per tick **at the frame boundary** and applies via the already-hot-swappable `CompositorDrive::set_layout`/`insert_store` and the `Salvo` value-machine batch. The engine never `.await`s, never holds a client-reachable lock, and never blocks on the bus (invariants #1 + #10).
3. **Live-apply classification (invariant #11).** Every command maps to **Class-1** (hot/seamless, applied in-loop at a tick boundary) or **Class-2** (controlled reset / make-before-break parallel-output migration). A dry-run/plan path surfaces the class (and `reset_required`) **before** applying, per the capability matrix.

The embedded SPA is served under the `embed-web` feature via `rust-embed` over `web/dist` (staged by `xtask build-web`), same-origin with `/api/v1`; OpenAPI + Scalar stay at `/docs`. All conventions already in the router are preserved (RFC 9457, `ETag`/`If-Match`→412, `Idempotency-Key`, WS primary `/api/v1/ws` + SSE `/api/v1/events`).

## Rationale

The hard part — isolation — is already solved in `multiview-engine` (`EnginePublisher`, drop-oldest broadcast) and `multiview-control` (`try_submit`/`try_drain`, lagged-skip realtime). Wiring those is the **minimum-risk** path and keeps invariant #10 structural rather than enforced by discipline. The engine is generic over its `<State, Event>` types, so the JSON-snapshot bridge needs **no engine change** — only the CLI's closures. Applying commands at the tick boundary (not asynchronously, not on a client thread) is what preserves invariant #1: the output clock samples the command queue exactly like it samples inputs, and an empty/overfull queue costs O(pending) bounded work, never a stall.

## Alternatives considered

A separate control **binary** talking to the engine over IPC/loopback (rejected: adds a failure surface, serialization latency, and a second supervisor for no gain — the in-process channels are already isolation-safe and zero-copy). Shared-memory or lock-based state sharing engine↔control (rejected: a shared lock is a back-pressure path — violates #10). Applying management changes **off-tick** on the handler thread (rejected: races the compositor and can block the clock — violates #1). Hand-rolling a new command/event schema (rejected: `command::Command` and `multiview_events::Event` already exist and are tested).

## Consequences

The CLI's publisher type changes from `EnginePublisher<TickState, TickState>` to `<EngineStateSnapshot, Event>`; the per-tick `state_of` closure now allocates a small JSON snapshot, so it must stay **compact and may be conflated/throttled** off the hot path if measurement shows per-tick serialization is material (start minimal: tick + pts + per-tile state summary; enrich under measurement — no per-frame field dumps, honouring the bounded-memory/NV12 hot-path rules). The engine gains a per-tick `try_drain` (bounded). `embed-web` requires the web build in CI (`xtask build-web` before the embedding crate compiles). Class-2 parallel-output migration is a larger follow-up (this ADR lands Class-1 hot-apply + the classification surface). `tokio`'s `net` feature is added to `multiview-control` (pure Rust, stays in the default build). Shipped in slices: A1 `serve` (done, real-socket + graceful-shutdown test); A2 serve from `run` + embed SPA + compact state bridge; A3 command-drain + enriched snapshot + mirrors; then the missing CRUD resources and the live output servers.
