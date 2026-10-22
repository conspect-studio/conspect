# ADR-W031: Control-plane header-read timeout via a hand-rolled hyper serve loop (SEC-14 slowloris floor)

- **Status:** Accepted
- **Area:** Web/API stack · control-plane security (invariant #10 isolation)
- **Date:** 2026-07-11
- **Source:** SEC-14 follow-up (task #126); the F-A finding on
  [ADR-W028](ADR-W028.md) (request-concurrency + rate caps); builds on the TLS-0
  serve path ([ADR-W029](ADR-W029.md)). Round-2 cross-vendor review widened the fix
  from "header-read timeout" to the complete connection floor (see Decision).

## Context

[ADR-W028](ADR-W028.md) added the management-plane DoS floor: a concurrent-request
cap (`503`) and per-IP / per-API-key rate caps (`429`). Those guards are axum
middleware — they engage **only after hyper has parsed a request's full header
block**. A **slowloris** client that opens a TCP connection and then dribbles (or
never finishes) its request headers therefore holds a hyper task + socket + read
buffer open **indefinitely without ever taking a concurrency permit or a rate
token**. W028 documented this (finding F-A) as out of scope for that change and
tracked the fix as #126.

A header-read timeout alone does **not** close the hole, for two reasons a round-2
review surfaced:

1. **HTTP/2 has no header-read timeout.** Both serve paths negotiated HTTP/2 —
   plain HTTP via `hyper_util`'s `auto::Builder` (which speaks h2 on the connection
   preface *regardless of ALPN*), TLS via `axum-server`, whose default ALPN offers
   `h2`. hyper's HTTP/2 server has **no** `header_read_timeout` (keep-alive PING
   detects a *dead* peer, not an idle-but-live one). So a hostile client that
   negotiates h2 and then stalls its `HEADERS` pins a connection **forever** — the
   header-read timeout, an HTTP/1-only knob, never applies to it.
2. **The connection *population* is unbounded.** Neither serve path capped the number
   of concurrently-accepted connections. The request-level caps ([ADR-W028](ADR-W028.md))
   are all post-header, so a flood of half-open (slow-header **or** slow-TLS-handshake)
   connections pins sockets + tasks + buffers without ever taking a request permit.

For a security **floor** the decisive question is whether a cap slot can be held
*forever* or is forced to *recycle*: a cap that fills with hold-forever h2 (or stalled-
handshake) connections is a DoS **amplifier**, not a floor. An HTTP/1-only serve loop
bounds every **slow-header** connection by the header-read timeout so its slot recycles,
and rejects an h2-preface connection immediately (the preface is a complete but invalid
HTTP/1 request line) — so no slot can be pinned open forever.

## Decision

Own the accept loop on **both** serve paths, serve **HTTP/1-only**, and add the
interdependent pieces that make the floor complete (they ship together — any subset is
a partial floor, rule 6).

1. **HTTP/1-only accept loop (both transports).** `serve_router_with` (plain) and
   `serve_router_tls_with` (TLS) drive `hyper::server::conn::http1::Builder`
   directly — never `auto` — with `timer(TokioTimer::new())` (installed
   unconditionally, since arming the timeout without a timer panics) +
   `header_read_timeout(configured)`, and `serve_connection(..).with_upgrades()` so
   `/api/v1/ws` still upgrades over HTTP/1. HTTP/2 is refused: an ALPN-respecting TLS
   client negotiates `http/1.1` (see 3), and a client that sends the h2 preface anyway
   sends a complete but invalid HTTP/1 request line, which the HTTP/1-only parser rejects
   immediately (never a timeout-free h2 session). Each connection's peer `SocketAddr` is
   re-injected as a `ConnectInfo`
   request extension — axum's `into_make_service_with_connect_info` cannot be reused
   because its `IncomingStream` constructor is private — so the SEC-14 per-IP guard
   keeps its peer-IP key. One shared connection driver (`drive_connection`) serves both
   transports, generic over the stream type.
2. **Accept-level connection population cap.** A `ConnectionAdmission` gate is
   consulted at accept, **before any header parse**: a global `tokio::sync::Semaphore`
   (`max_connections`) plus a per-source-IP counter (`max_connections_per_ip`, keyed on
   the peer IP, pre-auth). Over-cap connections are dropped immediately. Both bounds are
   released the instant the connection's RAII guard drops; the per-IP map is bounded by
   the concurrent **distinct-IP** count — an entry is **evicted at zero**, so an
   attacker rotating source IPs cannot grow it without also holding that many concurrent
   connections (themselves globally capped). This is the population floor the
   request-level caps miss.
