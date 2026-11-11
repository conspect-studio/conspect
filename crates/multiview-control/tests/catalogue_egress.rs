//! PR192-01: catalogue egress (sync, logos, deep probe, `POST /sources/{id}/network`)
//! is **public-only**. Under the default configuration — where the managed-device
//! `state.dial_policy` still permits RFC1918/ULA/CGNAT — none of these paths may
//! dial a private, CGNAT, ULA or IPv4-mapped-private target.
#![cfg(feature = "catalogue")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use multiview_config::device::net_guard::{screen_ip, DialPolicy};
use multiview_control::catalogue::guard::DialGuard;
use multiview_control::catalogue::sync::Fetcher;
use serde_json::{json, Value};
use support::{body_json, get, harness_with, post_json, send, OPERATOR_TOKEN};

/// Private / shared / ULA / mapped literals that must never be dialled.
const PRIVATE_URLS: [&str; 5] = [
    "http://10.0.0.5/live.m3u8",
    "http://192.168.1.2/live.m3u8",
    "http://100.64.0.1/live.m3u8",
    "http://[fd00::1]/live.m3u8",
    "http://[::ffff:10.0.0.5]/live.m3u8",
];

/// The two policies of a default-configured `AppState` (device LAN-permissive
/// policy, catalogue public-only policy).
struct Policies {
    dial_policy: Arc<DialPolicy>,
    catalogue_dial_policy: Arc<DialPolicy>,
}

fn default_state() -> Policies {
    let seen = std::sync::Mutex::new(None);
    let seen = Arc::new(seen);
    let s2 = Arc::clone(&seen);
    let _h = harness_with(move |s| {
        *s2.lock().unwrap() = Some(Policies {
            dial_policy: Arc::clone(&s.dial_policy),
            catalogue_dial_policy: Arc::clone(&s.catalogue_dial_policy),
        });
        s
    });
    let p = seen.lock().unwrap().take().unwrap();
    p
}

#[test]
fn device_policy_is_unchanged_and_still_allows_a_lan_literal() {
    let s = default_state();
    assert_eq!(*s.dial_policy, DialPolicy::allow_lan());
    for u in PRIVATE_URLS {
        let g = DialGuard::new(Arc::clone(&s.dial_policy));
        assert!(
            g.screen_url(&url::Url::parse(u).unwrap()).is_ok(),
            "device policy must still allow {u}"
        );
    }
}

#[test]
fn catalogue_policy_is_public_only_and_separate_from_the_device_policy() {
    let s = default_state();
    assert_eq!(
        *s.catalogue_dial_policy,
        DialPolicy::deny_all_allowlistable()
    );
    assert!(!Arc::ptr_eq(&s.dial_policy, &s.catalogue_dial_policy));
    let g = DialGuard::new(Arc::clone(&s.catalogue_dial_policy));
    for u in PRIVATE_URLS
        .into_iter()
        .chain([
            "http://127.0.0.1/x",
            "http://[::1]/x",
            "http://169.254.169.254/x",
        ])
        .chain([
            "http://0.0.0.0/x",
            "http://224.0.0.1/x",
            "http://255.255.255.255/x",
        ])
    {
        assert!(g.screen_url(&url::Url::parse(u).unwrap()).is_err(), "{u}");
    }
    assert!(g
        .screen_url(&url::Url::parse("https://example.com/x").unwrap())
        .is_ok());
    // Every resolved answer is screened too: one private address poisons the set.
    let policy = &*s.catalogue_dial_policy;
    assert!(screen_ip("10.0.0.5".parse().unwrap(), policy).is_err());
    assert!(multiview_config::device::net_guard::screen_resolved(
        [
            "93.184.216.34".parse().unwrap(),
            "10.0.0.5".parse().unwrap()
        ],
        policy
    )
    .is_err());
}

#[tokio::test]
async fn sync_fetcher_refuses_private_upstream_before_connecting() {
    let s = default_state();
    for u in PRIVATE_URLS {
        let base = u.trim_end_matches("live.m3u8").to_owned();
        let f = multiview_control::catalogue::net::UpstreamFetcher::with_parts(
            multiview_control::iptv::fetch::default_fetcher(),
            Arc::clone(&s.catalogue_dial_policy),
            base,
        );
        let err = tokio::time::timeout(Duration::from_secs(2), f.fetch("streams", None))
            .await
            .expect("refused before any connection (no connect timeout)")
            .expect_err("private upstream must be refused");
        assert!(err.contains("blocked"), "{u}: {err}");
    }
}

