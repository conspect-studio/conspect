//! Front-door slice 2 (#288) through the real wiring: `FrontDoorRun` on
//! `[::1]`, its QUIC lane serving `multiview_h3::H3Endpoint` with the door's
//! static certificate, the site router over h3, and the TCP door's
//! `alt-svc` advertisement:
//!
//! * an h3 GET over the shared UDP socket returns 200 from the site router;
//! * the WebRTC lane is untouched: a STUN datagram on the same socket is
//!   still classified to the WebRTC lane, never to QUIC;
//! * the TCP door's responses carry `alt-svc: h3=":<port>"; ma=86400` once
//!   the endpoint is up, and none with `[network] alt_svc = false`.
#![cfg(all(feature = "tls", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_config::MultiviewConfig;
use multiview_h3::client::{self, Client};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

fn free_tcp() -> u16 {
    std::net::TcpListener::bind("[::1]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp() -> u16 {
    std::net::UdpSocket::bind("[::]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Door {
    _dir: tempfile::TempDir,
    run: Option<multiview_cli::frontdoor::FrontDoorRun>,
    tcp: SocketAddr,
    udp: u16,
    _signal: tokio::sync::watch::Sender<bool>,
}

/// The real front door with the TCP door on a static certificate, the site
/// on both TCP (with the Alt-Svc layer) and h3, as `attach_front_door_site`
/// wires it.
async fn start(alt_svc: Option<bool>) -> Door {
    let dir = tempfile::tempdir().unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    std::fs::write(dir.path().join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), signing_key.serialize_pem()).unwrap();
    let tcp_port = free_tcp();
    let alt = alt_svc.map_or(String::new(), |v| format!("alt_svc = {v}\n"));
    let mut config = MultiviewConfig::load_from_toml(&format!(
        r##"schema_version = 1
[canvas]
width = 64
height = 36
fps = "30/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[network]
https_port = 443
mode = "direct"
bind = "[::1]"
tcp_port = {tcp_port}
http_redirect_port = 0
public_host = "localhost"
cert_file = "{cert}"
key_file = "{key}"
{alt}"##,
        cert = dir.path().join("cert.pem").display(),
        key = dir.path().join("key.pem").display(),
    ))
    .unwrap();
    config.network.as_mut().unwrap().https_port = Some(free_udp());
    let run = multiview_cli::frontdoor::FrontDoorRun::start_with(
        &config,
        multiview_frontdoor::ListenSockets::empty(),
    )
    .expect("front door starts");
    let udp = run.local_addr().port();
    let app = axum::Router::new().route(
        "/hello",
        axum::routing::get(|| async { "hello from the site" }),
    );
    let (signal, watch) = tokio::sync::watch::channel(false);
    let source = multiview_control::static_certificate_source(
        &config.network.as_ref().unwrap().static_site_tls().unwrap(),
    )
    .unwrap();
    let site_tls = multiview_control::site_server_config(Arc::new(source)).unwrap();
    run.start_h3(
        Arc::new(multiview_control::h3_site::H3Site::new(
            app.clone(),
            watch.clone(),
        )),
        multiview_cli::frontdoor::H3Tls {
            config: Some(Arc::clone(&site_tls)),
            ready: None,
            source: "test static certificate",
        },
    );
    let options = multiview_control::ServeOptions::default();
    let tcp_app = multiview_control::h3_site::alt_svc_layer(app, run.alt_svc());
    run.install_site(
        multiview_control::ConnectionServer::new(tcp_app, &options, watch),
        Some(site_tls),
        None,
    );
    let started = Instant::now();
    while !run.h3_running() {
        assert!(started.elapsed() < TIMEOUT, "the h3 endpoint never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Door {
        _dir: dir,
        tcp: run.tcp_local_addr().unwrap(),
        run: Some(run),
        udp,
        _signal: signal,
    }
}

impl Door {
    async fn stop(mut self) {
        if let Some(run) = self.run.take() {
            run.shutdown().await;
        }
    }
}

#[derive(Debug)]
struct AcceptAny;
impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// One HTTP/1.1 GET over TLS to the TCP door; the raw response head.
async fn tcp_get(addr: SocketAddr, path: &str) -> String {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAny))
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    let fut = async {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut s = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    };
    tokio::time::timeout(TIMEOUT, fut).await.unwrap()
}

