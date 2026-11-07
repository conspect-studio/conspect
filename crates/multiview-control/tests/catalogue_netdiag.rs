//! Network-path diagnostics (docs/io/iptv-org.md §3a) against a `[::1]` fixture
//! server, with the test-only dial-guard seam allowing exactly the fixture port.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::indexing_slicing
)]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use multiview_control::catalogue::guard::DialGuard;
use multiview_control::catalogue::netdiag::collect::Collector;
use multiview_control::catalogue::probe::{self, ProbeRequest};
use multiview_control::catalogue::sync::BoxFuture;
use multiview_control::catalogue::{Catalogue, NetworkCollectorHandle};
use serde_json::{json, Value};
use support::{body_json, harness_with, post_json, send, ADMIN_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Hits = Arc<Mutex<Vec<(String, String)>>>;

struct Fixture {
    port: u16,
    hits: Hits,
    trace_hits: Arc<AtomicUsize>,
}

async fn fixture() -> Fixture {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits: Hits = Arc::default();
    let trace_hits = Arc::new(AtomicUsize::new(0));
    let (h, t) = (Arc::clone(&hits), Arc::clone(&trace_hits));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (h, t) = (Arc::clone(&h), Arc::clone(&t));
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = text.split(' ').nth(1).unwrap_or("/").to_owned();
                let auth = text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .unwrap_or("")
                    .to_owned();
                h.lock().unwrap().push((path.clone(), auth));
                let route = path.split('?').next().unwrap_or("");
                let (status, extra, body) = match route {
                    "/trace" => {
                        t.fetch_add(1, Ordering::SeqCst);
                        (
                            200,
                            String::new(),
                            "fl=1\nip=::1\ncolo=DFW\nloc=US\n".to_owned(),
                        )
                    }
                    "/master.m3u8" => (
                        200,
                        "x-cache: HIT\r\nServer: fixture\r\n".to_owned(),
                        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nmedia.m3u8?sig=INNERSECRET\n"
                            .to_owned(),
                    ),
                    "/media.m3u8" => (
                        200,
                        String::new(),
                        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nseg1.ts?k=SEGSECRET\n"
                            .to_owned(),
                    ),
                    "/seg1.ts" => (200, String::new(), "x".to_owned()),
                    "/geo.m3u8" => (
                        403,
                        String::new(),
                        "This content is not available in your region /live.m3u8?token=secret123&sig=abc 0123456789abcdef0123456789abcdef01234567".to_owned(),
                    ),
                    _ => (404, String::new(), "nope".to_owned()),
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Fixture {
        port,
        hits,
        trace_hits,
    }
}

fn collector(f: &Fixture, egress: bool) -> Collector {
    Collector::new(
        DialGuard::for_tests_allowing_loopback_ports(&[f.port]),
        egress,
    )
    .with_egress_url_for_tests(format!("http://[::1]:{}/trace", f.port))
}

fn req(url: String) -> ProbeRequest {
    ProbeRequest {
        url,
        user_agent: Some("UA/9".into()),
        referrer: Some("https://ref.example/p?secret=REFSECRET".into()),
        quality: None,
        channel_country: None,
    }
}

#[tokio::test]
async fn report_walks_the_chain_redacts_secrets_and_caches_egress() {
    let f = fixture().await;
    let c = collector(&f, true);
    let url = format!(
        "http://user:PASSWORD@[::1]:{}/master.m3u8?token=TOPSECRET",
        f.port
    );
    let report = c.collect(&req(url.clone()), Some("AU")).await;
    let text = serde_json::to_string(&report).unwrap();
    for secret in [
        "PASSWORD",
        "TOPSECRET",
        "INNERSECRET",
        "SEGSECRET",
        "REFSECRET",
        "user:",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert_eq!(report.egress.ipv6.as_deref(), Some("::1"));
    assert_eq!(report.egress.country.as_deref(), Some("US"));
    assert_eq!(report.egress.colo.as_deref(), Some("DFW"));
    let roles: Vec<&str> = report.hops.iter().map(|_| "hop").collect();
    assert_eq!(roles.len(), 3, "master, media, segment: {text}");
    assert!(report.hops.iter().all(|h| h.status == Some(200)));
    assert_eq!(
        report.hops[0]
            .cdn_headers
            .get("x-cache")
            .map(String::as_str),
        Some("HIT")
    );
    assert_eq!(report.hops[0].server.as_deref(), Some("fixture"));
    assert_eq!(report.request_headers.user_agent.as_deref(), Some("UA/9"));
    assert_eq!(report.curl.len(), 3);
    assert!(
        report.curl.iter().all(|c| c.contains("-6 --resolve")),
        "{:?}",
        report.curl
    );
    assert!(report.hosts.iter().any(|h| h
        .addresses
        .iter()
        .any(|a| a.family == "ipv6" && a.status == Some(200) && a.connect_ms.is_some())));
    assert!(
        report
            .assessment
            .iter()
            .any(|a| a.contains("AU") && a.contains("US")),
        "{:?}",
        report.assessment
    );
    // No credentials were put on the wire.
    assert!(f
        .hits
        .lock()
        .unwrap()
        .iter()
        .all(|(_, auth)| auth.is_empty()));

    // Egress is cached for 10 minutes: a second report does not ask again.
    let _ = c.collect(&req(url), None).await;
    assert_eq!(f.trace_hits.load(Ordering::SeqCst), 1, "egress cached");
}

#[tokio::test]
async fn egress_lookup_switch_off_makes_no_request_and_4xx_gets_an_excerpt() {
    let f = fixture().await;
    let c = collector(&f, false);
    let report = c
        .collect(&req(format!("http://[::1]:{}/geo.m3u8", f.port)), None)
        .await;
    assert_eq!(
        f.trace_hits.load(Ordering::SeqCst),
        0,
        "egress_lookup=false"
    );
    assert!(report.egress.ipv4.is_none() && report.egress.ipv6.is_none());
    assert_eq!(report.hops[0].status, Some(403));
    assert!(report.hops[0]
        .body_excerpt
        .as_deref()
        .unwrap()
        .contains("not available in your region"));
    let excerpt = report.hops[0].body_excerpt.as_deref().unwrap();
    for leak in ["secret123", "sig=abc", "0123456789abcdef0123456789abcdef"] {
        assert!(!excerpt.contains(leak), "{leak} leaked: {excerpt}");
    }
}

#[tokio::test]
async fn every_dial_is_screened() {
    let f = fixture().await;
    let c = collector(&f, true);
    // Another loopback port, IMDS and a private literal are all refused, not dialled.
    for url in [
        format!("http://[::1]:{}/x.m3u8", f.port.wrapping_add(1).max(2)),
        "http://169.254.169.254/latest/meta-data".to_owned(),
        "http://127.0.0.1:9/x".to_owned(),
    ] {
        let report = c.collect(&req(url.clone()), None).await;
        assert_eq!(report.hops.len(), 1, "{url}");
        assert!(report.hops[0].status.is_none(), "{url}");
        assert!(
            report.hops[0]
                .body_excerpt
                .as_deref()
                .unwrap_or("")
                .contains("not dialled"),
            "{url}: {:?}",
            report.hops[0]
        );
    }
}

struct Never;
impl probe::ProbeRunner for Never {
    fn run(&self, _: String, _: ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn source_network_route_is_operate_only_and_object_scoped() {
    let f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalogue::new(
        dir.path().join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        Arc::new(NoNet),
    );
    let handle: NetworkCollectorHandle = Arc::new(collector(&f, true));
    let h = harness_with(move |s| {
        s.with_catalogue(
            cat,
            probe::ProbeService::with_network(Arc::new(Never), "h".into(), handle),
        )
    });
    let body = json!({ "name": "net-src", "body": { "id": "net-src", "kind": "hls",
        "url": format!("http://user:PASSWORD@[::1]:{}/master.m3u8?token=TOPSECRET", f.port),
        "user_agent": "UA/9" } });
    let r = send(
        &h.router,
        post_json("/api/v1/sources/net-src", ADMIN_TOKEN, &body),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);

    let path = "/api/v1/sources/net-src/network";
    assert_eq!(
        send(&h.router, post_json(path, VIEWER_TOKEN, &json!({})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&h.router, post_json(path, SCOPED_TOKEN, &json!({})))
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "outside the caller's object scope"
    );
    assert_eq!(
        send(
            &h.router,
            post_json("/api/v1/sources/nope/network", ADMIN_TOKEN, &json!({}))
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let r = send(&h.router, post_json(path, ADMIN_TOKEN, &json!({}))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = body_json(r).await;
    let text = v.to_string();
    for secret in ["PASSWORD", "TOPSECRET", "INNERSECRET", "SEGSECRET"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    for field in [
        "egress",
        "hosts",
        "hops",
        "request_headers",
        "assessment",
        "curl",
    ] {
        assert!(v.get(field).is_some(), "missing {field}");
    }
    assert_eq!(v["hops"].as_array().unwrap().len(), 3);
}

struct NoNet;
impl multiview_control::catalogue::sync::Fetcher for NoNet {
    fn fetch<'a>(
        &'a self,
        _: &'a str,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<multiview_control::catalogue::sync::FetchOutcome, String>> {
        Box::pin(async { Err("offline".to_owned()) })
    }
}

struct StubDns;

impl multiview_control::catalogue::netdiag::dns::DnsLookup for StubDns {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
    ) -> BoxFuture<'a, multiview_control::catalogue::netdiag::NetworkDns> {
        assert_eq!(host, "localhost");
        Box::pin(async {
            multiview_control::catalogue::netdiag::NetworkDns {
                cname: vec!["edge.cdn.example".into()],
                a: vec!["192.0.2.9".into()],
                aaaa: vec![],
                ttl: Some(42),
                resolver: Some("192.0.2.53".into()),
                error: None,
            }
        })
    }
}

#[tokio::test]
async fn dns_facts_come_from_the_resolver_seam_not_the_dialled_addresses() {
    let f = fixture().await;
    let c = collector(&f, false).with_dns(Arc::new(StubDns));
    let report = c
        .collect(
            &req(format!("http://localhost:{}/master.m3u8", f.port)),
            None,
        )
        .await;
    let dns = &report.hosts[0].dns;
    assert_eq!(dns.cname, vec!["edge.cdn.example"]);
    assert_eq!(dns.a, vec!["192.0.2.9"]);
    assert_eq!(dns.ttl, Some(42));
    assert_eq!(dns.resolver.as_deref(), Some("192.0.2.53"));
    // The stub's address was never dialled: only screened, resolved ones are.
    assert!(report.hosts[0]
        .addresses
        .iter()
        .all(|a| a.ip != "192.0.2.9"));
}
