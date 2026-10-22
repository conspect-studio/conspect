# ADR-M013: Outbound-dial SSRF guard for managed devices + Cast sessions

- **Status:** Accepted
- **Area:** Management
- **Date:** 2026-07-10
- **Source:** security audit (SSRF review of `multiview-control`, findings SEC-02 CRITICAL + SEC-04 MEDIUM, CWE-918/CWE-20)

## Context

The control plane dials operator-supplied addresses on two paths:

- The managed-device poller (`zowietek` driver) `POST`s to `<device.address>/…`
  over reqwest, sending the device's resolved `(username, password)` in cleartext
  ([ADR-M009](ADR-M009.md)).
- The ad-hoc Cast session actor opens TCP+TLS to `POST /api/v1/cast/sessions`'s
  `address` ([ADR-M011](ADR-M011.md)).

Before this ADR the address was only shape-checked: `Device::validate`
(`multiview-config`) rejected an empty string; `start_cast_session` only ran
`split_authority`. A **Write-role** principal (or a stolen Write API key) could
therefore point either dial at:

- **SEC-02 (CRITICAL):** `http://169.254.169.254/…` — the cloud-metadata (IMDS)
  endpoint — to steal instance credentials, and the reqwest client followed up to
  10 redirects, so even a validated host could 30x-bounce there. The device's
  named credentials are exfiltrated in cleartext to the attacker-chosen host.
- **SEC-04 (MEDIUM):** `[fd00::5]:6379` and similar — an internal TCP+TLS
  port-scan primitive (Online/Unreachable is a non-blind oracle).

The dial target is **orthogonal to the object-authz model**, so a scoped
operator escapes its object blast radius.

The binding constraint that shapes the fix: **managed devices legitimately live
on the LAN.** The canonical device example is a ULA address
(`http://[fd00:db8::42]`), and ~30 existing tests + the managed-devices brief use
private/ULA device addresses. A naive "block all private" would break the entire
device feature and the green suite. Invariant #10 (isolation) also holds: all of
this is control-plane, never touching the engine.

## Decision

A shared **two-layer** dial guard, `multiview_config::device::net_guard`,
consulted by config load and by every outbound-dial site.

**Layer 1 — config load (`Device::validate`, offline, deployment-independent).**
Reject only the **never-legitimate** literal dial targets — loopback, link-local
(incl. IMDS `169.254.169.254`), unspecified, multicast, broadcast, and their
IPv4-mapped-IPv6 forms — and, for a device-management address that carries a
URL scheme, enforce an `http`/`https` **scheme allowlist** (blocking
`file:`/`gopher:` gadgets; a scheme-less `host[:port]` authority, as discovery
emits, is accepted). Private/ULA/carrier LAN literals **pass** here — whether
they may be dialled is a runtime policy decision, not a config-syntax one.

