//! SSRF-screened HTTPS egress for the ACME engine (CA directory + DNS API).
//!
//! Every destination is screened with [`net_guard::classify`] before a byte is
//! sent:
//!
//! * an IP-literal host is screened here, before the request (reqwest dials
//!   literals without consulting any resolver);
//! * a DNS name is resolved ONCE by [`ScreeningResolver`], every resolved
//!   address is screened, and reqwest dials exactly those addresses (no second
//!   resolution, so no DNS-rebinding window);
//! * every redirect hop (at most [`MAX_REDIRECTS`]) is re-screened: scheme
//!   `https` plus the literal screen, names again through the resolver;
//! * `http://` is refused before sending, including as a redirect target;
//! * no proxy is consulted; bodies are read in chunks under a hard cap.
//!
//! Production policy refuses loopback, link-local, RFC 1918, ULA, CGNAT,
//! multicast, unspecified, broadcast and every IPv4-mapped/compatible form. `allow_private` (the config's `allow_private_directory`, ACME
//! directory client only) admits RFC 1918 / ULA / CGNAT and loopback, never
//! link-local, multicast, unspecified or broadcast.
//!
//! Test seam: `EgressPolicy::for_tests_allowing_loopback_ports` (feature
//! `_test-seams`, never in a release preset, not reachable from config) lets
//! an in-process fixture on `[::1]:<port>` be dialled by IP literal.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use multiview_config::device::net_guard::{classify, BlockClass};

use super::AcmeError;

/// Most redirect hops followed for one request.
pub const MAX_REDIRECTS: usize = 3;
/// Body cap for ACME responses (directory, orders, certificate chains).
pub const ACME_BODY_CAP: usize = 1024 * 1024;
/// Body cap for DNS-provider API responses.
pub const DNS_API_BODY_CAP: usize = 1024 * 1024;
/// Largest trust-anchor PEM bundle accepted.
const ANCHOR_CAP: u64 = 1024 * 1024;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Connection deadlines. Production uses [`EgressTimeouts::default`]
/// (connect 10 s, whole request 30 s); tests may shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressTimeouts {
    /// TCP connect deadline.
    pub connect: Duration,
    /// Whole-request deadline (connect + TLS + headers + body).
    pub request: Duration,
}

impl Default for EgressTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            request: Duration::from_secs(30),
        }
    }
}

/// Which destinations the egress client may dial.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Admit RFC 1918 / ULA / CGNAT and loopback (ACME directory only).
    pub allow_private: bool,
    /// Loopback ports dialable by IP literal (test seam only; empty in production).
    loopback_ports: Vec<u16>,
}

