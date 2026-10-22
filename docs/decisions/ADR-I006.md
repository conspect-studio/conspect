# ADR-I006: CONSPECT-3 device heartbeat client — key-trust verification & install convergence

- **Status:** Accepted
- **Area:** Implementation build-out
- **Date:** 2026-06-16
- **Source:** agent session implementing CONSPECT-3 against the finalized Conspect `/v0` API (v0.6.1) + the live well-known key document; refines the plan in ADR-0096.

## Context

The runtime licensing subsystem (`multiview-licence`, ADR-0050/0051/0052) was ~90%
shipped; the only gap was the device→server heartbeat client, gated by ADR-0096 on
the Conspect API publishing D1 (key-trust), D2 (PoP request-signing), and D3
(canonical-CBOR lease pre-image). The blocker is now resolved: the Conspect `/v0`
API is finalized at v0.6.1 and the live well-known document
(`GET https://api.conspect.studio/.well-known/conspect-licensing-keys.json`) is
published. ADR-0096 sketched the implementation but left three load-bearing
details to the implementing wave; the live data settles them. Binding constraints:
the `multiview-licence` crate is **physically incapable of touching output** (no
engine handle, no I/O in the default build — its CLAUDE.md invariant); the default
`cargo check`/`cargo deny` must stay a network-free, LGPL-clean shell; and the
never-off-air invariants (#1 output-clock, #10 isolation) bind any licensing code.

ADR-0096 specified an "object-safe `trait LicenceServer`" and assumed a CBOR codec
could produce the canonical pre-image; both needed refinement against the real
crypto and the house concurrency patterns.

## Decision

Land CONSPECT-3 as the off-by-default `heartbeat` feature on `multiview-licence`
(+ the cli `heartbeat` feature pulling the live `reqwest`/rustls transport),
appended to the `nvidia`/`apple`/`linux-vaapi` presets only (`full` inherits;
mirrors `mesh-mdns`). The implementation makes these concrete decisions:

1. **Key-trust against the live well-known doc** (`crates/multiview-licence/src/heartbeat.rs`,
   `keytrust`): `PinnedRoot` (ECDSA P-256, SEC1 uncompressed point via `p256`
   0.13) + `TrustedKeys::verify` — verify the advertised root byte-matches the
   pinned anchor (`RootMismatch` up front), verify the **root-attested revocation
   list** (fail closed: a forged/absent `root_revocation_sig` rejects the keyset),
   verify each intermediate's `root_sig` (ECDSA-P256/SHA-256 raw r‖s, base64url)
   over its canonical key pre-image, then trust the attested intermediates that are
   in-validity and not revoked, accepting both `current` and `next` (dual-pin).
2. **A hand-rolled RFC 8949 §4.2.1 canonical CBOR encoder** for the key &
   revocation pre-images (`canonical_key_preimage` / `canonical_revocation_preimage`),
   **not** a serde-CBOR codec. The pre-image is a fixed small map whose keys, in
   the well-known `key_pre_image` order `[key_id, key_type, statement,
   public_key(bstr raw-32), valid_from(uint), valid_until(uint)]`, already equal
   the canonical sort order — verified **byte-exact** against the live
   `intermediate-v1`/`intermediate-v2` `root_sig` and `root_revocation_sig` (a
   golden-vector test pins the exact bytes).
3. **Bare-Ed25519 signed-lease verification** (`verify_signed_lease_chain`):
   resolve `signerKeyId` to a trusted intermediate, verify the lease's bare Ed25519
   signature (lower-case hex, 64 bytes) over the **STANDARD-base64**-decoded
   (RFC 4648 §4, *not* base64url) `leaseBytes` — the device verifies the signature
   over the exact bytes received and never re-serializes the lease body — then
   `ciborium`-parse that body for the authoritative offline-enforcement inputs
   (`gpu_limit`, `hardware_class`, `not_after`).
4. **A generic `HeartbeatClient<S: LicenceServer>`** with a native
   `async fn`-in-trait + `Send` future seam (the house `ZowietekTransport`
   pattern), **not** an `async-trait`/`dyn`-object trait. The live HTTP transport
   (`ConspectHttpServer`) lives at the cli/app boundary (which owns `reqwest`);
   the leaf crate opens no socket. The loop drives the existing
   `LeaseStore::install_binding` convergence on a positively-verified lease and
   keeps last-good on every failure **and** on a withheld lease (`lease: null` —
   revocation by non-reissue).
5. **Environment-driven config** in the cli (`HeartbeatSettings::from_env`,
   `crates/multiview-cli/src/licence.rs`): the org id (config-driven, ADR-0096 O4),
   the pinned root, the API base, the account JWT, an optional claim code, and the
   salted device identity, all read from `MULTIVIEW_LICENCE_*` env vars — secret
   material never in the binary. `spawn_heartbeat()` (real under `cfg(heartbeat)`,
   no-op otherwise) is called beside `spawn_mesh_discovery()` at both `main.rs`
   run sites.
6. **The verified-body → entitlement mapping is fail-closed and purpose-bound**
   (hardened after the rule-21 Codex panel — the crypto chain is correct, but the
   guarantees must not be dropped at the mapping). Binding rules:
   - **Key-purpose binding:** a `root_sig`-attested intermediate is trusted as a
     lease signer **only** when its signed pre-image declared `key_type == "lease"`.
     A root-attested key minted for another purpose (e.g. an `update` key) is never
     a lease signer (a skip, not a keyset-poisoning reject). **The unsigned `status`
     field is NOT a trust gate** (see decision point #7): trust rests only on signed
     fields — `key_type`, the validity window, and the revocation list.
   - **Signed expiry is authoritative:** the installed lease's `expires_at` is the
     cryptographically-signed `not_after` (`Lease::new_online_expiring_at`
     back-derives `granted_at = not_after − 35d`), **never** `system_now() + 35d`.
     A signed lease already past its `not_after` is rejected
     (`HeartbeatError::LeaseExpired`, keep last-good) — so a short-lived or
     replayed-older still-Ed25519-valid lease can never mint a fresh term.
   - **Decode exactly:** `leaseBytes` is `STANDARD.decode(trim())` (no `=`
     stripping) — a canonically-padded body (CBOR length % 3 ≠ 0) decodes.
   - **Required fields fail closed:** `instance_binding_id` / `serial` /
     `licence_id` must be present **and non-empty** (`ok_or(MalformedBody)`), never
     `unwrap_or_default()`.
   - **Address the binding by id:** renewals use the server-issued
     `instanceBindingId` learned from the verified body
     (`HeartbeatClient::learned_binding_id`), **never** the lease serial.
   - **Reject nonsensical timestamps:** a negative `valid_from`/`valid_until` is
     rejected up front rather than coerced to unsigned 0 by the CBOR encoder.
   The never-off-air isolation is re-proven with a **genuine in-flight stall**
   (a black-holed transport call), asserting a concurrent store reader and the
   output clock are never blocked while a heartbeat call is parked (inv #10).
7. **Identity anchoring + signed-only trust** (hardened after the **round-2**
   rule-21 panel, which confirmed #1–#6 correct and found deeper holes). Binding
   rules:
   - **Cross-instance replay defence:** `install()` is anchored to this device's
     established binding (configured, or learned from a prior successful install).
     Once a binding is established, a returned body whose `instance_binding_id`
     differs is rejected (`HeartbeatError::BindingMismatch`) — a valid
     Conspect-signed lease minted for **another device** cannot be replayed here.
   - **Real fingerprint gate:** `seal_for_install` stamps the device's **actual**
     `identity.fingerprint_score`, never an unconditional `FINGERPRINT_MATCH_STRONG`.
     The store's fingerprint-continuity gate then genuinely rejects a non-matching
     machine (`HeartbeatError::FingerprintMismatch`, keep last-good).
   - **No reject-path identity poisoning:** the server-issued `instanceBindingId`
     is learned (`remember_binding_id`) **only after** a successful `install()`.
     A rejected (expired / foreign / stale) lease never mutates the learned
     binding, so the next renewal still addresses the legitimate binding.
   - **`status` is not a security gate (cryptographic binding):** the well-known
     `status` field is **not** in the root-signed key pre-image (`map(6)`:
     `key_id,key_type,statement,public_key,valid_from,valid_until`), so a MITM /
     compromised document can flip a retired key's `status` to `current` without
     breaking `root_sig`. Trust therefore rests on **signed** fields only:
     `key_type == "lease"` ∧ `now ∈ [valid_from, valid_until]` ∧ not in the signed
     revocation list. Retirement is expressed via the signed validity window or the
     signed revocation list — never the unsigned `status` (operational hint only).
   - **`gpu_limit` fails closed:** a **present-but-invalid** `gpu_limit` (negative,
     non-integer, or `> u32::MAX`) is `MalformedBody`; only an **absent** `gpu_limit`
     means `Unlimited`. A malformed-but-signed body can no longer install unlimited
     GPUs (the least-restrictive value).
   - **Deterministic expiry test:** the round-1 replay test passed via
     `InstallError::Stale` (a fixed-future fake epoch masked the path); a lease with
     an absolute-past `not_after` now deterministically exercises
     `HeartbeatError::LeaseExpired`.
8. **Install-genuineness + acceptance-time trust** (hardened after the **round-3**
   rule-21 panel, which confirmed #1–#7 correct and found two deep fail-open paths).
   Binding rules:
   - **"Ok from `install()`" ≠ "installed":** `install()` returns an
     `InstallOutcome::{Installed, StaleNoop}` rather than folding `Err(Stale) → Ok`.
     The binding id is learned (`remember_binding_id`) **only on `Installed`** —
     never on the stale no-op. Otherwise a crypto-valid-but-**stale FOREIGN** lease,
     which the store correctly keeps-last-good (`Stale`), would still poison the
     learned identity with the foreign binding without anything being installed.
   - **The store IS the device identity:** a genuine install records the binding in
     the store (in round-4 this moved into `LeaseStore::install_binding` as the
     single chokepoint — see decision point #9); the heartbeat anchor is
     `established_binding = configured ?? learned ?? store.current_binding_id()`. So
     a device that already holds a lease is **never "fresh"** — the cross-instance
     guard rejects a foreign binding even on the activation path (the round-2 guard
     was skipped when `established_binding == None`).
   - **Trust is re-evaluated at lease-ACCEPTANCE with a fresh clock (no TOCTOU):**
     the key-trust chain is verified again with a fresh `now()` immediately before
     `verify_signed_lease_chain` accepts the returned lease (`HeartbeatClient` gains
     a `NowMs` clock seam). A signer whose signed validity window elapses — or that
     is revoked — **during** an arbitrarily-stalled network call no longer validates
     the lease, because `verify_signed_lease_chain`/`lease_key` only check the
     Ed25519 signature, not time-validity/revocation. The pre-network verify is a
     fast-fail only; both reads go through the seam.
9. **Fresh-fetch acceptance trust + the single binding-anchor chokepoint +
   retry-stable idempotency** (hardened after the **round-4** rule-21 panel, which
   confirmed #1–#8 correct and found the round-3 acceptance re-check did not fully
   close two fail-open paths). Binding rules:
   - **Acceptance trust is re-evaluated against a FRESHLY RE-FETCHED key document,
     not the pre-network one (no revocation TOCTOU):** revocation is set-membership
     over the signed key document, so the round-3 fix — a fresh `now()` against the
     **same** fetched document — caught only an elapsed *validity window*, never a
     signer **added to the (signed) revocation list** during a stalled call.
     `run_once` now re-fetches `fetch_keys()` at acceptance and re-runs
     `TrustedKeys::verify` on that fresh document (which re-checks the root match,
     the root-attested revocation signature, every intermediate `root_sig`, the
     signed validity window at the fresh `now()`, **and** the revocation set). A
     newly-revoked or newly-expired signer is dropped from the re-fetched trusted
     set, so `verify_signed_lease_chain` cannot resolve `signerKeyId` and rejects
     the lease. The re-fetch fails closed on a transport error (keep last-good).
   - **`LeaseStore::install_binding` IS the binding-anchor chokepoint:** the
     round-3 anchor (`store.record_binding_id`) fired **only** from the heartbeat
     genuine-install path, so a lease installed via the control-route/offline
     upload, the file-drop watcher, or the mesh relay (all of which call
     `install_binding` directly) left `current_binding_id() == None` — the device
     looked "fresh" and a foreign-binding activate **skipped** the cross-instance
     guard (identity poisoning). The binding id now rides on the `LeaseBinding`
     (`instance_binding_id: Option<String>`, carried by every producer) and
     `install_binding` records it **atomically with the install**, so **every**
     surface anchors the device identity uniformly; the now-redundant
     heartbeat-path `record_binding_id` call is removed (no double-record).
   - **The anchor comes from SIGNED material (never an unsigned sidecar):** the
     panel's follow-up flagged that `LeaseBinding.instance_binding_id` was outside
     the crate's `SignedLease` envelope (which covered serial+source+dates only),
     so the offline/file-drop/relay surface could anchor an attacker-chosen binding
     id. `SignedLease::signing_bytes` now also covers `instance_binding_id` (a
     1-byte presence tag + length-prefixed bytes, so `None`, `Some("")`, and
     `Some("x")` are distinct signed values), `verify_signed_lease` recomputes
     over the binding id the caller will anchor, and `install_binding` passes
     `binding.instance_binding_id` into that check — so a grafted/tampered or
     absent-vs-present binding id fails `SignatureInvalid` and never anchors. Every
     producer signs over the id it carries (`seal_for_install`, the offline/relay
     minters). A binding-less (`None`) producer still installs and anchors nothing
     — forward-compatible with a server later signing a binding id.
   - **The Idempotency-Key is retry-stable per logical operation:** the round-3
     key was `format!("mv-{}", unix_millis_now())` — minted fresh per call inline,
     so a retry of the same logical operation issued a **new** key, defeating "a
     retry replays, never re-issues" (lost-response duplicate-mutation risk). The
     client now holds an `IdempotencyState { counter, current }`: a key is minted
     once per logical operation from a **monotonic per-client counter + the device
     machine id** (never the wall clock), **replayed** on every retry, and rotated
     **only after a fully-successful contact** (install / stale-no-op / withheld
     lease). An error *after* the mutation landed does not rotate, so its retry
     also replays — the server dedupes, never duplicating a binding/lease.
10. **Atomic install publish + durable idempotency nonce** (hardened after the
    **round-5** rule-21 panel, which confirmed #1–#9's intent but found the
    binding-anchor + idempotency fixes incomplete). Binding rules:
    - **The install publishes ONE atomic snapshot:** `LeaseStore` previously wrote
      `active` / `installed_at` / `fingerprint_score` / `instance_binding_id` in
      four separate `RwLock` critical sections, so a concurrent reader could observe
      `current().is_some()` while `current_binding_id()` was still `None` — a torn
      install that makes a freshly-licensed device look "fresh" to the heartbeat
      client (`established_binding == None`) and SKIP the cross-instance guard. The
      four fields are collapsed into one `Installed` struct behind a single
      `RwLock<Option<Installed>>`; `install_binding` publishes it in one write and
      every reader reads that one lock, so no reader sees a torn state. The round-4
      "recorded atomically" comment (false then) is now true. The dead
      `record_binding_id` standalone setter — which modelled exactly the
      binding-without-lease partial state — is removed (no production callers after
      #9 moved anchoring into `install_binding`).
    - **The binding anchor is bound into the SIGNED lease bytes** (the #9 follow-up,
      see commit history): `SignedLease::signing_bytes` covers `instance_binding_id`
      (presence tag + length-prefixed bytes); `verify_signed_lease` recomputes over
      the binding id `install_binding` will anchor, so a tampered CBOR binding id on
      the file-drop / mesh-relay path (inner lease otherwise validly signed) fails
      `SignatureInvalid` and never anchors — proven via the `from_bytes`→install
      tamper test.
    - **The idempotency mint counter is durable across restart:** the round-4
      counter lived in an in-memory `IdempotencyState` that reset to 0 on restart,
      so a post-restart op reused `mv-{machine}-1` and collided with a prior
      lifetime's first op. The counter is now seeded from — and committed to — a
      durable `NonceStore` seam (the leaf crate does no I/O; the cli supplies a
      file-backed `idempotency-nonce` beside the lease state, write-temp-then-rename,
      fail-open to 0). A new key is minted strictly above both the in-process and
      durable high-water (`saturating_add`) and the high-water is committed AT MINT,
      so a restart never reuses a prior lifetime's key. Within a process the round-4
      retry-replay is unchanged. Scope: this closes cross-restart key COLLISION (the
      distinctness test); replaying an *unacknowledged in-flight* op across a crash
      (persisting the in-flight key) is a heavier tier, not required here and not
      claimed.
11. **The client is RENEW-ONLY; device-side online-activate is DEFERRED** (operator
    decision after a **round-5c** 0.7.0 conformance audit). The audit found the
    activate path sent `ActivateRequest.serverNonce = String::new()` (empty), but
    the spec requires `serverNonce` to match `^[0-9a-f]{2,128}$` — it is a
    **server-issued** value (the freshness anchor for the per-instance lease chain),
    and the device-credential/nonce mechanism that would let a device obtain one is
    marked in the Conspect spec as "deferred to ADR-0036 §Deferred / not yet
    available". So a device **cannot** mint a valid activation request today, and a
    real server `422`s the empty value. Per rule 6 (never ship a stub/scaffold) the
    broken activate path is **not shipped**. Binding rules:
    - **Onboarding is operator/portal + the install surfaces, not the device:** the
      operator activates a licence in the Conspect portal, and the signed lease
      reaches the device via the **three existing install surfaces** — control-upload
      (`POST /api/v1/licence/lease`), the offline file-drop watcher, and the mesh
      relay — all of which feed `LeaseStore::install_binding`. Device-side activate
      was never required for onboarding.
    - **`run_once` is renew-only:** it resolves the binding to renew *before* any
      network call (`configured ?? learned ?? store.current_binding_id()`). With an
      established binding it RENEWS via the unchanged heartbeat path. With **no**
      binding there is nothing to renew and the device cannot self-activate, so it
      makes **no** server call, installs nothing, keeps last-good (output on air),
      and returns the new `HeartbeatOutcome::NoBinding` — a lease arrives via an
      install surface and a later cycle renews it.
    - **The dead activate scaffold is removed, not stubbed:** the `activate`
      `LicenceServer` method, `build_activate_request`, the
      `ActivateRequest`/`ActivateResponse` wire types, the run_once activate branch,
      the cli `ConspectHttpServer::activate` impl, and the activate-only settings
      (`HeartbeatConfig.claim_code`, `HeartbeatSettings.claim_code`/`CLAIM_ENV`) are
      deleted. The `DeviceIdentity` device-credential fields (`instance_id`,
      `hardware_digest`, the discriminator hash/digest, `device_public_key_b64url`)
      are **retained** on the identity (the cli's `MULTIVIEW_LICENCE_*` config
      contract) with `forward-compat:` docs, so the activate slice re-adds without
      re-plumbing the device config when the server-nonce flow lands; they are not
      sent today.
    - **`transport` is a closed enum:** `HeartbeatRequest.transport` is a closed
      `Transport::{Direct,Relay,File}` enum (default `Direct`), not an open `String`,
      so an out-of-vocabulary value can never reach the wire (a future `422` for an
      unknown transport label is structurally impossible).
    - **External blocker (named):** device-side activate is re-added as a future
      slice when the Conspect server issues the `serverNonce` (the per-instance
      lease-chain freshness anchor, part of the device-credential mechanism deferred
      to ADR-0036). This is the only blocker; nothing else in the activate path is
      missing on the device side.
12. **The cli boundary wiring FAILS CLOSED** (hardened after the **round-6** rule-21
    final panel, which confirmed #1–#11's core — the signed binding-anchor across
    all four install paths, the atomic single-lock install, renew-only, the
    revocation-TOCTOU re-fetch, and the crypto chain — clean, and found three
    fail-open paths only in the cli boundary). Binding rules:
    - **The durable idempotency nonce fails closed (no silent reset, no
      log-and-continue):** the `NonceStore` seam is now fallible
      (`load`/`commit → Result<_, NonceError>`). `FileNonceStore::load` returns an
      error for a **present-but-corrupt/unreadable** file — only an **absent** file
      is the trusted `Ok(0)` fresh start (a silent `0` would reset the high-water and
      re-mint `mv-{machine}-1`, colliding with a prior lifetime's first op after a
      restart). `FileNonceStore::commit` **propagates** both the write and the rename
      failure (an un-persisted high-water must block the mutation, not continue).
      `HeartbeatClient::idempotency_key` gates the mint on a trustworthy `load` + a
      successful durable `commit` **before** the key is exposed: on a nonce-store
      error it advances nothing and returns `HeartbeatError::NonceStore`, so
      `run_once` sends **no** mutation that cycle (a non-durable key could collide
      across a restart), keeps last-good, and retries next cycle — a nonce-store I/O
      failure never tightens output (inv #1/#10). The round-5 "a restart may reuse a
      key" comments (false then) are removed (rule 27).
    - **The HTTP transport is HTTPS-only and fails closed:**
      `ConspectHttpServer::new` propagates the `https_only(true)` build with `?`
      (was `unwrap_or_default()`, which silently fell back to a **default**
      non-HTTPS-only client while every request still attached the bearer JWT — a
      plaintext credential leak on an `http://` base). A failed build disables the
      heartbeat (keep last-good); `spawn_heartbeat` never constructs a
      plaintext-capable client.
    - **The durable nonce has an interprocess guard:** two `multiview` processes
      sharing one lease-state dir could each load the same high-water and mint
      colliding keys. `FileNonceStore` takes a **non-blocking exclusive advisory
      lock** (`rustix::fs::flock(NonBlockingLockExclusive)`, a safe wrapper) on
      `<dir>/idempotency-nonce.lock` at construction and holds it for the process
      lifetime (the OS releases it on exit, so a crashed owner never strands it). A
      second owner is refused and its heartbeat declines to start (fail closed). The
      anti-aliasing `assert_ne!(binding_id, lease_serial)` guard — dropped when
      round-5c removed `the_renewal_addresses_the_binding_by_id_…` — is restored on
      the renew-anchor test so "renew addresses by the signed `instanceBindingId`,
      never the lease serial" is a meaningful (non-tautological) assertion (rule 19).
13. **The durable nonce is CRASH-DURABLE and rejects a present `0`** (hardened after
    the **round-8** fix-verification panel, which confirmed #12's fail-closed
    load/commit + the held flock + the HTTPS `.build()?` + the restored `assert_ne`
    correct, and found two narrow durability fail-opens in `FileNonceStore`).
    Binding rules:
    - **`commit` is crash-durable (fsync), not just torn-write-safe:** the round-6
      `commit` did `std::fs::write` + `rename` with no fsync, so a power loss right
      after `commit` returned could roll the high-water back and let the next process
      re-mint a colliding key after reboot — the exact cross-restart replay the
      durable nonce exists to prevent, and under-delivering its own "durable commit"
      claim (rule 27). `commit` now uses the standard atomic-durable protocol:
      `File::create` + `write_all` → `sync_all()` (fsync the **temp** before the
      rename) → `rename` → open the **parent dir** and `sync_all()` (fsync the
      rename's directory entry). **Every** step (write, both fsyncs, rename)
      propagates as `NonceError`; the gated mint refuses on any failure, so the cycle
      keeps last-good (inv #1/#10). No `unwrap`/`expect`/`panic`.
    - **A present `0` is untrustworthy:** `commit` only ever persists
      `max(durable).saturating_add(1)` (≥ 1), so a present file reading the literal
      `0` can only be truncation / a partial write / tampering. `load` now rejects a
      present `0` (`Err`); the **only** trusted-zero path is an **absent** file
      (`Err(NotFound) ⇒ Ok(0)`, the fresh-device start). Any value ≥ 1 still loads.
    - **The flock's scope is documented (rule 27):** the interprocess uniqueness
      guarantee holds only for cooperating owners using this `FileNonceStore` on a
      local FS with working `flock` semantics — not over NFS/overlayfs, and not
      against a process reaching the same lease dir via a different code path. That
      operational assumption is now a doc note on `FileNonceStore`.

New deps behind `heartbeat`: `p256`, `base64`, `hex` (all `MIT`/`Apache-2.0`, with
the ECDSA closure resolving to `MIT`/`Apache-2.0`/`BSD-3-Clause`/`BSD-1-Clause`),
`tokio`; the cli adds `reqwest` (rustls) and — round-6 — `rustix` (`fs` feature,
for the safe `flock`; already in the workspace graph via the engine `ptp`/`display-kms`
paths, `MIT`/`Apache-2.0`/`BSD`, adds no new crate to `Cargo.lock`). `cargo deny`
runs `all-features = false`, so none enter the scanned default graph.

## Rationale

- **Hand-rolled canonical CBOR** is the safe choice because `ciborium::into_writer`
  preserves map *insertion* order rather than sorting keys to RFC 8949 §4.2.1, so
  relying on it for a signature pre-image would be a latent security bug if a field
  were ever reordered. A fixed, total, ~30-line encoder is auditable and is proven
  byte-exact against the production `root_sig` — the strongest possible golden
  vector.
- **Generic over `dyn`** matches the established `ZowietekTransport` pattern (no
  `async-trait` dep, no per-call heap alloc, no `Send`-bound surprises), and the
  spawn point holds exactly one concrete server type, so generics lose nothing.
- **Verify-over-received-bytes, never re-serialize** sidesteps any dependence on
  reproducing Conspect's lease-body canonicalization: the server hands the exact
  signed bytes, and `install_binding`'s body fields are read order-independently.
- **Fail closed on trust, lenient on enforcement** keeps the never-off-air promise:
  a rejected chain or bad signature merely withholds the next lease (the previous
  ages naturally); only a positively-verified signed lease ever tightens anything.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Use `ciborium` (a serde-CBOR codec) to build the key pre-image | It preserves insertion order, not RFC 8949 §4.2.1 canonical key order; trusting that for a signature pre-image is a latent security defect. The pre-image is a tiny fixed map — a hand-rolled canonical encoder is auditable and provably correct against the live `root_sig`. |
| Object-safe `dyn LicenceServer` (boxed-future trait or `async-trait`) per ADR-0096's wording | The repo's house pattern is native `async fn` in trait + generic dispatch (`ZowietekTransport`/`ZowietekClient<T>`); it is alloc-free, `async-trait`-free, and the spawn point holds one concrete server, so a generic `HeartbeatClient<S>` is strictly better. `async fn` in trait is not `dyn`-compatible without boxing anyway. |
| Re-derive Conspect's canonical lease-body CBOR to re-verify the lease | Unnecessary and fragile: the server returns the exact signed `leaseBytes`; verifying the bare-Ed25519 signature over the received bytes (then parsing fields order-independently) is simpler and cannot drift from the server's encoding. |
| Put the `reqwest` HTTP transport inside `multiview-licence` | Breaks the leaf crate's "no I/O, no socket" invariant (its CLAUDE.md) and would pull a TLS stack into a pure-data crate. The crypto/verify lives in the testable crate; the socket lives at the cli boundary behind the `LicenceServer` seam. |
| Hard-code the free auto-issue default org id | The default `{orgId}` for a free self-host is an external-doc residual (ADR-0096 O4, not yet provided). A guessed value would be wrong; the org id is a clearly-named env-driven config field, off unless set. |
| Mint the installed lease term from `system_now() + 35d` (ignoring the signed `not_after`) | Defeats the licensing crypto: a short-lived or **replayed older** still-Ed25519-valid lease would become a fresh 35-day entitlement (signed-expiry bypass / lease replay). The installed `expires_at` MUST be the signed `not_after`, and an already-expired signed lease is rejected. |
| Trust any `root_sig`-attested intermediate as a lease signer (ignore `key_type`) | A root may attest keys for several purposes; accepting a non-lease (e.g. `update`) key as a lease signer is a key-purpose bypass that the signed `key_type` field exists to prevent. Enforce `key_type == "lease"` (signed) — and **only** signed fields for the rest (validity window, revocation), never the unsigned `status`. |
| Default omitted/empty `instance_binding_id`/`serial` to `""` (`unwrap_or_default`) | A signed-but-malformed body would install with an empty id and mis-bind enforcement/renewals. Required identity fields fail closed. |
| Trust a lease for any binding (no install-time identity check) + stamp `FINGERPRINT_MATCH_STRONG` | Cross-instance replay: a valid Conspect-signed lease minted for another device installs onto this one, and the hardcoded strong stamp bypasses the store's fingerprint gate. Anchor install to the established binding + stamp the real fingerprint score. |
| Use the well-known `status` field to gate lease-signer trust | `status` is NOT in the root-signed pre-image, so a MITM can flip a retired key to `current` without breaking `root_sig`. Only signed fields (`key_type`, validity window, revocation list) may gate trust; `status` is an operational hint. |
| Treat any `Ok` from `install()` (incl. the `Stale → Ok` fold) as "installed" and learn the binding | Reject/stale identity poisoning: a crypto-valid-but-**stale FOREIGN** lease that the store keeps-last-good (`Stale`) would still poison the learned binding with the foreign id, with nothing installed. `install()` returns `Installed` vs `StaleNoop`; learn only on `Installed`. |
| Anchor the cross-instance guard to the heartbeat-learned binding only (skip it when `established_binding == None`) | A device with a pre-existing local store lease but no learned/configured heartbeat binding takes the activate path and the guard is skipped — a foreign lease installs. Fold the store's current lease binding into the anchor (`store.current_binding_id()`), recorded on every genuine install. |
| Evaluate signer time-validity / revocation only once, with the pre-network timestamp | Key-trust TOCTOU: a signer whose signed `valid_until` elapses (or that is revoked) **during** an arbitrarily-stalled network call still validates the returned lease, because the frozen `trusted` set is reused and `verify_signed_lease_chain` checks only the Ed25519 signature. Re-evaluate trust with a fresh `now()` at lease-acceptance. |
| Re-check acceptance trust with a fresh clock against the **same** fetched key document (round-3) | Catches only an elapsed validity *window*; revocation is set-membership over the signed document, so a signer added to the revocation list **during** a stalled call is never observed against the stale doc. Re-FETCH the key document at acceptance and re-verify the fresh one (root + revocation-sig + intermediates + window + revocation set). |
| Record the binding anchor only from the heartbeat genuine-install path (`store.record_binding_id`) | A lease installed via the control-route/offline upload, the file-drop watcher, or the mesh relay leaves `current_binding_id() == None`, so the device looks "fresh" and a foreign-binding activate skips the cross-instance guard (identity poisoning). Carry the id on `LeaseBinding` and record it atomically inside the single `install_binding` chokepoint every surface converges on. |
| Anchor identity from the `LeaseBinding.instance_binding_id` field as an unsigned sidecar | The field sat outside the crate's `SignedLease` envelope (serial+source+dates only), so the offline/file-drop/relay surface could anchor an attacker-chosen binding id (identity poisoning / renewal DoS). Bind `instance_binding_id` into `signing_bytes` (presence tag + bytes) and verify it in `install_binding`, so the anchor always rests on signed material; a grafted id fails `SignatureInvalid`. |
| Mint the Idempotency-Key from `unix_millis_now()` per call | A retry of the same logical operation mints a NEW key, so a lost response can create a DUPLICATE binding/lease ("replay, never re-issue" violated). Use a per-operation key from a monotonic counter + the device id, replayed on retry and rotated only after a successful contact. |
| Write the install's lease / install-instant / fingerprint / binding-anchor in separate `RwLock` sections | A concurrent reader can observe a torn state — `current().is_some()` while `current_binding_id()` is still `None` — so a freshly-licensed device looks "fresh" and the cross-instance guard is skipped. Publish one `Installed` snapshot under a single lock so a reader never sees a partial install. |
| Keep the idempotency counter in memory only (rotate on success) | Retry-stable within a process but the counter resets to 0 on restart, so a post-restart op reuses `mv-{machine}-1` and collides with a prior lifetime's first op (cross-restart duplicate mutation). Seed + commit the counter via a durable `NonceStore` seam (file-backed at the cli boundary); mint strictly above the durable high-water and commit at mint. |
| Persist the idempotency nonce inside `multiview-licence` | Breaks the leaf crate's no-I/O invariant. The crate exposes a `NonceStore` seam (like the clock); the file-backed implementation lives at the cli boundary. |
| Ship device-side online-activate with `serverNonce = ""` (empty) | The spec requires `serverNonce` to be a **server-issued** `^[0-9a-f]{2,128}$` value (the per-instance lease-chain freshness anchor); a real server `422`s an empty value, and the device-credential/nonce mechanism is "deferred to ADR-0036 §Deferred / not yet available", so the device cannot mint a valid request. Shipping it is a broken stub (rule 6). DEFER activate; ship the client RENEW-ONLY (decision point 11). |
| Keep the activate code behind `#[allow(dead_code)]` until the nonce flow lands | Dead scaffold with an unjustified suppression (rules 6 + 20). Onboarding does not need device-side activate (operator/portal + the three install surfaces handle it), so the activate path is removed; the device-credential `DeviceIdentity` fields are retained with `forward-compat:` docs so it re-adds cleanly. |
| Model `HeartbeatRequest.transport` as an open `String` | An out-of-vocabulary transport value could reach the wire and earn a `422`. The Conspect set is the fixed `{direct,relay,file}`; a closed `Transport` enum makes a bad value structurally unsendable. |
| `FileNonceStore::load` returns a silent `0` on a corrupt/unreadable nonce file (round-5) | A present-but-untrustworthy value silently resetting the high-water re-mints `mv-{machine}-1` and collides with a prior lifetime's first op after a restart. Fail closed: only an **absent** file is the trusted `Ok(0)`; a present-but-corrupt/unreadable file is `Err`, and the mint (gated on `load` + a durable `commit`) refuses, so no mutation is sent. |
| `FileNonceStore::commit` logs-and-continues on a write/rename failure (round-5) | An un-persisted high-water is exactly the cross-restart collision risk the durable nonce exists to prevent. Propagate both failures as `Err`; the gated mint then blocks the mutation (keep last-good) rather than send a possibly-colliding key. |
| Build the `reqwest` client with `.build().unwrap_or_default()` | On a build error `unwrap_or_default()` drops `https_only(true)` for a **default** client while every request still attaches the bearer JWT — a plaintext credential leak over `http://`. Propagate with `?`; a failed HTTPS-only build disables the heartbeat (keep last-good), never a plaintext-capable client. |
| Leave the single nonce file unguarded across processes (document single-owner only) | Two `multiview` processes sharing a lease-state dir could each `load` the same high-water and mint colliding keys. A real non-blocking `flock` (held for the process lifetime, OS-released on crash) is stronger than a documented invariant and cheap (`rustix` is already in the graph): a second owner is refused and fails closed. |
| `commit` with `write` + `rename`, no fsync | A power loss right after `commit` returns can roll the high-water back (data or the rename's directory entry not yet on stable storage), so the next process re-mints a colliding key after reboot. fsync the temp **before** the rename and fsync the parent dir **after**; propagate every step's error so the gated mint refuses on failure. |
| Trust a present nonce file that reads `0` | `commit` never writes `0` (the mint is `≥ 1`), so a present `0` is truncation/partial-write/tamper; trusting it resets the high-water and re-mints `mv-{machine}-1` after a restart. Reject a present `0` (`Err`); only an **absent** file is the trusted-zero fresh start. |

## Consequences

- **Easier:** a device with a lease (installed via the portal-driven control-upload,
  the offline file-drop, or the mesh relay) RENEWS it against conspect.studio over
  the same `install_binding` convergence those surfaces use, so S1/S2/S3 + the
  control routes + the web screens re-sample with zero extra wiring; the read-only
  heartbeat-status route now has a real device→server producer. Onboarding stays an
  operator/portal action (the device is renew-only) — no device-side activation
  needed.
- **Committed to maintain:** the canonical-CBOR encoder must stay byte-exact with
  the Conspect attestation contract (the golden-vector test guards this against the
  live well-known doc); the `MULTIVIEW_LICENCE_*` env surface is the device-config
  contract; the `p256`/`reqwest` closures must stay deny-clean.
- **Invariants:** the heartbeat task holds no engine handle/channel/lock (#10) and
  only ever tightens on a positively-verified signed lease, keeping last-good on
  every failure/withheld lease (#1) — re-proven by the extended never-off-air chaos
  gate (`crates/multiview-cli/tests/heartbeat_never_off_air.rs`) that SIGKILLs /
  stalls / partitions the heartbeat task while asserting one-frame-per-tick. The
  round-6 fail-closed paths extend this: a durable-nonce I/O failure and an
  un-buildable HTTPS-only client both keep last-good (no mutation, heartbeat off),
  never a colliding-key mutation and never a plaintext credential-carrying client.
  Round-8 makes the nonce **crash-durable** (fsync temp + parent dir) and rejects a
  present `0`, so a power loss / truncation cannot roll the high-water back into a
  colliding-key re-mint after a reboot.
- **Deferred (named blockers, not stubbed):** device-side **online-activate** is
  deferred — the external blocker is the Conspect server-side `serverNonce`
  issuance (the per-instance lease-chain freshness anchor, part of the
  device-credential mechanism deferred to ADR-0036); the client ships RENEW-ONLY
  and onboarding is via operator/portal + the three install surfaces (decision
  point 11). The device-PoP request-signing wire format (D2, deferred server-side,
  slice 5d) — the `devicePublicKey` is captured + stored but does not yet
  authenticate requests (account-JWT bearer today). The free-tier default org id
  (O4) is a config field, off until set. All are honestly tracked, not stubbed.
- **CI/licensing:** the default build is unchanged (network-free, LGPL-clean); the
  `heartbeat` feature is on only in the shipped deploy presets. `cargo deny`
  (`all-features = false`) is unaffected; the round-6 `rustix` (`fs`) addition is
  behind `heartbeat`, already in the workspace graph (engine `ptp`/`display-kms`),
  and adds **no** new crate to `Cargo.lock` (only the cli→rustix edge). The
  `webpki-roots` `CDLA-Permissive-2.0` edge under `--features heartbeat` is
  pre-existing for every `reqwest` feature (e.g. `devices-net`) and is never in CI's
  scanned default graph.
