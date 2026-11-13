//! Invariant #1 under ACME failure: a REAL `EngineRuntime` (monotonic clock,
//! realtime pacer, 25 fps) emits every tick on time while the `AcmeService`
//! runs an order against (a) a "CA" that accepts TCP and never answers,
//! (b) a CA port that refuses, and an in-process fake CA (account step
//! succeeds) while the ACME thread (c) sits in the TLS-ALPN-01 responder
//! wait, (d) runs the failure aftermath (`challenge_errors`, re-reading the
//! order to deactivate pending authorizations) and (e) holds slow
//! deactivation calls under the aftermath cap. The service reports
//! `last_error` without blocking anything. Bounds are counts and generous wall-clock limits (no
//! percentiles) that fail closed: a stalled clock either repeats frames or
//! overruns the wall budget. It runs in the `feature-clippy` `acme` leg.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

mod acme_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::{AcmeChallenge, CertificateAuthority, NetworkTlsConfig};
use multiview_control::acme::egress::EgressTimeouts;
use multiview_control::acme::{AcmeEngine, AcmeService};
use multiview_control::certificate_source::{
    bootstrap_certificate, AcmeCertificateSource, CertificateSource, ChallengeResolver,
};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::{
    CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource, OutputClock,
    RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;

const TICKS: u64 = 125;

fn build_drive(w: u32, h: u32) -> CompositorDrive<Nv12Image> {
    let color = ColorInfo::default().resolve_defaults(1920, 1080);
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "acme-cadence".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 25,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        Nv12Image::solid(w, h, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

fn config(port: u16, state_dir: &std::path::Path, anchor: &std::path::Path) -> NetworkTlsConfig {
    NetworkTlsConfig {
        ca: CertificateAuthority::Custom,
        staging: false,
        challenge: AcmeChallenge::TlsAlpn01,
        domains: vec!["cadence.example.org".into()],
        contact_email: None,
        acme_profile: None,
        directory: Some(format!("https://[::1]:{port}/dir")),
        trust_anchor: Some(anchor.to_path_buf()),
        allow_private_directory: true,
        eab_kid_env: None,
        eab_hmac_env: None,
        state_dir: state_dir.to_path_buf(),
        dns: None,
    }
}

/// Run a real 25 fps `EngineRuntime` for `TICKS` ticks (the ACME service is
/// already running on its own thread) and assert the cadence bounds.
async fn assert_cadence(started: Instant) {
    let ts: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        build_drive(64, 36),
        ts,
        RealtimePacer,
    );
    let outcome = runtime
        .run_for(
            publisher.as_ref(),
            &StopSignal::new(),
            TICKS,
            |f| f.tick.index,
            |_f| None::<u64>,
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome.ticks, TICKS, "every tick emitted");
    assert!(
        elapsed <= Duration::from_millis(TICKS * 40) + Duration::from_secs(2),
        "wall time {elapsed:?}"
    );
    assert!(
        runtime.frames_repeated() <= 10,
        "repeats {}",
        runtime.frames_repeated()
    );
}

/// Drop the service handle and assert the bounded join.
fn assert_bounded_drop(handle: multiview_control::acme::AcmeHandle) {
    let dropped = Instant::now();
    drop(handle);
    assert!(dropped.elapsed() <= Duration::from_secs(3), "bounded join");
}

/// Run the engine for `TICKS` ticks while the ACME service talks to `port`.
async fn cadence_against(port: u16) {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = acme_support::private_dir(tmp.path(), "state");
    let anchor = acme_support::write_root(&acme_support::fixture_pki(), tmp.path());
    let bootstrap = bootstrap_certificate("cadence.example.org").unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate.clone(),
        bootstrap.fingerprint.clone(),
        "custom".into(),
        vec!["cadence.example.org".into()],
    ));
    let resolver = Arc::new(ChallengeResolver::new(source.clone()));
    let engine = AcmeEngine::new(
        config(port, &state_dir, &anchor),
        None,
        resolver,
        source.clone(),
    )
    .unwrap()
    .with_egress_timeouts(EgressTimeouts {
        connect: Duration::from_secs(1),
        request: Duration::from_secs(2),
    });
    let started = Instant::now();
    let handle = AcmeService::spawn(engine).unwrap();
    assert_cadence(started).await;
    // The service gave up on the CA within its 2 s request deadline, while the
    // clock ran, and reported a sanitized reason; it still serves bootstrap.
    let status = source.status();
    assert_eq!(
        status.last_error.as_deref(),
        Some("ca-unavailable"),
        "{status:?}"
    );
    assert!(status.next_attempt_at.is_some(), "{status:?}");
    assert!(Arc::ptr_eq(&source.current(), &bootstrap.certificate));
    assert_bounded_drop(handle);
}

