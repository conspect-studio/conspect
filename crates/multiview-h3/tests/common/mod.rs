//! Shared harness: a real noq endpoint on `[::1]:0` with a self-signed
//! certificate, and an h3 client over the same noq adapter.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use multiview_h3::noq_adapter::{self, StreamRouter};
use multiview_h3::{
    EndpointParts, H3Endpoint, H3Limits, PeersHook, RequestBody, ResponseBody, Site, WtHandler,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

pub const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Cert {
    pub cert: rustls::pki_types::CertificateDer<'static>,
    pub key: rustls::pki_types::PrivateKeyDer<'static>,
}

pub fn cert() -> Cert {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    Cert {
        cert: ck.cert.der().clone(),
        key: rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into()),
    }
}

pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_tls(cert: &Cert) -> rustls::ServerConfig {
    rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.clone()], cert.key.clone_key())
        .unwrap()
}

pub fn client_config(cert: &Cert, alpn: &[u8]) -> noq::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.clone()).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let crypto = noq::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).unwrap();
    noq::ClientConfig::new(Arc::new(crypto))
}

pub struct Server {
    pub endpoint: H3Endpoint,
    pub addr: SocketAddr,
    pub cert: Cert,
}

pub async fn start(
    site: Arc<dyn Site>,
    wt: Option<Arc<dyn WtHandler>>,
    limits: H3Limits,
    peers: Option<PeersHook>,
) -> Server {
    let cert = cert();
    let socket = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let socket = noq::Runtime::wrap_udp_socket(&noq::TokioRuntime, socket).unwrap();
    let endpoint = H3Endpoint::start(EndpointParts {
        socket,
        endpoint: noq::EndpointConfig::default(),
        incoming: (64, 4 * 1024 * 1024),
        tls: Arc::new(server_tls(&cert)),
        runtime: tokio::runtime::Handle::current(),
        site,
        wt,
        limits,
        peers,
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    Server {
        endpoint,
        addr,
        cert,
    }
}

/// A plain QUIC connection to the server.
pub async fn quic_connect(
    server: &Server,
    alpn: &[u8],
) -> Result<noq::Connection, noq::ConnectionError> {
    let client = noq::Endpoint::client("[::1]:0".parse().unwrap()).unwrap();
    client.set_default_client_config(client_config(&server.cert, alpn));
    let connecting = client.connect(server.addr, "localhost").unwrap();
    tokio::time::timeout(TIMEOUT, connecting)
        .await
        .expect("connect timed out")
}

/// WebTransport streams the server opened toward the client.
pub type UniItem = (u64, noq::RecvStream, Option<Bytes>);

pub struct ClientRouter {
    pub uni: Mutex<Option<mpsc::UnboundedSender<UniItem>>>,
    permits: Arc<Semaphore>,
}

impl StreamRouter for ClientRouter {
    fn admit_request(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).try_acquire_owned().ok()
    }
    fn webtransport_bi(&self, _: u64, _: noq::SendStream, _: noq_adapter::RecvStream) {}
    fn webtransport_uni(&self, session_id: u64, recv: noq_adapter::RecvStream) {
        let (Some(raw), prefix) = recv.into_raw() else {
            return;
        };
        if let Some(tx) = self.uni.lock().unwrap().as_ref() {
            let _ = tx.send((session_id, raw, prefix));
        }
    }
}

pub type SendRequest = h3::client::SendRequest<noq_adapter::OpenStreams, Bytes>;
pub type ClientStream = h3::client::RequestStream<noq_adapter::BidiStream<Bytes>, Bytes>;

pub struct Client {
    pub conn: noq::Connection,
    pub send: SendRequest,
    pub uni: mpsc::UnboundedReceiver<UniItem>,
    driver: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

pub async fn h3_client(server: &Server) -> Client {
    let conn = quic_connect(server, b"h3").await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let router = Arc::new(ClientRouter {
        uni: Mutex::new(Some(tx)),
        permits: Arc::new(Semaphore::new(1024)),
    });
    let quic = noq_adapter::Connection::new(conn.clone()).with_router(router);
    let (mut driver, send) = h3::client::builder()
        .enable_extended_connect(true)
        .enable_datagram(true)
        .build::<_, _, Bytes>(quic)
        .await
        .unwrap();
    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    Client {
        conn,
        send,
        uni: rx,
        driver,
    }
}

pub fn request(method: http::Method, path: &str) -> http::Request<()> {
    http::Request::builder()
        .method(method)
        .uri(format!("https://localhost{path}"))
        .body(())
        .unwrap()
}

pub fn connect_request(path: &str, protocols: Option<&str>) -> http::Request<()> {
    let mut req = request(http::Method::CONNECT, path);
    req.extensions_mut()
        .insert(h3::ext::Protocol::WEB_TRANSPORT);
    if let Some(p) = protocols {
        req.headers_mut().insert(
            "wt-available-protocols",
            http::HeaderValue::from_str(p).unwrap(),
        );
    }
    req
}

/// Sends a body-less request and reads the full response.
pub async fn fetch(
    send: &mut SendRequest,
    req: http::Request<()>,
) -> (http::Response<()>, Vec<u8>) {
    let fut = async {
        let mut stream = send.send_request(req).await.unwrap();
        stream.finish().await.unwrap();
        let resp = stream.recv_response().await.unwrap();
        let body = read_body(&mut stream).await;
        (resp, body)
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("fetch timed out")
}

pub async fn read_body(stream: &mut ClientStream) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        body.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
    }
    body
}

/// What the test site saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: http::Method,
    pub uri: http::Uri,
    pub version: http::Version,
    pub headers: http::HeaderMap,
    pub peer: SocketAddr,
}

