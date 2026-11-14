// Copyright (c) 2020 h3 authors
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// Derived from h3-webtransport 0.1.2 (src/server.rs: the CONNECT validation
// and response, the stream headers of OpenBi/OpenUni; src/stream.rs: the
// stream wrappers). The structural edits (the h3 connection is NOT moved
// into the session, streams are routed by the noq adapter) are listed in
// crates/multiview-h3/PROVENANCE.md.

//! WebTransport sessions over the h3 server connection.
//!
//! A [`WtSession`] implements [`web_transport_trait::Session`]. It does not
//! own the h3 connection (h3-webtransport's `WebTransportSession` does,
//! which is why it can serve neither page requests nor a 429 on a pooled
//! connection without an extra lock): the noq adapter routes the session's
//! streams to its bounded queues before h3 sees them, and the connection
//! driver keeps serving page requests on the same connection.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use tokio::sync::{mpsc, watch};

use crate::capsule::{self, Capsule, CapsuleParser};
use crate::varint;

/// `sec-webtransport-http3-draft` value Chrome checks (h3-webtransport 0.1.2).
pub const DRAFT_HEADER: &str = "sec-webtransport-http3-draft";
/// The draft the server answers with.
pub const DRAFT_VALUE: &str = "draft02";
/// Client offer of subprotocols (draft-ietf-webtrans-http3-14 section 3.3).
pub const AVAILABLE_PROTOCOLS: &str = "wt-available-protocols";
/// The legacy name of the offer header (earlier drafts / some clients).
pub const AVAILABLE_PROTOCOLS_LEGACY: &str = "sec-webtransport-protocol";
/// The server's selected subprotocol.
pub const SELECTED_PROTOCOL: &str = "wt-protocol";

/// Stream type of a WebTransport unidirectional stream.
pub const STREAM_UNI: u64 = 0x54;
/// Signal of a WebTransport bidirectional stream.
pub const STREAM_BIDI: u64 = 0x41;

/// First h3 error code of the WebTransport application range.
const ERROR_FIRST: u64 = 0x52e4_a40f_a8db;
const ERROR_LAST: u64 = 0x52e5_ac98_3162;

/// Maps a WebTransport application error code to its HTTP/3 code
/// (draft-ietf-webtrans-http3 section 4.4; skips the reserved codepoints).
#[must_use]
pub fn error_to_http3(code: u32) -> u64 {
    let code = u64::from(code);
    ERROR_FIRST + code + code / 0x1e
}

/// Inverse of [`error_to_http3`]; `None` outside the WebTransport range or
/// on a reserved codepoint.
#[must_use]
pub fn error_from_http3(code: u64) -> Option<u32> {
    if !(ERROR_FIRST..=ERROR_LAST).contains(&code) {
        return None;
    }
    let shifted = code - ERROR_FIRST;
    if shifted % 0x1f == 0x1e {
        return None;
    }
    u32::try_from(shifted - shifted / 0x1f).ok()
}

/// `H3_WEBTRANSPORT_BUFFERED_STREAM_REJECTED`.
pub const BUFFERED_STREAM_REJECTED: u64 = 0x3994_bd84;
/// `H3_WEBTRANSPORT_SESSION_GONE`.
pub const SESSION_GONE: u64 = 0x170d_7b68;

/// Most offered protocols considered (Chrome sends what the page passes;
/// `@moq/net` offers 13).
pub const MAX_OFFERED: usize = 32;

/// Parses one complete `wt-available-protocols` field value (all field
/// lines already combined with `, `, RFC 9110 section 5.3) as an RFC 8941
/// List with a real structured-field parser. Draft-ietf-webtrans-http3
/// section 3.3: the only valid member type is String and "any value type
/// other than String MUST be treated as an error that causes the entire
/// field to be ignored"; parameters are ignored. So a field that does not
/// parse, contains a non-String member or an inner list, or offers more
/// than [`MAX_OFFERED`] protocols yields `None` (ignored), never a prefix.
#[must_use]
pub fn parse_protocol_list(value: &str) -> Option<Vec<String>> {
    let list = sfv::Parser::new(value).parse::<sfv::List>().ok()?;
    if list.len() > MAX_OFFERED {
        return None;
    }
    list.iter()
        .map(|entry| match entry {
            sfv::ListEntry::Item(item) => item.bare_item.as_string().map(|s| s.as_str().to_owned()),
            sfv::ListEntry::InnerList(_) => None,
        })
        .collect()
}

