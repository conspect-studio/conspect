//! A minimal native HTTP/3 client over the same noq adapter (feature
//! `test-client`): what the integration and chaos tests of the crates that
//! wire the endpoint (the front door, the control plane) use to reach it.
//! Never part of a shipped build.
//!
//! It speaks h3 with extended CONNECT enabled, verifies the server
//! certificate against the caller's roots (or accepts any certificate, for
//! self-signed test endpoints), and drops server-initiated WebTransport
//! streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::noq_adapter::{self, StreamRouter};
use crate::BoxError;

/// The h3 request sender of a [`Client`].
pub type SendRequest = h3::client::SendRequest<noq_adapter::OpenStreams, Bytes>;
/// One request stream of a [`Client`].
pub type ClientStream = h3::client::RequestStream<noq_adapter::BidiStream<Bytes>, Bytes>;

/// Accepts any server certificate (test endpoints with self-signed
/// certificates). Signatures are still checked with the provider.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

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
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
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
        cert: &rustls::pki_types::CertificateDer<'_>,
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

/// A noq client config offering exactly `alpn`, verifying nothing (test
/// endpoints), with `provider`.
///
/// # Errors
///
/// The provider has no TLS 1.3 / QUIC suite.
pub fn insecure_client_config(
    provider: Arc<rustls::crypto::CryptoProvider>,
    alpn: &[u8],
) -> Result<noq::ClientConfig, BoxError> {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let crypto = noq::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls))?;
    Ok(noq::ClientConfig::new(Arc::new(crypto)))
}

/// Admits every request stream; ignores server-opened WebTransport streams.
struct Router(Arc<Semaphore>);

impl StreamRouter for Router {
    fn admit_request(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.0).try_acquire_owned().ok()
    }
    fn webtransport_bi(&self, _: u64, _: noq::SendStream, _: noq_adapter::RecvStream) {}
    fn webtransport_uni(&self, _: u64, _: noq_adapter::RecvStream) {}
}

/// One h3 connection.
pub struct Client {
    /// The QUIC connection.
    pub conn: noq::Connection,
    /// The request sender.
    pub send: SendRequest,
    driver: tokio::task::JoinHandle<()>,
    _endpoint: noq::Endpoint,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.driver.abort();
        self.conn.close(noq::VarInt::from_u32(0x100), b"done");
    }
}

impl Client {
    /// Connects to `server` (SNI `server_name`) from `bind` with `config`
    /// (ALPN `h3`), within `timeout`, and completes the h3 SETTINGS exchange.
    ///
    /// # Errors
    ///
    /// Socket, handshake, h3 setup failures or the timeout.
    pub async fn connect(
        bind: SocketAddr,
        server: SocketAddr,
        server_name: &str,
        config: noq::ClientConfig,
        timeout: Duration,
    ) -> Result<Self, BoxError> {
        let endpoint = noq::Endpoint::client(bind)?;
        endpoint.set_default_client_config(config);
        Self::connect_on(&endpoint, server, server_name, timeout).await
    }

    /// [`connect`](Self::connect) on an existing client endpoint (its
    /// default client config), so many connections share one socket.
    ///
    /// # Errors
    ///
    /// As [`connect`](Self::connect).
    pub async fn connect_on(
        endpoint: &noq::Endpoint,
        server: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self, BoxError> {
        let endpoint = endpoint.clone();
        let connecting = endpoint.connect(server, server_name)?;
        let conn = tokio::time::timeout(timeout, connecting).await??;
        let quic = noq_adapter::Connection::new(conn.clone())
            .with_router(Arc::new(Router(Arc::new(Semaphore::new(1024)))));
        let (mut driver, send) = tokio::time::timeout(
            timeout,
            h3::client::builder()
                .enable_extended_connect(true)
                .enable_datagram(true)
                .build::<_, _, Bytes>(quic),
        )
        .await??;
        let driver = tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });
        Ok(Self {
            conn,
            send,
            driver,
            _endpoint: endpoint,
        })
    }

    /// Sends `req` with `body` (sent in 16 KiB DATA frames; empty: none),
    /// reads the whole response, all within `timeout`.
    ///
    /// # Errors
    ///
    /// Any stream error or the timeout. A server that stops reading the
    /// request with `H3_NO_ERROR` after answering is not an error (RFC 9114
    /// section 4.1).
    pub async fn fetch(
        &mut self,
        req: http::Request<()>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(http::Response<()>, Vec<u8>), BoxError> {
        let send = &mut self.send;
        let fut = async move {
            let mut stream = send.send_request(req).await?;
            for chunk in body.chunks(16 * 1024) {
                stream.send_data(Bytes::copy_from_slice(chunk)).await?;
            }
            if let Err(err) = stream.finish().await {
                let no_error = err.is_h3_no_error()
                    || matches!(
                        err,
                        h3::error::StreamError::RemoteTerminate { code } if code == h3::error::Code::H3_NO_ERROR
                    );
                if !no_error {
                    return Err(BoxError::from(err));
                }
            }
            let resp = stream.recv_response().await?;
            let mut out = Vec::new();
            while let Some(mut chunk) = stream.recv_data().await? {
                out.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
            }
            Ok((resp, out))
        };
        tokio::time::timeout(timeout, fut).await?
    }
}

impl Client {
    /// Sends `req` without finishing the request stream and reads only the
    /// response head (an extended CONNECT: on `200` the stream is the
    /// session's CONNECT stream and stays open while the returned stream
    /// lives), within `timeout`.
    ///
    /// # Errors
    ///
    /// Any stream error or the timeout.
    pub async fn open(
        &mut self,
        req: http::Request<()>,
        timeout: Duration,
    ) -> Result<(http::Response<()>, ClientStream), BoxError> {
        let send = &mut self.send;
        let fut = async move {
            let mut stream = send.send_request(req).await?;
            let resp = stream.recv_response().await?;
            Ok::<_, BoxError>((resp, stream))
        };
        tokio::time::timeout(timeout, fut).await?
    }
}

/// A body-less request for `https://<authority><path>`.
///
/// # Errors
///
/// An unparsable URI.
pub fn request(
    method: http::Method,
    authority: &str,
    path: &str,
) -> Result<http::Request<()>, http::Error> {
    http::Request::builder()
        .method(method)
        .uri(format!("https://{authority}{path}"))
        .body(())
}

/// A WebTransport extended CONNECT for `path` offering `protocols`.
///
/// # Errors
///
/// An unparsable URI or header value.
pub fn connect_request(
    authority: &str,
    path: &str,
    protocols: Option<&str>,
) -> Result<http::Request<()>, BoxError> {
    let mut req = request(http::Method::CONNECT, authority, path)?;
    req.extensions_mut()
        .insert(h3::ext::Protocol::WEB_TRANSPORT);
    if let Some(p) = protocols {
        req.headers_mut()
            .insert("wt-available-protocols", http::HeaderValue::from_str(p)?);
    }
    Ok(req)
}
