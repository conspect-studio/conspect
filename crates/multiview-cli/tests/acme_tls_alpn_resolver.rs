//! The front door's ACME route wired to a [`ChallengeResolver`] (#290/#291):
//! `FrontDoorRun::install_acme_resolver` builds the TLS-ALPN-01 responder's
//! `ServerConfig` and installs it without disturbing a previously installed
//! site configuration, and vice versa (`install_site` no longer clobbers the
//! ACME half back to `None`). The ACME route is challenge-only: an
//! `acme-tls/1` hello for an unpublished name, or without SNI, aborts the
//! handshake and never presents the site certificate.
#![cfg(all(feature = "tls", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use multiview_control::certificate_source::{
    alpn_challenge_certificate, ChallengeResolver, StaticCertificateSource,
};
use multiview_frontdoor::{
    BoxFuture, ConnGuard, DoorTlsStream, FrontDoor, FrontDoorConfig, SiteService, TcpDoorConfig,
    TlsRouting,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A trivial site that writes a fixed response and closes; only used here so
/// the `http/1.1` comparison hello completes its handshake.
struct FixedSite;

impl SiteService for FixedSite {
    fn serve(&self, mut io: DoorTlsStream, _peer: SocketAddr, guard: ConnGuard) -> BoxFuture<()> {
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

/// An explicit `ring` crypto provider for the test TLS client (distinct from
/// the server side's `aws-lc-rs`, matching the design note's "ring or
/// aws-lc provider" allowance for the test client).
fn client_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AcceptAnyRecordLeaf(std::sync::Mutex<Option<Vec<u8>>>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyRecordLeaf {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.0.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    // Deliberately bypass full chain/extension validation (as the existing
    // `AcceptAnyCert`/`VerifyAcmeExtension` test verifiers in
    // multiview-control/tests/certificate_source.rs do): the ACME challenge
    // leaf carries a critical, unrecognised `id-pe-acmeIdentifier` extension
    // that `rustls::crypto::verify_tls13_signature`'s webpki path rejects as
    // `UnsupportedCriticalExtension`, and this test's whole point is to
    // inspect that leaf, not to validate it as a real web certificate.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        client_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Connect to `addr`, SNI `sni`, offering `alpn`. Returns the negotiated ALPN
/// (if any) and the leaf certificate the server presented.
async fn handshake(
    addr: SocketAddr,
    sni: &str,
    alpn: &[&[u8]],
) -> std::io::Result<(Option<Vec<u8>>, Vec<u8>)> {
    let verifier: Arc<AcceptAnyRecordLeaf> =
        Arc::new(AcceptAnyRecordLeaf(std::sync::Mutex::new(None)));
    let verifier_clone: Arc<AcceptAnyRecordLeaf> = Arc::clone(&verifier);
    let dyn_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> = verifier_clone;
    let mut cfg = rustls::ClientConfig::builder_with_provider(client_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(dyn_verifier)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from(sni.to_owned()).unwrap();
    let mut tls = connector.connect(name, tcp).await?;
    let negotiated = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    // The ACME handshake serves no application data: read until EOF/close.
    let mut buf = [0u8; 16];
    let _ = tokio::time::timeout(Duration::from_secs(2), tls.read(&mut buf)).await;
    let _ = tls.shutdown().await;
    let leaf_der = verifier.0.lock().unwrap().clone().unwrap_or_default();
    Ok((negotiated, leaf_der))
}

/// One handshake attempt with an arbitrary `ServerName` (an IP literal sends
/// no SNI). Returns the handshake result and the leaf the server presented,
/// recorded even when the handshake then fails.
async fn attempt(
    addr: SocketAddr,
    name: ServerName<'static>,
    alpn: &[&[u8]],
) -> (std::io::Result<()>, Option<Vec<u8>>) {
    let verifier = Arc::new(AcceptAnyRecordLeaf(std::sync::Mutex::new(None)));
    let dyn_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> = verifier.clone();
    let mut cfg = rustls::ClientConfig::builder_with_provider(client_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(dyn_verifier)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let result = match TcpStream::connect(addr).await {
        Ok(tcp) => {
            match tokio::time::timeout(Duration::from_secs(5), connector.connect(name, tcp)).await {
                Ok(Ok(mut tls)) => {
                    let _ = tls.shutdown().await;
                    Ok(())
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err(std::io::Error::other("handshake timed out")),
            }
        }
        Err(e) => Err(e),
    };
    let leaf = verifier.0.lock().unwrap().clone();
    (result, leaf)
}

fn self_signed(host: &str) -> Arc<CertifiedKey> {
    let ck = rcgen::generate_simple_self_signed(vec![host.to_owned()]).unwrap();
    let cert = CertificateDer::from(ck.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    Arc::new(CertifiedKey::from_der(vec![cert], key, &provider).unwrap())
}

/// `true` if `der` contains the id-pe-acmeIdentifier OID
/// (1.3.6.1.5.5.7.1.31 = `06 08 2B 06 01 05 05 07 01 1F`).
fn has_acme_identifier_oid(der: &[u8]) -> bool {
    const OID: [u8; 10] = [0x06, 0x08, 0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x1F];
    der.windows(OID.len()).any(|w| w == OID)
}

#[tokio::test(flavor = "multi_thread")]
async fn acme_route_wired_through_install_acme_resolver() {
    let site_key = self_signed("localhost");
    let site_source: Arc<dyn multiview_control::certificate_source::CertificateSource> =
        Arc::new(StaticCertificateSource::new(site_key));
    let site_config = multiview_control::site_server_config(site_source).unwrap();

    let challenge_source: Arc<dyn multiview_control::certificate_source::CertificateSource> =
        Arc::new(StaticCertificateSource::new(self_signed("localhost")));
    let resolver = Arc::new(ChallengeResolver::new(challenge_source));

    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let addr = door
        .start_tcp(
            TcpDoorConfig {
                listen: "[::1]:0".parse().unwrap(),
                ..TcpDoorConfig::default()
            },
            TlsRouting::default(),
            Some(Arc::new(FixedSite)),
        )
        .unwrap();

    // Before any ACME configuration is installed: an acme-tls/1 hello closes
    // (handshake fails).
    let before = handshake(addr, "acme.test", &[b"acme-tls/1"]).await;
    assert!(
        before.is_err() || before.unwrap().1.is_empty(),
        "an acme-tls/1 hello before install_acme_resolver must not get a real handshake"
    );

    // Install the site config (set_site_config) then the ACME resolver
    // (set_acme_config), exactly as `FrontDoorRun::install_site` /
    // `install_acme_resolver` do: both halves installed independently.
    door.set_site_config(Some(site_config)).unwrap();
    let acme_config =
        multiview_control::acme_challenge_server_config(Arc::clone(&resolver)).unwrap();
    door.set_acme_config(Some(acme_config)).unwrap();

    // Publish a challenge for "acme.test".
    let key_authz = b"key-authz";
    let challenge = alpn_challenge_certificate("acme.test", key_authz).unwrap();
    resolver.publish("acme.test".to_owned(), challenge);

    // An acme-tls/1 hello for "acme.test" gets the challenge leaf, negotiates
    // acme-tls/1, and the server closes without serving application data.
    let (alpn, leaf) = handshake(addr, "acme.test", &[b"acme-tls/1"])
        .await
        .expect("the acme-tls/1 handshake completes");
    assert_eq!(alpn.as_deref(), Some(b"acme-tls/1".as_slice()));
    assert!(
        has_acme_identifier_oid(&leaf),
        "the leaf must carry the id-pe-acmeIdentifier extension OID"
    );

    // A plain http/1.1 hello for the same SNI gets the SITE certificate, not
    // the challenge: its leaf has no ACME extension.
    let (http_alpn, http_leaf) = handshake(addr, "acme.test", &[b"http/1.1"])
        .await
        .expect("the http/1.1 handshake completes");
    assert_eq!(http_alpn.as_deref(), Some(b"http/1.1".as_slice()));
    assert!(!has_acme_identifier_oid(&http_leaf));
    assert_ne!(http_leaf, leaf);

    // `set_site_config` (read-modify-write) must not have clobbered the ACME
    // half: a second acme-tls/1 handshake for "acme.test" still gets the
    // challenge leaf after the site half was installed.
    let (alpn2, leaf2) = handshake(addr, "acme.test", &[b"acme-tls/1"])
        .await
        .expect("the acme-tls/1 handshake still completes after the site install");
    assert_eq!(alpn2.as_deref(), Some(b"acme-tls/1".as_slice()));
    assert!(has_acme_identifier_oid(&leaf2));

    tokio::task::spawn_blocking(move || door.shutdown_blocking())
        .await
        .unwrap();
}

/// A1: the ACME route never falls back to the site/bootstrap key. The
/// resolver's site source here IS the site key, so a fallback would be
/// observable as the site leaf.
#[tokio::test(flavor = "multi_thread")]
async fn acme_route_is_challenge_only() {
    let site_key = self_signed("localhost");
    let site_leaf = site_key.cert[0].as_ref().to_vec();
    let site_source: Arc<dyn multiview_control::certificate_source::CertificateSource> =
        Arc::new(StaticCertificateSource::new(Arc::clone(&site_key)));
    let resolver = Arc::new(ChallengeResolver::new(Arc::clone(&site_source)));

    let mut door = FrontDoor::start(&FrontDoorConfig {
        port: 0,
        workers: 2,
        ..FrontDoorConfig::default()
    })
    .unwrap();
    let addr = door
        .start_tcp(
            TcpDoorConfig {
                listen: "[::1]:0".parse().unwrap(),
                ..TcpDoorConfig::default()
            },
            TlsRouting::default(),
            Some(Arc::new(FixedSite)),
        )
        .unwrap();
    door.set_site_config(Some(
        multiview_control::site_server_config(site_source).unwrap(),
    ))
    .unwrap();
    door.set_acme_config(Some(
        multiview_control::acme_challenge_server_config(Arc::clone(&resolver)).unwrap(),
    ))
    .unwrap();
    resolver.publish(
        "acme.test".to_owned(),
        alpn_challenge_certificate("acme.test", b"key-authz").unwrap(),
    );

    // Unpublished SNI: the handshake fails and the site leaf is never shown.
    let (result, leaf) = attempt(
        addr,
        ServerName::try_from("other.test").unwrap(),
        &[b"acme-tls/1"],
    )
    .await;
    assert!(
        result.is_err(),
        "acme-tls/1 for an unpublished name must fail"
    );
    assert_ne!(leaf.as_deref(), Some(site_leaf.as_slice()));
    assert!(leaf.is_none(), "no certificate may be presented");

    // No SNI (IP-literal server name): the handshake fails likewise.
    let (result, leaf) =
        attempt(addr, ServerName::try_from("::1").unwrap(), &[b"acme-tls/1"]).await;
    assert!(result.is_err(), "acme-tls/1 without SNI must fail");
    assert_ne!(leaf.as_deref(), Some(site_leaf.as_slice()));
    assert!(leaf.is_none(), "no certificate may be presented");

    // The published name still gets its challenge leaf.
    let (alpn, leaf) = handshake(addr, "acme.test", &[b"acme-tls/1"])
        .await
        .expect("the published acme-tls/1 handshake completes");
    assert_eq!(alpn.as_deref(), Some(b"acme-tls/1".as_slice()));
    assert!(has_acme_identifier_oid(&leaf));
    assert_ne!(leaf, site_leaf);

    // The site route still serves the site leaf.
    let (_, http_leaf) = handshake(addr, "acme.test", &[b"http/1.1"])
        .await
        .expect("the http/1.1 handshake completes");
    assert_eq!(http_leaf, site_leaf);

    tokio::task::spawn_blocking(move || door.shutdown_blocking())
        .await
        .unwrap();
}
