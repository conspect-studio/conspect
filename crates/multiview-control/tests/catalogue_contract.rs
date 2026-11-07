//! Web-contract conformance: every field name declared by the TypeScript interfaces
//! in `web/src/api/catalogue.ts` is present in the real endpoint output (index,
//! places, status, links, probe job). The field lists below are copied from those
//! interfaces; extra backend fields are allowed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::catalogue::sync::{BoxFuture, FetchOutcome, Fetcher};
use multiview_control::catalogue::{probe, Catalogue};
use serde_json::{json, Value};
use support::{body_json, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN};

const INDEX_ENVELOPE: &[&str] = &[
    "schema",
    "generated_at",
    "categories",
    "languages",
    "streams",
];
const INDEX_STREAM: &[&str] = &[
    "k",
    "url",
    "title",
    "ch",
    "feed",
    "feed_name",
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
    "logo_url",
    "guide",
    "closed",
    "replaced_by",
    "website",
];
const INDEX_GUIDE: &[&str] = &["site", "site_id", "lang"];
const GEO: &[&str] = &["lvl", "places"];
const PLACES_ENVELOPE: &[&str] = &["schema", "places"];
const PLACE: &[&str] = &[
    "id",
    "kind",
    "name",
    "alt",
    "country",
    "subdivision",
    "lat",
    "lon",
    "flag",
    "members",
];
const STATUS: &[&str] = &["state", "etag", "synced_at", "counts", "errors"];
const STATUS_COUNTS: &[&str] = &["streams", "channels"];
const PROBE_JOB: &[&str] = &["id", "state", "results", "error"];
const PROBE_RESULT: &[&str] = &[
    "k",
    "source_id",
    "verdict",
    "probed_from",
    "probed_at",
    "detail",
    "facts",
    "events",
];
const PROBE_FACTS: &[&str] = &[
    "width",
    "height",
    "fps",
    "video_codec",
    "audio_codec",
    "bitrate_kbps",
    "advertised_kbps",
    "audio_languages",
    "captions",
    "thumbnail",
];
const LINK_INFO: &[&str] = &[
    "source_id",
    "status",
    "checked_at",
    "upstream",
    "replaced_by",
    "alternatives",
];
const LINK_TARGET: &[&str] = &["url", "user_agent", "referrer", "labels"];
const LINK_ALTERNATIVE: &[&str] = &[
    "url",
    "user_agent",
    "referrer",
    "labels",
    "k",
    "ch",
    "feed",
    "q",
    "flags",
    "verdict",
];

fn has_all(v: &Value, fields: &[&str], what: &str) {
    let o = v
        .as_object()
        .unwrap_or_else(|| panic!("{what}: not an object: {v}"));
    for f in fields {
        assert!(o.contains_key(*f), "{what}: missing field {f:?} in {v}");
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/iptv-org")
}

fn fixture_bundle() -> Value {
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
        "guides",
    ] {
        let b = std::fs::read(fixture_dir().join(format!("{n}.json"))).unwrap();
        m.insert(n.to_owned(), serde_json::from_slice(&b).unwrap());
    }
    Value::Object(m)
}

struct NoNet;
impl Fetcher for NoNet {
    fn fetch<'a>(
        &'a self,
        _: &'a str,
        _: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
        Box::pin(async { Err("offline".to_owned()) })
    }
}

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
                facts: probe::MediaFacts {
                    video_codec: Some("h264".into()),
                    width: Some(1280),
                    height: Some(720),
                    ..probe::MediaFacts::default()
                },
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

fn links_bundle(first: bool) -> Value {
    let s = |url: &str, ch: &str, ua: Option<&str>| {
        json!({ "channel": ch, "feed": "SD", "title": ch, "url": url, "quality": "720p",
                "labels": [], "user_agent": ua, "referrer": null })
    };
    let streams = if first {
        vec![
            s("https://a/1.m3u8", "A.us", None),
            s("https://a/2.m3u8", "A.us", None),
            s("https://c/1.m3u8", "C.us", None),
        ]
    } else {
        vec![
            s("https://a/2.m3u8", "A.us", None),
            s("https://a/3.m3u8", "A.us", None),
            s("https://c/1.m3u8", "C.us", Some("UA/2")),
        ]
    };
    json!({
        "streams": streams,
        "channels": [
            { "id": "A.us", "name": "A", "country": "US", "categories": ["news"] },
            { "id": "C.us", "name": "C", "country": "US", "categories": ["news"] }
        ],
        "feeds": [
            { "channel": "A.us", "id": "SD", "is_main": true, "broadcast_area": ["c/US"], "languages": ["eng"] },
            { "channel": "C.us", "id": "SD", "is_main": true, "broadcast_area": ["c/US"], "languages": ["eng"] }
        ]
    })
}

