//! In-process raw-bytes fetch of a small **text** resource over libav's I/O
//! (`ffmpeg` feature) — the caption master/rendition playlist fetcher.
//!
//! Native HLS WebVTT caption ingest must read the *master* playlist text to find
//! its `TYPE=SUBTITLES` rendition (libav opens an HLS URL as a single program and
//! does not surface the master). Rather than shell out to the `curl` binary (a
//! foreign runtime dependency, and a TLS/redirect stack divergent from the libav
//! segment fetch), this reads the URL through libav's own `AVIOContext`.
//!
//! `ffmpeg_next` exposes no safe wrapper for `avio_open2`/`avio_read`, so the one
//! `unsafe fn` here drives the raw FFI: it opens the context (read-only, with a
//! protocol allowlist + read timeout), reads to EOF into a **bounded** buffer,
//! and closes the context on **every** return path. The crate is `unsafe = deny`;
//! every block carries a `// SAFETY:` note. Security: the URL scheme is validated
//! in Rust against `allowed_protocols` *before* opening (a hard guarantee), and
//! the same allowlist is handed to libav as defence-in-depth — so a stray
//! `file:`/`concat:`/`subfile:` URL can never be opened.

// reason: avio_open2/avio_read/avio_closep/av_dict_set have no safe ffmpeg_next
// wrapper; every `unsafe` operation below is bounded to libav buffers we own and
// carries a `// SAFETY:` note (the crate is otherwise `unsafe = deny`).
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::ptr;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};

/// A fetched text resource: its body plus the **effective** URL it was actually
/// read from — i.e. the final URL **after any HTTP(S) redirects** were followed.
///
/// Relative child URIs in an HLS playlist resolve against the playlist's effective
/// URI, not the URL originally requested (RFC 3986 §5 / RFC 8216): a redirecting or
/// CDN-fronted master only resolves its relative variant/rendition children
/// correctly when they are joined onto this post-redirect base. When the fetch did
/// not redirect — or the protocol exposes no final location (e.g. `file:`) — [`url`]
/// equals the requested URL.
///
/// [`url`]: FetchedText::url
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchedText {
    /// The effective (final, post-redirect) URL the body was read from. For HTTP(S)
    /// this is libav's `location` option after redirects; otherwise the requested
    /// URL verbatim.
    pub url: String,
    /// The fetched body text.
    pub body: String,
}

/// Read-loop chunk size (bytes).
const READ_CHUNK: usize = 16 * 1024;

/// libav read/write timeout handed to the context (microseconds): 30 s, matching
/// the previous `curl --max-time 30`, so a hung connection cannot block forever.
const RW_TIMEOUT_US: &str = "30000000";

/// Fetch the **text** at `url` over libav I/O, capping the body at `max_bytes`,
/// and surface the **effective** post-redirect URL alongside the body (see
/// [`FetchedText`]).
///
/// `allowed_protocols` is a comma list of permitted URL schemes (e.g.
/// `"http,https,tls,tcp"`): `url`'s scheme must be one of them (checked in Rust
/// before opening), and the same list is passed to libav as `protocol_whitelist`.
/// Intended for small text resources — an HLS master/rendition playlist is a few
/// KB; a body exceeding `max_bytes` is rejected so a misbehaving server cannot
/// grow memory without bound.
///
/// libav's HTTP(S) protocol follows redirects transparently; after the first read
/// drains the (possibly redirected) response, the http context's `location` option
/// holds the final URL. This reads it back via `av_opt_get(.., AV_OPT_SEARCH_CHILDREN)`
/// so HLS child URIs can resolve against the correct base. If the protocol exposes
/// no `location` (e.g. `file:`) or it is empty, the requested `url` is reported as
/// the effective URL (a non-redirecting fetch).
///
/// # Errors
///
/// [`FfmpegError::Fetch`] if `url`'s scheme is not in `allowed_protocols`, the
/// open fails (bad URL, network error), the body exceeds `max_bytes`, or the
/// body is not valid UTF-8.
pub fn fetch_url_text(url: &str, max_bytes: usize, allowed_protocols: &str) -> Result<FetchedText> {
    // Hard, Rust-side scheme guarantee (independent of libav's own enforcement).
    let scheme = url.split(':').next().unwrap_or("");
    if scheme.is_empty() || !allowed_protocols.split(',').any(|p| p == scheme) {
        return Err(fetch_err(
            url,
            format_args!("scheme {scheme:?} is not in the allowed protocols {allowed_protocols:?}"),
        ));
    }

    ensure_initialized()?;

    let curl =
        CString::new(url).map_err(|_| fetch_err(url, format_args!("URL has an interior NUL")))?;
    let wl_key = c_string("protocol_whitelist", url)?;
    let wl_val = CString::new(allowed_protocols)
        .map_err(|_| fetch_err(url, format_args!("protocol list has an interior NUL")))?;
    let to_key = c_string("rw_timeout", url)?;
    let to_val = c_string(RW_TIMEOUT_US, url)?;

    // SAFETY: all pointers handed to libav below are either a fresh null
    // out-param libav fills (`opts`, `ctx`) or a `CString` we own for the whole
    // call; the options dict and the AVIOContext are freed on every return path
    // inside `fetch_inner`.
    unsafe { fetch_inner(url, &curl, &wl_key, &wl_val, &to_key, &to_val, max_bytes) }
}

