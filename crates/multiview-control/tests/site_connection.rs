//! `ConnectionServer` (ADR-0107 slice 4, issue #290): the connection-level entry
//! point the network front door's TCP door uses. A TLS stream accepted and
//! terminated by the caller is served on the same ADR-W031 floor as the plain
//! listener: a 200 from a router, and a header-slowloris closed at the
//! header-read timeout. The caller's guard rides the connection to its end.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use multiview_config::TlsConfig;
use multiview_control::{
    site_server_config, static_certificate_source, ConnectionServer, ServeOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Flag(Arc<AtomicBool>);
impl Drop for Flag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn site(dir: &std::path::Path) -> Arc<rustls::ServerConfig> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert_file = dir.join("cert.pem");
    let key_file = dir.join("key.pem");
    std::fs::write(&cert_file, cert.pem()).unwrap();
    std::fs::write(&key_file, signing_key.serialize_pem()).unwrap();
    let source = static_certificate_source(&TlsConfig::Static {
        cert_file,
        key_file,
    })
    .unwrap();
    let config = site_server_config(Arc::new(source)).unwrap();
    assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    config
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

async fn tls_client(addr: std::net::SocketAddr) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAny))
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(addr).await.unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_terminated_tls_connection_is_served_on_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(site(dir.path()));
    let options =
        ServeOptions::default().with_header_read_timeout(Some(Duration::from_millis(500)));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let app =
        Router::new().route(
            "/hello",
            get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >| async move { format!("hi {}", peer.ip()) },
            ),
        );
    let server = ConnectionServer::new(app, &options, shutdown_rx);
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let accept_released = Arc::clone(&released);
    let accept = tokio::spawn(async move {
        for i in 0..2 {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            // The caller supplies the real client address (e.g. from PROXY v2).
            let peer: std::net::SocketAddr = "[2001:db8::7]:4000".parse().unwrap();
            let server = server.clone();
            let flag = Flag(Arc::clone(&accept_released));
            tokio::spawn(async move {
                if i == 0 {
                    server.serve(tls, peer, flag).await;
                } else {
                    server.serve(tls, peer, ()).await;
                }
            });
        }
    });

    // 1. A request gets a 200 with the caller's peer as ConnectInfo.
    let mut tls = tls_client(addr).await;
    tls.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    tls.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("hi 2001:db8::7"), "{out}");
    drop(tls);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        released.load(Ordering::SeqCst),
        "the caller's guard is released at the end"
    );

    // 2. Header slowloris: a partial header block is closed at the timeout.
    let mut slow = tls_client(addr).await;
    slow.write_all(b"GET /hello HTTP/1.1\r\nHost: local")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), slow.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "closed before the test deadline");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(350) && elapsed < Duration::from_secs(3),
        "closed at the header-read timeout, took {elapsed:?}"
    );
    accept.await.unwrap();
    // Every connection (and the server clone) is gone; the send has no receiver.
    let _ = shutdown_tx.send(true);
}