3. **TLS — hand-rolled `tokio-rustls` acceptor.** `serve_router_tls_with` wraps the
   loaded `rustls::ServerConfig` (ALPN pinned to `http/1.1` in `load_tls_material`) in a
   `tokio_rustls::TlsAcceptor` and drives the handshake itself, **bounded by a TLS
   handshake timeout** — load-bearing, because the header-read timeout is post-handshake
   and cannot catch a client that completes TCP then stalls mid-`ClientHello`; without
   it the (a) TLS cap would fill with stalled handshakes (the fillable-forever failure,
   one layer down). `axum-server` is dropped; `RustlsMaterial` now wraps
   `Arc<rustls::ServerConfig>`.
4. **Bounded drain + upgrade-safe accounting.** In-flight connection tasks are tracked in
   a `tokio::task::JoinSet`. At shutdown the loop signals each task (a `watch` channel) to
   begin a graceful shutdown — http1's `UpgradeableConnection` does not implement
   hyper-util's `GracefulConnection`, so it is driven directly — then waits up to a
   configurable ceiling and `abort_all()`s + reaps any straggling **tracked** tasks (the
   previous detached-`tokio::spawn` loop abandoned them). Each accepted stream is wrapped
   in a `serve_stream::TrackedStream` that (a) **owns** the accept-level admission guard,
   so when hyper hands the IO to its `Upgraded` at an HTTP/1 upgrade the guard rides into
   the (detached) WebSocket task and releases only when that socket finally closes — a
   live WebSocket therefore keeps counting against the global + per-IP population caps for
   its whole life (without this the guard dropped at the upgrade handshake — the point at
   which `serve_connection` returns, *not* the WebSocket's end — so sequential upgrades
   bypassed the per-IP cap); and (b) is **shutdown-aware** — once the shutdown `watch`
   flips, its reads return EOF and its writes error, with the read and write halves each
   holding an **independent** shutdown waiter (a `watch` receiver clone), so a parked read
   *and* a parked write each wake the instant shutdown fires regardless of which half was
   polled last — robust by construction even if the stream were `io::split`, not reliant
   on a single shared waker slot (which stays correct only because nothing splits the
   stream). `poll_shutdown` (write-half close) deliberately does not consult a waiter: it
   *is* the drain action for the write half, and it delegates to the inner transport's
   `poll_shutdown`, which per the `AsyncWrite` contract MAY return `Pending` (TLS
   `close_notify` backpressure, or a peer that has stopped reading) — a waiter cannot help,
   since finishing that flush / half-close is exactly what it would wait on. That is safe
   because **the drain path never awaits a parking `poll_shutdown`**: an
   upgraded WebSocket is a detached task the `JoinSet` does not track, so serve() does
   **not** synchronously await it; instead it drains **cooperatively-and-promptly**:
   `run_ws_session` selects on `socket.recv()`, which returns end-of-stream on the
   shutdown-aware EOF, so the session ends and releases its slot + concurrency permit. So
   serve() synchronously drains + aborts every **tracked** (non-upgraded) connection at
   the ceiling — so a `poll_shutdown` that parks (e.g. on `close_notify` backpressure) is
   aborted + dropped there, never outliving serve() — while an upgraded WebSocket drains
   cooperatively and is **not** synchronously awaited — the guarantee is prompt-cooperative,
   not a synchronous join.
   This relies on the invariant that **every upgrade handler is cooperative +
   shutdown-aware** (it reads its socket and ends on end-of-stream); a future
   non-cooperative upgrade handler would be a reviewable defect.
5. **Config + `ServeOptions`.** `[control.limits]` gains `max_connections` (default
   **1024**) and `max_connections_per_ip` (default **256**) alongside
   `header_read_timeout_secs` (default **20 s**). `validate()` rejects `0`, a per-IP cap
   above the global cap, and — like `max_concurrent_requests` — a global cap above the
   `Semaphore::MAX_PERMITS` ceiling (fail-closed, never silently clamped). The
   header-read + handshake timeouts apply **independent of the shed-layer `enabled`
   flag** (a lifetime bound has no downside for a legitimate client); the population
   caps **are** shed-layer controls, enforced only while `enabled`. The CLI threads all
   of it into both serve paths via `ServeOptions`; the handshake timeout reuses the
   `header_read_timeout_secs` budget (no second knob).

The bare `serve_router` / `serve` / `serve_router_tls` / `serve_tls` entry points
delegate to the `*_with` variants with `ServeOptions::default()` (caps + timeouts on),
so every existing caller gets the floor and the whole control suite runs through the new
loop.

