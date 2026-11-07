//! Bounded server-side logo cache: the browser never loads third-party images.
//! Logos are fetched through the same screened dial guard as probes (redirects off),
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

fn client(state: &AppState) -> Result<reqwest::Client, String> {
    let guard = super::guard::DialGuard::new(state.dial_policy.clone());
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(guard.resolver())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Fetch (or serve from cache) the logo for `key` at `url`.
///
/// # Errors
///
/// A message when the URL is refused by the guard, not an image, or too large.
pub async fn fetch(state: &AppState, key: &str, url: &str) -> Result<(Vec<u8>, String), String> {
    if let Some(hit) = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map
        .get(key)
        .cloned()
    {
        return Ok(hit);
    }
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    super::guard::DialGuard::new(state.dial_policy.clone()).screen_url(&parsed)?;
    let mut resp = client(state)?
        .get(parsed)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 200 {
        return Err(format!("upstream answered HTTP {}", resp.status().as_u16()));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if !ALLOWED.contains(&ctype.as_str()) {
        return Err(format!(
            "content type {ctype:?} is not an allowed image type"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_LOGO_BYTES {
            return Err("logo exceeds the 512 KiB cap".to_owned());
        }
        body.extend_from_slice(&chunk);
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
