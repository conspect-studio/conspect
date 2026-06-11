//! The HLS delivery surface over HTTP: ADR-0032's static-frontable tier
//! (master playlist + media playlists + closed segments + `init.mp4`) served
//! by Multiview itself with the §6 header contract.
//!
//! [`hls_router`] builds an axum [`Router`] over one HLS output directory (the
//! directory the segmenter writes playlists/segments into) and serves it with:
//!
//! - **explicit Content-Type per extension** (`.m3u8` / `.ts` / `.m4s` /
//!   `.mp4` / `.vtt`; anything else is **never served** — the segment
//!   directory may hold operator files that are not delivery surface);
//! - **Cache-Control tiers**: playlists `max-age=1, must-revalidate` (a live
//!   playlist mutates every segment), segments/init
//!   `public, max-age=31536000, immutable` (closed media never changes);
//! - **`Accept-Ranges: bytes`** with single-range `206 Partial Content` /
//!   `416 Range Not Satisfiable` handling ([`byte_range`]);
//! - **Origin-reflecting CORS** ([`with_hls_cors`]): a Cast receiver is a
//!   browser app on a Google origin fetching playlists/segments cross-origin,
//!   and every browser player benefits identically.
//!
//! Isolation (invariant #10): handlers only read files the segmenter already
//! published to disk — they never touch an engine channel, hold an engine
//! lock, or make the output clock await anything. A slow or hostile client
//! stalls only its own connection. The LL-HLS blocking-reload origin (held
//! GETs for `_HLS_msn`/`_HLS_part`) is the separate dynamic tier of ADR-0032
//! and will mount **behind this same CORS layer** when it lands.

use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::header::{self, HeaderValue};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio_util::io::ReaderStream;

/// `Access-Control-Allow-Methods` for the HLS delivery surface (read-only).
const ALLOW_METHODS: HeaderValue = HeaderValue::from_static("GET, HEAD, OPTIONS");

/// `Access-Control-Allow-Headers`: the request headers an HLS player sends
/// cross-origin (`Range` is the load-bearing one for segment fetches).
const ALLOW_HEADERS: HeaderValue = HeaderValue::from_static("Content-Type, Range, Accept-Encoding");

/// `Access-Control-Expose-Headers`: lets cross-origin players read segment
/// sizes and `Content-Range` on `206` responses.
const EXPOSE_HEADERS: HeaderValue = HeaderValue::from_static("Content-Length, Content-Range");

/// `Access-Control-Max-Age`: cache preflight results for a day (browsers clamp
/// to their own ceiling, e.g. 2 h in Chromium — harmlessly).
const MAX_AGE: HeaderValue = HeaderValue::from_static("86400");

/// The parsed outcome of an HTTP `Range` header against a resource of a known
/// length, per RFC 9110 §14 single-range semantics.
///
/// Pure and reusable: the LL-HLS byte-range-part origin (ADR-0032 §3 — parts
/// are byte ranges into one growing `.m4s`) parses part spans with the same
/// rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// Not a parseable single `bytes=` range (malformed, multi-range, or
    /// first-pos > last-pos). RFC 9110 lets a server ignore such a header and
    /// serve the full body with `200`.
    Ignored,
    /// A satisfiable inclusive byte span (`start <= end < len`): serve `206`.
    Span {
        /// First byte offset (inclusive).
        start: u64,
        /// Last byte offset (inclusive), clamped to the final byte.
        end: u64,
    },
    /// Syntactically valid but unsatisfiable (start at/after EOF, zero-length
    /// suffix, or an empty resource): serve `416` with `bytes */len`.
    Unsatisfiable,
}

