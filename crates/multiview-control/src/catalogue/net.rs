//! The real catalogue sync [`Fetcher`]: reqwest (rustls), bound to the fixed
//! upstream host, redirects off, `If-None-Match`, 64 MiB/file, 60 s/request.

use std::time::Duration;

use super::store::maybe_gunzip;
use super::sync::{BoxFuture, FetchOutcome, Fetcher, UPSTREAM_BASE};

/// Per-file download cap.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// Per-request wall clock.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Fetches from [`UPSTREAM_BASE`].
#[derive(Debug, Clone)]
pub struct HttpFetcher {
    client: reqwest::Client,
    base: String,
}

impl HttpFetcher {
    /// The production fetcher (fixed host).
    ///
    /// # Errors
    ///
    /// A message when the HTTP client cannot be built.
    pub fn new() -> Result<Self, String> {
        Self::with_base(UPSTREAM_BASE.to_owned())
    }

    /// A fetcher against another base (tests use a local `http://[::1]:<port>/` server).
    ///
    /// # Errors
    ///
    /// A message when the HTTP client cannot be built.
    pub fn with_base(base: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("conspect-catalogue/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, base })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch<'a>(
        &'a self,
        name: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
        Box::pin(async move {
            let url = format!("{}{name}.json", self.base);
            let mut req = self.client.get(&url).header("Accept-Encoding", "gzip");
            if let Some(e) = etag {
                req = req.header("If-None-Match", e);
            }
            let mut resp = req.send().await.map_err(|e| e.to_string())?;
            match resp.status().as_u16() {
                304 => return Ok(FetchOutcome::NotModified),
                200 => {}
                other => return Err(format!("upstream answered HTTP {other}")),
            }
            let new_etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            if resp.content_length().is_some_and(|n| n > MAX_BODY_BYTES) {
                return Err("upstream file exceeds the 64 MiB cap".to_owned());
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
                let total = u64::try_from(body.len() + chunk.len()).unwrap_or(u64::MAX);
                if total > MAX_BODY_BYTES {
                    return Err("upstream file exceeds the 64 MiB cap".to_owned());
                }
                body.extend_from_slice(&chunk);
            }
            let bytes = maybe_gunzip(body, MAX_BODY_BYTES).map_err(|e| e.to_string())?;
            Ok(FetchOutcome::Body {
                bytes,
                etag: new_etag,
            })
        })
    }
}