/// The client's offer: every `wt-available-protocols` field line combined;
/// the legacy `sec-webtransport-protocol` name only when the standard one
/// is absent. `None` when absent or ignored (see [`parse_protocol_list`]).
fn offered_protocols(headers: &http::HeaderMap) -> Option<Vec<String>> {
    let name = if headers.contains_key(AVAILABLE_PROTOCOLS) {
        AVAILABLE_PROTOCOLS
    } else {
        AVAILABLE_PROTOCOLS_LEGACY
    };
    let mut lines = headers.get_all(name).iter().peekable();
    lines.peek()?;
    // RFC 9110 section 5.3: lines combine with ", " between EVERY pair, an
    // empty line included (an empty member then makes the List malformed,
    // so the field is ignored rather than its other lines used).
    let lines: Vec<&str> = lines.map(|l| l.to_str().ok()).collect::<Option<_>>()?;
    let combined = lines.join(", ");
    parse_protocol_list(&combined)
}

/// Encodes `value` as an RFC 8941 sf-string (`"..."` with `\` escapes).
/// Returns `None` for characters an sf-string cannot carry.
#[must_use]
pub fn encode_sf_string(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if !(' '..='~').contains(&c) {
            return None;
        }
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// The outcome of subprotocol negotiation for one CONNECT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The client offered nothing usable (no offer header, an empty one, or
    /// one that is ignored as malformed: draft-ietf-webtrans-http3 section
    /// 3.3). The session gets no `wt-protocol`.
    NoOffer,
    /// The client's most preferred offer the server supports; echoed as
    /// `wt-protocol`.
    Selected(&'static str),
    /// The client offered protocols and the server supports none of them.
    NoMatch,
}

impl Selection {
    /// The protocol to echo, if any.
    #[must_use]
    pub const fn selected(self) -> Option<&'static str> {
        match self {
            Self::Selected(p) => Some(p),
            Self::NoOffer | Self::NoMatch => None,
        }
    }
}

/// Negotiates the subprotocol: the client lists `wt-available-protocols`
/// in its preference order (Chrome: an sf-list of sf-strings, exactly as
/// the page passed `protocols`); the first entry the server supports wins.
#[must_use]
pub fn negotiate(headers: &http::HeaderMap, supported: &[&'static str]) -> Selection {
    let Some(offered) = offered_protocols(headers) else {
        return Selection::NoOffer;
    };
    if offered.is_empty() {
        return Selection::NoOffer;
    }
    offered
        .iter()
        .find_map(|o| supported.iter().copied().find(|s| *s == o.as_str()))
        .map_or(Selection::NoMatch, Selection::Selected)
}

/// Picks the first protocol the client offered that the server supports.
#[must_use]
pub fn select_protocol(
    headers: &http::HeaderMap,
    supported: &[&'static str],
) -> Option<&'static str> {
    negotiate(headers, supported).selected()
}

/// The 200 response to an accepted WebTransport CONNECT.
///
/// # Errors
///
/// The selected protocol cannot be encoded (never for `&'static` ASCII).
pub fn connect_response(selected: Option<&str>) -> Result<http::Response<()>, http::Error> {
    let mut response = http::Response::builder()
        // This is the only header that chrome cares about.
        .header(DRAFT_HEADER, DRAFT_VALUE)
        .status(http::StatusCode::OK);
    if let Some(value) = selected.and_then(encode_sf_string) {
        response = response.header(SELECTED_PROTOCOL, value);
    }
    response.body(())
}

/// Why a WebTransport operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WtError {
    /// The session was closed (by either side) with this code and reason.
    SessionClosed {
        /// Application error code.
        code: u32,
        /// Reason text.
        reason: String,
    },
    /// The session ended without a CLOSE capsule (CONNECT stream finished
    /// or reset, or the connection went away).
    SessionGone(String),
    /// The peer reset the stream with this WebTransport code.
    StreamReset(u32),
    /// The peer stopped the stream with this WebTransport code.
    StreamStopped(u32),
    /// The stream was already finished or reset locally.
    ClosedStream,
    /// A datagram could not be sent.
    Datagram(&'static str),
}

impl std::fmt::Display for WtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionClosed { code, reason } => {
                write!(f, "session closed: code {code}, reason {reason:?}")
            }
            Self::SessionGone(why) => write!(f, "session gone: {why}"),
            Self::StreamReset(code) => write!(f, "stream reset by peer: code {code}"),
            Self::StreamStopped(code) => write!(f, "stream stopped by peer: code {code}"),
            Self::ClosedStream => f.write_str("stream already closed"),
            Self::Datagram(why) => write!(f, "datagram not sent: {why}"),
        }
    }
}

