# Research brief — ACME/TLS for the control plane (DNS-01 only)

> **Status:** design brief feeding ADR-0029 (Proposed). Subsystem: `multiview-control`.
> **Read first if you touch TLS:** [`conventions.md` §7 (licensing)](../architecture/conventions.md),
> CLAUDE.md invariant **#10 (isolation)** and safety rule **#2**, [realtime-api](realtime-api.md).
> **Source of truth is the Rust code + `conventions.md`.** This brief captures the *why* and the footguns.

## 0. The problem

The control plane (`multiview-control`: axum REST + WebSocket + SSE + embedded SPA + OpenAPI) is
served over **plain HTTP** today. `serve()` (`crates/multiview-control/src/lib.rs:182`) wraps
`axum::serve(listener, router(state)…)` over a pre-bound `tokio::net::TcpListener`; the CLI binds it
in `bind_and_serve` (`crates/multiview-cli/src/control.rs:57`). We need **automatic TLS** that:

- requires **no inbound reachability** on `:80`/`:443` (the host may be behind NAT, or have those
  ports firewalled/owned by the program output);
- can issue **wildcard** certs;
- never pulls **openssl** (the default build is LGPL-clean; cargo-deny allows **MIT/Apache-class only**,
  `deny.toml:40-52`);
- **cannot, under any failure mode, stall or back-pressure the output clock** (invariant #10).

## 1. Why DNS-01 ONLY (and HTTP-01 / TLS-ALPN-01 rejected)

ACME (RFC 8555) defines three challenge types. We mandate **DNS-01** and forbid the other two:

| Challenge | Needs inbound | Wildcards | Verdict |
|---|---|---|---|
| **HTTP-01** (RFC 8555 §8.3) | yes — CA must reach `:80` on the host | no | **rejected** — host may have `:80` unreachable / NAT'd |
| **TLS-ALPN-01** (RFC 8737) | yes — CA must reach `:443` w/ a special ALPN | no | **rejected** — same reachability problem; collides with the served listener |
| **DNS-01** (RFC 8555 §8.4) | **no** — prove control by publishing a TXT record | **yes** | **chosen** |

DNS-01 is the *only* challenge that (a) works with no open inbound port / behind NAT, and (b) can
issue **wildcards** (`*.mv.example.com`). Both are hard requirements here. Sources:
[RFC 8555 §8.4](https://www.rfc-editor.org/rfc/rfc8555#section-8.4),
[RFC 8737 (TLS-ALPN-01, for context on why excluded)](https://datatracker.ietf.org/doc/html/rfc8737).

**DNS-01 mechanics (RFC 8555 §8.1/§8.4):** for each identifier `D`, publish a **TXT** record at
`_acme-challenge.<D>` whose value is `base64url(SHA-256(keyAuthorization))`, where
`keyAuthorization = token || "." || base64url(JWK_thumbprint(accountKey))`. For a wildcard
`*.example.com` the identifier is `example.com`, so the record is `_acme-challenge.example.com`. The
apex + wildcard in one order publish **two** distinct TXT records *at the same name* — both must be
created and both record-ids tracked for cleanup.

## 2. Chosen ACME crate — `instant-acme` 0.8.5

| Crate | Version / date | License | Async+rustls | DNS-01 | Verdict |
|---|---|---|---|---|---|
| **`instant-acme`** | **0.8.5** (2026-02-24) | **Apache-2.0** | tokio + rustls 0.23, no openssl | **first-class** (`ChallengeType::Dns01`) | **CHOSEN** |
| `rustls-acme` / `tokio-rustls-acme` | 0.15.x (2026-06) | MIT OR Apache-2.0 | yes | **none** — TLS-ALPN-01 / HTTP-01 only | **rejected (the trap)** |
| `acme2` | 0.5.1 (2022-05) | MIT | async but **openssl** | yes | rejected (openssl + stale) |
| `acme-lib` | 0.9.1 (2024-01) | MIT | **sync (ureq)** + openssl | yes | rejected (sync + openssl) |

`rustls-acme` *looks* ideal (rustls-ecosystem, very recent) but is architecturally built around
TLS-ALPN-01 with no DNS-01 path — it is **disqualified** by the DNS-01-only rule. `instant-acme` is
Apache-2.0 (clean for the allowlist), pure-Rust async on **tokio + rustls 0.23** (no native TLS), and
intentionally leaves TXT publishing to the caller — exactly the seam our pluggable provider plugs into.
Verified on [docs.rs/instant-acme 0.8.5](https://docs.rs/instant-acme/latest/instant_acme/) and
[crates.io](https://crates.io/crates/instant-acme) (Apache-2.0; `rustls ^0.23`, `tokio ^1.22`).

**It computes the TXT value for you:** `KeyAuthorization::dns_value()` returns the already-hashed
base64url string — the provider never computes SHA-256, it just publishes the name/value.
*Confidence:* version/license/rustls/tokio = **high** (re-verified this session). Exact 0.8.x method
*signatures* have churned across point releases — pin 0.8.5 and re-check against locked docs at wiring time.

## 3. The pluggable `DnsProvider` trait — Cloudflare first

The whole point is "providers added incrementally." DNS-01 needs exactly two operations (publish,
delete) plus an optional propagation wait. The trait is **object-safe** (`Box<dyn DnsProvider>` /
`Arc<dyn DnsProvider>` selected from config), uses `async_trait` today, returns an **opaque concrete
newtype handle** (NOT `dyn Any` — repo guardrail) so each provider tracks its own record identity
(Cloudflare `record_id`, Route53 change-id, RFC2136 nothing):

```rust
#[derive(Clone, Debug)]
pub struct TxtRecordHandle(String);   // provider-owned; e.g. Cloudflare record id

#[derive(Clone, Debug)]
pub struct ChallengeRecord {
    pub zone: String,   // registrable zone, e.g. "example.com"
    pub fqdn: String,   // "_acme-challenge.example.com"
    pub value: String,  // from KeyAuthorization::dns_value()
}

#[async_trait::async_trait]
pub trait DnsProvider: Send + Sync {
    fn id(&self) -> &'static str;                                  // "cloudflare"
    async fn create_txt_record(&self, r: &ChallengeRecord)
        -> Result<TxtRecordHandle, DnsProviderError>;
    /// Idempotent: deleting an already-absent record is Ok(()) (renewal robustness).
    async fn delete_txt_record(&self, r: &ChallengeRecord, h: &TxtRecordHandle)
        -> Result<(), DnsProviderError>;
    /// Bounded propagation wait against AUTHORITATIVE NS. Default = no-op.
    /// MUST honour `deadline` and never block past it (invariant #10).
    async fn wait_for_propagation(&self, _r: &ChallengeRecord, _deadline: std::time::Instant)
        -> Result<(), DnsProviderError> { Ok(()) }
}
```

Design notes (consensus across lanes):
- **`delete_txt_record` is idempotent** so a crash-and-renew cycle that re-runs cleanup can't error.
- **`wait_for_propagation` takes a `deadline`, not a duration** — the caller owns the time budget,
  which is how the isolation invariant is enforced at the type level.
- **`create_txt` *adds*, doesn't overwrite** — apex+wildcard publish two records at one name.
- Route53 / Google Cloud DNS / RFC2136 each drop in as **one new `impl` + one config variant**, zero
  churn to the ACME engine.

### 3.1 Cloudflare provider (first impl) — call REST directly with `reqwest`+rustls

**Do NOT use the official `cloudflare` crate** (`cloudflare-rs` v0.14, Mar 2025): it is
**BSD-3-Clause** (would force widening the cargo-deny allowlist for a single thin dep) and defaults to
`native-tls`→openssl. The DNS-01 surface is tiny (3 endpoints + a `{success,errors,result}` envelope),
so a hand-written client over **`reqwest { default-features = false, features = ["json","rustls-tls","http2"] }`**
is less code and keeps the allowlist untouched and openssl-free.
Source: [`cloudflare` crate (BSD-3-Clause)](https://crates.io/crates/cloudflare).

Cloudflare API v4, `Authorization: Bearer <token>` on every call
([create](https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/create/),
[list zones](https://developers.cloudflare.com/api/resources/zones/methods/list/),
[Bearer token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/)):

| Step | Call |
|---|---|
| Resolve zone id (only if not pinned in config) | `GET /zones?name=<zone>` → `result[0].id` |
| Publish TXT | `POST /zones/{zone_id}/dns_records` body `{"type":"TXT","name":"_acme-challenge.<fqdn>","content":"<dns_value>","ttl":60}` → keep `result.id` |
| Cleanup | `DELETE /zones/{zone_id}/dns_records/{record_id}` (always, even on failed orders) |

`ttl:60` is Cloudflare's manual minimum — minimises cache staleness on the short-lived record.

**Least-privilege token (grounded):** **Zone → DNS → Edit** scoped to **one specific zone** (not "all
zones", never the legacy Global API Key). This is the documented minimum for certbot-dns-cloudflare /
cert-manager. **`Zone → Read` is needed only if** you resolve the zone id at runtime — avoid it by
**pinning `zone_id` in config** (recommended, tightest scope). Optional hardening: **Client IP filter**
on the token + a **TTL/expiry**. Token via 1Password/env, never committed (repo guardrail).
Sources: [permissions reference](https://developers.cloudflare.com/fundamentals/api/reference/permissions/),
[certbot-dns-cloudflare](https://certbot-dns-cloudflare.readthedocs.io/),
[cert-manager Cloudflare](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/).

### 3.2 Propagation footgun (grounded)

Cloudflare's API returns `200` **before** all its authoritative edge nameservers serve the record, and
Let's Encrypt validates against **authoritative** NS. **Poll the zone's authoritative nameservers
directly** (resolve the zone `NS`, query each for the TXT) until `dns_value` appears on all of them or a
deadline (default 120 s, exp backoff ~2→5→10 s) **before** `set_challenge_ready`. Query authoritative
NS, **not a recursive resolver** — a recursive resolver can negative-cache `NXDOMAIN` for
`_acme-challenge` before the record exists and poison the validation. Validating early also burns the
**5 failed-validations/identifier/hour** rate limit. Source:
[Cloudflare community — new TXT records don't immediately resolve via authoritative NS](https://community.cloudflare.com/t/dns-01-acme-challenge-new-txt-records-created-via-api-dont-resolve-via-authoritat/874877).

## 4. rustls termination + hot cert reload — `axum-server` (`tls-rustls`)

TLS serving glue is **`axum-server`** with the `tls-rustls` feature. It matches the as-built
pre-bound-listener model and provides built-in hot reload (verified this session on
[`RustlsConfig` docs.rs](https://docs.rs/axum-server/latest/axum_server/tls_rustls/struct.RustlsConfig.html);
license **MIT**):

- `from_tcp_rustls(std_listener, RustlsConfig)` wraps an already-bound `std::net::TcpListener` (the
  current `serve()` owns a `tokio::net::TcpListener` → `.into_std()`), preserving `local_addr()`/`:0`
  ephemeral-bind testability.
- `RustlsConfig` holds an `arc-swap`'d `Arc<rustls::ServerConfig>`; **`reload_from_pem(cert, key)` /
  `reload_from_pem_file(...)` / `reload_from_config(Arc<ServerConfig>)` / `reload_from_der(...)`** swap
  the served cert **atomically**. New handshakes use the new cert; **in-flight connections keep their
  already-negotiated session** (TLS session lifecycle + arc-swap semantics) — no listener restart, no
  dropped connections, no output disruption.
- `axum_server::Handle` drives graceful shutdown (replaces `with_graceful_shutdown`).

> `axum-server` is **MIT-only** (not dual MIT/Apache). It is on the allowlist so it's fine — flagged
> for awareness in the ADR.

rustls 0.23 arrives transitively via both `instant-acme` and `axum-server`. Select the **same crypto
provider** (`ring` or `aws-lc-rs`) across rustls + instant-acme to avoid double provider init; both are
Apache/ISC/MIT-class, not openssl.

## 5. Renewal & isolation model (invariant #10 — the heart of this design)

ACME ordering, the `DnsProvider` calls, and the renewal timer all live on a **detached
`tokio::spawn` background task owned by `multiview-control`** — never on the engine/output path. It
holds **nothing** the output clock needs: only a clone of the control plane's `RustlsConfig`. Structurally
it is a sibling of `bind_and_serve`'s spawned `serve` task.

**Account (one-time, persisted).** `Account::create(...)` → persist the serde `AccountCredentials`
(account key + kid) to `${state}/acme/account.<env>.json`, mode `0600`. On restart
`Account::from_credentials` — **never re-register** (account-registration rate limit ~10/IP/3h; a new
account per renewal is a classic bug). Staging and prod account files are **separate** (a staging key
is meaningless against prod). Config optionally carries `eab_kid`/`eab_hmac_key` so non-LE CAs
(ZeroSSL / Google Trust Services) drop in later.

**Per-cert flow (issuance and each renewal).** `new_order` (wildcards OK) → walk authorizations → take
`Dns01` → `key_authorization().dns_value()` → `provider.create_txt_record` → **wait for propagation
(authoritative NS, bounded)** → `set_challenge_ready` → poll `order.refresh()` with bounded exp backoff
+ hard timeout → on `Ready` finalize (fresh cert keypair via CSR, e.g. `rcgen`, Apache-2.0/MIT) → poll
`order.certificate()` → **`provider.delete_txt_record` cleanup in a finally/Drop** → atomic-install the
PEM (temp file in same dir, `fsync`, set `0600`, `rename()` over live path) → `RustlsConfig::reload_from_pem`.

**Schedule.** Renew when remaining lifetime `< ⅓` of validity (90-day LE cert → ~day 60 / ~30 days
left), threshold **lifetime-relative** (LE's announced ~6-day certs → renew at ~2 days left), plus
**jitter** (±hours) to avoid stampeding the CA. Design the scheduler to *optionally* consult ARI
(RFC 9773 `renewalInfo`) later; ship the ⅓-lifetime heuristic now.

**Failure = keep serving the existing cert.** On any error: `tracing::warn!` + telemetry, **retry with
bounded exponential backoff (cap 6–12 h)** capped well under 5 failed-validations/identifier/hour — the
live `RustlsConfig` is untouched, so serving continues on the old (still-valid) cert until real expiry.
A failed renewal **never** drops TLS, restarts the listener, panics, or touches the engine. The
**output clock cannot stall** because nothing on the data plane awaits this task or shares a lock with
it. The existing CI chaos/isolation gate gains an assertion: **a stalled/failing ACME order leaves the
program output and the existing TLS listener unaffected.**

**Startup ordering.** If no valid cert exists at boot in `acme` mode, bring the **engine up first**, run
ACME provisioning concurrently, and start the TLS listener only when the cert lands (log progress) —
never block the process or the engine on issuance.

## 6. Rate limits, staging-first, CAA (grounded)

- **Directories** (config-driven, default **staging** in non-prod):
  staging `https://acme-staging-v02.api.letsencrypt.org/directory`,
  prod `https://acme-v02.api.letsencrypt.org/directory`. Promotion to prod is an explicit reviewed
  action. CI prefers **Pebble** over staging. Staging roots are untrusted (expected).
  Sources: [LE staging](https://letsencrypt.org/docs/staging-environment/).
- **Prod rate limits to respect** ([LE rate-limits](https://letsencrypt.org/docs/rate-limits/)):
  ~50 certs/registered-domain/week; **5 duplicate certs (exact identifier set)/week** — the limit a
  renewal-loop bug hits first, so the scheduler must be **idempotent** and renew on a threshold, not on
  every boot; 5 failed-validations/identifier/hour.
- **CAA (operator prerequisite, manual zone record):** pin issuance with
  `example.com. CAA 0 issue "letsencrypt.org"` (+ `issuewild` per the wildcard decision). Harden with
  **RFC 8657 account binding**:
  `0 issue "letsencrypt.org; validationmethods=dns-01; accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<ID>"`
  so only *your* account, via *DNS-01*, can issue. Add an `iodef mailto:` for incident reports.
  **Verify your account ID before publishing or you lock yourself out.** Sources:
  [LE CAA](https://letsencrypt.org/docs/caa/), [RFC 8657](https://datatracker.ietf.org/doc/html/rfc8657).
  *Confidence:* prod CAA/8657 support **high**; the CA/B "mandatory by ~2027" date is from a secondary
  source — treat as **medium**, not load-bearing here.

## 7. Threat model (DNS-01-specific)

The Cloudflare token can write zone TXT → token compromise → an attacker passes DNS-01 at *any* CA →
**mis-issuance** for your domain/wildcard. Mitigations: least-privilege single-zone token + IP filter +
TTL (§3.1); **CAA `issue`+`accounturi`+`validationmethods=dns-01`** (§6) restricts issuance to your LE
account via DNS-01 only; mandatory TXT cleanup; **CT-log monitoring** (crt.sh / certspotter) on the
control-plane FQDN as an out-of-band tripwire; account key `0600` + off the data plane. **Prefer a SAN
(explicit-host) cert** over a wildcard where hostnames are known — a wildcard key on the box is a
credential for *every* subdomain; use wildcard only when hostnames are dynamic and justified.

## 8. On-disk state (uid 10001, container)

Per `deploy/Dockerfile:128-136` the container runs **non-root uid 10001**, config at `/etc/multiview`
(may be read-only), mutable state at `/var/lib/multiview`. ACME state therefore lives under
**`/var/lib/multiview/acme/`** (writable, uid-10001), separate from the config tree:

| Artifact | Path | Owner | Mode |
|---|---|---|---|
| Account creds | `…/acme/account.<env>.json` | 10001 | 0600 |
| Private key | `…/acme/<domain>/key.pem` | 10001 | 0600 |
| Cert chain | `…/acme/<domain>/fullchain.pem` | 10001 | 0644 (public) |
| Dirs | `…/acme/`, per-domain | 10001 | 0700 |

Atomic install (temp + `fsync` + mode-before-`rename`); key never world/group-readable, never logged,
never baked into image layers.

## 9. Off-by-default / LGPL-clean

TLS is opt-in by config **and** an off-by-default Cargo feature on `multiview-control`
(`tls` → `axum-server/tls-rustls`; `acme` → `instant-acme` + `reqwest/rustls-tls`), wired into the
`multiview-cli` aggregate flags. Default `cargo check` stays plain-HTTP, openssl-free, LGPL-clean. The
one verification point: ensure `reqwest`'s TLS backend is **rustls**, not native-tls — set
`default-features = false` and the `rustls-tls` feature explicitly; confirm in `Cargo.lock` and
`cargo deny check`.

## 10. Sources
- [RFC 8555 (ACME) §8.4 DNS-01](https://www.rfc-editor.org/rfc/rfc8555#section-8.4) · [RFC 8737 (TLS-ALPN-01)](https://datatracker.ietf.org/doc/html/rfc8737) · [RFC 8657 (CAA account binding)](https://datatracker.ietf.org/doc/html/rfc8657)
- [instant-acme docs.rs (0.8.5, Apache-2.0, rustls 0.23, tokio 1.22)](https://docs.rs/instant-acme/latest/instant_acme/) · [crates.io](https://crates.io/crates/instant-acme) · [`KeyAuthorization::dns_value`](https://docs.rs/instant-acme/latest/instant_acme/struct.KeyAuthorization.html) · [provision example](https://github.com/instant-labs/instant-acme/blob/main/examples/provision.rs)
- [rustls-acme docs.rs (TLS-ALPN-01/HTTP-01 only — rejected)](https://docs.rs/rustls-acme/latest/rustls_acme/)
- [axum-server RustlsConfig (MIT; reload_from_pem*, from_tcp_rustls)](https://docs.rs/axum-server/latest/axum_server/tls_rustls/struct.RustlsConfig.html)
- [`cloudflare` crate (BSD-3-Clause — rejected)](https://crates.io/crates/cloudflare)
- Cloudflare: [create DNS record](https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/create/) · [list zones](https://developers.cloudflare.com/api/resources/zones/methods/list/) · [create token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/) · [permissions reference](https://developers.cloudflare.com/fundamentals/api/reference/permissions/) · [propagation gotcha](https://community.cloudflare.com/t/dns-01-acme-challenge-new-txt-records-created-via-api-dont-resolve-via-authoritat/874877)
- [certbot-dns-cloudflare (min Zone:DNS:Edit)](https://certbot-dns-cloudflare.readthedocs.io/) · [cert-manager Cloudflare](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/)
- Let's Encrypt: [rate limits](https://letsencrypt.org/docs/rate-limits/) · [staging](https://letsencrypt.org/docs/staging-environment/) · [CAA](https://letsencrypt.org/docs/caa/)
