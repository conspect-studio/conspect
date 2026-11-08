# MoQ integration checkpoint — partial, not release-ready

Prepared for captain review; no push or deployment authorized by this report.

## Checkpoint identity

- Inspected aggregate HEAD: `85b90b06`.
- Central lease/API checkpoint: `f5e9ff09`.
- Cloudflare service startup and provider injection: `aaa73318`.
- HTTP scope and generation tests: `56237aa3`.
- Private master-key open revalidation: `9c37d559`.
- Optional transport dependency registration: `2a4351a4`.
- Latest generated schema and route inventory: `9ef272a0`.
- Trusted listener startup constructor and missing-factory regression: `d9cad6f1`.

These are incremental checks, not a claim that all checks ran on aggregate HEAD.
Uncommitted Cloudflare/provider/fixture changes observed at preparation time are excluded.
Task t5's failed full-scope acceptance remains unchanged.

## Implemented central behavior

Scoped program/input/output POST routes enforce write permission and object authorization before admission. Registry revocation additionally checks principal key ownership, scope, and exact generation. A successful revoke acknowledges the synchronous enqueue fence, not completion of QUIC teardown.

The bounded registry enforces a finite 1–60 second server TTL and bounded tombstones. Transport providers must independently enforce TTL and supply nonblocking admission/fence callbacks. The default provider remains unavailable. Missing production factory returns typed unavailable before listener bind or certificate loading.

Cloudflare startup is explicit: `MULTIVIEW_CLOUDFLARE_STATE_DIR` and `MULTIVIEW_CLOUDFLARE_MASTER_KEY_FILE`. The latter references a private regular file of exactly 32 raw bytes; it is not a raw key environment value. The service injects the durable provider and owns a reconciler abort guard. This wiring is not proof of complete external Cloudflare lifecycle behavior.

## Verification evidence

| Command | Observed result |
| --- | --- |
| `cargo check -p multiview-control -j2` | Passed |
| `cargo clippy -p multiview-control --lib -j2 -- -D warnings` | Passed after owner lint repairs |
| `cargo test -p multiview-control --lib moq::tests -j2` | 5 passed: registry ownership/scope/generation, expiry, capacity, default unavailable, route authorization/query validation |
| `cargo test -p multiview-control --test openapi --test preview_capabilities --test moq_routes -j2` | 15 passed: 10 OpenAPI, 3 capabilities, 2 actual HTTP authorization tests |
| `cargo test -p multiview-control --lib cloudflare_management::tests -j2` | Durable policy/owner-bound reference test passed |
| `cargo check -p multiview-cli -j2` | Passed with real Cloudflare startup wiring |
| `cargo test -p multiview-cli --lib bind_and_serve_exposes_openapi_then_shuts_down -j2` | 1 real listener startup/shutdown test passed |
| `cargo check -p multiview-control --features moq-transport -j2` | Passed; later runtime revision emitted an upstream-toolchain atomic deprecation warning, assigned to runtime owner |
| `cargo test -p multiview-control --features moq-transport --lib moq_startup::tests -j2` | Missing factory test passed without loading nonexistent key files |
| `cargo run -p xtask -j2 -- gen-openapi && npm --prefix web run generate:api` | Passed; generated artifacts committed |

No redundant workspace-wide suite was run by this integration task. CI, independent review and deployment acceptance remain separate gates.

## Schema consistency inspection

The canonical Rust Cloudflare request and generated TypeScript both include `mode`. Both credential-save and action endpoints are registered in the router and OpenAPI. MoQ DELETE operations include the required unsigned `generation` query. Creation and revoke schemas include typed problem responses. The Cloudflare action description is informational and not an exhaustive enum; new action implementations do not require a fabricated endpoint or hand-edited generated client.

## Release-blocking acceptance

1. No production `AuthorizedFeedFactory` connects effective shared renditions, binding generation and source-demand ownership to the transport. No duplicate pipeline or per-viewer encoder was introduced to hide this absence.
2. Actual integrated **production** media is unverified. Browser fixture decode evidence from media/browser owners is useful interoperability evidence, not production integration acceptance.
3. Runtime retained-memory accounting and eventual release remain under review. A fail-closed lifetime allowance is not sustained streaming capacity; do not advertise the runtime as ready on that basis.
4. Connected-session cutoff, cached/in-flight media bounds, quarantine, and per-peer source-demand release need end-to-end proof with the production binding owner.
5. Cloudflare independent review identified response/pagination and automatic-recovery concerns. Owner fixes and live evidence must be reviewed separately; central route compilation does not close those findings.

## Proposed PR summary

Introduce authenticated finite MoQ control leases, generated browser/relay APIs, safe unavailable defaults, optional trusted transport startup, and secure Cloudflare management service wiring. Preserve the production output path and keep MoQ disabled without an authorized real factory.

**Do not merge or deploy as complete MoQ shipping.** Attach factory/retention fixes and their independent acceptance evidence before converting this checkpoint into a release claim.