impl std::error::Error for WtError {}

impl web_transport_trait::Error for WtError {
    fn session_error(&self) -> Option<(u32, String)> {
        match self {
            Self::SessionClosed { code, reason } => Some((*code, reason.clone())),
            Self::SessionGone(_) => Some((0, String::new())),
            _ => None,
        }
    }

    fn stream_error(&self) -> Option<u32> {
        match self {
            Self::StreamReset(code) | Self::StreamStopped(code) => Some(*code),
            _ => None,
        }
    }
}

type ConnectSend = RequestStream<crate::noq_adapter::SendStream<Bytes>, Bytes>;
type ConnectRecv = RequestStream<crate::noq_adapter::RecvStream, Bytes>;

/// An incoming bidirectional stream as queued for the handler.
pub(crate) type QueuedBi = (noq::SendStream, noq::RecvStream, Option<Bytes>);
/// An incoming unidirectional stream as queued for the handler.
pub(crate) type QueuedUni = (noq::RecvStream, Option<Bytes>);

/// State shared by the session handles, its streams and the driver.
pub(crate) struct SessionState {
    conn: noq::Connection,
    /// The CONNECT stream id (= the WebTransport session id).
    session_id: u64,
    protocol: Option<String>,
    closed_tx: watch::Sender<Option<WtError>>,
    connect_send: tokio::sync::Mutex<Option<ConnectSend>>,
    /// The runtime the connection is served on (the front door's). Every
    /// task this session spawns runs there, whichever runtime or thread
    /// calls [`WtSession::close`] or drops the last handle.
    runtime: tokio::runtime::Handle,
}

impl SessionState {
    /// Marks the session closed (first reason wins). Returns true if this
    /// call closed it.
    pub(crate) fn mark_closed(&self, err: WtError) -> bool {
        self.closed_tx.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            *state = Some(err);
            true
        })
    }

    fn closed_now(&self) -> Option<WtError> {
        self.closed_tx.borrow().clone()
    }

    async fn wait_closed(&self) -> WtError {
        let mut rx = self.closed_tx.subscribe();
        let res = match rx.wait_for(Option::is_some).await {
            Ok(state) => state
                .clone()
                .unwrap_or_else(|| WtError::SessionGone("closed".into())),
            Err(_) => WtError::SessionGone("session state dropped".into()),
        };
        res
    }

    /// Sends `CLOSE_WEBTRANSPORT_SESSION` on the CONNECT stream and finishes
    /// it (best effort, bounded), on the connection's runtime: callable from
    /// any runtime or from a plain thread.
    fn send_close(self: &Arc<Self>, code: u32, reason: &str) {
        let capsule = Bytes::from(capsule::encode_close(code, reason));
        let state = Arc::clone(self);
        self.runtime.spawn(async move {
            let mut guard = state.connect_send.lock().await;
            if let Some(mut send) = guard.take() {
                // Bounded: a peer that never grants flow-control credit
                // cannot pin this task.
                let write = async {
                    send.send_data(capsule).await?;
                    send.finish().await
                };
                if tokio::time::timeout(std::time::Duration::from_secs(5), write)
                    .await
                    .is_err()
                {
                    send.stop_stream(h3::error::Code::H3_NO_ERROR);
                }
            }
        });
    }
}

/// The receive side of the session's bounded queues.
struct Queues {
    bi: tokio::sync::Mutex<mpsc::Receiver<QueuedBi>>,
    uni: tokio::sync::Mutex<mpsc::Receiver<QueuedUni>>,
}

/// One per session; the last [`WtSession`] handle dropping closes it.
struct Handle {
    state: Arc<SessionState>,
    queues: Queues,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.state.mark_closed(WtError::SessionClosed {
            code: 0,
            reason: String::new(),
        }) {
            self.state.send_close(0, "");
        }
    }
}

/// A WebTransport session (one per connection, design note section 5).
///
/// Cheap to clone; the session closes (`CLOSE_WEBTRANSPORT_SESSION` code 0)
/// when the last clone is dropped.
#[derive(Clone)]
pub struct WtSession {
    inner: Arc<Handle>,
}

impl std::fmt::Debug for WtSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WtSession")
            .field("session_id", &self.inner.state.session_id)
            .field("protocol", &self.inner.state.protocol)
            .finish_non_exhaustive()
    }
}

