//! A small, separate, bounded TLS handshake used only to *read* TLS facts
//! (protocol version, negotiated ALPN, leaf subject/issuer/expiry) that
//! reqwest's `TlsInfo` does not expose.
//!
//! It is dialled to an address the collector has **already screened** with
//! `net_guard`; it sends no application data and ends right after the
//! handshake. Certificates are deliberately not verified here: this describes
//! what the server presents, it does not decide trust (the real fetch, which
//! does verify, is a different connection). Bounded to [`HANDSHAKE_TIMEOUT`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    self, crypto::aws_lc_rs, ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use tokio_rustls::TlsConnector;

use super::NetworkTls;

/// Bound on connect + handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct PresentOnly {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for PresentOnly {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

fn clean(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(256).collect()
}

/// Handshake with `addr` presenting `server_name` (SNI) and report the facts.
///
/// # Errors
///
/// A short failure text (connect/handshake error or timeout).
pub async fn handshake(addr: SocketAddr, server_name: &str) -> Result<NetworkTls, String> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls config: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PresentOnly { schemes }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let name =
        ServerName::try_from(server_name.to_owned()).map_err(|e| format!("server name: {e}"))?;
    let work = async {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect: {}", clean(&e.to_string())))?;
        TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("handshake: {}", clean(&e.to_string())))
    };
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, work)
        .await
        .map_err(|_| "handshake: timed out".to_owned())??;
    let (_, conn) = stream.get_ref();
    let version = conn.protocol_version().map(|v| {
        match v {
            rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3",
            rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2",
            _ => "unknown",
        }
        .to_owned()
    });
    let alpn = conn
        .alpn_protocol()
        .map(|p| clean(&String::from_utf8_lossy(p)));
    let mut facts = NetworkTls {
        version,
        alpn,
        ..NetworkTls::default()
    };
    if let Some(der) = conn.peer_certificates().and_then(|c| c.first()) {
        if let Ok((_, cert)) = x509_parser::parse_x509_certificate(der.as_ref()) {
            facts.subject = Some(clean(&cert.subject().to_string()));
            facts.issuer = Some(clean(&cert.issuer().to_string()));
            facts.not_after = Some(crate::catalogue::store::unix_rfc3339(
                cert.validity().not_after.timestamp(),
            ));
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    async fn server(alpn: &[&[u8]]) -> SocketAddr {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["tv.example".to_owned()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();
        let mut cfg =
            rustls::ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], key)
                .unwrap();
        cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut s) = acceptor.accept(sock).await {
                        let _ = s.shutdown().await;
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn reports_version_alpn_and_certificate_facts() {
        let addr = server(&[b"h2", b"http/1.1"]).await;
        let facts = handshake(addr, "tv.example").await.unwrap();
        assert_eq!(facts.version.as_deref(), Some("TLSv1.3"));
        assert_eq!(facts.alpn.as_deref(), Some("h2"));
        assert!(facts.subject.is_some() && facts.issuer.is_some());
        assert!(facts.not_after.as_deref().is_some_and(|d| d.contains('T')));
    }

    #[tokio::test]
    async fn a_plain_tcp_listener_fails_with_text_not_invented_facts() {
        let l = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let _ = s.write_all(b"HTTP/1.1 400 no\r\n\r\n").await;
                let _ = s.shutdown().await;
            }
        });
        let err = handshake(addr, "tv.example").await.unwrap_err();
        assert!(err.starts_with("handshake:"), "{err}");
    }
}
