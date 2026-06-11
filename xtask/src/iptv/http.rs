//! The real, network-backed [`SourceCatalog`] + [`Prober`] (the `net` feature).
//!
//! This is the only part of the tool that touches the network. It is compiled
//! ONLY under the off-by-default `net` feature so the default build (and
//! `cargo deny`, which scans `all-features = false`) never links an HTTP/TLS
//! stack. `ureq` is a small blocking rustls-backed client; each blocking call
//! is run on a `tokio::task::spawn_blocking` worker so the async seam holds.
//!
//! Liveness probing replays the per-stream `user_agent` / `referrer` (origins
//! frequently 403 without them) and accepts any 2xx/3xx response whose body
//! begins like a readable HLS/DASH playlist, treating everything else as dead.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::iptv::error::IptvError;
use crate::iptv::fetch::{Catalog, ProbeOutcome, Prober, SourceCatalog};
use crate::iptv::model::{Channel, Stream};

/// iptv-org public API endpoints.
const STREAMS_URL: &str = "https://iptv-org.github.io/api/streams.json";
const CHANNELS_URL: &str = "https://iptv-org.github.io/api/channels.json";

/// A network catalog that fetches the live iptv-org API.
#[derive(Debug, Clone)]
pub struct HttpCatalog {
    request_timeout: Duration,
}

impl Default for HttpCatalog {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl HttpCatalog {
    /// Build a network catalog with a per-request timeout.
    #[must_use]
    pub fn new(request_timeout: Duration) -> Self {
        Self { request_timeout }
    }
}

/// Fetch + parse a single JSON document with `ureq`, mapping every failure into
/// a typed [`IptvError::Http`].
fn fetch_json<T: serde::de::DeserializeOwned>(
    url: &'static str,
    what: &'static str,
    timeout: Duration,
) -> Result<T, IptvError> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent.get(url).call().map_err(|e| IptvError::Http {
        what,
        url: url.to_owned(),
        message: e.to_string(),
    })?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| IptvError::Http {
            what,
            url: url.to_owned(),
            message: e.to_string(),
        })?;
    serde_json::from_str::<T>(&body).map_err(|e| IptvError::Http {
        what,
        url: url.to_owned(),
        message: format!("parsing JSON: {e}"),
    })
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl SourceCatalog for HttpCatalog {
    fn fetch(&self) -> BoxFuture<'_, Result<Catalog, IptvError>> {
        let timeout = self.request_timeout;
        Box::pin(async move {
            let streams = tokio::task::spawn_blocking(move || {
                fetch_json::<Vec<Stream>>(STREAMS_URL, "streams", timeout)
            })
            .await
            .map_err(|e| IptvError::Http {
                what: "streams",
                url: STREAMS_URL.to_owned(),
                message: format!("join error: {e}"),
            })??;
            let channels = tokio::task::spawn_blocking(move || {
                fetch_json::<Vec<Channel>>(CHANNELS_URL, "channels", timeout)
            })
            .await
            .map_err(|e| IptvError::Http {
                what: "channels",
                url: CHANNELS_URL.to_owned(),
                message: format!("join error: {e}"),
            })??;
            Ok((streams, channels))
        })
    }
}

/// A network prober that opens the master playlist/manifest.
#[derive(Debug, Clone)]
pub struct HttpProber {
    request_timeout: Duration,
}

impl Default for HttpProber {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(8),
        }
    }
}

impl HttpProber {
    /// Build a network prober with a per-probe timeout.
    #[must_use]
    pub fn new(request_timeout: Duration) -> Self {
        Self { request_timeout }
    }
}

/// A response body looks like a readable HLS/DASH playlist if it begins with the
/// HLS tag or contains a DASH `<MPD` root (best-effort; the goal is "the origin
/// served something playlist-shaped", not full validation).
fn looks_like_playlist(body: &str) -> bool {
    let head = body.trim_start();
    head.starts_with("#EXTM3U") || head.contains("<MPD") || head.contains("#EXT-X-")
}

/// Blocking probe of a single URL, replaying headers; returns the outcome.
fn probe_blocking(
    url: &str,
    user_agent: Option<&str>,
    referrer: Option<&str>,
    timeout: Duration,
) -> ProbeOutcome {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build();
    let agent: ureq::Agent = config.into();
    let mut request = agent.get(url);
    if let Some(ua) = user_agent {
        request = request.header("User-Agent", ua);
    }
    if let Some(referer) = referrer {
        request = request.header("Referer", referer);
    }
    let Ok(mut response) = request.call() else {
        return ProbeOutcome::Dead;
    };
    // Read a bounded prefix — a playlist head is tiny; never slurp a segment.
    match response.body_mut().read_to_string() {
        Ok(body) if looks_like_playlist(&body) => ProbeOutcome::Live,
        _ => ProbeOutcome::Dead,
    }
}

impl Prober for HttpProber {
    fn probe<'a>(
        &'a self,
        url: &'a str,
        user_agent: Option<&'a str>,
        referrer: Option<&'a str>,
    ) -> BoxFuture<'a, ProbeOutcome> {
        let url = url.to_owned();
        let user_agent = user_agent.map(str::to_owned);
        let referrer = referrer.map(str::to_owned);
        let timeout = self.request_timeout;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                probe_blocking(&url, user_agent.as_deref(), referrer.as_deref(), timeout)
            })
            .await
            .unwrap_or(ProbeOutcome::Dead)
        })
    }
}
