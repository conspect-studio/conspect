# Provenance of vendored code in `multiview-h3`

All upstream code below is MIT-licensed (`Copyright (c) 2020 h3 authors`,
<https://github.com/hyperium/h3>); the notice is kept at the top of every
vendored file.

| Upstream crate | Version | `.crate` sha256 | Vendored as |
| --- | --- | --- | --- |
| `h3-quinn` | 0.0.10 | `8b2e732c8d91a74731663ac8479ab505042fbf547b9a207213ab7fbcbfc4f8b4` | `src/noq_adapter/mod.rs` (`src/lib.rs`), `src/noq_adapter/datagram.rs` (`src/datagram.rs`) |
| `h3-webtransport` | 0.1.2 | `2d91a50fd582a5d67b1f756fba3cd9c66367ff4f23e1017c882f664d63b350a7` | `src/webtransport.rs` (derived from `src/server.rs` and `src/stream.rs`) |

Used as dependencies, unmodified: `h3 =0.0.8`
(`10872b55cfb02a821b69dc7cf8dc6a71d6af25eb9a79662bec4a9d016056b3be`) and
`h3-datagram =0.0.2`
(`9d2c9f77921668673721ae40f17c729fc48b9e38a663858097cea547484fdf0f`).

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
5. New: `web_transport_trait::Session` for `WtSession` (accept/open
   streams, datagrams, `protocol()`, `close(code, reason)` sending
   `CLOSE_WEBTRANSPORT_SESSION` (0x2843, reason truncated to 1024 bytes)
   then FIN, `closed()`), `SendStream`/`RecvStream` with WebTransport error
   code mapping (draft section 4.4), bounded incoming queues, and a bounded
   capsule reader on the CONNECT stream (`crate::capsule`).
