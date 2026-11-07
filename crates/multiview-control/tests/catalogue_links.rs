//! Link tracker end-to-end (slice D) through the real routes and the real write path:
//! upstream change → `catalogue.link_status` event (object-scoped) + the aggregate
//! `catalogue-link-attention` health warning; `follow = auto` applies only `updated`
//! via the normal If-Match write path, audited as the system principal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::catalogue::sync::{BoxFuture, FetchOutcome, Fetcher};
use multiview_control::catalogue::{link_runtime, probe, Catalogue};
use multiview_events::{Event, WarningCode};
use serde_json::{json, Value};
use support::{
    body_json, get, harness_with, post_json, send, ADMIN_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

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

struct Never;
impl probe::ProbeRunner for Never {
    fn run(&self, _: String, _: probe::ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        Box::pin(std::future::pending())
    }
}

fn bundle(url_a: &str, ua: Option<&str>, closed: bool) -> Value {
    json!({
        "streams": [
            { "channel": "A.us", "feed": "SD", "title": "A", "url": url_a, "quality": "720p", "labels": [], "user_agent": ua, "referrer": null },
            { "channel": "C.us", "feed": "SD", "title": "C", "url": "https://c/1.m3u8", "quality": "720p", "labels": [], "user_agent": null, "referrer": null }
        ],
        "channels": [
            { "id": "A.us", "name": "A", "country": "US", "categories": ["news"], "closed": if closed { json!("2026-01-01") } else { Value::Null }, "replaced_by": null },
            { "id": "C.us", "name": "C", "country": "US", "categories": ["news"] }
        ],
        "feeds": [
            { "channel": "A.us", "id": "SD", "is_main": true, "broadcast_area": ["c/US"], "languages": ["eng"] },
            { "channel": "C.us", "id": "SD", "is_main": true, "broadcast_area": ["c/US"], "languages": ["eng"] }
        ]
    })
}

fn link(id: &str, url: &str, ch: &str, follow: &str) -> Value {
    json!({ "name": id, "body": { "id": id, "kind": "hls", "url": url,
        "origin": { "provider": "iptv-org", "channel": ch, "feed": "SD", "stream_url": url, "follow": follow } } })
}

async fn put_source(h: &support::Harness, id: &str, body: &Value) {
    let r = send(
        &h.router,
        post_json(&format!("/api/v1/sources/{id}"), ADMIN_TOKEN, body),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{id}");
}

#[tokio::test]
async fn upstream_changes_raise_events_warning_and_auto_follow_updates_with_audit() {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalogue::new(
        dir.path().join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        Arc::new(NoNet),
    );
    let cat2 = Arc::clone(&cat);
    let audit: Arc<dyn multiview_control::AuditRepository> =
        Arc::new(multiview_control::InMemoryAuditLog::new());
    let audit2 = Arc::clone(&audit);
    let h = harness_with(move |s| {
        s.with_audit_log(audit2)
            .with_catalogue(cat2, probe::ProbeService::new(Arc::new(Never), "h".into()))
    });

    let imp = |b: Value| post_json("/api/v1/catalogues/iptv-org/import", ADMIN_TOKEN, &b);
    assert_eq!(
        send(&h.router, imp(bundle("https://a/1.m3u8", None, false)))
            .await
            .status(),
        StatusCode::OK
    );
    put_source(
        &h,
        "notify-src",
        &link("notify-src", "https://a/1.m3u8", "A.us", "notify"),
    )
    .await;
    put_source(
        &h,
        "auto-src",
        &link("auto-src", "https://a/1.m3u8", "A.us", "auto"),
    )
    .await;
    put_source(
        &h,
        "scoped-layout",
        &link("scoped-layout", "https://c/1.m3u8", "C.us", "notify"),
    )
    .await;

    let mut events = h.engine.subscribe();
    // Refresh recomputes: everything in_sync, nothing announced, no warning.
    let r = send(&h.router, imp(bundle("https://a/1.m3u8", None, false))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let links = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/links", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let statuses: Vec<(String, String)> = links["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["source_id"].as_str().unwrap().to_owned(),
                l["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(statuses.iter().all(|(_, s)| s == "in_sync"), "{statuses:?}");
    assert!(events.try_recv().is_err(), "a quiet sync announces nothing");

    // Upstream now requires a User-Agent for the same URL: `updated` for notify, auto-applied for auto.
    send(
        &h.router,
        imp(bundle("https://a/1.m3u8", Some("UA/2"), false)),
    )
    .await;
    let mut seen = Vec::new();
    let mut warning_raised = false;
    while let Ok(ev) = events.try_recv() {
        match ev.event.as_ref() {
            Event::CatalogueLinkStatus(e) => {
                seen.push((e.source_id.clone(), e.status.clone(), e.previous.clone()));
            }
            Event::HealthWarningRaised(w) if w.code == WarningCode::CatalogueLinkAttention => {
                warning_raised = true;
            }
            _ => {}
        }
    }
    assert!(
        seen.contains(&(
            "notify-src".into(),
            "updated".into(),
            Some("in_sync".into())
        )),
        "{seen:?}"
    );
    assert!(warning_raised, "aggregate health warning raised");
    let auto = body_json(send(&h.router, get("/api/v1/sources/auto-src", ADMIN_TOKEN)).await).await;
    assert_eq!(
        auto["body"]["user_agent"], "UA/2",
        "follow=auto applied the header update: {auto}"
    );
    let notify =
        body_json(send(&h.router, get("/api/v1/sources/notify-src", ADMIN_TOKEN)).await).await;
    assert!(
        notify["body"].get("user_agent").is_none() || notify["body"]["user_agent"].is_null(),
        "notify never edits"
    );
    let audit = audit.list(Some("auto-src")).unwrap();
    assert!(
        audit.iter().any(|e| e.actor == "system:catalogue"),
        "auto-follow is audited as the system principal: {audit:?}"
    );

    // Reviewing the diff for the notify source.
    let links = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/links", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let n = links["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["source_id"] == "notify-src")
        .unwrap();
    assert_eq!(n["status"], "updated");
    assert_eq!(n["upstream"]["user_agent"], "UA/2");

    // Channel A closes upstream: both sources report channel_closed (auto never switches channels).
    send(
        &h.router,
        imp(bundle("https://a/1.m3u8", Some("UA/2"), true)),
    )
    .await;
    let links = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/links", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    for id in ["notify-src", "auto-src"] {
        let l = links["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["source_id"] == id)
            .unwrap();
        assert_eq!(l["status"], "channel_closed", "{id}");
    }

    // Scoped caller sees only its own source in /links and the status tally.
    let scoped = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/links", SCOPED_TOKEN),
        )
        .await,
    )
    .await;
    let ids: Vec<&str> = scoped["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["source_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "BOLA: links are per-object scoped"
    );
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", SCOPED_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["links"], json!({"in_sync": 1}));
    let st = body_json(
        send(
            &h.router,
            get("/api/v1/catalogues/iptv-org/status", VIEWER_TOKEN),
        )
        .await,
    )
    .await;
    assert_eq!(st["links"]["channel_closed"], 2);

    // Channel reopens: auto-src is back in sync, but notify-src (never auto-edited) still
    // differs from upstream, so the warning must stay raised.
    send(
        &h.router,
        imp(bundle("https://a/1.m3u8", Some("UA/2"), false)),
    )
    .await;
    let mut cleared = false;
    while let Ok(ev) = events.try_recv() {
        if let Event::HealthWarningCleared(w) = ev.event.as_ref() {
            cleared |= w.code == WarningCode::CatalogueLinkAttention;
        }
    }
    assert!(
        !cleared,
        "warning stays while notify-src still needs attention"
    );
    // The operator resolves it (unlink by removing the source) → everything in sync → cleared.
    let got = send(&h.router, get("/api/v1/sources/notify-src", ADMIN_TOKEN)).await;
    let tag = support::etag(&got).unwrap();
    let del = send(
        &h.router,
        support::delete_if_match("/api/v1/sources/notify-src", ADMIN_TOKEN, Some(&tag)),
    )
    .await;
    assert!(del.status().is_success(), "{}", del.status());
    send(
        &h.router,
        imp(bundle("https://a/1.m3u8", Some("UA/2"), false)),
    )
    .await;
    while let Ok(ev) = events.try_recv() {
        if let Event::HealthWarningCleared(w) = ev.event.as_ref() {
            cleared |= w.code == WarningCode::CatalogueLinkAttention;
        }
    }
    assert!(cleared, "warning cleared once every link is back in sync");
    let _ = link_runtime::SYSTEM_ACTOR;
}

/// Counts the probes the runner is asked to execute.
struct Counting(Arc<std::sync::atomic::AtomicUsize>);
impl probe::ProbeRunner for Counting {
    fn run(&self, id: String, req: probe::ProbeRequest) -> BoxFuture<'_, probe::ProbeResult> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

#[tokio::test]
async fn probe_linked_round_probes_only_linked_sources_through_the_probe_service() {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalogue::new(
        dir.path().join("cat"),
        &multiview_config::IptvOrgConfig::default(),
        Arc::new(NoNet),
    );
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runs2 = Arc::clone(&runs);
    let captured: Arc<std::sync::Mutex<Option<multiview_control::AppState>>> = Arc::default();
    let cap = Arc::clone(&captured);
    let h = harness_with(move |s| {
        let s = s.with_catalogue(
            cat,
            probe::ProbeService::new(Arc::new(Counting(runs2)), "h".into()),
        );
        *cap.lock().unwrap() = Some(s.clone());
        s
    });
    put_source(
        &h,
        "linked-1",
        &link("linked-1", "https://a/1.m3u8", "A.us", "notify"),
    )
    .await;
    put_source(
        &h,
        "linked-2",
        &link("linked-2", "https://c/1.m3u8", "C.us", "notify"),
    )
    .await;
    put_source(
        &h,
        "plain",
        &json!({ "name": "plain", "body": { "id": "plain", "kind": "hls", "url": "https://p/1.m3u8" } }),
    )
    .await;
    let state = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        link_runtime_probe_round(&state),
        2,
        "only the two linked sources"
    );
    for _ in 0..100 {
        if runs.load(std::sync::atomic::Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The second round is served from the 15-minute cache: nothing new runs.
    assert_eq!(link_runtime_probe_round(&state), 2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "cache hit"
    );
}

fn link_runtime_probe_round(state: &multiview_control::AppState) -> usize {
    multiview_control::catalogue::probe_linked_round(state)
}

/// PR192-03: an operator decision committed between evaluating an auto-follow and
/// applying it must survive; the stale automatic update is dropped.
mod stale_auto_follow {
    use super::*;
    use multiview_control::catalogue::{apply_auto_decision, links, plan_auto};

    struct Rig {
        h: support::Harness,
        state: multiview_control::AppState,
        cat: Arc<Catalogue>,
        _dir: tempfile::TempDir,
    }

    async fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalogue::new(
            dir.path().join("cat"),
            &multiview_config::IptvOrgConfig::default(),
            Arc::new(NoNet),
        );
        let cat2 = Arc::clone(&cat);
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c2 = Arc::clone(&captured);
        let h = harness_with(move |s| {
            let s = s.with_catalogue(cat2, probe::ProbeService::new(Arc::new(Never), "h".into()));
            *c2.lock().unwrap() = Some(s.clone());
            s
        });
        let state = captured.lock().unwrap().take().unwrap();
        cat.import_bundle(serde_json::to_vec(&bundle("https://a/1.m3u8", None, false)).unwrap())
            .await
            .unwrap();
        put_source(
            &h,
            "auto-src",
            &link("auto-src", "https://a/1.m3u8", "A.us", "auto"),
        )
        .await;
        Rig {
            h,
            state,
            cat,
            _dir: dir,
        }
    }

    /// Upstream now wants a UA; evaluate (without applying) like the sync does.
    async fn decide(r: &Rig) -> multiview_control::catalogue::AutoDecision {
        r.cat
            .import_bundle(
                serde_json::to_vec(&bundle("https://a/1.m3u8", Some("UA/2"), false)).unwrap(),
            )
            .await
            .unwrap();
        let src = multiview_control::catalogue::accepted_source_docs(&r.state)
            .into_iter()
            .find(|s| s.id == "auto-src")
            .unwrap();
        let info = links::evaluate(&src, &r.cat.snapshot().unwrap()).unwrap();
        let mut plan = plan_auto(&r.state, &[info]);
        assert_eq!(plan.len(), 1, "follow=auto + updated is planned");
        plan.remove(0)
    }

    async fn operator_put(r: &Rig, body: &Value) {
        let g = send(&r.h.router, get("/api/v1/sources/auto-src", ADMIN_TOKEN)).await;
        let tag = support::etag(&g).unwrap();
        let resp = send(
            &r.h.router,
            support::put_json("/api/v1/sources/auto-src", ADMIN_TOKEN, Some(&tag), body),
        )
        .await;
        assert!(resp.status().is_success(), "{}", resp.status());
    }

    async fn current(r: &Rig) -> Value {
        body_json(send(&r.h.router, get("/api/v1/sources/auto-src", ADMIN_TOKEN)).await).await
    }

    #[tokio::test]
    async fn unmodified_source_still_gets_the_auto_update() {
        let r = rig().await;
        let d = decide(&r).await;
        let applied = apply_auto_decision(&r.state, &r.cat, &d, "system:catalogue")
            .await
            .unwrap();
        assert!(applied);
        assert_eq!(current(&r).await["body"]["user_agent"], "UA/2");
    }

    #[tokio::test]
    async fn operator_switching_follow_off_between_evaluation_and_apply_wins() {
        let r = rig().await;
        let d = decide(&r).await;
        operator_put(&r, &link("auto-src", "https://a/1.m3u8", "A.us", "manual")).await;
        let before = current(&r).await;
        let applied = apply_auto_decision(&r.state, &r.cat, &d, "system:catalogue")
            .await
            .unwrap();
        assert!(!applied, "stale auto decision must be dropped");
        let after = current(&r).await;
        assert_eq!(after, before, "no auto update was written");
        assert_eq!(after["body"]["origin"]["follow"], "manual");
        assert!(after["body"].get("user_agent").is_none() || after["body"]["user_agent"].is_null());
    }

    #[tokio::test]
    async fn operator_url_edit_between_evaluation_and_apply_wins() {
        let r = rig().await;
        let d = decide(&r).await;
        // Same follow=auto, but the operator pointed the stream elsewhere.
        let mut edited = link("auto-src", "https://a/1.m3u8", "A.us", "auto");
        edited["body"]["url"] = json!("https://operator.example/own.m3u8");
        operator_put(&r, &edited).await;
        let before = current(&r).await;
        let applied = apply_auto_decision(&r.state, &r.cat, &d, "system:catalogue")
            .await
            .unwrap();
        assert!(!applied);
        let after = current(&r).await;
        assert_eq!(after, before);
        assert_eq!(after["body"]["url"], "https://operator.example/own.m3u8");
    }
}
