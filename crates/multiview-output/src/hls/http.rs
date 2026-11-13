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
/// The symlink confinement is enforced separately by [`confine_to_root_blocking`],
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

/// Bytes per body chunk read off disk (and the most one response holds in
/// memory at a time): 64 KiB, the size nginx's `output_buffers` default
/// reads per `sendfile`-less write (TLS cannot `sendfile` without kTLS).
pub const BODY_CHUNK: usize = 64 * 1024;

/// An opened, confined media file: the handle positioned at offset 0 and the
/// metadata of **that** handle (`fstat`, so the length and validators describe
/// exactly the bytes that are streamed, even if the segmenter replaces the
/// path meanwhile).
struct Opened {
    file: std::fs::File,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// Resolve, confine, open and `fstat` in ONE blocking-pool hop (slice 6,
/// #292): the previous path took five separate `tokio::fs` hops
/// (canonicalize x2, metadata, open, seek), each a round trip through the
/// blocking pool, which is what segment time-to-first-byte pays.
fn open_confined(root: &Path, lexical: &Path) -> std::io::Result<Option<Opened>> {
    let Some(target) = confine_to_root_blocking(root, lexical) else {
        return Ok(None);
    };
    let file = match std::fs::File::open(&target) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some(Opened {
        file,
        len: meta.len(),
        modified: meta.modified().ok(),
    }))
}

/// Confine a lexically-resolved `target` to the served `root` by canonicalising
/// both and requiring the real target to live under the real root, so a
/// symlink **inside** the tree pointing outside it is rejected (the lexical
/// pre-check in [`resolve_media_path`] cannot catch that). A *missing* file
/// canonicalises its parent and re-appends the final component, so it still
/// 404s as a missing file (never a `500`), while a parent that escapes the
/// root is rejected. `None` maps to `404`.
fn confine_to_root_blocking(canonical_root: &Path, target: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(target) {
        return canonical.starts_with(canonical_root).then_some(canonical);
    }
    let parent = target.parent()?;
    let file_name = target.file_name()?;
    let canonical_parent = std::fs::canonicalize(parent).ok()?;
    canonical_parent
        .starts_with(canonical_root)
        .then(|| canonical_parent.join(file_name))
}

/// The nginx-format strong validator: `"<mtime secs hex>-<size hex>"`
/// (`ngx_http_set_etag`), so a cache that held an nginx-served copy
/// revalidates against the engine without refetching.
fn etag(len: u64, modified: Option<std::time::SystemTime>) -> Option<HeaderValue> {
    let secs = modified?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    HeaderValue::from_str(&format!("\"{secs:x}-{len:x}\"")).ok()
}

/// Whether the request's validators say the client's copy is current
/// (RFC 9110 section 13.1.2 / 13.1.3; `If-None-Match` wins over
/// `If-Modified-Since`, which nginx compares exactly by default).
fn not_modified(
    headers: &HeaderMap,
    etag: Option<&HeaderValue>,
    modified: Option<std::time::SystemTime>,
) -> bool {
    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        let Some(etag) = etag.and_then(|e| e.to_str().ok()) else {
            return false;
        };
        let Ok(inm) = inm.to_str() else {
            return false;
        };
        // RFC 9110 section 13.2.1: a range request needs a strong validator.
        // With `Range`, only an exact strong match revalidates (as nginx); a
        // weak tag or `*` falls through to the range (206), never an empty
        // 304 for a partial fetch.
        let ranged = headers.contains_key(header::RANGE);
        return inm.split(',').map(str::trim).any(|candidate| {
            if ranged {
                candidate == etag
            } else {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
            }
        });
    }
    let (Some(ims), Some(modified)) = (
        headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()),
        modified,
    ) else {
        return false;
    };
    // HTTP dates have one-second resolution.
    let secs = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    };
    secs(modified) <= secs(ims)
}

/// Whether an `If-Range` precondition still matches (RFC 9110 section
/// 13.1.5): absent, or equal to the current strong `ETag` / `Last-Modified`.
/// A mismatch serves the full `200` instead of the range.
fn if_range_matches(
    headers: &HeaderMap,
    etag: Option<&HeaderValue>,
    last_modified: Option<&HeaderValue>,
) -> bool {
    match headers.get(header::IF_RANGE) {
        None => true,
        Some(value) => {
            Some(value) == etag
                || (!value.as_bytes().starts_with(b"\"") && Some(value) == last_modified)
        }
    }
}

