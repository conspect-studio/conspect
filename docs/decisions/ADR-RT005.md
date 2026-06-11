# ADR-RT005: WS auth via short-lived one-time ticket (default), with subprotocol-token and same-origin-cookie alternatives

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-02
- **Source brief:** [realtime-api.md](../research/realtime-api.md)

## Decision

Default browser auth is a one-time, short-TTL (~30s), single-use, IP/origin-bound ticket minted by authenticated REST POST /api/v1/realtime/ticket, passed as ?ticket=. Also support a subprotocol token (Sec-WebSocket-Protocol: multiview.v1, multiview.token.<jwt>, with mandatory echo-back of exactly one subprotocol) and a same-origin session cookie (with mandatory strict Origin allow-list to block CSWSH). API/non-browser clients may send Authorization: Bearer directly. SSE uses the Authorization header or the same ticket. Validate before on_upgrade; close 4401 (auth)/4403 (forbidden scope). First server frame is always $hello{session_id, server_v, heartbeat_ms, max_rate_hz, clock_epoch, auth{sub,scopes}}. Scopes gate topics/ids. Keep the ticket store behind a trait (in-proc map now, Redis later).

## Rationale

Browsers cannot set Authorization on new WebSocket(). A ticket keeps long-lived tokens out of URLs/logs/history/Referer, is single-use, and works identically for WS and SSE. All three converge on one AuthContext shared with the REST identity system. Validating before upgrade yields debuggable HTTP errors instead of silent socket closes.

## Alternatives considered

Raw bearer in the query string (leaks to logs/history); cookie-only (CSWSH risk without manual Origin checks since WS bypasses CORS); subprotocol-token-only (leaks token to proxy logs/devtools, and the echo-back gotcha silently breaks browsers if missed); no socket auth (unacceptable).

## Consequences

A reconnect must mint a new ticket (or accept a small reuse window). The subprotocol path must always echo multiview.v1 (integration-tested) and is documented with its logging risk. Cookie auth requires an Origin allow-list. The in-proc ticket store does not span replicas — trait-gated for a future shared store.
