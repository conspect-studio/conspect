//! The real CASTV2 wire layer, behind the off-by-default `cast` feature
//! (DEV-D2, ADR-M011): TLS (rustls; the device presents a self-signed
//! certificate, so the sender uses a **permissive verifier by design** — the
//! whole sender ecosystem does) + the 4-byte big-endian length-prefixed
//! protobuf `CastMessage` framing.
//!
//! ## Spike verdict — hand-rolled over `rust_cast` (the DEV-D2 spike)
//!
//! `rust_cast` 0.21 was spiked on this exact toolchain first, per the
//! work-schedule acceptance. It **builds cleanly** here (edition-2024 dep is
//! fine under our pinned stable; its pinned `protobuf =3.7.2` conflicts with
//! nothing in our graph; rustls 0.23 aligns; `aws-lc-rs`/`cmake` are already
//! in our lock via the WebRTC stack, so `cargo deny` stays green) — the
//! rejection is **API fit**, verified in its sources:
//!
//! 1. Its `CastDevice` hardwires a `StreamOwned<ClientConnection, TcpStream>`
//!    created internally with **no read timeout** and no way to set one (the
//!    `TcpStream` is private; `connect_to_device` is private).
//! 2. Its `thread_safe` feature guards `send` and `receive` with the **same
//!    `Mutex`**, and the reader holds that lock through a blocking
//!    `read_exact`. A silent device therefore wedges the reader **and**
//!    every sender on the lock — the mandated heartbeat (PING every 10 s,
//!    dead after 20 s, reconnect at 5 s — ADR-M011) is unimplementable: the
//!    actor would hang for the TCP stack's own timeout, minutes not seconds.
//! 3. Its library code carries `lock().unwrap()` / `expect(...)` panics on
//!    paths a long-lived session actor exercises.
//!
//! The hand-rolled channel is the ADR's sanctioned fallback: ~400 lines of
//! `prost`(-derive, no build step, no protoc) + `tokio-rustls` implementing
//! the `CastMessage` schema from the **BSD-3-Clause Chromium Open Screen
//! sources** — async end to end (timeouts and select-driven heartbeats are
//! native), no new licence surface (`prost` Apache-2.0, rustls already in
//! the graph), and every byte of driver logic above the
//! [`CastChannel`](super::session::CastChannel) seam stays socket-free
//! testable. Google Cast and Chromecast are trademarks of Google LLC.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::device::net_guard::{self, DialPolicy};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{self, pki_types};
use tokio_rustls::TlsConnector;

use super::media::split_authority;
use super::protocol::CastFrame;
use super::session::{CastChannel, CastChannelError, CastConnector};

/// The CASTV2 protocol version token (`CASTV2_1_0`).
const CASTV2_1_0: i32 = 0;
/// The `payload_type` token for a UTF-8 string payload.
const PAYLOAD_STRING: i32 = 0;

/// The maximum accepted frame body (the CASTV2 message bound is 64 KiB): a
/// length prefix beyond this is rejected before any allocation — bounded
/// memory, never an attacker-sized buffer.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// How long a TCP+TLS dial may take before the attempt is declared refused
/// (the session actor then rides its 5 s reconnect cadence).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one frame write may take before the channel is declared dead: a
/// device that accepts the TLS session but stops draining its socket must
/// not wedge the actor's send path (the steady loop services heartbeats and
/// control commands between sends). On expiry the write errors and the
/// session actor rides its supervised reconnect.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The wire `CastMessage` (BSD-3-Clause Chromium Open Screen
/// `cast_channel.proto`), hand-declared with prost-derive — no protoc, no
/// build step. Every field is modelled `optional` and **always populated on
/// encode**, so the proto2-required fields (`protocol_version`,
/// `payload_type`, the ids, the namespace) are emitted even at their default
/// values (a plain prost field would skip a zero — and the receiver rejects
/// a message missing a required field).
#[derive(Clone, PartialEq, prost::Message)]
struct WireCastMessage {
    /// `CastMessage.protocol_version` (always `CASTV2_1_0`).
    #[prost(int32, optional, tag = "1")]
    protocol_version: Option<i32>,
    /// `CastMessage.source_id`.
    #[prost(string, optional, tag = "2")]
    source_id: Option<String>,
    /// `CastMessage.destination_id`.
    #[prost(string, optional, tag = "3")]
    destination_id: Option<String>,
    /// `CastMessage.namespace`.
    #[prost(string, optional, tag = "4")]
    namespace: Option<String>,
    /// `CastMessage.payload_type` (we only speak STRING payloads).
    #[prost(int32, optional, tag = "5")]
    payload_type: Option<i32>,
    /// `CastMessage.payload_utf8`.
    #[prost(string, optional, tag = "6")]
    payload_utf8: Option<String>,
    /// `CastMessage.payload_binary` (decoded but never produced — the four
    /// namespaces this driver speaks are all JSON-over-string).
    #[prost(bytes = "vec", optional, tag = "7")]
    payload_binary: Option<Vec<u8>>,
}

