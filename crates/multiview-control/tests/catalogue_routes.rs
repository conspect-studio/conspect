//! IPTV-ORG catalogue endpoints against the trimmed REAL-data fixtures
//! (`tests/fixtures/iptv-org/*.json`): offline import, index/places/status shapes,
//! RBAC, per-object `already_added`, sync via a fixture `Fetcher` (ETag/304,
//! partial-failure-keeps-snapshot), atomic persistence + reload.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use multiview_control::catalogue::sync::{BoxFuture, FetchOutcome, Fetcher};
use multiview_control::catalogue::{probe, Catalogue};
use serde_json::{json, Value};
use support::{
    body_bytes, body_json, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN, VIEWER_TOKEN,
};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/iptv-org")
}

fn fixture(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(fixture_dir().join(format!("{name}.json"))).unwrap())
        .unwrap()
}

fn bundle() -> Value {
    let mut m = serde_json::Map::new();
    for n in [
        "streams",
        "channels",
        "feeds",
        "logos",
        "cities",
        "countries",
        "subdivisions",
        "regions",
        "blocklist",
        "categories",
        "languages",
    ] {
        m.insert(n.to_owned(), fixture(n));
    }
    Value::Object(m)
}

/// A scripted fetcher: serves the fixtures, honours If-None-Match, can be told to fail.
struct FixtureFetcher {
    etag: Mutex<String>,
    fail: Mutex<Option<&'static str>>,
    calls: Mutex<Vec<(String, Option<String>)>>,
}

impl FixtureFetcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            etag: Mutex::new("\"v1\"".into()),
            fail: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        })
    }
}

impl Fetcher for FixtureFetcher {
    fn fetch<'a>(
        &'a self,
        name: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_owned(), etag.map(str::to_owned)));
            if *self.fail.lock().unwrap() == Some(name) {
                return Err("boom".to_owned());
            }
            let current = self.etag.lock().unwrap().clone();
            if etag == Some(current.as_str()) {
                return Ok(FetchOutcome::NotModified);
            }
            let bytes = std::fs::read(fixture_dir().join(format!("{name}.json")))
                .map_err(|e| e.to_string())?;
            Ok(FetchOutcome::Body {
                bytes,
                etag: Some(current),
            })
        })
    }
}

struct Never;
impl probe::ProbeRunner for Never {
    fn run(&self, _: String, _: probe::ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        Box::pin(std::future::pending())
    }
}

fn catalogue_in(dir: &std::path::Path, fetcher: Arc<dyn Fetcher>) -> Arc<Catalogue> {
    Catalogue::new(
        dir.join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        fetcher,
    )
}

fn harness_for(cat: &Arc<Catalogue>) -> support::Harness {
    let cat = Arc::clone(cat);
    harness_with(move |s| {
        s.with_catalogue(
            cat,
            probe::ProbeService::new(Arc::new(Never), "test-host".into()),
        )
    })
}

