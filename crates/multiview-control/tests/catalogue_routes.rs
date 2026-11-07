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
    assert_eq!(st["state"], "idle");
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
    assert!(idx["generated_at"].is_string() && idx["schema"].is_u64());
    assert!(idx["categories"].is_array() && idx["languages"].is_array());
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
    for row in idx["streams"].as_array().unwrap() {
        let obj = row.as_object().unwrap();
        for f in wanted {
            assert!(obj.contains_key(f), "row misses field {f}: {row}");
        }
        assert!(obj["k"].as_str().unwrap().starts_with("s:"));
        assert!(obj["geo"]["lvl"].is_string() && obj["geo"]["places"].is_array());
    }
    // Flag vocabulary coverage from the real data.
    let all_flags: std::collections::BTreeSet<String> = idx["streams"]
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
    assert!(idx["streams"]
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
    for k in ["schema", "places"] {
        assert!(places.get(k).is_some(), "places misses {k}");
    }
    let all = places["places"].as_array().unwrap();
    let find = |id: &str| {
        all.iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("{id}"))
    };
    let miami = find("ct/USMIA");
    assert_eq!(miami["kind"], "city");
    assert!(miami["lat"].as_f64().unwrap() > 25.0 && miami["lon"].as_f64().unwrap() < -79.0);
    assert!(miami["streams"].as_u64().unwrap() >= 1);
    let us = find("c/US");
    assert_eq!(us["kind"], "country");
    assert!(us["streams_within"].as_u64().unwrap() > us["streams"].as_u64().unwrap());
    assert!(all
        .iter()
        .any(|p| p["kind"] == "region" && p["id"].as_str().unwrap().starts_with("r/")));
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
    assert_eq!(st["state"], "idle");
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
    assert_eq!(st["state"], "idle");
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
    let rows = idx["streams"].as_array().unwrap();
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
        idx["streams"]
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
                &json!({"keys":["s:x"]})
            )
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let _ = BTreeMap::<u8, u8>::new();
}

/// A runner that answers instantly with a canned verdict (route-level tests).
struct Canned;
impl probe::ProbeRunner for Canned {
    fn run(&self, id: String, req: probe::ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        Box::pin(async move {
            probe::ProbeResult {
                id,
                state: "done",
                url: req.url,
                verdict: Some(probe::Verdict::Live),
                media_checked: false,
                verdict_basis: "canned",
                stages: Vec::new(),
                facts: probe::MediaFacts::default(),
                events: Vec::new(),
                probed_from: String::new(),
                probed_at: None,
                bytes: 0,
                cached: false,
                network: None,
            }
        })
    }
}

/// `bundle()` with 80 extra distinct stream rows (probe rate-limit test).
fn many_rows() -> Value {
    let mut b = bundle();
    let first = b["streams"][0].clone();
    let extra: Vec<Value> = (0..80)
        .map(|i| {
            let mut r = first.clone();
            r["url"] = Value::String(format!("https://example.com/many/{i}.m3u8"));
            r["title"] = Value::String(format!("Many {i}"));
            r
        })
        .collect();
    b["streams"].as_array_mut().unwrap().extend(extra);
    b
}
#[tokio::test]
async fn probe_routes_enforce_roles_resolve_rows_and_poll_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let cat = catalogue_in(dir.path(), FixtureFetcher::new());
    let cat2 = Arc::clone(&cat);
    let h = harness_with(move |s| {
        s.with_catalogue(
            cat2,
            probe::ProbeService::new(Arc::new(Canned), "route-host".into()),
        )
    });
    send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/import",
            ADMIN_TOKEN,
            &many_rows(),
        ),
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
    let row = idx["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["ua"].is_string())
        .expect("a header-gated row");

    // viewer cannot start a probe; operator can.
    let by_k = json!({ "keys": [row["k"]] });
    assert_eq!(
        send(
            &h.router,
            post_json("/api/v1/catalogues/iptv-org/probes", VIEWER_TOKEN, &by_k)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let resp = send(
        &h.router,
        post_json("/api/v1/catalogues/iptv-org/probes", OPERATOR_TOKEN, &by_k),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("/probes/"));
    let job = body_json(resp).await;
    assert!(job["results"].is_array(), "ProbeJob carries results[]");
    let id = job["id"].as_str().unwrap().to_owned();

    // Poll until done (any authenticated reader).
    let mut done = Value::Null;
    for _ in 0..40 {
        let v = body_json(
            send(
                &h.router,
                get(
                    &format!("/api/v1/catalogues/iptv-org/probes/{id}"),
                    VIEWER_TOKEN,
                ),
            )
            .await,
        )
        .await;
        if v["state"] == "done" {
            done = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let res = &done["results"][0];
    assert_eq!(res["k"], row["k"]);
    assert_eq!(res["verdict"], "live");
    assert_eq!(res["probed_from"], "route-host");
    assert_eq!(
        send(
            &h.router,
            get(
                "/api/v1/catalogues/iptv-org/probes/does-not-exist",
                VIEWER_TOKEN
            )
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    // Validation: empty body, unknown key, unknown source, > 50 entries.
    let too_many: Vec<String> = (0..51).map(|i| format!("s:{i}")).collect();
    for bad in [
        json!({}),
        json!({"keys": []}),
        json!({"keys": ["s:nope"]}),
        json!({"source_ids": ["no-such-source"]}),
        json!({"keys": too_many}),
    ] {
        let st = send(
            &h.router,
            post_json("/api/v1/catalogues/iptv-org/probes", OPERATOR_TOKEN, &bad),
        )
        .await
        .status();
        assert!(
            matches!(
                st,
                StatusCode::UNPROCESSABLE_ENTITY | StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
            ),
            "{bad} -> {st}"
        );
    }

    // The 60/min per-principal limit surfaces as 429 problem+json.
    // Distinct rows (a cache hit is not charged against the budget).
    let keys: Vec<Value> = idx["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["k"].clone())
        .collect();
    assert!(keys.len() >= 70, "{} rows", keys.len());
    let mut limited = None;
    for (i, k) in keys.iter().enumerate().take(70) {
        let r = send(
            &h.router,
            post_json(
                "/api/v1/catalogues/iptv-org/probes",
                OPERATOR_TOKEN,
                &json!({"keys": [k], "n": i}),
            ),
        )
        .await;
        if r.status() == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(body_json(r).await);
            break;
        }
    }
    let p = limited.expect("rate limit reached");
    assert_eq!(p["type"], "/problems/too-many-requests");
    assert_eq!(p["status"], 429);
}
