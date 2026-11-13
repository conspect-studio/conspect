# Runbook: front-door ACME certificates (ADR-0107 section 5a)

The engine's front door uses ACME certificates and nothing else (operator directive, ADR-0107
Decision H). This runbook covers configuring `[network.tls]`, the first-boot setup flow, secrets,
alarms, recovery from the host CLI, and the manual staging check. Design:
[network-front-door.md section 5a](../architecture/network-front-door.md#5a-certificates-tls-is-required-and-it-is-acme).
The older operator-managed certificate path is [control-plane-tls.md](control-plane-tls.md)
(`[control.tls] mode = "static"`). It still works and stays until #282 is decided.

## How it works (slices 4 and 5)

* Builds: the `acme` cargo feature, which is in the `nvidia`, `apple`, `linux-vaapi` and `full`
  presets. A config with `[network.tls]` in a build without `acme` refuses to start, and
  `multiview validate` reports the same reason.
* At `multiview run` start, with `network.public_host` + `[network.tls]`:
  * A fresh bootstrap certificate is created (ECDSA P-256, SAN = `public_host`, 7 days). Its key is
    written to `<state_dir>/bootstrap.key` (0600). Its SHA-256 fingerprint is logged at `info`.
  * The ACME service starts on its own thread. A valid stored certificate (`active.pem`) is
    installed first, without contacting the CA. Otherwise issuance starts at once, with no UI step.
    While still in `setup`, the bootstrap certificate is rotated every 6 days.
  * A one-time setup code is printed on stderr (the service journal). It never goes through the
    log ring, so `GET /api/v1/logs` cannot return it.
  * The output clock, inputs and outputs run in both states. Nothing on the clock path waits for
    the CA.
* The `[control] listen` listener is the loopback admin listener. It is not gated, so the CLI
  always works. With `[network.tls]` it also carries `GET /api/v1/tls/status` and the loopback-only
  admin routes (`setup-code`, `credential`, `issue`).
* With `network.https_port` (the UDP front door), the QUIC/MoQ listener is held until the
  certificate is `serving`. It then starts on the ACME certificate, and renewals reach new
  handshakes without a restart. Until then MoQ leases answer `503`. With `[network.tls]`,
  `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY` are ignored (warned) on the front door; only
  `MULTIVIEW_MOQ_ENDPOINT` and `_PROTOCOL` are read.
* The TCP door (slice 4) needs `network.mode` (`"direct"` or `"behind_proxy"`) as well as
  `network.https_port`. Without `mode` there is no TCP listener at all: the setup page and
  TLS-ALPN-01 are then unreachable from outside, and the engine warns at boot. With `mode` and
  `[network.tls]`, the door:
  * serves the ACME certificate source as its site certificate (ALPN `http/1.1`): the bootstrap
    certificate in `setup`, the issued certificate once `serving`. A renewal reaches the next
    handshake without a restart. `[network.tls]` wins over `[control.tls]` for the door; if both are
    set, a warning says so, and `[control.tls]` applies to the `[control] listen` listener only;
  * answers TLS-ALPN-01: a `ClientHello` offering only ALPN `acme-tls/1` gets the challenge
    certificate for its SNI while an authorization is pending;
  * serves the control router behind the setup gate. In `setup` only `GET /tls-setup`,
    `GET /api/v1/tls/status`, `POST /api/v1/tls/setup/session` and `POST /api/v1/tls/issue`
    answer; `GET /` redirects to `/tls-setup`; everything else is `503`
    `/problems/tls-setup-required` with `Retry-After: 30`, and no HSTS header is sent. Once `serving`
    the gate is a no-op.
* Behind a proxy (`mode = "behind_proxy"`), the setup-code limiter and the session log key on the
  PROXY v2 source address (the real client), never the proxy's address. `multiview tls status`
  reports the hint "TLS-ALPN-01 needs TCP 443 to reach the engine; use DNS-01" when TLS-ALPN-01 is
  configured there.

## Configure

Secrets never go in the TOML, only environment-variable NAMES. Each name is looked up in the
environment first, then in `<state_dir>/secrets/<NAME>` (a 0600 file written by
`multiview tls set-credential`). `state_dir` must be absolute; it is created 0700.

Let's Encrypt, DNS-01 through Cloudflare (recommended):

```toml
[network]
public_host = "mda001.example.net"
https_port = 443
mode = "direct"                     # required for the TCP door; or "behind_proxy" (+ trusted_proxies)

[network.tls]
ca = "letsencrypt"
staging = false                     # true = the staging directory (untrusted chain), for trials
challenge = "dns-01"
domains = ["mda001.example.net"]   # must include public_host; "*.example.net" needs dns-01
contact_email = "ops@example.net"
acme_profile = "shortlived"         # optional; used only if the directory advertises it
state_dir = "/var/lib/conspect/tls"

[network.tls.dns]
provider = "cloudflare"
token_env = "CLOUDFLARE_API_TOKEN"
propagation_timeout_s = 120
# confirm = "auto"                  # auto (default) | authoritative | doh, see below
```

With the Cloudflare global key instead of a token:

```toml
[network.tls.dns]
provider = "cloudflare"
email_env = "CLOUDFLARE_EMAIL"      # env-var NAMES; values via `multiview tls set-credential`
api_key_env = "CLOUDFLARE_API_KEY"
```

The same with TLS-ALPN-01. No DNS credential is needed, but the CA must reach TCP 443 of this host
directly, so it needs `mode = "direct"` (not `behind_proxy`). Let's Encrypt IP-address
certificates need `acme_profile = "shortlived"`:

```toml
[network]
public_host = "mda001.example.net"
https_port = 443
mode = "direct"

[network.tls]
ca = "letsencrypt"
challenge = "tls-alpn-01"
domains = ["mda001.example.net"]
state_dir = "/var/lib/conspect/tls"
```

Google Trust Services. EAB is required, with either challenge:

```toml
[network.tls]
ca = "google"
staging = false
challenge = "dns-01"                # or "tls-alpn-01" (then drop [network.tls.dns])
domains = ["mda001.example.net"]
eab_kid_env = "GTS_EAB_KID"
eab_hmac_env = "GTS_EAB_HMAC"
state_dir = "/var/lib/conspect/tls"
[network.tls.dns]
provider = "cloudflare"
token_env = "CLOUDFLARE_API_TOKEN"
```

Create the Google EAB key in the Google Cloud project that owns the certificates:
`gcloud publicca external-account-keys create`. It prints `keyId` and `b64MacKey`; store them as
`GTS_EAB_KID` / `GTS_EAB_HMAC`. Unused keys expire after 7 days, and a key binds one ACME account.
For staging, first run
`gcloud config set api_endpoint_overrides/publicca https://preprod-publicca.googleapis.com/`.

A company's own ACME server (`custom`), with either challenge and optional EAB:

```toml
[network.tls]
ca = "custom"
directory = "https://acme.corp.example/acme/directory"
trust_anchor = "/etc/conspect/corp-root.pem"   # PEM bundle; ONLY these roots are trusted
# allow_private_directory = true               # only for a directory on loopback / RFC 1918 /
#                                              # ULA / CGNAT / localhost; needs trust_anchor
challenge = "tls-alpn-01"                       # or "dns-01" + [network.tls.dns]
domains = ["mda001.corp.example"]
# eab_kid_env = "CORP_EAB_KID"
# eab_hmac_env = "CORP_EAB_HMAC"
state_dir = "/var/lib/conspect/tls"
```

There is no "accept invalid certificates" switch. Browsers trust a private CA only where the
company installed its root.

### Cloudflare credentials

* **API token (recommended).** Create it in the dashboard under My Profile → API Tokens → Create
  Token → Custom. Give it permissions `Zone - DNS - Edit` and `Zone - Zone - Read`, with Zone
  Resources set to the zone(s) holding `domains`. Configure it as `token_env`.
* **Global API key.** Configure `email_env` + `api_key_env`. **Warning:** the global key can change
  everything on the Cloudflare account. Use it only if a token is impossible, and keep it in the
  host secret store.

At the start of every attempt the engine checks the credential (`/user/tokens/verify` and a zone
lookup). A rejected credential shows as `dns-credential-rejected`.

### How the TXT record is confirmed (`confirm`)

Before telling the CA to validate, the engine checks that the TXT record is visible:

* `authoritative`: ask every authoritative nameserver of the zone directly (UDP then TCP, port 53).
* `doh`: ask Cloudflare's resolver over DNS over HTTPS (`https://cloudflare-dns.com/dns-query`).
* `auto` (default): send one query to documentation addresses where nothing listens
  (`192.0.2.1`, `2001:db8::1`). Any answer means the network intercepts port 53, and the engine
  uses DoH; otherwise it asks the authoritative servers. The journal says which (`acme: DNS-01
  confirmation via ...`), and the choice is reused for 10 minutes.

**Interception symptoms.** `dns-propagation-timeout` after the full `propagation_timeout_s` while
the record is visible in the Cloudflare dashboard, and `dig @<cloudflare-ns> _acme-challenge.<name>
TXT` (or `dig @192.0.2.1 example.org SOA`, where nothing should answer) returns NOERROR with a bogus
SOA such as `a.misconfigured.dns.server.invalid`. Keep `confirm = "auto"` or set `confirm = "doh"`.

### CAA

If the zone has CAA records, allow the CA and the methods:

```text
example.net. CAA 0 issue "letsencrypt.org; validationmethods=dns-01,tls-alpn-01"
example.net. CAA 0 issue "pki.goog; validationmethods=dns-01,tls-alpn-01"
```

Check with `dig CAA example.net` and `dig NS example.net`.

## Secrets

Supply a secret in one of two ways:

* Put it in the service environment (for example a systemd `EnvironmentFile=` owned by root,
  mode 0600). Never on a command line.
* On the host, run:

  ```sh
  multiview tls set-credential CLOUDFLARE_API_TOKEN --config /etc/multiview/multiview.toml
  ```

  It reads the value from stdin, with echo off on a terminal; it never takes the value from argv.
  With the engine running, it sends the value to the loopback admin listener, which writes
  `<state_dir>/secrets/CLOUDFLARE_API_TOKEN` (0600, atomic). It then starts issuance at once; no
  restart is needed, because the credential is re-read at every attempt. With the engine stopped,
  it writes the file directly. Only names referenced by `[network.tls]` are accepted. The value is
  never logged, echoed or returned by any route.

The CLI talks to `[control] listen` over loopback only (`[::]` is dialled as `[::1]`). It
authenticates with `MULTIVIEW_CONTROL_TOKEN`, either `admin.<secret>` or the bare secret. Any other
listen address is refused.

## First boot: the setup flow

1. Start the engine. The log shows:

   ```text
   tls: front door in setup with a self-generated bootstrap certificate ... fingerprint=AB:CD:...
   multiview tls: one-time setup code (valid 1 h, single use): word word word word word word word word
   ```

2. If the config holds everything (DNS-01 with the credential present, or TLS-ALPN-01 with
   `mode = "direct"` and TCP 443 reachable), issuance has already started. Watch it with `multiview tls status`.
3. If something is missing, `multiview tls status` names it in its reasons, for example
   `credential-missing: CLOUDFLARE_API_TOKEN` (names only). Fix it with
   `multiview tls set-credential NAME`, which also triggers issuance.
4. From a browser (needs `network.mode`): open `https://<public_host>/`, which redirects to `/tls-setup`
   in setup. **Compare the fingerprint on the page with the one your browser shows and with the log
   or `multiview tls status`. If they differ, stop.** Paste the setup code and press "Issue now".
   The page accepts no secret, and its CA, challenge and names come from the config.

The setup code has 8 words (BIP-39 English list). It is valid 1 hour and works once. Mint a new one
on the host with `multiview tls setup-code`, which replaces the old code. A successful code gives a
1-hour `HttpOnly; Secure; SameSite=Strict` session that is honoured only in setup. After 5 failed
codes from one source (an IPv6 /64, or an IPv4 address) within 15 minutes, that source is locked
for 15 minutes.

In setup, every other request on the front door answers `503` `application/problem+json` with
`type` `/problems/tls-setup-required` and `Retry-After: 30`. There is no HSTS. When the certificate
lands, `setup` changes to `serving` without a restart and the setup surface is gone. `serving`
never falls back to the bootstrap certificate.

## Verify

```sh
multiview tls status                                                  # config of the running engine
multiview tls status --config /etc/multiview/multiview.toml          # table
multiview tls status --config /etc/multiview/multiview.toml --json   # machine-readable
openssl s_client -connect mda001.example.net:443 -servername mda001.example.net </dev/null \
  | openssl x509 -noout -issuer -dates -ext subjectAltName
stat -c '%a %n' /var/lib/conspect/tls /var/lib/conspect/tls/*      # 700 dir, 600 files
```

`status` shows the state (`setup` / `serving` / `expired`), the CA (and staging), the challenge,
the names, the expiry, when renewal is due, the next retry, the bootstrap fingerprint (setup only),
the alarm, the last error token, the `last error detail` and the reasons. If the engine is not
running, it reads `<state_dir>/active.pem` offline and says so.

Without `--config`, every `multiview tls` command uses the config of the one running
`multiview run <CONFIG>` engine (found through `/proc`, a relative path resolved against the
engine's working directory), else `/etc/multiview/multiview.toml` if it exists. The first status
line says which (`config  <path> (running engine pid N)` or `(default path)`). With several engines
running it lists their configs and asks for `--config`.

`last error detail` is the human reason behind the token: the CA's problem type and text (with
subproblems and the challenge's own error, for example `unauthorized: Failed to connect to
203.0.113.7:443 for the tls-alpn-01 challenge`), or a local explanation such as "TXT not seen via
DNS over HTTPS ... (port 53 intercepted)". It is also in the journal line `acme: issuance attempt
failed` (`detail=`), the setup page and `GET /api/v1/tls/status` (`last_error_detail`). It is
bounded to 512 bytes and never contains a credential.

## Alarms and renewal

* Renewal uses ARI when the CA offers it, otherwise two thirds of the lifetime (with jitter).
  Failures back off from 15 minutes, doubling up to 6 hours. The current certificate keeps serving
  throughout.
* While renewal is failing:
  * with under 50 % of the lifetime left, the alarm is **warning**;
  * with under 25 % left, it is **critical**;
  * past `notAfter` it is **expired**. The expired certificate is still served, since nothing
    downgrades silently.
* The alarm appears as the `tls-certificate` health warning (`GET /api/v1/health`, UI health
  panel), logged at warn/error. It clears when a renewal succeeds. It never carries a secret.

## Recovery (host CLI only, no browser needed)

* **Expired certificate.** `multiview tls status` shows `expired` and the last error. Fix the cause
  (credential, CAA, reachability), then run `multiview tls issue`.
* **Credential rotated or revoked.** Run `multiview tls set-credential NAME` with the new value. It
  overwrites the file and triggers issuance. If the secret comes from the environment, update the
  service environment and restart.
* **Lost or corrupt account key.** A missing account file, or one that does not parse, is replaced
  automatically: once the CA has accepted a NEW account, the unreadable bytes are kept as
  `account.<directory-host>-<hash>.json.bad-<unix ns>` (0600, the 4 newest kept) and the file is
  replaced. If the registration fails, the file is left exactly as it was. `multiview tls issue`
  starts an attempt now.
  To force a new account, stop the engine, delete `<state_dir>/account.<directory-host>-<hash>.json`
  and start it. A file with the wrong mode or owner is never replaced: fix it to 0600 and owned by
  the service user. With EAB, Google needs a fresh EAB key, because a key binds one account.
* **Changed CA.** Edit `[network.tls]` (`ca`, `directory`, EAB names) and restart. Accounts are
  stored per directory, so the old CA's account is untouched. The stored certificate is installed at
  start and keeps serving until the new CA issues. A changed CA or name set only triggers an
  immediate re-issue; the old certificate is not dropped.
* **Stuck in setup.** `multiview tls status` names the reason, for example:
  * `credential-missing: NAME`;
  * `ca-unavailable` (CA unreachable or refusing);
  * `ca-offers-no-supported-challenge`;
  * `dns-credential-rejected`;
  * `dns-propagation-timeout` (see "How the TXT record is confirmed"; `last error detail` says
    whether DoH or the authoritative servers were asked);
  * `challenge-failed` (`last error detail` carries the CA's reason);
  * `tls-alpn-responder-not-installed` (TLS-ALPN-01 waited 60 s for the front door's `acme-tls/1`
    responder: `network.mode` and `network.https_port` must be set so the TCP door runs);
  * `eab-unavailable`;
  * `egress-refused` (the CA resolves to a private address without `allow_private_directory`, or
    the trust anchor does not match).
* **New setup code.** `multiview tls setup-code`.
* **Roll back** to `[control.tls] mode = "static"`. Remove `[network.tls]` / `public_host` and
  restart. The ACME `state_dir` can stay; it is unused.

## Manual staging check (lead-only, not PR CI)

Use disposable names and short-lived scoped credentials from 1Password. Never put them in TOML, a
shell command line, a PR or an issue.

1. Choose a disposable FQDN in a delegated Cloudflare zone. Add CAA for `letsencrypt.org` and
   `pki.goog` with `validationmethods=dns-01,tls-alpn-01`. Check `dig CAA` / `dig NS`.
2. Put the scoped Cloudflare token and a Google **staging** EAB key in a root-owned 0600 service
   environment file.
3. Configure:

   ```toml
   ca = "letsencrypt"
   staging = true
   challenge = "dns-01"
   domains = ["$HOST"]
   acme_profile = "shortlived"
   ```

   plus `[network.tls.dns] token_env = "CLOUDFLARE_API_TOKEN"`, and under `[network]`
   `public_host = "$HOST"`, `https_port = 443` and `mode = "direct"`. Then:
   * Run `multiview validate`, start the engine, and run `multiview tls status`.
   * Confirm `setup` changes to `serving` without a restart and the output ticks continue.
   * Inspect the chain with `openssl s_client` (staging chains are not publicly trusted).
4. Repeat with `ca = "google"`, `staging = true`, `eab_kid_env`/`eab_hmac_env` and a fresh staging
   EAB key. For TLS-ALPN-01 (`mode = "direct"`), switch the challenge, drop `[network.tls.dns]`, and let
   the CA reach TCP 443.
5. Check the following:
   * `status` shows the CA, names and expiry, and no credential.
   * `state_dir` is 0700 and its files 0600.
   * A failing renewal keeps the active certificate.
   * Recovery by CLI works.

   Then revoke the disposable token and EAB key, remove the test DNS records and the host, and roll
   back the service.

References: [Let's Encrypt staging](https://letsencrypt.org/docs/staging-environment/),
[Let's Encrypt profiles](https://letsencrypt.org/docs/profiles/),
[Google Public CA tutorial](https://docs.cloud.google.com/certificate-manager/docs/public-ca-tutorial),
[cert-manager Cloudflare token scopes](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/).
