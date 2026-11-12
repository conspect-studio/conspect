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
    bootstrap.persist(dir.path()).unwrap();
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
    source.install(next.certificate.clone(), 1_900_000_000);
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
    tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from("test.example.org").unwrap(), stream)
        .await
        .unwrap();
    task.await.unwrap();
    resolver.remove("test.example.org");
}
