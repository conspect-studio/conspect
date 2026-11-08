# Conspect patch record

Vendored base: the published rustls-platform-verifier 0.7.1 crates.io archive,
SHA256 `1167586491e2b18b8bfbb293e8180ec17c201c4f076d7cb3070ca964e7598f98`.
Its `.cargo_vcs_info.json` identifies upstream commit
`252e25161a91af476cbea620e29d277592d27ec7`, subdirectory
`rustls-platform-verifier`. This published archive is the source baseline for
byte comparisons, not a later upstream checkout.

Separately, later upstream HEAD `6e4c1c4fbb9ec5355fcf505995724d6aa9ebf84b`
was inspected only to research whether an upstream fix existed; it still used
the DER bundle. It is **not** the vendored base.

Upstream LICENSE-APACHE and LICENSE-MIT are copied unchanged. No legal policy changes.
The workspace `[patch.crates-io]` selects this path dependency on all targets.
The path entry in Cargo.lock has no registry checksum; the archive hash and
published commit above explicitly record its provenance.

Only runtime change: wasm32 root-store construction uses webpki-roots =1.0.11
trust anchors instead of reparsing webpki-root-certs DER certificates. Native
platform verification code is byte-identical. The existing repository license
exception already covers webpki-roots, not the separate DER distribution crate.
No TLS validation disabling or advisory/license suppression was added.

**Not byte-identical trust policy:** both 1.0.11 bundles contain 118 identical
subject/SPKI root identities. The anchor bundle adds exactly one Mozilla `.tr`
name constraint missing from the DER certificate. This stricter policy is kept;
it can reject a wasm TLS chain outside that constraint previously accepted by
the DER path. No root identities are added and no constraints are removed.
`tools/verify-moq-roots` checks these exact conditions in both directions against
both pinned upstream distributions. This binary compares bundle identities and
constraint data only: it does **not** exercise the wasm verifier or establish
certificate-chain acceptance/rejection behavior. It is offline development
verification, not a product dependency. No live out-of-scope certificate-chain
fixture was available.

Maintenance: rebase this minimal patch on verifier updates; re-run the comparison
when updating either root bundle. Prefer removing this vendor override when an
upstream release exposes equivalent trust-anchor support. Do not strip the added
constraint to force equivalence. The upstream development-test DER dependency is
preserved; it is not part of the product closure.

Verification: cargo-deny passes; targeted multiview-moq check, 11 tests and strict
Clippy pass. Installed targets do not include wasm32, so wasm target compilation
has not been claimed. No third full workspace suite was run; CI must test the
new lockfile. Native TLS source unchanged; runtime handshake evidence is separate.