/// The server's side of a new session: the queue senders the router feeds
/// and the CONNECT stream's receive half the driver reads capsules from.
pub(crate) struct SessionParts {
    pub(crate) session: WtSession,
    pub(crate) state: Arc<SessionState>,
    pub(crate) bi_tx: mpsc::Sender<QueuedBi>,
    pub(crate) uni_tx: mpsc::Sender<QueuedUni>,
}

impl WtSession {
    pub(crate) fn create(
        runtime: tokio::runtime::Handle,
        conn: noq::Connection,
        session_id: u64,
        protocol: Option<String>,
        connect_send: ConnectSend,
        bi_queue: usize,
        uni_queue: usize,
    ) -> SessionParts {
        let (bi_tx, bi_rx) = mpsc::channel(bi_queue.max(1));
        let (uni_tx, uni_rx) = mpsc::channel(uni_queue.max(1));
        let state = Arc::new(SessionState {
            conn,
            session_id,
            protocol,
            closed_tx: watch::channel(None).0,
            connect_send: tokio::sync::Mutex::new(Some(connect_send)),
            runtime,
        });
        let session = Self {
            inner: Arc::new(Handle {
                state: Arc::clone(&state),
                queues: Queues {
                    bi: tokio::sync::Mutex::new(bi_rx),
                    uni: tokio::sync::Mutex::new(uni_rx),
                },
            }),
        };
        SessionParts {
            session,
            state,
            bi_tx,
            uni_tx,
        }
    }

    /// The session id (the CONNECT stream id).
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.inner.state.session_id
    }

    fn header(&self, ty: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        varint::encode(ty, &mut out);
        varint::encode(self.inner.state.session_id, &mut out);
        out
    }

    /// The datagram context prefix: the quarter stream id of the CONNECT
    /// stream (RFC 9297 section 2.1, draft-ietf-webtrans-http3 section 4.5),
    /// which is what browsers send and expect.
    ///
    /// Interop note: `web-transport-moq` 1.3.5 (moq-tokio's own client)
    /// prefixes datagrams with the FULL CONNECT stream id, so its datagrams
    /// and ours do not round-trip until its prefix matches the draft. The
    /// moq-transport-16/18 versions this deployment uses deliver objects on
    /// streams, not datagrams, so nothing depends on it today; do not change
    /// this framing to match that client (browsers would break).
    fn quarter_id(&self) -> u64 {
        self.inner.state.session_id / 4
    }

    async fn race<T>(&self, fut: impl Future<Output = Result<T, WtError>>) -> Result<T, WtError> {
        if let Some(err) = self.inner.state.closed_now() {
            return Err(err);
        }
        tokio::select! {
            res = fut => res,
            err = self.inner.state.wait_closed() => Err(err),
        }
    }
}

fn map_connection_error(err: &noq::ConnectionError) -> WtError {
    WtError::SessionGone(err.to_string())
}

fn map_write_error(err: noq::WriteError) -> WtError {
    match err {
        noq::WriteError::Stopped(code) => {
            WtError::StreamStopped(error_from_http3(code.into_inner()).unwrap_or(0))
        }
        noq::WriteError::ConnectionLost(e) => map_connection_error(&e),
        noq::WriteError::ClosedStream | noq::WriteError::ZeroRttRejected => WtError::ClosedStream,
    }
}

fn map_read_error(err: noq::ReadError) -> WtError {
    match err {
        noq::ReadError::Reset(code) => {
            WtError::StreamReset(error_from_http3(code.into_inner()).unwrap_or(0))
        }
        noq::ReadError::ConnectionLost(e) => map_connection_error(&e),
        noq::ReadError::ClosedStream | noq::ReadError::ZeroRttRejected => WtError::ClosedStream,
    }
}

fn wt_code(code: u32) -> noq::VarInt {
    noq::VarInt::from_u64(error_to_http3(code)).unwrap_or(noq::VarInt::MAX)
}

impl web_transport_trait::Session for WtSession {
    type SendStream = WtSendStream;
    type RecvStream = WtRecvStream;
    type Error = WtError;

