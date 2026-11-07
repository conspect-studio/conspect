//! The production catalogue sync [`Fetcher`]: an adapter over the shared
//! [`crate::iptv::fetch`] transport (SSRF-screened through `net_guard`,
//! conditional `ETag`, byte cap, overall timeout), bound to the fixed upstream
//! host, 64 MiB/file, 60 s/request.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use multiview_config::device::net_guard::DialPolicy;

use super::store::maybe_gunzip;
use super::sync::{BoxFuture, FetchOutcome, Fetcher, UPSTREAM_BASE};
use crate::iptv::fetch as shared;

/// Per-file download cap.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// Per-request wall clock.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Fetches `<base><name>.json` through the shared fetcher.
pub struct UpstreamFetcher {
    inner: Arc<dyn shared::Fetcher>,
    policy: Arc<DialPolicy>,
    base: String,
}

impl std::fmt::Debug for UpstreamFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamFetcher")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl UpstreamFetcher {
    /// The production fetcher: fixed [`UPSTREAM_BASE`], the real transport.
    #[must_use]
    pub fn new(policy: Arc<DialPolicy>) -> Self {
        Self::with_parts(shared::default_fetcher(), policy, UPSTREAM_BASE.to_owned())
    }

    /// A fetcher over another transport/base (tests).
    #[must_use]
    pub fn with_parts(
        inner: Arc<dyn shared::Fetcher>,
        policy: Arc<DialPolicy>,
        base: String,
    ) -> Self {
        Self {
            inner,
            policy,
            base,
        }
    }
}

impl Fetcher for UpstreamFetcher {
    fn fetch<'a>(
        &'a self,
        name: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
        Box::pin(async move {
            let request = shared::FetchRequest {
                url: format!("{}{name}.json", self.base),
                max_bytes: MAX_BODY_BYTES,
                timeout: REQUEST_TIMEOUT,
                etag: etag.map(str::to_owned),
                last_modified: None,
            };
            match self.inner.fetch(request, Arc::clone(&self.policy)).await? {
                shared::FetchOutcome::NotModified => Ok(FetchOutcome::NotModified),
                shared::FetchOutcome::Body { mut file, etag, .. } => {
                    let mut raw = Vec::new();
                    file.by_ref()
                        .take(MAX_BODY_BYTES + 1)
                        .read_to_end(&mut raw)
                        .map_err(|e| format!("spool: {e}"))?;
                    let bytes = maybe_gunzip(raw, MAX_BODY_BYTES).map_err(|e| e.to_string())?;
                    Ok(FetchOutcome::Body { bytes, etag })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::sync::Mutex;

    struct Stub {
        seen: Mutex<Vec<shared::FetchRequest>>,
        reply: Mutex<Option<Result<shared::FetchOutcome, String>>>,
    }

    impl shared::Fetcher for Stub {
        fn fetch(
            &self,
            request: shared::FetchRequest,
            _policy: Arc<DialPolicy>,
        ) -> shared::FetchFuture<'_> {
            self.seen.lock().unwrap().push(request);
            let r = self.reply.lock().unwrap().take().unwrap();
            Box::pin(async move { r })
        }
    }

    fn body(bytes: &[u8]) -> shared::FetchOutcome {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        shared::FetchOutcome::Body {
            file,
            len: u64::try_from(bytes.len()).unwrap(),
            etag: Some("\"e1\"".into()),
            last_modified: None,
            content_type: None,
        }
    }

    #[tokio::test]
    async fn sync_requests_carry_the_caps_etag_and_fixed_url() {
        let stub = Arc::new(Stub {
            seen: Mutex::new(Vec::new()),
            reply: Mutex::new(Some(Ok(body(b"[]")))),
        });
        let f = UpstreamFetcher::with_parts(
            stub.clone(),
            Arc::new(DialPolicy::allow_lan()),
            "https://iptv-org.github.io/api/".into(),
        );
        let out = f.fetch("streams", Some("\"old\"")).await.unwrap();
        assert_eq!(
            out,
            FetchOutcome::Body {
                bytes: b"[]".to_vec(),
                etag: Some("\"e1\"".into())
            }
        );
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen[0].url, "https://iptv-org.github.io/api/streams.json");
        assert_eq!(seen[0].max_bytes, 64 * 1024 * 1024);
        assert_eq!(seen[0].timeout, Duration::from_secs(60));
        assert_eq!(seen[0].etag.as_deref(), Some("\"old\""));
    }

    #[tokio::test]
    async fn not_modified_and_errors_pass_through() {
        let stub = Arc::new(Stub {
            seen: Mutex::new(Vec::new()),
            reply: Mutex::new(Some(Ok(shared::FetchOutcome::NotModified))),
        });
        let f = UpstreamFetcher::with_parts(
            stub,
            Arc::new(DialPolicy::allow_lan()),
            "https://h/".into(),
        );
        assert_eq!(f.fetch("x", None).await.unwrap(), FetchOutcome::NotModified);
        let stub = Arc::new(Stub {
            seen: Mutex::new(Vec::new()),
            reply: Mutex::new(Some(Err("boom".into()))),
        });
        let f = UpstreamFetcher::with_parts(
            stub,
            Arc::new(DialPolicy::allow_lan()),
            "https://h/".into(),
        );
        assert_eq!(f.fetch("x", None).await.unwrap_err(), "boom");
    }
}
