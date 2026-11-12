//! The ACME egress client against an in-process HTTPS fixture on `[::1]`:
//! redirects to `http://` or to a private literal are refused, the hop count is
//! bounded, and bodies are capped.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod acme_support;

use acme_support::{fixture_pki, serve_https, write_root};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use multiview_control::acme::egress::{EgressClient, EgressError, EgressPolicy, EgressTimeouts};

async fn fixture() -> (u16, tempfile::TempDir, std::path::PathBuf) {
    let pki = fixture_pki();
    let app = Router::new()
        .route("/ok", get(|| async { "fine" }))
        .route("/big", get(|| async { vec![b'x'; 2 * 1024 * 1024] }))
        .route(
            "/to-http",
            get(|| async {
                (StatusCode::FOUND, [(header::LOCATION, "http://[::1]:1/ok")]).into_response()
            }),
        )
        .route(
            "/to-private",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "https://10.0.0.1/ok")],
                )
                    .into_response()
            }),
        )
        .route(
            "/to-imds",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "https://169.254.169.254/latest")],
                )
                    .into_response()
            }),
        )
        .route(
            "/loop",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")]).into_response() }),
        )
        .route(
            "/one-hop",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ok")]).into_response() }),
        );
    let addr = serve_https(app, &pki).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = write_root(&pki, tmp.path());
    (addr.port(), tmp, root)
}

fn client(port: u16, root: &std::path::Path, cap: usize) -> EgressClient {
    EgressClient::new(
        EgressPolicy::for_tests_allowing_loopback_ports(false, &[port]),
        Some(root),
        EgressTimeouts::default(),
        cap,
    )
    .unwrap()
}

async fn get_path(c: &EgressClient, port: u16, path: &str) -> Result<String, EgressError> {
    let request = c
        .reqwest()
        .get(format!("https://[::1]:{port}{path}"))
        .build()
        .unwrap();
    c.send(request)
        .await
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
}

#[tokio::test]
async fn follows_a_safe_redirect_and_reads_the_body() {
    let (port, _t, root) = fixture().await;
    let c = client(port, &root, 1024);
    assert_eq!(get_path(&c, port, "/ok").await.unwrap(), "fine");
    assert_eq!(get_path(&c, port, "/one-hop").await.unwrap(), "fine");
}

#[tokio::test]
async fn redirects_to_http_private_or_metadata_are_refused() {
    let (port, _t, root) = fixture().await;
    let c = client(port, &root, 1024);
    for path in ["/to-http", "/to-private", "/to-imds"] {
        assert_eq!(
            get_path(&c, port, path).await.unwrap_err(),
            EgressError::Refused,
            "{path}"
        );
    }
}

#[tokio::test]
async fn redirect_hops_are_bounded() {
    let (port, _t, root) = fixture().await;
    let c = client(port, &root, 1024);
    assert_eq!(
        get_path(&c, port, "/loop").await.unwrap_err(),
        EgressError::Refused
    );
}

#[tokio::test]
async fn oversize_bodies_are_refused() {
    let (port, _t, root) = fixture().await;
    let c = client(port, &root, 1024 * 1024);
    assert_eq!(
        get_path(&c, port, "/big").await.unwrap_err(),
        EgressError::TooLarge
    );
}

#[tokio::test]
async fn plain_http_and_unlisted_loopback_are_refused_before_sending() {
    let (port, _t, root) = fixture().await;
    let c = client(port, &root, 1024);
    let r = c
        .reqwest()
        .get(format!("http://[::1]:{port}/ok"))
        .build()
        .unwrap();
    assert_eq!(c.send(r).await.unwrap_err(), EgressError::Refused);
    let other = client(port.wrapping_add(1), &root, 1024);
    assert_eq!(
        get_path(&other, port, "/ok").await.unwrap_err(),
        EgressError::Refused
    );
}

#[tokio::test]
async fn trust_anchor_replaces_the_built_in_roots() {
    let (port, tmp, _root) = fixture().await;
    // A different fixture root: the server's chain no longer verifies.
    let other = write_root(&fixture_pki(), &tmp.path().join(""));
    let c = client(port, &other, 1024);
    assert_eq!(
        get_path(&c, port, "/ok").await.unwrap_err(),
        EgressError::Refused
    );
}
