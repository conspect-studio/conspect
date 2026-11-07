//! The network half of the §3a diagnostics (feature `catalogue`): the egress
//! identity, the host chain and a per-address, per-family fetch of every host.
//!
//! Every dial — each resolved address of each host, the egress lookup and every
//! redirect hop — is screened by `net_guard` (through [`DialGuard`]) before a
//! connection is made, and the vetted address is the one dialled (the client is
//! pinned with `resolve`, so DNS cannot change between screen and dial). No
//! credentials are sent: userinfo is stripped from every URL before dialling, and
//! userinfo and query strings never appear in the report, a curl line or an error.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_config::device::net_guard::{self, BlockClass};

use super::{
    assess, curl_for_addr, host_of, parse_trace, redact_url, NetworkAddress, NetworkCollector,
    NetworkDns, NetworkEgress, NetworkHop, NetworkHost, NetworkReport, NetworkRequestHeaders,
    NetworkTls,
};
use crate::catalogue::guard::DialGuard;
use crate::catalogue::probe::{parse_media, pick_variant, resolve_uri, ProbeRequest};
use crate::catalogue::sync::BoxFuture;

/// The fixed egress-identity host (docs/io/iptv-org.md §3a).
pub const EGRESS_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";
/// How long an egress answer is reused.
pub const EGRESS_TTL: Duration = Duration::from_secs(600);
const TOTAL_BUDGET: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_HOPS: usize = 12;
const MAX_ADDRS_PER_FAMILY: usize = 2;
const PLAYLIST_CAP: usize = 512 * 1024;
const TRACE_CAP: usize = 4096;
const EXCERPT_BYTES: usize = 512;

/// The diagnostics collector.
pub struct Collector {
    guard: DialGuard,
    egress_lookup: bool,
    egress_url: String,
    dns: Arc<dyn super::dns::DnsLookup>,
    cache: Mutex<Option<(Instant, NetworkEgress)>>,
}

impl std::fmt::Debug for Collector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collector").finish_non_exhaustive()
    }
}

/// Everything learned about one host in one hop.
struct HostProbe {
    host: NetworkHost,
    hop: NetworkHop,
    /// Address the primary response came from.
    used: Option<IpAddr>,
    location: Option<String>,
    body: Vec<u8>,
}

#[derive(Default)]
struct Partial {
    hosts: Vec<NetworkHost>,
    hops: Vec<NetworkHop>,
    curl: Vec<String>,
}

fn class_of(ip: IpAddr) -> &'static str {
    match net_guard::classify(ip) {
        None => "public",
        Some(BlockClass::Private | BlockClass::UniqueLocal | BlockClass::Carrier) => "private",
        Some(_) => "blocked",
    }
}

const fn family_of(ip: IpAddr) -> &'static str {
    if ip.is_ipv6() {
        "ipv6"
    } else {
        "ipv4"
    }
}

fn clean(v: &str, max: usize) -> String {
    v.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| clean(v, 256))
}

fn cdn_headers(resp: &reqwest::Response) -> std::collections::BTreeMap<String, String> {
    resp.headers()
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            matches!(
                k,
                "cf-ray"
                    | "x-cache"
                    | "x-served-by"
                    | "x-amz-cf-pop"
                    | "x-amz-cf-id"
                    | "x-cdn"
                    | "cf-cache-status"
                    | "cf-ipcountry"
            ) || k.starts_with("x-akamai-")
                || k.contains("geo")
                || k.contains("country")
                || k.contains("region")
        })
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), clean(v.to_str().ok()?, 256))))
        .take(16)
        .collect()
}

fn tls_facts(resp: &reqwest::Response) -> Option<NetworkTls> {
    let info = resp.extensions().get::<reqwest::tls::TlsInfo>()?;
    let der = info.peer_certificate()?;
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    Some(NetworkTls {
        version: None,
        alpn: match resp.version() {
            reqwest::Version::HTTP_2 => Some("h2".to_owned()),
            reqwest::Version::HTTP_11 => Some("http/1.1".to_owned()),
            _ => None,
        },
        subject: Some(clean(&cert.subject().to_string(), 256)),
        issuer: Some(clean(&cert.issuer().to_string(), 256)),
        not_after: Some(crate::catalogue::store::unix_rfc3339(
            cert.validity().not_after.timestamp(),
        )),
    })
}

