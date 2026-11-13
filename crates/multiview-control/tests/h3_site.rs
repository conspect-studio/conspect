//! Front-door slice 2 (#288): the control-plane router and `MoQ` over one
//! HTTP/3 endpoint ([`multiview_control::h3_site::H3Site`] +
//! [`multiview_control::moq_startup::MoqWt`]) on a real noq socket:
//!
//! * a page GET and a POST reach the axum router over h3 with `ConnectInfo`;
//! * `GET /api/v1/ws` over h3 (no `Upgrade` in HTTP/3) is a clean `4xx`;
//! * after the shutdown watch flips, new requests answer `503`;
//! * `MoQ`: a moq-tokio WebTransport client with an admitted `?ticket=`
//!   subscribes and reads the published frame; a wrong ticket and a reused
//!   ticket are `401` before any session exists.
#![cfg(feature = "moq-transport")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::http;
use axum::routing::{get, post};
use multiview_control::h3_site::H3Site;
use multiview_control::moq_startup::MoqWt;
use multiview_h3::client::{self, Client};
use multiview_h3::{EndpointParts, H3Endpoint, H3Limits, WtHandler};
use multiview_moq::provider::{Feed, Fence, Runtime};
use tokio_rustls::rustls;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Cert {
    cert: rustls::pki_types::CertificateDer<'static>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

fn cert() -> Cert {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    Cert {
        cert: ck.cert.der().clone(),
        key: rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into()),
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

async fn endpoint(
    site: Arc<dyn multiview_h3::Site>,
    wt: Option<Arc<dyn WtHandler>>,
) -> (H3Endpoint, SocketAddr) {
    let cert = cert();
    let tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert], cert.key)
        .unwrap();
    let socket = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let socket = noq::Runtime::wrap_udp_socket(&noq::TokioRuntime, socket).unwrap();
    let endpoint = H3Endpoint::start(EndpointParts {
        socket,
        endpoint: noq::EndpointConfig::default(),
        incoming: (64, 4 * 1024 * 1024),
        tls: Arc::new(tls),
        runtime: tokio::runtime::Handle::current(),
        site,
        wt,
        limits: H3Limits::default(),
        peers: None,
        refuse_source: None,
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    (endpoint, addr)
}

async fn h3(addr: SocketAddr) -> Client {
    Client::connect(
        "[::1]:0".parse().unwrap(),
        addr,
        "localhost",
        client::insecure_client_config(provider(), b"h3").unwrap(),
        TIMEOUT,
    )
    .await
    .unwrap()
}

fn router() -> axum::Router {
    axum::Router::new()
        .route(
            "/peer",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
        )
        .route("/echo", post(|body: axum::body::Bytes| async move { body }))
        .route(
            "/api/v1/ws",
            get(|ws: axum::extract::ws::WebSocketUpgrade| async move {
                ws.on_upgrade(|_socket| async {})
            }),
        )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_over_h3_with_connect_info_ws_rejection_and_shutdown() {
    let (signal, watch) = tokio::sync::watch::channel(false);
    let (endpoint, addr) = endpoint(Arc::new(H3Site::new(router(), watch)), None).await;
    let mut c = h3(addr).await;
    let auth = format!("localhost:{}", addr.port());

    let (resp, body) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/peer").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let peer: SocketAddr = String::from_utf8(body).unwrap().parse().unwrap();
    assert!(
        peer.ip().is_loopback(),
        "ConnectInfo carries the QUIC peer: {peer}"
    );

    let payload = vec![7u8; 256 * 1024];
    let (resp, body) = c
        .fetch(
            client::request(http::Method::POST, &auth, "/echo").unwrap(),
            &payload,
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(body, payload);

    // HTTP/3 has no Upgrade: the WebSocket route answers a clean 4xx, no hang.
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/api/v1/ws").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());

    // CONNECT without a WebTransport handler: 501, the site keeps serving.
    let (resp, _) = c
        .open(
            client::connect_request(&auth, "/moq/?ticket=x", None).unwrap(),
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 501);

    signal.send_replace(true);
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/peer").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    drop(c);
    endpoint.shutdown().await;
}

#[tokio::test]
async fn real_control_router_ws_over_h3_is_a_clean_4xx() {
    // The production router: /api/v1/ws runs the auth gate first, then the
    // upgrade extractor; neither can hang an h3 request.
    let engine = Arc::new(multiview_engine::EnginePublisher::<
        multiview_control::EngineStateSnapshot,
        multiview_events::Event,
    >::new(16));
    let (tx, _rx) = multiview_control::command_bus(4);
    let state = multiview_control::AppState::new(
        engine,
        tx,
        Arc::new(multiview_control::InMemoryRepository::new()),
        Arc::new(multiview_control::ApiKeyStore::new(
            b"h3-site-test-pepper".to_vec(),
        )),
    )
    .with_auth_disabled(true);
    let app = multiview_control::router(state);
    let (_signal, watch) = tokio::sync::watch::channel(false);
    let (endpoint, addr) = endpoint(Arc::new(H3Site::new(app, watch)), None).await;
    let mut c = h3(addr).await;
    let auth = format!("localhost:{}", addr.port());
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/api/v1/ws").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/api/v1/health").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(c);
    endpoint.shutdown().await;
}

/// A single-rendition lease through the real `admit` path, with one frame
/// published on its video track.
fn admit_lease(runtime: &Runtime) -> (String, Arc<Fence>, tokio::sync::mpsc::Sender<()>) {
    let (fence, windows) = Fence::channel(4);
    let feed = Feed {
        generation: 1,
        catalog: bytes::Bytes::from_static(
            br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#,
        ),
        audio_tracks: vec![],
        windows,
        demand: Box::new(()),
    };
    let admission = runtime
        .admit(feed, Arc::clone(&fence), Duration::from_secs(30))
        .unwrap();
    // Keep the lease alive while the test runs (the feed sender is the fence's).
    let (keep, _rx) = tokio::sync::mpsc::channel(1);
    (admission.url, fence, keep)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moq_over_h3_with_single_use_tickets() {
    let runtime = Runtime::external("https://localhost/moq".into(), "moq-transport-16").unwrap();
    let wt: Arc<dyn WtHandler> = Arc::new(MoqWt::new(Arc::clone(&runtime), None).unwrap());
    assert_eq!(wt.protocols(), &["moqt-16"]);
    let (_signal, watch) = tokio::sync::watch::channel(false);
    let (endpoint, addr) = endpoint(Arc::new(H3Site::new(router(), watch)), Some(wt)).await;
    let (url, fence, _keep) = admit_lease(&runtime);
    assert!(url.starts_with("https://localhost/moq/?ticket="), "{url}");
    let query = url.split_once('?').unwrap().1.to_owned();
    let auth = format!("localhost:{}", addr.port());

    // A wrong ticket: 401 on the CONNECT, before any session; the same
    // connection keeps serving pages (pooling).
    let mut c = h3(addr).await;
    let (resp, _) = c
        .open(
            client::connect_request(&auth, "/moq/?ticket=wrong", Some("\"moqt-16\"")).unwrap(),
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/peer").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The real moq-tokio WebTransport client with the admitted ticket.
    let sub_origin = moq_tokio::origin::spawn();
    let consumer = sub_origin.consume();
    let mut announcements = consumer.announced();
    let mut config = moq_tokio::connect::Config::default();
    config.tls.insecure = Some(true);
    config.bind = Some("[::1]:0".parse().unwrap());
    config.version = vec!["moq-transport-16".parse().unwrap()];
    let moq_client = config
        .init(moq_tokio::quic::Config::default())
        .unwrap()
        .with_subscriber(sub_origin);
    let target: url::Url = format!("https://[::1]:{}/moq/?{query}", addr.port())
        .parse()
        .unwrap();
    let _connection = tokio::time::timeout(
        TIMEOUT,
        moq_client
            .with_reconnect(false)
            .connect(target)
            .established(),
    )
    .await
    .expect("MoQ connect timed out")
    .expect("MoQ connect failed");
    tokio::time::timeout(TIMEOUT, announcements.next())
        .await
        .expect("announce timed out")
        .expect("origin closed");
    let remote = tokio::time::timeout(TIMEOUT, consumer.request_broadcast("media"))
        .await
        .expect("broadcast timed out")
        .expect("broadcast resolves");
    let mut sub = remote
        .track("catalog.json")
        .unwrap()
        .subscribe(None)
        .await
        .unwrap();
    let mut group = tokio::time::timeout(TIMEOUT, sub.recv_group())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = tokio::time::timeout(TIMEOUT, group.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        frame.payload.windows(5).any(|w| w == b"video"),
        "the admitted catalog arrived over h3/WebTransport"
    );

    // The same ticket a second time: 401 before any session.
    let mut c2 = h3(addr).await;
    let (resp, _) = c2
        .open(
            client::connect_request(&auth, &format!("/moq/?{query}"), Some("\"moqt-16\"")).unwrap(),
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(
        !fence.revocation().is_revoked(),
        "the live session keeps its fence"
    );
    drop((c, c2));
    endpoint.shutdown().await;
}
