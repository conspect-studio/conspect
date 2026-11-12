//! The TLS side of the TCP door: the accepted stream type, the `ClientHello`
//! cap, and the routing a `ClientHello` selects (design note section 4).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// The most bytes read before a `ClientHello` is parsed (design note section
/// 4): a larger hello is closed.
pub const MAX_CLIENT_HELLO: usize = 16 * 1024;

/// The ALPN protocol of a TLS-ALPN-01 challenge (RFC 8737).
pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// The accepted TCP stream handed to TLS. While the `ClientHello` is being
/// read it refuses to read more than [`MAX_CLIENT_HELLO`] bytes; the cap is
/// lifted once the hello is parsed.
#[derive(Debug)]
pub struct DoorStream {
    inner: TcpStream,
    remaining: Option<usize>,
}

impl DoorStream {
    pub(crate) fn capped(inner: TcpStream, cap: usize) -> Self {
        Self {
            inner,
            remaining: Some(cap),
        }
    }

    pub(crate) fn lift_cap(&mut self) {
        self.remaining = None;
    }

    /// The underlying TCP stream.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }
}

impl AsyncRead for DoorStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Some(remaining) = this.remaining else {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        };
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ClientHello larger than the front-door cap",
            )));
        }
        let limit = remaining.min(buf.remaining());
        let dst = buf.initialize_unfilled_to(limit);
        let mut limited = ReadBuf::new(dst);
        let poll = Pin::new(&mut this.inner).poll_read(cx, &mut limited);
        let read = limited.filled().len();
        if poll.is_ready() {
            buf.advance(read);
            this.remaining = Some(remaining - read);
        }
        poll
    }
}

impl AsyncWrite for DoorStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// The server configurations a `ClientHello` is routed to. Replaceable at run
/// time ([`crate::FrontDoor::set_tls_routing`]); read once per handshake.
#[derive(Clone, Default)]
pub struct TlsRouting {
    /// The site (ALPN pinned to `http/1.1` by the caller).
    pub site: Option<Arc<rustls::ServerConfig>>,
    /// The TLS-ALPN-01 challenge responder (ALPN `acme-tls/1`), slice 5.
    pub acme: Option<Arc<rustls::ServerConfig>>,
}

impl std::fmt::Debug for TlsRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsRouting")
            .field("site", &self.site.is_some())
            .field("acme", &self.acme.is_some())
            .finish()
    }
}

/// Where a `ClientHello` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelloRoute {
    Acme,
    TurnReserved,
    Site,
}

/// Route on the `ClientHello`: ALPN exactly `["acme-tls/1"]` is a challenge;
/// SNI equal to the reserved TURN name is closed (slice 8); everything else is
/// the site.
pub(crate) fn route_hello(
    hello: &rustls::server::ClientHello<'_>,
    turn_sni: Option<&str>,
) -> HelloRoute {
    let acme_only = hello
        .alpn()
        .is_some_and(|mut alpn| alpn.next() == Some(ACME_TLS_ALPN) && alpn.next().is_none());
    if acme_only {
        return HelloRoute::Acme;
    }
    if let (Some(turn), Some(name)) = (turn_sni, hello.server_name()) {
        if name.eq_ignore_ascii_case(turn) {
            return HelloRoute::TurnReserved;
        }
    }
    HelloRoute::Site
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Route a real `ClientHello` built by a rustls client with `alpn` and
    /// server name `name` (an IP literal sends no SNI).
    fn route(alpn: &[&[u8]], name: &str, turn_sni: Option<&str>) -> HelloRoute {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let mut client = rustls::ClientConnection::new(
            Arc::new(cfg),
            rustls::pki_types::ServerName::try_from(name.to_owned()).unwrap(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut bytes).unwrap();
        }
        let mut acceptor = rustls::server::Acceptor::default();
        let mut rd = bytes.as_slice();
        while !rd.is_empty() {
            acceptor.read_tls(&mut rd).unwrap();
        }
        let accepted = acceptor
            .accept()
            .map_err(|(e, _)| e)
            .unwrap()
            .expect("a complete ClientHello");
        route_hello(&accepted.client_hello(), turn_sni)
    }

    #[test]
    fn acme_only_when_alpn_is_exactly_acme() {
        assert_eq!(
            route(&[ACME_TLS_ALPN], "example.org", None),
            HelloRoute::Acme
        );
        assert_eq!(
            route(&[ACME_TLS_ALPN, b"http/1.1"], "example.org", None),
            HelloRoute::Site
        );
        assert_eq!(
            route(&[b"http/1.1", ACME_TLS_ALPN], "example.org", None),
            HelloRoute::Site
        );
        assert_eq!(route(&[b"http/1.1"], "example.org", None), HelloRoute::Site);
        assert_eq!(route(&[], "example.org", None), HelloRoute::Site);
    }

    #[test]
    fn turn_sni_is_reserved_case_insensitively() {
        let turn = Some("turn.example.org");
        assert_eq!(
            route(&[b"http/1.1"], "TURN.Example.ORG", turn),
            HelloRoute::TurnReserved
        );
        assert_eq!(
            route(&[b"http/1.1"], "turn.example.org", turn),
            HelloRoute::TurnReserved
        );
        assert_eq!(route(&[b"http/1.1"], "example.org", turn), HelloRoute::Site);
        // No SNI (an IP literal is never sent as SNI): the site.
        assert_eq!(route(&[b"http/1.1"], "::1", turn), HelloRoute::Site);
        assert_eq!(
            route(&[b"http/1.1"], "turn.example.org", None),
            HelloRoute::Site
        );
    }
}
