//! A strictly-parsed HTTP(S) *serialized origin* ([RFC 6454] §6.2) — the value
//! type the realtime CSWSH `Origin` allow-list (SEC-13, ADR-RT011) is built from
//! and matched against.
//!
//! An [`Origin`] is exactly `scheme "://" host [ ":" port ]` — an `http`/`https`
//! scheme, a non-empty host (a bracketed IPv6 literal keeps its brackets on the
//! wire), an optional port, and **nothing else**: no path, query, fragment, or
//! userinfo. The opaque `null` origin and any non-`http(s)` scheme are
//! unrepresentable — [`Origin::parse`] rejects them, so a value of this type is
//! always a real web origin (fail-closed by construction).
//!
//! This is deliberately *stricter* than a general URL parser. A lenient parser
//! that reduces `https://host/evil` to the bare authority `host` is a
//! same-origin-bypass footgun; a browser only ever sends the exact serialized
//! shape above in an `Origin` header, so that is all this grammar accepts. The
//! same parser backs config-load validation, allow-list construction, and the
//! request-header check, so the three can never diverge.
//!
//! [RFC 6454]: https://www.rfc-editor.org/rfc/rfc6454

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

/// Why a string is not a valid serialized origin. Carried in the config-load
/// error so a malformed `control.allowed_origins` entry fails fast with a precise
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OriginParseError {
    /// No `scheme://` delimiter, or the scheme is empty.
    MissingScheme,
    /// The scheme is not `http` or `https` (a browser `Origin` is always the
    /// initiating page's http(s) origin).
    UnsupportedScheme,
    /// The authority (host) is empty (e.g. `https://` or `https://:8080`).
    EmptyHost,
    /// The authority carries a path, query, fragment, or userinfo — a serialized
    /// origin has none of these.
    NotBareAuthority,
    /// The host is not a valid registered name, IPv4 literal, or bracketed IPv6.
    InvalidHost,
    /// The port is empty or not a decimal `1..=65535`.
    InvalidPort,
}

impl fmt::Display for OriginParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::MissingScheme => "missing or empty scheme (expected \"http://\" or \"https://\")",
            Self::UnsupportedScheme => "scheme must be http or https",
            Self::EmptyHost => "empty host",
            Self::NotBareAuthority => {
                "an origin is a bare scheme://host[:port] — no path, query, fragment, or userinfo"
            }
            Self::InvalidHost => {
                "invalid host (expected a hostname, IPv4 literal, or bracketed [IPv6] address)"
            }
            Self::InvalidPort => "invalid port (expected a decimal 1..=65535)",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for OriginParseError {}

/// The scheme of an [`Origin`] — the only two a browser ever puts in an `Origin`
/// header (the initiating page's scheme; never `ws`/`wss`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

/// A strictly-parsed HTTP(S) serialized origin (`scheme://host[:port]`). See the
/// [module docs](self).
///
/// Equality is the RFC 6454 origin tuple `(scheme, host, port)`, canonicalized at
/// parse (scheme + host ASCII-lowercased, an IPv6 host compressed to its canonical
/// form), so it is a sound key for an allow-list membership test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    scheme: Scheme,
    /// ASCII-lowercased host. A bracketed IPv6 literal is stored as its canonical
    /// compressed inner form (no brackets).
    host: String,
    port: Option<u16>,
}

impl Origin {
    /// Parse a serialized origin, strictly. Returns an [`OriginParseError`] for
    /// anything that is not exactly `http(s)://host[:port]` — including the opaque
    /// `null` origin, a path/query/fragment/userinfo, or a non-http(s) scheme.
    ///
    /// # Errors
    ///
    /// See [`OriginParseError`] for the precise rejection reasons.
    pub fn parse(input: &str) -> Result<Self, OriginParseError> {
        // Parse the input VERBATIM — no trim. A serialized Origin is exactly
        // `scheme://host[:port]`; leading/trailing/embedded ASCII whitespace makes
        // it malformed and must fail closed (a padded scheme fails the scheme
        // match below; padding elsewhere is caught by `parse_authority`'s
        // whitespace rejection). Trimming would silently normalize an attacker- or
        // typo-supplied `" https://x "` into an allow-listed origin.
        let (scheme_raw, rest) = input
            .split_once("://")
            .ok_or(OriginParseError::MissingScheme)?;
        let scheme = match scheme_raw.to_ascii_lowercase().as_str() {
            "" => return Err(OriginParseError::MissingScheme),
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            _ => return Err(OriginParseError::UnsupportedScheme),
        };
        let (host, port) = parse_authority(rest)?;
        Ok(Self { scheme, host, port })
    }

