# Security Policy

Multiview is a live GPU video multiview engine that ingests many networked sources and
serves robust continuous output. Because it handles **source credentials**, exposes a
**management API**, and links **third-party media libraries**, security is treated as a
first-class concern. This document explains how to report vulnerabilities and the
security model of the management surface.

> Multiview project code is **source-available** under the **Multiview Source-Available
> Non-Commercial License** (© Aperim Pty Ltd; see [`LICENSE`](LICENSE)). See the licensing notes in
> [`docs/architecture/conventions.md`](docs/architecture/conventions.md) §7 for the
> FFmpeg/NDI/codec build-profile model summarised below.

---

## Reporting a Vulnerability

**Please do not open public GitHub issues for security vulnerabilities.**

Report privately via GitHub's **[Security Advisories](https://github.com/aperim/multiview/security/advisories/new)**
("Report a vulnerability") on this repository. This keeps the details embargoed until a
fix is available.

When reporting, include where you can:

- Affected component (e.g. `multiview-control` API, `multiview-input` ingest, preview endpoints).
- Affected version / commit and build feature flags (`ndi`, `gpl-codecs`, `webrtc`, …).
- Reproduction steps or a proof-of-concept, and the observed vs. expected behaviour.
- Impact assessment (auth bypass, RCE, credential disclosure, DoS of the output, …).

### What to expect

| Stage | Target |
|-------|--------|
| Acknowledgement of report | within **3 business days** |
| Initial triage & severity assessment | within **7 business days** |
| Fix or mitigation plan | communicated after triage; timeline scales with severity |
| Coordinated disclosure | after a fix ships, credit given unless you prefer anonymity |

Please allow a reasonable embargo period for a fix before any public disclosure. We
appreciate good-faith research and will not pursue researchers who follow this policy.

### Supported versions

Until a `1.0` release, only the **latest release / `main`** receives security fixes.

---

## Threat Model at a Glance

```mermaid
flowchart LR
  U["Operator / Admin (browser)"] -->|cookie session + CSRF| API
  M["Machine client / SDK"] -->|API key (Bearer)| API
  subgraph BIN["Single multiview binary (tokio)"]
    API["multiview-control<br/>axum · auth · RBAC · per-object authz"]
    API <-->|sqlx| DB[("SQLite WAL<br/>hashed keys · encrypted creds")]
    API --> ENG["multiview-engine<br/>(isolated, no client back-pressure)"]
    ENG --> PRV["multiview-preview<br/>token-gated taps"]
  end
  API -.->|source URLs may carry creds| SRC["RTSP / SRT / RTMP / NDI sources"]
  V["Preview viewer"] -->|short-lived signed token over WSS/HTTPS| PRV
```

The control plane, preview, and realtime layers are **physically incapable of
back-pressuring the engine** (invariant 10 in the conventions). A compromised or abusive
client cannot stall or crash the protected output path.

---

## Management API Security Model

The management API is the `multiview-control` crate (axum 0.8 + tower-http), served from the
same single binary as the engine. See the deep brief at
[`docs/research/web-api-stack.md`](docs/research/web-api-stack.md) and the decision record
[`docs/decisions/ADR-W005.md`](docs/decisions/ADR-W005.md) for the auth design.

### Authentication — dual credential

| Caller | Credential | Storage / handling |
|--------|-----------|--------------------|
| **Web UI** | `tower-sessions` signed+encrypted private cookie (`HttpOnly` + `Secure` + `SameSite`) | server-side session; CSRF synchronizer token required on state-changing requests |
| **Machine / SDK** | long random **API key** as `Bearer` token | stored as **SHA-256/HMAC** hash (high entropy → no slow KDF); shown **once** at creation, revocable |
| **Human passwords** (if enabled) | password | hashed with **Argon2id** (OWASP params) |

- `SameSite` cookies alone are **not** treated as sufficient CSRF defence; a CSRF
  synchronizer token is enforced on mutating routes.
- Authentication endpoints are rate-limited (`tower-governor`) to blunt credential
  stuffing and brute force.
- **OIDC/SSO** is an optional add-on (`axum-oidc`), not the default.

### Authorization — RBAC plus per-object checks (BOLA)

- Static roles **admin / operator / viewer** via `axum-login` (`permission_required`).
- **BOLA (OWASP API1: Broken Object-Level Authorization) is the #1 risk.** Role gating is
  not enough: every request that names a resource id — source, output, template, preview —
  performs a **per-object authorization check**, not merely a role check. This is an
  invariant (conventions §6) and must hold on every handler.

### Concurrency / integrity

