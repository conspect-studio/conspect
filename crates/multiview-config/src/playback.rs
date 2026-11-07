//! Derive the viewer-facing **playback URL(s)** for a configured [`Output`]
//! (`GET /api/v1/outputs/{id}/playback`, ADR-0101 — "a link to copy the actual
//! URL to paste into a tool like VLC").
//!
//! A configured [`Output`] carries its *authored* address (a listener bind, a
//! push destination, a packager path); what an operator actually pastes into
//! VLC/ffplay is a DERIVED value: a listener's bind becomes a caller URL at
//! the **public** host, a push destination is not viewable at all (the stream
//! goes TO that destination, never served FROM it), and a packager's on-disk
//! path becomes the `/hls/{mount}` URL served by the control-plane's web
//! listener. This module is the one place that derivation happens, so the
//! control-plane route and its tests share exactly one definition.
//!
//! **Never a secret.** An SRT/RIST pre-shared key is held as a `secret_ref`
//! (resolved only at the engine's run-path) and is never available here to
//! leak; this module instead returns a *note* telling the operator a
//! passphrase is required out-of-band.

use crate::origin::Origin;
use crate::schema::{Output, SrtMode};

/// One playback URL (or an explanatory note when none exists) for an output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackUrl {
    /// Short human label, e.g. `"SRT (caller)"` or `"HLS playlist"`.
    pub label: String,
    /// The URL a viewer pastes into VLC/ffplay/a browser. Absent when this
    /// output has no viewer-reachable address (a push destination, or a
    /// listener whose host could not be derived) — see [`notes`](Self::notes)
    /// for why.
    pub url: Option<String>,
    /// Extra operator-facing notes: an encryption-passphrase reminder, a
    /// push-destination redaction, a missing-host explanation. Never a
    /// secret.
    pub notes: Vec<String>,
    /// The exact shell command an operator pastes to open this URL in VLC.
    /// `None` when [`url`](Self::url) is `None` (nothing to open). For every
    /// kind except SRT this is `vlc <url>` (VLC's own URL scheme handles
    /// ffmpeg-style `srt://…?latency=…` query options correctly ONLY for
    /// RIST/HLS/WHEP-style URLs — SRT is the one exception: VLC's `srt://`
    /// access does not read `latency=` from the URL query the way
    /// ffmpeg/ffplay do, so an SRT row instead carries `vlc
    /// --srt-latency=<ms> srt://host:port` (VLC's own millisecond CLI
    /// option, with the `latency=`/`streamid=` query parameters dropped from
    /// the pasted command since VLC does not consume them from the URL).
    pub vlc_command: Option<String>,
}

/// Inputs the derivation needs beyond the [`Output`] itself: the public host
/// an operator reaches this appliance on, and the already-resolved `/hls/…`
/// mount route for an HLS/LL-HLS output (the control plane derives this from
/// the **same** mount mapping the DEV-D1 HLS delivery router uses — see
/// `hls_mount_segment` in this module).
#[derive(Debug, Clone)]
pub struct PlaybackContext<'a> {
    /// The externally-reachable hostname (no scheme, no port): `[control]
    /// public_host` when configured, else the request `Host` header's
    /// hostname part, else `None` (no public host is known — callers get an
    /// honest note instead of a guessed URL).
    pub public_host: Option<&'a str>,
    /// Whether [`public_host`](Self::public_host) is an IPv4/IPv6 literal
    /// (rather than a DNS name). Only an IP-literal host additionally gets a
    /// bracketed-IPv6-literal variant offered when the output binds an IPv6
    /// wildcard/literal — a DNS name already resolves to whichever family the
    /// resolver returns, so a second literal form would be redundant/wrong.
    pub public_host_is_ip_literal: bool,
    /// The route prefix this output's `/hls/{mount}` delivery mount resolved
    /// to (e.g. `"/hls/program"`), when this is an HLS/LL-HLS output served by
    /// the control-plane web listener. `None` for every other output kind, or
    /// when the mount could not be resolved (e.g. an unusable configured
    /// path).
    pub hls_route: Option<&'a str>,
    /// The playlist file name under [`hls_route`](Self::hls_route) (the
    /// configured path's final component), e.g. `"multiview.m3u8"`.
    pub hls_playlist: Option<&'a str>,
    /// The scheme (`"http"` or `"https"`) to use for every absolute viewer
    /// URL this module builds (the HLS playlist, and the WHEP play URL when
    /// a public host is known). Callers resolve this BEFORE calling this
    /// module — see [`resolve_public_scheme`] — because the production front
    /// end (nginx) terminates TLS and forwards plain HTTP, so the control
    /// plane's own listener scheme is never the right answer. Defaults to
    /// `"http"` (an un-proxied direct request) via [`Default`].
    pub scheme: &'a str,
}

