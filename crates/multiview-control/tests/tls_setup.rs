//! The front-door certificate setup surface (ADR-0107 section 5a, #291):
//! the setup gate, setup-code sessions, loopback-only admin routes, credential
//! storage and the non-secret status body. Router-level (`tower::oneshot`).
#![cfg(all(feature = "acme", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use multiview_config::{AcmeChallenge, CertificateAuthority, DnsProviderConfig, NetworkTlsConfig};
use multiview_control::certificate_source::{
    bootstrap_certificate, AcmeCertificateSource, CertificateSource, ChallengeResolver,
    SourceResolver,
};
use multiview_control::tls_setup::{front_door_router, SetupAuth, TlsContext, SESSION_COOKIE};
use multiview_control::AppState;
use support::{body_bytes, body_json, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN};
use tower::ServiceExt;

const SECRET: &str = "cf-token-VALUE-must-never-appear-0123456789";
const LOOPBACK: &str = "[::1]:40000";
const REMOTE: &str = "[2001:db8:0:1::7]:40000";

struct Fixture {
    _dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    source: Arc<AcmeCertificateSource>,
    tls: Arc<TlsContext>,
    issued: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let boot = bootstrap_certificate("fd.example.org").unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        boot.certificate.clone(),
        boot.fingerprint,
        "letsencrypt".into(),
        vec!["fd.example.org".into()],
    ));
    let certificates: Arc<dyn CertificateSource> = source.clone();
    let resolver = Arc::new(ChallengeResolver::new(certificates.clone()));
    let issued = Arc::new(AtomicUsize::new(0));
    let counter = issued.clone();
    let config = NetworkTlsConfig {
        ca: CertificateAuthority::Letsencrypt,
        staging: true,
        challenge: AcmeChallenge::Dns01,
        domains: vec!["fd.example.org".into()],
        contact_email: None,
        acme_profile: None,
        directory: None,
        trust_anchor: None,
        allow_private_directory: false,
        eab_kid_env: None,
        eab_hmac_env: None,
        state_dir: state_dir.clone(),
        dns: Some(DnsProviderConfig::Cloudflare {
            token_env: Some("CONSPECT_TEST_TLS_SETUP_CF_TOKEN".into()),
            email_env: None,
            api_key_env: None,
            propagation_timeout_s: 120,
        }),
    };
    let tls = Arc::new(TlsContext {
        source: source.clone(),
        certificates,
        resolver,
        issue: Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        config: Arc::new(config),
        setup: Arc::new(SetupAuth::new()),
        behind_proxy: false,
    });
    Fixture {
        _dir: dir,
        state_dir,
        source,
        tls,
        issued,
    }
}

fn state(f: &Fixture) -> AppState {
    let mut built = None;
    let _ = support::harness_with(|s| {
        let s = s.with_tls(f.tls.clone());
        built = Some(s.clone());
        s
    });
    built.unwrap()
}

fn front(f: &Fixture) -> Router {
    front_door_router(state(f))
}

fn admin(f: &Fixture) -> Router {
    multiview_control::router(state(f))
}

fn request(method: &str, path: &str, peer: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = peer.parse().unwrap();
    let mut builder = Request::builder().method(method).uri(path);
    builder.extensions_mut().unwrap().insert(ConnectInfo(peer));
    builder
}

async fn send(router: &Router, req: Request<Body>) -> axum::response::Response {
    router.clone().oneshot(req).await.unwrap()
}

fn install(f: &Fixture) {
    let next = bootstrap_certificate("fd.example.org").unwrap();
    f.source
        .install(next.certificate, i64::MAX / 2, &f.state_dir)
        .unwrap();
}

async fn assert_setup_required(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "30");
    assert!(response
        .headers()
        .get(header::STRICT_TRANSPORT_SECURITY)
        .is_none());
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    let body = body_json(response).await;
    assert_eq!(body["type"], "/problems/tls-setup-required");
}

