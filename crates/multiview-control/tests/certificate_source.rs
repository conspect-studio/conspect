#![cfg(feature = "tls")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_control::certificate_source::{
    alpn_challenge_certificate, bootstrap_certificate, require_serving, AcmeCertificateSource,
    CertificateSource, ChallengeResolver, TlsState, TLS_SETUP_REQUIRED,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::ServerName;
use std::{os::unix::fs::PermissionsExt, sync::Arc};

/// ACME challenge certificates deliberately contain an unsupported critical extension.
/// Inspect that extension in this test verifier instead of pretending they are web certs.
#[derive(Debug)]
struct VerifyAcmeExtension;
impl ServerCertVerifier for VerifyAcmeExtension {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let (_, parsed) = x509_parser::parse_x509_certificate(end_entity.as_ref())
            .map_err(|_| rustls::Error::General("invalid ACME certificate".into()))?;
        let ext = parsed
            .extensions()
            .iter()
            .find(|ext| ext.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .ok_or_else(|| rustls::Error::General("ACME extension missing".into()))?;
        // rcgen encodes the SHA-256 digest in a DER OCTET STRING.
        let digest = Sha256::digest(b"token.authorization");
        if !ext.critical
            || ext.value.len() != 34
            || ext.value.get(..2) != Some([4, 32].as_slice())
            || ext.value.get(2..) != Some(digest.as_slice())
        {
            return Err(rustls::Error::General("invalid ACME extension".into()));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[test]
fn bootstrap_is_fresh_private_and_hot_swaps_without_downgrade() {
    let bootstrap = bootstrap_certificate("test.example.org").unwrap();
    let next = bootstrap_certificate("test.example.org").unwrap();
    assert_ne!(bootstrap.fingerprint, next.fingerprint);
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    bootstrap.persist(dir.path()).unwrap();
    let shared = tempfile::tempdir().unwrap();
    std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(bootstrap.persist(shared.path()).is_err());
    let linked = shared.path().join("linked");
    std::os::unix::fs::symlink(dir.path(), &linked).unwrap();
    assert!(bootstrap.persist(&linked.join("nested")).is_err());
    assert!(bootstrap
        .persist(&dir.path().join("inside").join("..").join("escape"))
        .is_err());
    assert_eq!(
        std::fs::metadata(shared.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("bootstrap.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let source = AcmeCertificateSource::new(
        bootstrap.certificate.clone(),
        bootstrap.fingerprint.clone(),
        "custom".into(),
        vec!["test.example.org".into()],
    );
    let receiver = source.subscribe();
    assert_eq!(source.status().state, TlsState::Setup);
    assert_eq!(
        require_serving(source.status().state),
        Err(TLS_SETUP_REQUIRED)
    );
    source.report_error("ca-unavailable");
    assert!(Arc::ptr_eq(&bootstrap.certificate, &source.current()));
    assert!(source
        .rotate_bootstrap("test.example.org", dir.path())
        .unwrap());
    assert_ne!(
        source.status().bootstrap_fingerprint,
        Some(bootstrap.fingerprint)
    );
    source
        .install(next.certificate.clone(), 1_900_000_000, dir.path())
        .unwrap();
    assert!(!source
        .rotate_bootstrap("test.example.org", dir.path())
        .unwrap());
    assert!(!dir.path().join("bootstrap.key").exists());
    assert!(receiver.has_changed().unwrap());
    assert!(Arc::ptr_eq(&next.certificate, &source.current()));
    assert_eq!(source.status().state, TlsState::Serving);
    assert_eq!(source.status().bootstrap_fingerprint, None);
    assert_eq!(source.status().last_error, None);
    assert_eq!(require_serving(source.status().state), Ok(()));
    let debug = format!("{:?}", ChallengeResolver::new(Arc::new(source)));
    assert_eq!(debug, "ChallengeResolver([redacted])");
}

#[tokio::test]
async fn alpn_challenge_completes_real_rustls_handshake() {
    let bootstrap = bootstrap_certificate("test.example.org").unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate,
        bootstrap.fingerprint,
        "custom".into(),
        vec!["test.example.org".into()],
    ));
    let resolver = Arc::new(ChallengeResolver::new(source));
    let cert = alpn_challenge_certificate("test.example.org", b"token.authorization").unwrap();
    resolver.publish("test.example.org".into(), cert.clone());
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_cert_resolver(resolver.clone());
    let mut server = server;
    server.alpn_protocols = vec![b"acme-tls/1".to_vec()];
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(VerifyAcmeExtension))
    .with_no_client_auth();
    let mut client = client;
    client.alpn_protocols = vec![b"acme-tls/1".to_vec()];
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(stream)
            .await
            .unwrap();
    });
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from("test.example.org").unwrap(), stream)
        .await
        .unwrap();
    assert_eq!(
        handshake.get_ref().1.alpn_protocol(),
        Some(b"acme-tls/1".as_slice())
    );
    task.await.unwrap();

    // Mixed ALPN is ordinary traffic: the site certificate, not the challenge.
    let site = Arc::new(AcceptAnyCert);
    handshake_with(&resolver, Some(site), &[b"h2", b"acme-tls/1"])
        .await
        .expect("mixed ALPN uses the site certificate");
    handshake_with(&resolver, Some(Arc::new(AcceptAnyCert)), &[])
        .await
        .expect("no ALPN uses the site certificate");

    resolver.remove("test.example.org");
    handshake_with(&resolver, Some(Arc::new(AcceptAnyCert)), &[b"acme-tls/1"])
        .await
        .expect("a removed challenge falls back to the site certificate");
}

