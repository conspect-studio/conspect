# ADR-RT001: WebSocket primary, SSE one-way fallback, REST for commands

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-02
- **Source brief:** [realtime-api.md](../research/realtime-api.md)

## Decision

Use a single axum router in multiview-control exposing REST (OpenAPI, commands/CRUD) plus a PRIMARY bidirectional WebSocket realtime channel for events/state and WHEP signaling, with SSE as a one-way server→client degraded fallback carrying the identical envelope. WS and SSE share one transport-agnostic fan-out core with two wire encoders (WsJson/WsBinary, SseText).

## Rationale

WS is the only channel supporting the full requirement set: bidirectional (client subscribe/ack + WHEP offer/answer/ICE), binary frames for compact high-rate meters, and low overhead. SSE is needed because some proxies/corporate MITM strip the WS Upgrade; the browser EventSource gives free auto-reconnect + Last-Event-ID for resume. Sharing one core means exactly one event-production path (no drift) with only framing differing.

## Alternatives considered

WS-only (loses connectivity behind restrictive proxies); SSE-only (cannot carry WHEP signaling or client subscribes; text-only, no binary meters; HTTP/1.1 6-connection cap); long-polling (high latency/overhead); gRPC streaming (poor browser support, second auth surface).

## Consequences

SSE clients cannot do live preview (WHEP) and must use a separate plain-HTTP WHEP endpoint or disable preview; SSE meters are base64/JSON and lower-rate; SSE requires HTTP/2 to avoid the per-origin connection cap. Two encoders and an SSE-degraded UX path must be tested. The split (REST=commands, WS=events) must be stated verbatim in both specs.
