//! Bounded, SSRF-screened HTTP(S) fetch for M3U import and XMLTV refresh.
//!
//! The transport is a seam ([`Fetcher`]) so tests drive the refresher with a
//! fake. The real [`HttpFetcher`] (feature `iptv-net`) screens every dial
//! target through `multiview_config::device::net_guard`:
//!
//! * DNS names resolve through [`net_guard::resolve_and_screen`] inside the
//!   client's resolver, so the vetted addresses are the ones dialled (defeats
//!   DNS rebinding);
//! * IP-literal hosts (which bypass the resolver) are screened directly, on the
//!   first request **and on every redirect hop**;
//! * ambient proxies are disabled, redirects are capped, no credentials are
//!   ever sent, and the body is streamed into an anonymous temp file under a
//!   hard byte cap and an overall timeout.
//!
//! Private-network targets are refused unless the caller passes the operator
//! dial policy (an explicit per-feed `allow_private_network`); loopback,
//! link-local/metadata, unspecified and multicast targets are always refused.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use multiview_config::device::net_guard::DialPolicy;

/// One fetch request (owned so the boxed future is `'static`-friendly).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// Absolute http(s) URL.
    pub url: String,
    /// Maximum accepted body bytes (on the wire).
    pub max_bytes: u64,
    /// Overall request timeout.
    pub timeout: Duration,
    /// Prior `ETag` for a conditional request.
    pub etag: Option<String>,
    /// Prior `Last-Modified` for a conditional request.
    pub last_modified: Option<String>,
}

/// A fetch result.
#[derive(Debug)]
pub enum FetchOutcome {
    /// A fresh body, spooled to an anonymous temp file rewound to the start.
    Body {
        /// The spooled body.
        file: std::fs::File,
        /// Its length in bytes.
        len: u64,
        /// Response `ETag`, if any.
        etag: Option<String>,
        /// Response `Last-Modified`, if any.
        last_modified: Option<String>,
        /// Response media type (parameters dropped, lower-cased), if any.
        content_type: Option<String>,
    },
    /// `304 Not Modified` for a conditional request.
    NotModified,
}

/// The boxed future a [`Fetcher`] returns.
pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<FetchOutcome, String>> + Send + 'a>>;

/// The fetch seam. Errors are operator-facing strings that never contain
/// credentials or query strings.
pub trait Fetcher: Send + Sync + 'static {
    /// Fetch `request` under `policy`.
    fn fetch(&self, request: FetchRequest, policy: Arc<DialPolicy>) -> FetchFuture<'_>;
}

/// Redact a URL for logs/status: drop userinfo, query and fragment.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let without_tail = url.split(['?', '#']).next().unwrap_or("");
    match without_tail.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.find('/').map_or((rest, ""), |at| {
                (rest.get(..at).unwrap_or(""), rest.get(at..).unwrap_or(""))
            });
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            let suffix = if url.contains('?') { "?…" } else { "" };
            format!("{scheme}://{host}{path}{suffix}")
        }
        None => "(invalid url)".to_owned(),
    }
}

/// Check the URL shape every fetch requires: absolute http(s), a host, no
/// userinfo, no whitespace/control characters, bounded length.
///
/// # Errors
/// An operator-facing reason.
pub fn check_fetch_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or_else(|| "only http:// and https:// URLs can be fetched".to_owned())?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("the URL has no host".into());
    }
    if authority.contains('@') {
        return Err("URLs with embedded credentials are not fetched".into());
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("the URL is too long or contains whitespace".into());
    }
    Ok(())
}

/// A fetcher for builds without the `iptv-net` transport: every fetch fails
/// with an honest reason (pasted M3U text still imports).
#[derive(Debug, Default)]
pub struct UnavailableFetcher;

impl Fetcher for UnavailableFetcher {
    fn fetch(&self, _request: FetchRequest, _policy: Arc<DialPolicy>) -> FetchFuture<'_> {
        Box::pin(async {
            Err("this build has no outbound HTTP transport (feature `iptv-net`)".to_owned())
        })
    }
}

/// The default fetcher for this build.
#[must_use]
pub fn default_fetcher() -> Arc<dyn Fetcher> {
    #[cfg(feature = "iptv-net")]
    {
        Arc::new(net::HttpFetcher)
    }
    #[cfg(not(feature = "iptv-net"))]
    {
        Arc::new(UnavailableFetcher)
    }
}

#[cfg(feature = "iptv-net")]
pub use net::HttpFetcher;

#[cfg(feature = "iptv-net")]
mod net {
    use std::io::{Seek, Write};
    use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
    use std::sync::Arc;

    use multiview_config::device::net_guard::{self, DialPolicy, HostResolver};

    use super::{check_fetch_url, redact_url, FetchFuture, FetchOutcome, FetchRequest, Fetcher};

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// The maximum redirect hops followed (each re-screened).
    const MAX_REDIRECTS: usize = 5;

    /// The system resolver behind [`net_guard::resolve_and_screen`].
    struct SystemResolver;

