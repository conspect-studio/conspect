//! The embedded single-page web UI (feature `embed-web`).
//!
//! The built SPA (`web/dist`, produced by the web build) is inlined into the
//! binary with [`rust_embed`] so the management UI ships self-contained — no
//! external file serving, same-origin with the API. [`spa_fallback`] is wired as
//! the control router's [`fallback`](axum::Router::fallback): it runs only for
//! requests no API/NMOS/docs route matched, so it can never shadow `/api/v1`,
//! `/x-nmos`, `/docs`, or `/api/v1/openapi.json`.
//!
//! Serving rules:
//! * a `GET` whose path maps to an embedded asset returns that asset with its
//!   guessed `Content-Type` (and a long immutable cache for the content-hashed
//!   `/assets/*` bundles Vite emits);
//! * any other `GET` returns `index.html` (HTTP 200) so the client-side router
//!   (`BrowserRouter`, HTML5 history) owns deep links like `/layouts/new`;
//! * a non-`GET` unmatched request is a genuine `404` (the SPA only serves
//!   reads; mutations live under `/api/v1`).
//!
//! If the UI was not built, `index.html` is absent and the fallback returns a
//! `503` explaining how to build it rather than a confusing blank `404`.
//!
//! Caching and encoding (slice 6, issue #292, replacing the nginx site on
//! mda001): hashed `/assets/*` are `public, max-age=31536000, immutable`,
//! everything else `no-cache`; every response carries a strong `ETag` (the
//! file's SHA-256) and `If-None-Match` answers `304`; text assets are gzipped
//! when the client accepts it (`Vary: Accept-Encoding`). A missing
//! `/assets/*` file is a `404`, never `index.html`.
//!
//! `[control] ui_dir` ([`UiDir`]) serves a UI build from disk instead of the
//! embedded copy; it is read once at startup into memory (bounded), so a
//! request never touches the filesystem.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The embedded production build of the web UI (`web/dist`).
///
/// `debug-embed` (see `Cargo.toml`) inlines the bytes in every profile, so this
/// is identical in debug and release and tests exercise the real assets. The
/// folder is resolved at compile time relative to this crate; the web build must
/// have produced `web/dist` first (the `embed-web` feature is off by default, so
/// the GPU-free default build never requires it).
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// The most bytes [`UiDir::load`] reads (the built UI is about 6 MiB).
pub const UI_DIR_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The most files [`UiDir::load`] reads.
pub const UI_DIR_MAX_FILES: usize = 10_000;

/// A UI build loaded from `[control] ui_dir` at startup.
#[derive(Debug, Default)]
pub struct UiDir {
    files: std::collections::HashMap<String, (bytes::Bytes, [u8; 32])>,
}

impl UiDir {
    /// Read every regular file under `root` (symlinks are not followed) into
    /// memory, keyed by its `/`-separated relative path, with its SHA-256.
    ///
    /// # Errors
    /// An unreadable directory, no `index.html`, or more than
    /// [`UI_DIR_MAX_FILES`] files / [`UI_DIR_MAX_BYTES`] bytes.
    pub fn load(root: &std::path::Path) -> std::io::Result<Self> {
        use sha2::Digest as _;
        let mut files = std::collections::HashMap::new();
        let mut total = 0u64;
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                let path = entry.path();
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Some(key) = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|rel| rel.to_str())
                    .map(|rel| rel.replace(std::path::MAIN_SEPARATOR, "/"))
                else {
                    continue;
                };
                let data = std::fs::read(&path)?;
                total = total.saturating_add(data.len() as u64);
                if files.len() >= UI_DIR_MAX_FILES || total > UI_DIR_MAX_BYTES {
                    return Err(std::io::Error::other(format!(
                        "control.ui_dir {} is larger than {UI_DIR_MAX_FILES} files / {UI_DIR_MAX_BYTES} bytes",
                        root.display()
                    )));
                }
                let sha: [u8; 32] = sha2::Sha256::digest(&data).into();
                files.insert(key, (bytes::Bytes::from(data), sha));
            }
        }
        if !files.contains_key(INDEX_HTML) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("control.ui_dir {} has no index.html", root.display()),
            ));
        }
        Ok(Self { files })
    }

    /// The number of files loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// The served UI: `[control] ui_dir` when the binary installed one with
/// [`install_ui_dir`], else the embedded copy.
static UI_DIR: std::sync::OnceLock<UiDir> = std::sync::OnceLock::new();

