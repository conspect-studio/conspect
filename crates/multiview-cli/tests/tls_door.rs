//! The real front door's TCP door with `[network.tls]` (ADR-0107 section 5a,
//! slice 4 + slice 5, #291): a dead CA keeps the engine in `setup`, and over
//! real TLS handshakes on the door:
//!
//! * a normal client (ALPN `http/1.1`, SNI = `public_host`) gets the
//!   bootstrap certificate; non-setup routes answer `503`
//!   `/problems/tls-setup-required`; `GET /api/v1/tls/status` answers `200`
//!   with no HSTS;
//! * a `ClientHello` offering only `acme-tls/1` for a published name gets the
//!   RFC 8737 challenge certificate (acmeIdentifier extension);
//! * after `AcmeCertificateSource::install`, a NEW handshake gets the new
//!   certificate with no restart, and the gate lets normal routes through;
//! * behind a proxy, the setup-code limiter keys on the PROXY v2 source
//!   address, and status carries the TLS-ALPN-01 hint.
#![cfg(all(feature = "acme", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use multiview_config::MultiviewConfig;
use multiview_control::certificate_source::{alpn_challenge_certificate, TlsState};
use multiview_control::EngineStateSnapshot;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HOST: &str = "fd.example.org";
/// DER of the OID 1.3.6.1.5.5.7.1.31 (id-pe-acmeIdentifier, RFC 8737).
const ACME_IDENTIFIER_OID: &[u8] = &[0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x1f];

const BASE: &str = r##"
schema_version = 1

[canvas]
width = 320
height = 180
fps = "25/1"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "bars"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/conspect-tls-door-test.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// The run's TLS context is a process-global install slot: one door at a time.
fn serial() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind("[::1]:0").unwrap();
    socket.local_addr().unwrap().port()
}

fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// `[network]` + `[network.tls]` against a custom CA on a closed loopback
/// port (connection refused): the engine stays in `setup`.
fn config_text(state: &std::path::Path, network: &str) -> String {
    let dead = free_tcp_port();
    let anchor = state.parent().unwrap().join("root.pem");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    std::fs::write(&anchor, ca.self_signed(&ca_key).unwrap().pem()).unwrap();
    format!(
        "{BASE}\n[network]\nhttps_port = {}\npublic_host = \"{HOST}\"\n{network}\n\
         [network.tls]\nca = \"custom\"\nchallenge = \"tls-alpn-01\"\n\
         directory = \"https://[::1]:{dead}/dir\"\ntrust_anchor = \"{}\"\n\
         allow_private_directory = true\ndomains = [\"{HOST}\"]\nstate_dir = \"{}\"\n",
        free_port(),
        anchor.display(),
        state.display()
    )
}

struct Door {
    _dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    boot: multiview_cli::tls_boot::TlsBoot,
    run: Option<multiview_cli::frontdoor::FrontDoorRun>,
    tcp: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Door {
    async fn stop(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.server).await;
        if let Some(door) = self.run.take() {
            door.shutdown().await;
        }
        multiview_cli::tls_boot::install_tls_context(None);
    }
}