/// Why a destination was refused, carried through reqwest's error chain so
/// the caller can classify it as [`AcmeError::Egress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("egress refused: {0}")]
pub struct EgressRefused(pub &'static str);

impl EgressPolicy {
    /// Public destinations only (the DNS API, authoritative nameservers).
    #[must_use]
    pub fn public() -> Self {
        Self::default()
    }

    /// The ACME directory policy: public, plus private ranges when
    /// `allow_private_directory` is set.
    #[must_use]
    pub fn directory(allow_private: bool) -> Self {
        Self {
            allow_private,
            loopback_ports: Vec::new(),
        }
    }

    /// TEST SEAM: additionally allow loopback IP literals on exactly these ports.
    #[cfg(feature = "_test-seams")]
    #[must_use]
    pub fn for_tests_allowing_loopback_ports(allow_private: bool, ports: &[u16]) -> Self {
        Self {
            allow_private,
            loopback_ports: ports.to_vec(),
        }
    }

    /// Screen one address (`port` 0 = unknown, as for a resolved name).
    ///
    /// # Errors
    /// [`EgressRefused`] naming the refused class.
    pub fn screen_ip(&self, ip: IpAddr, port: u16) -> Result<(), EgressRefused> {
        if let IpAddr::V6(v6) = ip {
            // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`)
            // forms are always refused: a CA or DNS API is dialled by its real
            // address, and these forms only exist to dodge a naive screen.
            if !v6.is_loopback()
                && !v6.is_unspecified()
                && (v6.to_ipv4_mapped().is_some() || v6.to_ipv4().is_some())
            {
                return Err(EgressRefused("ipv4-mapped"));
            }
        }
        let Some(class) = classify(ip) else {
            return Ok(());
        };
        if class == BlockClass::Loopback && port != 0 && self.loopback_ports.contains(&port) {
            return Ok(());
        }
        let private = matches!(
            class,
            BlockClass::Loopback
                | BlockClass::Private
                | BlockClass::UniqueLocal
                | BlockClass::Carrier
        );
        if private && self.allow_private {
            return Ok(());
        }
        Err(EgressRefused(match class {
            BlockClass::Loopback => "loopback",
            BlockClass::LinkLocal => "link-local",
            BlockClass::Unspecified => "unspecified",
            BlockClass::Multicast => "multicast",
            BlockClass::Broadcast => "broadcast",
            BlockClass::Private => "private",
            BlockClass::UniqueLocal => "unique-local",
            BlockClass::Carrier => "cgnat",
            _ => "reserved",
        }))
    }

    /// Screen a URL before sending: HTTPS only, no credentials, literal host screened.
    ///
    /// # Errors
    /// [`EgressRefused`] for a non-HTTPS scheme, a missing host or a refused literal.
    pub fn screen_url(&self, url: &url::Url) -> Result<(), EgressRefused> {
        if url.scheme() != "https" {
            return Err(EgressRefused("not-https"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EgressRefused("credentials-in-url"));
        }
        let port = url.port_or_known_default().unwrap_or(443);
        match url.host() {
            Some(url::Host::Ipv4(v4)) => self.screen_ip(IpAddr::V4(v4), port),
            Some(url::Host::Ipv6(v6)) => self.screen_ip(IpAddr::V6(v6), port),
            Some(url::Host::Domain(_)) => Ok(()),
            None => Err(EgressRefused("no-host")),
        }
    }
}

/// reqwest resolver: resolve once, screen EVERY address, return exactly those.
#[derive(Debug)]
pub struct ScreeningResolver {
    policy: EgressPolicy,
}

impl reqwest::dns::Resolve for ScreeningResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy.clone();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|e| -> BoxError { Box::new(e) })?
                .collect();
            if resolved.is_empty() {
                let refused: BoxError = Box::new(EgressRefused("no-address"));
                return Err(refused);
            }
            for addr in &resolved {
                policy
                    .screen_ip(addr.ip(), 0)
                    .map_err(|e| -> BoxError { Box::new(e) })?;
            }
            let addrs: reqwest::dns::Addrs = Box::new(resolved.into_iter());
            Ok(addrs)
        })
    }
}

/// How an egress request failed (no request data, no secrets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    /// The egress screen refused the destination, or TLS trust failed.
    #[error("egress refused")]
    Refused,
    /// The connection failed or timed out.
    #[error("transport failed")]
    Transport,
    /// The response body exceeded its cap.
    #[error("response body too large")]
    TooLarge,
}

impl EgressError {
    /// Map to an engine category; `transport` is the category for a plain
    /// network failure (the CA → `Authority`, the DNS API → `DnsApi`).
    #[must_use]
    pub const fn category(self, transport: AcmeError) -> AcmeError {
        match self {
            Self::Refused | Self::TooLarge => AcmeError::Egress,
            Self::Transport => transport,
        }
    }
}

/// A buffered, bounded response.
#[derive(Debug)]
pub struct EgressResponse {
    /// Status code.
    pub status: http::StatusCode,
    /// Response headers.
    pub headers: http::HeaderMap,
    /// Body, at most the client's cap.
    pub body: Bytes,
}

/// The screened HTTPS client.
#[derive(Clone)]
pub struct EgressClient {
    client: reqwest::Client,
    policy: EgressPolicy,
    body_cap: usize,
}

impl std::fmt::Debug for EgressClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressClient")
            .field("policy", &self.policy)
            .field("body_cap", &self.body_cap)
            .finish_non_exhaustive()
    }
}

/// Read a PEM trust-anchor bundle without following a symlink at the leaf.
fn read_anchor(path: &Path) -> Result<Vec<reqwest::Certificate>, AcmeError> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Read;
    crate::web_capture_startup::reject_symlink(path).map_err(|_| AcmeError::Egress)?;
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| AcmeError::Egress)?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| AcmeError::Egress)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(AcmeError::Egress);
    }
    let mut pem = Vec::new();
    std::fs::File::from(fd)
        .take(ANCHOR_CAP + 1)
        .read_to_end(&mut pem)
        .map_err(|_| AcmeError::Egress)?;
    if u64::try_from(pem.len()).unwrap_or(u64::MAX) > ANCHOR_CAP {
        return Err(AcmeError::Egress);
    }
    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| AcmeError::Egress)?;
    if certs.is_empty() {
        return Err(AcmeError::Egress);
    }
    Ok(certs)
}

