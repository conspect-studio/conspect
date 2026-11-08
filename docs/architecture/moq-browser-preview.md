# MoQ-first browser preview — implementation design

Status: proposed; not production-wired. This document describes the acceptance contract, not existing support.

## Goal and ownership

A browser uses Media-over-QUIC (MoQ) when the deployment, browser, and selected media format support the same protocol. WebRTC is the fallback only when MoQ is unavailable. WebTransport alone does not establish MoQ or codec support.

This change owns the MoQ adapter, authenticated control API, browser player, and transport selection. The concurrent broadcast-output redesign owns the production shared-rendition subscription and pipeline wiring. No additional decoder, compositor, or video encoder is created per compatible browser viewer.

## Safety classification

R3: a new media subscriber must preserve output-clock and isolation invariants. Production output dimensions and rational cadence remain unchanged. The producer never awaits a browser, network stream, admission request, or teardown. The implementation requires bounded-consumer chaos tests before integration.

## Transport selection

1. Read authenticated per-scope server capabilities.
2. Check secure-context WebTransport and the player APIs, then probe the advertised video and requested audio configurations with WebCodecs.
3. If the protocol or required browser capabilities are absent, select WebRTC without opening a MoQ lease.
4. Otherwise acquire one authorized MoQ lease and connect to its exact authorized broadcast.
5. On a typed transport-unavailable result, fully release that lease before a WebRTC attempt.
6. Authorization, capacity, invalid configuration, and unsupported rendition errors remain visible. They never trigger a concealed transport retry.
7. An established stream failure is visible and permits an explicit retry. It does not create an unbounded automatic reconnection loop.

The existing still-image path remains available when no live transport is available. A still image is never labelled live video.

Expose requested transport policy separately from effective transport (`moq-webtransport`, `webrtc`, or `jpeg`). Preserve the typed availability/fallback reason, connection lifecycle, and effective binding generation for UI diagnostics. Never report an intended MoQ connection as effective before negotiation and media readiness. Redact credentials, relay query strings, and unauthorized dependency identifiers from diagnostics. The UI design lane requires this distinction for future transport/fallback indicators.

## Proposed shared-rendition seam

The output redesign will finalize concrete Rust types. Preserve public `program`/`input`/`output` scopes: input acquires a source-demand lease, program retains the legacy main-program mapping, and output resolves its effective mux binding to the actual rendition/bus, never a global-program assumption or pending desired configuration. Carry bus identity internally; a public bus scope waits for switcher runtime support. Authorize both the requested public object and its resolved dependencies without disclosing unauthorized identifiers.

The shipping owners agreed the following ownership split (concrete runtime types remain pending): the binding registry keyed by `(program, bus, output, generation)` owns the stop-enqueue revocation fence and source-demand release; the batch queue owns in-flight leases and reserved count/bytes until actual drop; the MoQ adapter owns QUIC shutdown and bounded quarantine. Every peer additionally has a distinct subscription ID: revoking one viewer must not invalidate the shared rendition or another viewer on the same output.

The transport contract requires:

- An admitted, principal-authorized scope and an immutable binding identity plus generation.
- Video codec/profile, initialization bytes, color metadata, dimensions, rational cadence, and rational timestamp timebase.
- Independent audio track identities and codec/configuration, sample rate, channel layout, and timebase.
- Shared immutable encoded payloads with PTS, DTS, duration, verified random-access status, and configuration generation.
- Admission operates on an immutable encoded-time batch/window, not a sequence of independent packet offers that can accept half an audio/video window. `try_offer(window, now)` either retains the entire validated window or rejects it with an explicit discontinuity. Window validation and capacity accounting precede publication. Count, byte, and age limits apply to both the window and the complete subscriber backlog.
- Transport-side demultiplexing may use separate bounded video/audio receivers only with explicit per-track gap/configuration generations and common binding readiness. A per-track packet acceptance is not atomic audio/video acceptance. Recovery must coordinate the binding epoch; video waits for matching configuration and verified RAP, while audio uses the same epoch and declared timestamp alignment.
- Retained payload accounting includes shared `Arc` allocations: sharing prevents copies but does not make retained bytes free. Enforce conservative per-subscriber referenced-byte limits and a global retained-allocation budget at the shared-rendition owner. Bounded queues alone do not bound a library's downstream stream buffers.
- An explicit discontinuity after loss or configuration change. Video resumes only at a verified independently decodable random-access point with matching decoder configuration. The adapter must not forward a partial dependency chain.
- One RAII lease that owns admission and source demand. Cancellation and timeout release it exactly once off the production clock.
- Transport closure is not the authority-revocation boundary. On a bounded close deadline, the server revokes the subscription generation, stops enqueue, and acknowledges the generation fence. It releases source demand once retained media operations no longer need it. Quarantine a stalled underlying QUIC teardown separately under strict connection-count and retained-byte caps; account retained packets until actual release. Never await transport shutdown on the output clock. A fallback lease requires explicit server revocation acknowledgement, not merely a client timer or a locally closed socket.
- The current browser ownership primitive only bounds its close wait and rejects on expiry; it has no server revocation endpoint yet. It is not production-ready lease management. Integration must replace that limitation with the acknowledged revocation path and deterministic stalled-close/generation-fence tests.

The adapter may transform framing and timestamps for the selected MoQ media mapping. It must not decode, encode, bake, cause global keyframe storms, or retain an unbounded group of packets. Audio/video watermark and ordering semantics require agreement with the output redesign before concrete types are frozen.

## Security and protocol decision gates