    impl HostResolver for SystemResolver {
        fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
            Ok((host, 0).to_socket_addrs()?.map(|addr| addr.ip()).collect())
        }
    }

    /// reqwest resolver: resolve + screen on a blocking thread, return only
    /// the vetted addresses (pinned — defeats DNS rebinding).
    struct ScreeningResolver {
        policy: Arc<DialPolicy>,
    }

    impl reqwest::dns::Resolve for ScreeningResolver {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let policy = Arc::clone(&self.policy);
            let host = name.as_str().to_owned();
            Box::pin(async move {
                let vetted = tokio::task::spawn_blocking(move || {
                    net_guard::resolve_and_screen(&host, &policy, &SystemResolver)
                })
                .await
                .map_err(|e| -> BoxError { Box::new(e) })?
                .map_err(|e| -> BoxError { Box::new(e) })?;
                let addrs: reqwest::dns::Addrs =
                    Box::new(vetted.into_iter().map(|ip| SocketAddr::new(ip, 0)));
                Ok(addrs)
            })
        }
    }

    /// The error plus its `source()` chain, so callers can tell a DNS failure
    /// ("dns error: …") from a timeout or a refused connection.
    fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
        let mut out = e.to_string();
        let mut cur = e.source();
        while let Some(c) = cur {
            out.push_str(": ");
            out.push_str(&c.to_string());
            cur = c.source();
        }
        out
    }

    fn screen_literal(url: &url::Url, policy: &DialPolicy) -> Result<(), String> {
        let ip = match url.host() {
            Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4),
            Some(url::Host::Ipv6(v6)) => IpAddr::V6(v6),
            Some(url::Host::Domain(_)) => return Ok(()),
            None => return Err("the URL has no host".into()),
        };
        net_guard::screen_ip(ip, policy).map_err(|e| e.to_string())
    }

    /// The real reqwest-backed fetcher.
    #[derive(Debug, Default)]
    pub struct HttpFetcher;

    impl Fetcher for HttpFetcher {
        fn fetch(&self, request: FetchRequest, policy: Arc<DialPolicy>) -> FetchFuture<'_> {
            Box::pin(async move { fetch(request, policy).await })
        }
    }

    async fn fetch(request: FetchRequest, policy: Arc<DialPolicy>) -> Result<FetchOutcome, String> {
        check_fetch_url(&request.url)?;
        let shown = redact_url(&request.url);
        let parsed = url::Url::parse(&request.url).map_err(|e| format!("{shown}: {e}"))?;
        screen_literal(&parsed, &policy).map_err(|e| format!("{shown}: {e}"))?;
        let redirect_policy = Arc::clone(&policy);
        let client = reqwest::Client::builder()
            .timeout(request.timeout)
            .connect_timeout(request.timeout.min(std::time::Duration::from_secs(10)))
            .no_proxy()
            .dns_resolver(Arc::new(ScreeningResolver {
                policy: Arc::clone(&policy),
            }))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                let next = attempt.url();
                if !matches!(next.scheme(), "http" | "https")
                    || !next.username().is_empty()
                    || next.password().is_some()
                {
                    return attempt.error("redirect to a disallowed URL");
                }
                if let Err(e) = screen_literal(next, &redirect_policy) {
                    return attempt.error(e);
                }
                attempt.follow()
            }))
            .user_agent(concat!(
                "Conspect-iptv/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/conspect-studio/conspect; catalogue and logo fetcher)"
            ))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut builder = client
            .get(parsed)
            .header("accept-encoding", "gzip, identity");
        if let Some(etag) = &request.etag {
            builder = builder.header("if-none-match", etag);
        }
        if let Some(modified) = &request.last_modified {
            builder = builder.header("if-modified-since", modified);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|e| format!("{shown}: {}", error_chain(&e.without_url())))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchOutcome::NotModified);
        }
        if !response.status().is_success() {
            return Err(format!("{shown}: HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|len| len > request.max_bytes)
        {
            return Err(format!("{shown}: body exceeds {} bytes", request.max_bytes));
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 256)
                .map(str::to_owned)
        };
        let etag = header("etag");
        let last_modified = header("last-modified");
        let content_type = header("content-type").map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
        let mut file = tempfile::tempfile().map_err(|e| format!("spool: {e}"))?;
        let mut len: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("{shown}: {}", e.without_url()))?
        {
            len = len.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if len > request.max_bytes {
                return Err(format!("{shown}: body exceeds {} bytes", request.max_bytes));
            }
            // A short blocking write into a local temp file (page cache).
            file.write_all(&chunk).map_err(|e| format!("spool: {e}"))?;
        }
        file.rewind().map_err(|e| format!("spool: {e}"))?;
        Ok(FetchOutcome::Body {
            file,
            len,
            etag,
            last_modified,
            content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_drops_userinfo_and_query() {
        assert_eq!(
            redact_url("https://u:p@epg.example/x/g.xml?token=s#f"),
            "https://epg.example/x/g.xml?…"
        );
        assert_eq!(redact_url("http://h"), "http://h");
        assert_eq!(redact_url("nope"), "(invalid url)");
    }

    #[test]
    fn fetch_url_shape_is_checked() {
        assert!(check_fetch_url("https://epg.example/g.xml").is_ok());
        for bad in [
            "ftp://h/x",
            "file:///etc/passwd",
            "http://user:pw@h/x",
            "https:///x",
            "http://h/a b",
        ] {
            assert!(check_fetch_url(bad).is_err(), "{bad}");
        }
    }
}
