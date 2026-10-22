# ADR-RT011: Realtime WS/SSE browser auth via a single-use ticket + mandatory Origin allow-list

- **Status:** Accepted
- **Area:** Realtime API
- **Date:** 2026-07-10
- **Source:** 2026-07 security audit — SEC-01 (CRITICAL, live in the shipped build) and SEC-13 (CSWSH); implements the ticket path [ADR-RT005](ADR-RT005.md) decided but never built.

## Context

The realtime transports authenticate a browser by accepting the caller's **durable
`Bearer` API key as a URL query parameter** — `GET /api/v1/ws?access_token=…` and
`GET /api/v1/events?access_token=…` (`crates/multiview-control/src/realtime.rs`,
`resolve_principal` / `AccessTokenQuery`). A browser `WebSocket` / `EventSource`
cannot set an `Authorization` header, so the token was smuggled in the query.

- **SEC-01 (CRITICAL, CWE-598 / RFC 6750 §2.3):** query strings are written to
  reverse-proxy and web-server access logs, browser history, and the `Referer` in
  cleartext by default. The token in `?access_token=` is the operator's **durable,
  full-admin** bearer (the bootstrap `admin` key, or any config-declared key). It
  does not expire and re-authenticates every REST route. A single proxy log line is
  a full credential compromise. The `same-origin only` doc comment on that code was
  never enforced. This is live in the shipped build with **no** scoped-principal
  precondition.
- **SEC-13 (CSWSH):** the WS upgrade performed **no `Origin` check**. A WebSocket
  handshake is exempt from the Same-Origin Policy and CORS, so any site the victim
  visits can `new WebSocket("wss://appliance/api/v1/ws")` and read the entire engine
  event firehose. An opt-in `auth_disabled` trusted-network mode makes this a
  zero-credential read of everything.

Constraints that bind the answer: **invariant #10** — the realtime/control plane must
be *physically incapable of back-pressuring the engine* (no lock/await/unbounded queue
the engine can observe; [ADR-RT004](ADR-RT004.md)). The mitigating properties that must
be **preserved, not regressed**: the WS is send-only (`run_ws_session` never reads
`socket.recv`), REST mutations are `Json`-only (no ambient-credential form posts, so the
REST surface is structurally CSRF-immune), and the JWT path refuses `alg:none`. The SPA
holds the bearer in `localStorage` (`web/src/api/token.ts`, key `multiview.apiToken`) —
there is no cookie/session/CSRF and none is being added.

[ADR-RT005](ADR-RT005.md) already **decided** the fix (a short-TTL single-use ticket)
but it was never implemented; this ADR is that implementation plus the Origin allow-list.

## Decision

**1. Ticket handshake (SEC-01).** A new authenticated `POST /api/v1/ws/ticket` mints a
short-lived, single-use, high-entropy **ticket** bound to the caller's full
[`Principal`] (role + all three [`AuthzScopes`] axes) and its [ADR-RT010](ADR-RT010.md)
live-reauth baseline generation. The WS (`GET /api/v1/ws`) and SSE (`GET /api/v1/events`)
upgrades accept **`?ticket=<t>` instead of the durable bearer**, and **consume it
atomically** (single-use) on accept. Mint requires the same credential as REST (the
`Authorization: Bearer` header) and the same read gate (`Action::Read`).

- The durable-bearer `?access_token=` query is **removed** from the WS, SSE, **and**
  `GET /api/v1/auth/status` paths (a clean cut — the SPA is migrated in the same push).
  Native/non-browser clients continue to send `Authorization: Bearer` **on the upgrade
  request itself** (headers are not logged in URLs); they never mint a ticket.
- Carrier choice: a `?ticket=` query, not `Sec-WebSocket-Protocol`. SSE has no
  subprotocol mechanism, so the query carrier is required for SSE parity regardless; the
  subprotocol token still lands in proxy logs and carries the echo-back footgun
  (ADR-RT005 alternatives). The decisive difference from the status quo is that a
  **consumed or expired single-use ticket in a log is inert**, whereas a durable bearer
  is not.
- Ticket shape: `expires_in_secs = 30` (`WS_TICKET_TTL`), a ≥240-bit CSPRNG token (two
  `getrandom`-backed `uuid::Uuid::new_v4()` values, hex — no new dependency), single-use.
  Response body `{ "ticket": String, "expires_in_secs": u64 }`.

