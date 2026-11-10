//! Real IPv6 HTTP -> actual durable Service -> immutable artifact handoff.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use multiview_control::{
    command_bus,
    web_capture::Provider,
    web_capture_store::{DurableTerms, Ledger},
    AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::Tools,
    service::{Config, Service},
};
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
    token: &str,
) -> String {
    request_keyed(addr, method, path, body, token, "get-key").await
}
async fn request_keyed(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
    token: &str,
    key: &str,
) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let text=format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut output = Vec::new();
    stream.read_to_end(&mut output).await.unwrap();
    String::from_utf8(output).unwrap()
}
fn json(response: &str) -> serde_json::Value {
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}
#[allow(clippy::too_many_lines)] // One server lifetime covers the complete authorized transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_tcp_post_runs_service_and_returns_owner_scoped_artifact() {
    let root = tempfile::tempdir().unwrap();
    let producer = root.path().join("node");
    // This test exercises HTTP/service/artifact integration, not the Chrome codec.
    // Real Chrome/FFmpeg capture is proved by the runtime's required browser tests.
    std::fs::write(&producer,"#!/bin/sh\ncat >/dev/null\nprintf 'actual-producer-bytes'\nprintf '%s\\n' '{\"event\":\"capture_complete\",\"frames\":2}' >&2\n").unwrap();
    std::fs::set_permissions(&producer, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store_path = root.path().join("store");
    std::fs::create_dir(&store_path).unwrap();
    let store = ArtifactStore::open(
        &store_path,
        Limits {
            bytes: 128 * 1024 * 1024,
            count: 20,
        },
    )
    .unwrap();
    let service = Arc::new(
        Service::start_durable(
            Tools {
                node: producer,
                browser: "/bin/sh".into(),
                ffmpeg: "/bin/sh".into(),
            },
            store,
            Config {
                queue_capacity: 2,
                history: 8,
                max_fills: 4,
            },
            &root.path().join("journal"),
        )
        .unwrap(),
    );
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let (tx, _rx) = command_bus(8);
    let state = AppState::new(
        publisher,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_web_capture(Provider::configured(
        service.clone(),
        Arc::new(DurableTerms::open(&root.path().join("terms.log")).unwrap()),
        Arc::new(Ledger::durable(&root.path().join("idem.log")).unwrap()),
    ));
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, done) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(multiview_control::serve(listener, state, async move {
        let _ = done.await;
    }));
    let body=serde_json::json!({"fill_id":"radar","view":{"lat":-33.946,"lon":150.37,"zoom":10,"overlay":"radar","width":1280,"height":720,"fps":2,"frames":2,"public":true},"terms":{"accepted":true,"version":"windy-2023-09-01"}}).to_string();
    let response = request(
        address,
        "POST",
        "/api/v1/media/captures",
        &body,
        support::OPERATOR_TOKEN,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.0 202") || response.starts_with("HTTP/1.1 202"),
        "{response}"
    );
    let id = json(&response)["job_id"].as_u64().unwrap();
    // Same key + body replays the same job; divergent body is 409.
    let replay = request_keyed(
        address,
        "POST",
        "/api/v1/media/captures",
        &body,
        support::OPERATOR_TOKEN,
        "get-key",
    )
    .await;
    assert_eq!(json(&replay)["job_id"].as_u64().unwrap(), id, "{replay}");
    let divergent = body.replace("\"frames\":2", "\"frames\":3");
    let r = request_keyed(
        address,
        "POST",
        "/api/v1/media/captures",
        &divergent,
        support::OPERATOR_TOKEN,
        "get-key",
    )
    .await;
    assert!(r.contains(" 409"), "{r}");
    // Missing key rejected.
    let no_key = request_keyed(
        address,
        "POST",
        "/api/v1/media/captures",
        &body,
        support::OPERATOR_TOKEN,
        "",
    )
    .await;
    assert!(no_key.contains(" 422"), "{no_key}");
    // Unknown target player rejected before allocation.
    let with_player = body.replace("\"view\"", "\"target_player\":\"no-such-player\",\"view\"");
    let r = request_keyed(
        address,
        "POST",
        "/api/v1/media/captures",
        &with_player,
        support::OPERATOR_TOKEN,
        "k-player",
    )
    .await;
    assert!(r.contains(" 404"), "{r}");
    // Terms acceptance durably recorded.
    let log = std::fs::read_to_string(root.path().join("terms.log")).unwrap();
    assert!(log.contains("operator-key") && log.contains("windy-2023-09-01"));
    assert_eq!(log.lines().count(), 1, "replay must not re-record terms");
    let path = format!("/api/v1/media/captures/{id}");
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = request(address, "GET", &path, "", support::OPERATOR_TOKEN).await;
            let value = json(&response);
            if value["state"] == "succeeded" {
                break value;
            }
            assert_ne!(
                value["state"],
                "failed",
                "{value}; {:?}",
                service.status(
                    "operator-key",
                    multiview_webengine::service::JobId::from_raw(id)
                )
            );
            assert_ne!(value["state"], "uncertain", "{value}");
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(status["artifact_id"].as_str().unwrap().starts_with('a'));
    let denied = request(address, "GET", &path, "", support::VIEWER_TOKEN).await;
    assert!(denied.contains("404"));
    // A different key with the same Operator role must not see the job or artifact.
    let other = request(address, "GET", &path, "", support::ADMIN_TOKEN).await;
    assert!(other.contains(" 404"), "{other}");
    let other = request(
        address,
        "GET",
        &format!("{path}/artifact"),
        "",
        support::ADMIN_TOKEN,
    )
    .await;
    assert!(other.contains(" 404"), "{other}");
    // Cancelling a finished job is 409; unknown job 404.
    let r = request_keyed(
        address,
        "POST",
        &format!("{path}/cancel"),
        "",
        support::OPERATOR_TOKEN,
        "c1",
    )
    .await;
    assert!(r.contains(" 409"), "{r}");
    let r = request_keyed(
        address,
        "POST",
        "/api/v1/media/captures/999999/cancel",
        "",
        support::OPERATOR_TOKEN,
        "c2",
    )
    .await;
    assert!(r.contains(" 404"), "{r}");
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    service.shutdown();
}
