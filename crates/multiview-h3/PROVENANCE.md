# Provenance of vendored code in `multiview-h3`

All upstream code below is MIT-licensed (`Copyright (c) 2020 h3 authors`,
<https://github.com/hyperium/h3>); the notice is kept at the top of every
vendored file.

| Upstream crate | Version | `.crate` sha256 | Vendored as |
| --- | --- | --- | --- |
| `h3-quinn` | 0.0.10 | `8b2e732c8d91a74731663ac8479ab505042fbf547b9a207213ab7fbcbfc4f8b4` | `src/noq_adapter/mod.rs` (`src/lib.rs`), `src/noq_adapter/datagram.rs` (`src/datagram.rs`) |
| `h3-webtransport` | 0.1.2 | `2d91a50fd582a5d67b1f756fba3cd9c66367ff4f23e1017c882f664d63b350a7` | `src/webtransport.rs` (derived from `src/server.rs` and `src/stream.rs`) |

Used as a dependency, unmodified: `h3-datagram =0.0.2`
(`9d2c9f77921668673721ae40f17c729fc48b9e38a663858097cea547484fdf0f`).

`h3 =0.0.8` (`.crate` sha256
`10872b55cfb02a821b69dc7cf8dc6a71d6af25eb9a79662bec4a9d016056b3be`, upstream
commit `22c1aa3f44d1463cd7644c8f654fffc9a6da305c`, path `h3`) is vendored
whole at `vendor/h3` and selected by the root `[patch.crates-io]` (and the
fuzz workspace's), because of a peer-reachable panic (issue #403); see below.

## h3 0.0.8 → `vendor/h3` (issue #403)

The published archive, copied unchanged except for these edits, each marked
`conspect patch (issue #403)` in the source (`.cargo-ok` dropped; `LICENSE`
unchanged). Upstream `master` still carries the panic as of 2026-11-14.

1. `src/frame.rs` `FrameStream::poll_data`: when the stream ends while a
   DATA frame still owes payload bytes, return
   `FrameStreamError::UnexpectedEnd` (H3_FRAME_ERROR, RFC 9114 section 7.1)
   instead of `Ok(None)`. Upstream left `remaining_data` non-zero, so the
   request-body consumer's next step (trailers, `poll_next`) hit
   `assert!(remaining_data == 0)` — the fuzz crash `JAEI+/dQATjR9/EAFg==`
   (HEADERS, then a DATA frame declaring 22 bytes, then FIN). A WebTransport
   stream (unbounded payload, `usize::MAX`) still ends cleanly.
2. `src/frame.rs` `FrameStream::poll_next`: the `assert!` becomes
   `FrameStreamError::Proto(Malformed)`, so no other caller order can panic
   on peer-driven state. Its upstream `#[should_panic]` unit test now asserts
   the error, and a test for the truncated frame is added.
3. `src/buf.rs` `BufList::push_bytes`: the `debug_assert!` on a non-empty
   transport chunk becomes an early return.

Audit of the remaining panics in `h3` reachable from a server request: the
`expect`s in `stream.rs` `into_stream` and `connection.rs` run only after
`poll_type` resolved (guarded by construction); `proto/frame.rs`'s
`unreachable!` is behind the DATA/WebTransport early returns; the
`AsyncRead` `assert!`s in `stream.rs` compare a chunk taken with the
destination's length as its cap; the varint `unwrap`/`unreachable!` are on
encode paths or behind length checks. None is reachable from peer bytes.
Panic containment for anything missed is in the server (edit below).

## h3-quinn 0.0.10 → `noq_adapter`

1. Ported from `quinn 0.11` to `moq-noq 1.3.5` (imported as `noq`): the three
   design-note edits — `RecvStream::read_chunk` takes one argument (no
   `ordered` flag), it returns `Bytes` rather than `Chunk`, and
   `ReadError::IllegalOrderedRead` does not exist.
2. Lint-clean under the workspace rules: every `expect`/`unwrap`/`panic!` is
   replaced by an error (`ConnectionErrorIncoming::InternalError` for the
   never-ending stream sources, `StreamErrorIncoming::Unknown` for
   `poll_send` while a write is buffered, `VarInt::MAX` saturation for codes).
3. `RecvStream::poll_data` polls a fresh `read_chunk` future per call instead
   of a stored `ReusableBoxFuture` (noq's `read_chunk` is cancel safe and
   stateless between polls), dropping the `tokio-util` dependency; stream ids
   are cached at construction so `recv_id`/`send_id` cannot fail.
4. Added (server side): the header-block guard and the per-request header
   deadline on every accepted bidirectional stream (`GuardConfig`,
   `crate::guard`); a rejected stream is stopped and reset with
   `H3_EXCESSIVE_LOAD` (guard) or `H3_REQUEST_REJECTED` (deadline).
   Every peer stream still being classified carries the header deadline,
   with one timer for the earliest polled from the accept path. The peer's
   control stream is handed to h3 behind `crate::guard::ControlGuard` (per
   frame caps) with SETTINGS and partial-frame deadlines; its QPACK streams
   are drained by the adapter (64 bytes each); push/unknown uni stream types
   are stopped at classification. Classification reads at most 16 bytes.
5. Added: the `StreamRouter` hook. Peer streams are classified by their first
   varint before h3 sees them: `0x41` bidi / `0x54` uni (WebTransport) are
   handed to the router as raw noq streams; request streams take a
   concurrency permit or are reset with `H3_REQUEST_REJECTED`. Already-read
   bytes are pushed back (`RecvStream::unread`).
6. `RecvStream` stops an unfinished stream with `H3_NO_ERROR` on drop
   (noq's default stop code 0 is not an HTTP/3 code).
7. `tracing` is unconditional (no `tracing` feature); the `#[instrument]`
   attributes are dropped.
8. `datagram.rs`: the `expect` on the datagram stream source becomes an
   error; the h3 → h3-datagram error conversion is gone (h3-datagram 0.0.2
   re-exports h3's `ConnectionErrorIncoming`).

## h3-webtransport 0.1.2 → `webtransport`

Structural edit: the `h3::server::Connection` is **not** moved into the
session. Upstream `WebTransportSession::accept` takes the connection by value
and its `accept_bi` resolves each page request's headers inline, which would
let one slow request stall the accept loop and makes a 429 for a second
CONNECT impossible without extra locking. Here the noq adapter routes the
session's streams (edit 5 above) and the connection driver keeps serving
requests, so the design's pooling contract holds.

1. `accept` → `server::handle_request` + `connect_response`: the subprotocol
   is negotiated — the client's `wt-available-protocols` (RFC 8941 list of
   sf-strings; tokens and the legacy `sec-webtransport-protocol` name are
   tolerated) is matched against the handler's list and the first supported
   entry is echoed as `wt-protocol: "<sel>"` beside
   `sec-webtransport-http3-draft: draft02` in the 200.
2. No `unwrap()`: `Response::builder().body(())` errors are mapped; mutex
   poisoning is recovered (`unpoison`), never unwrapped; the `assert!` in
   `OpenUni` is gone.
3. Stream headers (`0x41`/`0x54` + session id) are written with the CONNECT
   stream id itself as the session id. (Upstream converts through
   `StreamId::index()`, which is only correct for stream 0.)
4. Datagrams are framed directly with the quarter stream id. (h3-datagram
   0.0.2's `Datagram::encode` never copies the encoded id into its output.)
   Interop note: moq-tokio's own WebTransport client (`web-transport-moq`
   1.3.5) prefixes datagrams with the FULL CONNECT stream id, so it does not
   round-trip datagrams with this server until its prefix matches the draft
   (`tests/datagram.rs` pins the quarter-id framing browsers use). The
   moq-transport-16/18 versions this deployment uses deliver objects on
   streams, not datagrams.
5. New: `web_transport_trait::Session` for `WtSession` (accept/open
   streams, datagrams, `protocol()`, `close(code, reason)` sending
   `CLOSE_WEBTRANSPORT_SESSION` (0x2843, reason truncated to 1024 bytes)
   then FIN, `closed()`), `SendStream`/`RecvStream` with WebTransport error
   code mapping (draft section 4.4), bounded incoming queues, and a bounded
   capsule reader on the CONNECT stream (`crate::capsule`).