**2. Ticket store (invariant #10).** A `WsTicketStore` on `AppState` (behind `Arc`):
a `Mutex<HashMap<String, TicketRecord>>` bounded to `WS_TICKET_CAPACITY = 4096`, TTL-swept
on mint, **drop-oldest** past the cap, consumed by an atomic `remove` under the lock.
Control-plane only — the engine never touches it. Mint/consume are O(1)+bounded-sweep,
never `.await`, never grow unbounded. The store is trait-free for now (in-proc single
engine; a shared store is future work, ADR-RT005).

**3. Origin allow-list (SEC-13).** An `AllowedOrigins` gate is checked on **both** the WS
and SSE upgrade, **before** auth and **regardless of `auth_disabled`** (CSWSH needs no
credential). Both the configured entries and the request `Origin` are **strictly parsed**
to a canonical `Origin` value (`crates/multiview-config/src/origin.rs`): exactly
`scheme://host[:port]` with an `http`/`https` scheme and **nothing else** — no path,
query, fragment, or userinfo. The parse is deliberately stricter than a general URL
parser, so a lenient reduction like `https://host/evil` → the bare authority `host` (a
same-origin bypass) is impossible. Policy: an **absent** `Origin` header passes
(non-browser clients are not subject to SOP and are not a CSWSH vector — browsers always
send `Origin` on a WS/SSE handshake); a **present** `Origin` passes iff, once parsed, it
is in the configured allow-list **or** is **same-origin** (its parsed authority equals the
request's `Host` header, host case-insensitive + port exact). A present `Origin` that does
**not** strictly parse — `Origin: null`, a path/query/userinfo, a non-http(s) scheme, or a
non-UTF-8 header — is **denied fail-closed** (it is never folded into the absent case). The
same parser guards **every** construction path, so `null`/malformed can never be stored as
an allow-list entry (`AllowedOrigins::new` drops what does not parse) and thus can never be
matched. The list is sourced from a new **`control.allowed_origins`** config field
(`crates/multiview-config/src/lib.rs`, default empty ⇒ same-origin-only), whose entries are
rejected at config-load if malformed (same `Origin::parse`), and is wired to `AppState` by
`multiview-cli`. The "derive the default from the bind/public origin" of the task is
realized dynamically via the `Host`-header same-origin comparison — strictly more robust
than parsing the wildcard `listen = "[::]:8080"`, and correct behind a Host-preserving
reverse proxy.

The `POST /api/v1/ws/ticket` mint declares its Bearer auth in the OpenAPI spec (the
`bearer_auth` HTTP security scheme, referenced per-operation — no global default, so
unauthenticated routes are not falsely marked), so the generated TS client carries typed
auth for the ticket call.

**4. SSE parity.** `/api/v1/events` gets the identical ticket + Origin treatment.

Touched: `crates/multiview-control/src/{realtime.rs, auth.rs, state.rs, lib.rs,
routes/mod.rs, openapi.rs}`, `crates/multiview-config/src/{lib.rs, origin.rs}`,
`crates/multiview-cli/src/control.rs`, and the SPA realtime client under `web/src`.

## Rationale

- A single-use, 30-second, principal-bound ticket keeps the durable credential out of
  every URL, log, history entry, and `Referer`. Even a leaked ticket is inert once
  consumed or after 30 s, and a leaked-but-unconsumed ticket cannot be replayed by a
  browser attacker (the Origin gate rejects their origin) — a strict dominance over the
  durable bearer.
- Binding the ticket to the **captured** RT010 baseline generation preserves live
  authz-revocation semantics end to end: a key revoked between mint and connect advances
  the generation past the ticket's baseline, so the first `reauthorize` on the new session
  disconnects it (no re-probe race).
- The Origin gate is the *only* CSWSH defense that also holds in `auth_disabled` mode, and
  it composes with the ticket (defence in depth): a cross-site page can neither read the
  `localStorage` token to mint a ticket **nor** pass the Origin check.
- Comparing `Origin` to the `Host` header makes the embed-web SPA work with **zero
  config** (it connects to the same authority it was served from) while still rejecting a
  foreign origin, because a browser cannot forge either header.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Keep `?access_token=` but stop proxies logging it | Not enforceable — logging is the *reverse proxy's* default, outside our control; the leak is structural to putting a durable secret in a URL. |
| `Sec-WebSocket-Protocol` token carrier | SSE cannot use it (no subprotocol), so a query carrier is still needed for parity; the subprotocol token still reaches proxy logs and the mandatory echo-back silently breaks browsers if missed (ADR-RT005). |
| Bearer-with-deprecation-window (accept both ticket and durable query) | Leaves the CRITICAL leak open during the window for zero benefit — the SPA is the only browser consumer and is migrated in the same push (rule 6, clean cut). |
| Cookie/session + CSRF for the browser path | No cookie/session exists and none is wanted (memory: auth is token-in-header only); a cookie would *introduce* an ambient credential and a CSWSH/ CSRF surface. |
| Ticket bound to client IP | Fragile behind proxies/dual-stack; the single-use + 30 s TTL + Origin gate + ≥240-bit entropy already make a leaked ticket inert. Left as documented future hardening (honest, not claimed). |
| Persisting/deriving the allow-list from `listen` | `listen` is typically the wildcard `[::]:8080`, not a real origin; the dynamic `Host` comparison is both simpler and correct behind a Host-preserving proxy. |

## Consequences

- **Breaking change for browser realtime clients** that used `?access_token=`: they must
  now `POST /api/v1/ws/ticket` and connect with `?ticket=`. The shipped SPA is migrated in
  the same change; native clients (header `Bearer` on the upgrade) are unaffected. Release
  notes must call this out.
- New committed surface to maintain: `POST /api/v1/ws/ticket` (OpenAPI-documented,
  spec regenerated), the `control.allowed_origins` config field, and the `WsTicketStore`.
- Operators fronting the appliance with a **Host-rewriting** reverse proxy, or serving the
  SPA from a **separate** web origin, must set `control.allowed_origins`; the default
  same-origin policy covers the built-in embed-web SPA with no config.
- **Invariant #10 preserved:** the ticket store and Origin check are control-plane-only,
  bounded, wait-free-to-the-engine; neither adds a path the engine can await or be
  back-pressured by. **Send-only WS, Json-only REST, JWT `alg:none` refusal, the RT009
  watermark, and RT010 live-reauth are all preserved.**
- **Residual risk (highest):** a ticket that leaks *within its ≤30 s window before it is
  consumed* could be replayed by a **non-browser** client (which sends no `Origin`, so the
  Origin gate does not apply to it). Mitigated by the short TTL, single-use consumption,
  and ≥240-bit entropy; IP-binding is the documented next hardening step.

[`Principal`]: ../../crates/multiview-control/src/auth.rs
[`AuthzScopes`]: ../../crates/multiview-control/src/auth.rs