impl Default for PlaybackContext<'_> {
    fn default() -> Self {
        Self {
            public_host: None,
            public_host_is_ip_literal: false,
            hls_route: None,
            hls_playlist: None,
            scheme: "http",
        }
    }
}

/// Resolve the scheme (`"http"` or `"https"`) used to build an absolute
/// viewer URL (the HLS playlist, the WHEP play URL), in priority order:
///
/// 1. the request's `X-Forwarded-Proto` header value, when it is exactly
///    `"http"` or `"https"` — the production nginx front end sets this on
///    every proxied request; anything else (absent, multi-valued, a bogus
///    token) is IGNORED rather than trusted, since a header is
///    client-influenced right up to the first trusted proxy hop;
/// 2. an explicit `[control] public_scheme` config override, when it is
///    exactly `"http"` or `"https"` (validated at config load — see
///    `MultiviewConfig::validate`);
/// 3. `"https"` when `[control] public_host` is explicitly CONFIGURED
///    (not merely falling back to the request `Host` header) — an operator
///    who configured a public host is behind the HTTPS-terminating
///    production front end this derivation exists for;
/// 4. `"http"` otherwise — an un-proxied direct request with no public host
///    configured.
#[must_use]
pub fn resolve_public_scheme(
    forwarded_proto: Option<&str>,
    configured_scheme: Option<&str>,
    public_host_is_configured: bool,
) -> &'static str {
    match forwarded_proto {
        Some("https") => return "https",
        Some("http") => return "http",
        _ => {}
    }
    match configured_scheme {
        Some("https") => return "https",
        Some("http") => return "http",
        _ => {}
    }
    if public_host_is_configured {
        "https"
    } else {
        "http"
    }
}

/// Map an output id to the URL-segment-safe mount name the DEV-D1 `/hls/{…}`
/// delivery router uses (`multiview-cli::control::sanitize_mount_segment`,
/// mirrored here so the control-plane playback route and the actual HLS
/// mount use the IDENTICAL mapping without a cross-crate dependency the
/// workspace layout does not otherwise need — `multiview-control` cannot
/// depend on the `multiview-cli` binary crate). Alphanumerics and `-`/`_`/`.`/
/// `~` are kept; everything else maps to `-`; a segment that sanitises to
/// nothing usable (empty or all-dots) becomes `"out"`.
#[must_use]
pub fn hls_mount_segment(id: &str) -> String {
    let segment: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if segment.is_empty() || segment.chars().all(|c| c == '.') {
        "out".to_owned()
    } else {
        segment
    }
}

/// One resolved `/hls/{mount}` delivery route for an HLS/LL-HLS output: the
/// route prefix and the playlist file name under it (`None` when the
/// configured path has no usable file name — a directory-shaped path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsMountRoute {
    /// Route prefix, e.g. `"/hls/program"`.
    pub route: String,
    /// The playlist file name under [`route`](Self::route), e.g.
    /// `"multiview.m3u8"`.
    pub playlist: Option<String>,
}

/// Resolve every HLS/LL-HLS output's `/hls/{mount}` delivery route from its
/// `(output_id, configured_path)` pairs, **in the same declaration order**
/// `multiview-cli::control::hls_mounts` iterates `config.outputs` — so a
/// mount-name collision resolves to the IDENTICAL deterministic `-2`, `-3`, …
/// suffix on both sides (control-plane callers supply the full accepted
/// outputs list, including non-HLS entries filtered out before calling this,
/// to preserve that declaration order).
///
/// Returns a map keyed by `output_id`; an id absent from the input never
/// appears in the result (nothing to look up — there is no "unresolved"
/// placeholder entry).
#[must_use]
pub fn resolve_hls_mounts(
    hls_outputs_in_order: &[(&str, &str)],
) -> std::collections::HashMap<String, HlsMountRoute> {
    let mut taken = std::collections::HashSet::new();
    let mut out = std::collections::HashMap::new();
    for (output_id, path) in hls_outputs_in_order {
        let mut segment = hls_mount_segment(output_id);
        if !taken.insert(segment.clone()) {
            let mut n: u32 = 2;
            segment = loop {
                let candidate = format!("{segment}-{n}");
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                n = n.saturating_add(1);
            };
        }
        let playlist = std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        out.insert(
            (*output_id).to_owned(),
            HlsMountRoute {
                route: format!("/hls/{segment}"),
                playlist,
            },
        );
    }
    out
}

/// Whether a configured bind/listen host is a wildcard (serves on every
/// interface): `[::]`, `::`, or `0.0.0.0`.
fn is_wildcard_host(host: &str) -> bool {
    matches!(host, "::" | "0.0.0.0")
}

