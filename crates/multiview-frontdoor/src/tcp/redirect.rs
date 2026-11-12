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