    async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
        self.race(async {
            let mut rx = self.inner.queues.uni.lock().await;
            match rx.recv().await {
                Some((recv, prefix)) => Ok(WtRecvStream::new(recv, prefix, &self.inner.state)),
                None => Err(WtError::SessionGone("stream queue closed".into())),
            }
        })
        .await
    }

    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        self.race(async {
            let mut rx = self.inner.queues.bi.lock().await;
            match rx.recv().await {
                Some((send, recv, prefix)) => Ok((
                    WtSendStream::new(send, &self.inner.state),
                    WtRecvStream::new(recv, prefix, &self.inner.state),
                )),
                None => Err(WtError::SessionGone("stream queue closed".into())),
            }
        })
        .await
    }

    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        let header = self.header(STREAM_BIDI);
        self.race(async {
            let (mut send, recv) = self
                .inner
                .state
                .conn
                .open_bi()
                .await
                .map_err(|e| map_connection_error(&e))?;
            send.write_all(&header).await.map_err(map_write_error)?;
            Ok((
                WtSendStream::new(send, &self.inner.state),
                WtRecvStream::new(recv, None, &self.inner.state),
            ))
        })
        .await
    }

    async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
        let header = self.header(STREAM_UNI);
        self.race(async {
            let mut send = self
                .inner
                .state
                .conn
                .open_uni()
                .await
                .map_err(|e| map_connection_error(&e))?;
            send.write_all(&header).await.map_err(map_write_error)?;
            Ok(WtSendStream::new(send, &self.inner.state))
        })
        .await
    }

    fn send_datagram(&self, payload: Bytes) -> Result<(), Self::Error> {
        if let Some(err) = self.inner.state.closed_now() {
            return Err(err);
        }
        let mut buf = Vec::with_capacity(8 + payload.len());
        varint::encode(self.quarter_id(), &mut buf);
        buf.extend_from_slice(&payload);
        self.inner
            .state
            .conn
            .send_datagram(Bytes::from(buf))
            .map_err(|e| match e {
                noq::SendDatagramError::UnsupportedByPeer | noq::SendDatagramError::Disabled => {
                    WtError::Datagram("datagrams not available")
                }
                noq::SendDatagramError::TooLarge => WtError::Datagram("datagram too large"),
                noq::SendDatagramError::ConnectionLost(e) => map_connection_error(&e),
            })
    }

    async fn recv_datagram(&self) -> Result<Bytes, Self::Error> {
        let quarter = self.quarter_id();
        self.race(async {
            loop {
                let datagram = self
                    .inner
                    .state
                    .conn
                    .read_datagram()
                    .await
                    .map_err(|e| map_connection_error(&e))?;
                // Datagrams for another (or no) session are dropped; noq's
                // receive buffer is the only queue (bounded, drops oldest).
                if let Some((id, len)) = varint::decode(&datagram) {
                    if id == quarter {
                        let mut datagram = datagram;
                        datagram.advance(len);
                        return Ok(datagram);
                    }
                }
            }
        })
        .await
    }

    fn max_datagram_size(&self) -> usize {
        let prefix = varint::size(self.quarter_id()).unwrap_or(8);
        self.inner
            .state
            .conn
            .max_datagram_size()
            .unwrap_or(0)
            .saturating_sub(prefix)
    }

    fn protocol(&self) -> Option<&str> {
        self.inner.state.protocol.as_deref()
    }

    fn close(&self, code: u32, reason: &str) {
        let reason = capsule::truncate_reason(reason);
        if self.inner.state.mark_closed(WtError::SessionClosed {
            code,
            reason: reason.to_owned(),
        }) {
            self.inner.state.send_close(code, reason);
        }
    }

    async fn closed(&self) -> Self::Error {
        self.inner.state.wait_closed().await
    }
}

/// Outgoing WebTransport stream. Reset (code 0) on drop unless finished.
pub struct WtSendStream {
    stream: noq::SendStream,
    state: Arc<SessionState>,
    finished: bool,
}

impl WtSendStream {
    fn new(stream: noq::SendStream, state: &Arc<SessionState>) -> Self {
        Self {
            stream,
            state: Arc::clone(state),
            finished: false,
        }
    }

