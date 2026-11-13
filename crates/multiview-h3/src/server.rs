//! The connection driver: one h3 server connection per QUIC connection,
//! page requests to the [`Site`], WebTransport CONNECT under the prefix to
//! the [`WtHandler`].

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use bytes::{Buf, Bytes};
use h3::error::Code;
use h3::ext::Protocol;
use h3::server::RequestStream;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::config::H3Limits;
use crate::counters::Counters;
use crate::noq_adapter::{self, GuardConfig, StreamRouter};
use crate::webtransport::{
    self, QueuedBi, QueuedUni, StdMutex, WtSession, BUFFERED_STREAM_REJECTED, SESSION_GONE,
};
use crate::{BoxError, Error, ResponseBody, Site, WtHandler};

type H3Conn = h3::server::Connection<noq_adapter::Connection, Bytes>;
type H3Stream = RequestStream<noq_adapter::BidiStream<Bytes>, Bytes>;
type H3Send = RequestStream<noq_adapter::SendStream<Bytes>, Bytes>;
type H3Recv = RequestStream<noq_adapter::RecvStream, Bytes>;

/// Response headers that are connection-specific in HTTP/1.1 and MUST NOT
/// appear in an HTTP/3 message (RFC 9114 section 4.2).
pub const CONNECTION_SPECIFIC: [&str; 5] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
];

/// The receive half of a request stream, type-erased so the request body
/// does not name the transport (the fuzz harness uses an in-memory one).
trait RecvSide: Send {
    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, BoxError>>;
    fn poll_trailers(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, BoxError>>;
}

impl<S> RecvSide for RequestStream<S, Bytes>
where
    S: h3::quic::RecvStream + Send,
{
    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, BoxError>> {
        match ready!(self.poll_recv_data(cx)) {
            Ok(Some(mut buf)) => Poll::Ready(Ok(Some(buf.copy_to_bytes(buf.remaining())))),
            Ok(None) => Poll::Ready(Ok(None)),
            Err(err) => Poll::Ready(Err(Box::new(err))),
        }
    }

    fn poll_trailers(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, BoxError>> {
        self.poll_recv_trailers(cx)
            .map_err(|err| -> BoxError { Box::new(err) })
    }
}

/// The h3 request body: streams DATA frames on demand (one chunk per poll,
/// nothing buffered beyond it; backpressure is QUIC flow control), then the
/// trailers if any.
pub struct RequestBody {
    stream: Option<Box<dyn RecvSide>>,
    data_done: bool,
}

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestBody")
            .field("open", &self.stream.is_some())
            .field("data_done", &self.data_done)
            .finish()
    }
}

impl RequestBody {
    pub(crate) fn new<S>(stream: RequestStream<S, Bytes>) -> Self
    where
        S: h3::quic::RecvStream + Send + 'static,
    {
        Self {
            stream: Some(Box::new(stream)),
            data_done: false,
        }
    }

    /// An empty body.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            stream: None,
            data_done: true,
        }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let Some(stream) = this.stream.as_mut() else {
            return Poll::Ready(None);
        };
        if !this.data_done {
            match ready!(stream.poll_data(cx)) {
                Ok(Some(data)) => return Poll::Ready(Some(Ok(Frame::data(data)))),
                Ok(None) => this.data_done = true,
                Err(err) => {
                    this.stream = None;
                    return Poll::Ready(Some(Err(err)));
                }
            }
        }
        let res = ready!(stream.poll_trailers(cx));
        this.stream = None;
        match res {
            Ok(Some(trailers)) => Poll::Ready(Some(Ok(Frame::trailers(trailers)))),
            Ok(None) => Poll::Ready(None),
            Err(err) => Poll::Ready(Some(Err(err))),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.stream.is_none()
    }
}

/// Where the connection's one WebTransport session stands.
enum Slot {
    /// No session yet.
    Free,
    /// A CONNECT is being admitted/answered; other `CONNECTs` get 429.
    Reserved,
    /// The session is live.
    Active {
        session_id: u64,
        bi_tx: mpsc::Sender<QueuedBi>,
        uni_tx: mpsc::Sender<QueuedUni>,
    },
    /// The session ended; the connection never carries another (429).
    Ended,
}

/// A WebTransport stream that arrived before its session was installed.
enum Early {
    Bi(u64, QueuedBi),
    Uni(u64, QueuedUni),
}

struct RouterState {
    slot: Slot,
    /// Bounded by `H3Limits::wt_early_streams`.
    early: Vec<Early>,
}

/// The per-connection [`StreamRouter`].
struct Router {
    requests: Arc<Semaphore>,
    counters: Arc<Counters>,
    early_cap: usize,
    state: StdMutex<RouterState>,
}