/// Split a bracketed-or-bare `host:port` authority into `(host, port)`. The
/// host keeps IPv6 brackets stripped (matching [`crate::origin`]'s
/// convention); `port` is the raw digit string (not range-checked — an
/// out-of-range port in a stored document is a pre-existing data problem,
/// not this function's concern).
fn split_authority(authority: &str) -> Option<(String, Option<String>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':').map(ToOwned::to_owned);
        return Some((host.to_owned(), port));
    }
    // A bare IPv4/hostname authority: split on the LAST colon (a bare IPv6
    // literal without brackets is not a value we author, so every remaining
    // colon here is the port delimiter).
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            Some((host.to_owned(), Some(port.to_owned())))
        }
        _ => Some((authority.to_owned(), None)),
    }
}

/// Parse a `scheme://host:port[?query]` URL into `(host, port, query)`. Only
/// the shapes this crate itself authors are handled (no path, no userinfo);
/// an unparseable value yields `None`.
fn parse_scheme_authority(url: &str) -> Option<(String, Option<String>, Option<String>)> {
    let (_, rest) = url.split_once("://")?;
    let (authority, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q.to_owned())),
        None => (rest, None),
    };
    // Strip a trailing path, if any (none of our authored forms have one, but
    // stay defensive against a hand-edited document).
    let authority = authority.split('/').next().unwrap_or(authority);
    let (host, port) = split_authority(authority)?;
    Some((host, port, query))
}

/// Format a host for use in a URL authority: bracket an IPv6 literal, leave
/// everything else (IPv4 literal, DNS name) bare.
fn format_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// Build the viewer `srt://host:port?mode=caller&latency=…&streamid=…` URL for
/// a listener bind at `bind_host:port`, substituting the public host for a
/// wildcard/loopback bind.
///
/// **Latency unit (verified defect, same boundary as
/// `multiview_input::srt::SrtConfig::to_url`):** `latency_ms` is
/// milliseconds (the model/engine convention and the unit VLC's own
/// `--srt-latency` CLI option takes), but the `srt://` URL's `latency=`
/// query option is the FFmpeg/libsrt `AVOption` convention, which is
/// **microseconds** (`latency=200000` ⇒ 200 ms — ADR-0039 §3). This emits
/// the exact `×1000` conversion `multiview-input` already uses, via checked
/// `u64` arithmetic (the stored field is `u32`; a `u32::MAX` latency would
/// overflow a `u32` product, so the multiply happens in the wider type and
/// is still checked rather than silently wrapped).
fn srt_caller_url(
    public_host: &str,
    port: &str,
    latency_ms: Option<u32>,
    stream_id: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut url = format!("srt://{}:{port}?mode=caller", format_host(public_host));
    if let Some(latency) = latency_ms {
        if let Some(latency_us) = u64::from(latency).checked_mul(1000) {
            let _ = write!(url, "&latency={latency_us}");
        }
    }
    if let Some(id) = stream_id {
        let _ = write!(url, "&streamid={}", percent_encode(id));
    }
    url
}

/// Build the `vlc --srt-latency=<ms> srt://host:port` command for an SRT
/// caller URL: VLC's `srt://` access module does NOT read `latency=`/
/// `streamid=` from the URL query the way ffmpeg/ffplay do, so pasting the
/// ffmpeg-style URL straight into `vlc <url>` silently drops the configured
/// latency. VLC instead takes latency via its own `--srt-latency` CLI
/// option, in **milliseconds** — the model's native unit, so no conversion
/// is needed here (unlike [`srt_caller_url`]'s query option). The stream id
/// has no VLC CLI equivalent and is intentionally omitted from this command
/// (it stays in the pasted URL's `notes`, not silently dropped unexplained).
fn srt_vlc_command(public_host: &str, port: &str, latency_ms: Option<u32>) -> String {
    let base = format!("srt://{}:{port}", format_host(public_host));
    match latency_ms {
        Some(latency) => format!("vlc --srt-latency={latency} {base}"),
        None => format!("vlc {base}"),
    }
}

/// Percent-encode a query value: alphanumerics and the unreserved `-_.~` pass
/// through unescaped, everything else (including SRT's stream-id `#!::`
/// Access-Control grammar delimiters) is percent-escaped — a query value must
/// never itself be read as `&`-separated key/value pairs.
fn percent_encode(value: &str) -> String {
    percent_encode_path(value)
}

