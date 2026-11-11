# Private authority scenario runner

This tool is an experimental HTTPS authority + CPU decoder observer. It does not enable paid delivery or production routes. Its media transport is the local MoQ model plus IPv6 TCP, not QUIC or a device renderer.

```
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo run --locked \
  --manifest-path tools/native-exchange-runtime/Cargo.toml -- \
  --bootstrap /PRIVATE/manifest.json --marker /PRIVATE/engine-bookings.json \
  --expected /PRIVATE/expected-fresh.json --scenario release
```

Use `withdraw` or `exhaustion` for the other scenarios. Each invocation requires a fresh manifest/session and fresh output paths. Do not launch against the remote harness before captain review, the private 0600 manifest, and the approved SSH forward.

The process generates real HEVC and five AAC streams with FFmpeg. It acquires its own publisher lease, books two receivers, mints the mapping, writes the marker, and waits at most 120 seconds for the independent expected fragment. The reusable bootstrap renews during this handoff and extends the bookings. Credentials and roots are not logged.

The native phase reads the actual current-root DTO, checks its crypto IDs, creates `Publisher::from_authority` with a monotonic lease fence, and accepts signed grants against all six independently supplied raw IDs. Each receiver persists replay and sticky denial state. CPU decoder timestamps produce intervals, which pass through `record_decoded` and `ReceiptSpool::record_and_queue` before HTTP. Only the matching sequence acknowledgement removes an exact queued receipt. The engine does not debit a balance.

Release and exhaustion deny receiver A while receiver B continues. Story withdrawal denies both receivers. `Flush` clears bounded encoded, decoded, presentation and audio observer queues; this is not proof of device callbacks.

## Funded expiry (`credit-cutoff`)

The optional `scenario.fixtureProfile.name` is `funded-expiry`. Missing profile or `standard` preserves the existing request bodies and extension cadence. Funded defaults: rate 600000 micro/minute, A duration 10000ms, extra 5000ms, extension due 3000ms after booking, status 409, cause `insufficient_credit`, detail containing `prepaid exchange balance cannot cover`. B always books 60000ms and retains its normal 201 extension worker. Optional wallet metadata is decimal text, not a JavaScript number; no credentials belong in this block.

Focused checks:

```sh
cargo test --locked --manifest-path tools/media-crypto/Cargo.toml --test harness_bootstrap
cargo test --locked --manifest-path tools/native-exchange-runtime/Cargo.toml --test authority_scenario credit_cutoff_uses_early_credit_refusal_and_paid_boundary -- --test-threads=1
```

Connected command (controlled harness only):

```sh
cargo run --locked --manifest-path tools/native-exchange-runtime/Cargo.toml -- --bootstrap /PRIVATE/manifest.json --marker /PRIVATE/engine-bookings.json --expected /PRIVATE/expected-fresh.json --scenario credit-cutoff
```

The paid boundary is the booking 201 row's `expires_at` (snake_case JSON integer, Unix ms; `created_at + durationMs` is checked). If the expected-fresh fragment arrives so late that booking A's paid window has (nearly) ended, the run fails with an explicit message instead of fabricating media. On success it prints one measured, non-secret line: A's `expires_at`, the 409 refusal instant, the observed cutoff instant and accepted receipt counts.

A's one-shot extension replaces its normal worker, including during the expected-fragment handoff. Only a timely 409 problem body with the credit-specific detail succeeds; expired or wrong-cause 409 fails closed. The engine never checks D1: the web captain independently verifies the unchanged expiry/reservation row. Grant expiry independently gates decode callbacks and the watchdog flushes queues. Actual decoded timestamps supply receipts; no future media is invented. At the paid boundary the runner requires the existing grant route to return 404, denies A durably, and proves B continues. It does not require `reservation_exhausted`. Finalize is intentionally omitted: the captain may verify the released settle/finalize path after the **real** expiry + 15000ms deadline, including replay refund zero. No endpoint, query parameter, or stop reason is added.

## Local evidence

`cargo test --manifest-path tools/native-exchange-runtime/Cargo.toml --test authority_scenario -- --test-threads=1` invokes the complete binary against a synthetic authority over real TLS in all three scenarios. It does not merely parse arguments. The mock independently supplies identities and signs binary MG02 grants. Native all-target clippy passes with `-D warnings`.

## Review concerns / not yet live-ready

- No remote harness call has occurred. The captain must confirm the root/receipt contract and supply private files and forwarding.
- The contributor org raw identity comes from the authenticated root DTO, not from the expected fragment (which has no contributor field). All six MG02 identities are independently pinned.
- A bounded native coordinator uses one dedicated lease worker, two booking-extension workers, two grant workers, and one watchdog. Renewal starts on a one-second monotonic cadence; the transport timeout is three seconds. Each active booking extends by 20 seconds every 20 seconds. A lease or extension failure closes the shared fence without reacquisition.
- The 10ms watchdog flushes receiver queues without a decoder callback. Decode and receipt HTTP do not hold receiver locks. Ordinary grant expiry suspends presentation; only a verified current grant resumes it. Authority denial remains durable and sticky. These changed expiry semantics require fresh security review.
- Local tests use a 35-second delayed native FFmpeg invocation to observe actual TLS renewals within five seconds, booking extension, and denial flush before decoder completion. The delayed command must fail closed on its stale result; this is not a successful long-running media-delivery claim.
- The local exhaustion mock supplies `reservation_exhausted` after actual decode evidence. It does not prove the remote fixture's real funding/rate threshold.
- Near-boundary grant retry is bounded but needs adversarial epoch-transition review. No live readiness is claimed solely from compilation or these local tests.
