# Runbook — managed-device / Cast outbound-dial allowlist

**What & why.** The control plane screens every outbound dial (managed-device
poller + ad-hoc Cast session actor) against an SSRF guard
([ADR-M013](../decisions/ADR-M013.md), SEC-02/SEC-04, CWE-918) on the **actual IP
it dials** — resolved DNS names **and** `url`-canonicalized literals alike, so
alt-encoded forms (`http://2130706433/` = 127.0.0.1) and DNS-rebind answers are
caught, not just the raw hostname string.

**Default posture is non-breaking.** Real managed devices (ZowieBox encoders,
Cast TVs) live on the LAN, so out of the box the guard **allows** private
(RFC 1918), ULA (`fc00::/7`), and carrier-grade-NAT (`100.64.0.0/10`) targets — a
`devices-net`/`zowietek`/`cast` build reaches its LAN devices with no
configuration. The **never-legitimate** ranges are **always** refused and no
allowlist can re-enable them: loopback, link-local (incl. the cloud-metadata IP
`169.254.169.254`), unspecified, multicast/broadcast, and their IPv4-mapped forms.

**The allowlist *tightens*.** Setting an allowlist locks the dial to exactly the
listed subnets — an allowlistable target outside them (e.g. an authenticated
internal port-scan attempt, SEC-04) is then refused. This is the recommended
hardening for a deployment that knows its device subnets.

## The knob

Two sources; **config wins**.

### 1. `control.device_dial_allow` (config-as-code — primary)

```toml
[control]
listen = "[::]:8080"
device_dial_allow = ["192.168.10.0/24", "fd00:db8::/64"]
```

- A list of CIDRs (or bare IPs, taken as `/32`/`/128`). IPv6-first; a CIDR, not a
  URL literal: `fd00:db8::/32`, not `[fd00:db8::]`.
- Validated at **config load** — a typo fails `multiview run` fail-closed (the
  error names `device_dial_allow`), never silently widening the screen.
- Travels with the config (carries no secret); the permanent home for the allowlist.

### 2. `MULTIVIEW_DEVICE_DIAL_ALLOW` (env var — legacy fallback)

```
MULTIVIEW_DEVICE_DIAL_ALLOW="<cidr>[,<cidr>...]"
```

- Comma-/whitespace-separated CIDRs. Consulted **only when
  `control.device_dial_allow` is absent** (config wins). Kept for backward
  compatibility and on the deprecation path.

### Semantics (both sources)

- **Unset (neither source)** ⇒ the non-breaking default: LAN ranges dialable,
  never-legit blocked.
- **Set** ⇒ tightens: only the listed CIDRs' allowlistable ranges are dialable. An
  **empty** list (`device_dial_allow = []`) denies every allowlistable range
  (public hosts only — the strictest lock-down).
- **Never-legitimate ranges cannot be re-enabled** — listing `169.254.0.0/16`,
  `127.0.0.0/8`, `::1/128`, multicast, or the unspecified address has no effect;
  the cloud-metadata IP and loopback stay hard-blocked.
- A **malformed** allowlist is **fail-closed**: config load rejects it outright; a
  malformed env var logs a warning and denies every allowlistable range (never
  dials wide).

### Example

Managed devices on `192.168.10.0/24` and a ULA `fd00:db8::/64`, config-as-code:

```toml
[control]
device_dial_allow = ["192.168.10.0/24", "fd00:db8::/64"]
```

Or via the legacy env var (OCI image: a container env var — carries no secret, so
it may live in plain deployment config):

```bash
export MULTIVIEW_DEVICE_DIAL_ALLOW="192.168.10.0/24, fd00:db8::/64"
multiview run config.toml
```

## Verify

1. On start, the log emits `device-dial allowlist loaded from
   control.device_dial_allow entries=<n>` (or `… from MULTIVIEW_DEVICE_DIAL_ALLOW`
   for the env var; **nothing** when both are unset → the non-breaking LAN default).
2. A managed device on the LAN reaches `ONLINE` (`GET /api/v1/devices` → its
   status) out of the box; with an allowlist set, only a device **inside** it stays
   reachable (others ride `UNREACHABLE`).
3. Negative check: `POST /api/v1/cast/sessions` with `address` at
   `169.254.169.254:8009` returns `422` and records no session **regardless of the
   allowlist**; an internal host outside a **set** allowlist also returns `422`.
4. Alt-encoded literal: a `zowietek` device at `http://2130706433/` (= 127.0.0.1)
   never connects — the poller logs `zowietek transport build failed; no poller
   spawned` and spawns nothing (the dial-time screen canonicalizes the literal).

## Change / roll back

- **Tighten/widen:** edit `control.device_dial_allow` (or the env var) and restart
  the control plane (the policy is read at start, not hot-reloaded).
- **Roll back to the non-breaking default:** remove `control.device_dial_allow`
  (and unset the env var) and restart — LAN ranges are dialable again, the
  never-legitimate ranges stay blocked.
- **Strictest lock-down (public-only):** set `device_dial_allow = []` (or set the
  env var to an empty-after-parse value); every allowlistable internal range is
  then denied.
