# ADR-I007: CONSPECT-3 device proof-of-possession (PoP) on the heartbeat client

- **Status:** Accepted
- **Area:** Implementation build-out
- **Date:** 2026-06-17
- **Source:** operator (Conspect API v0.9.0 is now **enforcing** device-PoP on the device-mutating ops; the merged renew-only heartbeat client — account-JWT only, no PoP — is rejected `pop-invalid`, so leases can no longer renew). Refines [ADR-0096](ADR-0096.md) D2 and [ADR-I006](ADR-I006.md) decision point 11.

## Context

The merged renew-only heartbeat client (PR #177, [ADR-I006](ADR-I006.md)) authenticates with
the account-JWT `Authorization: Bearer` chain only. ADR-0096 D2 recorded that the device PoP
key was *captured and stored* (`devicePublicKey`) but that **authenticating a request with it
was deferred server-side** ("slice 5d"). That deferral has ended: **Conspect API v0.9.0 now
enforces device proof-of-possession** on the four device-mutating operations
(activate/heartbeat/rebind/deactivate). A heartbeat without a valid `Conspect-Device-PoP`
header is rejected `pop-required`/`pop-invalid` (401), so the renew-only client can no longer
renew a lease. This is **security-critical and currently broken in production**.

The wire (extracted byte-exact from the authoritative spec
`/tmp/salvage/conspect-openapi.v0.9.0.json` — there is no golden vector or integration guide
available locally, so the spec descriptions are the sole authority, mirroring how #177's
`leaseBytes` CBOR had to be reproduced):

- **`GET /v0/devices/licence/challenge?orgId=…`** (account-JWT bearer, organisation **operator**
  role floor) → `DeviceChallenge { nonce: ^[0-9a-f]{64}$ (32 bytes / 64 lower-case hex,
  single-use, ~120 s TTL), expiresAtMs }`. A cross-tenant `orgId` is a byte-identical 404.
- **`HeartbeatRequest`** gains a **required** `nonce` field (`^[0-9a-f]{64}$`): the `nextNonce`
  from the prior heartbeat/activate response (RFC 9449 DPoP-nonce style), or a fresh
  `/challenge` at cold start.
- **`HeartbeatResponse`** gains a **required** `nextNonce` (`^[0-9a-f]{64}$`) — so steady-state
  renewal needs **no** extra `/challenge` round-trip.
- **`Conspect-Device-PoP` header** (`required: false` in the schema, but **handler-enforced**:
  a missing/empty header is `pop-required` 401, a failing proof is `pop-invalid` 401): a
  **base64 COSE_Sign1** the device signs over the canonical PoP pre-image
  `htm | htu | sha256(body) | instance_id | nonce | iat` with the binding's bound **Ed25519
  device key**. The server "pins Ed25519, recomputes the pre-image from the actual request,
  verifies against the … STORED key (continuity), checks the **iat ±60 s** leeway, and burns
  the single-use nonce." `devicePublicKey` is the base64url of the raw 32-byte point; the server
  embeds its **RFC 7638 thumbprint** as the lease `cnf_jkt` (holder-of-key).

The renew-only client **dropped `activate`**, which is where a device keypair would have been
generated — so the client today holds **only a configured public-key string**
(`DeviceIdentity::device_public_key_b64url`, captured-but-unused) and **has no private key to
sign with**. PoP therefore requires adding device keypair **generation + durable persistence**.

Binding constraints (the crate's reason to exist):
[`multiview-licence/CLAUDE.md`](../../crates/multiview-licence/CLAUDE.md) — the crate
**computes data and verifies signatures, and nothing else**: no engine handle, **no I/O**, and
**no key generation / no RNG in non-test code** (data minimisation). The default `cargo check`
/ `cargo deny` must stay a network-free, LGPL-clean shell. The never-off-air invariants (#1
output-clock, #10 isolation) bind any licensing code: **any** PoP failure must fail closed
gracefully (skip this cycle, keep last-good, back off), never panic, never tighten output.

## Decision

Add device-PoP to the heartbeat path as part of the off-by-default `heartbeat` feature, split
across the leaf crate (pure, byte-exact, unit-tested crypto) and the cli boundary (the I/O:
keypair generation + persistence + the HTTP transport), respecting the crate's no-I/O /
no-keygen invariant.

### 1. COSE library — `coset` 0.4.2 (`dep:coset`, under `heartbeat`, in `multiview-licence`)

`coset` (Apache-2.0) builds the COSE_Sign1 with `CoseSign1Builder`. Its **only** runtime deps
are `ciborium` + `ciborium-io` — **already in the `multiview-licence` graph** (the crate uses
`ciborium 0.2.2`), so it adds **zero new transitive crates** and is fully `cargo deny`-clean
(no allowlist change). `CoseSign1Builder::create_signature(aad, |tbs| signer.sign(tbs))` hands
the closure the canonical COSE `Sig_structure` (`"Signature1"`) bytes to sign — the protected
header (`alg = EdDSA = -8`) must be set first. `to_vec()` produces the **untagged** 4-element
array `[protected, unprotected, payload, signature]`; the header value is the **standard-base64**
(RFC 4648 §4) of those bytes.

### 2. Device keypair — generated + durably persisted at the **cli boundary** (`licence.rs`)

The Ed25519 device keypair is **generated once** (the only RNG use; cli-side, never the leaf
crate) and **persisted** to `<lease-dir>/device-key.ed25519` (the same dir as the lease state
and the idempotency nonce) with restrictive perms (`0600`), via the same crash-durable
write-temp → fsync → rename → fsync-parent protocol the `FileNonceStore` already uses. Reload
on restart gives a **stable device identity** (continuity — the server verifies against the
*stored* key). The 32-byte seed is the only secret on disk; it is never logged. A
generate-once-then-reuse `DeviceKeyStore` at the cli boundary owns this; the leaf crate receives
a loaded signer through a seam.

**Provisioning model — client-generated, persisted on first run (SSH-host-key style).** The
device keypair is **minted by the device on first boot** and reused forever after, exactly like
an SSH host key. The generated **public** key is the device's identity: it is what populates
`devicePublicKey` (base64url raw-32) on the eventual activate, and its RFC 7638 thumbprint is the
lease `cnf_jkt` (holder-of-key) the server binds. This **supersedes the existing captured-but-
unused `MULTIVIEW_LICENCE_DEVICE_KEY` placeholder** (`DeviceIdentity::device_public_key_b64url`):
that env var carried only a **public-key string** (forward-compat for the deferred activate, and
never sent on the renew path) — a public key cannot sign a PoP, so it was never the credential.
The authoritative device key is now the **generated + persisted private** key in `DeviceKeyStore`;
the `MULTIVIEW_LICENCE_DEVICE_KEY` placeholder is inert for PoP and is a deprecation candidate
(left in place this slice to avoid churning the cli config contract, since it is not load-bearing
for PoP). **No operator-provisioned device key is supported** — there is no secure way for an
operator to hand a *private* key in via the current env/config surface (env vars are gitignored
+ read-denied but still process-visible; the secret belongs only on the device's disk), and the
SSH-host-key model needs none. If an operator-provisioned model is ever wanted (e.g. a
pre-seeded fleet image), it would drop a `device-key.ed25519` seed into the lease dir before
first boot and `DeviceKeyStore::load_or_generate` would load it unchanged — but that is out of
scope here and not built.

### 3. The signer seam — `DeviceSigner` (leaf crate), implemented by the cli

```rust
pub trait DeviceSigner: Send + Sync {
    /// The raw 32-byte Ed25519 public point (the binding's bound device key).
    fn public_key_raw(&self) -> [u8; 32];
    /// Deterministic Ed25519 signature over `message` (RFC 8032 — no RNG).
    fn sign(&self, message: &[u8]) -> [u8; 64];
}
```

Ed25519 signing is **deterministic** (RFC 8032), so the signer needs **no RNG** — the leaf
crate's no-RNG invariant holds (only *generation* needs entropy, and that stays in the cli). A
loaded `SigningKey` signs; the leaf crate holds it through the seam exactly like the existing
`NowMs` / `NonceStore` seams. A `FixedDeviceSigner` test impl (a known seed) lets the leaf
crate's tests build a proof **and verify it against the public key** byte-exactly.

### 4. Canonical PoP pre-image + COSE_Sign1 — pure, in the leaf crate

A hand-rolled, total `canonical_pop_preimage(htm, htu, body, instance_id, nonce_hex, iat)`
mirrors the existing `canonical_key_preimage` style. The pre-image is a **deterministic-CBOR
`map(6)`** over the field order the spec names —
`htm | htu | sha256(body) | instance_id | nonce | iat` — encoded as:

| field | CBOR encoding |
| ----- | ------------- |
| `htm` | text string — the upper-case HTTP method (`"POST"`) |
| `htu` | text string — the full request URI (scheme+host+path, no query) |
| `sha256_body` | byte string — the raw 32-byte SHA-256 of the exact request body bytes |
| `instance_id` | text string — `DeviceIdentity::instance_id` |
| `nonce` | byte string — the **32 raw bytes** decoded from the 64-hex challenge nonce |
| `iat` | unsigned int — issued-at in **epoch seconds** (the server checks ±60 s) |

These pre-image bytes are the COSE_Sign1 **payload** (attached); the signature covers the COSE
`Sig_structure` wrapping `protected ‖ external_aad(empty) ‖ payload`. `pop_header_value(...)`
returns the standard-base64 of the untagged COSE_Sign1 — the `Conspect-Device-PoP` header value.

> **Spec ambiguities held as rule-26 follow-ups (flagged in the PR; the operator validates
> against the live server).** The spec gives the pre-image in `a | b | …` notation and does not
> spell out the concrete byte-layout, the iat unit (seconds inferred from the ±60 s leeway), or
> whether the payload is attached vs detached. The header **example** (`g1gg…`) decodes to
> `0x83 0x58 0x20 …` = a CBOR **array(3)** + 32-byte bstr, whereas a standard untagged
> COSE_Sign1 is **array(4)**; the example is truncated/illustrative, and the prose ("recomputes
> the pre-image … verifies the COSE_Sign1") describes standard COSE_Sign1 verification, so we
> emit the canonical 4-element form `coset` produces. The chosen pre-image CBOR map shape is the
> house style (matching the proven `canonical_key_preimage`); these are the load-bearing
> bytes the live server must confirm.

### 5. Nonce lifecycle — cold-start `/challenge`, steady-state `nextNonce`

A `LicenceServer::fetch_challenge(org) -> DeviceChallenge` method is added to the seam (the cli
implements it with the live `GET /challenge`). The `HeartbeatClient` holds the **PoP challenge
nonce** as control-plane state (a `Mutex<Option<PopNonce>>`, loop-only): on cold start (no held
nonce) it fetches `/challenge`; steady-state it uses the prior `HeartbeatResponse::next_nonce`.
This PoP nonce is **entirely separate** from the durable idempotency-key mint counter
(`FileNonceStore`) — they are different things (one is the server's single-use PoP challenge,
the other is the client's retry-stable mutation key) and are never conflated.

### 6. Wire on `heartbeat()` — header + `nonce` body field

The `LicenceServer::heartbeat` seam gains a `pop_header: &str` parameter (the base64
COSE_Sign1); `HeartbeatRequest` gains the `nonce` field. The cli's `ConspectHttpServer` attaches
`Conspect-Device-PoP` alongside the existing `Authorization: Bearer` + `Idempotency-Key`. The
`sha256(body)` in the pre-image is computed over the **exact serialized request body** the
transport sends, so the device and server hash the same bytes.

### 7. Fail closed (never off air, invariants #1/#10)

Every PoP failure mode keeps last-good and skips the cycle, exactly like #177's other failure
paths — a new `HeartbeatError::Pop(PopError)` variant joins the existing fail-closed set
(`Transport`/`KeyTrust`/`SignedLease`/`NonceStore` etc.):

- **No PoP nonce** (cold-start `/challenge` unreachable, or a missing/empty `nextNonce`) → no
  heartbeat mutation this cycle; keep last-good; back off; retry (fetch `/challenge` next cycle).
- **Nonce expired** (`expiresAtMs` in the past, or the server rejects it `pop-invalid`) →
  discard it, fetch fresh next cycle; keep last-good.
- **Signing error / keypair unavailable / unreadable** → keep last-good; back off; the engine
  is untouched (the heartbeat task holds no engine handle, so it is physically unable to stall
  output, just as the leaf-crate invariant guarantees).
- **All of the above are non-panicking** `Result` paths through the existing `run_forever`
  backoff. A keypair the cli cannot generate/persist → the heartbeat declines to start (fail
  closed) and the machine runs unlicensed-honest, never a crash.

### 8. Cross-vendor-panel hardening (round 2)

Two further majors from the round-2 review:

- **Retry coupling — `{Idempotency-Key, body, nonce, proof}` is ONE retry unit.** The
  Idempotency-Key is retry-stable, but the single-use PoP `nonce` is a **body field** hashed
  into the proof, so a fresh nonce changes the serialised body. A lost-response-after-commit
  retry that re-presented the same key with a *freshly-nonce'd* body would be rejected by a
  strict server as an idempotency body-mismatch and could strand the device. Fix: the loop pins
  a `PendingAttempt { idempotency_key, body, nonce, pop_header }` and **replays it verbatim** on
  a retry; only a genuinely-new logical operation builds fresh. The pin is cleared + the key
  rotated together (`clear_pending`) on a **successful server contact** — the server commits the
  mutation under the key regardless of whether the device then accepts the returned lease, so
  the contact (not the local install outcome) is the right success boundary.
- **Load-side TOCTOU — inode-bind the key read.** `DeviceKeyStore::load_existing` opens the key
  ONCE with `O_NOFOLLOW` (a symlink at the path is refused — the swap vector), `fstat`s the
  **open fd** (regular file + exactly `0600`), and reads from that **same fd**. So the perms
  checked and the bytes accepted provably come from one inode — a concurrent replacer cannot
  swap the path between the check and the read. (The install side already used
  `O_EXCL` + `hard_link` + explicit `0600`.)

#### Round 3 — status-aware retry (replay only on an ambiguous failure)

The round-2 coupling replayed the pinned `PendingAttempt` verbatim on **every** failed contact.
That is correct for an *ambiguous* failure (no response — the mutation may have committed) but
wrong for a **definitive** server rejection: a `401 pop-invalid`/`pop-required` (or `409`
body-mismatch) means the single-use nonce was **seen and burned**, so replaying it loops
`pop-invalid` forever and renewal is permanently stranded (the lease drifts to expiry while
last-good silently holds). The retry is now **status-aware**:

- The cli transport (`ConspectHttpServer::post_raw_json`, via the pure `heartbeat_status_error`
  helper) maps a **received 4xx** to a new `HeartbeatError::ServerRejected` (definitive) and a
  **5xx** to `Transport` (ambiguous); a no-response send error stays `Transport`. The leaf crate
  stays I/O-free — it only **matches** the variant.
- `run_once`: an **ambiguous** `Transport` keeps the attempt pinned and replays it verbatim; a
  **definitive** `ServerRejected` calls `reset_on_rejection` (drop the pinned attempt **and** the
  burned nonce) so the next cycle fetches a fresh `/challenge` and signs a fresh proof — the
  device key is untouched. Both keep last-good, back off, never panic.

**Invariant:** a single-use PoP nonce the server has seen and rejected is burned and must never
be replayed; replay only when it is unknown whether the server received the request.

## Rationale

- **The leaf-crate invariant is preserved exactly.** Generation (RNG) + persistence (I/O) — the
  only things the crate forbids — stay at the cli boundary. Ed25519 **signing is deterministic**
  (RFC 8032), so holding a loaded key and signing the COSE `Sig_structure` introduces no RNG and
  no I/O into the leaf crate. The byte-exact COSE/pre-image logic — the part that must be
  spec-exact and unit-tested — lives where it is testable in isolation, mirroring the existing
  `canonical_key_preimage` + key-trust verifier.
- **`coset` is the lowest-risk COSE choice**: its entire runtime closure is already in the graph
  (`ciborium`/`ciborium-io`), it is Apache-2.0 (deny-clean, no allowlist change), and its
  `create_signature(aad, signer)` API builds exactly the standard COSE_Sign1 the server
  verifies, with the Ed25519 signing delegated to a closure (so the seam stays clean and the
  RNG-free signing key stays at the boundary we choose).
- **The DPoP-nonce lifecycle** keeps the steady-state hot path free of an extra `/challenge`
  round-trip (the server hands the next nonce in each response), and `/challenge` is consulted
  only on cold start / nonce loss — matching the RFC 9449 pattern the spec cites.
- **Fail-closed everywhere** is the product promise: a licensing failure (including a PoP
  failure) must never take a broadcaster's program off air. PoP failures join the identical
  keep-last-good/back-off path #177 already proves under the CONSPECT-2 chaos gate.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Hand-roll the COSE_Sign1 (no `coset`) | The COSE `Sig_structure` + protected-header CBOR is exactly the kind of byte-exact wire the spec under-specifies; a vetted, in-graph (`ciborium`-only), deny-clean library removes a whole class of encoding bugs at zero dependency cost. We already hand-roll the *pre-image* (a fixed map) where the contract is fully pinned; the COSE envelope is not. |
| Generate + persist the keypair **in the leaf crate** | Violates the crate's load-bearing invariant (no I/O, no RNG/entropy in non-test code). Generation needs entropy and persistence needs filesystem I/O — both belong at the cli boundary, alongside the existing `FileNonceStore` and `ConspectHttpServer`. |
| Build the whole COSE proof **in the cli** | The COSE/pre-image bytes are the part most likely to be wrong and most needing isolated unit tests (self-verify a produced proof). Keeping them pure in the leaf crate (behind a `DeviceSigner` seam) makes them testable byte-exactly without a network or a real key, exactly like the key-trust verifier. |
| Reuse the durable idempotency `FileNonceStore` for the PoP nonce | They are different objects with different lifecycles — the idempotency key is a client-minted retry-stable mutation key; the PoP nonce is the server's single-use challenge (DPoP-nonce). Conflating them would break both (a replayed idempotency key must reuse one value; a PoP nonce must rotate every cycle). Kept strictly separate. |
| Emit a **tagged** COSE_Sign1 (tag 18) | The spec example and prose describe a bare COSE_Sign1 verified by recomputation; the untagged 4-element array is the canonical interchange form. Flagged as a rule-26 item to confirm against the live server (tagged vs untagged is a one-line `to_tagged_vec()` swap if the server requires the tag). |
| Wait for a golden vector / live server before implementing | The server is **enforcing now** and leases cannot renew — this is broken in production. The spec descriptions are byte-exact enough to implement spec-correctly and unit-test (self-verifiable COSE_Sign1 over the exact pre-image); live-server validation is the explicit rule-26 follow-up the operator runs. |

## Consequences

- **Renewal works again** once the live server accepts the PoP proof (the broken-in-production
  state is resolved). The device gains a **stable cryptographic identity** persisted across
  restarts; the lease binds its `cnf_jkt` (holder-of-key).
- **The default build is unchanged**: `coset` is gated behind `heartbeat` (off by default), so
  `cargo check --workspace` + `cargo deny check` stay a pure, network-free, LGPL-clean shell.
- **A new secret on disk** (`<lease-dir>/device-key.ed25519`, `0600`) — documented in the
  licensing runbook; it must be backed up/migrated with the lease state to preserve device
  continuity (losing it forces a re-bind, which consumes the 3-free-per-year rebind budget).
- **Invariants #1/#10 are honoured**: every PoP failure is a fail-closed, non-panicking,
  keep-last-good path; the heartbeat task holds no engine handle and cannot stall output.
- **Rule-26 follow-up is REQUIRED and flagged in the PR**: this implementation is
  **spec-correct + unit-tested** (it produces a structurally-valid COSE_Sign1 over the exact
  pre-image that an independent verifier — the test itself, with the public key — accepts). It
  is **NOT live-server-validated** here (no live Conspect account/server is available in this
  environment). The operator runs PoP against the live Conspect server; the load-bearing
  unknowns to confirm are: the pre-image byte-layout/CBOR shape, the iat unit (seconds vs ms),
  attached-vs-detached payload, and tagged-vs-untagged COSE_Sign1.
- **Forward-compat**: activate/rebind/deactivate also require PoP; the same
  `canonical_pop_preimage` + `pop_header_value` + `DeviceSigner` seam serve them when those
  slices land (only `htm`/`htu`/body differ).
