//! Bounded server-side logo cache: the browser never loads third-party images.
//! Logos are fetched through the shared screened `iptv::fetch` transport,
//! image content types only, ≤ 512 KiB each, and kept in an LRU-ish in-memory map
//! bounded by entry count and total bytes. Control plane only.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::state::AppState;

const MAX_LOGO_BYTES: usize = 512 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_TOTAL_BYTES: usize = 48 * 1024 * 1024;
const ALLOWED: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/svg+xml",
];

#[derive(Default)]
struct Cache {
    map: HashMap<String, (Vec<u8>, String)>,
    order: VecDeque<String>,
    total: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Cache::default()))
}

/// Fetch (or serve from cache) the logo for `key` at `url`.
///
/// # Errors
///
/// A message when the URL is refused by the guard, not an image, or too large.
pub async fn fetch(state: &AppState, key: &str, url: &str) -> Result<(Vec<u8>, String), String> {
    fetch_with(
        &*crate::iptv::fetch::default_fetcher(),
        &state.dial_policy,
        key,
        url,
    )
    .await
}

/// [`fetch`] over an explicit transport (the shared SSRF-screened fetcher seam).
///
/// # Errors
///
/// As [`fetch`].
pub async fn fetch_with(
    fetcher: &dyn crate::iptv::fetch::Fetcher,
    policy: &std::sync::Arc<multiview_config::device::net_guard::DialPolicy>,
    key: &str,
    url: &str,
) -> Result<(Vec<u8>, String), String> {
    use std::io::Read as _;
    if let Some(hit) = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map
        .get(key)
        .cloned()
    {
        return Ok(hit);
    }
    let request = crate::iptv::fetch::FetchRequest {
        url: url.to_owned(),
        max_bytes: MAX_LOGO_BYTES as u64,
        timeout: Duration::from_secs(10),
        etag: None,
        last_modified: None,
    };
    let crate::iptv::fetch::FetchOutcome::Body {
        mut file,
        content_type,
        ..
    } = fetcher
        .fetch(request, std::sync::Arc::clone(policy))
        .await?
    else {
        return Err("upstream answered HTTP 304".to_owned());
    };
    let ctype = content_type.unwrap_or_default();
    if !ALLOWED.contains(&ctype.as_str()) {
        return Err(format!(
            "content type {ctype:?} is not an allowed image type"
        ));
    }
    let mut body = Vec::new();
    file.by_ref()
        .take(MAX_LOGO_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() > MAX_LOGO_BYTES {
        return Err("logo exceeds the 512 KiB cap".to_owned());
    }
    let mut g = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while g.map.len() >= MAX_ENTRIES || g.total + body.len() > MAX_TOTAL_BYTES {
        let Some(old) = g.order.pop_front() else {
            break;
        };
        if let Some((b, _)) = g.map.remove(&old) {
            g.total = g.total.saturating_sub(b.len());
        }
    }
    g.total += body.len();
    g.order.push_back(key.to_owned());
    g.map.insert(key.to_owned(), (body.clone(), ctype.clone()));
    Ok((body, ctype))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iptv::fetch::{FetchFuture, FetchOutcome, FetchRequest, Fetcher};
    use multiview_config::device::net_guard::DialPolicy;
    use std::io::{Seek, Write};
    use std::sync::Arc;

    struct Stub(Vec<u8>, Option<&'static str>);

    impl Fetcher for Stub {
        fn fetch(&self, request: FetchRequest, _: Arc<DialPolicy>) -> FetchFuture<'_> {
            assert_eq!(request.max_bytes, 512 * 1024);
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&self.0).unwrap();
            file.rewind().unwrap();
            let out = FetchOutcome::Body {
                file,
                len: self.0.len() as u64,
                etag: None,
                last_modified: None,
                content_type: self.1.map(str::to_owned),
            };
            Box::pin(async move { Ok(out) })
        }
    }

    #[tokio::test]
    async fn image_types_only_and_cached() {
        let policy = Arc::new(DialPolicy::allow_lan());
        let ok = fetch_with(
            &Stub(b"PNG".to_vec(), Some("image/png")),
            &policy,
            "k-ok",
            "https://h/a.png",
        )
        .await
        .unwrap();
        assert_eq!(ok, (b"PNG".to_vec(), "image/png".to_owned()));
        let html = fetch_with(
            &Stub(b"<html>".to_vec(), Some("text/html")),
            &policy,
            "k-html",
            "https://h/a",
        )
        .await
        .unwrap_err();
        assert!(html.contains("not an allowed image type"), "{html}");
        let none = fetch_with(&Stub(b"x".to_vec(), None), &policy, "k-none", "https://h/a")
            .await
            .unwrap_err();
        assert!(none.contains("not an allowed"), "{none}");
        // Second call is served from the cache without touching the transport.
        let hit = fetch_with(
            &Stub(b"other".to_vec(), Some("image/png")),
            &policy,
            "k-ok",
            "https://h/a.png",
        )
        .await
        .unwrap();
        assert_eq!(hit.0, b"PNG".to_vec());
    }
}