/// Serve the UI from `dir` instead of the embedded copy (once per process,
/// at startup; a second call is ignored and returns `false`).
pub fn install_ui_dir(dir: UiDir) -> bool {
    UI_DIR.set(dir).is_ok()
}

/// One servable file: bytes and SHA-256.
fn lookup(key: &str) -> Option<(bytes::Bytes, [u8; 32])> {
    if let Some(dir) = UI_DIR.get() {
        return dir.files.get(key).cloned();
    }
    let file = WebAssets::get(key)?;
    let sha = file.metadata.sha256_hash();
    Some((bytes::Bytes::from(file.data.into_owned()), sha))
}

/// The SPA index document, served for client-routed paths.
const INDEX_HTML: &str = "index.html";

/// Normalize a request path to an embedded-asset key: strip the leading `/`, and
/// map the site root to `index.html`.
fn asset_key(path: &str) -> &str {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        INDEX_HTML
    } else {
        trimmed
    }
}

/// The cache tier of an embedded asset: the content-hashed `/assets/*`
/// bundles Vite emits are immutable for a year; everything else (notably
/// `index.html`) is `no-cache`, so a redeploy is picked up on the next load
/// (revalidated cheaply through the `ETag`). Same split as the nginx site on
/// mda001 that this replaces (slice 6, #292).
fn cache_control(key: &str) -> &'static str {
    if key.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Text types worth compressing (nginx `gzip_types` plus `text/html`).
fn compressible(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/javascript"
                | "application/json"
                | "application/geo+json"
                | "application/manifest+json"
                | "image/svg+xml"
                | "application/wasm"
        )
}

/// Bodies below this are sent as they are (nginx `gzip_min_length` scale).
const GZIP_MIN_LENGTH: usize = 1024;

/// Gzipped copies of the embedded text assets, built on first request and
/// kept for the process lifetime. Bounded by construction: at most one entry
/// per file compiled into the binary, keyed by the embedded path (a request
/// for a path that is not embedded never reaches this table).
static GZIPPED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<bytes::Bytes>>>,
> = std::sync::OnceLock::new();

fn gzip_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<bytes::Bytes>>>
{
    GZIPPED.get_or_init(Default::default)
}

/// The gzipped body for `key`, compressing on first use (on the blocking
/// pool). `None` when compression does not pay (smaller than the original).
async fn gzipped(key: &str, data: bytes::Bytes) -> Option<bytes::Bytes> {
    if let Some(hit) = gzip_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
    {
        return hit.clone();
    }
    let original = data.len();
    let compressed = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(
            Vec::with_capacity(original / 3),
            flate2::Compression::new(6),
        );
        encoder.write_all(&data).ok()?;
        encoder.finish().ok()
    })
    .await
    .ok()
    .flatten()
    .filter(|gz| gz.len() < original)
    .map(bytes::Bytes::from);
    gzip_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_owned(), compressed.clone());
    compressed
}

/// Whether the client accepts gzip (`Accept-Encoding`, `q=0` refuses).
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|item| {
            let mut parts = item.split(';').map(str::trim);
            let coding = parts.next()?;
            let refused = parts.any(|p| {
                p.strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            Some((coding, refused))
        })
        .any(|(coding, refused)| (coding.eq_ignore_ascii_case("gzip") || coding == "*") && !refused)
}

/// The strong entity tag of an embedded file: its SHA-256 (computed at build
/// time by `rust-embed`), truncated to 128 bits. The gzipped representation
/// gets its own tag (`-gz`), as RFC 9110 section 8.8.3 requires.
fn entity_tag(sha256: &[u8; 32], gzip: bool) -> Option<HeaderValue> {
    let mut hex = String::with_capacity(40);
    for byte in sha256.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    let suffix = if gzip { "-gz" } else { "" };
    HeaderValue::from_str(&format!("\"{hex}{suffix}\"")).ok()
}

/// `If-None-Match` matches either representation's tag (a client that
/// cached the gzip copy and one that cached the identity copy both get 304).
fn if_none_match(headers: &HeaderMap, sha256: &[u8; 32]) -> bool {
    let Some(value) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let plain = entity_tag(sha256, false);
    let gz = entity_tag(sha256, true);
    value.split(',').map(str::trim).any(|candidate| {
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate == "*"
            || plain.as_ref().is_some_and(|t| t.as_bytes() == candidate.as_bytes())
            || gz.as_ref().is_some_and(|t| t.as_bytes() == candidate.as_bytes())
    })
}

