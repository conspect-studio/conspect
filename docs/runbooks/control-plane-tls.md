# Runbook: control-plane TLS (HTTPS) — TLS-0 static certificate

Operational how-to for serving the Multiview management plane (REST/WS/SSE API,
`/docs`, the SPA) over **HTTPS** with an operator-managed certificate. This is
the **TLS-0** floor of [ADR-W029](../decisions/ADR-W029.md): a static cert + key
you manage; no ACME / automatic renewal (a later phase).

Absent `[control.tls]`, the control plane serves **plain HTTP** exactly as before
— TLS is opt-in.

## Prerequisites

- A `multiview` binary **built with the `tls` feature**. It is included in the
  shipped umbrella presets (`nvidia`, `apple`, `linux-vaapi`, `full`); for a
  custom build add `--features tls` (or `--features <preset>`).
  - A binary **without** `tls` that is given a `[control.tls]` config **fails
    startup loudly** — it never silently downgrades to plain HTTP.
- A PEM **certificate chain** (leaf first, then intermediates) and its **private
  key** (PKCS#8, PKCS#1, or SEC1), readable by the `multiview` process user.

## Configure

Add a `[control.tls]` table to the `[control]` section of your config:

```toml
[control]
listen = "[::]:8080"          # dual-stack IPv6 (accepts IPv4-mapped too)

[control.tls]
mode = "static"
cert_file = "/etc/multiview/tls/fullchain.pem"
key_file  = "/etc/multiview/tls/privkey.pem"
```

- `mode = "static"` is the only TLS-0 mode. `cert_file`/`key_file` are filesystem
  paths.
- Validation at config load rejects an **empty** `cert_file`/`key_file` path. The
  files
  themselves are **read at serve time** (so a config can be authored off the
  deployment host); a missing/unreadable/garbage cert or key **aborts startup**
  with a clear error — it never falls back to plain HTTP.
- The listener bind is unchanged: keep `listen = "[::]:PORT"` for dual-stack
  IPv6-first (conventions §10). TLS terminates on the same socket.

Validate before deploying:

```bash
multiview validate --config /etc/multiview/config.toml
```

## Generate a certificate

**Production:** use your CA / internal PKI and point `cert_file`/`key_file` at the
issued PEM files. Concatenate the leaf + intermediates into the `cert_file` (leaf
first).

**Lab / self-signed** (browsers/clients will warn — expected):

```bash
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout /etc/multiview/tls/privkey.pem \
  -out    /etc/multiview/tls/fullchain.pem \
  -days 365 -subj "/CN=multiview.local" \
  -addext "subjectAltName=DNS:multiview.local,IP:::1,IP:127.0.0.1"
```

## Verify

```bash
# Handshake + served docs (self-signed ⇒ add -k to skip verification):
curl -kIv https://[::1]:8080/api/v1/openapi.json

# Inspect the presented certificate:
openssl s_client -connect [::1]:8080 -servername multiview.local </dev/null 2>/dev/null \
  | openssl x509 -noout -subject -dates
```

A successful TLS handshake plus a `200` on `/api/v1/openapi.json` confirms the
termination is live. The [SEC-14](../decisions/ADR-W028.md) per-IP rate limit and
per-key limit continue to apply under TLS (the peer IP is preserved through the
TLS accept loop) — this is covered by the `tls_serve` end-to-end test.

## Rotate / renew

TLS-0 has **no hot-reload**: replace the `cert_file`/`key_file` PEMs and
**restart** the daemon. Plan rotation before expiry; monitor `NotAfter` (the `openssl x509
-dates` output above).

## Troubleshoot

| Symptom | Cause / fix |
| ------- | ----------- |
| Startup error: `control.tls configured but this build lacks the 'tls' feature` | Rebuild/deploy with `--features tls` (or a preset), or remove `[control.tls]` to serve plain HTTP. |
| Startup error reading/parsing the cert or key | Path wrong, wrong permissions for the process user, or not PEM. Check the leaf-first chain order and that the key is PKCS#8/PKCS#1/SEC1 PEM. |
| Startup error: certificate/key mismatch (rustls) | The `key_file` does not correspond to the `cert_file` leaf — reissue or repair the pair. |
| Client `connection reset` / handshake failure | The client is speaking plain HTTP to the TLS port (use `https://`), or an ancient TLS-1.0/1.1-only client (TLS 1.2+ only). |
| Credentials still in cleartext | `[control.tls]` absent, or a plain-HTTP reverse proxy in front terminates TLS and re-origins over HTTP — terminate TLS here or secure the proxy hop. |

## Scope / limits

- **No ACME / auto-renew** in TLS-0 (a later phase); rotation is manual + restart.
- **Slowloris** (half-open slow-header floods) remain out of scope for the
  in-process floor (as for plain HTTP, [ADR-W028](../decisions/ADR-W028.md)); front
  the plane with a reverse proxy if that is a concern (tracked as follow-up #126).
- Reverse-proxy TLS termination stays fully supported — TLS-0 makes in-process
  HTTPS possible without a proxy, it does not require dropping one.