/// Write one frame: 4-byte big-endian length prefix + protobuf body, bounded
/// by [`WRITE_TIMEOUT`].
///
/// # Errors
///
/// [`CastChannelError`] on a write failure or a peer that does not drain the
/// frame within the write window (the channel is then dead and the session
/// actor rides its supervised reconnect).
pub async fn write_frame<W: AsyncWrite + Unpin + Send>(
    writer: &mut W,
    frame: &CastFrame,
) -> Result<(), CastChannelError> {
    let message = WireCastMessage {
        protocol_version: Some(CASTV2_1_0),
        source_id: Some(frame.source.clone()),
        destination_id: Some(frame.destination.clone()),
        namespace: Some(frame.namespace.clone()),
        payload_type: Some(PAYLOAD_STRING),
        payload_utf8: Some(frame.payload.clone()),
        payload_binary: None,
    };
    let body = prost::Message::encode_to_vec(&message);
    let len = u32::try_from(body.len()).map_err(|_| CastChannelError {
        message: "cast frame exceeds the u32 length prefix".to_owned(),
    })?;
    let write = async {
        writer
            .write_all(&len.to_be_bytes())
            .await
            .map_err(CastChannelError::new)?;
        writer
            .write_all(&body)
            .await
            .map_err(CastChannelError::new)?;
        writer.flush().await.map_err(CastChannelError::new)
    };
    tokio::time::timeout(WRITE_TIMEOUT, write)
        .await
        .map_err(|_elapsed| CastChannelError {
            message: format!(
                "cast frame write timed out after {WRITE_TIMEOUT:?} (peer not draining)"
            ),
        })?
}

/// Read one frame: the length prefix (bounded by [`MAX_FRAME_LEN`]) + body.
///
/// # Errors
///
/// [`CastChannelError`] on a read failure, an oversized frame, or a body
/// that does not decode as a `CastMessage`.
pub async fn read_frame<R: AsyncRead + Unpin + Send>(
    reader: &mut R,
) -> Result<CastFrame, CastChannelError> {
    let mut prefix = [0_u8; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(CastChannelError::new)?;
    let len = usize::try_from(u32::from_be_bytes(prefix)).map_err(CastChannelError::new)?;
    if len > MAX_FRAME_LEN {
        return Err(CastChannelError {
            message: format!(
                "cast frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte frame bound"
            ),
        });
    }
    let mut body = vec![0_u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(CastChannelError::new)?;
    let message: WireCastMessage =
        prost::Message::decode(body.as_slice()).map_err(CastChannelError::new)?;
    Ok(CastFrame {
        namespace: message.namespace.unwrap_or_default(),
        source: message.source_id.unwrap_or_default(),
        destination: message.destination_id.unwrap_or_default(),
        // A binary payload (none of our namespaces use one) decodes to an
        // empty payload string, which the tolerant protocol decoder maps to
        // `Unknown` — tolerated, never an error.
        payload: message.payload_utf8.unwrap_or_default(),
    })
}

/// The permissive certificate verifier (ADR-M011): Cast devices present
/// self-signed certificates, so the sender accepts the presented certificate
/// — exactly the posture of the established sender ecosystem. The TLS layer
/// still provides transport encryption; it does not authenticate the device
/// (and CASTV2 has no sender authentication either — LAN trust is the
/// documented model).
#[derive(Debug)]
struct AcceptDeviceCert {
    schemes: Vec<rustls::SignatureScheme>,
}

impl rustls::client::danger::ServerCertVerifier for AcceptDeviceCert {
    fn verify_server_cert(
        &self,
        _end_entity: &pki_types::CertificateDer<'_>,
        _intermediates: &[pki_types::CertificateDer<'_>],
        _server_name: &pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}

/// One live CASTV2 channel: TLS over TCP, framed per [`read_frame`] /
/// [`write_frame`].
pub struct TlsCastChannel {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
}

impl CastChannel for TlsCastChannel {
    async fn send(&mut self, frame: CastFrame) -> Result<(), CastChannelError> {
        write_frame(&mut self.stream, &frame).await
    }

    async fn recv(&mut self) -> Result<CastFrame, CastChannelError> {
        read_frame(&mut self.stream).await
    }
}

/// The live connector: dials `host[:port]` (default 8009) with a bounded
/// connect timeout and the permissive device-certificate verifier. Every dial
/// resolves the host and screens the RESOLVED IP against the operator dial
/// policy (SEC-04, CWE-918), so a rebind between session start and (re)connect
/// cannot redirect the actor at an internal host.
pub struct TlsCastConnector {
    connector: TlsConnector,
    dial_policy: Arc<DialPolicy>,
}

impl TlsCastConnector {
    /// Build the connector (one rustls client config shared by every dial) with
    /// the outbound-dial SSRF `dial_policy`.
    ///
    /// # Errors
    ///
    /// [`CastChannelError`] when the rustls client config cannot be built —
    /// only possible if the linked provider supports none of rustls's default
    /// protocol versions, i.e. a broken build. Propagated (never panicked) so
    /// the binary can log it and run without a cast factory.
    pub fn new(dial_policy: Arc<DialPolicy>) -> Result<Self, CastChannelError> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let schemes = provider
            .signature_verification_algorithms
            .supported_schemes();
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(CastChannelError::new)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptDeviceCert { schemes }))
            .with_no_client_auth();
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            dial_policy,
        })
    }
}