fn link(id: &str, url: &str, ch: &str) -> Value {
    json!({ "name": id, "body": { "id": id, "kind": "hls", "url": url,
        "origin": { "provider": "iptv-org", "channel": ch, "feed": "SD", "stream_url": url, "follow": "notify" } } })
}

#[tokio::test]
async fn endpoints_carry_every_field_the_web_contract_declares() {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalogue::new(
        dir.path().join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        Arc::new(NoNet),
    );
    let h = harness_with(move |s| {
        s.with_catalogue(cat, probe::ProbeService::new(Arc::new(Canned), "h".into()))
    });
    let import = |b: Value| post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &b);
    assert_eq!(
        send(&h.router, import(fixture_bundle())).await.status(),
        StatusCode::OK
    );

    // status
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    has_all(&st, STATUS, "status");
    has_all(&st["counts"], STATUS_COUNTS, "status.counts");
    assert_eq!(st["state"], "idle");
    assert!(st["errors"].is_array());

    // index
    let idx = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/index", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    has_all(&idx, INDEX_ENVELOPE, "index");
    let rows = idx["streams"].as_array().unwrap();
    assert!(!rows.is_empty());
    for r in rows {
        has_all(r, INDEX_STREAM, "index row");
        has_all(&r["geo"], GEO, "index row geo");
    }
    let guided = rows.iter().find(|r| r["guide"].is_object());
    has_all(
        &guided.expect("a row with a guide from guides.json")["guide"],
        INDEX_GUIDE,
        "guide",
    );

    // places
    let pl = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/places", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    has_all(&pl, PLACES_ENVELOPE, "places");
    let places = pl["places"].as_array().unwrap();
    assert!(!places.is_empty());
    for p in places {
        has_all(p, PLACE, "place");
    }

    // probe job (by key)
    let k = rows[0]["k"].clone();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/probes",
            OPERATOR_TOKEN,
            &json!({ "keys": [k] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let posted = body_json(resp).await;
    has_all(&posted, PROBE_JOB, "probe job (POST)");
    let id = posted["id"].as_str().unwrap().to_owned();
    let mut done = Value::Null;
    for _ in 0..80 {
        let v = body_json(
            send(
                &h.router,
                get(
                    &format!("/api/v1/catalogues/iptv-org/probes/{id}"),
                    ADMIN_TOKEN,
                ),
            )
            .await,
        )
        .await;
        has_all(&v, PROBE_JOB, "probe job (GET)");
        if v["state"] == "done" {
            done = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(done["state"], "done");
    let res = &done["results"][0];
    has_all(res, PROBE_RESULT, "probe result");
    has_all(&res["facts"], PROBE_FACTS, "probe facts");

    // links: updated (upstream) + stream_removed (alternatives)
    send(&h.router, import(links_bundle(true))).await;
    for (id, url, ch) in [
        ("rm-src", "https://a/1.m3u8", "A.us"),
        ("up-src", "https://c/1.m3u8", "C.us"),
    ] {
        let r = send(
            &h.router,
            post_json(
                &format!("/api/v1/sources/{id}"),
                ADMIN_TOKEN,
                &link(id, url, ch),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED, "{id}");
    }
    send(&h.router, import(links_bundle(false))).await;
    let links = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/links", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let list = links["links"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{links}");
    let mut saw_upstream = false;
    let mut saw_alt = false;
    for l in list {
        has_all(l, LINK_INFO, "link");
        if l["upstream"].is_object() {
            has_all(&l["upstream"], LINK_TARGET, "link.upstream");
            saw_upstream = true;
        }
        for a in l["alternatives"].as_array().unwrap() {
            has_all(a, LINK_ALTERNATIVE, "link alternative");
            saw_alt = true;
        }
    }
    assert!(saw_upstream, "an `updated` link carries upstream: {links}");
    assert!(
        saw_alt,
        "a `stream_removed` link carries alternatives: {links}"
    );

    // probe job by source id
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/catalogues/iptv-org/probes",
            OPERATOR_TOKEN,
            &json!({ "source_ids": ["up-src"] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    has_all(&body_json(resp).await, PROBE_JOB, "probe job (source_ids)");
}
