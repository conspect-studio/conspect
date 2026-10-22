# ADR-W029: TLS-0 — static-certificate rustls termination for the control plane

- **Status:** Accepted
- **Area:** Web/API
- **Date:** 2026-07-11
- **Source:** operator/team-lead directive (task #34, LANE-API security floor); research brief [`acme-tls.md`](../research/acme-tls.md); follows [ADR-W028](ADR-W028.md) (SEC-14 management-plane caps)

## Context

The control plane (`multiview-control`, axum) serves the REST/WS/SSE API, the
OpenAPI/Scalar docs, and the embedded SPA over **plain HTTP** only. Bearer API
keys, the `tower-sessions` cookie, and the realtime firehose therefore cross the
wire in cleartext unless an operator fronts the daemon with their own reverse
proxy. For a self-hosted appliance that is the security floor's biggest gap:
credentials in cleartext on the LAN.

This ADR records the **TLS-0** decision — the *static-certificate floor* of the
TLS/ACME ladder (`acme-tls.md`). It deliberately does **not** include ACME /
automatic issuance / renewal (a later phase); it gives the operator in-process
HTTPS with a certificate + key they manage.

Binding constraints:

- **SEC-14 must not regress** ([ADR-W028](ADR-W028.md)). The served router keys
  its pre-auth per-IP rate limit on the peer `SocketAddr` delivered via
  `into_make_service_with_connect_info::<SocketAddr>`. The TLS serve path must
  preserve that wiring, or the DoS floor silently fails open under HTTPS.
- **Isolation (inv #10)** — the serve path stays best-effort and can never
  back-pressure the engine; TLS changes only the transport, not that contract.
- **IPv6-first** (conventions §10) — bind dual-stack `[::]`, bracket IPv6
  literals, no v4-only path.
- **LGPL-clean default build** (conventions §7) — the TLS stack must be
  off-by-default and pull no license-escalating or new-to-the-tree native code.
- **No control-plane panics** (safety §3) — a rustls setup fault must surface as
  a typed error, never a panic.

## Decision

Add an **off-by-default `tls` Cargo feature** to `multiview-control` (forwarded
by a `multiview-cli` `tls` feature into the `nvidia`/`apple`/`linux-vaapi`/`full`
umbrella presets). It pulls `axum-server` (`tls-rustls`), `rustls`, and
`rustls-pki-types` — all rustls 0.23 / **aws-lc-rs**, already in the lock via the
`cast` (`tokio-rustls`) and `reqwest` rustls backends. The default build is
unchanged at the dependency level: the `TlsConfig` schema types and the CLI's
feature-off rejection path compile unconditionally (pure Rust, no new crates),
but with `tls` off **no native TLS dependency** (`rustls`/`aws-lc-rs`/
`axum-server`) is pulled — so the default `cargo check` stays LGPL-clean and
native-dep-free.

Concrete shape:

- **Config** (`multiview-config`, always compiled): a new `[control.tls]` section
  — `TlsConfig`, an enum **internally tagged by `mode`**
  (`#[serde(tag = "mode")]`, never `untagged`; `#[non_exhaustive]`). The only
  TLS-0 variant is `mode = "static"` carrying `cert_file` + `key_file` PEM paths.
  Absent ⇒ plain HTTP. `MultiviewConfig::validate` rejects an empty
  `cert_file`/`key_file` path fail-closed; file existence/parse is checked at
  serve time (a config may be authored off the deployment host, mirroring
  `cast_media_base`).
- **Serve** (`multiview-control`, `#[cfg(feature = "tls")]`):
  - `load_tls_material(&TlsConfig) -> Result<RustlsMaterial, TlsSetupError>` — a
    **separate, synchronous startup step** that reads the PEM chain + key and
    builds a `rustls::ServerConfig` with an **explicit `aws-lc-rs` provider**
    (`builder_with_provider(rustls::crypto::aws_lc_rs::default_provider())`),
    wrapped as an opaque `RustlsMaterial`.
  - `serve_router_tls(listener, app, material, shutdown)` — the TLS sibling of
    `serve_router`: `axum_server::from_tcp_rustls(listener.into_std()?, material)`
    with `.serve(app.into_make_service_with_connect_info::<SocketAddr>())` (the
    SEC-14 wiring, preserved) and an `axum_server::Handle` graceful drain bridged
    from the caller's `shutdown` future.
  - `serve_tls(listener, state, material, shutdown)` — the TLS sibling of `serve`
    (`serve_router_tls(listener, router(state), material, shutdown)`).
- **CLI** (`multiview-cli`): `bind_and_serve` loads the material at startup and
  spawns `serve_router_tls` when `[control.tls]` is present, else `serve_router`.
  A build **without** the `tls` feature that is handed a `[control.tls]` config
  **fails startup loudly** (never silently serves plain HTTP).

## Rationale

- **`axum-server` over a hand-rolled hyper accept loop.** `axum::serve` (the
  plain path) does not expose a TLS acceptor; `axum-server` provides
  `from_tcp_rustls` over an already-bound listener plus a `Handle` for graceful
  shutdown, and — verified by the e2e test — carries
  `into_make_service_with_connect_info::<SocketAddr>`, so the SEC-14 peer-IP key
  survives. It reuses the *same* bound `TcpListener` the caller already created
  for dual-stack `[::]`, so the IPv6-first bind is unchanged.
- **Explicit `aws-lc-rs` provider, not the process default.** A `full` build
  links **both** `ring` (via the WebRTC stack) and `aws-lc-rs`; rustls 0.23 then
  cannot auto-pick a process-default `CryptoProvider` and
  `RustlsConfig::from_pem_file` (which relies on it) **panics** at first use.
  Building the `ServerConfig` with `builder_with_provider(aws_lc_rs)` — the exact
  idiom the cast driver already uses (`devices/cast/net.rs`) — is deterministic
  and panic-free regardless of what else is linked.
- **`mode`-tagged enum** matches the house union convention (`#[serde(tag=…)]`
  everywhere in `multiview-config`) and reserves the wire shape for later modes
  (`mode = "acme"`) without a breaking change.
- **Load-at-startup, separate from serve.** A missing/garbage certificate is an
  operator error that must abort startup with a clear message, not fail silently
  inside a spawned task.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| `RustlsConfig::from_pem_file` (axum-server's built-in PEM loader) | Builds the `ServerConfig` via the **process-default** `CryptoProvider`; panics when both `ring` and `aws-lc-rs` are linked (the `full` build). Explicit-provider build is panic-free. |
| Terminate TLS only in a reverse proxy (nginx/caddy), no in-process TLS | Leaves the self-hosted single-binary deploy (the primary target) with cleartext credentials out of the box; the appliance must be secure without extra infrastructure. Reverse-proxy termination stays supported, not required. |
| `rustls-acme` / built-in ACME now (skip TLS-0) | ACME (DNS-01) is a larger, separately-verified phase (`acme-tls.md`); the static-cert floor is shippable now and unblocks the security floor. TLS-0's `mode`-tagged enum leaves room for it. |
| Make `tls` a default feature | Would pull `aws-lc-rs` (C/asm) + rustls into the LGPL-clean default `cargo check`, breaking the no-native-deps baseline. Must stay off-by-default. |
| `ring` provider | The tree's established rustls provider is `aws-lc-rs` (reqwest + cast backends); matching it avoids a second provider and keeps one crypto backend. |

## Consequences

- **Easier:** the self-hosted daemon serves HTTPS with an operator cert + key and
  no external proxy; the SPA, API, and realtime stream are encrypted end-to-end
  on the LAN. Shipped deploy images (`nvidia`/`apple`/`linux-vaapi`/`full`) carry
  the capability.
- **Harder / committed to maintain:** a new off-by-default dependency surface
  (`axum-server`) tracking axum/hyper; a second serve path (`serve_router_tls`)
  alongside `serve_router` — kept minimal and covered by an e2e test that pins the
  SEC-14 ConnectInfo behaviour under TLS.
- **Certificate lifecycle is the operator's** in TLS-0 (drop-in replacement +
  restart to rotate; no hot-reload, no ACME). Hot-reload and ACME are later TLS
  phases; the `RustlsMaterial` seam and the `mode`-tagged config leave room for
  both.
- **Slowloris (half-open slow-header) is still out of scope** — as with the plain
  path (ADR-W028), TLS-0 does not add an HTTP header-read timeout (tracked as
  follow-up #126); TLS termination does not change that surface.
- **Licensing:** unchanged — `aws-lc-rs` (ISC/Apache-2.0-like, already in the
  lock) + `axum-server`/`rustls`/`rustls-pki-types` (MIT/Apache-2.0) keep the
  build LGPL-clean; `deny.toml [graph] all-features = false` keeps the
  off-by-default deps out of the scanned default graph.
- **Invariants:** #10 (isolation) unchanged — TLS is transport-only; the serve
  path still cannot back-pressure the engine. SEC-14 (ADR-W028) preserved and
  test-pinned under TLS.
