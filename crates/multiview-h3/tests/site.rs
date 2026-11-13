//! Page requests over real QUIC on `[::1]`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use multiview_h3::{Counters, H3Limits};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_returns_site_response_with_clean_headers() {
    let site = TestSite::new();
    let server = start(site.clone(), None, H3Limits::default(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, body) = fetch(&mut client.send, request(http::Method::GET, "/index.html?x=1")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body, b"hello /index.html");
    assert_eq!(resp.headers()["x-site"], "yes");
    for name in multiview_h3::server::CONNECTION_SPECIFIC {
        assert!(!resp.headers().contains_key(name), "{name} not stripped");
    }
    let seen = site.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.method, http::Method::GET);
    assert_eq!(seen.uri.path_and_query().unwrap().as_str(), "/index.html?x=1");
    assert_eq!(seen.version, http::Version::HTTP_3);
    assert_eq!(seen.headers["host"], "localhost");
    assert!(seen.headers.keys().all(|k| !k.as_str().starts_with(':')));
    assert_ne!(seen.peer.port(), 0);
    assert!(seen.peer.ip().is_loopback());
    assert_eq!(Counters::get(&server.endpoint.counters().requests), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_body_streams_through() {
    let site = TestSite::new();
    let server = start(site, None, H3Limits::default(), None).await;
    let mut client = h3_client(&server).await;
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let fut = async {
        let mut stream = client
            .send
            .send_request(request(http::Method::POST, "/echo"))
            .await
            .unwrap();
        for chunk in payload.chunks(16 * 1024) {
            stream.send_data(Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        stream.finish().await.unwrap();
        let resp = stream.recv_response().await.unwrap();
        (resp, read_body(&mut stream).await)
    };
    let (resp, body) = tokio::time::timeout(TIMEOUT, fut).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(body.len(), payload.len());
    assert!(body == payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_beyond_the_cap_are_rejected_not_queued() {
    let site = TestSite::new();
    let limits = H3Limits {
        max_concurrent_requests: 2,
        ..H3Limits::default()
    };
    let server = start(site.clone(), None, limits, None).await;
    let mut client = h3_client(&server).await;
    // Two requests block inside the site.
    let mut blocked = Vec::new();
    for _ in 0..2 {
        let mut send = client.send.clone();
        blocked.push(tokio::spawn(async move {
            fetch(&mut send, request(http::Method::GET, "/block")).await
        }));
    }
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while site.blocked.load(Ordering::SeqCst) < 2 {
        assert!(tokio::time::Instant::now() < deadline, "requests never reached the site");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The third is reset promptly with H3_REQUEST_REJECTED.
    let third = async {
        let mut stream = client
            .send
            .send_request(request(http::Method::GET, "/third"))
            .await
            .unwrap();
        let _ = stream.finish().await;
        stream.recv_response().await
    };
    let err = tokio::time::timeout(TIMEOUT, third)
        .await
        .expect("the third request was queued instead of rejected")
        .expect_err("the third request was served");
    assert!(format!("{err:?}").contains("REQUEST_REJECTED"), "{err:?}");
    assert_eq!(
        Counters::get(&server.endpoint.counters().requests_rejected),
        1
    );
    // Release the blocked ones; both complete, and the connection still works.
    site.gate.add_permits(2);
    for task in blocked {
        let (resp, _) = task.await.unwrap();
        assert_eq!(resp.status(), 200);
    }
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_routing_without_handler_and_plain_connect() {
    let site = TestSite::new();
    let server = start(site, None, H3Limits::default(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, connect_request("/moq/x", None)).await;
    assert_eq!(resp.status(), 501, "extended CONNECT without a handler");
    let (resp, _) = fetch(&mut client.send, request(http::Method::CONNECT, "/")).await;
    assert_eq!(resp.status(), 405, "plain CONNECT");
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/still")).await;
    assert_eq!(resp.status(), 200);
    let _ = Arc::clone(&server.endpoint.counters());
}