    fn check_session(&mut self) -> Result<(), WtError> {
        if let Some(err) = self.state.closed_now() {
            let _ = self
                .stream
                .reset(noq::VarInt::from_u64(SESSION_GONE).unwrap_or(noq::VarInt::MAX));
            self.finished = true;
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for WtSendStream {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.stream.reset(wt_code(0));
        }
    }
}

impl web_transport_trait::SendStream for WtSendStream {
    type Error = WtError;

    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.check_session()?;
        // `tokio::select!` between `self.stream.write(buf)` and the close
        // watch, even `biased`, can lose or duplicate bytes: noq's `write`
        // commits bytes into the QUIC send buffer synchronously inside
        // `execute_poll` (moq-noq-1.3.5 send_stream.rs) on the very poll
        // that returns `Ready`, and `proto::Send::write`/`write_source`
        // (moq-noq-proto-1.3.5 connection/streams/send.rs and send.rs) is
        // all-or-nothing per poll: a poll either writes (returns `Ready`
        // with the committed count) or returns `Err(Blocked)` with no
        // write attempted at all — there is no state where a poll both
        // commits bytes and resolves `Pending`. `select!` would still
        // drop a `Ready(Ok(n))` outcome if the close branch happened to
        // win the race in the same `poll()` round, reporting the session
        // closed while `n` bytes were in fact already sent.
        //
        // `SendStream::poll_write` is itself poll-based, so poll it
        // directly and give it priority: report any `Ready` outcome
        // immediately (every committed byte is observed), and only look
        // at the session's close state when the stream poll is `Pending`
        // (by the invariant above, nothing was committed in that case),
        // which makes the two outcomes mutually exclusive by construction
        // rather than by race-loser luck.
        let stream = &mut self.stream;
        let state = &self.state;
        let result: Result<usize, WtError> = poll_fn(|cx| {
            if let Poll::Ready(res) = Pin::new(&mut *stream).poll_write(cx, buf) {
                return Poll::Ready(res.map_err(map_write_error));
            }
            let closed = state.wait_closed();
            let mut closed = std::pin::pin!(closed);
            closed.as_mut().poll(cx).map(Err)
        })
        .await;
        if result.is_err() {
            let _ = self
                .stream
                .reset(noq::VarInt::from_u64(SESSION_GONE).unwrap_or(noq::VarInt::MAX));
            self.finished = true;
        }
        result
    }

    fn set_priority(&mut self, order: u8) {
        let _ = self.stream.set_priority(i32::from(order));
    }

    fn finish(&mut self) -> Result<(), Self::Error> {
        self.check_session()?;
        self.finished = true;
        self.stream.finish().map_err(|_| WtError::ClosedStream)
    }

    fn reset(&mut self, code: u32) {
        self.finished = true;
        let _ = self.stream.reset(wt_code(code));
    }

    async fn closed(&mut self) -> Result<(), Self::Error> {
        let state = Arc::clone(&self.state);
        tokio::select! {
            res = self.stream.stopped() => match res {
                Ok(None) => Ok(()),
                Ok(Some(code)) => Err(WtError::StreamStopped(error_from_http3(code.into_inner()).unwrap_or(0))),
                Err(noq::StoppedError::ConnectionLost(e)) => Err(map_connection_error(&e)),
                Err(noq::StoppedError::ZeroRttRejected) => Err(WtError::ClosedStream),
            },
            err = state.wait_closed() => Err(err),
        }
    }
}

/// Incoming WebTransport stream. Stopped (code 0) on drop by noq.
pub struct WtRecvStream {
    stream: noq::RecvStream,
    prefix: Option<Bytes>,
    state: Arc<SessionState>,
}

impl WtRecvStream {
    fn new(stream: noq::RecvStream, prefix: Option<Bytes>, state: &Arc<SessionState>) -> Self {
        Self {
            stream,
            prefix: prefix.filter(|p| !p.is_empty()),
            state: Arc::clone(state),
        }
    }

    fn check_session(&mut self) -> Result<(), WtError> {
        if let Some(err) = self.state.closed_now() {
            let _ = self
                .stream
                .stop(noq::VarInt::from_u64(SESSION_GONE).unwrap_or(noq::VarInt::MAX));
            return Err(err);
        }
        Ok(())
    }
}

impl WtRecvStream {
    /// One chunk from the QUIC stream, at most `max` bytes; `None` is the FIN.
    ///
    /// The stream is polled first and the session-close watch only when it
    /// is `Pending`. noq's `ReadChunk::poll` removes the chunk from the
    /// receive buffer in the same poll that returns `Ready` (moq-noq 1.3.5
    /// `recv_stream.rs` `poll_read_chunk` -> `Assembler::read`) and consumes
    /// nothing when it returns `Pending`; a `select!` could drop a `Ready`
    /// chunk in favour of the close branch and lose it. A fresh `ReadChunk`
    /// per poll is equivalent to one long-lived future: it holds no state.
    async fn next_chunk(&mut self, max: usize) -> Result<Option<Bytes>, WtError> {
        let stream = &mut self.stream;
        let state = &self.state;
        let result: Result<Option<Bytes>, WtError> = poll_fn(|cx| {
            let read = stream.read_chunk(max);
            let mut read = std::pin::pin!(read);
            if let Poll::Ready(res) = read.as_mut().poll(cx) {
                return Poll::Ready(res.map_err(map_read_error));
            }
            let closed = state.wait_closed();
            let mut closed = std::pin::pin!(closed);
            closed.as_mut().poll(cx).map(Err)
        })
        .await;
        if result.is_err() {
            let _ = self
                .stream
                .stop(noq::VarInt::from_u64(SESSION_GONE).unwrap_or(noq::VarInt::MAX));
        }
        result
    }
}