Candidate ecosystem: moq-dev's `moq-lite` protocol and `hang` media mapping, with Rust `moq-relay` 0.17.14 / `moq-net` 0.3.21 / `moq-tokio` 0.19.34 / `hang` 0.21.22 / `moq-mux` 0.10.22 and browser `@moq/net` 0.4.7 / `@moq/watch` 0.6.6. Only `@moq/net` 0.4.7 is installed and pinned, with direct runtime schema peers `zod` and `@zod/mini` 4.6.7. The other packages remain research candidates. Browser/server interoperability is not yet verified. Pin remaining exact versions after API verification. Do not describe the moq-lite subset as full IETF MoQ Transport feature parity.

The upstream [transport documentation](https://doc.moq.dev/concept/transport) states that its browser client restricts WebTransport to Firefox 153+ and currently avoids Safari WebTransport, despite API presence. It also automatically races a WebSocket/qmux fallback. The Conspect adapter must disable that race: TCP qmux is not the requested QUIC-first/WebRTC fallback policy. Verify the pinned client can do this before adoption. Do not advertise browser support from a user-agent table or API presence alone.

The pinned [relay API](https://docs.rs/moq-relay/0.17.14/moq_relay/struct.Relay.html) supports embedded admission decisions and bounded shutdown. A relay is not itself the production publisher: bounded subscriber queues, downstream stream/cache budgets, and shared rendition wiring still require implementation and tests.

Research identified a transitive LGPL-2.1 libav.js audio polyfill in the watch stack. Resolve distribution obligations with the product's legal process before adopting that bundle; do not edit legal documents as engineering work. Prefer native decoder capability checks where possible, without silently losing requested audio.

Use interoperable MoQ, not a proprietary WebTransport byte stream labelled MoQ.

Authorize every program/input/output identifier at lease creation and bind the transport credential to that lease and scope. Every server lease has a finite maximum lifetime, enforced by a server-side monotonic expiry task independently of browser callbacks and transport promises. Renewal requires current authorization and admission; a disconnected or stalled browser cannot retain source demand indefinitely. A revoke endpoint must acknowledge the revoked generation idempotently. Report a client close deadline as a retryable teardown failure, not as successful release or an indefinitely pending UI state. Browser transport credentials must be short-lived; never put a long-lived control bearer in a URL or log. Admission and subscriber limits also apply to the QUIC endpoint. Configure trusted TLS and IPv6-first binding; an HTTP reverse proxy does not implicitly provide a QUIC/WebTransport endpoint.

Do not advertise MoQ merely because the binary includes its crate. Advertise it only when the configured listener, protocol adapter, and authorized rendition provider are available.

## Cloudflare relay management (required scope)

The user requires full Cloudflare MoQ relay management, not merely a manually supplied relay URL: automatic and manual provisioning, discovery/import of existing relays, configuration, credential rotation, teardown, health/status monitoring, error reporting and recovery. Distinguish application-owned resources from imported resources before destructive operations. Preserve an audit trail without credential material.

Accept either a user-supplied scoped API token or Global API Key (with its required account email). Store credentials server-side under the product secret-storage policy; never return keys to the browser or logs. When a global key is supplied, offer creation of a least-privilege scoped token if the user has fewer than 50 existing scoped tokens. Count all pages, handle unavailable permission/count information explicitly, and recheck quota/conflict errors at creation. Offer, do not silently create. Do not delete unrelated tokens to make room. A supplied scoped token remains a supported path. The operator's 1Password credential reference is operational input, not shipped product configuration.

Cloudflare's [current service documentation](https://developers.cloudflare.com/moq/) lists deployed draft-14 and authenticated draft-16, with draft-18 testing through the [interop runner](https://github.com/englishm/moq-interop-runner). The runner registry also lists experimental Cloudflare draft-18/22. Target authenticated draft-16 and draft-18 compatibility with explicit version/endpoint selection and negotiated-version diagnostics. Do not call draft-18 the latest IETF draft or experimental endpoints production-ready. Confirm current endpoint capabilities when implementation starts; pin the interop runner revision and implementation versions in test evidence. Legacy unauthenticated draft-14 must not become a silent downgrade.

Cloudflare relay credentials are distinct from account-management API credentials. Its draft-16 relay token travels in the URL path, so redact the entire credential-bearing path, not only query parameters. Keep publisher and subscriber permissions separate, use finite expiry, and support revocation/rotation. Relay creation returns token secrets only once: securely persist or recover from partial provisioning without leaking secrets or creating orphan relays.

Protocol interop runner tests are necessary but insufficient: its current CLI/container tests explicitly exclude browser-only implementations. Require a separate actual browser WebTransport media decode test through the Cloudflare relay, plus authorization, revocation, fallback and isolation tests.

## Delivery and verification plan

- [ ] Pin interoperable protocol, media mapping, and browser/server library versions.
- [ ] Finalize transport contract with the shared-rendition owner.
- [ ] Implement bounded transport adapter and fake-producer tests before pipeline wiring.
- [ ] Implement per-object authorization, lease cleanup, capabilities, and generated API types.
- [ ] Implement browser playback and MoQ-first selection tests.
- [ ] Verify actual browser/server interoperability and decoded media delivery.
- [ ] Verify slow, disconnected, malicious, and failed consumers cannot stall production or exceed queue budgets.
- [ ] Integrate shared-rendition production wiring with its owner.
- [ ] Complete independent cross-vendor review, CI, and reviewed integration.

Required selection tests cover unsupported browser/deployment, supported MoQ, cleanup-before-fallback, no fallback for authorization/capacity/codec errors, unmount during connection, and idempotent lease release. Isolation tests cover blocked consumers, audio bursts, oversized frames, loss within a GOP, configuration generation changes, and bounded teardown.