/// Parse a `Range` request header against a resource of `len` bytes.
///
/// Supports the three single-range forms — `bytes=a-b`, `bytes=a-` and the
/// suffix `bytes=-n` — and classifies everything else as [`ByteRange::Ignored`]
/// (multi-range requests are deliberately not split: HLS players fetch one
/// span per request, and a `200` full response is always a valid answer).
#[must_use]
pub fn byte_range(header: &str, len: u64) -> ByteRange {
    let Some(spec) = header.strip_prefix("bytes=") else {
        return ByteRange::Ignored;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        // Multi-range: legitimately ignorable; the full body is served.
        return ByteRange::Ignored;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Ignored;
    };
    match (first, last) {
        // Suffix form `-n`: the final n bytes.
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) | Err(_) if suffix.is_empty() => ByteRange::Ignored,
            Ok(0) => ByteRange::Unsatisfiable,
            Ok(n) => {
                if len == 0 {
                    ByteRange::Unsatisfiable
                } else {
                    ByteRange::Span {
                        start: len.saturating_sub(n),
                        end: len.saturating_sub(1),
                    }
                }
            }
            Err(_) => ByteRange::Ignored,
        },
        // Open-ended form `a-`: from a to EOF.
        (start, "") => match start.parse::<u64>() {
            Ok(start) if start < len => ByteRange::Span {
                start,
                end: len.saturating_sub(1),
            },
            Ok(_) => ByteRange::Unsatisfiable,
            Err(_) => ByteRange::Ignored,
        },
        // Closed form `a-b`.
        (start, end) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(start), Ok(end)) if start > end => ByteRange::Ignored,
            (Ok(start), Ok(end)) => {
                if start < len {
                    ByteRange::Span {
                        start,
                        end: end.min(len.saturating_sub(1)),
                    }
                } else {
                    ByteRange::Unsatisfiable
                }
            }
            _ => ByteRange::Ignored,
        },
    }
}

/// Build the delivery router for one HLS output directory.
///
/// Routes `GET`/`HEAD` for every path under the mount against `root`,
/// restricted to the HLS media extensions, with the full ADR-0032 §6 header
/// contract (see the module docs). `OPTIONS` (including CORS preflight) is
/// answered `204` on **every** path by the [`with_hls_cors`] layer this router
/// is wrapped in.
///
/// CORS is **on by default with no configuration**: these are public media
/// endpoints, the reflected-Origin contract is safe for them, and a Cast
/// receiver cannot work without it. (The ADR-0032 §7 `cors_allowed_origins`
/// config knob, when that serving-config slice lands, narrows the reflected
/// set inside this same layer.)
pub fn hls_router(root: impl Into<PathBuf>) -> Router {
    let root = root.into();
    // Canonicalise the served root **once** at build time (a one-shot startup
    // cost, off any hot path) so per-request symlink confinement compares
    // against the real root. If the directory does not exist yet (the segmenter
    // creates it lazily), fall back to the lexical root — every served file
    // still 404s until the directory exists, and confinement re-canonicalises
    // per request once it does.
    let canonical_root = std::fs::canonicalize(&root).unwrap_or(root);
    with_hls_cors(
        Router::new()
            .route("/{*path}", get(serve_media))
            .with_state(Arc::new(canonical_root)),
    )
}

/// Wrap `router` in the **single** HLS CORS implementation (DEV-D1).
///
/// The contract, applied uniformly to every route in the wrapped router:
///
/// - requests **with** an `Origin` header get `Access-Control-Allow-Origin`
///   **reflecting that Origin** (never a hardcoded `*`, which is ambiguous for
///   protected content) plus `Access-Control-Expose-Headers:
///   Content-Length, Content-Range`;
/// - `OPTIONS` is answered `204 No Content` with the allow set
///   (`GET, HEAD, OPTIONS`; `Content-Type, Range, Accept-Encoding`;
///   `Access-Control-Max-Age`) before routing, so preflight succeeds on every
///   HLS path;
/// - requests **without** an `Origin` (normal, non-browser players) get **no**
///   CORS headers;
/// - **every** response carries `Vary: Origin`, so a shared cache never serves
///   a header-less variant to a cross-origin caller.
///
/// # Why not `tower-http`'s `CorsLayer`
/// This is a small, purpose-built middleware rather than `tower_http::cors::
/// CorsLayer`, and the deviation from ADR-0032 §6's "wire `tower-http`
/// `CorsLayer`" wording is **deliberate and contract-identical**: (1) `CorsLayer`
/// answers preflight `OPTIONS` with `200 OK`, but this surface returns `204 No
/// Content` (the §6 contract this router and its tests pin); (2) `tower-http` is
/// not a dependency of this crate, and pulling it in for one reflected-Origin
/// rule is not worth the build/licence surface. The contract above (reflect the
/// Origin, `Vary: Origin`, the allow/expose sets, `204` preflight) is exactly
/// what the ADR specifies — only the implementation differs.
pub fn with_hls_cors(router: Router) -> Router {
    router.layer(middleware::from_fn(hls_cors))
}