    /// Whether this origin's authority (`host[:port]`) equals the request `Host`
    /// header — the same-origin case (the embed-web SPA served from the appliance,
    /// zero config). The host compares case-insensitively and the port exactly, on
    /// the *parsed* authority; a `Host` value that is not itself a bare authority
    /// fails closed (`false`).
    #[must_use]
    pub fn matches_host(&self, host: &str) -> bool {
        match parse_authority(host) {
            Ok((host, port)) => self.host == host && self.port == port,
            Err(_) => false,
        }
    }
}

/// Parse a bare authority `host[:port]` (no scheme) into a canonical
/// `(host, port)`: host ASCII-lowercased (an IPv6 literal compressed, no
/// brackets), port an optional `1..=65535`. Rejects an empty host, any
/// path/query/fragment/userinfo, and a non-numeric or out-of-range port. Shared by
/// [`Origin::parse`] and the same-origin `Host` compare so the two never diverge.
fn parse_authority(input: &str) -> Result<(String, Option<u16>), OriginParseError> {
    if input.is_empty() {
        return Err(OriginParseError::EmptyHost);
    }
    // A serialized origin's authority is bare: no path/query/fragment start, no
    // userinfo `@`, no whitespace. Reject these up front so a lenient reduction
    // like `https://host/evil` → `host` (a same-origin bypass) is impossible.
    // (`:` is the port delimiter and `[` `]` are the IPv6 brackets — handled
    // below; everything else structural is refused here.)
    if input
        .bytes()
        .any(|b| matches!(b, b'/' | b'?' | b'#' | b'@') || b.is_ascii_whitespace())
    {
        return Err(OriginParseError::NotBareAuthority);
    }

    if let Some(rest) = input.strip_prefix('[') {
        // Bracketed IPv6 literal: `[<ipv6>]` optionally followed by `:<port>`.
        let (inner, after) = rest.split_once(']').ok_or(OriginParseError::InvalidHost)?;
        let addr = Ipv6Addr::from_str(inner).map_err(|_| OriginParseError::InvalidHost)?;
        let port = if after.is_empty() {
            None
        } else {
            // Anything after the `]` must be exactly `:<port>`; a bare suffix (no
            // colon) is not a valid authority.
            let digits = after
                .strip_prefix(':')
                .ok_or(OriginParseError::NotBareAuthority)?;
            Some(parse_port(digits)?)
        };
        return Ok((addr.to_string(), port));
    }

    // A registered name or IPv4 literal, optionally `:port`. Split on the LAST
    // colon: a valid host has at most one colon (the port delimiter). An
    // unbracketed IPv6 (`::1`, `2001:db8::7`) leaves a colon in the host part,
    // which the reg-name charset check below then rejects.
    let (host, port) = match input.rsplit_once(':') {
        Some((host, port)) => (host, Some(parse_port(port)?)),
        None => (input, None),
    };
    if host.is_empty() {
        return Err(OriginParseError::EmptyHost);
    }
    if !host.bytes().all(is_reg_name_byte) {
        return Err(OriginParseError::InvalidHost);
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Bytes allowed in an unbracketed host (a DNS registered name or IPv4 literal):
/// ASCII letters, digits, `-`, and `.`. Admits punycode (`xn--…`) and dotted-quad
/// IPv4 while rejecting the delimiters that would smuggle a path, port, or an
/// unbracketed IPv6 colon into the host.
const fn is_reg_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')
}

/// Parse a port segment: a non-empty decimal `1..=65535`. Rejects empty,
/// non-digit, and out of range (`0` is never a real origin port).
fn parse_port(s: &str) -> Result<u16, OriginParseError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(OriginParseError::InvalidPort);
    }
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(OriginParseError::InvalidPort),
        Ok(port) => Ok(port),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_shapes() {
        for good in [
            "https://ops.example",
            "http://mv.local:8080",
            "https://MV.Local",      // case-insensitive scheme + host
            "http://192.0.2.7:9000", // IPv4 literal
            "http://[2001:db8::7]",  // bracketed IPv6, no port
            "https://[2001:db8::7]:8443",
            "https://xn--e1afmkfd.example", // punycode reg-name
        ] {
            Origin::parse(good).unwrap_or_else(|e| panic!("{good:?} should parse: {e}"));
        }
    }

    #[test]
    fn rejects_the_panel_bad_shapes() {
        // The six the pre-fix `contains("://")` check wrongly accepted, plus the
        // opaque `null` and an unbracketed IPv6.
        for (bad, want) in [
            ("://", OriginParseError::MissingScheme),
            ("https://", OriginParseError::EmptyHost),
            ("https://user@host", OriginParseError::NotBareAuthority),
            ("https://host/path", OriginParseError::NotBareAuthority),
            ("https://host?x", OriginParseError::NotBareAuthority),
            ("garbage://host", OriginParseError::UnsupportedScheme),
            ("null", OriginParseError::MissingScheme),
            ("ws://host", OriginParseError::UnsupportedScheme),
            ("https://2001:db8::7", OriginParseError::InvalidHost),
            ("https://host:", OriginParseError::InvalidPort),
            ("https://host:0", OriginParseError::InvalidPort),
            ("https://host:70000", OriginParseError::InvalidPort),
            ("https://host#frag", OriginParseError::NotBareAuthority),
        ] {
            assert_eq!(
                Origin::parse(bad),
                Err(want),
                "{bad:?} must be rejected as {want:?}"
            );
        }
    }

    #[test]
    fn rejects_whitespace_padding() {
        // A serialized Origin has no surrounding or embedded whitespace. The parser
        // is verbatim (no `trim`), so a padded value fails closed rather than being
        // normalized into an allow-listed origin (panel rev3 finding).
        for bad in [
            " https://ops.example",  // leading space → scheme " https" mismatch
            "https://ops.example ",  // trailing space → authority whitespace
            "https://ops.example\n", // trailing newline
            "\thttps://ops.example", // leading tab
            "https:// ops.example",  // embedded space in the authority
        ] {
            assert!(
                Origin::parse(bad).is_err(),
                "{bad:?} (whitespace-padded) must be rejected, not trimmed-and-accepted"
            );
        }
    }

    #[test]
    fn canonicalizes_for_equality() {
        // Case + IPv6 form fold, so allow-list membership is exact-yet-canonical.
        assert_eq!(
            Origin::parse("https://MV.Local").unwrap(),
            Origin::parse("https://mv.local").unwrap()
        );
        assert_eq!(
            Origin::parse("http://[2001:db8:0:0:0:0:0:7]:80").unwrap(),
            Origin::parse("http://[2001:DB8::7]:80").unwrap()
        );
        // Scheme and port are part of the identity.
        assert_ne!(
            Origin::parse("http://mv.local").unwrap(),
            Origin::parse("https://mv.local").unwrap()
        );
        assert_ne!(
            Origin::parse("https://mv.local").unwrap(),
            Origin::parse("https://mv.local:8443").unwrap()
        );
    }

    #[test]
    fn matches_host_is_authority_only_and_fails_closed() {
        let origin = Origin::parse("https://mv.local:8080").unwrap();
        assert!(origin.matches_host("mv.local:8080"));
        assert!(origin.matches_host("MV.LOCAL:8080")); // case-insensitive host
        assert!(!origin.matches_host("mv.local")); // port is part of the authority
        assert!(!origin.matches_host("mv.local:9999")); // different port
        assert!(!origin.matches_host("mv.local/evil")); // not a bare authority → deny
        assert!(!origin.matches_host("")); // empty → deny

        // Scheme is NOT part of the same-origin (authority) compare — a
        // TLS-terminating proxy makes the backend see http while the browser
        // Origin is https; the authority is what defeats CSWSH.
        let https = Origin::parse("https://mv.local").unwrap();
        assert!(https.matches_host("mv.local"));

        // Bracketed IPv6 Host authority.
        let v6 = Origin::parse("http://[2001:db8::7]:8443").unwrap();
        assert!(v6.matches_host("[2001:db8::7]:8443"));
        assert!(!v6.matches_host("[2001:db8::7]"));
    }
}
