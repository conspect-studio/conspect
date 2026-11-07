//! Network-path diagnostics (docs/io/iptv-org.md §3a): the wire shapes of
//! `NetworkReport` (web `catalogue.ts`), plus the pure helpers that redact URLs
//! and judge the path. The network half (egress lookup, per-family dials, TLS
//! facts) lives in `netdiag::probe` behind the `catalogue` feature.
//!
//! Control plane only; nothing here is reachable from the data plane
//! (invariants #1/#10).

use std::collections::BTreeMap;

use serde::Serialize;

use super::probe::ProbeRequest;
use super::sync::BoxFuture;

#[cfg(feature = "catalogue")]
pub mod collect;

/// Collects a [`NetworkReport`] for one stream request. The control plane holds it
/// as a trait object so the routes compile without the `catalogue` feature.
pub trait NetworkCollector: Send + Sync {
    /// Collect the report; `channel_country` (ISO alpha-2) feeds the geo hint.
    fn collect(
        &self,
        req: ProbeRequest,
        channel_country: Option<String>,
    ) -> BoxFuture<'_, NetworkReport>;
}

/// How the internet sees this server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkEgress {
    /// Egress IPv4, when the server has v4 egress.
    pub ipv4: Option<String>,
    /// Egress IPv6, when the server has v6 egress.
    pub ipv6: Option<String>,
    /// Egress country (Cloudflare `loc`).
    pub country: Option<String>,
    /// CDN edge (Cloudflare `colo`).
    pub colo: Option<String>,
}

/// One resolved address with its per-family connection result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkAddress {
    /// The address.
    pub ip: String,
    /// `ipv4` or `ipv6`.
    pub family: &'static str,
    /// `net_guard` class: `public`, `private` or `blocked`.
    pub class: &'static str,
    /// TCP connect time.
    pub connect_ms: Option<u64>,
    /// HTTP status over this address.
    pub status: Option<u16>,
    /// Failure text.
    pub error: Option<String>,
}

/// DNS answer for a host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkDns {
    /// CNAME chain. The std resolver does not expose it, so this is empty.
    pub cname: Vec<String>,
    /// A records.
    pub a: Vec<String>,
    /// AAAA records.
    pub aaaa: Vec<String>,
    /// TTL. The std resolver does not expose it, so this is `null`.
    pub ttl: Option<u32>,
}

/// TLS facts for a host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkTls {
    /// Protocol version.
    pub version: Option<String>,
    /// Negotiated ALPN.
    pub alpn: Option<String>,
    /// Leaf subject.
    pub subject: Option<String>,
    /// Leaf issuer.
    pub issuer: Option<String>,
    /// Leaf expiry (RFC 3339).
    pub not_after: Option<String>,
}

/// One distinct host in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkHost {
    /// Host name (no userinfo, no port).
    pub host: String,
    /// `master`, `media`, `segment`, `key` or `redirect`.
    pub role: &'static str,
    /// DNS.
    pub dns: NetworkDns,
    /// Per-address results.
    pub addresses: Vec<NetworkAddress>,
    /// TLS facts, for https hosts.
    pub tls: Option<NetworkTls>,
}

/// One HTTP hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkHop {
    /// URL with userinfo, query and fragment removed.
    pub url_redacted: String,
    /// Status.
    pub status: Option<u16>,
    /// Elapsed ms.
    pub ms: Option<u64>,
    /// `Server` header.
    pub server: Option<String>,
    /// `Via` header.
    pub via: Option<String>,
    /// `Age` header.
    pub age: Option<String>,
    /// Known CDN headers.
    pub cdn_headers: BTreeMap<String, String>,
    /// At most 512 bytes of text for a 4xx response.
    pub body_excerpt: Option<String>,
}

/// The request headers the server sent (names only for the two it replays).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkRequestHeaders {
    /// `User-Agent`.
    pub user_agent: Option<String>,
    /// `Referer`.
    pub referrer: Option<String>,
}

/// The network-path report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkReport {
    /// Egress identity.
    pub egress: NetworkEgress,
    /// Distinct hosts.
    pub hosts: Vec<NetworkHost>,
    /// HTTP hops.
    pub hops: Vec<NetworkHop>,
    /// Headers sent.
    pub request_headers: NetworkRequestHeaders,
    /// Plain-language hints.
    pub assessment: Vec<String>,
    /// One equivalent `curl` per hop.
    pub curl: Vec<String>,
}

/// Remove userinfo, query and fragment from a URL.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or("");
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest
                .find('/')
                .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}://{host}{path}")
        }
        None => "(invalid url)".to_owned(),
    }
}

/// One equivalent `curl` command for a hop; URL redacted, header values shown
/// only as the operator's own `User-Agent`/`Referer`.
#[must_use]
pub fn curl_for(url: &str, headers: &NetworkRequestHeaders) -> String {
    let mut s = String::from("curl -sS -I");
    if let Some(ua) = &headers.user_agent {
        s.push_str(&format!(" -A {}", shell_quote(ua)));
    }
    if let Some(r) = &headers.referrer {
        s.push_str(&format!(" -e {}", shell_quote(r)));
    }
    s.push(' ');
    s.push_str(&shell_quote(&redact_url(url)));
    s
}