/// The CORS middleware behind [`with_hls_cors`]. One implementation; never
/// per-handler copy-paste.
async fn hls_cors(request: axum::extract::Request, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let mut response = if request.method() == Method::OPTIONS {
        // Preflight (and plain OPTIONS): answered here, before routing, so it
        // succeeds on every HLS path with no per-route handler.
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(header::ALLOW, ALLOW_METHODS);
        if origin.is_some() {
            headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, ALLOW_METHODS);
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS);
            headers.insert(header::ACCESS_CONTROL_MAX_AGE, MAX_AGE);
        }
        response
    } else {
        let mut response = next.run(request).await;
        if origin.is_some() {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSE_HEADERS);
        }
        response
    };
    let headers = response.headers_mut();
    // Always vary by Origin (cache correctness even for no-Origin responses);
    // reflect the Origin only when one was sent.
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

/// The ADR-0032 §6 cache tier a served file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheTier {
    /// Playlists mutate every segment: tiny TTL, always revalidated.
    Playlist,
    /// Closed segments / init never change: immutable for a year.
    Media,
}

impl CacheTier {
    /// The `Cache-Control` value for this tier.
    const fn cache_control(self) -> HeaderValue {
        match self {
            CacheTier::Playlist => HeaderValue::from_static("max-age=1, must-revalidate"),
            CacheTier::Media => HeaderValue::from_static("public, max-age=31536000, immutable"),
        }
    }
}

/// Classify a served extension into its explicit Content-Type + cache tier.
/// Anything not in this table is **not HLS delivery surface** and 404s.
fn classify(extension: &str) -> Option<(HeaderValue, CacheTier)> {
    match extension {
        "m3u8" => Some((
            HeaderValue::from_static("application/vnd.apple.mpegurl"),
            CacheTier::Playlist,
        )),
        "ts" => Some((HeaderValue::from_static("video/mp2t"), CacheTier::Media)),
        // The CMAF segment MIME type (absent from stock nginx mime.types too —
        // the reference fronting config adds it the same way).
        "m4s" => Some((
            HeaderValue::from_static("video/iso.segment"),
            CacheTier::Media,
        )),
        "mp4" => Some((HeaderValue::from_static("video/mp4"), CacheTier::Media)),
        // WebVTT subtitle segments referenced by a subtitle media playlist.
        "vtt" => Some((HeaderValue::from_static("text/vtt"), CacheTier::Media)),
        _ => None,
    }
}

/// Lexically resolve the request path against the output root, rejecting
/// anything that is not a plain relative file path (no `..`, no leading `/`,
/// no `.`).
///
/// This is a **cheap pre-check only**: it stops the obvious `..`/absolute
/// traversals before any filesystem work, but it cannot stop a symlink
/// **inside** the tree pointing out of it (its components are all `Normal`).
/// The symlink confinement is enforced separately by [`confine_to_root`],
/// which canonicalises the resolved target against the canonical root.
fn resolve_media_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let mut any = false;
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
        any = true;
    }
    any.then(|| root.join(relative))
}

/// Confine a lexically-resolved `target` to the served `root` by canonicalising
/// both and requiring the real target to live under the real root — so a
/// symlink **inside** the tree pointing outside it is rejected (the lexical
/// pre-check in [`resolve_media_path`] cannot catch that; its components are all
/// `Normal`).
///
/// Canonicalisation requires the path to exist, so a *missing* file (the
/// common 404) is handled by canonicalising the target's **parent** and
/// re-appending the final component: a request for a file that does not exist
/// yet still resolves its real directory and 404s as a missing file (never a
/// `500`), while a request whose parent escapes the root is rejected.
///
/// Returns the canonical, confined absolute path on success, or `None` when the
/// target (or its parent) cannot be confined to the canonical root — the caller
/// maps `None` to `404`.
async fn confine_to_root(canonical_root: &Path, target: &Path) -> Option<PathBuf> {
    // Fast path: the target itself exists (a real file or a symlink to one
    // inside the tree) — canonicalise it directly.
    if let Ok(canonical) = tokio::fs::canonicalize(target).await {
        return canonical.starts_with(canonical_root).then_some(canonical);
    }
    // The target does not exist (the common 404): canonicalise its parent and
    // re-append the final component, so a missing file still resolves its real
    // directory and confines correctly instead of erroring.
    let parent = target.parent()?;
    let file_name = target.file_name()?;
    let canonical_parent = tokio::fs::canonicalize(parent).await.ok()?;
    canonical_parent
        .starts_with(canonical_root)
        .then(|| canonical_parent.join(file_name))
}