/// Build a response serving an embedded file with its guessed content type,
/// the cache tier of [`cache_control`], a strong `ETag` (`304` on a match),
/// and a gzipped body for text assets when the client accepts it.
async fn serve_asset(key: &str, method: &Method, headers: &HeaderMap) -> Option<Response> {
    let (data, sha256) = lookup(key)?;
    let mime = mime_guess::from_path(key).first_or_octet_stream();
    let text = compressible(mime.as_ref());
    let mut response = if if_none_match(headers, &sha256) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let gzip = if text && data.len() >= GZIP_MIN_LENGTH && accepts_gzip(headers) {
            gzipped(key, data.clone()).await
        } else {
            None
        };
        let encoded = gzip.is_some();
        let body = gzip.unwrap_or(data);
        let len = body.len();
        let mut response = if *method == Method::HEAD {
            Response::new(Body::empty())
        } else {
            Response::new(Body::from(body))
        };
        let out = response.headers_mut();
        out.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
            out.insert(header::CONTENT_TYPE, value);
        }
        if encoded {
            out.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        if let Some(tag) = entity_tag(&sha256, encoded) {
            out.insert(header::ETAG, tag);
        }
        response
    };
    let out = response.headers_mut();
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(key)),
    );
    if text {
        out.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if response.status() == StatusCode::NOT_MODIFIED {
        if let Some(tag) = entity_tag(&sha256, false) {
            response.headers_mut().insert(header::ETAG, tag);
        }
    }
    Some(response)
}

/// The control router's fallback: serve the embedded SPA (asset-or-index for
/// `GET`/`HEAD`, `404` otherwise). See the module docs for the exact rules.
pub async fn spa_fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let key = asset_key(uri.path());

    // A direct asset hit (JS/CSS/index/favicon...).
    if let Some(response) = serve_asset(key, &method, &headers).await {
        return response;
    }

    // A missing hashed bundle is a real 404 (a stale tab asking for a
    // previous build's chunk must fail loudly, not get index.html as
    // JavaScript); everything else is a client-side route.
    if key.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    // No asset matched: hand deep links to the client-side router by serving the
    // index document (HTTP 200). If the UI was never built, say so clearly.
    match serve_asset(INDEX_HTML, &method, &headers).await {
        Some(response) => response,
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the web UI is not built into this binary; run the web build \
             (`npm --prefix web ci && npm --prefix web run build`) and rebuild \
             with `--features embed-web`",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn asset_key_maps_root_to_index() {
        assert_eq!(asset_key("/"), INDEX_HTML);
        assert_eq!(asset_key(""), INDEX_HTML);
        assert_eq!(asset_key("/assets/app.js"), "assets/app.js");
        assert_eq!(asset_key("/layouts/new"), "layouts/new");
    }

    #[test]
    fn ui_dir_loads_a_build_and_requires_index() {
        let dir = std::env::temp_dir().join(format!("spa-ui-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/app-abc.js"), b"console.log(1)").unwrap();
        assert!(UiDir::load(&dir).is_err(), "no index.html is refused");
        std::fs::write(dir.join("index.html"), b"<!doctype html>").unwrap();
        let ui = UiDir::load(&dir).unwrap();
        assert_eq!(ui.len(), 2);
        assert!(ui.files.contains_key("assets/app-abc.js"));
        assert!(ui.files.contains_key("index.html"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_negotiation() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
            m
        };
        assert!(accepts_gzip(&h("gzip, deflate, br")));
        assert!(accepts_gzip(&h("br;q=1.0, gzip;q=0.8")));
        assert!(accepts_gzip(&h("*")));
        assert!(!accepts_gzip(&h("gzip;q=0")));
        assert!(!accepts_gzip(&h("br")));
        assert!(!accepts_gzip(&HeaderMap::new()));
    }

    #[test]
    fn index_html_is_embedded() {
        // The web build ran (CI / local), so the index document is present and
        // looks like an HTML document.
        let index = WebAssets::get(INDEX_HTML).expect("index.html must be embedded");
        let text = String::from_utf8_lossy(&index.data);
        assert!(
            text.to_ascii_lowercase().contains("<!doctype html")
                || text.to_ascii_lowercase().contains("<html"),
            "index.html should be an HTML document"
        );
    }
}
