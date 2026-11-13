//! HTTP/3 and WebTransport on the moq-noq QUIC stack (front-door slice 2,
//! design note `docs/architecture/network-front-door.md` section 5).
//!
//! One QUIC endpoint (one UDP socket, ALPN `h3`, one certificate) serves
//! both the site and `MoQ`. Every request on a connection goes to the
//! [`Site`], except an extended CONNECT with `:protocol = webtransport`
//! under the configured prefix (`/moq/`), which becomes the connection's one
//! WebTransport session handed to the [`WtHandler`] as a [`WtSession`]
//! (`web_transport_trait::Session`, so moq-net accepts it through the
//! moq-tokio async adapter).
//!
//! # Structure
//!
//! * [`noq_adapter`]: h3-quinn 0.0.10 ported to moq-noq 1.3.5 (the
//!   `h3::quic` traits), plus the receive-side guards and the stream router
//!   (below).
//! * [`webtransport`]: the session, derived from h3-webtransport 0.1.2.
//!   Unlike upstream, the h3 connection is never moved into the session:
//!   the adapter classifies every peer stream by its first varint (`0x41`
//!   bidirectional / `0x54` unidirectional = WebTransport) before h3 sees
//!   it and routes WebTransport streams to the session's bounded queues,
//!   so the same connection driver keeps serving page requests while a
//!   session lives and answers any further extended CONNECT with `429`.
//! * [`server`]: the connection driver and the request/response conversion.
//! * [`endpoint`]: the noq endpoint on a caller-provided socket, TLS policy
//!   (TLS 1.3, ALPN `h3`, 0-RTT off, no key log), the accept loop with its
//!   connection caps, and the peers hook.
//!
//! # Pooling
//!
//! One WebTransport session per connection, for the connection's life
//! (`SETTINGS_WEBTRANSPORT_MAX_SESSIONS = 1`): a further CONNECT is
//! answered `429` and its stream finished, also after the session ended. A
//! CONNECT the handler's [`WtHandler::admit`] rejects (401/403/...) does not
//! take the slot.
//!
//! # Bounds (invariant #10)
//!
//! Every queue or allocation an HTTP/3 peer can drive, and what caps it:
//!
//! | Peer-driven state | Cap |
//! | --- | --- |
//! | Connections | [`H3Limits::max_connections`] (1024) and [`H3Limits::max_connections_per_source`] (64 per IPv4 address / IPv6 /64), pre-checked before the handshake (over a cap the `Incoming` is refused) and charged only when the handshake completes |
//! | Handshakes in progress | [`H3Limits::max_handshakes`] (64) and [`H3Limits::max_handshakes_per_source`] (16), counted separately from connections (an incomplete handshake never holds a connection slot); beyond them the `Incoming` is refused. Deadline [`H3Limits::handshake_timeout`] (5 s). noq `max_incoming` / `incoming_buffer_size_total` from the caller bound the queue before that |
//! | Streams per connection | QUIC [`QuicLimits::max_concurrent_bidi_streams`] (128) / [`QuicLimits::max_concurrent_uni_streams`] (128); every stream the adapter holds while classifying it is one of these |
//! | Unclassified streams | the classifier reads at most [`noq_adapter::HEAD_MAX`] (16) bytes per stream into a fixed buffer; a stream still unclassified at [`H3Limits::header_deadline`] is reset (bidi `H3_REQUEST_REJECTED`, uni `H3_STREAM_CREATION_ERROR`) by one timer polled from the accept path, so a silent stream never stalls the connection |
//! | Bytes per stream / connection | [`QuicLimits::stream_receive_window`] (1 MiB) / [`QuicLimits::receive_window`] (16 MiB) |
//! | Request header block | the guard ([`guard`]) rejects any non-DATA frame declaring more than `max_field_section_size` (16 384) + 1 KiB the moment its length varint arrives (`H3_EXCESSIVE_LOAD`), before h3 buffers it; QPACK then enforces the field-section size; header deadline [`H3Limits::header_deadline`] (10 s from stream accept to a complete HEADERS frame, else `H3_REQUEST_REJECTED`) |
//! | Peer control stream | [`guard::ControlGuard`]: SETTINGS at most 1 KiB, `CANCEL_PUSH`/GOAWAY/`MAX_PUSH_ID` at most 16 bytes, any other frame at most 1 KiB and 16 KiB over the connection's life, else the connection is closed `H3_EXCESSIVE_LOAD`; SETTINGS complete within [`H3Limits::settings_deadline`] (10 s from connection start, else `H3_MISSING_SETTINGS`); no frame partially received for longer than [`H3Limits::control_frame_deadline`] (10 s, else `H3_EXCESSIVE_LOAD`). The timer is polled from the accept path |
//! | Peer QPACK streams | one encoder and one decoder (a second is `H3_STREAM_CREATION_ERROR`), drained by the adapter, [`noq_adapter::QPACK_STREAM_MAX`] (64) bytes each over their life (the server's dynamic table is 0), else `QPACK_ENCODER/DECODER_STREAM_ERROR` |
//! | Other peer uni streams | push and unknown/grease types are stopped with `H3_STREAM_CREATION_ERROR` at classification |
//! | Requests in flight | [`H3Limits::max_concurrent_requests`] (128) permits; a request stream beyond it is reset with `H3_REQUEST_REJECTED`, never queued |
//! | Site handler | [`Site::call`] must return the response head within [`H3Limits::response_head_timeout`] (30 s), else the future is dropped, the stream reset `H3_REQUEST_REJECTED` and the permit released; each response write (head, DATA, trailers, FIN) must progress within [`H3Limits::response_stall_timeout`] (30 s), else `H3_REQUEST_CANCELLED`. The body's total duration is unbounded (SSE) but held by QUIC flow control |
//! | Request body | streamed on demand, one QUIC chunk at a time ([`RequestBody`]) |
//! | WT handler | runs until the session ends (either side's close, FIN, reset, connection loss), then [`H3Limits::wt_close_grace`] (5 s), after which its future is dropped |
//! | WT streams before the session exists | [`H3Limits::wt_early_streams`] (16), then reset with `H3_WEBTRANSPORT_BUFFERED_STREAM_REJECTED` |
//! | WT incoming stream queues | [`H3Limits::wt_bidi_queue`] / [`H3Limits::wt_uni_queue`] (64 each, `try_send`); when full the new stream is reset with `H3_WEBTRANSPORT_BUFFERED_STREAM_REJECTED` |
//! | WT datagrams | noq's receive buffer [`QuicLimits::datagram_receive_buffer_size`] (256 KiB), which drops the OLDEST datagram when full; nothing is queued above it |
//! | CONNECT-stream capsules | one capsule body of at most 1028 bytes; unknown capsules are skipped by counting |
//! | Request tasks | one per admitted request (bounded by the permits) plus the session task; all spawned on the runtime serving the connection (the session close too, from whichever runtime or thread `close`/drop happens) |
//!
//! Nothing here awaits the engine; the caller runs the endpoint on the
//! front door's own runtime.