/// Accept any certificate. Used to prove ordinary ALPN never selects the
/// challenge certificate, which this verifier would reject.
#[derive(Debug)]
struct AcceptAnyCert;
impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let (_, parsed) = x509_parser::parse_x509_certificate(end_entity.as_ref())
            .map_err(|_| rustls::Error::General("invalid certificate".into()))?;
        if parsed
            .extensions()
            .iter()
            .any(|ext| ext.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
        {
            return Err(rustls::Error::General(
                "ordinary traffic received the ACME challenge certificate".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn handshake_with(
    resolver: &Arc<ChallengeResolver>,
    verifier: Option<Arc<dyn ServerCertVerifier>>,
    alpn: &[&[u8]],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(resolver.clone());
    let mut server = server;
    server.alpn_protocols = vec![b"acme-tls/1".to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(verifier.unwrap_or_else(|| Arc::new(AcceptAnyCert)))
    .with_no_client_auth();
    let mut client = client;
    client.alpn_protocols = alpn.iter().map(|item| item.to_vec()).collect();
    let listener = tokio::net::TcpListener::bind("[::1]:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(stream)
            .await
            .map(|_| ())
            .map_err(|error| std::io::Error::other(error.to_string()))
    });
    let stream = tokio::net::TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from("test.example.org").unwrap(), stream)
        .await
        .map_err(|error| error.to_string())?;
    task.await??;
    Ok(())
}

/// Review of #338 (final head): an RFC 8738 IP identifier's challenge is
/// published under the reverse name, which is a DNS name (not an IP literal),
/// so rustls exposes it as SNI and the door's challenge-only route selects
/// it. The acmeIdentifier is SHA-256 of the RAW key authorization, hashed
/// once (`VerifyAcmeExtension` checks the exact bytes).
#[cfg(unix)]
#[tokio::test]
async fn ip_identifier_challenge_is_selected_by_its_reverse_name_sni() {
    let bootstrap = bootstrap_certificate("test.example.org").unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate,
        bootstrap.fingerprint,
        "custom".into(),
        vec!["203.0.113.5".into()],
    ));
    let resolver = Arc::new(ChallengeResolver::new(source));
    let cert = alpn_challenge_certificate("203.0.113.5", b"token.authorization").unwrap();
    resolver.publish("5.113.0.203.in-addr.arpa".into(), cert);
    let server = multiview_control::acme_challenge_server_config(resolver).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(VerifyAcmeExtension))
    .with_no_client_auth();
    client.alpn_protocols = vec![b"acme-tls/1".to_vec()];
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(server)
            .accept(stream)
            .await
            .map(|_| ())
    });
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("5.113.0.203.in-addr.arpa").unwrap();
    assert!(
        matches!(name, ServerName::DnsName(_)),
        "the reverse name is a DNS name, sent as SNI"
    );
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(name, stream)
        .await
        .expect("challenge certificate selected by the reverse-name SNI, digest verified");
    assert_eq!(
        handshake.get_ref().1.alpn_protocol(),
        Some(b"acme-tls/1".as_slice())
    );
    task.await.unwrap().unwrap();
}