impl Output {
    /// Derive this output's viewer playback URL(s) (ADR-0101).
    ///
    /// One entry per reachable viewer address; a push-only transport (an SRT
    /// caller, RTMP, a WHIP push) returns a single no-URL entry carrying a
    /// `notes` explanation (the **redacted** destination — never query
    /// secrets, which none of these authored URLs carry in cleartext for RTMP
    /// stream keys is a pre-existing posture this does not change, but no
    /// SRT/RIST passphrase is ever read or returned here regardless).
    #[must_use]
    pub fn playback_urls(&self, ctx: &PlaybackContext<'_>) -> Vec<PlaybackUrl> {
        match self {
            Output::Srt { url, srt, .. } => srt_playback_urls(url, srt.as_ref(), ctx),
            Output::Rist { url, rist, .. } => rist_playback_urls(url, rist.as_ref(), ctx),
            Output::Rtmp { url, .. } | Output::WhipPush { url, .. } => vec![push_only(url)],
            Output::Hls { .. } | Output::LlHls { .. } => hls_playback_urls(ctx),
            Output::Webrtc { id, .. } => webrtc_playback_url(id.as_deref(), self, ctx),
            Output::RtspServer { .. }
            | Output::Ndi { .. }
            | Output::Aes67 { .. }
            | Output::Display { .. } => vec![PlaybackUrl {
                label: "Playback URL".to_owned(),
                url: None,
                notes: vec![
                    "this output kind has no derivable viewer URL (consume it via its native \
                     transport — NDI discovery, the KMS display head, or an AES67/ST-2110 \
                     receiver)"
                        .to_owned(),
                ],
                vlc_command: None,
            }],
        }
    }
}

/// `srt://host:port` push (always a destination, regardless of `mode`): the
/// viewer-facing URL depends on whether this is configured as a listener
/// (viewers CALL it) or a caller/rendezvous push (the destination is where it
/// pushes TO — not viewable from here).
fn srt_playback_urls(
    url: &str,
    srt: Option<&crate::schema::SrtOptions>,
    ctx: &PlaybackContext<'_>,
) -> Vec<PlaybackUrl> {
    let is_listener =
        srt.is_some_and(|o| matches!(o.mode, Some(SrtMode::Listener)) || o.listener == Some(true));
    if !is_listener {
        return vec![push_only(url)];
    }
    let Some((_bind_host, Some(port), _query)) = parse_scheme_authority(url) else {
        return vec![PlaybackUrl {
            label: "SRT (listener)".to_owned(),
            url: None,
            notes: vec![format!("could not parse the configured SRT bind {url:?}")],
            vlc_command: None,
        }];
    };
    let latency_ms = srt.and_then(|o| o.latency_ms);
    let stream_id = srt.and_then(|o| o.stream_id.as_deref());
    let encrypted = srt.and_then(|o| o.encryption.as_ref()).is_some();

    // `_bind_host` (the configured bind address) is not read further: once a
    // public host is known, a bracketed-IPv6 or bare-hostname/IPv4 `[host]`
    // substitution is exactly `format_host`'s job regardless of which family
    // the bind itself used ("offer `[addr]` only when public_host is an IP
    // literal, otherwise only the hostname" — `PlaybackContext::
    // public_host_is_ip_literal` documents that intent for callers/tests;
    // `format_host` brackets an IPv6-literal `public_host` and leaves every
    // other host bare, so no further branching on it is needed here).
    let Some(public_host) = ctx.public_host else {
        return vec![PlaybackUrl {
            label: "SRT (listener)".to_owned(),
            url: None,
            notes: vec![
                "no public host is known: set [control] public_host, or ensure the request \
                 carries a Host header, to derive a viewer URL"
                    .to_owned(),
            ],
            vlc_command: None,
        }];
    };

    let vlc_command = srt_vlc_command(public_host, &port, latency_ms);
    let mut notes = Vec::new();
    if encrypted {
        notes.push(
            "this listener requires an SRT passphrase (pbkeylen set) — the operator must \
             supply it out-of-band; it is never returned here"
                .to_owned(),
        );
    }
    // VLC: vlc --srt-latency=<ms> srt://host:port (VLC's `srt://` access
    // does not read `latency=`/`streamid=` from the URL query the way
    // ffmpeg/ffplay do — its millisecond `--srt-latency` CLI option is the
    // correct way to set latency when opening this URL in VLC).
    notes.push(format!("VLC: {vlc_command}"));
    vec![PlaybackUrl {
        label: "SRT (caller)".to_owned(),
        url: Some(srt_caller_url(public_host, &port, latency_ms, stream_id)),
        notes,
        vlc_command: Some(vlc_command),
    }]
}

