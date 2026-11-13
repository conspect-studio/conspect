//! The plaintext port-80 -> HTTPS redirect (design note section 10).
//!
//! Reads at most [`MAX_REQUEST_HEAD`] bytes looking for the end of the request
//! line, answers `301` to `https://<public_host>[:port]<path>` and closes. The
//! target host comes from configuration only: the `Host` header is never read
//! (an open redirect through a forged `Host` is impossible by construction).

use std::net::IpAddr;

/// The most bytes read looking for the request line's `\r\n`.
pub const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// The sanitised path + query of a request line, `/` for anything unsafe or
/// not in origin form.
#[must_use]
pub fn request_path(request_head: &[u8]) -> String {
    let line_end = request_head
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(request_head.len());
    let line = request_head.get(..line_end).unwrap_or_default();
    let mut parts = line.split(|&b| b == b' ');
    let (Some(_method), Some(target)) = (parts.next(), parts.next()) else {
        return "/".to_owned();
    };
    let target = target
        .iter()
        .position(|&b| b == b'#')
        .and_then(|i| target.get(..i))
        .unwrap_or(target);
    let safe = target.first() == Some(&b'/')
        && !target.starts_with(b"//")
        && target.iter().all(|&b| b.is_ascii_graphic() && b != b'\\');
    if !safe {
        return "/".to_owned();
    }
    String::from_utf8(target.to_vec()).unwrap_or_else(|_| "/".to_owned())
}

/// The URL prefix of an HTTP-01 challenge (RFC 8555 section 8.3).
pub const ACME_CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// The largest challenge response served (a key authorization is about 87
/// bytes: token, `.`, a base64url SHA-256 thumbprint).
pub const MAX_ACME_RESPONSE: u64 = 1024;

/// The HTTP-01 token of a `GET` request line for the challenge path, or
/// `None`. The token must be base64url (RFC 8555 section 8.1: `[A-Za-z0-9_-]`,
/// 1..=128 characters), so it can never name anything but a file directly in
/// the challenge directory (no `/`, no `.`, no `..`).
#[must_use]
pub fn acme_token(request_head: &[u8]) -> Option<&str> {
    let line_end = request_head
        .windows(2)
        .position(|w| w == b"\r\n")?;
    let line = request_head.get(..line_end)?;
    let mut parts = line.split(|&b| b == b' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if method != b"GET" && method != b"HEAD" {
        return None;
    }
    let token = target.strip_prefix(ACME_CHALLENGE_PREFIX.as_bytes())?;
    let valid = (1..=128).contains(&token.len())
        && token
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| std::str::from_utf8(token).ok()).flatten()
}

/// A `200` carrying an HTTP-01 key authorization.
#[must_use]
pub fn acme_response(body: &[u8], head_only: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body);
    }
    out
}

/// The `404` for a challenge token with no file.
pub const ACME_NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// The redirect authority: `public_host`, an IPv6 literal bracketed, plus
/// `:port` unless it is 443.
#[must_use]
pub fn authority(public_host: &str, port: u16) -> String {
    let host = match public_host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
        _ => public_host.to_owned(),
    };
    if port == 443 {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// The complete `301` response for one request head.
#[must_use]
pub fn redirect_response(public_host: &str, port: u16, request_head: &[u8]) -> Vec<u8> {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        authority(public_host, port),
        request_path(request_head)
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(head: &[u8]) -> String {
        let r = String::from_utf8(redirect_response("mda001.example.net", 443, head)).unwrap();
        r.lines()
            .find_map(|l| l.strip_prefix("Location: "))
            .unwrap()
            .to_owned()
    }

    #[test]
    fn keeps_path_and_query_and_ignores_host() {
        assert_eq!(
            location(b"GET /a?b=1 HTTP/1.1\r\nHost: evil.example\r\n\r\n"),
            "https://mda001.example.net/a?b=1"
        );
        assert_eq!(
            location(b"GET /x#frag HTTP/1.1\r\n"),
            "https://mda001.example.net/x"
        );
    }

    #[test]
    fn unsafe_targets_become_root() {
        for head in [
            &b"GET //evil.example/ HTTP/1.1\r\n"[..],
            b"GET http://evil.example/ HTTP/1.1\r\n",
            b"GET /a\\b HTTP/1.1\r\n",
            b"GET /a\x01b HTTP/1.1\r\n",
            b"OPTIONS * HTTP/1.1\r\n",
            b"GET\r\n",
            b"",
            b"\xff\xfe",
            b"GET /\xc3\xa9 HTTP/1.1\r\n",
        ] {
            assert_eq!(location(head), "https://mda001.example.net/", "{head:?}");
        }
    }

    #[test]
    fn acme_tokens_are_base64url_only() {
        assert_eq!(
            acme_token(b"GET /.well-known/acme-challenge/abc_DEF-123 HTTP/1.1\r\nHost: x\r\n"),
            Some("abc_DEF-123")
        );
        assert_eq!(
            acme_token(b"HEAD /.well-known/acme-challenge/t HTTP/1.1\r\n"),
            Some("t")
        );
        for head in [
            &b"GET /.well-known/acme-challenge/ HTTP/1.1\r\n"[..],
            b"GET /.well-known/acme-challenge/../../etc/passwd HTTP/1.1\r\n",
            b"GET /.well-known/acme-challenge/a.b HTTP/1.1\r\n",
            b"GET /.well-known/acme-challenge/a/b HTTP/1.1\r\n",
            b"GET /.well-known/acme-challenge/a%2fb HTTP/1.1\r\n",
            b"GET /.well-known/acme-challenge/abc?x=1 HTTP/1.1\r\n",
            b"POST /.well-known/acme-challenge/abc HTTP/1.1\r\n",
            b"GET /.well-known/acme-challenge/abc HTTP/1.1",
            b"GET /x HTTP/1.1\r\n",
        ] {
            assert_eq!(acme_token(head), None, "{head:?}");
        }
        let long = format!("GET /.well-known/acme-challenge/{} HTTP/1.1\r\n", "a".repeat(129));
        assert_eq!(acme_token(long.as_bytes()), None);
    }

    #[test]
    fn authority_forms() {
        assert_eq!(authority("2001:db8::1", 443), "[2001:db8::1]");
        assert_eq!(authority("2001:db8::1", 8443), "[2001:db8::1]:8443");
        assert_eq!(authority("203.0.113.1", 8443), "203.0.113.1:8443");
        let r =
            String::from_utf8(redirect_response("h.example", 8443, b"GET / HTTP/1.1\r\n")).unwrap();
        assert_eq!(
            r,
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://h.example:8443/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }

    proptest::proptest! {
        #[test]
        fn never_panics_and_never_leaves_the_host(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let r = redirect_response("h.example", 443, &data);
            let text = String::from_utf8(r).unwrap();
            proptest::prop_assert!(text.contains("\r\nLocation: https://h.example/"));
            proptest::prop_assert_eq!(text.matches("\r\n").count(), 5);
        }
    }
}