/// `curl` for one hop pinned to the address the server really used
/// (`-4`/`-6` and `--resolve host:port:ip`), so it reproduces the same path.
#[must_use]
pub fn curl_for_addr(
    url: &str,
    headers: &NetworkRequestHeaders,
    used: Option<(std::net::IpAddr, u16)>,
) -> String {
    let base = curl_for(url, headers);
    let Some((ip, port)) = used else { return base };
    let Some(host) = host_of(url) else {
        return base;
    };
    let fam = if ip.is_ipv6() { "-6" } else { "-4" };
    let host = host.trim_matches(['[', ']']);
    base.replacen(
        "curl -sS -I",
        &format!("curl -sS -I {fam} --resolve {host}:{port}:{ip}"),
        1,
    )
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

/// The host part of a URL, lower-cased, without port or userinfo.
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        hostport.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Parse the `key=value` lines of Cloudflare's `/cdn-cgi/trace` into
/// `(ip, loc, colo)`.
#[must_use]
pub fn parse_trace(body: &str) -> (Option<String>, Option<String>, Option<String>) {
    let get = |k: &str| {
        body.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty() && v.len() <= 64)
    };
    (get("ip"), get("loc"), get("colo"))
}

/// Build the plain-language assessment (§3a rules).
///
/// * channel country differs from the egress country → geo hint;
/// * the same host answers differently over v4 and v6 → family hint;
/// * the segment host differs from the playlist host → CDN hint.
#[must_use]
pub fn assess(report: &NetworkReport, channel_country: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(ch), Some(eg)) = (channel_country, report.egress.country.as_deref()) {
        if !ch.eq_ignore_ascii_case(eg) {
            out.push(format!(
                "The channel is listed for {} but this server reaches the internet from {}; a geo-restricted origin may refuse it.",
                ch.to_ascii_uppercase(),
                eg.to_ascii_uppercase()
            ));
        }
    }
    for h in &report.hosts {
        let status_of = |family: &str| {
            h.addresses
                .iter()
                .filter(|a| a.family == family)
                .map(|a| a.status)
                .next()
        };
        if let (Some(v4), Some(v6)) = (status_of("ipv4"), status_of("ipv6")) {
            if v4 != v6 {
                out.push(format!(
                    "{} answers differently over IPv4 ({}) and IPv6 ({}); one address family may be treated differently by the origin.",
                    h.host,
                    v4.map_or_else(|| "no response".to_owned(), |s| s.to_string()),
                    v6.map_or_else(|| "no response".to_owned(), |s| s.to_string()),
                ));
            }
        }
    }
    let playlist = report
        .hosts
        .iter()
        .find(|h| matches!(h.role, "master" | "media"))
        .map(|h| h.host.as_str());
    let segment = report
        .hosts
        .iter()
        .find(|h| h.role == "segment")
        .map(|h| h.host.as_str());
    if let (Some(p), Some(s)) = (playlist, segment) {
        if p != s {
            out.push(format!(
                "Segments come from {s}, not the playlist host {p}; the segment CDN may enforce its own geo or header rules."
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_drops_userinfo_query_fragment() {
        assert_eq!(
            redact_url("https://u:p@h.example:8443/a/b.m3u8?token=SECRET#f"),
            "https://h.example:8443/a/b.m3u8"
        );
        assert_eq!(redact_url("nope"), "(invalid url)");
    }

    #[test]
    fn curl_never_carries_query_or_userinfo() {
        let c = curl_for(
            "https://u:p@h/x.m3u8?sig=SECRET",
            &NetworkRequestHeaders {
                user_agent: Some("UA'1".into()),
                referrer: None,
            },
        );
        assert!(!c.contains("SECRET") && !c.contains("u:p"), "{c}");
        assert!(c.contains("-A 'UA'\\''1'"), "{c}");
    }

    #[test]
    fn trace_parses_loc_colo_ip() {
        let (ip, loc, colo) = parse_trace("fl=1\nip=2001:db8::1\ncolo=DFW\nloc=US\n");
        assert_eq!(ip.as_deref(), Some("2001:db8::1"));
        assert_eq!(loc.as_deref(), Some("US"));
        assert_eq!(colo.as_deref(), Some("DFW"));
    }

    fn host(h: &str, role: &'static str, v4: Option<u16>, v6: Option<u16>) -> NetworkHost {
        let mut addresses = Vec::new();
        if let Some(s) = v4 {
            addresses.push(NetworkAddress {
                ip: "192.0.2.1".into(),
                family: "ipv4",
                class: "public",
                connect_ms: Some(1),
                status: Some(s),
                error: None,
            });
        }
        if let Some(s) = v6 {
            addresses.push(NetworkAddress {
                ip: "2001:db8::1".into(),
                family: "ipv6",
                class: "public",
                connect_ms: Some(1),
                status: Some(s),
                error: None,
            });
        }
        NetworkHost {
            host: h.into(),
            role,
            dns: NetworkDns::default(),
            addresses,
            tls: None,
        }
    }

    #[test]
    fn assessment_rules() {
        let report = NetworkReport {
            egress: NetworkEgress {
                country: Some("DE".into()),
                ..NetworkEgress::default()
            },
            hosts: vec![
                host("playlist.example", "media", Some(200), Some(403)),
                host("cdn.example", "segment", Some(200), None),
            ],
            ..NetworkReport::default()
        };
        let a = assess(&report, Some("US"));
        assert_eq!(a.len(), 3, "{a:?}");
        assert!(a[0].contains("US") && a[0].contains("DE"));
        assert!(a[1].contains("IPv4 (200)") && a[1].contains("IPv6 (403)"));
        assert!(a[2].contains("cdn.example") && a[2].contains("playlist.example"));
        assert!(assess(&report, Some("de")).len() == 2);
    }
}
