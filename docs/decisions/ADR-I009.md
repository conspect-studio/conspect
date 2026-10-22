# ADR-I009: CONSPECT device REBIND + DEACTIVATE — the device-initiated lifecycle slice (device-PoP)

- **Status:** Proposed
- **Area:** Implementation build-out
- **Date:** 2026-06-21
- **Source:** operator (backlog task #133). Completes the two remaining device-mutating
  lifecycle ops that [ADR-I008](ADR-I008.md) §Consequences named as forward-compat ("those two
  device-mutating ops also require the PoP header + the challenge nonce; the same
  `canonical_pop_preimage` + `pop_header_value` + `DeviceSigner` + `fetch_challenge` serve them
  when those slices land … only `htm`/`htu`/body differ"). Stacks on the merged
  activate/heartbeat/device-PoP client ([ADR-I006](ADR-I006.md)/[ADR-I007](ADR-I007.md)/ADR-I008).
  Refines [ADR-0096](ADR-0096.md) and [ADR-0050](ADR-0050.md) §7 (the rebind budget).

## Context

The merged device-licensing client carries three of the four device-PoP-gated Conspect operations:
`fetch_challenge` (`GET …/devices/licence/challenge`), `heartbeat` (renew), and `activate`
(first-contact enrolment). The two remaining lifecycle ops — **rebind** and **deactivate** — are
specified in the live Conspect wire but unbuilt on the device:

- **`POST /organisations/{orgId}/rebind`** — "Rebind an instance to refreshed hardware." It
  **reactivates the SAME instance binding** (consumes **no new seat**), charges the
  **3-free-per-licence-per-AEST-calendar-year** budget ([ADR-0050](ADR-0050.md) §7; the 4th in a
  year is a server `409`), and issues a fresh signed lease carrying the refreshed fingerprint
  digest. It is the lawful-hardware-change / fingerprint-self-heal path (a salted-fingerprint match
  score below `FINGERPRINT_MATCH_THRESHOLD = 70` is the trigger, but the **device never
  auto-rebinds** — the budget is scarce and the client "never silently regenerates", so a rebind is
  an explicit operator remediation, not a heartbeat-loop event).
- **`POST /organisations/{orgId}/deactivate`** — "Deactivate (decommission) an instance binding."
  It returns the seat (the binding moves to `lifecycleState: released`), is **idempotent** (a
  re-deactivate of an already-released binding is a `200` no-op), issues **no** new lease
  (revocation-by-non-reissue), and **never errors a running output**.

Both are PoP-gated exactly like heartbeat/activate (a `Conspect-Device-PoP` header — a base64
`COSE_Sign1` over `htm | htu | sha256(body) | instance_id | nonce | iat`, verified server-side
against the binding's **STORED** Ed25519 device key for **continuity**) and both require the
`Idempotency-Key` header.

The constraints that bind the answer are the same ones that bound activate:

- **`multiview-licence` is VERIFICATION-ONLY** ([`crates/multiview-licence/CLAUDE.md`](../../crates/multiview-licence/CLAUDE.md)):
  the crate **computes data and verifies signatures, and nothing else** — **no I/O**, **no key
  generation / no RNG** in non-test code, **no engine handle**. Rebind/deactivate are
  device-**initiated** HTTPS lifecycle calls; the wire bytes + deterministic Ed25519 signing live
  in the leaf crate, all I/O (HTTP, the device-key file) stays at the cli boundary.
- **Never off air (invariants #1/#10).** A rebind/deactivate contact failure must **fail closed**
  (keep last-good, back off, never panic, never tighten output), and even a *successful* deactivate
  must **not** stop local output — the installed last-good lease ages out naturally via the
  enforcement ladder. The lifecycle ops hold no engine handle and are physically unable to
  back-pressure the output clock.
- **No new secret, no new RNG, no new dependency.** Both reuse the **same** persisted 0600
  `device-key.ed25519` seed `DeviceKeyStore` already generates-and-persists, the **same**
  `DeviceSigner` seam, `canonical_pop_preimage`, `pop_header_value`, `fetch_challenge`, the
  pinned-retry-unit (`PendingAttempt`), the durable `idempotency_key()`, the status-aware retry, and
  the `verify_signed_lease_chain` + `LeaseStore::install_binding` chokepoint (rebind only — deactivate
  installs nothing). The default `cargo check`/`cargo deny` shell stays network-free + LGPL-clean.

### The byte-exact rebind/deactivate wire (Conspect live OpenAPI v0.46.0, verified 2026-06-21)

Fetched from `https://api.conspect.studio/v0/openapi.json`; both endpoints + schemas are fully
specified (this is the readiness gate that unblocks the slice — the wire is **not** invented):

- **`POST /organisations/{orgId}/rebind`** — headers: `orgId` (path), `idempotency-key`
  (required), **`conspect-device-pop`** (schema `required:false`, **handler-enforced**: missing →
  `pop-required` 401, bad proof → `pop-invalid` 401). Body **`RebindRequest`**, **required**:
  `licenceId`, `bindingId`, `instanceId`, `instanceDiscriminatorHash`, `fingerprintDigest`
  (`^[0-9a-f]{64}$`), `fpScore` (0–100; below 70 is the self-heal trigger, recorded, **never** used
  to invent a refusal), `nonce` (`^[0-9a-f]{64}$`). **No `devicePublicKey`** (continuity: the server
  verifies against the SAME bound key; a rotation is the audited case-(b) path). Response
  **`RebindResponse`**, required: `rebound` (bool), `leaseSerial` (string|null — null when the
  re-issue is withheld for a revoked entitlement, never off air), `notAfter` (int|null),
  `enforcementState` (the §6 ladder rung as DATA, always returned), `rebindsThisYear` (int — charged
  against the 3-free budget, including this one), `seatConsumed` (**always false**), `fpScore` (int,
  echoed), `nextNonce` (`^[0-9a-f]{64}$` — the next single-use PoP nonce).
- **`POST /organisations/{orgId}/deactivate`** — same required headers. Body **`DeactivateRequest`**,
  **required**: `bindingId`, `nonce` (`^[0-9a-f]{64}$`). **No `devicePublicKey`** (continuity).
  Response (200): an **`InstanceBinding`** whose `lifecycleState` is `released` after a successful
  release (or already-released — idempotent). Errors: `400` (no Idempotency-Key), `401` (no bearer /
  pop-required / pop-invalid), `403` (below engineer role), `404` (cross-tenant/unknown id — a
  byte-identical no-existence-oracle 404), `409` (a request with the same Idempotency-Key is still
  in progress), `422` (missing/empty bindingId), `429`, `500`.

The PoP `instance_id` bound into the pre-image for **both** ops is the device's **OWN** durable
`instance_id` (`DeviceIdentity::instance_id`), **not** a server-assigned challenge id — these are
continuity operations on an **already-bound** instance (the server reserves a fresh `instanceId`
only on first-contact *activate*). This is the load-bearing distinction from activate.

## Decision

Add device **REBIND** and **DEACTIVATE** to the `multiview-licence` `heartbeat` feature as
**operator-invoked, one-shot** device-initiated lifecycle calls that complement the renew/activate
loop, split across the same two boundaries: pure byte-exact wire + crypto in the leaf crate, all I/O
at the cli boundary. The device key, the `DeviceSigner` seam, `canonical_pop_preimage`,
`pop_header_value`, `fetch_challenge`, the `PendingAttempt` retry unit, `idempotency_key()`, the
status-aware retry, and (for rebind) `verify_signed_lease_chain` + `install` are **reused unchanged**.

### 1. Two new leaf-crate request types + two response types

- **`RebindRequest`** (`Serialize + Deserialize`, `rename_all = "camelCase"`, `#[non_exhaustive]`):
  `licence_id`, `binding_id`, `instance_id`, `instance_discriminator_hash`, `fingerprint_digest`,
  `fp_score: u8`, `nonce`. **No `device_public_key` field** (the type cannot send it — continuity).
- **`RebindResponse`** (`Deserialize`, camelCase, `#[non_exhaustive]`, `new()` constructor):
  `rebound: bool`, `lease_serial: Option<String>`, `not_after: Option<i64>`,
  `enforcement_state: EnforcementState`, `rebinds_this_year: i64`, `seat_consumed: bool`,
  `fp_score: u8`, `next_nonce: String`. **Decoded strictly** (all 8 fields are v0.46.0-`required`):
  the non-nullable fields — `rebound`, `enforcement_state`, `rebinds_this_year`, `seat_consumed`,
  `fp_score`, and **`next_nonce`** — carry **no `#[serde(default)]`**, so a response missing one is
  **rejected**, not silently defaulted (a missing `next_nonce` would otherwise silently strand the
  next renew). The required-but-nullable `lease_serial`/`not_after` are `Option` (serde treats an
  absent key identically to an explicit `null` → `None`; operationally identical for the device,
  which never installs from the response either way). **Note: the live v0.46.0 `RebindResponse`
  carries the fresh lease only as a `leaseSerial`, not an embedded signed lease envelope** — so the
  rebind client cannot install a lease from the response alone. It seeds
  the steady-state nonce from `nextNonce` and lets the **next heartbeat cycle** fetch + install the
  refreshed lease via the unchanged renew chokepoint (the binding is unchanged, so the renew path
  picks up the new lease serial naturally). This keeps the install path single-chokepoint and avoids
  a second lease-verification site. (If a future Conspect wire embeds the signed lease in
  `RebindResponse`, install it directly via the same `verify_signed_lease_chain` + `install(...,
  Some(binding))` the renew path uses — the `install` seam already accepts this.)
- **`DeactivateRequest`** (`Serialize + Deserialize`, camelCase, `#[non_exhaustive]`): `binding_id`,
  `nonce`. **No `device_public_key`.**
- **`DeactivateResponse`** (`Deserialize`, camelCase, `#[non_exhaustive]`, `new()`): the device needs
  only the seat-return confirmation, so it parses the load-bearing subset of `InstanceBinding`:
  `id`, `lifecycle_state: String`, `enforcement_state: EnforcementState` (serde drops the rest of the
  `InstanceBinding` payload by default — `#[non_exhaustive]`, no `deny_unknown_fields`). A successful
  deactivate yields `lifecycle_state == "released"`.

Two pure builders mirror `build_activate_request`:
`build_rebind_request_for(identity, licence_id, binding_id, challenge) -> RebindRequest` takes the
**caller-resolved `binding_id`** explicitly (so the live client passes the binding from the
learned → store → configured chain, never an `instance_id` fallback) and echoes the device's own
`instance_id`/`licence_id`/fingerprint material + the challenge `nonce`;
`build_rebind_request(identity, licence_id, challenge)` is a thin convenience that resolves the
binding from the identity alone (`identity.binding_id`, else `identity.instance_id`) and delegates to
`build_rebind_request_for` — used by the pure wire tests. And
`build_deactivate_request(binding_id, nonce) -> DeactivateRequest`. Two thin PoP wrappers mirror
`pop_activate_header_value`: `pop_rebind_header_value` / `pop_deactivate_header_value` (both
`htm = "POST"`, `htu = …/organisations/{org}/rebind|deactivate`, `instance_id` = the device's own).

### 2. Two new `LicenceServer` trait methods + a 409-replay refinement (idempotency correctness)

`rebind(&self, org, body, idempotency_key, pop_header) -> Result<RebindResponse, HeartbeatError>`
and `deactivate(&self, org, body, idempotency_key, pop_header) -> Result<DeactivateResponse,
HeartbeatError>` — the **identical** `(org, body, idempotency_key, pop_header)` shape as `heartbeat`/
`activate`. The Error-mapping contract is the same burned-nonce boundary **with one deliberate
refinement for the lifecycle ops**: a **`409` maps to `Transport`** (ambiguous — keep the pinned
attempt, replay the SAME idempotency-key + body), not to `ServerRejected`. The live wire overloads
`409` across "the same Idempotency-Key is still in progress" (deactivate **and** rebind), plus, for
rebind, "no live instance / rebind-required", "discriminator mismatch", and "rebinds-exhausted" — and
the `409` carries **no JSON body to disambiguate them**. Replaying the same idempotency-key + body is
the **only** choice that is correct for **all** of them: the server dedups a replayed key (an
exhausted/mismatch re-POST returns the same `409` deterministically with **no second rebind charge**;
an in-progress request completes; a no-live-instance retries). Dropping the pinned attempt and
minting a **fresh** idempotency-key (as a blanket `ServerRejected` would) is what **double-charges**
the scarce 3-free-per-year rebind budget on an ambiguous failure — so the lifecycle transport maps
`409 → Transport`. The other received non-2xx (`401 pop-invalid`/`pop-required`, `403`, `404`, `422`)
remain `ServerRejected` (definitive, the nonce is burned, reset + fresh challenge), and a received
`2xx` with an unparseable body remains `Malformed`. This refinement lives in a **dedicated** cli
transport helper for the two lifecycle verbs (`post_raw_json_lifecycle`, which maps `409 → Transport`
and reuses the rest of `post_raw_json`'s contract); the **existing** `heartbeat`/`activate`
`post_raw_json` mapping is **unchanged** (their `409` semantics are "idempotency/body-mismatch", which
a verbatim replay never triggers — so the merged paths keep their proven behaviour byte-for-byte).

### 3. Two one-shot client methods (`rebind_once` / `deactivate_once`) — NOT on the renew loop

`HeartbeatClient::rebind_once()` mirrors `activate_once`'s four stages — pre-network key-trust
fast-fail; build-or-replay the pinned `{idempotency-key, body, nonce, proof}` retry unit (fetch a
fresh `/challenge` for the single-use nonce, build the `RebindRequest` — its `bindingId` **resolved
via the shared `resolve_binding_id()` chain that renew + deactivate also use**: a genuinely
**server-learned** binding → `store.current_binding_id()` (the installed lease) → configured
`identity.binding_id`; fail-closed if none, so a store-learned binding is rebound under the correct
id, never the `instance_id` fallback. **`learned_binding_id` is NOT seeded from the configured
`identity.binding_id`** (it holds only real server-learned ids), so the precedence is honest — a
**stale configured** id can never short-circuit a fresher learned/installed binding (the iso-lens
finding; the configured value is the FINAL fallback). Its PoP pre-image is bound to the device's
**own** `instance_id` (continuity); serialise once, sign the PoP, mint the durable key last, pin); POST with the
status-aware retry (`ServerRejected`/`Malformed` → `reset_on_rejection`; `Transport` → leave pinned);
on a `2xx` clear/rotate the attempt and **seed the steady-state nonce from `nextNonce`** so the next
heartbeat renews with no extra `/challenge`. It returns a new
`HeartbeatOutcome::Rebound { rebound, lease_serial, rebinds_this_year, seat_consumed }`. The
refreshed lease is installed by the **next renew cycle** (per §1), so `rebind_once` itself never
touches `install` — it only learns/keeps the binding and seeds the nonce (keep-last-good is the
default the whole way).

`HeartbeatClient::deactivate_once()` is the same shape **minus** any lease install: fetch challenge →
build `DeactivateRequest` (the device's `binding_id` + nonce) → sign → POST → on a `2xx` clear/rotate
the attempt and **stop** (it does **not** seed a nonce for renew and **does not** touch the store or
the engine — the local last-good lease ages out naturally). It returns
`HeartbeatOutcome::Deactivated { binding_id, lifecycle_state }`. The caller (cli) is responsible for
**stopping the heartbeat loop after a successful deactivate** (there is nothing to renew — the server
will non-reissue); that is a control-plane lifecycle decision at the cli boundary, **never** an
engine action.

Both are **public methods invoked explicitly** on a **single, in-process `HeartbeatClient`
instance** — **not** branches of `run_once`/`run_forever`, and **not** a fresh short-lived process
per invocation. This is the load-bearing model choice (the alternative — a standalone one-shot
subcommand process — breaks idempotency, see Rationale + the rejected alternative): the running
`EntitlementPlane` **retains** an `Arc<HeartbeatClient<ConspectHttpServer>>` after `spawn_heartbeat`,
so the operator action calls `rebind_once`/`deactivate_once` on the **same** client that owns the
`FileNonceStore` lock and the in-memory `PendingAttempt` pin. Consequence: an ambiguous `Transport`
failure leaves the pinned `{idempotency-key, body, nonce, proof}` intact, so a re-invocation on that
same running client **replays it verbatim** (the server dedups — **no second rebind charge**); and
there is **no lock contention** (the one process holds the `FileNonceStore` flock). `run_once` is
**unchanged** — a fingerprint mismatch or pop-invalid on the renew path still keeps last-good and
ages the lease; it never auto-rebinds (that would silently spend the scarce 3/year budget against the
operator's intent).

**The pin is VERB-KEYED (the money-path keystone — a 3-lens-panel correction).** Sharing the running
client between the always-on renew loop and the operator ops is only safe if the pinned attempt
**cannot be consumed or cleared by a different verb**. Both the `PendingAttempt` slot **and** the
in-flight idempotency key are keyed by `AttemptVerb` (`Renew | Activate | Rebind | Deactivate`) in
**four separate per-verb slots** — `renew`, `activate`, `rebind`, `deactivate` (each verb has its
own slot; no two verbs ever share pending state). Renew and Activate get **distinct** slots rather
than a shared one: although a device is normally either bound→renew or unbound→activate, the binding
state can change between cycles (an ambiguous activate's pin persists, then a lease arrives via an
install surface so the next cycle renews), and a shared slot would let a stale activate attempt be
posted by the renew path — separate slots make that structurally impossible.
`pinned_attempt(verb)` returns a slot's attempt **only when its `.verb` matches the requested verb**
(defence in depth — a wrong-verb attempt is dropped, never replayed/posted). So a verb only ever
(a) replays **its own** pinned attempt — the renew loop never posts a rebind/activate body to
`/heartbeat`, and an operator op never posts a renew body (no cross-verb replay) — and (b) clears
**its own** pin/key on success or definitive rejection — so a definitive `/heartbeat` rejection never
clears a pending rebind pin. An ambiguous `/rebind`'s pin + idempotency key therefore **persist
untouched through any number of background renew cycles** until the operator's rebind retry replays
them verbatim; the background loop is **physically unable** to mint a fresh rebind key. (Without
verb-keying — a single shared slot — the renew loop would consume the pinned rebind attempt, a
definitive rejection would clear it, and the operator's retry would mint a fresh key = a **second
charge** against the budget. That was the critical defect a single shared slot introduced; the
per-verb slots close it.)

**The rebind → next-renew lease handoff (the fingerprint-continuity gate).** A rebind exists because
the device's fingerprint match score against the **old** binding dropped (`fpScore < 70` — the
self-heal trigger the request reports). After the server rebinds, the binding's reference fingerprint
becomes the **new** hardware's, so a fresh local measurement of the new hardware scores high (~100)
against the refreshed binding. The operator therefore re-runs the device with the post-rebind
`MULTIVIEW_LICENCE_FP_SCORE` (the new hardware's self-match), and that configured score is what the
**next renew's** `install` stamps into `seal_for_install` — so it clears the store's `≥ 70`
continuity gate and the refreshed lease installs. (If the configured score were still the
pre-rebind cross-match `< 70`, the renew install would reject with `FingerprintMismatch` and the
refreshed lease would never install — still **never off air** (keep-last-good), but a stuck refresh.
The runbook step makes the post-rebind `FP_SCORE` update explicit per rule 42.)

### 4. CLI / operator surface (device-initiated, PoP-signed, idempotent)

`multiview-cli/src/licence.rs` gains two operator-invoked async methods on `EntitlementPlane` —
`pub async fn rebind(&self) -> Result<HeartbeatOutcome, HeartbeatError>` and
`pub async fn deactivate(&self) -> Result<HeartbeatOutcome, HeartbeatError>` (the success arm is
`HeartbeatOutcome::Rebound` / `HeartbeatOutcome::Deactivated`; every failure is a fail-closed
`HeartbeatError`) — that delegate to the **retained, running** `HeartbeatClient` (built once by
`spawn_heartbeat` via a factored `build_client()` helper the daemon and the lifecycle ops share), so
the nonce-store lock and the retry pin stay coherent (§3). They reuse `HeartbeatSettings::from_env`
for the org/api/token/identity config; rebind additionally needs `licenceId` (a new
`MULTIVIEW_LICENCE_LICENCE_ID` env field — the only config addition) and the established binding id
(resolved via the `resolve_binding_id()` chain — learned → store → configured `MULTIVIEW_LICENCE_BINDING_ID`).
These public `EntitlementPlane` methods **are** the device-initiated HTTPS lifecycle calls (config →
the running client → a PoP-signed `POST` → a fail-closed `Result`) — fully wired and tested
end-to-end, exactly as `spawn_heartbeat` is the wired heartbeat path. Every failure keeps the
entitlement plane last-good and returns an `Err(HeartbeatError)` — never a crash, never an engine
touch.

**This slice ships the operations as the `EntitlementPlane::rebind()`/`deactivate()` methods only; it
adds NO arg-parsing subcommands and NO REST/UI route.** A user-facing trigger surface — a thin CLI
subcommand (e.g. `multiview licence rebind|deactivate`) in the shared `cli.rs`/`main.rs` arg surface,
and/or an account-side `system/actions` re-claim REST/UI surface in `multiview-control` — is an
explicit **FOLLOW-ON, not implemented here** (deliberately kept out of this slice's `licence.rs`
territory: a top-level subcommand contends with other lanes on a hot shared file, rule 32). The
device-initiated lifecycle calls themselves are complete and callable today via the methods; wiring a
trigger surface to them is the only remaining step, tracked separately.

### 5. Fail closed (never off air, invariants #1/#10) — identical charter

Every rebind/deactivate failure mode is a non-panicking `Result` that keeps last-good, reusing the
existing fail-closed set (`Transport`/`ServerRejected`/`Malformed`/`Pop`/`NonceStore`/`KeyTrust` +,
for rebind's later install, `SignedLease`/`LeaseExpired`/`BindingMismatch`/`FingerprintMismatch`): an
unreachable `/challenge`, an expired/absent challenge nonce, a signing failure, a `409`
rebinds-exhausted, a `404` unknown binding, a `pop-invalid`/`pop-required` — all keep last-good. The
one-shot methods hold **no** engine handle, so they are physically unable to stall output. A
**successful deactivate explicitly does NOT stop output** — it surrenders the seat server-side and
returns; the local lease ages via the ladder. The never-off-air chaos gate
(`heartbeat_never_off_air.rs`) is **extended** to stall/partition the **rebind** and **deactivate**
POSTs (a one-shot rebind/deactivate against a hostile server asserts the engine still emits
one-frame-per-tick and the store is untouched), mirroring the activate proof. The default build is
unchanged: no new dependency (reuses `coset`/`ed25519`/the existing transport, all behind
`heartbeat`), so `cargo check --workspace` + `cargo deny check` stay a pure, network-free, LGPL-clean
shell.

## Rationale

- **The slice is unblocked + spec-real.** The live Conspect OpenAPI v0.46.0 fully specifies both
  endpoints + schemas (verified 2026-06-21) — this is not a stub-without-a-server (rule 6); both are
  real, spec-valid device-initiated requests. ADR-I008 §Consequences already named the
  `RebindRequest` required set as forward-compat, and the live spec matches it field-for-field. Per
  rule 3, the design is written first (this ADR), then implemented (task #133).
- **Maximum reuse, minimum surface.** Each op is ~90% the activate machinery: same device key, same
  `DeviceSigner`, same `canonical_pop_preimage`, same `pop_header_value`, same `fetch_challenge`,
  same `PendingAttempt` retry unit, same `idempotency_key()`, same status-aware retry, same
  fail-closed set; rebind additionally reuses `install` via the next renew cycle. The genuinely new
  bytes are four request/response types, two pure builders, two PoP wrappers, two trait methods, two
  one-shot client methods, and two outcome variants — which is why a security-path slice is low-risk.
- **Continuity, not enrolment, is the keystone.** Rebind/deactivate are operations on an
  **already-bound** instance: the PoP `instance_id` is the device's **own** durable id (NOT a
  server-assigned challenge id), there is **no `devicePublicKey`** on either wire (the server
  verifies against the STORED key), and rebind installs no new seat (`seatConsumed` always false).
  Getting these three right is what makes a rebind/deactivate proof verify against the existing
  binding rather than mis-binding.
- **Operator-invoked, never automatic.** The 3-free-per-AEST-year rebind budget is scarce, and the
  client's standing rule is "never silently regenerate" — auto-rebinding on a fingerprint/PoP
  mismatch would burn the operator's budget without consent. So rebind/deactivate are explicit
  one-shot operator actions, not heartbeat-loop events; `run_once` is untouched.
- **The leaf-crate verification-only charter is preserved exactly.** No new RNG (Ed25519 signing is
  deterministic; *generation* stays cli-side and is reused) and no new I/O in the leaf crate; the
  byte-exact wire/crypto is unit-testable in isolation (self-verify a produced rebind/deactivate
  COSE_Sign1 against the device public key over the exact pre-image), mirroring activate.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Auto-rebind on the heartbeat loop when `fpScore < 70` / `pop-invalid` | The 3-free-per-AEST-year budget is scarce and the client "never silently regenerates" (runbook). A silent auto-rebind spends the operator's budget without consent and can loop into `409 rebinds-exhausted`. Rebind is an explicit operator remediation; the renew path keeps last-good and ages the lease (unchanged `run_once`). |
| Send `devicePublicKey` on rebind/deactivate (mirror activate for uniformity) | Both verify against the **STORED** bound key (continuity) — the live wire has **no** `devicePublicKey` field on `RebindRequest`/`DeactivateRequest`. Presenting one invents a presented-vs-bound divergence the spec does not have (the exact class of misread that produced the #182 false-positive). `devicePublicKey` belongs to **activate only**. |
| Bind the **challenge's** `instanceId` into the rebind/deactivate PoP (as activate does) | The challenge's server-assigned `instanceId` is for **first-contact** enrolment. Rebind/deactivate act on an **already-bound** instance, so the proof's `instance_id` MUST be the device's own durable `DeviceIdentity::instance_id` — otherwise the proof binds an id the existing binding does not have → `pop-invalid`. |
| Install the rebind's fresh lease from `RebindResponse` directly | The live v0.46.0 `RebindResponse` carries only `leaseSerial` (+ `notAfter`), **not** an embedded signed lease envelope (contrast `ActivateResponse.lease` / `HeartbeatResponse.lease`) — there is nothing to `verify_signed_lease_chain`. Installing from a serial alone would bypass signature verification. Instead seed `nextNonce` and let the next renew cycle fetch + install the refreshed lease via the unchanged single chokepoint (the binding is unchanged; the post-rebind `FP_SCORE` clears continuity — §3). |
| Forcibly drop the local binding / stop output on a successful deactivate | Stopping output would violate invariant #1 (never off air). Deactivate surrenders the seat server-side; the local last-good lease ages out via the **time-based** offline ladder (`LEASE_FULL=35d` → grace → hard — heartbeat-independent, so it expires without a beat; it does NOT stay Active forever). Stopping the **heartbeat loop** (nothing to renew) is a control-plane decision, never an engine action. |
| Make rebind/deactivate branches of `run_once`/`run_forever` | They are explicit operator actions, not steady-state cycles. Folding them into the loop would make them automatic (budget-burning) or add dead branches the loop never takes. Public methods invoked on the running client keep `run_once` clean and the budget under operator control. |
| Run rebind/deactivate as a **standalone one-shot subcommand process** (a fresh process per invocation) | **This breaks idempotency.** The `PendingAttempt` retry pin lives in process memory and the `FileNonceStore` is single-owner (flock'd). A fresh process (a) cannot acquire the nonce-store lock while the heartbeat daemon holds it, and (b) loses the in-memory pin on exit — so an ambiguous `Transport` failure cannot be replayed verbatim; a re-run mints a **fresh** idempotency-key + nonce + body = a **second** logical rebind, **double-charging** the scarce 3-free-per-year budget on a network blip. Instead the ops run on the **retained, in-process** running client (§3): the pin + the lock are coherent, and a re-invocation replays verbatim. |
| Map `409 → ServerRejected` (drop the pinned attempt, mint a fresh key) for the lifecycle ops | A `409` is overloaded ("still in progress" for both; plus rebind's "rebinds-exhausted"/"discriminator-mismatch"/"no-live-instance") with **no JSON body to disambiguate**. "Still in progress" is NOT a burned-nonce definitive rejection — it means *retry the same key*. Dropping + minting fresh re-issues the op as a **new** idempotency-key, double-charging the rebind budget on an ambiguous failure. `409 → Transport` (replay the same key) is correct for **all** sub-cases: the server dedups, an exhausted/mismatch returns the same `409` deterministically (no second charge), an in-progress completes. |
| Generate/persist the device key inside `multiview-licence` for these ops | Violates the leaf crate's no-I/O / no-RNG charter. Generation needs entropy + the filesystem; both stay at the cli boundary with `DeviceKeyStore` (reused unchanged — these ops sign with the SAME continuity key the activate registered). |

## Consequences

- **A device can self-rebind after lawful hardware change** (operator-invoked: rebind → seed nonce →
  the next renew installs the refreshed lease) and **gracefully surrender its seat** (deactivate →
  seat released server-side). Onboarding/offboarding gains the two device-initiated lifecycle ops
  beside enrol/renew; the offline/portal flows are unchanged.
- **The rebind budget is now reachable from the device** (and surfaced via `RebindResponse.rebindsThisYear`
  + the `409 rebinds-exhausted` rejection on the 4th in an AEST year). The budget remains
  **server-side** — the device never tracks or computes it; it reports the server's count. The runbook
  ([`docs/runbooks/conspect-licensing.md`](../runbooks/conspect-licensing.md)) gains operator
  procedures for invoking rebind + deactivate (runbook update lands **with** the implementation
  commit per rule 42, not in this docs-only ADR).
- **One config field returns** (`MULTIVIEW_LICENCE_LICENCE_ID`, for the rebind `licenceId`); no other
  `MULTIVIEW_LICENCE_*` change. The device-initiated operations ship as the
  `EntitlementPlane::rebind()`/`deactivate()` methods. A user-facing **trigger surface** — a thin CLI
  subcommand (`multiview licence rebind|deactivate`) and/or the account-side `system/actions` REST/UI
  re-claim surface — is an explicit **FOLLOW-ON, not built in this slice** (out of this slice's
  `licence.rs` territory; the management-completeness counterpart is tracked separately).
- **`multiview-licence` stays VERIFICATION-ONLY**: rebind/deactivate add no RNG and no I/O to the
  leaf crate (deterministic signing + the reused cli-side key/transport seams), and **no** new
  dependency, so the default `cargo check`/`cargo deny` shell is unchanged (network-free, LGPL-clean;
  `heartbeat` off by default).
- **Graceful surrender keeps serving for the offline window.** A successful deactivate surrenders the
  seat server-side but the **local** lease keeps producing output until it ages out via the offline
  ladder — up to `LEASE_FULL = 35d` (then grace/hard). This is the intended revocation-by-non-reissue
  behaviour (never off air), but it is a security-relevant property worth stating plainly: a
  decommissioned seat keeps the program on air for weeks, not minutes. There is no kill verb (by
  design — invariant #1); an operator needing an immediate local stop stops the `multiview` process
  itself.
- **Invariants #1/#10 are honoured**: every rebind/deactivate failure is a fail-closed,
  non-panicking, keep-last-good path; a **successful deactivate does not stop output**; the lifecycle
  methods hold no engine handle and cannot stall output. The never-off-air chaos gate is extended to
  stall/partition the **rebind** and **deactivate** POSTs (one-frame-per-tick asserted while each is
  parked), mirroring the heartbeat + activate proofs.
- **Idempotency is preserved across operator re-invocations** because the ops run on the retained
  in-process client with a **verb-keyed** pin + idempotency key (§3): the durable `FileNonceStore`
  counter + the per-verb `PendingAttempt` pin mean an ambiguous `Transport`/`409` rebind replays the
  SAME idempotency-key verbatim — and the background renew loop can neither consume nor clear it — so
  a network blip never burns a second rebind charge. The `DeactivateResponse` type is a **device-local
  projection** of the wire `InstanceBinding` (there is no `DeactivateResponse` schema upstream — the
  200 returns `InstanceBinding`); it parses only `id` + `lifecycleState` + `enforcementState` via
  serde-drop-unknown. `RebindResponse` decodes **strictly**: the v0.46.0-required non-nullable fields
  (`nextNonce`, `rebound`, `enforcementState`, `rebindsThisYear`, `seatConsumed`, `fpScore`) have **no
  serde default**, so a malformed response missing one is rejected (a missing `nextNonce` would
  otherwise silently strand the next renew); the required-but-nullable `leaseSerial`/`notAfter` accept
  absent-or-null as `None` (the device never installs from them).
- **Trait-impl blast radius**: adding two `LicenceServer` methods means every existing impl gains
  them — the cli `ConspectHttpServer` (real, routed through `post_raw_json`), and the test fakes
  (`FakeLicenceServer`, `HostileServer`, `ChaosActivateServer`). The hostile/chaos impls return the
  same fail-closed `Transport`/stall so the isolation property holds for the new verbs.
- **Rule-26 follow-up — REQUIRED and unchanged in character.** Like #182/activate, the
  implementation is **spec-correct + unit-tested** (self-verifiable rebind/deactivate COSE_Sign1 over
  the exact pre-image), but **NOT live-server-validated** in this environment (no live Conspect
  account). The operator runs rebind + deactivate end-to-end against the live server; the load-bearing
  unknowns to confirm are the same heartbeat/activate ones (pre-image byte-layout, iat unit,
  attached-vs-detached payload, tagged-vs-untagged COSE) **plus** the lifecycle specifics: that the
  rebind PoP binds the device's own `instance_id` (continuity), that `seatConsumed` is false + the
  refreshed lease is picked up by the next renew, and that deactivate yields `lifecycleState:
  released` + the local lease then ages without taking output off air.

## Biggest residual risk

**The continuity binding (the device's own `instance_id` in the rebind/deactivate PoP pre-image) and
the rebind-lease handoff are verified only against the saved/live Conspect OpenAPI snapshots, never a
live rebind/deactivate.** The slice rests on three unproven-against-a-real-server assumptions: (1) the
PoP pre-image's `instance_id` on rebind/deactivate is the device's **own** durable
`DeviceIdentity::instance_id` (continuity), not a challenge id — get this wrong and the proof is
`pop-invalid` against the existing binding; (2) the refreshed lease arrives via the **next heartbeat**
(not the `RebindResponse`, which carries only a serial), AND the post-rebind `FP_SCORE` the renew
stamps clears the local `≥ 70` continuity gate — if either is off, the rebind succeeds server-side but
the refreshed lease never installs locally (still never off air — keep-last-good — but a stuck
refresh; mitigated by the runbook's explicit post-rebind `FP_SCORE` step); (3) a successful deactivate
truly stops reissue and the local lease ages without a kill (the spec says revocation-by-non-reissue,
but the post-deactivate `enforcementState` rung is not pinned). The idempotency/double-charge risk an
earlier draft carried (a one-shot subcommand process losing the retry pin) is **resolved by design** —
the ops run on the retained in-process client (§3), so a `Transport`/`409` ambiguity replays the same
idempotency-key verbatim and never burns a second budget charge. These remaining unknowns are the same
class of under-specified-wire risk that produced the #182 review churn. Mitigation: implement
spec-correct + unit-test the self-verifying proofs, gate the live-server validation as the explicit
rule-26 operator step, and keep the whole path fail-closed so a wrong guess **never** takes output off
air — it only leaves the rebind/deactivate un-applied (a one-field fix) until the wire is confirmed.
