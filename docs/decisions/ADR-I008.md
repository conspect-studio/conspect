# ADR-I008: CONSPECT device ACTIVATE / enrolment — the first-contact registration slice (device-PoP)

- **Status:** Proposed
- **Area:** Implementation build-out
- **Date:** 2026-06-19
- **Source:** operator (backlog task #40). Completes the activate/enrolment path that
  [ADR-I007](ADR-I007.md) §11 + Consequences explicitly **deferred** ("device-side online-activate is
  DEFERRED … forward-compat"), now that the Conspect server-side blocker has changed shape. Refines
  [ADR-0096](ADR-0096.md) D2 and [ADR-I006](ADR-I006.md) decision point 11.

## Context

The merged heartbeat client (PR #182, [ADR-I007](ADR-I007.md)) is **renew-only**. It signs a
device proof-of-possession (`Conspect-Device-PoP`: a base64 COSE_Sign1 over
`htm | htu | sha256(body) | instance_id | nonce | iat`) with a generated-and-persisted Ed25519
device key on every `POST …/heartbeat`, and the never-off-air invariants (#1 output-clock, #10
isolation) are proven under the chaos gate. But it **cannot enrol a fresh device**: `run_once`
resolves the binding to renew *before* any network call (`configured ?? learned ??
store.current_binding_id()`) and, with no binding, makes **no** server call and returns
`HeartbeatOutcome::NoBinding`. Onboarding is operator/portal + the three install surfaces
(control-upload, file-drop watcher, mesh relay), all of which feed `LeaseStore::install_binding`.

The reason activate was deferred has **changed**. [ADR-I007](ADR-I007.md) §11 named the blocker as
the Conspect server-side `serverNonce` issuance (the per-instance lease-chain freshness anchor). The
authoritative live wire — `ActivateRequest` byte-exact in Conspect OpenAPI **v0.9.0 → v0.20.0**
(saved snapshots) — has since **deprecated `serverNonce`**:

> `serverNonce` — **DEPRECATED (ADR-0042): the server now mints the lease freshness nonce itself**
> (as rebind/offline already do), so this field is ignored and OPTIONAL … **Do not send it**; the
> device proves liveness via the **PoP challenge nonce**, not by supplying the lease nonce.

The freshness anchor is now the **device-PoP challenge nonce** — the exact mechanism PR #182 already
implements for heartbeat. So the original blocker is gone: a device with the device key + a fetched
challenge nonce **can** mint a spec-valid activation request today. This ADR is **forward-prep, not
an in-production break**: Conspect **v0.20.0 confirmed the heartbeat wire is byte-identical** to
v0.16.0 (memory: `conspect-api-v0.20.0-device-pop-diff`, `breakingForMergedClient=false`), so the
merged renew-only client is **not broken**; activate is purely additive.

### What v0.16.0 added that the merged client tolerates-but-ignores

The merged `DeviceChallenge` (`crates/multiview-licence/src/heartbeat.rs` ~L997) is **two fields**:
`nonce` + `expires_at_ms` (`#[non_exhaustive]`, `Deserialize`). Conspect v0.16.0 added a **third
REQUIRED** field, `instanceId`:

> `instanceId` — **the SERVER-ASSIGNED durable instance id (`ib_<uuidv7>`, ADR-0015) reserved with
> this nonce**. A **FIRST-CONTACT device** (no prior binding) **sends this back as the activate
> `instanceId`** and binds the PoP pre-image's `instance_id` to it — the activate creates the binding
> with this exact id and signs the lease with it, so **the proof, the binding, and the lease share
> ONE server id**. A **RENEWING device** already knows its durable id and signs the PoP over THAT,
> **ignoring this value**.

The merged client tolerates the new field only because serde drops unknown JSON keys by default (the
struct has no `deny_unknown_fields`) — it does **not** parse or consume it. That is correct for the
renew path (a renewing device ignores it), but enrolment **must** consume it: it is the server id
that ties proof+binding+lease together on first contact.

### The byte-exact activate wire (Conspect v0.9.0 → v0.20.0, unchanged)

- **`GET …/devices/licence/challenge?orgId=…`** → `DeviceChallenge { nonce, expiresAtMs,
  **instanceId** }`. (The existing `fetch_challenge` seam, [ADR-I007](ADR-I007.md) §5.)
- **`POST /organisations/{orgId}/activate`** — headers: `orgId` (path), `idempotency-key`
  (required), **`conspect-device-pop`** (schema `required:false`, **handler-enforced**: missing →
  `pop-required` 401, bad proof → `pop-invalid` 401). Body `ActivateRequest`, **required**:
  `machineId`, `fingerprintDigest` (`^[0-9a-f]{64}$`), `fingerprintScore` (0–100, **≥ 70 or 422**),
  `hardwareDigest`, `instanceId`, `instanceDiscriminatorHash`, `instanceDiscriminatorDigest`
  (`^[0-9a-f]{64}$`), **`devicePublicKey`** (base64url raw-32 Ed25519), `nonce` (`^[0-9a-f]{64}$`).
  Optional: `claimCode` (6 chars — **omit to auto-issue a free non-commercial licence**). Deprecated:
  `serverNonce` (do **not** send).
- **`devicePublicKey` is set HERE and only here on the registration path.** The server "verifies the
  COSE_Sign1 proof against this key **before binding**, then embeds the key's **RFC 7638 thumbprint
  as the lease `cnf_jkt`** (holder-of-key)." Heartbeat **omits** `devicePublicKey` (the server
  verifies the proof against the **stored** key — continuity). This is the registration path where
  the server **first receives** the device public key and sets `cnf_jkt`.
- **`ActivateResponse`** (required `enforcementState`, `nextNonce`; optional `lease`): the
  `nextNonce` seeds the **steady-state heartbeat DPoP-nonce**, so the device transitions activate →
  renew with **no** extra `/challenge` round-trip. The signed `lease` is installed via the same
  `verify_signed_lease_chain` + `LeaseStore::install_binding` chokepoint the renew path uses.

### Binding constraints (the crate's reason to exist — unchanged)

[`multiview-licence/CLAUDE.md`](../../crates/multiview-licence/CLAUDE.md): the crate **computes data
and verifies signatures, and nothing else** — **no I/O**, and **no key generation / no RNG in
non-test code**. **`multiview-licence` is VERIFICATION-ONLY.** The default `cargo check` / `cargo
deny` stays a network-free, LGPL-clean shell. Any activate failure must **fail closed** (no
enrolment this cycle, keep last-good, back off), **never panic, never tighten output** — exactly the
heartbeat fail-closed charter. Activate adds **no** new secret and **no** new RNG: it **reuses the
0600 `device-key.ed25519` seed** `DeviceKeyStore` already persists (SSH-host-key model,
[ADR-I007](ADR-I007.md) §2), signing the activate proof with the **same** deterministic Ed25519 key
(RFC 8032 — no entropy) whose **public** half becomes `devicePublicKey`.

## Decision

Add device **ACTIVATE / enrolment** to the `multiview-licence` `heartbeat` feature as the
first-contact registration path that complements the merged renew path, split across the same two
boundaries: pure byte-exact wire + crypto in the leaf crate, all I/O (HTTP, the device-key file) at
the cli boundary. The device key, the `DeviceSigner` seam, the `canonical_pop_preimage`, the
`pop_header_value` COSE builder, and the `fetch_challenge` seam are **reused unchanged**; activate
only adds the activate-specific request type, the `instanceId` field on `DeviceChallenge`, and the
`run_once` first-contact branch.

### 1. Consume `DeviceChallenge.instanceId` and bind it through activate

`DeviceChallenge` gains the **REQUIRED** `instance_id: String` field (serde
`rename_all = "camelCase"` already maps `instanceId`; the type stays `#[non_exhaustive]` +
`Deserialize`). The field is **load-bearing only on first contact**:

- **First-contact device** (no `established_binding`): the fetched `DeviceChallenge.instance_id`
  (`ib_<uuidv7>`) is echoed as the `ActivateRequest.instanceId`, **and** is the `instance_id` bound
  into the PoP pre-image (`canonical_pop_preimage(htm, htu, body, **instance_id**, nonce_hex, iat)`)
  — so proof, binding, and lease share **one** server id. The returned binding id is learned
  (`remember_binding_id`) and recorded **only on a genuine `Installed`** (never a stale no-op),
  matching the renew path's anti-poisoning rule ([ADR-I006](ADR-I006.md) #8/#9).
- **Renewing device** (`established_binding` is `Some`): the heartbeat path is **unchanged** — it
  signs the PoP over its own durable id (`DeviceIdentity::instance_id`) and **ignores** the
  challenge's `instanceId`, exactly as the spec states.

This makes the field's two consumers explicit: renew ignores it (already true), enrolment binds it.

### 2. Set `devicePublicKey` at ACTIVATE only

A new `build_activate_request(identity, challenge, claim_code)` (leaf crate, pure) assembles
`ActivateRequest` from `DeviceIdentity` + the fetched challenge. It is the **only** wire shape that
carries `devicePublicKey` — sourced from the **persisted device key's public half**
(`DeviceSigner::public_key_raw()` → base64url of the raw 32 bytes), **not** from the legacy
`MULTIVIEW_LICENCE_DEVICE_KEY` env string (which is inert for PoP, [ADR-I007](ADR-I007.md) §2 and a
deprecation candidate). The server verifies the PoP proof against this presented key, binds it, and
sets the lease `cnf_jkt` to its RFC 7638 thumbprint. `HeartbeatRequest` is **untouched** — it never
carries `devicePublicKey` (the server verifies against the stored key; this was the cross-vendor
false-positive in the #182 review — heartbeat carries only `nonce`). The activate proof signs over
the **exact serialized `ActivateRequest` body bytes** the transport sends (`sha256(body)` in the
pre-image), the same verbatim-body discipline the heartbeat path uses (no re-serialize drift).

`serverNonce` is **never sent** (deprecated; the closed `ActivateRequest` type simply has no field
for it). `claimCode` is **omitted** to auto-issue the free non-commercial licence (the default
self-host path); it is sent only when an operator supplies a paid claim code via the
`MULTIVIEW_LICENCE_*` config surface (re-instating the `CLAIM_ENV` that [ADR-I007](ADR-I007.md) §11
deleted, now that activate is real).

### 3. The offline-file and link-challenge activation paths

Both are **named and scoped** so the ADR is complete and honest about what enrolment does and does
**not** subsume:

- **Offline-file activation is an OPERATOR/PORTAL flow, not a new device-PoP path.** The Conspect
  `OfflineChallenge`/`offline-leases/challenge` surface issues a signed offline lease from a
  `.challenge` artefact, and the spec is explicit that **"the operator session is the trust anchor;
  the challenge file is NOT device-…"** — there is **no** device-PoP on the offline issue path. On
  the **device** side this is the **existing file-drop install surface** (`LeaseStore::install_binding`
  via the directory watcher, [ADR-I006](ADR-I006.md)) — an offline-issued signed lease drops into
  the lease dir and installs through the **unchanged** `verify_signed_lease_chain` chokepoint. So
  the offline path needs **no** new device-side code: it is enrolment-without-activate, and the
  online activate slice does **not** replace it. (`OfflineLeaseInstallRequest` /
  `OfflineTrueUpRequest` are portal/server-side, out of the device's scope.)
- **The `…/devices/{id}/link/challenge` + `/link` (engine uplink WebSocket) is a SEPARATE
  cold-start link mechanism, not the licensing activate path.** `EngineLinkChallenge` is its own
  short-TTL nonce for opening the engine uplink WS; it is **out of scope** for this licensing slice
  and explicitly **not** conflated with the device-PoP lease activate. It is noted here only so a
  future reader does not mistake the two `link` nonces for the licensing challenge.

### 4. Device-key lifecycle reuse — the 0600 seed already on disk

Activate introduces **no new key material and no new RNG**. It reuses the **same**
`<lease-dir>/device-key.ed25519` (0600, crash-durable, atomic create-once) `DeviceKeyStore` already
generates-and-persists and loads inode-bound (`O_NOFOLLOW` + fstat 0600 + read-same-fd,
[ADR-I006](ADR-I006.md)/[ADR-I007](ADR-I007.md) §8 round-2). The **same** loaded `SigningKey` signs
the activate proof (deterministic Ed25519, no entropy in non-test code — the leaf-crate charter
holds), and its **public** half is the `devicePublicKey` the server binds. **Generation stays at the
cli boundary**; the leaf crate only holds the loaded signer through the `DeviceSigner` seam and signs
the COSE `Sig_structure` — identical to the heartbeat path. **Continuity is the whole point**: the
key minted on first boot is the key the server stores at activate (as `devicePublicKey` → `cnf_jkt`),
and every subsequent heartbeat re-proves possession of *that* key — so the device key **must** be
generated *before* activate and **migrated with the lease state** (losing it forces a rebind, which
spends the 3-free-per-year budget). This closes the rule-26 caveat #182 flagged ("a fresh device
`pop-invalid`s until the activate slice lands — that registration path is where `devicePublicKey`
first reaches the server + `cnf_jkt` is set"): **this is that slice.**

### 5. `run_once` gains a first-contact ACTIVATE branch (still never off air)

`run_once` keeps its renew-only behaviour for an **established** binding (unchanged). When there is
**no** established binding **and** activate is configured (an org id + account JWT + a loadable
device key + the device-identity triple are present), it takes the **enrolment** branch:
`fetch_challenge` → `build_activate_request` (echoing `instanceId`, setting `devicePublicKey`) →
sign the PoP over the exact body → `POST /activate` with `conspect-device-pop` +
`idempotency-key` → on a positively-verified `ActivateResponse`, install the lease via
`install_binding`, learn the binding id on `Installed` only, and **seed the steady-state nonce from
the response `nextNonce`** so the next cycle renews with no extra `/challenge`. The
`HeartbeatOutcome::NoBinding` (no-activate-config) path is retained verbatim — a device that cannot
self-activate still makes no call, installs nothing, keeps last-good, and waits for an install
surface. The same **status-aware retry** rules apply to activate as to heartbeat
([ADR-I007](ADR-I007.md) §8 round-3): the `{idempotency-key, body, nonce, proof}` is **one retry
unit**; an ambiguous transport failure replays it verbatim (the mutation may have committed), a
definitive `ServerRejected` (`401 pop-invalid`/`422`) drops the pinned attempt and burned nonce and
fetches a fresh challenge next cycle. A single-use nonce the server has seen-and-rejected is never
replayed.

### 6. Fail closed (never off air, invariants #1/#10) — identical charter

Every activate failure mode is a non-panicking `Result` that keeps last-good and skips the cycle,
reusing the existing fail-closed set (a new `HeartbeatError::Activate`/reuse of `Pop` +
`ServerRejected` + `Transport`): unreachable `/challenge`, an expired/absent challenge nonce, a
`devicePublicKey`/signing failure, a `422` (low fingerprint score / malformed identity), a
`pop-invalid`/`pop-required` — all keep last-good, back off, and retry, exactly like the renew path.
A device key the cli cannot load/generate → the heartbeat (and thus activate) **declines to start**
and the machine runs unlicensed-honest, never a crash. The activate task holds **no** engine
handle, so it is **physically unable** to stall output (the leaf-crate isolation guarantee). The
default build is unchanged: activate adds **no** new dependency (it reuses `coset`/`ed25519`/the
existing transport, all behind `heartbeat`, off by default), so `cargo check --workspace` + `cargo
deny check` stay a pure, network-free, LGPL-clean shell.

## Rationale

- **The blocker genuinely cleared.** [ADR-I007](ADR-I007.md) §11 deferred activate on a concrete,
  honestly-named external blocker (server-issued `serverNonce`). The live wire deprecated that field
  and moved freshness onto the device-PoP challenge nonce — the mechanism #182 already ships — so
  this is no longer a stub-without-a-server (rule 6); it is a real, spec-valid request the device can
  mint today. Per rule 3, the deferred-but-now-unblocked design is written first (this ADR), then
  implemented (task #40).
- **Maximum reuse, minimum surface.** Activate is ~90% the heartbeat machinery: same device key, same
  `DeviceSigner`, same `canonical_pop_preimage`, same `pop_header_value`, same `fetch_challenge`,
  same `install_binding` + `verify_signed_lease_chain`, same status-aware retry, same fail-closed
  set. The only genuinely new bytes are the `ActivateRequest` type, the `instanceId` field, and the
  first-contact branch — which is why the slice is low-risk despite touching the auth path.
- **The leaf-crate verification-only charter is preserved exactly.** No new RNG (Ed25519 signing is
  deterministic; *generation* stays cli-side and is **reused**, not duplicated) and no new I/O in the
  leaf crate. The byte-exact wire/crypto lives where it is unit-testable in isolation (self-verify a
  produced activate COSE_Sign1 against the device public key over the exact pre-image), mirroring the
  proven heartbeat tests.
- **`instanceId` consumption is the correctness keystone.** Binding the **server-assigned**
  `ib_<uuidv7>` into the proof, the `ActivateRequest`, and the lease — and learning it only on a
  genuine install — is what makes proof+binding+lease share one id and prevents first-contact
  identity poisoning, consistent with [ADR-I006](ADR-I006.md)'s anchoring rules.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Keep the client renew-only; never add device-side activate | The original deferral blocker (`serverNonce` issuance) is **gone** — the live wire deprecated it and moved freshness to the device-PoP challenge nonce #182 already implements. A fresh device cannot self-enrol and `pop-invalid`s forever against a key the server never registered; activate is the registration path that sets `devicePublicKey` + `cnf_jkt`. Leaving it deferred is now an avoidable gap, not an honest external block. |
| Parse `DeviceChallenge.instanceId` but ignore it on activate (mint a client-chosen instance id) | The server **assigns** the durable `ib_<uuidv7>` and reserves it with the nonce; the activate must echo **that** exact id and bind it into the PoP pre-image so proof+binding+lease share one server id. A client-chosen id would not match the reserved binding and the activate would fail / mis-bind. Echo the server id. |
| Send `devicePublicKey` on **every** request (heartbeat too) for uniformity | Heartbeat verifies against the **stored** key (continuity); the wire omits `devicePublicKey` from `HeartbeatRequest` by design (this exact misread was the #182 cross-vendor **false positive**). Sending a public key on heartbeat invents a divergence vector (presented vs signing key) the spec does not have. `devicePublicKey` belongs to **activate only**. |
| Mint a **fresh** device key for activate (or let the operator provide one) | Breaks continuity: every subsequent heartbeat re-proves possession of the key the server stored **at activate**; a fresh/rotated key `pop-invalid`s until a rebind (spending the rebind budget). And an operator-provided **private** key has no secure config channel (env vars are process-visible). Reuse the SSH-host-key-model seed `DeviceKeyStore` already persists. |
| Re-send the deprecated `serverNonce` for back-compat | The live spec says **do not send it** (ignored, OPTIONAL, removed next major); freshness is the device-PoP challenge nonce. A closed `ActivateRequest` type with no `serverNonce` field makes sending it structurally impossible — the [ADR-I007](ADR-I007.md) `Transport`-enum discipline applied to the activate body. |
| Fold offline-file activation into a new device-PoP path | The offline issue path has **no** device-PoP ("the operator session is the trust anchor; the challenge file is NOT device-…"); on the device it is the **existing** file-drop install surface (`install_binding`), needing no new code. Treating it as an activate variant would invent a device credential the offline flow deliberately does not use. |
| Treat `…/devices/{id}/link/challenge` as the licensing activate challenge | It is a **separate** engine-uplink-WebSocket cold-start nonce (`EngineLinkChallenge`), unrelated to the lease activate. Conflating the two `link` nonces would mis-wire enrolment. Out of scope; named to prevent the confusion. |
| Generate/persist the device key inside `multiview-licence` for activate | Violates the leaf crate's no-I/O / no-RNG charter (its CLAUDE.md). Generation needs entropy + the filesystem; both stay at the cli boundary alongside `DeviceKeyStore`/`FileNonceStore`/`ConspectHttpServer`. The leaf crate holds the loaded signer through the seam and signs, nothing else. |

## Consequences

- **A fresh device can self-enrol online** (activate → install signed lease → transition to renew via
  the response `nextNonce`), closing the rule-26 gap #182 flagged. Onboarding gains a **fourth**
  surface (online activate) beside the three install surfaces; operator/portal + file-drop remain
  fully supported (the offline path is unchanged and not replaced).
- **The device key is now load-bearing *before* first contact**, not just for renew. The runbook
  ([`docs/runbooks/conspect-licensing.md`](../runbooks/conspect-licensing.md)) must state that the
  0600 `device-key.ed25519` is created and its public half registered **at activate** (as
  `devicePublicKey` → lease `cnf_jkt`), and must be **migrated with the lease state** — losing it
  after activate forces a rebind (3-free-per-year). (Runbook update lands **with** the implementation
  commit per rule 42, not in this docs-only ADR.)
- **The `claimCode` config field returns** (`CLAIM_ENV`, removed by [ADR-I007](ADR-I007.md) §11 when
  activate was dropped): omitted → free non-commercial auto-issue; set → paid claim redemption. No
  other `MULTIVIEW_LICENCE_*` change; `serverNonce` config never returns (deprecated).
- **`multiview-licence` stays VERIFICATION-ONLY**: activate adds no RNG and no I/O to the leaf crate
  (deterministic signing + the reused cli-side key/transport seams), and **no** new dependency, so
  the default `cargo check`/`cargo deny` shell is unchanged (network-free, LGPL-clean; `heartbeat`
  off by default).
- **Invariants #1/#10 are honoured**: every activate failure is a fail-closed, non-panicking,
  keep-last-good path; the activate task holds no engine handle and cannot stall output. The
  never-off-air chaos gate is extended to SIGKILL/stall/partition the **activate** path (a
  first-contact device asserts one-frame-per-tick while activate is parked), mirroring the heartbeat
  proof.
- **Forward-compat for rebind/deactivate**: those two device-mutating ops also require the PoP header
  + the challenge nonce; the same `canonical_pop_preimage` + `pop_header_value` + `DeviceSigner` +
  `fetch_challenge` serve them when those slices land (`RebindRequest` required = `licenceId`,
  `bindingId`, `instanceId`, `instanceDiscriminatorHash`, `fingerprintDigest`, `fpScore`, `nonce`;
  only `htm`/`htu`/body differ). Out of scope here; named so the reuse is explicit.
- **Rule-26 follow-up — REQUIRED and unchanged in character.** Like #182, the activate implementation
  will be **spec-correct + unit-tested** (a self-verifiable activate COSE_Sign1 over the exact
  pre-image), but **NOT live-server-validated** in this environment (no live Conspect account). The
  operator runs activate end-to-end against the live server; the load-bearing unknowns to confirm are
  the same heartbeat ones (pre-image byte-layout, iat unit, attached-vs-detached payload,
  tagged-vs-untagged COSE) **plus** the fresh-enrolment specifics: that the server accepts the echoed
  `DeviceChallenge.instanceId` as the activate `instanceId`, binds the presented `devicePublicKey`,
  and sets `cnf_jkt` to its RFC 7638 thumbprint.

## Biggest residual risk

**The activate pre-image / `instanceId` binding is verified only against the saved Conspect OpenAPI
snapshots, never a live activation.** The whole slice rests on three unproven-against-a-real-server
assumptions: (1) the PoP pre-image's `instance_id` field on **first contact** is the
**server-assigned** `DeviceChallenge.instanceId` (echoed), not the device's own
`DeviceIdentity::instance_id` (the renew value) — get this wrong and a first-contact proof is
`pop-invalid` against a binding id the server never reserved; (2) the activate body must carry
`devicePublicKey` while heartbeat must omit it (a divergence the merged client already encodes, but
never live-confirmed for the activate direction); (3) `serverNonce` is truly ignored (the spec says
so, but it sat `required:false` and the field's own example is still populated). These are the same
class of under-specified-wire risk that produced the #182 review churn (the v0.9.0 spec's **own**
examples were unreliable — a 65-char nonce, a truncated COSE header). Mitigation: implement
spec-correct + unit-test the self-verifying activate proof, gate the live-server validation as the
explicit rule-26 operator step, and keep the whole path fail-closed so a wrong guess **never** takes
output off air — it only leaves the device unlicensed-honest until the wire is corrected (a one-field
fix, exactly as #182's tagged-vs-untagged was scoped to a one-line swap).
