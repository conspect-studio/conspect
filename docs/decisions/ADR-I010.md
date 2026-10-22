# ADR-I010: Migrate ed25519-dalek 2 → 3 on the Conspect signature path (MSRV 1.82 → 1.85)

- **Status:** Accepted
- **Area:** Implementation build-out
- **Date:** 2026-07-10
- **Source:** operator (backlog task #11) — unblocks Dependabot PR #227 (a cargo group bump that
  partially bumped `ed25519-dalek` to 3 in `[dependencies]` but left some `[dev-dependencies]` at 2,
  producing `E0464: multiple candidates for rmeta dependency ed25519_dalek`). Landed as its OWN clean,
  reviewed PR rather than on #227's branch, because it is a security-sensitive crypto migration.

## Context

`ed25519-dalek` is the Ed25519 **signature-verification** primitive for the Conspect entitlement
plane: the signed-lease verifier (`multiview-licence::verify` + `heartbeat`), the mesh announce
verifier (`multiview-mesh::announce`), the control licence-route tests, and the cli device-key
identity (`multiview-cli`). Four crates reference it (deps AND dev-deps):
`multiview-cli`, `multiview-control`, `multiview-licence`, `multiview-mesh`.

A partial 2→3 bump (deps at 3, dev-deps at 2) makes the licence lib-test see **two** `ed25519_dalek`
rmetas → `E0464`. The fix must bump **every** occurrence — deps, dev-deps, and the umbrella feature
plumbing — in lockstep.

`ed25519-dalek` 3.0 aligns its closure to **`signature` 3.0 / `ed25519` 3.0 / `curve25519-dalek` 5.0**
and moves to **`rand_core` 0.10 / `getrandom` 0.4**. It is **edition 2024** with **MSRV 1.85**
(so is `curve25519-dalek` 5.0). Both are **default** dependencies of `multiview-licence` and
`multiview-mesh`, so the whole default build now requires **rustc ≥ 1.85**.

Binding constraints:

- **`multiview-licence` / `multiview-mesh` are VERIFICATION-ONLY** ([their `CLAUDE.md`](../../crates/multiview-licence/CLAUDE.md)):
  no key generation, no RNG, no I/O in non-test code; the RNG lives in dev-deps for test keypairs only.
- **Verification must remain byte-identical** — Ed25519 is a standard, format-stable scheme, but the
  property/verification tests must PROVE it: same accept/reject for the same `(key, msg, sig)`, and a
  malformed/short key or signature must still **fail closed** (a typed error, never a new panic; rule 17,
  safety §7).
- **Default build stays LGPL-clean + `cargo deny`-clean** (conventions §7): the ed25519-dalek 3 closure
  must stay MIT/BSD/Apache.
- **No panic on any non-test path** (rule 17) — the one production keygen must fail closed.

## Decision

Bump `ed25519-dalek` from `"2"` to `"3"` in **all** four crates (deps AND dev-deps AND the cli
`heartbeat` feature list), regenerate `Cargo.lock` (only `ed25519-dalek 3.0.0` remains), and adapt the
three API deltas the bump forces. **Verification code is unchanged** — the method signatures
`VerifyingKey::from_bytes` (fallible `Result`), `Signature::from_bytes(&[u8; 64])` (infallible),
`verify_strict`, the `Verifier` trait, and `Signature::BYTE_SIZE` are all identical across 2→3.

### 1. Production device keygen — `getrandom::fill` + `from_bytes` (fail-closed)

`rand_core` 0.10 **removed `OsRng`**. The single non-test keygen — the first-boot device key in
`multiview-cli/src/licence.rs` — no longer uses `SigningKey::generate(&mut rand_core::OsRng)`. It draws
the 32 seed bytes straight from the OS CSPRNG via `getrandom::fill`, mapping an entropy error to the
existing `HeartbeatError::Transport` with `?`, then `SigningKey::from_bytes(&seed)`:

```rust
let mut seed = [0u8; 32];
getrandom::fill(&mut seed).map_err(|e| {
    HeartbeatError::Transport(format!(
        "failed to draw OS entropy for a fresh device key: {e}"
    ))
})?;
let key = ed25519_dalek::SigningKey::from_bytes(&seed);
```