/// Start the real wiring the run uses: the ACME boot (dead CA), the front
/// door with its TCP door, and the control plane attached as the door's site.
async fn start(network: &str) -> Door {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let text = config_text(&state_dir, network);
    let config = MultiviewConfig::load_from_toml(&text).unwrap();
    config.validate().unwrap();
    let options = multiview_cli::tls_boot::BootOptions {
        egress_timeouts: None,
        attempt_timeout: Some(Duration::from_secs(5)),
    };
    let boot = multiview_cli::tls_boot::TlsBoot::start(&config, None, &options)
        .unwrap()
        .expect("[network.tls] configured");
    multiview_cli::tls_boot::install_tls_context(Some(Arc::clone(&boot.context)));
    let front_door = multiview_cli::frontdoor::FrontDoorRun::start(&config).expect("front door");
    let tcp = front_door.tcp_local_addr().expect("TCP door running");
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = multiview_control::command_bus(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (_addr, server, _state) = Box::pin(multiview_cli::control::bind_and_serve_with_moq(
        "[::1]:0",
        &config,
        publisher,
        commands,
        multiview_control::no_preview(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_only(),
        None,
        None,
        #[cfg(feature = "moq-output")]
        None,
        #[cfg(feature = "moq-output")]
        None,
        Some(&front_door),
        async move {
            let _ = shutdown_rx.await;
        },
    ))
    .await
    .expect("bind + serve");
    Door {
        _dir: dir,
        state_dir,
        boot,
        run: Some(front_door),
        tcp,
        shutdown: Some(shutdown_tx),
        server,
    }
}

#[derive(Debug)]
struct AcceptAny;
impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// One TLS handshake on the door (optionally after a PROXY v2 header).
async fn handshake(door: SocketAddr, alpn: &[&[u8]], proxy_src: Option<SocketAddr>) -> Tls {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let mut tcp = TcpStream::connect(door).await.unwrap();
    if let Some(src) = proxy_src {
        let header = multiview_frontdoor::tcp::proxy_v2::encode(src, door, &[]);
        tcp.write_all(&header).await.unwrap();
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(ServerName::try_from(HOST).unwrap(), tcp),
    )
    .await
    .expect("handshake in time")
    .expect("handshake")
}

fn leaf(tls: &Tls) -> Vec<u8> {
    tls.get_ref().1.peer_certificates().unwrap()[0].to_vec()
}

/// One HTTP/1.1 request on a fresh `http/1.1` connection: (status, head, body).
async fn http(
    door: SocketAddr,
    proxy_src: Option<SocketAddr>,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String, String) {
    let mut tls = handshake(door, &[b"http/1.1"], proxy_src).await;
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"http/1.1".as_slice())
    );
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), tls.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"));
    (status, head.to_ascii_lowercase(), body.to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn door_serves_bootstrap_answers_tls_alpn_and_swaps_on_install() {
    let _serial = serial().lock().await;
    let door = start(&format!(
        "mode = \"direct\"\nbind = \"[::1]\"\ntcp_port = {}\nhttp_redirect_port = 0\n",
        free_tcp_port()
    ))
    .await;
    let source = Arc::clone(&door.boot.context.source);
    assert_eq!(source.state(), TlsState::Setup);
    let bootstrap = door.boot.context.certificates.current().cert[0].to_vec();

    // (a) Normal client: the bootstrap certificate, the setup gate, no HSTS.
    let tls = handshake(door.tcp, &[b"http/1.1"], None).await;
    assert_eq!(
        leaf(&tls),
        bootstrap,
        "setup serves the bootstrap certificate"
    );
    drop(tls);
    let (status, head, body) = http(door.tcp, None, "GET", "/api/v1/outputs", "").await;
    assert_eq!(status, 503, "{head}{body}");
    assert!(body.contains("/problems/tls-setup-required"), "{body}");
    assert!(head.contains("retry-after: 30"), "{head}");
    let (status, head, body) = http(door.tcp, None, "GET", "/api/v1/tls/status", "").await;
    assert_eq!(status, 200, "{head}{body}");
    assert!(!head.contains("strict-transport-security"), "{head}");
    let status_json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status_json["state"], "setup");
    // Direct mode: no behind-proxy hint.
    assert_eq!(status_json["hints"], serde_json::json!([]));
    // Slice 2: no QUIC in setup (the h3 endpoint waits for `serving`) and
    // therefore no Alt-Svc advertisement on the TCP door.
    let run = door.run.as_ref().unwrap();
    assert!(!run.h3_running(), "no HTTP/3 endpoint while in setup");
    assert!(!head.to_ascii_lowercase().contains("alt-svc"), "{head}");

    // (b) TLS-ALPN-01: ALPN only `acme-tls/1` for a published name gets the
    // challenge certificate with the acmeIdentifier extension.
    let challenge = alpn_challenge_certificate(HOST, b"token.thumbprint").unwrap();
    let challenge_der = challenge.cert[0].to_vec();
    door.boot
        .context
        .resolver
        .publish(HOST.to_owned(), challenge);
    let tls = handshake(door.tcp, &[b"acme-tls/1"], None).await;
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"acme-tls/1".as_slice())
    );
    let served = leaf(&tls);
    assert_eq!(served, challenge_der, "the published challenge certificate");
    assert!(
        served
            .windows(ACME_IDENTIFIER_OID.len())
            .any(|w| w == ACME_IDENTIFIER_OID),
        "acmeIdentifier extension (1.3.6.1.5.5.7.1.31) present"
    );
    drop(tls);
    // The site handshake is unaffected by the published challenge.
    let tls = handshake(door.tcp, &[b"http/1.1"], None).await;
    assert_eq!(leaf(&tls), bootstrap);
    drop(tls);
    door.boot.context.resolver.remove(HOST);

    // (c) Install a fresh certificate: the NEXT handshake gets it, no restart,
    // and the gate opens.
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let issued = rcgen::CertificateParams::new(vec![HOST.to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let issued_der = issued.der().to_vec();
    let signing = rustls::crypto::aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    ))
    .unwrap();
    let certified = Arc::new(rustls::sign::CertifiedKey::new(
        vec![CertificateDer::from(issued_der.clone())],
        signing,
    ));
    let expires = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 90 * 86_400;
    source.install(certified, expires, &door.state_dir).unwrap();
    assert_eq!(source.state(), TlsState::Serving);
    let tls = handshake(door.tcp, &[b"http/1.1"], None).await;
    assert_eq!(
        leaf(&tls),
        issued_der,
        "the installed certificate, no restart"
    );
    drop(tls);
    let (status, head, body) = http(door.tcp, None, "GET", "/api/v1/outputs", "").await;
    assert_ne!(status, 503, "the gate is open once serving: {head}{body}");
    assert!(!body.contains("tls-setup-required"), "{body}");

    // Slice 2: `serving` starts the h3 endpoint on the QUIC lane with the
    // ACME certificate; then the TCP door advertises it.
    let run = door.run.as_ref().unwrap();
    let started = std::time::Instant::now();
    while !run.h3_running() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the h3 endpoint never started after serving"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let udp = run.local_addr().port();
    let (tcp_status, head, _) = http(door.tcp, None, "GET", "/api/v1/outputs", "").await;
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("alt-svc: h3=\":{udp}\"; ma=86400")),
        "{head}"
    );
    let mut h3 = multiview_h3::client::Client::connect(
        "[::1]:0".parse().unwrap(),
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, udp)),
        HOST,
        multiview_h3::client::insecure_client_config(
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            b"h3",
        )
        .unwrap(),
        Duration::from_secs(10),
    )
    .await
    .expect("h3 handshake with the ACME certificate");
    let issued_on_quic = h3
        .conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .map(|chain| chain[0].to_vec());
    assert_eq!(
        issued_on_quic,
        Some(issued_der.clone()),
        "QUIC serves the installed certificate"
    );
    let (resp, body) = h3
        .fetch(
            multiview_h3::client::request(
                http::Method::GET,
                &format!("{HOST}:{udp}"),
                "/api/v1/outputs",
            )
            .unwrap(),
            &[],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    // The same gated router answers over h3 exactly as over TCP (here: the
    // auth gate, no token), never the setup 503.
    assert_eq!(
        resp.status().as_u16(),
        tcp_status,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!String::from_utf8_lossy(&body).contains("tls-setup-required"));
    drop(h3);

    door.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_proxy_limits_setup_codes_by_the_proxy_v2_source() {
    let _serial = serial().lock().await;
    let proxy_listen = free_tcp_port();
    let door = start(&format!(
        "mode = \"behind_proxy\"\ntrusted_proxies = [\"::1\"]\nproxy_listen = \"[::1]:{proxy_listen}\"\n"
    ))
    .await;
    assert_eq!(door.tcp.port(), proxy_listen);
    let alice: SocketAddr = "[2001:db8:1::7]:40000".parse().unwrap();
    let bob: SocketAddr = "[2001:db8:2::7]:40000".parse().unwrap();

    // The §5a hint: TLS-ALPN-01 cannot reach the engine behind a proxy.
    let (status, _, body) = http(door.tcp, Some(alice), "GET", "/api/v1/tls/status", "").await;
    assert_eq!(status, 200);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        value["hints"].to_string().contains("TLS-ALPN-01"),
        "{value}"
    );

    // Five wrong codes from alice lock alice's /64 ...
    let wrong = r#"{"code":"not the code"}"#;
    for _ in 0..5 {
        let (status, head, body) = http(
            door.tcp,
            Some(alice),
            "POST",
            "/api/v1/tls/setup/session",
            wrong,
        )
        .await;
        assert_eq!(status, 401, "{head}{body}");
    }
    let (status, head, body) = http(
        door.tcp,
        Some(alice),
        "POST",
        "/api/v1/tls/setup/session",
        wrong,
    )
    .await;
    assert_eq!(status, 429, "alice locked: {head}{body}");
    // ... and not bob, although every TCP connection comes from the same
    // loopback proxy address.
    let (status, head, body) = http(
        door.tcp,
        Some(bob),
        "POST",
        "/api/v1/tls/setup/session",
        wrong,
    )
    .await;
    assert_eq!(status, 401, "bob keyed on his own address: {head}{body}");

    door.stop().await;
}