fn error_text(e: &reqwest::Error) -> String {
    let chain = std::iter::successors(std::error::Error::source(e), |s| s.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    let text = if chain.is_empty() {
        e.to_string()
    } else {
        format!("{e}: {chain}")
    };
    // Belt and braces: no URL (and so no userinfo or query) survives in an error.
    text.split_whitespace()
        .map(|w| {
            if w.contains("://") {
                redact_url(w)
            } else {
                w.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn read_capped(resp: &mut reqwest::Response, cap: usize) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < cap {
        match resp.chunk().await {
            Ok(Some(c)) => {
                let room = cap - body.len();
                body.extend_from_slice(c.get(..room.min(c.len())).unwrap_or(&[]));
            }
            _ => break,
        }
    }
    body
}

impl Collector {
    /// A collector screening every dial with `guard`. `egress_lookup = false`
    /// (`[network_diagnostics] egress_lookup`) makes no request to the egress host.
    #[must_use]
    pub fn new(guard: DialGuard, egress_lookup: bool) -> Self {
        Self {
            guard,
            egress_lookup,
            egress_url: EGRESS_URL.to_owned(),
            dns: Arc::new(super::dns::SystemDns::new()),
            cache: Mutex::new(None),
        }
    }

    /// Replace the DNS lookup (tests use a stub; no real DNS in tests).
    #[must_use]
    pub fn with_dns(mut self, dns: Arc<dyn super::dns::DnsLookup>) -> Self {
        self.dns = dns;
        self
    }

    /// TEST SEAM: point the egress lookup at a fixture server.
    #[cfg(feature = "_test-seams")]
    #[must_use]
    pub fn with_egress_url_for_tests(mut self, url: String) -> Self {
        self.egress_url = url;
        self
    }

    fn client(
        &self,
        host: &str,
        pin: Option<SocketAddr>,
        local: Option<IpAddr>,
    ) -> Result<reqwest::Client, String> {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .tls_info(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .dns_resolver(self.guard.resolver());
        if let Some(addr) = pin {
            if host.parse::<IpAddr>().is_err() {
                b = b.resolve(host, addr);
            }
        }
        if let Some(l) = local {
            b = b.local_address(l);
        }
        b.build().map_err(|e| error_text(&e))
    }

    /// One family of the egress lookup.
    async fn trace(
        &self,
        url: &url::Url,
        v6: bool,
    ) -> Option<(IpAddr, Option<String>, Option<String>)> {
        let local = if v6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        };
        let client = self.client(url.host_str()?, None, Some(local)).ok()?;
        let mut resp = client.get(url.as_str()).send().await.ok()?;
        if resp.status().as_u16() != 200 {
            return None;
        }
        let body = read_capped(&mut resp, TRACE_CAP).await;
        let (ip, loc, colo) = parse_trace(&String::from_utf8_lossy(&body));
        let ip: IpAddr = ip?.parse().ok()?;
        // The answer is untrusted: it must belong to the family we asked over.
        (ip.is_ipv6() == v6).then_some((ip, loc, colo))
    }

    /// How the internet sees this server (cached 10 min; empty when disabled).
    async fn egress(&self) -> NetworkEgress {
        if !self.egress_lookup {
            return NetworkEgress::default();
        }
        if let Some((at, e)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            if at.elapsed() < EGRESS_TTL {
                return e;
            }
        }
        let Ok(url) = url::Url::parse(&self.egress_url) else {
            return NetworkEgress::default();
        };
        if self.guard.screen_url(&url).is_err() {
            return NetworkEgress::default();
        }
        let (v4, v6) = tokio::join!(self.trace(&url, false), self.trace(&url, true));
        let mut e = NetworkEgress::default();
        for (ip, loc, colo) in [v6, v4].into_iter().flatten() {
            if ip.is_ipv6() {
                e.ipv6 = Some(ip.to_string());
            } else {
                e.ipv4 = Some(ip.to_string());
            }
            e.country = e.country.take().or(loc);
            e.colo = e.colo.take().or(colo);
        }
        if e.ipv4.is_some() || e.ipv6.is_some() {
            *self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((Instant::now(), e.clone()));
        }
        e
    }

    /// Resolve, screen and fetch one URL over every vetted address.
    #[allow(clippy::too_many_lines)] // one linear per-address walk; splitting hides the order
    async fn probe_host(
        &self,
        url: &url::Url,
        role: &'static str,
        req: &ProbeRequest,
        read_body: bool,
    ) -> HostProbe {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(0);
        let started = Instant::now();
        let mut addresses: Vec<NetworkAddress> = Vec::new();
        let mut dns = NetworkDns::default();
        let mut used = None;
        let mut hop = NetworkHop {
            url_redacted: redact_url(url.as_str()),
            status: None,
            ms: None,
            server: None,
            via: None,
            age: None,
            cdn_headers: std::collections::BTreeMap::new(),
            body_excerpt: None,
        };
        let mut tls = None;
        let mut tls_error: Option<String> = None;
        let mut location = None;
        let mut body = Vec::new();

        let ips: Vec<IpAddr> = match url.host() {
            Some(url::Host::Ipv4(v)) => vec![IpAddr::V4(v)],
            Some(url::Host::Ipv6(v)) => vec![IpAddr::V6(v)],
            Some(url::Host::Domain(d)) => match tokio::net::lookup_host((d, port)).await {
                Ok(it) => {
                    let mut seen = Vec::new();
                    for a in it {
                        if !seen.contains(&a.ip()) {
                            seen.push(a.ip());
                        }
                    }
                    seen
                }
                Err(e) => {
                    hop.body_excerpt = None;
                    addresses.push(NetworkAddress {
                        ip: String::new(),
                        family: "ipv4",
                        class: "public",
                        connect_ms: None,
                        status: None,
                        error: Some(format!("dns: {}", clean(&e.to_string(), 120))),
                    });
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // Drop the placeholder row of a failed lookup (no address to show).
        addresses.retain(|a| !a.ip.is_empty());
        if let Some(url::Host::Domain(d)) = url.host() {
            // Real CNAME/A/AAAA/TTL facts; a failure is reported, never invented.
            dns = self.dns.lookup(d).await;
        } else {
            for ip in &ips {
                if ip.is_ipv6() {
                    dns.aaaa.push(ip.to_string());
                } else {
                    dns.a.push(ip.to_string());
                }
            }
        }
        let mut picked: Vec<IpAddr> = Vec::new();
        for v6 in [true, false] {
            picked.extend(
                ips.iter()
                    .copied()
                    .filter(|ip| ip.is_ipv6() == v6)
                    .take(MAX_ADDRS_PER_FAMILY),
            );
        }
        for ip in picked {
            let mut row = NetworkAddress {
                ip: ip.to_string(),
                family: family_of(ip),
                class: class_of(ip),
                connect_ms: None,
                status: None,
                error: None,
            };
            if let Err(why) = self.guard.screen_addr(ip, port) {
                row.error = Some(format!("not dialled: {why}"));
                addresses.push(row);
                continue;
            }
            let sock = SocketAddr::new(ip, port);
            let t = Instant::now();
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(sock)).await
            {
                Ok(Ok(_)) => {
                    row.connect_ms =
                        Some(u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
                }
                Ok(Err(e)) => {
                    row.error = Some(format!("connect: {}", clean(&e.to_string(), 120)));
                    addresses.push(row);
                    continue;
                }
                Err(_) => {
                    row.error = Some("connect: timed out".to_owned());
                    addresses.push(row);
                    continue;
                }
            }
            let sent = Instant::now();
            let client = match self.client(&host, Some(sock), None) {
                Ok(c) => c,
                Err(e) => {
                    row.error = Some(e);
                    addresses.push(row);
                    continue;
                }
            };
            let mut rb = client
                .get(url.as_str())
                .header("Accept-Encoding", "identity");
            if let Some(ua) = &req.user_agent {
                rb = rb.header(reqwest::header::USER_AGENT, ua);
            }
            if let Some(r) = &req.referrer {
                rb = rb.header(reqwest::header::REFERER, r);
            }
            match rb.send().await {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    row.status = Some(status);
                    if used.is_none() {
                        used = Some(ip);
                        hop.status = Some(status);
                        hop.ms =
                            Some(u64::try_from(sent.elapsed().as_millis()).unwrap_or(u64::MAX));
                        hop.server = header(&resp, "server");
                        hop.via = header(&resp, "via");
                        hop.age = header(&resp, "age");
                        hop.cdn_headers = cdn_headers(&resp);
                        tls = tls_facts(&resp);
                        if url.scheme() == "https" {
                            // The address is already screened above; the handshake
                            // only reads version/ALPN/certificate facts.
                            match super::tls::handshake(sock, &host).await {
                                Ok(h) => {
                                    tls = Some(NetworkTls {
                                        version: h.version,
                                        alpn: h
                                            .alpn
                                            .or_else(|| tls.as_ref().and_then(|t| t.alpn.clone())),
                                        subject: h.subject.or_else(|| {
                                            tls.as_ref().and_then(|t| t.subject.clone())
                                        }),
                                        issuer: h.issuer.or_else(|| {
                                            tls.as_ref().and_then(|t| t.issuer.clone())
                                        }),
                                        not_after: h.not_after.or_else(|| {
                                            tls.as_ref().and_then(|t| t.not_after.clone())
                                        }),
                                    });
                                }
                                Err(e) => tls_error = Some(e),
                            }
                        }
                        location = header(&resp, "location");
                        if (400..500).contains(&status) {
                            let b = read_capped(&mut resp, EXCERPT_BYTES).await;
                            let text = clean(&String::from_utf8_lossy(&b), EXCERPT_BYTES);
                            hop.body_excerpt = (!text.trim().is_empty()).then_some(text);
                        } else if read_body && status == 200 {
                            body = read_capped(&mut resp, PLAYLIST_CAP).await;
                        }
                    }
                }
                Err(e) => row.error = Some(error_text(&e)),
            }
            addresses.push(row);
        }
        let _ = started;
        if let (Some(e), None) = (tls_error, tls.as_ref()) {
            // No facts at all: say why instead of showing an empty block.
            tls = Some(NetworkTls {
                subject: Some(format!("unavailable ({e})")),
                ..NetworkTls::default()
            });
        }
        HostProbe {
            host: NetworkHost {
                host,
                role,
                dns,
                addresses,
                tls: tls.filter(|_| url.scheme() == "https"),
            },
            hop,
            used,
            location,
            body,
        }
    }

    async fn chain(&self, req: &ProbeRequest, acc: &mut Partial, headers: &NetworkRequestHeaders) {
        let mut next: Option<(String, &'static str)> = Some((req.url.clone(), "master"));
        let mut redirects = 0_usize;
        while let Some((raw, role)) = next.take() {
            if acc.hops.len() >= MAX_HOPS {
                break;
            }
            let Ok(mut url) = url::Url::parse(&raw) else {
                break;
            };
            // No credentials are ever sent or shown.
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_fragment(None);
            if let Err(why) = self.guard.screen_url(&url) {
                acc.hops.push(NetworkHop {
                    url_redacted: redact_url(url.as_str()),
                    status: None,
                    ms: None,
                    server: None,
                    via: None,
                    age: None,
                    cdn_headers: std::collections::BTreeMap::new(),
                    body_excerpt: Some(format!("not dialled: {}", clean(&why, 160))),
                });
                acc.curl.push(curl_for_addr(url.as_str(), headers, None));
                break;
            }
            let playlist_role = matches!(role, "master" | "media" | "redirect");
            let hp = self.probe_host(&url, role, req, playlist_role).await;
            acc.curl.push(curl_for_addr(
                url.as_str(),
                headers,
                hp.used
                    .map(|ip| (ip, url.port_or_known_default().unwrap_or(0))),
            ));
            if !acc.hosts.iter().any(|h| h.host == hp.host.host) {
                acc.hosts.push(hp.host);
            }
            let status = hp.hop.status;
            acc.hops.push(hp.hop);
            if matches!(status, Some(300..=399)) {
                redirects += 1;
                if redirects > 5 {
                    break;
                }
                next = hp
                    .location
                    .and_then(|l| resolve_uri(url.as_str(), &l))
                    .map(|u| (u, "redirect"));
                continue;
            }
            if status != Some(200) || hp.body.is_empty() {
                break;
            }
            let text = String::from_utf8_lossy(&hp.body).into_owned();
            if !text.trim_start().starts_with("#EXTM3U") {
                break;
            }
            if let Some((uri, _, _)) = pick_variant(&text, req.quality.as_deref()) {
                next = resolve_uri(url.as_str(), &uri).map(|u| (u, "media"));
            } else if let Some(info) = parse_media(&text) {
                let target = info.init.clone().or_else(|| info.segments.first().cloned());
                next = target
                    .and_then(|s| resolve_uri(url.as_str(), &s))
                    .map(|u| (u, "segment"));
            }
        }
    }

    /// Collect the report for `req`. `channel_country` (ISO alpha-2) feeds the
    /// geo hint of the assessment. Bounded by a 15 s budget; a timeout keeps what
    /// was learned so far.
    pub async fn collect(
        &self,
        req: &ProbeRequest,
        channel_country: Option<&str>,
    ) -> NetworkReport {
        let headers = NetworkRequestHeaders {
            user_agent: req.user_agent.clone(),
            referrer: req.referrer.as_deref().map(redact_url),
        };
        let mut acc = Partial::default();
        let (egress, timed_out) = {
            let work = async {
                let e = self.egress().await;
                self.chain(req, &mut acc, &headers).await;
                e
            };
            match tokio::time::timeout(TOTAL_BUDGET, work).await {
                Ok(e) => (e, false),
                Err(_) => (NetworkEgress::default(), true),
            }
        };
        let mut report = NetworkReport {
            egress,
            hosts: acc.hosts,
            hops: acc.hops,
            request_headers: headers,
            assessment: Vec::new(),
            curl: acc.curl,
        };
        report.assessment = assess(&report, channel_country);
        if timed_out {
            report
                .assessment
                .push("Diagnostics hit their time budget; the report is partial.".to_owned());
        }
        // The first host really dialled, for an operator who needs the family.
        let _ = host_of;
        report
    }
}

impl NetworkCollector for Collector {
    fn collect(
        &self,
        req: ProbeRequest,
        channel_country: Option<String>,
    ) -> BoxFuture<'_, NetworkReport> {
        Box::pin(async move { Collector::collect(self, &req, channel_country.as_deref()).await })
    }
}