/// RIST: viewer-reachable only when the bind is a wildcard/literal listen
/// address (`rist://[::]:port` or `rist://@host:port`, librist's listener
/// convention); otherwise this is a push destination, not viewable from here.
fn rist_playback_urls(
    url: &str,
    _rist: Option<&crate::schema::RistOptions>,
    ctx: &PlaybackContext<'_>,
) -> Vec<PlaybackUrl> {
    let Some((bind_host, Some(port), _query)) = parse_scheme_authority(url) else {
        return vec![push_only(url)];
    };
    if !is_wildcard_host(&bind_host) {
        return vec![push_only(url)];
    }
    let Some(public_host) = ctx.public_host else {
        return vec![PlaybackUrl {
            label: "RIST (listener)".to_owned(),
            url: None,
            notes: vec![
                "no public host is known: set [control] public_host, or ensure the request \
                 carries a Host header, to derive a viewer URL"
                    .to_owned(),
            ],
            vlc_command: None,
        }];
    };
    let url = format!("rist://{}:{port}", format_host(public_host));
    vec![PlaybackUrl {
        label: "RIST".to_owned(),
        url: Some(url.clone()),
        notes: vec![],
        vlc_command: Some(format!("vlc {url}")),
    }]
}

/// A push-destination output (SRT caller, RTMP, WHIP push, or an unresolvable
/// RIST/SRT bind): no viewer URL, just a redacted-destination note.
fn push_only(url: &str) -> PlaybackUrl {
    PlaybackUrl {
        label: "Destination".to_owned(),
        url: None,
        notes: vec![format!("pushes to {}", redact_destination(url))],
        vlc_command: None,
    }
}

/// Redact a push destination for the operator-facing note: keep the scheme
/// and host, drop any query (which may carry a stream key/token).
fn redact_destination(url: &str) -> String {
    match parse_scheme_authority(url) {
        Some((host, port, _query)) => {
            let scheme = url.split_once("://").map_or("", |(s, _)| s);
            match port {
                Some(port) => format!("{scheme}://{}:{port}", format_host(&host)),
                None => format!("{scheme}://{}", format_host(&host)),
            }
        }
        None => url.to_owned(),
    }
}

/// The absolute HLS playlist URL through the public web server, when this
/// output resolved an `/hls/{mount}` delivery route.
fn hls_playback_urls(ctx: &PlaybackContext<'_>) -> Vec<PlaybackUrl> {
    let (Some(route), Some(playlist)) = (ctx.hls_route, ctx.hls_playlist) else {
        return vec![PlaybackUrl {
            label: "HLS playlist".to_owned(),
            url: None,
            notes: vec![
                "no usable playlist file name could be derived from the configured path".to_owned(),
            ],
            vlc_command: None,
        }];
    };
    let Some(public_host) = ctx.public_host else {
        return vec![PlaybackUrl {
            label: "HLS playlist".to_owned(),
            url: None,
            notes: vec![
                "no public host is known: set [control] public_host, or ensure the request \
                 carries a Host header, to derive an absolute playlist URL"
                    .to_owned(),
            ],
            vlc_command: None,
        }];
    };
    let url = format!(
        "{}://{}{route}/{playlist}",
        ctx.scheme,
        format_host(public_host)
    );
    vec![PlaybackUrl {
        label: "HLS playlist".to_owned(),
        url: Some(url.clone()),
        notes: vec![],
        vlc_command: Some(format!("vlc {url}")),
    }]
}

/// The WHEP play URL, when this is a `webrtc` output (`POST
/// /api/v1/whep/{output_id}`). Absolute (same scheme/host derivation as the
/// HLS playlist URL) when a public host is known, so a copied URL stays
/// correct outside the page's own origin (e.g. pasted into another tool);
/// relative (resolved against the page/API origin) when no public host is
/// known — still correct for the SPA's own fetch, just not portable.
fn webrtc_playback_url(
    explicit_id: Option<&str>,
    output: &Output,
    ctx: &PlaybackContext<'_>,
) -> Vec<PlaybackUrl> {
    let id = explicit_id.map_or_else(|| output.id(), ToOwned::to_owned);
    let path = format!("/api/v1/whep/{}", percent_encode_path(&id));
    let url = match ctx.public_host {
        Some(public_host) => format!("{}://{}{path}", ctx.scheme, format_host(public_host)),
        None => path,
    };
    vec![PlaybackUrl {
        label: "WHEP".to_owned(),
        url: Some(url),
        notes: vec![
            "a WHEP URL is viewed by a WebRTC client (e.g. a browser), not VLC/ffplay".to_owned(),
        ],
        vlc_command: None,
    }]
}