/// Serve one playlist/segment/init file (`GET`/`HEAD`) with the §6 contract
/// and nginx's static-file behaviour (`deploy/hls-nginx.conf`, the golden
/// table in `tests/hls_nginx_parity.rs`): `ETag` + `Last-Modified`
/// validators, `304 Not Modified`, `If-Range`, single-range `206`/`416`.
///
/// Reads only what the response needs: `HEAD` and `304` never read a byte, a
/// `Range` `206` streams exactly the requested span, a full `GET` streams the
/// file in [`BODY_CHUNK`] pieces. All file I/O is on the blocking pool, off
/// the engine hot path.
async fn serve_media(
    State(root): State<Arc<PathBuf>>,
    UrlPath(relative): UrlPath<String>,
    method: Method,
    request_headers: HeaderMap,
) -> Response {
    serve_in_root(root, &relative, method, request_headers).await
}

/// Serve one file of the HLS directory `dir` with exactly the [`hls_router`]
/// contract (extension allow-list, content types, cache tiers, validators,
/// ranges, symlink confinement), without CORS. For a caller that picks the
/// directory per request (the token-scoped cast media route). `dir` is
/// canonicalised on the blocking pool; a directory that does not exist yet
/// is a plain `404`.
pub async fn serve_file(
    dir: PathBuf,
    relative: &str,
    method: Method,
    request_headers: HeaderMap,
) -> Response {
    let root = match tokio::task::spawn_blocking(move || std::fs::canonicalize(&dir)).await {
        Ok(Ok(root)) => root,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    serve_in_root(Arc::new(root), relative, method, request_headers).await
}

async fn serve_in_root(
    root: Arc<PathBuf>,
    relative: &str,
    method: Method,
    request_headers: HeaderMap,
) -> Response {
    let Some(lexical) = resolve_media_path(&root, relative) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((content_type, tier)) = lexical
        .extension()
        .and_then(|e| e.to_str())
        .and_then(classify)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Symlink confinement + open + fstat, in one blocking hop. A missing file
    // (or one that raced away, e.g. the segmenter's deferred-unlink prune)
    // is an honest 404, never a 500.
    let root_for_open = Arc::clone(&root);
    let opened =
        match tokio::task::spawn_blocking(move || open_confined(&root_for_open, &lexical)).await {
            Ok(Ok(Some(opened))) => opened,
            Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "HLS delivery open failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    let len = opened.len;
    let etag = etag(len, opened.modified);
    let last_modified = opened
        .modified
        .and_then(|t| HeaderValue::from_str(&httpdate::fmt_http_date(t)).ok());

    let validators = |headers: &mut HeaderMap| {
        if let Some(value) = &etag {
            headers.insert(header::ETAG, value.clone());
        }
        if let Some(value) = &last_modified {
            headers.insert(header::LAST_MODIFIED, value.clone());
        }
    };

    if not_modified(&request_headers, etag.as_ref(), opened.modified) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, tier.cache_control());
        validators(headers);
        return response;
    }

    let range = if if_range_matches(&request_headers, etag.as_ref(), last_modified.as_ref()) {
        request_headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map_or(ByteRange::Ignored, |spec| byte_range(spec, len))
    } else {
        ByteRange::Ignored
    };

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
        // Metadata only: never read a segment to answer HEAD.
        Body::empty()
    } else {
        let mut handle = tokio::fs::File::from_std(opened.file);
        if body_start > 0 {
            use tokio::io::AsyncSeekExt;
            if let Err(e) = handle.seek(SeekFrom::Start(body_start)).await {
                tracing::warn!(error = %e, "HLS delivery seek failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
        span_body(handle, body_len)
    };

    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, tier.cache_control());
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    validators(headers);
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

/// Stream exactly `count` bytes from the positioned `handle` into a response
/// [`Body`], holding only a bounded chunk in memory at a time.
///
/// `.take(count)` bounds the stream to the span length, so the body matches the
/// explicit `Content-Length` even though the underlying file may be longer (the
/// `206` Range path) — framing is unchanged from the previous buffered path.
fn span_body(handle: tokio::fs::File, count: u64) -> Body {
    use tokio::io::AsyncReadExt;
    Body::from_stream(ReaderStream::with_capacity(handle.take(count), BODY_CHUNK))
}