/// Resolve `host` to its candidate socket addresses and screen every RESOLVED
/// IP against `policy` — fail-closed — returning only the vetted addresses to
/// dial. An IP literal resolves to itself. Screening the resolved answer (not
/// the name) defeats DNS-rebind.
async fn resolve_screened_addrs(
    host: &str,
    port: u16,
    policy: &DialPolicy,
) -> Result<Vec<SocketAddr>, CastChannelError> {
    let addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        net_guard::screen_ip(ip, policy).map_err(CastChannelError::new)?;
        vec![SocketAddr::new(ip, port)]
    } else {
        let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(CastChannelError::new)?
            .collect();
        net_guard::screen_resolved(resolved.iter().map(SocketAddr::ip), policy)
            .map_err(CastChannelError::new)?;
        resolved
    };
    if addrs.is_empty() {
        return Err(CastChannelError {
            message: format!("cast host {host:?} did not resolve to a dialable address"),
        });
    }
    Ok(addrs)
}

impl CastConnector for TlsCastConnector {
    type Channel = TlsCastChannel;

    async fn connect(&self, authority: &str) -> Result<TlsCastChannel, CastChannelError> {
        let (host, port) = split_authority(authority).ok_or_else(|| CastChannelError {
            message: format!("cast authority {authority:?} is not a valid host[:port]"),
        })?;
        // SSRF guard (SEC-04): resolve + screen the RESOLVED IP, then dial ONLY
        // a vetted address (pinned) — every reconnect re-screens, defeating a
        // DNS-rebind after the start-time check.
        let vetted = resolve_screened_addrs(&host, port, &self.dial_policy).await?;
        // SNI is irrelevant under the permissive verifier; an IP literal
        // becomes ServerName::IpAddress, a name stays a DNS name.
        let server_name =
            pki_types::ServerName::try_from(host.clone()).map_err(CastChannelError::new)?;
        // One bounded window covers the whole dial (TCP connect + TLS
        // handshake), so a device that accepts the socket but stalls the
        // handshake still times out into the supervised-reconnect path.
        let dial = async {
            let tcp = TcpStream::connect(vetted.as_slice())
                .await
                .map_err(CastChannelError::new)?;
            self.connector
                .connect(server_name, tcp)
                .await
                .map_err(CastChannelError::new)
        };
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, dial)
            .await
            .map_err(|_| CastChannelError {
                message: format!("cast connect to {authority} timed out"),
            })??;
        Ok(TlsCastChannel { stream })
    }
}