impl web_transport_trait::RecvStream for WtRecvStream {
    type Error = WtError;

    async fn read(&mut self, dst: &mut [u8]) -> Result<Option<usize>, Self::Error> {
        if let Some(mut prefix) = self.prefix.take() {
            let n = prefix.len().min(dst.len());
            if let (Some(out), Some(src)) = (dst.get_mut(..n), prefix.get(..n)) {
                out.copy_from_slice(src);
            }
            prefix.advance(n);
            if !prefix.is_empty() {
                self.prefix = Some(prefix);
            }
            return Ok(Some(n));
        }
        self.check_session()?;
        if dst.is_empty() {
            return Ok(Some(0));
        }
        // Read through `read_chunk` so a FIN (`None`) is never confused with
        // a zero-length read: noq's `poll_read` reports both as `Ok(0)`.
        // (moq-noq-proto 1.3.5's assembler never stores an empty chunk, so
        // an empty `Some` is skipped defensively, not expected.)
        loop {
            match self.next_chunk(dst.len()).await? {
                None => return Ok(None),
                Some(chunk) if chunk.is_empty() => {}
                Some(chunk) => {
                    let n = chunk.len().min(dst.len());
                    if let (Some(out), Some(src)) = (dst.get_mut(..n), chunk.get(..n)) {
                        out.copy_from_slice(src);
                    }
                    return Ok(Some(n));
                }
            }
        }
    }

    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, Self::Error> {
        if let Some(mut prefix) = self.prefix.take() {
            let chunk = prefix.split_to(max.min(prefix.len()));
            if !prefix.is_empty() {
                self.prefix = Some(prefix);
            }
            return Ok(Some(chunk));
        }
        self.check_session()?;
        self.next_chunk(max).await
    }

    fn stop(&mut self, code: u32) {
        let _ = self.stream.stop(wt_code(code));
    }

    async fn closed(&mut self) -> Result<(), Self::Error> {
        let state = Arc::clone(&self.state);
        tokio::select! {
            res = self.stream.received_reset() => match res {
                Ok(None) => Ok(()),
                Ok(Some(code)) => Err(WtError::StreamReset(error_from_http3(code.into_inner()).unwrap_or(0))),
                Err(noq::ResetError::ConnectionLost(e)) => Err(map_connection_error(&e)),
                Err(noq::ResetError::ZeroRttRejected) => Err(WtError::ClosedStream),
            },
            err = state.wait_closed() => Err(err),
        }
    }
}

/// Reads the CONNECT stream's capsules until the session ends; marks the
/// session closed with the peer's reason (or "gone" on FIN/reset/error).
/// Holds at most one capsule body ([`capsule::MAX_KNOWN_CAPSULE`]).
pub(crate) async fn drive_connect_stream(mut recv: ConnectRecv, state: Arc<SessionState>) {
    let mut parser = CapsuleParser::new();
    let mut capsules = Vec::new();
    let outcome = loop {
        let data = tokio::select! {
            data = recv.recv_data() => data,
            // Closed locally (close() or the last handle dropped): stop
            // reading; the close capsule is sent by `send_close`.
            _ = state.wait_closed() => break None,
        };
        match data {
            Ok(Some(mut buf)) => {
                let bytes = buf.copy_to_bytes(buf.remaining());
                if let Err(err) = parser.feed(&bytes, &mut capsules) {
                    recv.stop_sending(h3::error::Code::H3_MESSAGE_ERROR);
                    break Some(WtError::SessionGone(err.to_string()));
                }
                if let Some(Capsule::Close { code, reason }) = capsules
                    .drain(..)
                    .find(|c| matches!(c, Capsule::Close { .. }))
                {
                    break Some(WtError::SessionClosed { code, reason });
                }
            }
            Ok(None) => break Some(WtError::SessionGone("CONNECT stream finished".into())),
            Err(err) => break Some(WtError::SessionGone(err.to_string())),
        }
    };
    if let Some(err) = outcome {
        state.mark_closed(err);
        // The peer ended the session: finish our side of the CONNECT stream.
        let mut guard = state.connect_send.lock().await;
        if let Some(mut send) = guard.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), send.finish()).await;
        }
    }
}

