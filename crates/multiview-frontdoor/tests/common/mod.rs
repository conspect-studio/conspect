//! Shared helpers for the TCP door tests: throwaway certificates (ring
//! provider, dev only), a TLS client, a trivial site, ICE-TCP frames.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_frontdoor::tcp::BoxFuture;
use multiview_frontdoor::{ConnGuard, DoorStream, SiteService};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A self-signed site `ServerConfig` (ALPN http/1.1) for `localhost` and the
/// loopback literals.
pub fn site_config() -> Arc<rustls::ServerConfig> {
    let ck = rcgen::generate_simple_self_signed(vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ])
    .unwrap();
    let cert = CertificateDer::from(ck.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub fn client_config(alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    let p = provider();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::clone(&p))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(p)))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Arc::new(cfg)
}

/// TLS-connect to `addr` with SNI `sni`, send a GET, return the response.
pub async fn https_get(addr: SocketAddr, sni: &str) -> std::io::Result<String> {
    let connector = tokio_rustls::TlsConnector::from(client_config(&[b"http/1.1"]));
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from(sni.to_owned()).unwrap();
    let mut tls = connector.connect(name, tcp).await?;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut out = Vec::new();
    tls.read_to_end(&mut out).await.ok();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A site that writes a fixed 200 and records the peers it served.
#[derive(Default)]
pub struct FixedSite {
    pub served: AtomicU64,
    pub peers: Mutex<Vec<SocketAddr>>,
}

impl SiteService for FixedSite {
    fn serve(
        &self,
        mut io: tokio_rustls::server::TlsStream<DoorStream>,
        peer: SocketAddr,
        guard: ConnGuard,
    ) -> BoxFuture<()> {
        self.served.fetch_add(1, Ordering::Relaxed);
        self.peers.lock().unwrap().push(peer);
        Box::pin(async move {
            let _guard = guard;
            let mut buf = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_secs(2), io.read(&mut buf)).await;
            let _ = io
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
            let _ = io.shutdown().await;
        })
    }
}

/// A Binding Request with `USERNAME` = `username`.
pub fn binding_request(username: &str) -> Vec<u8> {
    let mut m = vec![0x00, 0x01, 0, 0, 0x21, 0x12, 0xA4, 0x42];
    m.extend_from_slice(&[7; 12]);
    let name = username.as_bytes();
    m.extend_from_slice(&[0x00, 0x06]);
    m.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
    m.extend_from_slice(name);
    while m.len() % 4 != 0 {
        m.push(0);
    }
    let body = u16::try_from(m.len() - 20).unwrap();
    m[2..4].copy_from_slice(&body.to_be_bytes());
    m
}

/// `len_be16 || payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = u16::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// Wait (bounded) until the peer closes: `true` if EOF/reset arrived in `within`.
pub async fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {}
            Err(_) => return false,
        }
    }
}