async fn h3(udp: u16) -> Client {
    Client::connect(
        "[::1]:0".parse().unwrap(),
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, udp)),
        "localhost",
        client::insecure_client_config(Arc::new(rustls::crypto::ring::default_provider()), b"h3")
            .unwrap(),
        TIMEOUT,
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn site_over_h3_on_the_shared_socket_and_alt_svc_on_tcp() {
    let door = start(None).await;
    let run = door.run.as_ref().unwrap();
    assert_eq!(run.h3_local_addr().map(|a| a.port()), Some(door.udp));

    // h3 GET through the shared UDP socket.
    let mut c = h3(door.udp).await;
    let auth = format!("localhost:{}", door.udp);
    for _ in 0..3 {
        let (resp, body) = c
            .fetch(
                client::request(http::Method::GET, &auth, "/hello").unwrap(),
                &[],
                TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body, b"hello from the site");
        // h3 responses do not carry Alt-Svc (TCP only, design note section 5).
        assert!(resp.headers().get("alt-svc").is_none());
    }
    // No MoQ installed: a WebTransport CONNECT is 501, the site still serves.
    let (resp, _) = c
        .open(
            client::connect_request(&auth, "/moq/?ticket=x", None).unwrap(),
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 501);

    // The WebRTC lane is untouched: a STUN Binding Request on the same
    // socket is classified to the WebRTC lane (no consumer here), not QUIC.
    let before = run.stats().snapshot();
    let mut rng = multiview_frontdoor::flood::Rng::new(7);
    let stun =
        multiview_frontdoor::flood::datagram(multiview_frontdoor::flood::Shape::Stun, &mut rng);
    let probe = std::net::UdpSocket::bind("[::1]:0").unwrap();
    probe.send_to(&stun, ("::1", door.udp)).unwrap();
    let started = Instant::now();
    loop {
        let now = run.stats().snapshot();
        let rtc = (now.to_rtc + now.dropped_no_consumer)
            .saturating_sub(before.to_rtc + before.dropped_no_consumer);
        if rtc >= 1 {
            break;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "STUN never reached the WebRTC lane: {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Alt-Svc on the TCP door's site responses.
    let head = tcp_get(door.tcp, "/hello").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let expected = format!("alt-svc: h3=\":{}\"; ma=86400", door.udp);
    assert!(
        head.to_ascii_lowercase().contains(&expected),
        "{expected} missing:\n{head}"
    );
    drop(c);
    door.stop().await;
}

/// Like [`start`], but `behind_proxy` mode with `proxy_listen` on
/// `[::1]:0` and the given `trusted_proxies` list.
async fn start_behind_proxy(trusted_proxies: &[&str]) -> Door {
    let dir = tempfile::tempdir().unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    std::fs::write(dir.path().join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), signing_key.serialize_pem()).unwrap();
    let trusted = trusted_proxies
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let proxy_port = free_tcp();
    let mut config = MultiviewConfig::load_from_toml(&format!(
        r##"schema_version = 1
[canvas]
width = 64
height = 36
fps = "30/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[network]
https_port = 443
mode = "behind_proxy"
proxy_listen = "[::1]:{proxy_port}"
public_host = "localhost"
trusted_proxies = [{trusted}]
cert_file = "{cert}"
key_file = "{key}"
"##,
        cert = dir.path().join("cert.pem").display(),
        key = dir.path().join("key.pem").display(),
    ))
    .unwrap();
    config.network.as_mut().unwrap().https_port = Some(free_udp());
    let run = multiview_cli::frontdoor::FrontDoorRun::start_with(
        &config,
        multiview_frontdoor::ListenSockets::empty(),
    )
    .expect("front door starts");
    let udp = run.local_addr().port();
    let app = axum::Router::new().route(
        "/hello",
        axum::routing::get(|| async { "hello from the site" }),
    );
    let (signal, watch) = tokio::sync::watch::channel(false);
    let source = multiview_control::static_certificate_source(
        &config.network.as_ref().unwrap().static_site_tls().unwrap(),
    )
    .unwrap();
    let site_tls = multiview_control::site_server_config(Arc::new(source)).unwrap();
    run.start_h3(
        Arc::new(multiview_control::h3_site::H3Site::new(
            app.clone(),
            watch.clone(),
        )),
        multiview_cli::frontdoor::H3Tls {
            config: Some(Arc::clone(&site_tls)),
            ready: None,
            source: "test static certificate",
        },
    );
    let options = multiview_control::ServeOptions::default();
    let tcp_app = multiview_control::h3_site::alt_svc_layer(app, run.alt_svc());
    run.install_site(
        multiview_control::ConnectionServer::new(tcp_app, &options, watch),
        Some(site_tls),
        None,
    );
    let started = Instant::now();
    while !run.h3_running() {
        assert!(started.elapsed() < TIMEOUT, "the h3 endpoint never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Door {
        _dir: dir,
        tcp: run.tcp_local_addr().unwrap(),
        run: Some(run),
        udp,
        _signal: signal,
    }
}

/// Design note §8 / review #371 finding F1: in `behind_proxy` mode a QUIC
/// source that is a `trusted_proxies` address is a forwarder hop that has
/// lost the real client's address (no PROXY header on QUIC), so it is
/// refused before any cap check, failing closed like the TCP door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_proxy_refuses_quic_from_a_trusted_proxy_source() {
    // The h3 client in this test connects from `[::1]`, which is listed as
    // a trusted proxy: the QUIC connection must be refused.
    let door = start_behind_proxy(&["::1"]).await;
    let run = door.run.as_ref().unwrap();
    let endpoint = noq::Endpoint::client("[::1]:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(
        client::insecure_client_config(Arc::new(rustls::crypto::ring::default_provider()), b"h3")
            .unwrap(),
    );
    let connecting = endpoint
        .connect(
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, door.udp)),
            "localhost",
        )
        .unwrap();
    let result = tokio::time::timeout(TIMEOUT, connecting).await.unwrap();
    assert!(
        result.is_err(),
        "a QUIC connection from a trusted-proxy source must be refused"
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let refused = run
            .h3_counters()
            .map_or(0, |c| multiview_h3::Counters::get(&c.sources_refused));
        if refused >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sources_refused never incremented"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    door.stop().await;
}