fn reset_code(code: u64) -> noq::VarInt {
    noq::VarInt::from_u64(code).unwrap_or(noq::VarInt::MAX)
}

fn reject_bi((mut send, mut recv, _): QueuedBi, code: u64) {
    let _ = send.reset(reset_code(code));
    let _ = recv.stop(reset_code(code));
}

fn reject_uni((mut recv, _): QueuedUni, code: u64) {
    let _ = recv.stop(reset_code(code));
}

fn reject_early(early: Early, code: u64) {
    match early {
        Early::Bi(_, bi) => reject_bi(bi, code),
        Early::Uni(_, uni) => reject_uni(uni, code),
    }
}

impl Router {
    fn lock(&self) -> std::sync::MutexGuard<'_, RouterState> {
        webtransport::unpoison(self.state.lock())
    }

    fn deliver_bi(&self, tx: &mpsc::Sender<QueuedBi>, bi: QueuedBi) {
        if let Err(err) = tx.try_send(bi) {
            Counters::inc(&self.counters.wt_streams_rejected);
            let code = match err {
                mpsc::error::TrySendError::Full(_) => BUFFERED_STREAM_REJECTED,
                mpsc::error::TrySendError::Closed(_) => SESSION_GONE,
            };
            reject_bi(err.into_inner(), code);
        }
    }

    fn deliver_uni(&self, tx: &mpsc::Sender<QueuedUni>, uni: QueuedUni) {
        if let Err(err) = tx.try_send(uni) {
            Counters::inc(&self.counters.wt_streams_rejected);
            let code = match err {
                mpsc::error::TrySendError::Full(_) => BUFFERED_STREAM_REJECTED,
                mpsc::error::TrySendError::Closed(_) => SESSION_GONE,
            };
            reject_uni(err.into_inner(), code);
        }
    }

    fn route(&self, session_id: u64, early: Early) {
        let mut state = self.lock();
        let early_len = state.early.len();
        match &state.slot {
            Slot::Active {
                session_id: id,
                bi_tx,
                uni_tx,
            } if *id == session_id => match early {
                Early::Bi(_, bi) => self.deliver_bi(bi_tx, bi),
                Early::Uni(_, uni) => self.deliver_uni(uni_tx, uni),
            },
            Slot::Free | Slot::Reserved if early_len < self.early_cap => state.early.push(early),
            Slot::Free | Slot::Reserved => {
                Counters::inc(&self.counters.wt_streams_rejected);
                reject_early(early, BUFFERED_STREAM_REJECTED);
            }
            Slot::Active { .. } | Slot::Ended => {
                Counters::inc(&self.counters.wt_streams_rejected);
                reject_early(early, SESSION_GONE);
            }
        }
    }

    /// Reserves the session slot. `Err(())` when a session exists/ended.
    fn reserve(&self) -> Result<(), ()> {
        let mut state = self.lock();
        if matches!(state.slot, Slot::Free) {
            state.slot = Slot::Reserved;
            Ok(())
        } else {
            Err(())
        }
    }

    /// Returns a reservation (the CONNECT was not admitted).
    fn release(&self) {
        let mut state = self.lock();
        if matches!(state.slot, Slot::Reserved) {
            state.slot = Slot::Free;
        }
    }

    fn install(
        &self,
        session_id: u64,
        bi_tx: mpsc::Sender<QueuedBi>,
        uni_tx: mpsc::Sender<QueuedUni>,
    ) {
        let mut state = self.lock();
        let early = std::mem::take(&mut state.early);
        for item in early {
            match item {
                Early::Bi(id, bi) if id == session_id => self.deliver_bi(&bi_tx, bi),
                Early::Uni(id, uni) if id == session_id => self.deliver_uni(&uni_tx, uni),
                other => {
                    Counters::inc(&self.counters.wt_streams_rejected);
                    reject_early(other, SESSION_GONE);
                }
            }
        }
        state.slot = Slot::Active {
            session_id,
            bi_tx,
            uni_tx,
        };
    }

    fn end(&self) {
        let mut state = self.lock();
        state.slot = Slot::Ended;
        for item in std::mem::take(&mut state.early) {
            reject_early(item, SESSION_GONE);
        }
    }
}

impl StreamRouter for Router {
    fn admit_request(&self) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.requests).try_acquire_owned().ok();
        if permit.is_none() {
            Counters::inc(&self.counters.requests_rejected);
        }
        permit
    }

    fn webtransport_bi(
        &self,
        session_id: u64,
        send: noq::SendStream,
        recv: noq_adapter::RecvStream,
    ) {
        let (Some(raw), prefix) = recv.into_raw() else {
            return;
        };
        self.route(session_id, Early::Bi(session_id, (send, raw, prefix)));
    }

    fn webtransport_uni(&self, session_id: u64, recv: noq_adapter::RecvStream) {
        let (Some(raw), prefix) = recv.into_raw() else {
            return;
        };
        self.route(session_id, Early::Uni(session_id, (raw, prefix)));
    }
}

