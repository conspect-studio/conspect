# MoQ aggregate integration status

This branch is unfinished. It is not a production deployment or a full ABR delivery claim.

## Source boundaries

- Curated main-based engine prerequisites: `3f8e2c0e`, not the unrelated 442-path reconciliation history.
- Actual emitted H.264 SPS/PPS metadata, Annex-B-to-AVC repacketization, canonical registry lifecycle fences, typed recoverable ladder overflow, and actual control HTTP lease authority are integrated.
- Browser final source derives from reviewed `f529c3ee`; sanitized proof source derives from main-based `dcaa2c1c`.
- Native publisher delivery feedback uses explicit patched pinned transport sources. Viewer sessions do not supply publisher capacity evidence.
- Outgoing family activation is separate from viewer leases. It resolves an existing configured API key identity, authorizes the family, uses a private relay-URL file, and stops on authorization/configuration withdrawal. Protected activation is unavailable and must not fall back to cleartext.

## Focused evidence

- Historical coherent native H.264 Pipeline -> actual HTTP POST 201 / DELETE 200: passed (`e52fb16b`).
- Clean aggregate native compile, including vendored transport, family owner and initial caller: passed before the final handshake cancellation changes.
- Existing service principal/scoped-output/revocation policy test: passed.
- Browser ABR/player focused tests: 45 passed. Protocol instrumentation: 3 passed. Scoped MoQ browser lint passed.
- Native TLS descriptor pinning, malformed framing termination, copied-buffer budget lifetime and scoped relay terminal findings have focused evidence and scoped independent reviews. These are not whole-feature approvals.
- Full reachable-history secret scan at `d45fc21e`: 271 commits, 41.59 MB, no leaks. Re-run after final source changes before push.

## CI required; not run locally after cache cap

Local target reached 5.4 GiB. No further native builds or network proofs are authorized in that lane until CI or an explicit cache decision.

Run these focused commands on the final aggregate:

```sh
cargo check --locked --workspace
cargo check --locked -p multiview-cli --features moq-output,gpl-codecs
cargo test --locked -p multiview-cli --features moq-output --lib outgoing_
cargo test --locked -p multiview-cli --features moq-output --lib pending_outgoing_handshake
cargo test --locked -p multiview-cli --features moq-output --lib relay_url_file
cargo test --locked -p multiview-cli --features moq-output,gpl-codecs --lib actual_h264_pipeline_http
cargo test --locked -p multiview-moq --lib
cargo test --locked -p multiview-control --features moq-transport --example abr_authority
npm --prefix web ci
npm --prefix web run test -- src/preview/moqAbr.test.ts src/preview/MoqPlayerAbr.test.tsx
npm --prefix web run lint
npm --prefix web run build
```

The final handshake stop/ACL/configuration-generation cancellation, secure URL-file and awaited shutdown tests are source-only and remain UNRUN until CI.

## Known gaps

- Optional A/V preview startup overlay and its actual application activation are pending final integration.
- Direct protected Cloudflare access is unavailable. A relay-wide token is not a per-lease authorization grant.
- Historical Cloudflare QoS evidence failed; no final Cloudflare or real production-caller network acceptance is claimed.
- Protected publication cryptography has unresolved security findings and is not integrated.
- Distinct optional cadence and HEVC metadata are not supported by the admitted implementation.
- No aggregate CI-green, whole-feature adversarial pass, merge, host deployment, or full completion is claimed.