pub mod capsule;
#[cfg(feature = "test-client")]
pub mod client;
pub mod config;
pub mod counters;
pub mod endpoint;
pub mod guard;
pub mod noq_adapter;
pub mod server;
#[cfg(any(test, feature = "fuzzing"))]
pub mod testing;
pub mod varint;
pub mod webtransport;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use bytes::Bytes;

pub use config::{H3Limits, H3Settings, QuicLimits};
pub use counters::Counters;
pub use endpoint::{server_config, server_tls, EndpointParts, H3Endpoint, PeersHook, ALPN_H3};
pub use server::{serve_connection, RequestBody};
pub use webtransport::{WtError, WtRecvStream, WtSendStream, WtSession};

/// A boxed, sendable error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// The response body the [`Site`] returns (an axum `Body` converts into it
/// with `http_body_util::BodyExt::map_err(..).boxed_unsync()`).
pub type ResponseBody = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;

/// The site: every request that is not a WebTransport session.
///
/// The request carries `Version::HTTP_3`, a `host` header (from
/// `:authority` when the client sent none), no pseudo-headers, and a body
/// streamed on demand. Connection-specific response headers are removed
/// before sending (RFC 9114 section 4.2).
pub trait Site: Send + Sync + 'static {
    /// Answers one request from `peer` (the QUIC remote address).
    fn call(
        &self,
        req: http::Request<RequestBody>,
        peer: SocketAddr,
    ) -> BoxFuture<'static, http::Response<ResponseBody>>;
}

/// An opaque token [`WtHandler::admit`] hands to [`WtHandler::accept`].
pub type AdmitToken = Box<dyn std::any::Any + Send>;

/// The WebTransport handler (`MoQ`), for `CONNECTs` under the prefix.
pub trait WtHandler: Send + Sync + 'static {
    /// Decides synchronously, BEFORE the 200 is sent, whether to accept the
    /// session (e.g. the single-use ticket check). `Err(status)` answers
    /// with that status, finishes the stream, and leaves the connection's
    /// session slot free.
    ///
    /// # Errors
    ///
    /// The status to answer with.
    fn admit(
        &self,
        req: &http::request::Parts,
        peer: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode>;

    /// Runs the accepted session. Returning (or dropping every clone of the
    /// session) closes it.
    fn accept(
        &self,
        session: WtSession,
        token: AdmitToken,
        req: http::request::Parts,
        peer: SocketAddr,
    ) -> BoxFuture<'static, ()>;

    /// Subprotocols the server supports, in preference order; the first one
    /// the client also offers in `wt-available-protocols` is selected and
    /// echoed as `wt-protocol`.
    fn protocols(&self) -> &[&'static str];
}

/// Errors of this crate.
#[derive(Debug)]
pub enum Error {
    /// A configuration value is unusable.
    Config(&'static str),
    /// An I/O error from the socket.
    Io(std::io::Error),
    /// The HTTP/3 connection failed.
    H3(h3::error::ConnectionError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(why) => write!(f, "invalid configuration: {why}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::H3(err) => write!(f, "HTTP/3 connection error: {err}"),
        }
    }
}

impl std::error::Error for Error {}