/// Everything a request task needs.
struct Shared {
    conn: noq::Connection,
    peer: SocketAddr,
    site: Arc<dyn Site>,
    wt: Option<Arc<dyn WtHandler>>,
    limits: H3Limits,
    counters: Arc<Counters>,
    router: Arc<Router>,
}

/// Serves one established QUIC connection until it ends.
///
/// Each request runs in its own task; at most
/// `limits.max_concurrent_requests` are in flight (a request stream beyond
/// that is reset with `H3_REQUEST_REJECTED` before h3 reads it). The first
/// admitted WebTransport CONNECT under `limits.wt_prefix` becomes the
/// connection's session; while it lives, and after it ends, page requests
/// keep being served and any further extended CONNECT is answered 429 (one
/// session per connection for the connection's whole life: a client that
/// wants another session opens another connection, as the
/// `max_webtransport_sessions = 1` setting tells it).
///
/// # Errors
///
/// The h3 handshake failed or the connection closed with an error other
/// than `H3_NO_ERROR`.
pub async fn serve_connection(
    conn: noq::Connection,
    peer: SocketAddr,
    site: Arc<dyn Site>,
    wt: Option<Arc<dyn WtHandler>>,
    limits: &H3Limits,
    counters: Arc<Counters>,
) -> Result<(), Error> {
    let router = Arc::new(Router {
        requests: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
        counters: Arc::clone(&counters),
        early_cap: limits.wt_early_streams,
        state: StdMutex::new(RouterState {
            slot: Slot::Free,
            early: Vec::new(),
        }),
    });
    let quic = noq_adapter::Connection::with_guard(
        conn.clone(),
        GuardConfig {
            max_field_section_size: limits.settings.max_field_section_size,
            header_deadline: limits.header_deadline,
            counters: Arc::clone(&counters),
        },
    )
    .with_router(Arc::<Router>::clone(&router));
    let mut h3conn: H3Conn = limits
        .settings
        .builder()
        .build(quic)
        .await
        .map_err(Error::H3)?;
    let shared = Arc::new(Shared {
        conn,
        peer,
        site,
        wt,
        limits: limits.clone(),
        counters,
        router,
    });
    // Bounded: every task holds a request permit (its stream) or is the
    // session task, which holds the CONNECT stream's permit.
    let mut tasks = JoinSet::new();
    let result = loop {
        while tasks.try_join_next().is_some() {}
        match h3conn.accept().await {
            Ok(Some(resolver)) => {
                let shared = Arc::clone(&shared);
                tasks.spawn(async move {
                    match resolver.resolve_request().await {
                        Ok((req, stream)) => handle_request(shared, req, stream).await,
                        Err(err) => {
                            tracing::debug!(%err, "h3 request not resolved");
                        }
                    }
                });
            }
            Ok(None) => break Ok(()),
            Err(err) if err.is_h3_no_error() => break Ok(()),
            Err(err) => break Err(Error::H3(err)),
        }
    };
    // Keep the h3 connection (its drop closes the QUIC connection) until
    // in-flight requests have finished; they end with the connection anyway.
    while tasks.join_next().await.is_some() {}
    drop(h3conn);
    result
}

fn plain_response(status: http::StatusCode) -> http::Response<()> {
    let mut response = http::Response::new(());
    *response.status_mut() = status;
    response
}

async fn respond_and_finish(stream: &mut H3Stream, status: http::StatusCode) {
    if stream.send_response(plain_response(status)).await.is_ok() {
        let _ = stream.finish().await;
    }
}

fn under_prefix(path: &str, prefix: &str) -> bool {
    path.starts_with(prefix) || path == prefix.trim_end_matches('/')
}