impl EgressClient {
    /// Build the client. With `trust_anchor`, ONLY that PEM bundle is trusted
    /// (built-in roots off); otherwise the webpki roots.
    ///
    /// # Errors
    /// [`AcmeError::Egress`] if the anchor cannot be read safely or the client
    /// cannot be built.
    pub fn new(
        policy: EgressPolicy,
        trust_anchor: Option<&Path>,
        timeouts: EgressTimeouts,
        body_cap: usize,
    ) -> Result<Self, AcmeError> {
        let redirect_policy = policy.clone();
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .https_only(true)
            .connect_timeout(timeouts.connect)
            .timeout(timeouts.request)
            .dns_resolver(Arc::new(ScreeningResolver {
                policy: policy.clone(),
            }))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(EgressRefused("too-many-redirects"));
                }
                match redirect_policy.screen_url(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(refused) => attempt.error(refused),
                }
            }));
        if let Some(anchor) = trust_anchor {
            builder = builder.tls_built_in_root_certs(false);
            for cert in read_anchor(anchor)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder.build().map_err(|_| AcmeError::Egress)?;
        Ok(Self {
            client,
            policy,
            body_cap,
        })
    }

    /// The underlying reqwest client (to build requests).
    #[must_use]
    pub fn reqwest(&self) -> &reqwest::Client {
        &self.client
    }

    /// Screen, send and read a bounded body.
    ///
    /// # Errors
    /// [`EgressError`] (no request data in it).
    pub async fn send(&self, request: reqwest::Request) -> Result<EgressResponse, EgressError> {
        self.policy
            .screen_url(request.url())
            .map_err(|_| EgressError::Refused)?;
        let mut response = self
            .client
            .execute(request)
            .await
            .map_err(|e| classify_error(&e))?;
        let status = response.status();
        let headers = response.headers().clone();
        if response
            .content_length()
            .is_some_and(|len| usize::try_from(len).map_or(true, |len| len > self.body_cap))
        {
            return Err(EgressError::TooLarge);
        }
        let mut body = BytesMut::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| classify_error(&e))? {
            if body.len().saturating_add(chunk.len()) > self.body_cap {
                return Err(EgressError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(EgressResponse {
            status,
            headers,
            body: body.freeze(),
        })
    }
}

/// Walk an error chain: a screen refusal or a rustls trust failure is
/// `Refused`; anything else is a transport failure.
fn classify_error(error: &(dyn std::error::Error + 'static)) -> EgressError {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if e.is::<EgressRefused>() || e.is::<rustls::Error>() {
            return EgressError::Refused;
        }
        // `io::Error::source` skips its custom payload; look inside explicitly
        // (hyper-rustls wraps the rustls error in an io::Error).
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            // Possibly nested io::Error payloads (reqwest → hyper → tokio-rustls).
            current = Some(inner);
            continue;
        }
        current = e.source();
    }
    EgressError::Transport
}

/// Error passed through `instant_acme::Error::Other`, so the engine can
/// recover the category after the ACME client returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ACME HTTP failure: {0}")]
pub struct HttpFailure(pub AcmeError);

/// [`instant_acme::HttpClient`] on top of the screened client.
#[derive(Debug, Clone)]
pub struct AcmeHttp {
    client: EgressClient,
}

impl AcmeHttp {
    /// Wrap a screened client (built with [`ACME_BODY_CAP`]).
    #[must_use]
    pub fn new(client: EgressClient) -> Self {
        Self { client }
    }
}