/// Serve one playlist/segment/init file (`GET`/`HEAD`) with the §6 contract.
///
/// Reads only what the response needs: `HEAD` is metadata-only, a `Range`
/// `206` reads exactly the requested span (seek + exact read), a full `GET`
/// reads the file once. All I/O is `tokio::fs` off the engine hot path.
async fn serve_media(
    State(root): State<Arc<PathBuf>>,
    UrlPath(relative): UrlPath<String>,
    method: Method,
    request_headers: HeaderMap,
) -> Response {
    let Some(lexical) = resolve_media_path(&root, &relative) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((content_type, tier)) = lexical
        .extension()
        .and_then(|e| e.to_str())
        .and_then(classify)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Symlink confinement: canonicalise the lexically-resolved target against
    // the canonical root, so a symlink **inside** the tree pointing outside it
    // is rejected (the lexical check above cannot catch that). A missing file
    // still 404s (handled inside `confine_to_root`), never 500s.
    let Some(file) = confine_to_root(&root, &lexical).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = match tokio::fs::metadata(&file).await {
        Ok(meta) if meta.is_file() => meta.len(),
        // Missing, a directory, or unreadable: not a servable media file.
        Ok(_) | Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let range = request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(ByteRange::Ignored, |spec| byte_range(spec, len));

    let (status, span) = match range {
        ByteRange::Unsatisfiable => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            let headers = response.headers_mut();
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            return response;
        }
        ByteRange::Span { start, end } => (StatusCode::PARTIAL_CONTENT, Some((start, end))),
        ByteRange::Ignored => (StatusCode::OK, None),
    };

    let (body_start, body_len) = match span {
        Some((start, end)) => (start, end.saturating_sub(start).saturating_add(1)),
        None => (0, len),
    };

    let body = if method == Method::HEAD {
        // Metadata only — never open or read a segment to answer HEAD.
        Body::empty()
    } else {
        // Open + seek to the span start, then **stream** the (bounded) span
        // straight off disk: never buffer a whole multi-MB segment into RAM.
        // The open/seek happens before the response is built, so a raced-away
        // file (404) or an I/O fault (500) still maps to the right status; once
        // streaming begins the `Content-Length` framing is already fixed.
        match open_span(&file, body_start).await {
            Ok(handle) => span_body(handle, body_len),
            // The file raced away (e.g. the segmenter's deferred-unlink prune)
            // between metadata and open: honest 404, never a panic.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return StatusCode::NOT_FOUND.into_response();
            }
            Err(e) => {
                tracing::warn!(file = %file.display(), error = %e, "HLS delivery open failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, tier.cache_control());
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // Explicit length: load-bearing for HEAD (empty body would otherwise
    // advertise 0) and harmless-correct for GET.
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    if let Some((start, end)) = span {
        if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    response
}

/// Open `file` and seek to byte `start`, returning the positioned handle.
///
/// The handle is then streamed by [`span_body`] — the whole segment is never
/// materialised in RAM (an unauthenticated GET of a multi-MB segment must not
/// amplify into a multi-MB allocation, and streaming improves LL-HLS TTFB).
async fn open_span(file: &Path, start: u64) -> std::io::Result<tokio::fs::File> {
    use tokio::io::AsyncSeekExt;
    let mut handle = tokio::fs::File::open(file).await?;
    if start > 0 {
        handle.seek(SeekFrom::Start(start)).await?;
    }
    Ok(handle)
}

/// Stream exactly `count` bytes from the positioned `handle` into a response
/// [`Body`], holding only a bounded chunk in memory at a time.
///
/// `.take(count)` bounds the stream to the span length, so the body matches the
/// explicit `Content-Length` even though the underlying file may be longer (the
/// `206` Range path) — framing is unchanged from the previous buffered path.
fn span_body(handle: tokio::fs::File, count: u64) -> Body {
    use tokio::io::AsyncReadExt;
    Body::from_stream(ReaderStream::new(handle.take(count)))
}