/// `AVOption` name for the HTTP(S) protocol's current/final URL after redirects.
const LOCATION_OPT: &[u8] = b"location\0";

/// Build a `CString`, mapping an interior NUL to a [`FfmpegError::Fetch`].
fn c_string(s: &str, url: &str) -> Result<CString> {
    CString::new(s).map_err(|_| fetch_err(url, format_args!("{s:?} has an interior NUL")))
}

/// Open `url` read-only with the allowlist + timeout options, read to EOF into a
/// bounded buffer, and close the context on every path.
#[allow(clippy::too_many_arguments)]
unsafe fn fetch_inner(
    url: &str,
    curl: &CString,
    wl_key: &CString,
    wl_val: &CString,
    to_key: &CString,
    to_val: &CString,
    max_bytes: usize,
) -> Result<FetchedText> {
    let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
    // SAFETY: `opts` starts null; av_dict_set allocates and links entries into it.
    // We free it (whether or not avio_open2 consumes it) before returning.
    let r = ffi::av_dict_set(&raw mut opts, wl_key.as_ptr(), wl_val.as_ptr(), 0);
    if r >= 0 {
        // SAFETY: same live dict; second key.
        ffi::av_dict_set(&raw mut opts, to_key.as_ptr(), to_val.as_ptr(), 0);
    }
    if r < 0 {
        // SAFETY: frees and nulls whatever av_dict_set allocated.
        ffi::av_dict_free(&raw mut opts);
        return Err(fetch_err(url, ffmpeg::Error::from(r)));
    }

    let mut ctx: *mut ffi::AVIOContext = ptr::null_mut();
    // SAFETY: `ctx` out-param starts null; `curl` is a valid CString; `opts` is
    // our live dict. libav fills `ctx` on success (checked below).
    let open = ffi::avio_open2(
        &raw mut ctx,
        curl.as_ptr(),
        ffi::AVIO_FLAG_READ,
        ptr::null(),
        &raw mut opts,
    );
    // SAFETY: av_dict_free is null-safe and frees any entries avio_open2 left.
    ffi::av_dict_free(&raw mut opts);
    if open < 0 || ctx.is_null() {
        return Err(fetch_err(url, ffmpeg::Error::from(open)));
    }

    let body = read_to_end(url, ctx, max_bytes);

    // Read the effective (final, post-redirect) URL from the http context's
    // `location` option *before* closing the context. This is best-effort: a
    // protocol with no `location` (e.g. `file:`) leaves the requested URL as the
    // effective base. Only meaningful once the body has been read (the redirect is
    // followed lazily on the first read), so this runs after `read_to_end`.
    // SAFETY: `ctx` is still the live context avio_open2 returned (not yet closed).
    let effective = effective_url(url, ctx);

    // SAFETY: `ctx` is the live context avio_open2 returned; closed + nulled once.
    ffi::avio_closep(&raw mut ctx);
    body.map(|body| FetchedText {
        url: effective,
        body,
    })
}

/// Read the effective (final, post-redirect) URL of the open context `ctx` from
/// the http protocol's `location` option, falling back to the requested `url` when
/// the protocol exposes no such option (e.g. `file:`) or it is empty/non-UTF-8.
///
/// `av_opt_get` searches the context's children (`AV_OPT_SEARCH_CHILDREN`), where
/// the http `URLContext` lives, and allocates the value the caller must `av_free`.
unsafe fn effective_url(url: &str, ctx: *mut ffi::AVIOContext) -> String {
    let mut out: *mut u8 = ptr::null_mut();
    // SAFETY: `ctx` is the live AVIOContext (an AVClass-bearing object); `LOCATION_OPT`
    // is a NUL-terminated static; libav allocates `*out` on success (we free it
    // below). A negative return (AVERROR_OPTION_NOT_FOUND for `file:` etc.) leaves
    // `out` null.
    let rc = ffi::av_opt_get(
        ctx.cast::<core::ffi::c_void>(),
        LOCATION_OPT.as_ptr().cast::<core::ffi::c_char>(),
        ffi::AV_OPT_SEARCH_CHILDREN,
        &raw mut out,
    );
    if rc < 0 || out.is_null() {
        return url.to_owned();
    }
    // SAFETY: `out` is a libav-allocated NUL-terminated string; we copy it into an
    // owned `String` and then free the libav allocation, regardless of UTF-8 result.
    let effective = CStr::from_ptr(out.cast::<core::ffi::c_char>())
        .to_str()
        .ok()
        .filter(|s| !s.is_empty())
        .map_or_else(|| url.to_owned(), str::to_owned);
    // SAFETY: `out` was allocated by `av_opt_get`; free it through libav's allocator.
    ffi::av_free(out.cast::<core::ffi::c_void>());
    effective
}