#[tokio::test]
async fn logo_fetcher_refuses_private_urls_before_connecting() {
    let s = default_state();
    for u in PRIVATE_URLS {
        let req = multiview_control::iptv::fetch::FetchRequest {
            url: u.to_owned(),
            max_bytes: 1024,
            timeout: Duration::from_secs(20),
            etag: None,
            last_modified: None,
        };
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            multiview_control::iptv::fetch::default_fetcher()
                .fetch(req, Arc::clone(&s.catalogue_dial_policy)),
        )
        .await
        .expect("refused before any connection");
        let err = out.expect_err("must be refused");
        assert!(err.contains("blocked"), "{u}: {err}");
    }
}

fn installed_harness(dir: &std::path::Path) -> support::Harness {
    let dir = dir.to_owned();
    harness_with(move |s| {
        multiview_control::catalogue::install(
            s,
            &multiview_config::IptvOrgConfig::default(),
            Some(&dir.join("multiview.toml")),
            "test-host".to_owned(),
            false,
        )
    })
}

async fn add_source(h: &support::Harness, id: &str, url: &str) {
    let r = send(
        &h.router,
        post_json(
            &format!("/api/v1/sources/{id}"),
            OPERATOR_TOKEN,
            &json!({ "name": id, "body": { "id": id, "kind": "hls", "url": url } }),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED, "create {url}");
}

#[tokio::test]
async fn probe_and_network_refuse_private_sources_under_default_config() {
    let dir = tempfile::tempdir().unwrap();
    let h = installed_harness(dir.path());
    for (i, u) in PRIVATE_URLS.iter().enumerate() {
        let id = format!("priv-{i}");
        add_source(&h, &id, u).await;

        // Deep probe by source id.
        let r = send(
            &h.router,
            post_json(
                "/api/v1/catalogues/iptv-org/probes",
                OPERATOR_TOKEN,
                &json!({ "source_ids": [id] }),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "{u}");
        let job_id = body_json(r).await["id"].as_str().unwrap().to_owned();
        let mut verdict = Value::Null;
        for _ in 0..100 {
            let j = body_json(
                send(
                    &h.router,
                    get(
                        &format!("/api/v1/catalogues/iptv-org/probes/{job_id}"),
                        OPERATOR_TOKEN,
                    ),
                )
                .await,
            )
            .await;
            if j["state"] == "done" {
                verdict = j["results"][0]["verdict"].clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(verdict, "blocked", "probe {u}");

        // Network report: the first hop is refused, no connection attempted.
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            send(
                &h.router,
                post_json(
                    &format!("/api/v1/sources/{id}/network"),
                    OPERATOR_TOKEN,
                    &json!({}),
                ),
            ),
        )
        .await
        .expect("network report must not wait on a connect");
        assert_eq!(r.status(), StatusCode::OK, "{u}");
        let rep = body_json(r).await;
        let hop = rep["hops"][0]["body_excerpt"].as_str().unwrap_or_default();
        assert!(hop.contains("not dialled"), "{u}: {rep}");
    }
}

/// A loopback fixture answering every request with `302 -> target`; counts hits.
async fn redirector(target: String) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            h.fetch_add(1, Ordering::SeqCst);
            let t = target.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {t}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (port, hits)
}

#[tokio::test]
async fn public_fixture_redirecting_to_private_is_refused_by_probe_and_network() {
    use multiview_control::catalogue::netdiag::collect::Collector;
    use multiview_control::catalogue::probe::engine::HttpProbeRunner;
    use multiview_control::catalogue::probe::{ProbeRequest, ProbeRunner, Verdict};
    for target in PRIVATE_URLS {
        let (port, hits) = redirector(target.to_owned()).await;
        let url = format!("http://[::1]:{port}/start.m3u8");
        let req = ProbeRequest {
            url,
            user_agent: None,
            referrer: None,
            quality: None,
            channel_country: None,
        };
        let guard = DialGuard::for_tests_allowing_loopback_ports(&[port]);
        let runner = HttpProbeRunner::new(guard.clone()).unwrap();
        let r = runner.run("job".to_owned(), req.clone()).await;
        assert_eq!(r.verdict, Some(Verdict::Blocked), "probe {target}: {r:?}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the redirecting hop was dialled ({target})"
        );
        let rep = Collector::new(guard, false).collect(&req, None).await;
        assert!(
            rep.hops.iter().any(|h| h
                .body_excerpt
                .as_deref()
                .is_some_and(|b| b.contains("not dialled"))),
            "network {target}: {rep:?}"
        );
    }
}