## Alternatives considered

- **Header-read timeout only, keeping `auto`/`axum-server` + ALPN=`http/1.1` (the
  round-1 design + the review's initial (b) lean).** Rejected: ALPN does not stop
  hyper's `auto` builder from speaking h2 on a post-handshake preface, and hyper's h2
  server has no header-read timeout — so the cap fills with hold-forever h2 slots and
  becomes an amplifier. The recycling-vs-fillable-forever property is the whole ballgame
  for a floor; only HTTP/1-only delivers it.
- **`axum-server`'s `http_builder()` for the TLS timeout (round-1).** Configures the
  *same* `auto` builder, so it inherits the h2 hole above. Superseded by the
  `tokio-rustls` acceptor, which also gives us the handshake timeout `axum-server` did
  not expose.
- **Reverse proxy in front (W028's original note).** Valid for defence-in-depth, but a
  self-hosted appliance's in-process floor must not *depend* on external infra — the
  daemon terminates TLS directly (TLS-0). Kept as an optional layer, not the mechanism.
- **Keep `axum::serve`, wrap the IO with a first-bytes deadline.** "End of the header
  block" is not observable at the raw-IO layer without re-implementing HTTP parsing;
  strictly worse than hyper's built-in `header_read_timeout`. Rejected.

## Consequences

- **Isolation (#10) preserved.** The loop only serves HTTP and never awaits the
  engine; a slow/abusive client cannot back-pressure the data plane. A transient
  `accept` error backs off and continues rather than tearing down the plane. The full
  `multiview-control` suite (serve, management_limits, realtime, ws_ticket, tls_serve,
  serve_header_timeout, serve_connection_floor) passes through the new loop.
- **Behaviourally verified.** RED→GREEN tests: an HTTP/2 preface is rejected promptly by
  the HTTP/1-only parser (well within the header-read deadline) instead of held open as a
  timeout-free session (RED held it ~5 s); plain + TLS slowloris are dropped at the
  header-read deadline; a live upgraded WebSocket keeps its per-IP population slot and
  drains when serve() shuts down (RED: the guard dropped at the HTTP/1 upgrade, freeing
  the slot, and the detached socket outlived serve()); the transport's read and write
  halves each wake independently on the shutdown flip (RED: a single shared shutdown waker
  dropped the parked reader's wakeup when the writer was polled last); an over-cap
  connection is dropped
  promptly at
  accept; an in-flight connection is aborted at the drain ceiling. `ConnectionAdmission`
  unit tests cover the global cap, per-IP cap, drop-frees-both, evict-at-zero, and
  never-exceeds-either-cap. Positive controls prove a complete request is still served.
- **HTTP/2 is not served.** The management API + SPA + realtime stream run over
  HTTP/1.1 (WebSocket via the h1 Upgrade mechanism, not h2 extended-CONNECT). No
  management surface needs h2; this is the price of a recycling floor and is stated
  plainly rather than left as an unbounded h2 hole.
- **Slow request-body is bounded by the concurrency cap, not the header-read timeout.**
  `header_read_timeout` bounds only the request *header* read; a client that completes its
  headers then dribbles a request *body* is past that deadline. It is nonetheless bounded:
  `concurrency_cap` acquires the request-concurrency permit **before**
  `next.run(request).await` and body extraction runs inside the handler, so a slow body
  holds one `max_concurrent_requests` permit for its whole read — a flood of dribbled
  bodies is shed `503` once the cap is reached (and the accept-level population caps bound
  the connection count independently). A dedicated request-body / total-request deadline
  is a **separable** mechanism (a distinct hyper knob) and a tracked follow-up, not part
  of this change — the damage is already bounded.
- **Dependencies.** `hyper` (features trimmed to `server`+`http1`) + `hyper-util`
  (trimmed to `tokio`) are direct deps; `axum-server` is **removed**; `tokio-rustls`
  moves from dev-only to a normal dep of the `tls` feature — but it is already a normal
  (optional) dep via `cast`, and `deny.toml [graph] all-features = false` keeps both
  off-by-default features out of the scanned default graph, so no `deny.toml` change is
  needed. All pure-Rust, LGPL-clean, `cargo deny`-clean.
- **Scope.** This closes the W028 F-A residual: the slow-header hole (header-read
  timeout), h2 refusal (immediate parser reject), connection population (accept-level
  caps), TLS-handshake stall (handshake timeout), and the bounded drain — with slow
  request-body bounded by the concurrency semaphore (above). A dedicated
  request-body / total-request deadline is a separable, tracked follow-up, not a hole this
  leaves open. W028 is updated to point here.