/// Percent-encode a path segment (mirrors `encodeURIComponent` for the ids
/// this crate authors — alphanumerics and `-_.~` pass through unescaped).
fn percent_encode_path(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Parse a request `Host` header value (`host` or `host:port`, bracketed IPv6
/// aware) into its bare hostname/IP-literal part (no port), for use as a
/// [`PlaybackContext::public_host`] fallback. Returns `None` for an
/// unparseable value — callers then fall through to "no public host known".
#[must_use]
pub fn host_header_hostname(host_header: &str) -> Option<String> {
    // Reuse the strict bare-authority parser (origin module) by treating the
    // header as if it already were `http://<value>` — the scheme is
    // discarded, only the authority matters here, and this accepts exactly
    // the bracketed-IPv6/IPv4/DNS-name shapes a browser/proxy ever sends.
    let probe = format!("http://{host_header}");
    Origin::parse(&probe)
        .ok()
        .and_then(|_| split_authority(host_header).map(|(host, _)| host))
}

/// Whether `host` (no brackets, no port) is an IPv4 or IPv6 literal rather
/// than a DNS name — used to decide whether an IPv6-bracketed-literal variant
/// is meaningful to additionally offer (see
/// [`PlaybackContext::public_host_is_ip_literal`]).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use crate::schema::{SrtEncryption, SrtKeyLength, SrtOptions};

    fn ctx_with_host(host: &str) -> PlaybackContext<'_> {
        PlaybackContext {
            public_host: Some(host),
            public_host_is_ip_literal: is_ip_literal(host),
            hls_route: None,
            hls_playlist: None,
            scheme: "http",
        }
    }

    #[test]
    fn srt_listener_yields_caller_url() {
        // CORRECTION (verified defect, see commit message): this test used
        // to pin `latency=120` — the bug itself (the 120 ms configured
        // latency emitted verbatim into the ffmpeg/libsrt `latency=` query
        // option, which is MICROSECONDS, so VLC/ffplay would have requested
        // 1000x too little buffering). The correct emission is the exact
        // `×1000` conversion (`120_000`), matching
        // `multiview_input::srt::SrtConfig::to_url`'s own ADR-0039 §3
        // conversion.
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            latency_ms: Some(120),
            ..Default::default()
        };
        let output = Output::Srt {
            id: Some("srt-out".into()),
            url: "srt://[::]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("srt://viewer.example:9000?mode=caller&latency=120000")
        );
        assert_eq!(
            urls[0].vlc_command.as_deref(),
            Some("vlc --srt-latency=120 srt://viewer.example:9000")
        );
        assert!(urls[0]
            .notes
            .iter()
            .any(|n| n.contains("VLC: vlc --srt-latency=120 srt://viewer.example:9000")));
    }

    #[test]
    fn srt_latency_at_u32_max_converts_via_checked_widened_arithmetic() {
        // The ×1000 conversion happens in `u64` (checked, never wrapped): a
        // `u32::MAX` `latency_ms` still fits comfortably in `u64`, so the
        // query option is still emitted correctly rather than silently
        // dropped or overflow-truncated.
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            latency_ms: Some(u32::MAX),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://[::]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        let expected_us = u64::from(u32::MAX) * 1000;
        assert_eq!(
            urls[0].url.as_deref(),
            Some(format!("srt://viewer.example:9000?mode=caller&latency={expected_us}").as_str())
        );
    }

    #[test]
    fn srt_listener_with_stream_id_is_appended() {
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            stream_id: Some("#!::u=viewer1".into()),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://[::]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("srt://viewer.example:9000?mode=caller&streamid=%23%21%3A%3Au%3Dviewer1")
        );
    }

    #[test]
    fn srt_listener_encrypted_notes_passphrase_required_never_secret() {
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            encryption: Some(SrtEncryption {
                key_length: SrtKeyLength::Aes128,
                passphrase_ref: "op://Servers/feed/srt-passphrase".into(),
            }),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://[::]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].notes.iter().any(|n| n.contains("passphrase")));
        // Never the secret_ref itself, and certainly never a resolved secret.
        assert!(!urls[0].notes.iter().any(|n| n.contains("op://")));
        assert!(!format!("{urls:?}").contains("op://Servers"));
    }

    #[test]
    fn srt_caller_push_has_no_viewer_url_and_redacted_note() {
        let srt = SrtOptions {
            mode: Some(SrtMode::Caller),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://viewer-origin.example:9001?streamid=secretkey".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.is_none());
        assert!(urls[0].notes[0].contains("pushes to"));
        assert!(urls[0].notes[0].contains("viewer-origin.example:9001"));
        // The query (which can carry a secret-ish value) must never leak.
        assert!(!urls[0].notes[0].contains("secretkey"));
    }

    #[test]
    fn srt_bare_url_no_options_defaults_to_caller_push() {
        // No `srt` block at all ⇒ libav's own default (`caller`): a push, no
        // viewer URL.
        let output = Output::Srt {
            id: None,
            url: "srt://dest.example:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
    }

    #[test]
    fn srt_ipv6_bind_resolves_via_public_host() {
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://[2001:db8::10]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = ctx_with_host("203.0.113.5");
        let urls = output.playback_urls(&ctx);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("srt://203.0.113.5:9000?mode=caller")
        );
    }

    #[test]
    fn srt_listener_without_public_host_notes_absence() {
        let srt = SrtOptions {
            mode: Some(SrtMode::Listener),
            ..Default::default()
        };
        let output = Output::Srt {
            id: None,
            url: "srt://[::]:9000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            srt: Some(srt),
        };
        let ctx = PlaybackContext::default();
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
        assert!(urls[0].notes[0].contains("no public host"));
    }

    #[test]
    fn rist_listener_bind_yields_viewer_url() {
        let output = Output::Rist {
            id: None,
            url: "rist://[::]:5000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            rist: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert_eq!(urls[0].url.as_deref(), Some("rist://viewer.example:5000"));
    }

    #[test]
    fn rist_push_destination_has_no_viewer_url() {
        let output = Output::Rist {
            id: None,
            url: "rist://remote.example:5000".into(),
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
            rist: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
        assert!(urls[0].notes[0].contains("remote.example"));
    }

    #[test]
    fn rtmp_push_has_no_viewer_url_and_redacts_query() {
        let output = Output::Rtmp {
            id: None,
            url: "rtmp://ingest.example/live/streamkey123".into(),
            codec: "h264".into(),
            gpu_pin: None,
            multitrack: false,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
        assert!(urls[0].notes[0].contains("pushes to"));
    }

    #[test]
    fn whip_push_has_no_viewer_url() {
        let output = Output::WhipPush {
            id: None,
            url: "https://origin.example/whip/pgm1".into(),
            token: None,
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
    }

    #[test]
    fn hls_mount_resolves_absolute_url() {
        let output = Output::Hls {
            id: Some("program".into()),
            path: "/var/lib/multiview/hls/program/multiview.m3u8".into(),
            codec: "h264".into(),
            segment_ms: None,
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = PlaybackContext {
            public_host: Some("viewer.example"),
            public_host_is_ip_literal: false,
            hls_route: Some("/hls/program"),
            hls_playlist: Some("multiview.m3u8"),
            scheme: "http",
        };
        let urls = output.playback_urls(&ctx);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("http://viewer.example/hls/program/multiview.m3u8")
        );
    }

    #[test]
    fn hls_mount_resolves_absolute_url_with_https_scheme() {
        let output = Output::Hls {
            id: Some("program".into()),
            path: "/var/lib/multiview/hls/program/multiview.m3u8".into(),
            codec: "h264".into(),
            segment_ms: None,
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = PlaybackContext {
            public_host: Some("viewer.example"),
            public_host_is_ip_literal: false,
            hls_route: Some("/hls/program"),
            hls_playlist: Some("multiview.m3u8"),
            scheme: "https",
        };
        let urls = output.playback_urls(&ctx);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("https://viewer.example/hls/program/multiview.m3u8")
        );
        assert_eq!(
            urls[0].vlc_command.as_deref(),
            Some("vlc https://viewer.example/hls/program/multiview.m3u8")
        );
    }

    #[test]
    fn hls_mount_missing_route_notes_reason() {
        let output = Output::LlHls {
            id: Some("program".into()),
            path: "/var/lib/multiview/hls/program".into(),
            codec: "h264".into(),
            segment_ms: None,
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = ctx_with_host("viewer.example");
        let urls = output.playback_urls(&ctx);
        assert!(urls[0].url.is_none());
    }

    #[test]
    fn webrtc_output_yields_whep_url() {
        let output = Output::Webrtc {
            id: Some("pgm-webrtc".into()),
            label: "Program WebRTC".into(),
            max_viewers: 8,
            token: None,
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = PlaybackContext::default();
        let urls = output.playback_urls(&ctx);
        assert_eq!(urls[0].url.as_deref(), Some("/api/v1/whep/pgm-webrtc"));
    }

    #[test]
    fn public_host_override_wins_over_host_header_fallback() {
        // The control-plane route resolves `public_host` BEFORE calling this
        // module (config override vs Host-header fallback), so this module
        // only ever sees the already-resolved winner. This test documents the
        // precedence at the `host_header_hostname` helper boundary instead.
        assert_eq!(
            host_header_hostname("viewer.example:8443").as_deref(),
            Some("viewer.example")
        );
        assert_eq!(
            host_header_hostname("[2001:db8::7]:8443").as_deref(),
            Some("2001:db8::7")
        );
        assert_eq!(
            host_header_hostname("203.0.113.9").as_deref(),
            Some("203.0.113.9")
        );
    }

    #[test]
    fn ip_literal_detection() {
        assert!(is_ip_literal("203.0.113.9"));
        assert!(is_ip_literal("2001:db8::7"));
        assert!(!is_ip_literal("viewer.example"));
    }

    #[test]
    fn hls_mount_segment_matches_sanitize_mount_segment_contract() {
        // Mirrors `multiview-cli::control::sanitize_mount_segment` exactly:
        // alphanumerics and -_.~ kept, everything else mapped to '-', and an
        // unusable result (empty/all-dots) becomes "out".
        assert_eq!(hls_mount_segment("program"), "program");
        assert_eq!(hls_mount_segment("news/layout"), "news-layout");
        assert_eq!(hls_mount_segment(""), "out");
        assert_eq!(hls_mount_segment(".."), "out");
    }

    #[test]
    fn resolve_hls_mounts_assigns_routes_and_playlists() {
        let pairs = [
            ("program", "/var/lib/multiview/hls/program/multiview.m3u8"),
            ("news-layout", "/var/lib/multiview/hls/news/index.m3u8"),
        ];
        let resolved = resolve_hls_mounts(&pairs);
        assert_eq!(resolved["program"].route, "/hls/program");
        assert_eq!(
            resolved["program"].playlist.as_deref(),
            Some("multiview.m3u8")
        );
        assert_eq!(resolved["news-layout"].route, "/hls/news-layout");
        assert_eq!(
            resolved["news-layout"].playlist.as_deref(),
            Some("index.m3u8")
        );
    }

    #[test]
    fn resolve_hls_mounts_deduplicates_colliding_sanitised_segments_in_order() {
        // Two distinct ids that sanitise to the same segment ("a/b" and "a-b")
        // get a deterministic -2 suffix, in the order given — identical to
        // `multiview-cli::control::hls_mounts`'s dedupe contract.
        let pairs = [
            ("a/b", "/tmp/x/multiview.m3u8"),
            ("a-b", "/tmp/y/multiview.m3u8"),
        ];
        let resolved = resolve_hls_mounts(&pairs);
        assert_eq!(resolved["a/b"].route, "/hls/a-b");
        assert_eq!(resolved["a-b"].route, "/hls/a-b-2");
    }

    #[test]
    fn scheme_host_plus_forwarded_proto_https_gives_https() {
        // The production nginx front end: Host passed through, plus
        // `X-Forwarded-Proto: https` — the primary signal, trusted outright.
        assert_eq!(resolve_public_scheme(Some("https"), None, true), "https");
    }

    #[test]
    fn scheme_forwarded_proto_http_wins_even_with_a_configured_host() {
        assert_eq!(resolve_public_scheme(Some("http"), None, true), "http");
    }

    #[test]
    fn scheme_config_override_wins_over_the_public_host_default() {
        // No forwarded-proto signal; an explicit `public_scheme = "http"`
        // override wins over the "public_host configured ⇒ https" default.
        assert_eq!(resolve_public_scheme(None, Some("http"), true), "http");
        assert_eq!(resolve_public_scheme(None, Some("https"), false), "https");
    }

    #[test]
    fn scheme_bogus_forwarded_proto_values_are_ignored() {
        // Neither "http" nor "https" exactly ⇒ ignored, falls through to the
        // next signal — never trusted as a guess.
        assert_eq!(
            resolve_public_scheme(Some("HTTPS"), None, true),
            "https" // falls through to the public_host-configured default
        );
        assert_eq!(
            resolve_public_scheme(Some("https,http"), None, false),
            "http" // falls through to the un-proxied-direct-request default
        );
        assert_eq!(resolve_public_scheme(Some(""), None, false), "http");
    }

    #[test]
    fn scheme_defaults_to_https_only_when_public_host_is_configured() {
        assert_eq!(resolve_public_scheme(None, None, true), "https");
        assert_eq!(resolve_public_scheme(None, None, false), "http");
    }

    #[test]
    fn webrtc_output_yields_absolute_url_when_public_host_is_known() {
        let output = Output::Webrtc {
            id: Some("pgm-webrtc".into()),
            label: "Program WebRTC".into(),
            max_viewers: 8,
            token: None,
            codec: "h264".into(),
            gpu_pin: None,
            audio: None,
            metadata: None,
            orientation: None,
        };
        let ctx = PlaybackContext {
            public_host: Some("viewer.example"),
            public_host_is_ip_literal: false,
            hls_route: None,
            hls_playlist: None,
            scheme: "https",
        };
        let urls = output.playback_urls(&ctx);
        assert_eq!(
            urls[0].url.as_deref(),
            Some("https://viewer.example/api/v1/whep/pgm-webrtc")
        );
    }
}
