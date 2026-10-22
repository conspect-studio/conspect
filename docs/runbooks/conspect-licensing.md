# Runbook — Conspect device licensing (heartbeat + device-PoP)

## What it is and why

The `multiview` binary's **device-licensing client** phones home to the Conspect
`/v0` API to renew its entitlement lease (the monthly heartbeat) and, since Conspect
API **v0.9.0**, proves possession of a per-instance **Ed25519 device key** on every
device-mutating call (device proof-of-possession, "PoP"). This runbook is the
operational **how** for the two pieces of durable state the client owns on disk — the
**device-key secret** and the lease state — plus the **live-server PoP validation**
the operator must perform (the implementation is spec-correct + unit-tested, not yet
validated against the live server).

- Feature: off-by-default `heartbeat` (Cargo) — only the `nvidia`/`apple`/
  `linux-vaapi`/`full` presets compile it. The default build is network-free.
- Decisions: [ADR-0096](../decisions/ADR-0096.md) (the wire gate), [ADR-I006](../decisions/ADR-I006.md)
  (renew-only client), [ADR-I007](../decisions/ADR-I007.md) (device-PoP — the why for
  everything here). Crate notes: [`crates/multiview-licence/CLAUDE.md`](../../crates/multiview-licence/CLAUDE.md).