/// The counterpart: `trusted_proxies` does NOT list the client's address
/// (`2001:db8::1` is not `::1`), so the connection is served normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_proxy_serves_a_source_not_in_trusted_proxies() {
    let door = start_behind_proxy(&["2001:db8::1"]).await;
    let udp = door.udp;
    let mut c = h3(udp).await;
    let auth = format!("localhost:{udp}");
    let (resp, body) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/hello").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(body, b"hello from the site");
    let run = door.run.as_ref().unwrap();
    assert_eq!(
        run.h3_counters()
            .map_or(0, |c| multiview_h3::Counters::get(&c.sources_refused)),
        0,
        "the client's own source must not be refused"
    );
    drop(c);
    door.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alt_svc_false_keeps_the_advertisement_off() {
    let door = start(Some(false)).await;
    // The endpoint is up and serving h3 ...
    let mut c = h3(door.udp).await;
    let auth = format!("localhost:{}", door.udp);
    let (resp, _) = c
        .fetch(
            client::request(http::Method::GET, &auth, "/hello").unwrap(),
            &[],
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // ... but the operator declared UDP unreachable: no advertisement.
    let head = tcp_get(door.tcp, "/hello").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("alt-svc"),
        "alt-svc must be absent:\n{head}"
    );
    drop(c);
    door.stop().await;
}
