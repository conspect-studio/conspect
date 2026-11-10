# Curated engine runtime prerequisite bundle

## Scope and status

Base: `origin/main` **7f7d38ed**, sampled 2026-11-09. This is a curated prerequisite commit, not a merge of the release branch and not a production release claim. Cross-vendor review, mixed/live acceptance execution, full CI, and integration remain outstanding. No deploy, root checkout edits, HTTP factory, optional preview owner, or MoQ transport overlay belongs to this commit.

The runtime owner must layer its owned MoQ factory over the actual Pipeline subscription slot. It must retain b41324ae's permanent single optional publisher claim when it adds optional registration. This bundle intentionally does not introduce that registration subsystem.

## Source and review mapping

These are source-history references, not a claim that this new main adaptation has independently passed review:

- Canonical `release/takeover-integration` **344dcd51** supplies the output batch, binding, rendition workers, dispatch, sink and rational audio/video window code. Its **12ea6569** root includes no-falter admission/frame substitution and the regression. **344dcd51** supplies detached stream-parameter snapshots.
- Carrier/recovery: **eeed4cab**, **c3ed099d**, **6a9d159a**; typed watermark and bounded binding: **07ecebcc**, **1bcaaabb**; independent video/shared audio and actual runtime: **bc45e7c1**, **4fdcbd47**, **8951b08e**.
- Shared payload/ownership and opened metadata: **9016dd5c**, **9aadb4a4**, **982e1ceb**; exact rational windows/AAC priming: **df1eb700**; selected/opened audio truth: **7122097d**, **5bc9c235**, **ec3c9ecd**.
- Fatal/no-falter: **9f4fb6cd**, **bca09a0b**, **4a15a80b**, **5874a1bd**, **12ea6569**. MPEG-2 ordered-delay handling: **b85bb79b**.
- Actual registry: **51633f72**, **8000bcb4**, **ccc3ffab** supplies actual Pipeline publication, final runner identities, immutable effective catalogue, per-tick quiet-tail pump, cadence separate from timestamp scale, and live subscription tests.
- **b41324ae** contributes ONLY actual opened audio bitrate metadata plus its regression. Optional ingress, owner lifecycle, and HTTP factory changes are excluded.

## Main API conflicts and resolution

The canonical Pipeline contains demand rails, dynamic decode sizing, weather/graphics, event journal, IPTV request headers and YouTube changes absent from main. Only output-path hunks were applied to main. Existing main source lifecycle, decode, preview, configuration schema, control, MoQ and Cloudflare code remains intact. Appended RC-only HTTP-header/display-follow tests were not imported; all pre-existing main tests remain.

The ccc3 registry expects `WindowDelivery::pump`; import that exact default method with the producer, not the old delivery trait alone. Preserve final runner order when an internal HLS anchor is prepended. Match explicit audio declarations against the encoder that actually opened, not requested config.

Main's codec fallback now fails closed for an unavailable/unknown requested codec. The actual runtime opens separate encoders per concrete codec and shares compatible coded payload. This is not a claim of arbitrary per-output geometry/rate control support: the config plan and runtime still have different expressive limits.

The resilient sink uses a coupled, opt-in `multiview-srt-sys` leaf and deadline-aware safe mux wrappers. Add its dependency/feature only to the output crate; do not enable it in CLI/platform presets. Preserve main's `rustls-platform-verifier` patch and all existing lock versions. Lock changes add only this local crate and edge.

HLS sink continuity, strict decoder-ready RAP recovery, mux lease acknowledgement, and bounded audio rendition retention travel with the sink. They are coupled to the replaced packet-delivery path. Unrelated production ingest/audio/HLS UI fixes were not copied.

## Security and invariants

- Registry metadata comes from opened codec snapshots, actual cadence and real dependency axes. Unknown audio bitrate remains absent.
- Attachments validate run generation and explicit dependencies. The downstream MoQ owner must apply per-resource authorization and identity/revocation fences; this bundle does not authorize HTTP requests.
- Encoded packet references share immutable payload and independent headers. Codec-aware AVC/HEVC framing fails closed for malformed configuration.
- Demux parameters deep-copy ownership before the input interrupt state can die.
- Link status uses redacted targets; no raw credentials enter its status URL. Native FFI remains inside safe wrapper crates with existing safety comments.
- Every live tick has bounded admission; strict-RAP gates handle supported coded recovery. Fatal consumer errors stop at a tick boundary and retain their exact cause.
- No default native/GPU/GPL feature was enabled. No main feature or test was deleted or disabled.

## Verification so far

All commands use `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.

- `cargo check --workspace`: PASS.
- `cargo check -p multiview-cli --features ffmpeg`: PASS.
- `cargo check -p multiview-cli --features ffmpeg --tests`: PASS.
- `cargo test -p multiview-cli --features ffmpeg --lib window_subscription`: 2 PASS, including actual Pipeline delivery and internal anchor order.
- `cargo test --locked -p multiview-cli --features ffmpeg --lib output_window_registry`: 10 PASS before the additional isolated audio bitrate test.
- `cargo test --locked -p multiview-cli --features ffmpeg --lib egress_fatal_tests`: 5 PASS.
- `cargo test --locked -p multiview-cli --features ffmpeg --lib audio_bitrate_is_only`: 1 PASS.
- `cargo fmt --all -- --check` and `git diff --check`: PASS before final documentation/staging.

Build monitor stops at target >6 GiB or free capacity <360 GiB. Latest target was 1.6 GiB, free capacity 384 GiB. No other lane artifacts were removed. Mixed/live test sources are included and compile, but their native/GPL/network executions and full CI remain later bounded gates.

## Exact code inventory (35 paths)

```text
Cargo.lock
Cargo.toml
crates/multiview-cli/src/binding_delivery.rs
crates/multiview-cli/src/control.rs
crates/multiview-cli/src/lib.rs
crates/multiview-cli/src/output_window_registry.rs
crates/multiview-cli/src/pipeline.rs
crates/multiview-cli/src/rendition_dispatch.rs
crates/multiview-cli/src/rendition_plan.rs
crates/multiview-cli/src/rendition_workers.rs
crates/multiview-cli/tests/audio_binding_truth.rs
crates/multiview-cli/tests/broadcast_live_isolation.rs
crates/multiview-cli/tests/broadcast_mixed_output.rs
crates/multiview-cli/tests/common/broadcast_fixture.rs
crates/multiview-cli/tests/rendition_plan.rs
crates/multiview-cli/tests/streaming_encode.rs
crates/multiview-config/src/lib.rs
crates/multiview-config/src/rendition.rs
crates/multiview-config/tests/rendition_plan.rs
crates/multiview-core/src/lib.rs
crates/multiview-core/src/redact.rs
crates/multiview-ffmpeg/src/demux.rs
crates/multiview-ffmpeg/src/encode.rs
crates/multiview-ffmpeg/src/lib.rs
crates/multiview-ffmpeg/src/mux.rs
crates/multiview-ffmpeg/src/packet.rs
crates/multiview-output/Cargo.toml
crates/multiview-output/src/batch.rs
crates/multiview-output/src/hls/live.rs
crates/multiview-output/src/lib.rs
crates/multiview-output/src/push_link.rs
crates/multiview-output/src/sink.rs
crates/multiview-output/src/sink/push.rs
crates/multiview-srt-sys/Cargo.toml
crates/multiview-srt-sys/src/lib.rs
```

This document is the 36th path. No release docs, legacy control branch, IPTV/weather implementation, legal content, deployment config or main MoQ/Cloudflare file is part of the inventory.