/// Wait (bounded, real time) until the service reports an error.
async fn reported_error(source: &AcmeCertificateSource, within: Duration) -> Option<String> {
    let deadline = Instant::now() + within;
    loop {
        let status = source.status();
        if status.last_error.is_some() || Instant::now() >= deadline {
            return status.last_error;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run the cadence check while the service talks to the in-process fake CA
/// (`offered` challenge types). `responder_ready` marks the TLS-ALPN-01
/// responder installed; otherwise the engine waits `responder_wait` for it.
async fn cadence_against_fake_ca(
    offered: Vec<&'static str>,
    responder_ready: bool,
    deactivate_delay: Option<Duration>,
) -> (acme_support::SharedCa, Option<String>, Option<String>) {
    let name = "cadence.example.org";
    let (ca, directory, pki) = acme_support::fake_ca(
        name,
        acme_support::FakeCaMode {
            offered,
            fail_detail: "203.0.113.7: Connection refused".into(),
            deactivate_delay,
        },
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = acme_support::private_dir(tmp.path(), "state");
    let anchor = acme_support::write_root(&pki, tmp.path());
    let bootstrap = bootstrap_certificate(name).unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate.clone(),
        bootstrap.fingerprint.clone(),
        "custom".into(),
        vec![name.into()],
    ));
    let resolver = Arc::new(ChallengeResolver::new(source.clone()));
    if responder_ready {
        resolver.mark_responder_ready();
    }
    let mut cfg = config(0, &state_dir, &anchor);
    cfg.directory = Some(directory);
    let engine = AcmeEngine::new(cfg, None, resolver, source.clone())
        .unwrap()
        .with_responder_wait(Duration::from_secs(4))
        .with_aftermath_total(Duration::from_secs(3));
    let started = Instant::now();
    let handle = AcmeService::spawn(engine).unwrap();
    assert_cadence(started).await;
    let error = reported_error(&source, Duration::from_secs(10)).await;
    let detail = source.status().last_error_detail;
    assert!(Arc::ptr_eq(&source.current(), &bootstrap.certificate));
    assert_bounded_drop(handle);
    (ca, error, detail)
}

/// (c) The ACME thread sits in `wait_responder_ready` (4 s) for most of the
/// run: the account step succeeded, the responder is never installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_keeps_cadence_while_waiting_for_the_alpn_responder() {
    let (ca, error, detail) = cadence_against_fake_ca(vec!["tls-alpn-01"], false, None).await;
    assert_eq!(error.as_deref(), Some("tls-alpn-responder-not-installed"));
    assert!(detail.unwrap().contains("acme-tls/1 responder"));
    let st = ca.lock().unwrap();
    assert_eq!(st.accounts, 1, "the account step succeeded first");
    assert_eq!(st.orders, 0, "no order while the responder is missing");
}

/// (d) The order fails during the ticks: `challenge_errors` refreshes the
/// authorization and the failed order is re-read for deactivation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_keeps_cadence_while_a_failed_order_is_examined() {
    let (ca, error, detail) = cadence_against_fake_ca(vec!["tls-alpn-01"], true, None).await;
    assert_eq!(error.as_deref(), Some("challenge-failed"));
    let detail = detail.unwrap();
    assert!(detail.contains("Connection refused"), "{detail}");
    assert_eq!(ca.lock().unwrap().set_ready, 1);
}

/// (e) The CA offers no TLS-ALPN-01 challenge (unsupported, at once) and
/// holds every deactivation request for 30 s: the aftermath is cut at its
/// total cap (3 s here) while the clock keeps cadence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_keeps_cadence_while_deactivation_hangs() {
    let (ca, error, detail) =
        cadence_against_fake_ca(vec!["http-01"], true, Some(Duration::from_secs(30))).await;
    assert_eq!(error.as_deref(), Some("ca-offers-no-supported-challenge"));
    assert!(detail.unwrap().contains("offers no tls-alpn-01"));
    assert_eq!(ca.lock().unwrap().deactivations, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_keeps_cadence_while_the_ca_hangs() {
    // Accepts TCP, never answers (no TLS ServerHello, nothing).
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let keep = held.clone();
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            keep.lock().await.push(stream);
        }
    });
    cadence_against(port).await;
    accept.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_keeps_cadence_while_the_ca_refuses() {
    // Bind then drop: the port refuses connections.
    let port = {
        let l = std::net::TcpListener::bind("[::1]:0").unwrap();
        l.local_addr().unwrap().port()
    };
    cadence_against(port).await;
}

/// Review I2: a `Shutdown` that arrives while the aftermath hangs on the CA
/// (deactivation held 30 s, default 60 s aftermath cap) stops the service
/// within the 2 s drop budget; the thread really exits (not detached).
#[test]
fn shutdown_cuts_a_hanging_aftermath_short() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let name = "cadence.example.org";
    let (ca, directory, pki) = rt.block_on(acme_support::fake_ca(
        name,
        acme_support::FakeCaMode {
            offered: vec!["http-01"],
            fail_detail: String::new(),
            deactivate_delay: Some(Duration::from_secs(30)),
        },
    ));
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = acme_support::private_dir(tmp.path(), "state");
    let anchor = acme_support::write_root(&pki, tmp.path());
    let bootstrap = bootstrap_certificate(name).unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate,
        bootstrap.fingerprint,
        "custom".into(),
        vec![name.into()],
    ));
    let resolver = Arc::new(ChallengeResolver::new(source.clone()));
    resolver.mark_responder_ready();
    let mut cfg = config(0, &state_dir, &anchor);
    cfg.directory = Some(directory);
    let engine = AcmeEngine::new(cfg, None, resolver, source.clone()).unwrap();
    let handle = AcmeService::spawn(engine).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while ca.lock().unwrap().deactivations == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ca.lock().unwrap().deactivations, 1, "in the aftermath");
    assert!(source.status().last_error.is_none(), "not reported yet");
    let dropped = Instant::now();
    drop(handle);
    assert!(dropped.elapsed() <= Duration::from_secs(3), "bounded join");
    // The drop waited for the thread to finish (it did not detach): no
    // further CA call happens after shutdown.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(ca.lock().unwrap().deactivations, 1);
    drop(rt);
}
