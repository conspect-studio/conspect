//! Endpoint policy: ALPN, 0-RTT off, connection caps, the peers hook.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use multiview_h3::{server_tls, Counters, H3Limits, PeersHook, SourceFilter};

#[test]
fn built_tls_config_is_h3_only_without_early_data() {
    let cert = cert();
    let mut tls = common::server_tls(&cert);
    tls.max_early_data_size = u32::MAX;
    tls.alpn_protocols = vec![b"moql".to_vec(), b"h3".to_vec()];
    tls.key_log = Arc::new(rustls::KeyLogFile::new());
    let built = server_tls(&tls).unwrap();
    assert_eq!(built.max_early_data_size, 0);
    assert_eq!(built.alpn_protocols, vec![b"h3".to_vec()]);
    assert!(!built.key_log.will_log("CLIENT_RANDOM"));
}

#[test]
fn tls_without_tls13_is_rejected() {
    let cert = cert();
    let tls12 = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.clone()], cert.key.clone_key())
        .unwrap();
    assert!(server_tls(&tls12).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alpn_other_than_h3_fails_the_handshake() {
    let server = start(TestSite::new(), None, H3Limits::default(), None).await;
    assert!(quic_connect(&server, b"moql").await.is_err());
    assert!(quic_connect(&server, b"h3").await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_source_and_global_caps_refuse() {
    let limits = H3Limits {
        max_connections_per_source: 2,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    let a = quic_connect(&server, b"h3").await.unwrap();
    let b = quic_connect(&server, b"h3").await.unwrap();
    // The cap is charged when the SERVER side of the handshake completes,
    // which can trail the client's; wait for it so the third attempt meets
    // the pre-handshake refusal deterministically.
    wait_accepted(&server, 2).await;
    assert!(
        quic_connect(&server, b"h3").await.is_err(),
        "per-source cap"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_refused),
        1
    );
    drop((a, b));

    let limits = H3Limits {
        max_connections: 1,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    let held = quic_connect(&server, b"h3").await.unwrap();
    wait_accepted(&server, 1).await;
    assert!(quic_connect(&server, b"h3").await.is_err(), "global cap");
    // Closing frees the slot.
    held.close(noq::VarInt::from_u32(0), b"");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if quic_connect(&server, b"h3").await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "slot never freed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the server has charged `n` connections against its caps.
async fn wait_accepted(server: &Server, n: u64) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while Counters::get(&server.endpoint.counters().connections_accepted) < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "server never charged the connection"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_hook_sees_up_then_down() {
    let events: Arc<Mutex<Vec<(SocketAddr, bool)>>> = Arc::default();
    let hook: PeersHook = {
        let events = Arc::clone(&events);
        Arc::new(move |addr, up| events.lock().unwrap().push((addr, up)))
    };
    let server = start(TestSite::new(), None, H3Limits::default(), Some(hook)).await;
    let conn = quic_connect(&server, b"h3").await.unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while events.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    conn.close(noq::VarInt::from_u32(0), b"bye");
    while events.lock().unwrap().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no down event");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(events[0].1 && !events[1].1);
    assert_eq!(events[0].0, events[1].0);
    server.endpoint.shutdown().await;
}

/// `refuse_source` refuses a matching source before any cap check, and
/// leaves a non-matching source untouched (design note §8: the forwarder
/// hop a `behind_proxy` deployment must fail closed on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuse_source_refuses_the_matched_address_only() {
    let other: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
    let filter: SourceFilter = Arc::new(move |addr: SocketAddr| addr.ip().is_loopback());
    let server = start_with_filter(TestSite::new(), None, H3Limits::default(), None, filter).await;

    assert!(
        quic_connect(&server, b"h3").await.is_err(),
        "a loopback source (the client in this test) is refused"
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if Counters::get(&server.endpoint.counters().sources_refused) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sources_refused never incremented"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_accepted),
        0,
        "the refused source never reaches the cap checks"
    );

    // A filter refusing a DIFFERENT address lets the real (loopback) client
    // through normally.
    let other_filter: SourceFilter = Arc::new(move |addr: SocketAddr| addr.ip() == other.ip());
    let server = start_with_filter(
        TestSite::new(),
        None,
        H3Limits::default(),
        None,
        other_filter,
    )
    .await;
    assert!(
        quic_connect(&server, b"h3").await.is_ok(),
        "a non-matching filter admits the real client"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().sources_refused),
        0
    );
}