#[tokio::test]
async fn setup_gate_refuses_everything_but_the_setup_surface() {
    let f = fixture();
    let router = front(&f);
    for (method, path) in [
        ("GET", "/api/v1/layouts"),
        ("GET", "/api/v1/health"),
        ("GET", "/api/v1/ws"),
        ("GET", "/api/v1/events"),
        ("POST", "/api/v1/commands/start"),
        ("POST", "/api/v1/preview/program/whep"),
        ("POST", "/api/v1/preview/program/moq"),
        ("GET", "/api/v1/openapi.json"),
        ("GET", "/docs"),
        ("GET", "/x-nmos/node/v1.3/self"),
        ("GET", "/hls/main/index.m3u8"),
        ("GET", "/index.html"),
        ("POST", "/api/v1/tls/setup-code"),
        ("POST", "/api/v1/tls/credential"),
        ("GET", "/api/v1/auth/status"),
    ] {
        let response = send(
            &router,
            request(method, path, REMOTE)
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_setup_required(response).await;
    }
}

#[tokio::test]
async fn setup_surface_is_reachable_in_setup_without_hsts() {
    let f = fixture();
    let router = front(&f);
    let page = send(
        &router,
        request("GET", "/tls-setup", REMOTE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.starts_with("default-src 'self'"), "{csp}");
    assert!(page
        .headers()
        .get(header::STRICT_TRANSPORT_SECURITY)
        .is_none());
    let html = String::from_utf8(body_bytes(page).await).unwrap();
    assert!(html.contains("fingerprint"));
    assert!(
        !html.contains("type=\"password\""),
        "no secret input on the page"
    );
    assert!(
        !html.contains("http://") && !html.contains("https://"),
        "no external assets"
    );

    let root = send(
        &router,
        request("GET", "/", REMOTE).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(root.status(), StatusCode::SEE_OTHER);
    assert_eq!(root.headers().get(header::LOCATION).unwrap(), "/tls-setup");

    let status = send(
        &router,
        request("GET", "/api/v1/tls/status", REMOTE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status.status(), StatusCode::OK);
    assert!(status
        .headers()
        .get(header::STRICT_TRANSPORT_SECURITY)
        .is_none());
    let body = body_json(status).await;
    assert_eq!(body["state"], "setup");
    assert_eq!(body["challenge"], "dns-01");
    assert!(body["bootstrap_fingerprint"]
        .as_str()
        .unwrap()
        .contains(':'));
    assert_eq!(
        body["reasons"][0],
        "credential-missing: CONSPECT_TEST_TLS_SETUP_CF_TOKEN"
    );

    // Issue without credentials: 401; with a setup session: 202.
    let refused = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    let code = f.tls.setup.mint();
    let login = send(
        &router,
        request("POST", "/api/v1/tls/setup/session", REMOTE)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({ "code": code }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::NO_CONTENT);
    let cookie = login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    for attr in ["HttpOnly", "Secure", "SameSite=Strict", "Max-Age=3600"] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    let pair = cookie.split(';').next().unwrap().to_owned();
    assert!(pair.starts_with(SESSION_COOKIE));
    let issued = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .header(header::COOKIE, pair.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(issued.status(), StatusCode::ACCEPTED);
    assert_eq!(f.issued.load(Ordering::SeqCst), 1);
    // A forged cookie is refused.
    let forged = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .header(
                header::COOKIE,
                format!("{SESSION_COOKIE}={}", "0".repeat(64)),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    // The admin bearer works on the front door too.
    let by_admin = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(by_admin.status(), StatusCode::ACCEPTED);
    // An operator bearer is not enough.
    let by_operator = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(by_operator.status(), StatusCode::UNAUTHORIZED);
    // Once serving, the setup session no longer authorizes anything.
    install(&f);
    let after = send(
        &router,
        request("POST", "/api/v1/tls/issue", REMOTE)
            .header(header::COOKIE, pair)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn setup_code_attempts_are_bounded_per_source() {
    let f = fixture();
    let router = front(&f);
    let code = f.tls.setup.mint();
    let login = |peer: &'static str, code: String| {
        request("POST", "/api/v1/tls/setup/session", peer)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({ "code": code }).to_string()))
            .unwrap()
    };
    for _ in 0..5 {
        let wrong = send(&router, login(REMOTE, "abandon ability able".into())).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
    // Another host in the same /64 is locked too, even with the right code.
    let locked = send(&router, login("[2001:db8:0:1::99]:1", code.clone())).await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.headers().get(header::RETRY_AFTER).is_some());
    // A different /64 succeeds once; the code is then spent.
    let ok = send(&router, login("[2001:db8:0:2::1]:1", code.clone())).await;
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    let reused = send(&router, login("[2001:db8:0:3::1]:1", code)).await;
    assert_eq!(reused.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn serving_makes_the_gate_a_no_op() {
    let f = fixture();
    install(&f);
    let router = front(&f);
    let layouts = send(
        &router,
        request("GET", "/api/v1/layouts", REMOTE)
            .header(header::AUTHORIZATION, format!("Bearer {VIEWER_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(layouts.status(), StatusCode::OK);
    let unauthenticated = send(
        &router,
        request("GET", "/api/v1/layouts", REMOTE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    // Status needs Read once serving and reports no bootstrap fingerprint.
    let anonymous = send(
        &router,
        request("GET", "/api/v1/tls/status", REMOTE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let status = body_json(
        send(
            &router,
            request("GET", "/api/v1/tls/status", REMOTE)
                .header(header::AUTHORIZATION, format!("Bearer {VIEWER_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert_eq!(status["state"], "serving");
    assert!(status["bootstrap_fingerprint"].is_null());
    assert_eq!(status["reasons"], serde_json::json!([]));
}

#[tokio::test]
async fn loopback_admin_routes_refuse_remote_peers_even_for_admin() {
    let f = fixture();
    let router = admin(&f);
    for path in [
        "/api/v1/tls/setup-code",
        "/api/v1/tls/credential",
        "/api/v1/tls/issue",
    ] {
        let response = send(
            &router,
            request("POST", path, REMOTE)
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"name": "CONSPECT_TEST_TLS_SETUP_CF_TOKEN", "value": SECRET})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    // Loopback but not admin: refused as well.
    let operator = send(
        &router,
        request("POST", "/api/v1/tls/setup-code", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(operator.status(), StatusCode::FORBIDDEN);
    // Loopback + admin: a fresh code, returned once, usable on the front door.
    let minted = send(
        &router,
        request("POST", "/api/v1/tls/setup-code", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(minted.status(), StatusCode::OK);
    let body = body_json(minted).await;
    let code = body["code"].as_str().unwrap().to_owned();
    assert_eq!(code.split(' ').count(), 8);
    assert_eq!(body["expires_in"], 3600);
    let issue = send(
        &router,
        request("POST", "/api/v1/tls/issue", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(issue.status(), StatusCode::ACCEPTED);
    // The admin listener is NOT gated (it is the recovery path).
    let layouts = send(
        &router,
        request("GET", "/api/v1/layouts", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(layouts.status(), StatusCode::OK);
}

/// Captures every event's message and fields into one string.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct V<'a>(&'a mut String);
        impl tracing::field::Visit for V<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, " {}={:?}", field.name(), value);
            }
        }
        let mut out = self.0.lock().unwrap();
        event.record(&mut V(&mut out));
        out.push('\n');
    }
}

#[tokio::test]
async fn credential_is_stored_0600_and_never_echoed_or_logged() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let f = fixture();
    let router = admin(&f);
    let post = |name: &str, value: &str| {
        request("POST", "/api/v1/tls/credential", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"name": name, "value": value}).to_string(),
            ))
            .unwrap()
    };
    // A name the config does not reference is refused (and nothing is written).
    let unknown = send(&router, post("AWS_SECRET_ACCESS_KEY", SECRET)).await;
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let unknown_body = String::from_utf8(body_bytes(unknown).await).unwrap();
    assert!(!unknown_body.contains(SECRET));
    assert!(!f.state_dir.join("secrets/AWS_SECRET_ACCESS_KEY").exists());
    // Path tricks are not names the config references either.
    let traversal = send(&router, post("../active.pem", SECRET)).await;
    assert_eq!(traversal.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let stored = send(&router, post("CONSPECT_TEST_TLS_SETUP_CF_TOKEN", SECRET)).await;
    assert_eq!(stored.status(), StatusCode::NO_CONTENT);
    let stored_body = body_bytes(stored).await;
    assert_eq!(stored_body, Vec::<u8>::new());
    let file = f.state_dir.join("secrets/CONSPECT_TEST_TLS_SETUP_CF_TOKEN");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SECRET);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(f.state_dir.join("secrets"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // Body cap: 8 KiB.
    let huge = send(
        &router,
        post("CONSPECT_TEST_TLS_SETUP_CF_TOKEN", &"x".repeat(9 * 1024)),
    )
    .await;
    assert_eq!(huge.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // A malformed body never echoes its content.
    let malformed = send(
        &router,
        request("POST", "/api/v1/tls/credential", LOOPBACK)
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                "{{\"name\": 1, \"value\": \"{SECRET}\""
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(!String::from_utf8(body_bytes(malformed).await)
        .unwrap()
        .contains(SECRET));

    // The status now has no missing-credential reason and no secret field.
    let status = send(
        &router,
        request("GET", "/api/v1/tls/status", LOOPBACK)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(body_bytes(status).await).unwrap();
    assert!(!text.contains(SECRET));
    assert!(!text.contains("credential-missing"), "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for key in &keys {
        assert!(
            ![
                "value",
                "token",
                "secret",
                "key",
                "hmac",
                "password",
                "credential"
            ]
            .contains(key),
            "{key}"
        );
    }

    let logged = capture.0.lock().unwrap().clone();
    assert!(logged.contains("credential stored"), "{logged}");
    assert!(
        !logged.contains(SECRET),
        "credential value leaked into the log"
    );
}

#[derive(Debug)]
struct AcceptAny;
impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
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
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// One TLS handshake against a server using `resolver`; returns the leaf DER.
async fn served_leaf(resolver: Arc<SourceResolver>) -> Vec<u8> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(stream)
            .await;
    });
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(
            rustls::pki_types::ServerName::try_from("fd.example.org").unwrap(),
            stream,
        )
        .await
        .unwrap();
    let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    drop(tls);
    let _ = accept.await;
    leaf
}

#[tokio::test]
async fn setup_to_serving_swap_reaches_new_handshakes_without_restart() {
    let f = fixture();
    // One resolver instance for the listener's whole life (QUIC/MoQ and TCP 443 hold it).
    let resolver = Arc::new(SourceResolver(f.tls.certificates.clone()));
    let bootstrap_leaf = served_leaf(resolver.clone()).await;
    assert_eq!(
        bootstrap_leaf,
        f.tls.certificates.current().cert[0].to_vec()
    );
    let next = bootstrap_certificate("fd.example.org").unwrap();
    let acme_leaf = next.certificate.cert[0].to_vec();
    let changes = f.tls.certificates.subscribe();
    f.source
        .install(next.certificate, i64::MAX / 2, &f.state_dir)
        .unwrap();
    assert!(changes.has_changed().unwrap());
    let served = served_leaf(resolver).await;
    assert_eq!(served, acme_leaf);
    assert_ne!(served, bootstrap_leaf);
    // The bootstrap key file is gone once an ACME certificate is installed.
    assert!(!f.state_dir.join("bootstrap.key").exists());
}