This is strictly **fail-closed**: an OS entropy failure keeps last-good and never mints a low-entropy
identity, with **no `unwrap`/`expect`/`panic`** — an improvement over the v2 `OsRng` path, whose
`fill_bytes` panics inside rand_core on entropy failure. `SigningKey::from_bytes` over a uniformly
random seed produces exactly the key `generate` would from the same RNG bytes. The cli's `ed25519-dalek`
drops the now-unneeded `rand_core` feature; `getrandom` (deny-clean, already in the graph) replaces
`rand_core` in the cli `[dependencies]` and the `heartbeat` feature list.

### 2. Test keygen — `UnwrapErr(getrandom::SysRng)`

Test dev-deps swap `rand_core = "0.6"` for `getrandom = { version = "0.4", features = ["sys_rng"] }`
and keep the ed25519-dalek `rand_core` feature (which re-exports the exact `rand_core` 0.10
`SigningKey::generate` binds to). `let mut rng = OsRng` / `SigningKey::generate(&mut OsRng)` become
`UnwrapErr(SysRng)` (`getrandom::SysRng` is a `TryCryptoRng`; `rand_core::UnwrapErr` forwards it to
`CryptoRng`, panicking on error — acceptable in tests). This keeps the tests producing **OS-random,
distinct** keypairs (e.g. `verify::wrong_key_is_rejected` needs two distinct keys), i.e. no test
semantics change.

### 3. `signature` 2.2 vs 3.0 trait split — p256 stays on 2.2

`ed25519-dalek` 3 uses `signature` 3.0 while `p256` 0.13 (the Conspect ECDSA-P256 root verifier) stays
on `signature` 2.2.0. Their `Signer`/`Verifier` traits are now **distinct types**, so a single
`use ed25519_dalek::Signer` no longer covers a p256 key. The licence `fake` test signer
(`tests/fake/mod.rs`), which signs a fabricated attested keyset with a p256 root, now imports
`p256::ecdsa::signature::Signer` explicitly for the p256 `.sign()` alongside the ed25519 `Signer` for
the ed25519 intermediate. Each `.sign()` resolves by receiver type (ed25519 vs p256 key) — no
ambiguity. **No version unification is attempted**: p256 and ed25519-dalek legitimately carry different
`signature`/`der`/`sha2` versions (`multiple-versions = "warn"`, not a deny failure); forcing them onto
one `signature` version would require downgrading ed25519-dalek or a p256 major bump — out of scope and
unnecessary.

### 4. MSRV 1.82 → 1.85

`ed25519-dalek` 3.0 + `curve25519-dalek` 5.0 are edition-2024 (rust-version 1.85) and are default deps,
so the workspace's true minimum toolchain rises to **1.85**. The load-bearing declarations are updated
to match reality (rule 27): `Cargo.toml` `rust-version = "1.85"`, [`AGENTS.md`](../../AGENTS.md) rule 39
+ §A, and [`docs/stack.md`](../stack.md). Multiview's own crates stay **edition 2021** (editions are
per-crate; an edition-2021 crate may depend on an edition-2024 crate). `rust-toolchain.toml` already
pins `channel = "stable"` (floating; currently 1.97), so CI builds unaffected — this is a
declaration/reality alignment, not a toolchain change.

## Rationale

- **Verification is byte-identical + proven.** The 2→3 method signatures on the verify path are
  unchanged, and the existing Ed25519 verification + property tests
  (`multiview-licence/tests/verify.rs` valid/tampered/wrong-key/malformed, `store.rs` tamper-rejection,
  `multiview-mesh/tests/announce_payload.rs` + `relay_carrier.rs` spoof/tamper rejection) pass
  **unchanged** — the required-by-rule-25 proof that behaviour is preserved.
- **The keygen change is a fail-closed improvement.** `getrandom::fill` + `?` converts a latent
  entropy-failure panic (v2 `OsRng`) into a typed, last-good-preserving error — better aligned with the
  crate's never-off-air charter, with zero `unwrap`/`expect`/`panic`.