- Each mutable resource carries a monotonic version surfaced as an `ETag`; mutations
  require `If-Match` and return **412** on mismatch, preventing two operators from
  silently clobbering each other's layout/config.
- Long-running reconfiguration returns **202 + operation id**; the result arrives on the
  realtime event stream after the engine applies it at a frame boundary.

### Transport (TLS)

| Deployment | TLS path |
|------------|----------|
| Internet-exposed | front with **Caddy** (automatic Let's Encrypt) |
| Air-gapped / on-prem | built-in **rustls** self-signed or operator-supplied cert |

- **CORS** is locked to the application origin. Never use `*` with credentials; because
  the API docs are served same-origin, only minimal CORS is required.
- All credentialed traffic (sessions, API keys, preview tokens) must travel over
  HTTPS/WSS.

### Preview token gating

Live preview is **isolated from the program path** (`multiview-preview`) and is **not**
open by virtue of an API session alone:

- Preview access (WHEP sub-second and MJPEG/JPEG fallback) is gated by **short-lived,
  signed tokens** delivered over HTTPS/WSS.
- Preview taps **auto-stop when there are no subscribers**, limiting exposure and load.

See the preview deep brief at
[`docs/research/preview-subsystem.md`](docs/research/preview-subsystem.md).

---

## Secrets Handling

Source and output endpoints frequently embed credentials (RTSP/SRT/RTMP/NDI). Multiview
treats these as sensitive throughout:

- Credentials are wrapped in **`secrecy::SecretString`** (zeroized on drop, redacted in
  `Debug`) so they do not leak into logs, traces, or panic messages.
- They are **encrypted at rest** in the SQLite store.
- The API is **write-only / masked**: credentials are accepted on create/update but are
  **never echoed back** in responses.
- **Config-as-code** export (`:export` / `:import`) carries secrets by **reference only**,
  not plaintext — see [`docs/decisions/ADR-M006.md`](docs/decisions/ADR-M006.md). Exported
  config is safe to commit to GitOps; resolve the referenced secrets out of band.

### ⚠️ Source URLs may contain credentials

A source URL such as `rtsp://user:pass@host/stream` carries the username and password
**inline**. Operators must be aware that:

- Such URLs are sensitive even though they look like ordinary configuration.
- Avoid pasting full source URLs into bug reports, screenshots, logs, or chat. Redact the
  userinfo component (`user:pass@`) before sharing.
- Multiview masks these in API responses and redacts them in logs, but **anything you copy
  manually is your responsibility.**

---

## Supply Chain: FFmpeg, NDI, and Codecs

Multiview builds in **off-by-default native/FFI features** so the default `cargo check` is a
pure-Rust, LGPL-clean, no-native-deps build (conventions §3–§4, §7).

- **FFmpeg / libav\*** (`ffmpeg` feature): linked **LGPL** in the default profile. No
  `--enable-gpl`/`--enable-nonfree`; **no x264/x265/libnpp** by default (scaling and
  compositing are done in-house). Keep the linked FFmpeg patched — media-parsing libraries
  are a classic source of memory-safety CVEs, and untrusted source streams reach the
  demux/decode path.
- **`gpl-codecs`** feature: opt-in only; pulling x264/x265 makes the resulting build
  **GPL**. Do not enable unless you accept that license obligation.
- **NDI SDK** (`ndi` feature): **proprietary** (royalty-free, attribution required,
  redistribution restricted). It is **never vendored**; the feature uses a **runtime
  dynamic-load** path, and providing/patching the SDK/runtime is the **operator's
  responsibility**. Source it only from the official vendor and observe the EULA and
  attribution requirements.

CI enforces license and advisory hygiene via **`cargo-deny`** (`deny.toml`), which gates
vulnerable/yanked dependencies and disallowed licenses.

---

## Hardening Checklist for Operators

- [ ] Serve the API and preview exclusively over **TLS** (Caddy for public, rustls cert for air-gapped).
- [ ] Create per-client **API keys** with least privilege; rotate and revoke promptly.
- [ ] Restrict network exposure of the management port; do not expose it to the public internet without a reverse proxy and authentication in front.
- [ ] Treat **source URLs as secrets**; rely on write-only/masked credential handling and reference-only config export.
- [ ] Keep the linked **FFmpeg** and the **NDI runtime** patched.
- [ ] Run `cargo-deny` in CI; review advisories before upgrading dependencies.
- [ ] Restrict preview access to authorized viewers; rely on short-lived signed tokens and no-subscriber auto-stop.

---

*For naming, feature flags, invariants, and licensing that govern this policy, the source
of truth is [`docs/architecture/conventions.md`](docs/architecture/conventions.md).*