#[tokio::test]
async fn import_then_index_places_status_have_the_design_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let cat = catalogue_in(dir.path(), FixtureFetcher::new());
    let h = harness_for(&cat);

    // Before any data: honest conflict, status says empty.
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "empty");
    assert_eq!(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN)
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );

    // Import needs administer.
    let denied = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/import",
            OPERATOR_TOKEN,
            &bundle(),
        ),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let ok = send(
        &h.router,
        post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &bundle()),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);
    let out = body_json(ok).await;
    assert_eq!(out["outcome"], "updated");

    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "ready");
    assert_eq!(st["origin"], "import");
    assert_eq!(st["counts"]["streams"], 45);
    assert!(st["counts"]["index"].as_u64().unwrap() > 40);

    // Index: exact design fields on every row.
    let resp = send(
        &h.router,
        get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let idx = body_json(resp).await;
    assert_eq!(
        idx["count"].as_u64().unwrap() as usize,
        idx["rows"].as_array().unwrap().len()
    );
    let wanted = [
        "k",
        "url",
        "title",
        "ch",
        "feed",
        "name",
        "alt",
        "net",
        "cats",
        "langs",
        "country",
        "area",
        "geo",
        "q",
        "proto",
        "flags",
        "ua",
        "ref",
        "logo",
        "closed",
        "replaced_by",
        "website",
    ];
    for row in idx["rows"].as_array().unwrap() {
        let obj = row.as_object().unwrap();
        for f in wanted {
            assert!(obj.contains_key(f), "row misses field {f}: {row}");
        }
        assert!(obj["k"].as_str().unwrap().starts_with("s:"));
        assert!(obj["geo"]["lvl"].is_string() && obj["geo"]["places"].is_array());
    }
    // Flag vocabulary coverage from the real data.
    let all_flags: std::collections::BTreeSet<String> = idx["rows"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| {
            r["flags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_owned())
        })
        .collect();
    for f in [
        "geo_blocked",
        "not_24_7",
        "needs_headers",
        "http_only",
        "closed",
        "unsupported",
        "no_metadata",
    ] {
        assert!(
            all_flags.contains(f),
            "flag {f} not exercised; got {all_flags:?}"
        );
    }
    // City-level geo with the city code.
    assert!(idx["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["geo"]["lvl"] == "city" && r["geo"]["places"][0] == "USMIA"));

    // Conditional GET.
    let mut req = get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN);
    req.headers_mut()
        .insert("if-none-match", etag.parse().unwrap());
    assert_eq!(
        send(&h.router, req).await.status(),
        StatusCode::NOT_MODIFIED
    );

    // gzip when accepted.
    let mut req = get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN);
    req.headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
    let gz = send(&h.router, req).await;
    assert_eq!(gz.headers().get("content-encoding").unwrap(), "gzip");
    let raw = body_bytes(gz).await;
    assert_eq!(&raw[..2], &[0x1f, 0x8b]);

    // Places: gazetteer shape + counts + coordinates.
    let places = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/places", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    for k in ["etag", "countries", "subdivisions", "cities", "regions"] {
        assert!(places.get(k).is_some(), "places misses {k}");
    }
    let miami = places["cities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == "USMIA")
        .expect("Miami");
    assert!(miami["lat"].as_f64().unwrap() > 25.0 && miami["lon"].as_f64().unwrap() < -79.0);
    assert!(miami["streams"].as_u64().unwrap() >= 1);
    let us = places["countries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == "US")
        .unwrap();
    assert!(us["streams_within"].as_u64().unwrap() > us["streams"].as_u64().unwrap());
    assert_ne!(places["regions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn import_rejects_malformed_bundles_and_keeps_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let cat = catalogue_in(dir.path(), FixtureFetcher::new());
    let h = harness_for(&cat);
    assert_eq!(
        send(
            &h.router,
            post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &bundle())
        )
        .await
        .status(),
        StatusCode::OK
    );
    for bad in [
        json!({"nonsense": []}),
        json!({"streams": "x"}),
        json!([]),
        json!({"streams": []}),
    ] {
        let r = send(
            &h.router,
            post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &bad),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "error");
    assert!(st["last_error"].is_string());
    assert_eq!(
        st["counts"]["streams"], 45,
        "the good snapshot is still served"
    );
    assert_eq!(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN)
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn sync_uses_if_none_match_and_a_partial_failure_keeps_the_previous_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = FixtureFetcher::new();
    let cat = catalogue_in(dir.path(), fetcher.clone());
    let h = harness_for(&cat);
    let r = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/refresh",
            ADMIN_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["outcome"], "updated");
    // Second sync: every file 304s → no-op.
    fetcher.calls.lock().unwrap().clear();
    let r = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/refresh",
            ADMIN_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(body_json(r).await["outcome"], "unchanged");
    assert!(
        fetcher
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, e)| e.as_deref() == Some("\"v1\"")),
        "If-None-Match sent for every file"
    );
    // New upstream version but one file fails mid-sync: previous snapshot stays.
    *fetcher.etag.lock().unwrap() = "\"v2\"".into();
    *fetcher.fail.lock().unwrap() = Some("feeds");
    let r = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/refresh",
            ADMIN_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "error");
    assert!(st["last_error"].as_str().unwrap().contains("feeds"));
    assert_eq!(
        st["files"]["streams"]["etag"], "\"v1\"",
        "no half-applied sync"
    );
    assert_eq!(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", VIEWER_TOKEN)
        )
        .await
        .status(),
        StatusCode::OK
    );
    // Recovery.
    *fetcher.fail.lock().unwrap() = None;
    let r = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/refresh",
            ADMIN_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(body_json(r).await["outcome"], "updated");
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "ready");
    assert_eq!(st["files"]["streams"]["etag"], "\"v2\"");
}

