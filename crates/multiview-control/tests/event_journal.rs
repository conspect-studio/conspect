//! Event journal storage bounds, and the authorization and paging of its API.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use axum::http::StatusCode;
use multiview_control::event_journal::{
    dedupe_key, rfc3339_ns, source_dir_name, JournalCounters, JournalLimits, JournalOrigin,
    JournalQuery, JournalReader, JournalRecord, JournalStore, JournalTransport,
};
use std::{path::Path, sync::Arc};
mod support;
use support::{body_json, get, harness_with, send, ADMIN_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN};

const DAY: i64 = 86_400_000_000_000;
const T0: i64 = 1_793_000_000_000_000_000;

fn record(source: &str, origin: JournalOrigin, ns: i64) -> JournalRecord {
    JournalRecord {
        record_id: String::new(),
        source_id: source.to_owned(),
        received_at: rfc3339_ns(ns),
        received_ns: ns.to_string(),
        origin,
        name: "n".to_owned(),
        summary: "s".to_owned(),
        transport: JournalTransport::default(),
        source_pts: None,
        raw_encoding: "base64".to_owned(),
        raw: "AA==".to_owned(),
        decoded: None,
        timing: None,
        error: None,
        clock_reference: false,
        repeats: 0,
        last_seen_at: None,
    }
}

fn store(root: &Path, limits: JournalLimits) -> JournalStore {
    JournalStore::new(root.to_path_buf(), limits, Arc::default())
}

fn reader(root: &Path) -> JournalReader {
    JournalReader::new(root.to_path_buf(), Arc::default())
}

fn seed(root: &Path, source: &str, count: i64) {
    let mut s = store(root, JournalLimits::default());
    for n in 0..count {
        let origin = if n % 2 == 0 {
            JournalOrigin::Scte35
        } else {
            JournalOrigin::Hls
        };
        s.append(&mut record(source, origin, T0 + n), None, T0 + n)
            .unwrap();
    }
}

fn days(root: &Path, source: &str) -> usize {
    std::fs::read_dir(root.join(source_dir_name(source))).map_or(0, Iterator::count)
}

#[test]
fn files_rotate_per_utc_day_and_pages_cross_them_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path(), JournalLimits::default());
    for n in 0..3 {
        let at = T0 + n * DAY;
        s.append(&mut record("a", JournalOrigin::Scte35, at), None, at)
            .unwrap();
    }
    assert_eq!(days(dir.path(), "a"), 3);
    let r = reader(dir.path());
    let mut query = JournalQuery {
        limit: 2,
        ..JournalQuery::default()
    };
    let first = r.page("a", &query).unwrap();
    assert_eq!(first.records.len(), 2);
    assert_eq!(first.records[0].received_ns, (T0 + 2 * DAY).to_string());
    query.cursor.clone_from(&first.next_cursor);
    let second = r.page("a", &query).unwrap();
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].received_ns, T0.to_string());
    assert_eq!(second.next_cursor, None);
}

#[test]
fn repeats_inside_the_window_fold_into_one_record_and_expire_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path(), JournalLimits::default());
    let key = dedupe_key("a", JournalOrigin::Scte35, 0, b"section");
    assert!(!s.repeat(&key, T0));
    s.append(&mut record("a", JournalOrigin::Scte35, T0), Some(key), T0)
        .unwrap();
    assert!(s.repeat(&key, T0 + 1));
    assert!(s.repeat(&key, T0 + 2));
    s.flush(T0 + 3);
    let page = reader(dir.path())
        .page(
            "a",
            &JournalQuery {
                limit: 10,
                ..JournalQuery::default()
            },
        )
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].repeats, 2);
    assert_eq!(page.records[0].last_seen_at, Some(rfc3339_ns(T0 + 2)));
    assert!(!s.repeat(&key, T0 + 61_000_000_000));
}

#[test]
fn the_same_payload_at_a_new_position_is_a_new_event() {
    let first = dedupe_key("a", JournalOrigin::Hls, 7, b"#EXT-X-CUE-IN");
    assert_ne!(
        first,
        dedupe_key("a", JournalOrigin::Hls, 8, b"#EXT-X-CUE-IN")
    );
    assert_ne!(
        first,
        dedupe_key("b", JournalOrigin::Hls, 7, b"#EXT-X-CUE-IN")
    );
}