- **deny-clean.** The whole ed25519-dalek 3 closure resolves to allowlisted licences —
  `curve25519-dalek` 5.0 / `ed25519-dalek` 3.0 / `subtle` are BSD-3-Clause (on the allowlist),
  `ed25519`/`signature`/`der`/`pkcs8`/`spki` are Apache-2.0 OR MIT, `getrandom` 0.4 / `rand_core` 0.10
  are MIT OR Apache-2.0. `cargo deny check` → advisories/bans/licenses/sources all **ok**.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Keep `ed25519-dalek` at v2 (Dependabot-ignore its major); land only #227's other bumps | Defers the inevitable and leaves the security-critical Ed25519 crate on the old `curve25519-dalek` 4 stack, diverging from the RustCrypto ecosystem the rest of the graph tracks. The operator explicitly scheduled this as its own deliberate migration; v3 is the maintained line. |
| Use `SigningKey::generate(&mut UnwrapErr(SysRng))` for the **production** keygen (mirror the tests) | `UnwrapErr` panics on an OS entropy failure — a non-test panic (rule 17) that also violates fail-closed. `getrandom::fill` + `?` fails closed with no panic and produces the identical key. |
| Fixed-seed keys in tests (drop OS randomness) | Loses the random-distinct-key property some tests rely on (`wrong_key_is_rejected` generates two keys sequentially and asserts they differ) — a semantic weakening of the tests (rule 19). `UnwrapErr(SysRng)` keeps them OS-random. |
| Unify `signature`/`der`/`sha2` to one version across p256 + ed25519-dalek | Would force a p256 major bump or an ed25519-dalek downgrade. The duplicate versions are a benign `multiple-versions = "warn"`; unification is unnecessary and out of scope. |
| Leave `rust-version = "1.82"` declared | Inaccurate (rule 27): the edition-2024 deps make 1.85 the true minimum. The manifest + governance docs must state the real floor. |

## Consequences

- **The Conspect signature path is on the maintained ed25519-dalek 3 / curve25519-dalek 5 line**, with
  verification behaviour unchanged (same accept/reject, same fail-closed on malformed/short input). This
  unblocks Dependabot #227 (the E0464 collision is gone once #227 rebases onto this).
- **MSRV rises 1.82 → 1.85** (operator-set value in rule 39 — surfaced for review). CI is unaffected
  (`channel = "stable"`), but anyone building on a pinned < 1.85 toolchain must upgrade. Two tangential
  `1.82` mentions are intentionally **not** rewritten: [`ADR-0048`](ADR-0048.md) (an immutable
  historical record) and [`docs/research/webrtc.md`](../research/webrtc.md) (a research brief's passing
  note) — the load-bearing MSRV declaration (Cargo.toml + AGENTS.md + stack.md) is authoritative.
- **New duplicate crate versions coexist** (`signature` 2.2 + 3.0, `der` 0.7 + 0.8, `sha2` 0.10 + 0.11,
  `digest`, `crypto-common`, `cpufeatures`, `getrandom` 0.2/0.3/0.4, `rand_core` 0.6/0.9/0.10) because
  `p256` 0.13 stays on the old RustCrypto stack while ed25519-dalek 3 moved to the new one. This is
  `multiple-versions = "warn"` — a slightly larger dependency closure and compile surface, not a deny
  failure. It resolves naturally when `p256` next majors onto `signature` 3.
- **`multiview-licence` / `multiview-mesh` stay verification-only** (no RNG/I-O added to non-test code;
  the cli keeps the only RNG at its boundary, now `getrandom::fill`). The default `cargo check` /
  `cargo deny` shell stays pure-Rust, network-free, LGPL-clean.
- **Invariant #1 (never off air) is honoured**: the keygen fail-closes on entropy error (keep last-good,
  no panic); the verify path fail-closes on malformed input exactly as before; no engine handle is
  touched.

## Biggest residual risk

**The MSRV bump is the material consequence, not the crypto.** The Ed25519 verification is standard and
format-stable, and the full property/verification suite passes unchanged — so a signature-behaviour
regression is very unlikely (and would be caught by the committed tests). The real decision is the
operator-set MSRV: 1.82 → 1.85 is forced by the edition-2024 deps and cannot be avoided while taking
ed25519-dalek 3. It is low-impact in practice (CI pins floating stable), but it is a policy value the
operator owns — if 1.82 support is required, the only remedy is the rejected "stay on v2" alternative
(revert this PR). Surfaced explicitly in the PR body for the operator's accept/veto.
