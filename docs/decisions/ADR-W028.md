# ADR-W028: Management-plane request-concurrency + rate caps (SEC-14 control-plane DoS floor)

- **Status:** Accepted
- **Area:** Web/API stack · control-plane security (invariant #10 isolation)
- **Date:** 2026-07-11
- **Source:** the security/BOLA audit (task #18, SEC-14 — the last un-addressed item);
  [web-api-stack](../research/web-api-stack.md); builds on the auth model
  ([ADR-W026](ADR-W026.md), API keys `Bearer <key_id>.<secret>`) and the RFC-9457
  problem+json convention ([conventions §6](../architecture/conventions.md)).

## Context

The management plane (`multiview-control`: axum REST + WebSocket + SSE) had **no
request-concurrency or request-rate ceiling**. A single misbehaving client, a runaway
script, or an unauthenticated brute-forcer hammering the auth path could saturate the
control plane's Tokio workers and its SQLite/command-bus back-ends. This is a
denial-of-service floor, not a perfect shield: the deployment target is a self-hosted
appliance on a **trusted facility network**, so the threat model is a buggy/runaway client,
credential brute-force, and a basic flood — not internet-scale DDoS.

The control plane is a **best-effort actor that must never back-pressure the engine**
(invariant #10). Any cap we add therefore has to **shed** (reject) rather than **queue** —
a queue is back-pressure — and must hold no engine handle.

## Decision

Add three config-driven guards to the served router, all returning **RFC 9457
`application/problem+json`** with a **`Retry-After`** header:

1. **Concurrent-request cap → `503`.** A `tokio::sync::Semaphore`; each request
   `try_acquire`s one permit held across the handler and released when the work completes
   (the response body ends, or a claimed permit's detached WebSocket/SSE session closes).
   Over the cap the request is **shed** with `503` + `Retry-After` — never queued
   (invariant #10). This bounds in-flight requests and established connections, not
   half-open slow-header connections (see Consequences).
2. **Per-source-IP rate limit → `429`, pre-auth.** A token bucket keyed on the
   `ConnectInfo` peer IP, applied **outermost** so an abusive IP is shed before it consumes
   a concurrency permit. This is the brute-force guard on the auth path. If the peer IP is
   unavailable the guard **fails open** (the concurrency cap + per-key limit still apply) —
   a DoS floor must never become a self-inflicted outage.
3. **Per-API-key rate limit → `429`, post-auth.** A token bucket keyed on the **validated**
   `key_id`: the middleware resolves the presented `Bearer` credential through the existing
   `ApiKeyStore` and limits **only a request that authenticates**. Unauthenticated requests
   pass through to the per-IP + concurrency caps.

**The token bucket is hand-rolled, bounded by construction.** A keyed limiter that grows a
per-key map is itself a memory-exhaustion vector (an attacker rotates source addresses to
inflate it). Instead each limiter hashes every key into a **fixed-size table** of buckets
(`RATE_LIMIT_CELLS = 4096`), so memory is `O(cells)` and **never grows** regardless of how
many distinct IPs/keys are seen — this is the anti-DoS property. Two keys that hash to the
same cell **share** a bucket, which can only make limiting *stricter* for that pair, never
looser. The hasher is **per-process random-seeded** (`RandomState`), so an attacker cannot
predict or force a collision to target a victim. Accounting is **integer nano-token fixed
point** (never float, saturating everywhere → a frozen/jumped clock cannot panic) and
**clock-injected** (the pure `check(key, now_ns)` is exhaustively testable offline).

**Config** (`[control.limits]`, `multiview-config`): `enabled` + `max_concurrent_requests`
+ per-IP / per-API-key `{burst, refill_per_sec}`. **Secure defaults** (limits **on**): 256
concurrent, per-IP 120 burst / 40 s, per-API-key 240 burst / 80 s. Validated at config load
(a zero cap/burst/refill is rejected fail-closed). **Disabled ⇒ no middleware is installed**
at all — the router is byte-for-byte unchanged (zero overhead). The bare `AppState`
constructor defaults to the **inert** set, so existing tests/embedders are unlimited unless
they opt in; the binary installs the configured limits via `AppState::with_limits`.

## Consequences

- A single client/IP/key can no longer wedge or monopolise the management plane; the auth
  path is brute-force-throttled. The caps are control-plane-only and **physically cannot
  back-pressure the engine** (they shed, never queue; hold no engine handle — invariant #10).
- **Connection-level slow-header exhaustion (slowloris) is addressed separately by a
  header-read timeout ([ADR-W031](ADR-W031.md), #126).** All three guards here are axum
  middleware, so they engage only *after* hyper has parsed a request's headers; a client that
  opens many TCP connections and dribbles partial headers consumes sockets and hyper tasks
  without ever taking a concurrency permit. Because `axum::serve` (0.8) exposes no
  `header_read_timeout`, W031 hand-rolls the accept/serve/graceful-shutdown loop on
  `hyper_util`'s `conn::auto::Builder` (and configures axum-server's builder on the TLS path)
  to bound the header-read phase. Deployments exposed to untrusted clients may still front the
  plane with a reverse proxy (nginx/HAProxy) for defence-in-depth.
- **Behind a reverse proxy**, the per-IP limit keys on the *proxy* IP (`ConnectInfo` is the
  direct peer). Trusting `X-Forwarded-For` is a limiter-bypass vector, so it is **not**
  trusted by default; a trusted-proxy XFF option is a documented follow-up. The per-IP guard
  also requires the router be served via `serve`/`serve_router` (which install `ConnectInfo`);
  serving it through another make-service disables that guard (it fails open, and warns once).
- **JWT / NMOS IS-10** requests are covered by the per-IP + concurrency caps but not the
  per-key limit (which targets the native API-key path, token shape `<key_id>.<secret>`).
- No new dependency: the limiter is ~200 lines of well-understood algorithm, keeping the
  default build LGPL-clean and the `cargo deny` closure unchanged.

## Alternatives considered

- **`tower_governor` / `governor`** for the rate limit — rejected: a non-trivial new
  dependency closure (`governor` + `dashmap` + `nonzero_ext` + `quanta`), and its keyed
  limiter still grows a `DashMap` that needs periodic `retain_recent` shrinking. The
  hand-rolled fixed-cell table is bounded by construction and dependency-free.
- **`tower::limit::GlobalConcurrencyLimitLayer` + `LoadShedLayer`** for the concurrency cap
  — a valid option (and the initial preference), but it needs a `HandleErrorLayer` to map
  the generic `Overloaded` error into RFC-9457 `503`. A `tokio::sync::Semaphore` `from_fn`
  gives exact `503` + `Retry-After` control, uniform with the two rate-limit `from_fn`
  layers, and adds no tower feature.
- **Keying the per-key limit on the *presented* (unvalidated) credential** — rejected: an
  attacker who knows a victim's `key_id` could spam wrong-secret requests to drain the
  victim's bucket. Resolving through `ApiKeyStore` first (one extra HMAC per authenticated
  request, negligible) makes the guard genuinely post-auth.