- **Never off air:** every licensing failure (including any PoP failure) keeps the
  last-good lease and ages it via the enforcement ladder — it never stalls or stops the
  program output (invariants #1/#10).

## Resource identity — the lease-state directory

Both the device key and the offline lease drop live in the **lease-state directory**:

- **Env var:** `MULTIVIEW_LICENCE_DIR` (`LEASE_DIR_ENV`).
- **Default:** `/var/lib/conspect/licence/` (`DEFAULT_LEASE_DIR`).
- Contents (created/owned by `multiview` at runtime):
  - `device-key.ed25519` — **the device-PoP signing secret** (see below). `0600`.
  - `idempotency-nonce` + `idempotency-nonce.lock` — the durable Idempotency-Key mint
    counter + its advisory `flock` (cross-restart duplicate-mutation defence). A
    **different** thing from the PoP nonce; do not conflate.
  - any dropped, signed offline lease file the directory watcher verifies.

## The device-key secret — `device-key.ed25519`

### What it is

A **per-instance Ed25519 keypair**, stored as its **32-byte private seed** at
`<lease-dir>/device-key.ed25519`. It is the device's stable cryptographic identity —
**SSH-host-key style**: generated **once** on first boot and reused forever after. Its
**public** key is the binding's `devicePublicKey`, and the server binds that key's
**RFC 7638 thumbprint** into each lease as `cnf_jkt` (holder-of-key). Every heartbeat
carries a `Conspect-Device-PoP` header — a COSE_Sign1 signed with this key — that the
server verifies against the **stored** key (continuity). There is **no
operator-provisioned key**: the device mints its own (ADR-I007). The legacy
`MULTIVIEW_LICENCE_DEVICE_KEY` env var (a public-key string) is **inert for PoP** and a
deprecation candidate.

### Handling rules (these are load-bearing)

- **Permissions: `0600` (owner read/write only).** The client writes it `0600` and, on
  load, **refuses (fails closed) any key whose mode is not `0600`** — a world-readable
  signing secret is never trusted. Do not loosen the perms.
- **Never log it. Never commit it to git.** It is a private key. It is written only to
  the lease-state dir; nothing prints or transmits the seed.
- **It is a secret, treat it like one** — if you back up the lease-state dir, that backup
  now contains a private key; protect it accordingly (the 1Password flow / an encrypted
  volume — see [AGENTS.md](../../AGENTS.md) §rule 34).

### Backup / migrate — WITH the lease state

**Migrate `device-key.ed25519` together with the lease state** when you move/rebuild a
device (same machine, new disk image, container re-home, etc.). The server verifies the
heartbeat against the key it stored at the original **activate**, so a device that comes
back with a **different** key (or none) **`pop-invalid`s** and cannot renew.

- **Losing the key burns a rebind.** Recovering from a lost key means re-binding the
  instance, which **consumes one of the 3-free-per-licence-per-AEST-year rebinds** (the
  4th in a year is a `409 rebinds-exhausted`). So: **back it up**, and **migrate it with
  the lease state**, rather than letting the device regenerate.
- The client **never silently regenerates** over a present-but-bad key: a corrupt /
  wrong-size / weak-perm key **fails closed** (the heartbeat declines to start, last-good
  is kept) rather than minting a new identity that would break continuity. Only a
  genuinely **absent** key triggers first-boot generation.
- **Concurrent first boot is safe:** if two `multiview` processes start at once on a
  fresh device, exactly one wins the atomic create-once (`O_EXCL` + `hard_link`), the
  other reloads the winner — both end up signing with the one key that is durably on
  disk.

## The heartbeat + PoP nonce model

Two **distinct** single-use nonces are in play — keep them straight:

1. **PoP challenge nonce** (the device-PoP one). Single-use, 32 bytes / 64 lower-case
   hex, ~120 s TTL. Lifecycle (RFC 9449 DPoP-nonce style):
   - **Cold start / no held nonce:** `GET /v0/devices/licence/challenge?orgId=…` →
     `DeviceChallenge{ nonce, expiresAtMs }`. The client **expiry-checks** it (a stale
     fresh nonce is refused, fail-closed) before signing.
   - **Steady state:** each `HeartbeatResponse` carries `nextNonce`; the client signs
     **that** on the next heartbeat — so steady-state renewal needs **no** extra
     `/challenge` round-trip.
   - It is **bound into the signed PoP pre-image** and **burned** by the server on the
     first successful proof.
2. **Idempotency-Key mint counter** (`idempotency-nonce`). A monotonic, **durable**
   (crash-safe, restart-safe) counter the client uses to build a retry-stable
   `Idempotency-Key` on each mutation, so a lost-response retry replays (never
   duplicates) the operation. Persisted in the lease-state dir under an advisory
   `flock`. **Not** the PoP nonce.

The heartbeat request body carries the PoP `nonce`; the matching `Conspect-Device-PoP`
header carries the COSE_Sign1 proof over the canonical pre-image
`htm | htu | sha256(body) | instance_id | nonce | iat`.

## Configuration (the `MULTIVIEW_LICENCE_*` env contract)

The heartbeat runs only when the four essentials are set (else it stays off and the
machine runs unlicensed-honest):

| Env var | Purpose |
| ------- | ------- |
| `MULTIVIEW_LICENCE_ORG` | organisation id (`{orgId}`) |
| `MULTIVIEW_LICENCE_ROOT` | pinned ECDSA-P256 root key (base64url uncompressed point) |
| `MULTIVIEW_LICENCE_API` | Conspect API base URL (e.g. `https://api.conspect.studio/v0`) |
| `MULTIVIEW_LICENCE_TOKEN` | account JWT bearer (operator role for heartbeat) — never logged |
| `MULTIVIEW_LICENCE_DIR` | lease-state dir (default `/var/lib/conspect/licence/`) |
| `MULTIVIEW_LICENCE_DEVICE_KEY` | **legacy public-key string — inert for PoP** (deprecation candidate) |
| `MULTIVIEW_LICENCE_CLAIM_CODE` | optional 6-char paid claim code sent on **activate** (ADR-I008); **unset → free non-commercial auto-issue** |
| `MULTIVIEW_LICENCE_LICENCE_ID` | licence id the binding draws its seat from — **required by the device REBIND op** (ADR-I009, `RebindRequest.licenceId`); unused by heartbeat/activate/deactivate |

The remaining `MULTIVIEW_LICENCE_*` vars (machine/instance/binding ids, fingerprint
digest/score, hardware/discriminator digests) carry the salted device identity on the
renew request — never raw serials/MACs (data minimisation, ADR-0036 §8).

## Device lifecycle ops — REBIND + DEACTIVATE (ADR-I009)

Two **operator-invoked, one-shot** device-initiated lifecycle ops complement the
always-on renew loop. They are **device-PoP-signed** (continuity — the server verifies
the binding's STORED key; neither carries `devicePublicKey`, which is activate-only) and
**idempotent** (`Idempotency-Key`). They run on the **same in-process client** the
heartbeat daemon uses, so the in-memory retry pin + the single-owner `FileNonceStore`
flock stay coherent — an ambiguous network failure replays the **same** Idempotency-Key
verbatim and **never double-charges** the scarce rebind budget. Both **fail closed** and
**never take output off air** (they hold no engine handle).

> Programmatically they are `EntitlementPlane::rebind()` / `EntitlementPlane::deactivate()`.
> Invoke them via the running daemon (the operator surface — a CLI subcommand / the
> account-side `system/actions` re-claim affordance — is the thin presentation layer).

### REBIND — lawful hardware change / fingerprint self-heal

When this device's salted fingerprint match score against its binding drops below `70`
(a disk swap, NIC change, re-platform — the self-heal trigger), an operator **rebinds**:
it reactivates the **same** binding (consumes **no new seat**) and refreshes the lease.

- **Budget:** rebind charges the **3-free-per-licence-per-AEST-calendar-year** budget
  (server-side; the response carries `rebindsThisYear`). The **4th** in a year is a
  `409 rebinds-exhausted` — the client surfaces it and keeps last-good (no local retry
  loop; the only recovery is the next AEST year or a paid seat).
- **Config:** rebind requires `MULTIVIEW_LICENCE_LICENCE_ID` (the `RebindRequest.licenceId`)
  in addition to the four heartbeat essentials + the device-identity triple.
- **The fingerprint handoff (load-bearing):** `RebindResponse` carries only a
  `leaseSerial` (NOT an embedded signed lease), so the rebind itself installs **nothing**
  — it seeds the steady-state nonce and the **next renew** installs the refreshed lease
  via the unchanged chokepoint. For that renew's install to clear the local `≥ 70`
  fingerprint-continuity gate, **update `MULTIVIEW_LICENCE_FP_SCORE` to the post-rebind
  self-match** (the new hardware scores ~100 against the refreshed binding) **before** the
  next renew. If the configured score is still the pre-rebind `< 70`, the renew install is
  rejected `FingerprintMismatch` and the refreshed lease never installs (still never off
  air — keep-last-good — but a stuck refresh).
- **Verify:** the op returns `Rebound { rebound: true, seat_consumed: false,
  rebinds_this_year: N }`; the next renew returns `200` with a fresh lease.

### DEACTIVATE — graceful seat surrender / decommission

When retiring a device, an operator **deactivates**: it returns the seat server-side (the
binding moves to `lifecycleState: released`), is idempotent (a re-deactivate is a `200`
no-op), and issues **no** new lease (revocation by non-reissue).

- **Local effect (important):** deactivate installs **nothing** and does **NOT** stop
  local output. The installed last-good lease keeps the program on air and **ages out via
  the offline ladder** (up to `LEASE_FULL = 35d`, then grace/hard) — a decommissioned
  seat keeps producing output for **weeks**, not minutes. There is **no kill verb** (by
  design — invariant #1). For an immediate local stop, **stop the `multiview` process**.
- After a successful deactivate, **stop the heartbeat loop** (nothing to renew — the
  server will non-reissue); the lease then expires on its own offline clock.
- **Verify:** the op returns `Deactivated { lifecycle_state: "released" }`.

## Rule-26 — live-server PoP validation (REQUIRED operator action)

The device-PoP implementation is **spec-correct + self-verifying-unit-tested**, but it
has **NOT** been validated against the live Conspect server (no live account in the dev
environment). **Before relying on renewal in production, validate against the live
Conspect server.** The v0.9.0 spec under-specifies four byte-level details (and its own
examples are unreliable — the `DeviceChallenge` example nonce is 65 chars, violating its
own `^[0-9a-f]{64}$`; the header example `g1gg…` is truncated). Each is made trivially
swappable in the code so a fix is a one-line flip:

1. **Pre-image byte-layout** — we emit a deterministic-CBOR `map(6)` (matching the
   house `canonical_key_preimage` style). If the server expects a different shape
   (length-prefixed concat, text join, array), edit
   `multiview_licence::heartbeat::canonical_pop_preimage`.
2. **`iat` unit** — we use epoch **seconds** (inferred from the server's `±60 s`
   leeway). If the server wants milliseconds, change the `iat` computation in
   `build_pop_header` (`(self.now_ms)() / 1000` → `(self.now_ms)()`).
3. **Attached vs detached** COSE payload — we **attach** the pre-image as the
   COSE_Sign1 payload. If the server expects a detached payload, switch
   `pop_header_value` to `create_detached_signature`.
4. **Tagged vs untagged** COSE_Sign1 — we emit the **untagged** 4-element array
   (`to_vec()`). If the server requires the COSE_Sign1 tag (18), switch to
   `to_tagged_vec()`.

**Plus the activate-continuity check:** heartbeat verifies the proof against the key
Conspect stored for this binding at the **original activate**. First-contact device
**ACTIVATE / enrolment is now implemented** ([ADR-I008](../decisions/ADR-I008.md)):
activate is where this generated public key first reaches the server (as
`devicePublicKey`) and its RFC 7638 thumbprint becomes the lease `cnf_jkt`. The client
enables activate **only when the device-identity triple is configured** (machine id +
fingerprint digest + a **≥ 70** score + hardware + discriminator hash/digest); a fresh
device then enrols online (`GET /challenge` → `POST /activate` with a PoP proof bound to
the **server-assigned `instanceId`** → install the issued lease → renew). Absent that
triple the device is **renew-only** and onboarding stays via an install surface
(control-upload / offline file-drop / mesh relay). Either way, a fresh device with **no
activate-registered key** still `pop-invalid`s on the renew path until it has enrolled
(or a lease arrives via an install surface).

How to validate (rule-26, REQUIRED — the activate path is spec-correct + self-verifying
unit-tested but **NOT** live-server-validated here):
- **Renew:** with a real org JWT + an activate-registered binding, run a heartbeat and
  confirm a `200` (a fresh lease + `nextNonce`), not a `401 pop-invalid`.
- **Activate (first contact):** on a fresh device with the identity triple set + activate
  enabled, confirm `GET /challenge` returns an `instanceId`, the `POST /activate` returns
  `200` with a signed lease + `nextNonce` (not `401 pop-invalid` / `422`), the server
  bound the presented `devicePublicKey` (the lease `cnf_jkt` is its thumbprint), and the
  **next** cycle renews via heartbeat with no extra `/challenge`. The load-bearing
  first-contact unknowns to confirm against the live server: that the PoP pre-image
  `instance_id` on activate is the **server-assigned** `DeviceChallenge.instanceId`
  (echoed), and that activate carries `devicePublicKey` while heartbeat omits it.
- **Rebind (ADR-I009):** with a bound device + `MULTIVIEW_LICENCE_LICENCE_ID` set,
  trigger a rebind and confirm `POST /rebind` returns `200` (`rebound: true`,
  `seatConsumed: false`, `rebindsThisYear: N`), not `401 pop-invalid` / `409`. The
  load-bearing rebind unknown to confirm: that the PoP pre-image `instance_id` is the
  device's **OWN** durable id (continuity), **not** a challenge id; and that after
  updating `MULTIVIEW_LICENCE_FP_SCORE` to the post-rebind self-match, the **next renew**
  installs the refreshed lease (the response carries only a `leaseSerial`).
- **Deactivate (ADR-I009):** trigger a deactivate and confirm `POST /deactivate` returns
  `200` with `lifecycleState: released` (idempotent on a repeat), and that the local
  program **stays on air** afterwards (the last-good lease ages out — no kill).

A `pop-invalid` points at one of the four byte-level items above (or a key/continuity
mismatch); flip the corresponding item and re-test.

## Failure modes (all keep last-good — never off air)

| Symptom | Cause | The client does |
| ------- | ----- | --------------- |
| Heartbeat off, log "could not load/generate the device key" | lease-state dir unwritable, or a corrupt / weak-perm `device-key.ed25519` | declines to start the heartbeat; keeps last-good |
| `401 pop-invalid` on heartbeat | wrong pre-image/iat/payload/tag (rule-26), or the loaded key ≠ the server's stored key | surfaces a `Pop` error; skips the cycle; keeps last-good; backs off |
| `/challenge` unreachable at cold start | network / server down with no held `nextNonce` | skips the cycle; keeps last-good; retries (fresh `/challenge`) next cycle |
| Held/fresh nonce expired | clock skew / slow round-trip | fails closed; fetches a fresh `/challenge` next cycle |
| `lease: null` in a `200` | revoked entitlement (revocation by non-reissue) | keeps last-good; ages it via the ladder; never tightens output on its own |
| `409` on rebind/deactivate | rebinds-exhausted / discriminator-mismatch / no-live-instance / same key still in progress (body-less, overloaded) | maps to `Transport` → replays the **same** Idempotency-Key (server dedups; **no second rebind charge**); a persistent `409` surfaces to the operator (e.g. budget exhausted) |
| Rebind `200` but the next renew won't install | `MULTIVIEW_LICENCE_FP_SCORE` still the pre-rebind `< 70` → the local continuity gate rejects the refreshed lease | keeps last-good (never off air); fix: set the post-rebind self-match score, then the next renew installs |
| `multiview licence rebind` with no licence id | `MULTIVIEW_LICENCE_LICENCE_ID` unset | fails closed with a clear message; sends nothing; keeps last-good |

None of these stop the program output — the device-licensing client holds **no** engine
handle and is physically unable to back-pressure the output clock. A **successful
deactivate** likewise does not stop output: the seat is surrendered server-side and the
local lease ages out via the ladder (no kill verb — invariant #1).