**Layer 2 — dial time (`screen_ip`/`screen_resolved` on the address actually
dialled).** Screen against the operator `DialPolicy`, **fail-closed**. The default
(`allow_lan`) is **non-breaking**: private/ULA/carrier LAN ranges are dialable so
a self-hosted appliance keeps reaching its devices out of the box, while the
**never-legitimate** ranges (loopback, link-local incl. IMDS, unspecified,
multicast/broadcast, IPv4-mapped forms) are **always** refused — no default and no
allowlist can re-enable them. An operator allowlist **tightens** the dial: an
allowlistable range is then reachable only inside one of its CIDRs (closing
SEC-04's authenticated internal port-scan), and an empty allowlist denies every
allowlistable range. The caller dials only the vetted, **pinned** address.
Screening the **resolved/canonical** IP (not the raw hostname string) is what
defeats **DNS-rebind** and alt-encoded-literal bypasses. Applied at:

- `start_cast_session` — resolve + screen **before** the actor spawns (422, no
  record).
- `TlsCastConnector::connect` (`cast` feature) — resolve + screen + pin on
  **every** (re)connect.
- The `zowietek` reqwest client (`zowietek` feature) — screens **the address
  reqwest actually dials**: an IP-literal host (incl. alt-encoded IPv4 like
  `2130706433`/`0x7f000001` and IPv4-mapped IPv6, canonicalized with the same
  WHATWG `url` parser reqwest uses) is screened directly with `screen_ip`
  (reqwest dials a literal **without** a resolver hop, so a resolver-only screen
  misses it); a DNS name is screened + pinned by a custom `reqwest::dns::Resolve`
  that returns only the vetted answers. Plus `no_proxy()` (a system
  `HTTP(S)_PROXY` cannot dial the destination unscreened) and
  `redirect(Policy::none())`.

> **Mechanism correction (2026-07-10, re-drive after a 3-reviewer BLOCK).** The
> first cut screened only DNS *names* via the custom resolver. Because reqwest
> dials an IP-literal / alt-encoded URL **without invoking the resolver**,
> `http://10.0.0.8`, `http://2130706433/` (= 127.0.0.1) and
> `http://[::ffff:169.254.169.254]` reached the target unscreened — SEC-02 was
> not closed. The fix screens the canonical dialed IP (the `url`-parsed literal),
> mirroring the panel-approved `TlsCastConnector`, and a real-listener regression
> test (`dial_screen_tests`) exercises the actual connect path — asserting a
> loopback / decimal-encoded-loopback dial never reaches a bound `TcpListener`.

**Operator allowlist source.** `control.device_dial_allow` — a validated
`Option<Vec<String>>` of CIDRs on `ControlConfig` — is the config-as-code home
and the **primary** source; the `MULTIVIEW_DEVICE_DIAL_ALLOW` environment
variable (comma/whitespace-separated CIDRs, e.g. `192.168.0.0/16, fd00:db8::/32`)
remains a legacy fallback. Both are validated fail-closed (a malformed allowlist
denies every allowlistable range rather than widening). `AppState.dial_policy`
defaults to `allow_lan`. The never-legitimate ranges are **not** allowlistable.
`ipnet` (MIT/Apache-2.0, already in the lock) parses the CIDRs.

## Rationale

- **The CRITICAL vector is closed with zero feature breakage.** IMDS is
  link-local, so blocking the never-legitimate ranges (at both layers) stops
  cloud-credential theft without touching the LAN ranges real devices sit in.
- **Config validation stays deployment-independent.** A config valid on one host
  is valid on another; the LAN dial policy is a runtime concern. This keeps the
  ULA example + the existing device/cast suites green (no test-weakening,
  rule 19).
- **Resolved-IP screening is the only correct DNS-rebind defence** — a literal or
  hostname check at config load cannot see the rebind. Pinning the vetted IP for
  the dial closes the resolve→connect TOCTOU.
- **`redirect(Policy::none())`** prevents a validated host from bouncing the
  poller — and its cleartext credentials — to a blocked one.
- **Non-breaking default + an allowlist that tightens** is the right
  security-vs-usability line for a self-hosted LAN appliance: the CRITICAL vector
  (metadata / loopback / rebind) is closed unconditionally out of the box while
  LAN devices keep working; an operator who wants the stricter posture locks the
  dial to exactly their device subnet with `control.device_dial_allow`, closing
  SEC-04. Screening the **canonical dialed IP** (not the raw hostname) is what
  makes this robust against alt-encoded literals and DNS-rebind alike.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Block all private/ULA at `Device::validate` | Breaks the managed-device feature (devices live on the LAN) and ~30 green tests + the ULA brief example; config load would fail on upgrade. |
| Guard only at config load (literal/hostname screen) | Cannot defeat DNS-rebind (the name resolves differently at dial), and exotic literal encodings (`0x7f.0.0.1`, `2130706433`) differ from what the dialer resolves. The resolved-IP screen is differential-proof. |
| Hard cast port allowlist (8009 only) | Cast **groups** advertise dynamic ephemeral ports; a strict allowlist breaks multizone. The resolved-IP screen already blocks SEC-04's `[fd00::5]:6379` (the host is ULA), so the port allowlist adds little and costs a feature. |
| Deny-all-internal by default (the first proposal) | Breaks every existing LAN deployment on upgrade (devices legitimately live on private/ULA); overridden for the non-breaking default that always blocks never-legit and lets an operator allowlist *tighten*. |
| Screen only DNS names (a custom reqwest resolver, the first cut) | reqwest dials IP-literal / alt-encoded hosts **without** invoking the resolver, so it never sees them and SEC-02 stayed open; superseded by screening the `url`-canonical dialed IP, plus `no_proxy()`. |

## Consequences

- **Non-breaking by default:** out of the box LAN devices (private/ULA/carrier)
  stay reachable and the metadata / loopback / rebind / alt-encoded-literal
  vectors are closed. An operator hardens further (SEC-04) by setting
  `control.device_dial_allow` (or the legacy `MULTIVIEW_DEVICE_DIAL_ALLOW` env
  var) to their device subnet, after which an out-of-subnet internal target is
  refused. Runbook:
  [`docs/runbooks/device-dial-allowlist.md`](../runbooks/device-dial-allowlist.md).
- **Follow-up:** surface `control.device_dial_allow` in the REST API + web UI (the
  field, its validation, and the runtime wiring already land here) and fully
  retire the env-var source.
- **Residual (documented):** dialling a **globally-routable** host on an arbitrary
  port is still allowed (any client can already reach public hosts); this is not
  an SSRF escalation. A rebind to an **allowlisted** internal range is permitted
  by definition (the operator opted in). Config-load (`Device::validate`) accepts
  an alt-encoded literal as a *name* (Rust's `IpAddr` parser does not decode
  `2130706433`); the authoritative dial-time screen canonicalizes it with the
  `url` parser and blocks it — config-load is best-effort, the dial site is
  binding.
- Adds a direct `ipnet` dependency to `multiview-config` and an off-by-default
  `url` dependency to `multiview-control` (behind `zowietek`; both deny-clean and
  already in the lock). No new network I/O in `multiview-config` (resolution
  happens at the async dial sites); config validation stays pure/offline.
- Invariant #10 holds: `dial_policy` is read-only control-plane state; the guard
  runs on the control/IO plane and never touches the engine.