/// Drain `ctx` into a `String`, bounded by `max_bytes`.
unsafe fn read_to_end(url: &str, ctx: *mut ffi::AVIOContext, max_bytes: usize) -> Result<String> {
    let chunk_len = i32::try_from(READ_CHUNK).unwrap_or(1).max(1);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK];
    loop {
        // SAFETY: `ctx` is live; we read at most `chunk_len` bytes into our own
        // stack buffer of `READ_CHUNK`.
        let n = ffi::avio_read(ctx, chunk.as_mut_ptr(), chunk_len);
        match n.cmp(&0) {
            core::cmp::Ordering::Greater => {
                let got = usize::try_from(n).unwrap_or(0);
                if buf.len().saturating_add(got) > max_bytes {
                    return Err(fetch_err(
                        url,
                        format_args!("body exceeds the {max_bytes}-byte budget"),
                    ));
                }
                buf.extend_from_slice(chunk.get(..got).unwrap_or(&[]));
            }
            // Zero bytes (some protocols' clean end) or AVERROR_EOF: done.
            core::cmp::Ordering::Equal => break,
            core::cmp::Ordering::Less => match ffmpeg::Error::from(n) {
                ffmpeg::Error::Eof => break,
                other => return Err(fetch_err(url, other)),
            },
        }
    }
    String::from_utf8(buf).map_err(|_| fetch_err(url, format_args!("body is not valid UTF-8")))
}

/// Build a [`FfmpegError::Fetch`] for `url` with a `Display` reason.
fn fetch_err(url: &str, reason: impl core::fmt::Display) -> FfmpegError {
    FfmpegError::Fetch {
        url: url.to_owned(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::fetch_url_text;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn reads_a_local_file_via_the_file_protocol() {
        let path = temp("multiview_avio_fetch_ok.m3u8");
        std::fs::write(&path, b"#EXTM3U\nhello-playlist\n").expect("write fixture");
        let url = format!("file:{}", path.display());
        let fetched = fetch_url_text(&url, 64 * 1024, "file").expect("fetch local file");
        assert!(fetched.body.contains("hello-playlist"), "got: {fetched:?}");
        // The `file:` protocol exposes no `location` option (and never redirects),
        // so the effective URL falls back to the requested URL verbatim.
        assert_eq!(
            fetched.url, url,
            "a non-redirecting fetch reports the requested URL as effective"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_a_body_over_the_byte_budget() {
        let path = temp("multiview_avio_fetch_big.bin");
        std::fs::write(&path, vec![b'x'; 4096]).expect("write fixture");
        let url = format!("file:{}", path.display());
        let result = fetch_url_text(&url, 1024, "file");
        assert!(result.is_err(), "a 4 KiB body must exceed a 1 KiB budget");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_scheme_outside_the_allowlist_is_refused_before_opening() {
        // `file:` is not in an http-only allowlist -> refused in Rust, never opened
        // (libav *can* open `file:`, so an Ok would mean the guard was bypassed).
        let result = fetch_url_text("file:/etc/hostname", 1024, "http,https");
        match result {
            Err(crate::error::FfmpegError::Fetch { reason, .. }) => assert!(
                reason.contains("not in the allowed protocols"),
                "reason: {reason}"
            ),
            other => panic!("expected a Fetch error for a blocked scheme, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_non_utf8_body() {
        let path = temp("multiview_avio_fetch_binary.bin");
        std::fs::write(&path, [0xFF_u8, 0xFE, 0x00, 0x01]).expect("write fixture");
        let url = format!("file:{}", path.display());
        let result = fetch_url_text(&url, 1024, "file");
        assert!(result.is_err(), "a non-UTF-8 body must be rejected");
        let _ = std::fs::remove_file(&path);
    }
}