/// A site that records requests, echoes POST bodies, adds connection
/// headers to every response (they must be stripped), and blocks on
/// `/block` until released.
pub struct TestSite {
    pub seen: Mutex<Vec<Seen>>,
    pub gate: Arc<Semaphore>,
    pub blocked: Arc<std::sync::atomic::AtomicUsize>,
}

impl TestSite {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            gate: Arc::new(Semaphore::new(0)),
            blocked: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }
}

pub fn full_body(data: impl Into<Bytes>) -> ResponseBody {
    http_body_util::Full::new(data.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

impl Site for TestSite {
    fn call(
        &self,
        req: http::Request<RequestBody>,
        peer: SocketAddr,
    ) -> multiview_h3::BoxFuture<'static, http::Response<ResponseBody>> {
        self.seen.lock().unwrap().push(Seen {
            method: req.method().clone(),
            uri: req.uri().clone(),
            version: req.version(),
            headers: req.headers().clone(),
            peer,
        });
        let gate = Arc::clone(&self.gate);
        let blocked = Arc::clone(&self.blocked);
        Box::pin(async move {
            let path = req.uri().path().to_owned();
            if path == "/block" {
                blocked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = gate.acquire().await.unwrap();
            }
            let body = if req.method() == http::Method::POST {
                // Stream the body through, chunk by chunk.
                let mut body = req.into_body();
                let mut out = Vec::new();
                let mut chunks = 0usize;
                while let Some(frame) = body.frame().await {
                    let frame = frame.unwrap();
                    if let Ok(data) = frame.into_data() {
                        chunks += 1;
                        out.extend_from_slice(&data);
                    }
                }
                assert!(chunks > 1, "the body arrived in one piece");
                out
            } else {
                format!("hello {path}").into_bytes()
            };
            http::Response::builder()
                .status(200)
                .header("connection", "close")
                .header("keep-alive", "timeout=5")
                .header("transfer-encoding", "chunked")
                .header("upgrade", "h2c")
                .header("proxy-connection", "x")
                .header("x-site", "yes")
                .body(full_body(body))
                .unwrap()
        })
    }
}