#[tokio::test]
async fn snapshot_persists_atomically_and_reloads_on_boot() {
    let dir = tempfile::tempdir().unwrap();
    {
        let cat = catalogue_in(dir.path(), FixtureFetcher::new());
        let h = harness_for(&cat);
        send(
            &h.router,
            post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &bundle()),
        )
        .await;
    }
    let names: Vec<String> = std::fs::read_dir(dir.path().join("cat"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"manifest.json".to_owned())
            && names.contains(&"streams.json.gz".to_owned()),
        "{names:?}"
    );
    assert!(
        names.iter().all(|n| !n.starts_with(".tmp")),
        "no temp leftovers: {names:?}"
    );
    let reloaded = catalogue_in(dir.path(), FixtureFetcher::new());
    let h = harness_for(&reloaded);
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "ready");
    assert_eq!(st["origin"], "import");
}

fn linked_source(id: &str, url: &str, channel: &str, feed: &str) -> Value {
    json!({
        "name": id,
        "body": { "id": id, "kind": "hls", "url": url,
                  "origin": { "provider": "iptv-org", "channel": channel, "feed": feed, "stream_url": url } }
    })
}

#[tokio::test]
async fn already_added_is_scoped_to_the_callers_visible_sources() {
    let dir = tempfile::tempdir().unwrap();
    let cat = catalogue_in(dir.path(), FixtureFetcher::new());
    let h = harness_for(&cat);
    send(
        &h.router,
        post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &bundle()),
    )
    .await;
    let idx = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let rows = idx["rows"].as_array().unwrap();
    let a = rows.iter().find(|r| r["ch"] == "NBC.us").unwrap();
    let b = rows.iter().find(|r| r["ch"] == "ShopLC.us").unwrap();
    for (id, row) in [("src-a", a), ("scoped-layout", b)] {
        let r = send(
            &h.router,
            post_json(
                &format!("/api/v1/sources/{id}"),
                ADMIN_TOKEN,
                &linked_source(
                    id,
                    row["url"].as_str().unwrap(),
                    row["ch"].as_str().unwrap(),
                    row["feed"].as_str().unwrap_or("SD"),
                ),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED, "{id}");
    }
    let flagged = |idx: &Value, k: &str| {
        idx["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["k"] == k)
            .unwrap()["flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "already_added")
    };
    let admin_idx = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    assert!(
        flagged(&admin_idx, a["k"].as_str().unwrap())
            && flagged(&admin_idx, b["k"].as_str().unwrap())
    );
    // The scoped principal may only see `scoped-layout`: src-a must NOT be revealed.
    let scoped_idx = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", SCOPED_TOKEN),
        )
        .await,
    )
    .await;
    assert!(
        !flagged(&scoped_idx, a["k"].as_str().unwrap()),
        "BOLA: scoped caller learned another source is linked"
    );
    assert!(flagged(&scoped_idx, b["k"].as_str().unwrap()));
}

#[tokio::test]
async fn disabled_catalogue_answers_conflict_and_probe_needs_write() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: multiview_config::IptvOrgConfig = toml::from_str("enabled = false").unwrap();
    let cat = Catalogue::new(dir.path().join("cat"), &cfg, FixtureFetcher::new());
    let h = harness_for(&cat);
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["state"], "disabled");
    assert_eq!(
        send(
            &h.router,
            post_json(
                "/api/v1/catalogues/iptv-org/refresh",
                ADMIN_TOKEN,
                &json!({})
            )
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(
            &h.router,
            post_json(
                "/api/v1/catalogues/iptv-org/probes",
                VIEWER_TOKEN,
                &json!({"url":"https://x/y.m3u8"})
            )
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let _ = BTreeMap::<u8, u8>::new();
}
