# Experimental publisher delivery adapter — NOT production complete

Sources copied without upstream licensing edits from the cached crates.io archives:

- moq-net 0.3.21 (package provenance in its `.cargo_vcs_info.json`)
- web-transport-moq 1.3.5 (same provenance file)

The root Cargo.lock pins the original registry packages' transitive dependencies; root path patches select these exact local source versions. No custom MoQ messages are added.

## Changed semantics

Native `web-transport-moq` trait `SendStream::closed` uses raw `moq-noq::SendStream::stopped`: only `Ok(None)` means all finished stream data was acknowledged. An unmappable HTTP/3 STOP code is now an error, never a successful ACK. Public `stopped()` remains source-compatible. Regression tests use real IPv6 QUIC endpoints.

`moq-net::stats::Session::with_delivery` is an experimental opt-in per-connection measurement handle. Both lite and IETF outgoing GROUP streams attach an RAII guard. Writer raw payload writes count exact bytes accepted; buffered framing/header writes are excluded. After FIN, successful terminal close credits the group payload exactly once. Reset, STOP, cancellation, expiry and driver drop abandon the accepted payload. Pending counts payload awaiting full-group ACK, not transport bytes-in-flight or actual retained allocator bytes. No per-group history or payload is retained by the meter; one guard lives in each existing group stream writer.

## Remaining gates — do not use for production ABR yet

- Explicit backend ACK capability must default unsupported and be propagated through the moq-tokio async-to-poll adapter. The existing opt-in alone cannot prove native QUIC semantics for all generic transports.
- Wire the controller into the actual outgoing publisher-to-relay session, never direct viewer sessions. Enforce actual candidate-prefix trial memory/deadline cancellation externally and reserve audio/production independently.
- Run real shaped publisher path, two independently shaped viewer paths, Cloudflare supported transport and reset scenarios. Current controller unit traces are not those tests.
- Independent review, cargo-deny/licensing gate and integration CI remain required. Do not claim Cloudflare or unknown-capacity ABR completion based on these unit tests.

Targeted evidence: jobs=2 cargo check --locked -p multiview-moq; native raw_close tests 3 passed; delivery guard tests 2 passed; controller tests 2 passed. Full suite deliberately not run. Local target growth 3.6GiB as of checkpoint4a93d034.

## Capability propagation checkpoint463d89f6

Additional unchanged source archives: moq-tokio0.19.34 and web-transport-trait0.4.0.
Both trait interfaces add a source-compatible defaultfalse capability method. Native
web-transport-moq overrides true; moq-tokio captures and forwards the actual stream
capability. Group Writer latches unsupported and refuses ACK credits for false streams.
Controller rejects unsupported snapshots. A default untagged stats session now retains
its explicit delivery handle independently of billing/traffic registry scopes.

Archive SHA256 (cached .crate bytes):
- moq-net0.3.21: e069329fa033f1a336e61f3cff8042860b5108deb9a460c9d3f557cd831a21df
- web-transport-moq1.3.5: b896208c8b083650b384c7b152ec05831a32df5029c85d88dc6fa11fd5f3627b
- moq-tokio0.19.34: 3c06b212a12ed6defbcfe386332446fd55ba2e183162e334c2e3b02276fc91be
- web-transport-trait0.4.0: 2a916df726e858e8e6cc21e852129f59e6277d3a01bb2254c0506dcf829fdd5d

Focused unsupported-writer regression passed (one test;1523filtered): defaultfalse
fake stream cannot credit payload ACK, sets unsupported and releases guard. Root
workspace members/lock restored after test invocation; no dev-dependency graph retained.