impl instant_acme::HttpClient for AcmeHttp {
    fn request(
        &self,
        req: http::Request<instant_acme::BodyWrapper<Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let client = self.client.clone();
        Box::pin(async move {
            use http_body_util::BodyExt;
            let fail = |category| instant_acme::Error::Other(Box::new(HttpFailure(category)));
            let (parts, body) = req.into_parts();
            let body = body
                .collect()
                .await
                .map(http_body_util::Collected::to_bytes)
                .unwrap_or_default();
            let url =
                url::Url::parse(&parts.uri.to_string()).map_err(|_| fail(AcmeError::Egress))?;
            let mut request = reqwest::Request::new(parts.method, url);
            *request.headers_mut() = parts.headers;
            *request.body_mut() = Some(reqwest::Body::from(body));
            let response = client
                .send(request)
                .await
                .map_err(|e| fail(e.category(AcmeError::Authority)))?;
            let mut builder = http::Response::builder().status(response.status);
            if let Some(headers) = builder.headers_mut() {
                *headers = response.headers;
            }
            let (parts, ()) = builder
                .body(())
                .map_err(|_| fail(AcmeError::Authority))?
                .into_parts();
            Ok(instant_acme::BytesResponse {
                parts,
                body: Box::new(response.body),
            })
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn production_screen_refuses_every_non_public_class() {
        let p = EgressPolicy::public();
        for s in [
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "fd00::1",
            "169.254.169.254",
            "fe80::1",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
            "::ffff:10.0.0.1",
            "::ffff:8.8.8.8",
        ] {
            assert!(p.screen_ip(ip(s), 443).is_err(), "{s}");
        }
        for s in ["8.8.8.8", "2001:4860:4860::8888"] {
            assert!(p.screen_ip(ip(s), 443).is_ok(), "{s}");
        }
    }

    #[test]
    fn allow_private_admits_lan_and_loopback_only() {
        let p = EgressPolicy::directory(true);
        for s in [
            "127.0.0.1",
            "::1",
            "10.0.0.1",
            "100.64.0.1",
            "fd00::1",
            "192.168.0.9",
        ] {
            assert!(p.screen_ip(ip(s), 443).is_ok(), "{s}");
        }
        for s in [
            "169.254.169.254",
            "fe80::1",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(p.screen_ip(ip(s), 443).is_err(), "{s}");
        }
    }

    #[test]
    fn url_screen_refuses_http_credentials_and_private_literals() {
        let p = EgressPolicy::public();
        for u in [
            "http://acme.example.org/dir",
            "https://user:pw@acme.example.org/dir",
            "https://[::1]/dir",
            "https://127.0.0.1/dir",
            "https://2130706433/dir",
            "https://[::ffff:127.0.0.1]/dir",
            "https://169.254.169.254/latest",
        ] {
            assert!(p.screen_url(&url::Url::parse(u).unwrap()).is_err(), "{u}");
        }
        assert!(p
            .screen_url(&url::Url::parse("https://acme.example.org/dir").unwrap())
            .is_ok());
    }

    #[cfg(feature = "_test-seams")]
    #[test]
    fn loopback_seam_admits_only_named_ports() {
        let p = EgressPolicy::for_tests_allowing_loopback_ports(false, &[4321]);
        assert!(p.screen_ip(ip("::1"), 4321).is_ok());
        assert!(p.screen_ip(ip("::1"), 4322).is_err());
        assert!(p.screen_ip(ip("::1"), 0).is_err());
        assert!(p.screen_ip(ip("10.0.0.1"), 4321).is_err());
    }

    #[tokio::test]
    async fn resolver_screens_every_resolved_address() {
        use reqwest::dns::Resolve;
        let r = ScreeningResolver {
            policy: EgressPolicy::public(),
        };
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        assert!(r.resolve(name).await.is_err());
        let r = ScreeningResolver {
            policy: EgressPolicy::directory(true),
        };
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let addrs: Vec<_> = r.resolve(name).await.unwrap().collect();
        assert_ne!(addrs.len(), 0);
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }

    #[test]
    fn anchor_must_not_be_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("root.pem");
        std::fs::write(&real, b"not a pem").unwrap();
        assert_eq!(read_anchor(&real).unwrap_err(), AcmeError::Egress);
        let link = dir.path().join("link.pem");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(read_anchor(&link).unwrap_err(), AcmeError::Egress);
    }

    #[test]
    fn categories_map_transport_to_caller_choice() {
        assert_eq!(
            EgressError::Transport.category(AcmeError::DnsApi),
            AcmeError::DnsApi
        );
        assert_eq!(
            EgressError::Refused.category(AcmeError::DnsApi),
            AcmeError::Egress
        );
        assert_eq!(
            EgressError::TooLarge.category(AcmeError::Authority),
            AcmeError::Egress
        );
    }
}