async fn handle_request(shared: Arc<Shared>, req: http::Request<()>, mut stream: H3Stream) {
    if req.method() != http::Method::CONNECT {
        return serve_site(&shared, req, stream).await;
    }
    let Some(protocol) = req.extensions().get::<Protocol>().copied() else {
        // Plain CONNECT (a proxy tunnel): not served here.
        return respond_and_finish(&mut stream, http::StatusCode::METHOD_NOT_ALLOWED).await;
    };
    let Some(handler) = shared.wt.clone() else {
        return respond_and_finish(&mut stream, http::StatusCode::NOT_IMPLEMENTED).await;
    };
    if protocol != Protocol::WEB_TRANSPORT {
        return respond_and_finish(&mut stream, http::StatusCode::NOT_IMPLEMENTED).await;
    }
    if !under_prefix(req.uri().path(), shared.limits.wt_prefix) {
        return respond_and_finish(&mut stream, http::StatusCode::NOT_FOUND).await;
    }
    if shared.router.reserve().is_err() {
        Counters::inc(&shared.counters.wt_429);
        return respond_and_finish(&mut stream, http::StatusCode::TOO_MANY_REQUESTS).await;
    }
    let (parts, ()) = req.into_parts();
    let token = match handler.admit(&parts, shared.peer) {
        Ok(token) => token,
        Err(status) => {
            shared.router.release();
            return respond_and_finish(&mut stream, status).await;
        }
    };
    let selected = webtransport::select_protocol(&parts.headers, handler.protocols());
    let session_id = stream.send_id().into_inner();
    let (mut send, recv): (H3Send, H3Recv) = stream.split();
    let response = match webtransport::connect_response(selected) {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(%err, "building the CONNECT response");
            shared.router.release();
            send.stop_stream(Code::H3_INTERNAL_ERROR);
            return;
        }
    };
    if let Err(err) = send.send_response(response).await {
        tracing::debug!(%err, "sending the CONNECT response");
        shared.router.release();
        return;
    }
    let parts_for_session = webtransport::WtSession::create(
        shared.conn.clone(),
        session_id,
        selected.map(str::to_owned),
        send,
        shared.limits.wt_bidi_queue,
        shared.limits.wt_uni_queue,
    );
    shared.router.install(
        session_id,
        parts_for_session.bi_tx,
        parts_for_session.uni_tx,
    );
    Counters::inc(&shared.counters.wt_sessions);
    let session: WtSession = parts_for_session.session;
    let state = parts_for_session.state;
    let handler_fut = handler.accept(session, token, parts, shared.peer);
    let driver = webtransport::drive_connect_stream(recv, Arc::clone(&state));
    tokio::join!(handler_fut, driver);
    shared.router.end();
}

/// Converts the h3 request for the [`Site`]: `host` from `:authority` when
/// absent (h3 already moved the pseudo-headers into method/uri/extensions),
/// version HTTP/3.
pub(crate) fn site_request(
    req: http::Request<()>,
    body: RequestBody,
) -> http::Request<RequestBody> {
    let (mut parts, ()) = req.into_parts();
    if !parts.headers.contains_key(http::header::HOST) {
        if let Some(value) = parts
            .uri
            .authority()
            .and_then(|a| http::HeaderValue::from_str(a.as_str()).ok())
        {
            parts.headers.insert(http::header::HOST, value);
        }
    }
    parts.version = http::Version::HTTP_3;
    http::Request::from_parts(parts, body)
}

/// Removes the RFC 9114 section 4.2 connection-specific fields.
pub fn strip_connection_headers(headers: &mut http::HeaderMap) {
    for name in CONNECTION_SPECIFIC {
        headers.remove(name);
    }
}

async fn serve_site(shared: &Shared, req: http::Request<()>, stream: H3Stream) {
    Counters::inc(&shared.counters.requests);
    let (send, recv) = stream.split();
    let req = site_request(req, RequestBody::new(recv));
    let response = shared.site.call(req, shared.peer).await;
    if let Err(err) = send_site_response(send, response).await {
        tracing::debug!(%err, "h3 response not completed");
    }
}

async fn send_site_response(
    mut send: H3Send,
    response: http::Response<ResponseBody>,
) -> Result<(), BoxError> {
    let (mut parts, mut body) = response.into_parts();
    strip_connection_headers(&mut parts.headers);
    send.send_response(http::Response::from_parts(parts, ()))
        .await?;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) => {
                send.stop_stream(Code::H3_INTERNAL_ERROR);
                return Err(err);
            }
        };
        match frame.into_data() {
            Ok(data) => {
                if !data.is_empty() {
                    send.send_data(data).await?;
                }
            }
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    send.send_trailers(trailers).await?;
                }
            }
        }
    }
    send.finish().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matching() {
        assert!(under_prefix("/moq/x", "/moq/"));
        assert!(under_prefix("/moq", "/moq/"));
        assert!(!under_prefix("/moqx", "/moq/"));
        assert!(!under_prefix("/other", "/moq/"));
    }

    #[test]
    fn host_from_authority_and_stripping() {
        let req = http::Request::builder()
            .uri("https://example.test:8443/a?b")
            .body(())
            .unwrap();
        let req = site_request(req, RequestBody::empty());
        assert_eq!(req.headers()[http::header::HOST], "example.test:8443");
        assert_eq!(req.version(), http::Version::HTTP_3);

        let mut headers = http::HeaderMap::new();
        for name in CONNECTION_SPECIFIC {
            headers.insert(name, http::HeaderValue::from_static("x"));
        }
        headers.insert("x-keep", http::HeaderValue::from_static("y"));
        strip_connection_headers(&mut headers);
        assert_eq!(headers.len(), 1);
    }
}