#[test]
fn retention_removes_expired_days_and_then_the_oldest_over_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let limits = JournalLimits {
        retention_days: 2,
        max_bytes: u64::MAX,
    };
    let mut s = store(dir.path(), limits);
    for n in 0..5 {
        let at = T0 + n * DAY;
        s.append(&mut record("a", JournalOrigin::Id3, at), None, at)
            .unwrap();
    }
    s.apply_retention(T0 + 4 * DAY);
    assert_eq!(days(dir.path(), "a"), 3);

    let dir = tempfile::tempdir().unwrap();
    let limits = JournalLimits {
        retention_days: 365,
        max_bytes: 1,
    };
    let mut s = store(dir.path(), limits);
    for n in 0..3 {
        let at = T0 + n * DAY;
        s.append(&mut record("a", JournalOrigin::Id3, at), None, at)
            .unwrap();
    }
    s.apply_retention(T0 + 2 * DAY);
    assert_eq!(days(dir.path(), "a"), 0);
    // A later write reopens rather than appending to an unlinked file.
    let at = T0 + 2 * DAY + 1;
    s.append(&mut record("a", JournalOrigin::Id3, at), None, at)
        .unwrap();
    assert_eq!(days(dir.path(), "a"), 1);
}

#[test]
fn a_torn_tail_is_skipped_and_a_failed_write_is_counted() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a", 1);
    let source = dir.path().join("a");
    let file = std::fs::read_dir(&source)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut handle = std::fs::OpenOptions::new().append(true).open(file).unwrap();
    handle.write_all(b"{\"record_id\":\"torn").unwrap();
    let page = reader(dir.path())
        .page(
            "a",
            &JournalQuery {
                limit: 10,
                ..JournalQuery::default()
            },
        )
        .unwrap();
    assert_eq!(page.records.len(), 1);

    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, b"a file where the root directory should be").unwrap();
    let counters = Arc::new(JournalCounters::default());
    let mut s = JournalStore::new(blocked, JournalLimits::default(), Arc::clone(&counters));
    assert!(s
        .append(&mut record("a", JournalOrigin::Id3, T0), None, T0)
        .is_err());
    assert_eq!(counters.snapshot().write_errors, 1);
}

#[test]
fn source_ids_cannot_escape_the_journal_root() {
    // No `/` survives and no name starts with `.`, so every id is one child.
    assert_eq!(source_dir_name("../../etc"), "%2E.%2F..%2Fetc");
    assert_eq!(source_dir_name(".."), "%2E.");
    assert_eq!(source_dir_name("cnn-usa_1.b"), "cnn-usa_1.b");
    assert_ne!(source_dir_name("a/b"), source_dir_name("a%2Fb"));
}

fn app(root: &Path) -> support::Harness {
    let journal = reader(root);
    harness_with(|s| s.with_event_journal(journal))
}

#[tokio::test]
async fn the_page_route_filters_and_pages() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "cam", 5);
    let h = app(dir.path());
    let url = "/api/v1/inputs/cam/events/journal?limit=2";
    let response = send(&h.router, get(url, VIEWER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = body_json(response).await;
    assert_eq!(body["records"].as_array().unwrap().len(), 2);
    let cursor = body["next_cursor"].as_str().unwrap();
    let next = format!("{url}&cursor={cursor}");
    let body = body_json(send(&h.router, get(&next, VIEWER_TOKEN)).await).await;
    assert_eq!(body["records"][0]["received_ns"], (T0 + 2).to_string());
    let hls = "/api/v1/inputs/cam/events/journal?origin=hls";
    let body = body_json(send(&h.router, get(hls, VIEWER_TOKEN)).await).await;
    assert_eq!(body["records"].as_array().unwrap().len(), 2);
    for bad in ["origin=x", "cursor=x", "from=yesterday"] {
        let url = format!("/api/v1/inputs/cam/events/journal?{bad}");
        let response = send(&h.router, get(&url, VIEWER_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn the_journal_is_scoped_per_object_and_hides_unknown_sources() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "cam", 1);
    let h = app(dir.path());
    for suffix in ["journal", "journal.jsonl"] {
        let url = format!("/api/v1/inputs/cam/events/{suffix}");
        let response = send(&h.router, get(&url, SCOPED_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{suffix}");
        let url = format!("/api/v1/inputs/none/events/{suffix}");
        let response = send(&h.router, get(&url, VIEWER_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{suffix}");
    }
    let unauthenticated = axum::http::Request::builder()
        .uri("/api/v1/inputs/cam/events/journal")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = send(&h.router, unauthenticated).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn downloads_stream_jsonl_and_the_export_is_admin_only() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "cam", 3);
    seed(dir.path(), "other", 2);
    let h = app(dir.path());
    let lines = |bytes: &[u8]| bytes.split(|b| *b == b'\n').count() - 1;
    let response = send(
        &h.router,
        get("/api/v1/inputs/cam/events/journal.jsonl", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/x-ndjson");
    assert!(response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("cam-events.jsonl"));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(lines(&body), 3);
    let url = "/api/v1/events/journal.jsonl";
    let response = send(&h.router, get(url, VIEWER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send(&h.router, get(url, ADMIN_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(lines(&body), 5);
}

#[tokio::test]
async fn a_build_without_a_journal_says_so() {
    let h = harness_with(|s| s);
    let response = send(
        &h.router,
        get("/api/v1/inputs/cam/events/journal", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