/// `PoisonError` mapping for the std mutexes in this crate: a poisoned lock
/// only means another task panicked mid-update; the protected data here is
/// always left consistent, so recover the guard.
pub(crate) fn unpoison<T>(res: Result<T, PoisonError<T>>) -> T {
    res.unwrap_or_else(PoisonError::into_inner)
}

/// A std mutex alias used by the router.
pub(crate) type StdMutex<T> = Mutex<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_mapping_round_trips() {
        for code in [0u32, 1, 29, 30, 31, 1000, u32::MAX] {
            assert_eq!(error_from_http3(error_to_http3(code)), Some(code));
        }
        assert_eq!(error_from_http3(0x100), None);
    }

    #[test]
    fn protocol_list_parsing_and_selection() {
        // Parameters are ignored structurally, also when they contain commas
        // or protocol-looking strings.
        assert_eq!(
            parse_protocol_list(r#""moq-lite-03";q=1, "moqt-15""#),
            Some(vec!["moq-lite-03".to_owned(), "moqt-15".to_owned()])
        );
        assert_eq!(
            parse_protocol_list(r#""other";p=",moqt-16,""#),
            Some(vec!["other".to_owned()])
        );
        assert_eq!(
            parse_protocol_list(r#""a\"b""#),
            Some(vec!["a\"b".to_owned()])
        );
        // Malformed or non-String members: the whole field is ignored.
        for bad in [
            r#""other", "unterminated"#,
            r#""a", tok"#,
            r#"("a" "b")"#,
            "1",
            "",
        ] {
            assert_eq!(
                parse_protocol_list(bad).filter(|l| !l.is_empty()),
                None,
                "{bad}"
            );
        }
        let many = vec!["\"x\""; MAX_OFFERED + 1].join(", ");
        assert_eq!(parse_protocol_list(&many), None);

        let headers = |lines: &[&'static str]| {
            let mut h = http::HeaderMap::new();
            for l in lines {
                h.append(AVAILABLE_PROTOCOLS, http::HeaderValue::from_static(l));
            }
            h
        };
        let supported = ["moq-lite-03", "moqt-15"];
        // The client's preference order wins.
        assert_eq!(
            negotiate(&headers(&[r#""x", "moqt-15", "moq-lite-03""#]), &supported),
            Selection::Selected("moqt-15")
        );
        assert_eq!(
            select_protocol(&headers(&[r#""x", "moqt-15""#]), &supported),
            Some("moqt-15")
        );
        assert_eq!(
            negotiate(&headers(&[r#""x""#]), &supported),
            Selection::NoMatch
        );
        // A value only inside a parameter is never selected.
        assert_eq!(
            negotiate(&headers(&[r#""other";p=",moqt-15,""#]), &supported),
            Selection::NoMatch
        );
        // Several field lines form one list; a malformed line spoils the field.
        assert_eq!(
            negotiate(&headers(&[r#""x""#, r#""moqt-15""#]), &supported),
            Selection::Selected("moqt-15")
        );
        assert_eq!(
            negotiate(&headers(&[r#""moqt-15""#, r#""bad"#]), &supported),
            Selection::NoOffer
        );
        assert_eq!(
            negotiate(&http::HeaderMap::new(), &supported),
            Selection::NoOffer
        );
        // RFC 9110 section 5.3: an empty field line still takes its
        // separator, so `, "moqt-15"` / `"moqt-15", ` are malformed and the
        // whole field is ignored.
        assert_eq!(
            negotiate(&headers(&["", r#""moqt-15""#]), &supported),
            Selection::NoOffer
        );
        assert_eq!(
            negotiate(&headers(&[r#""moqt-15""#, ""]), &supported),
            Selection::NoOffer
        );
        // Legacy name only without the standard one.
        let mut legacy = http::HeaderMap::new();
        legacy.insert(
            AVAILABLE_PROTOCOLS_LEGACY,
            http::HeaderValue::from_static(r#""moqt-15""#),
        );
        assert_eq!(
            negotiate(&legacy, &supported),
            Selection::Selected("moqt-15")
        );
        legacy.insert(
            AVAILABLE_PROTOCOLS,
            http::HeaderValue::from_static(r#""x""#),
        );
        assert_eq!(negotiate(&legacy, &supported), Selection::NoMatch);
        assert_eq!(encode_sf_string("moq"), Some("\"moq\"".into()));
        let resp = connect_response(Some("moqt-15")).unwrap();
        assert_eq!(resp.headers()[SELECTED_PROTOCOL], "\"moqt-15\"");
        assert_eq!(resp.headers()[DRAFT_HEADER], DRAFT_VALUE);
    }
}
