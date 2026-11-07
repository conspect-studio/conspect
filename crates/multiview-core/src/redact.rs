//! Display-safe push/transport URLs.
//!
//! One rule set shared by the pipeline (which stores the link status) and the
//! control plane (which serves it to read-role users), so the two can never
//! disagree about what is secret (review SRT-03).

/// A display-safe form of a transport URL: scheme and host only, plus, for
/// RTMP/RTMPS, the application path with the stream key (the final non-empty
/// path segment, even when it is the only one) replaced by `***`. Query,
/// fragment and userinfo are always removed, so an SRT passphrase or stream id,
/// a RIST secret and any `user:password@` never survive. A string that is not
/// a URL becomes `***`.
#[must_use]
pub fn redact_transport_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = base.split_once("://") else {
        return "***".to_owned();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path = path.trim_matches('/');
    if !scheme.to_ascii_lowercase().starts_with("rtmp") || path.is_empty() {
        return format!("{scheme}://{host}");
    }
    match path.rsplit_once('/') {
        Some((app, _key)) => format!("{scheme}://{host}/{app}/***"),
        None => format!("{scheme}://{host}/***"),
    }
}

/// [`redact_transport_url`] for a value that may be a URL or a bare socket
/// address (`[::1]:5000`, kept as is: it carries no credential).
#[must_use]
pub fn redact_peer(peer: &str) -> String {
    if peer.contains("://") || peer.contains('?') || peer.contains('@') || peer.contains('/') {
        redact_transport_url(peer)
    } else {
        peer.to_owned()
    }
}

/// A display-safe form of a **stored config-document** URL (review SRT-03-R2):
/// unlike [`redact_transport_url`] (used for the live link telemetry snapshot,
/// which drops every non-RTMP path because a push/pull transport's path there
/// is operationally uninteresting to a viewer), this keeps a benign path
/// (an HLS/WHIP/RTSP mount, a WHEP route, …) intact for usability and masks
/// only what the brief names as credential-bearing:
///
/// * **userinfo** (`user:pass@`) is always removed — an `rtsp://user:pass@…`
///   credential never survives;
/// * **query and fragment** are always removed — an SRT `?passphrase=&
///   streamid=`, a RIST `?secret=`, or an HLS/TS query-string access token
///   never survives;
/// * for **RTMP/RTMPS** only, the final non-empty path segment (the stream
///   key, even when it is the only segment) is additionally replaced with
///   `***` — the one scheme where the credential rides in the path, not the
///   query.
///
/// A non-URL string (no `scheme://`) becomes `***`, matching
/// [`redact_transport_url`].
#[must_use]
pub fn redact_resource_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = base.split_once("://") else {
        return "***".to_owned();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path = path.trim_end_matches('/');
    if scheme.to_ascii_lowercase().starts_with("rtmp") {
        if path.is_empty() {
            return format!("{scheme}://{host}");
        }
        return match path.rsplit_once('/') {
            Some((app, _key)) => format!("{scheme}://{host}/{app}/***"),
            None => format!("{scheme}://{host}/***"),
        };
    }
    // Every other scheme: the path is kept (it is operationally meaningful —
    // an RTSP mount, an HLS manifest path, a WHIP/WHEP route — and the brief
    // does not name it as credential-bearing); only userinfo/query/fragment,
    // already stripped above, carried the secret.
    if path.is_empty() {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/{path}")
    }
}

/// [`redact_resource_url`] for a value that may be a URL or a bare socket
/// address (`[::1]:5000`, kept as is: it carries no credential).
#[must_use]
pub fn redact_resource_peer(peer: &str) -> String {
    if peer.contains("://") || peer.contains('?') || peer.contains('@') {
        redact_resource_url(peer)
    } else {
        peer.to_owned()
    }
}
