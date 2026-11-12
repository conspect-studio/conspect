//! Invariant #1 under ACME failure: a REAL `EngineRuntime` (monotonic clock,
//! realtime pacer, 25 fps) emits every tick on time while the `AcmeService`
//! runs an order against (a) a "CA" that accepts TCP and never answers and
//! (b) a CA port that refuses. The service reports `last_error` without
//! blocking anything. Bounds are counts and generous wall-clock limits that
//! hold on a loaded macOS runner (no percentiles).
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
    let dropped = Instant::now();
    drop(handle);
    assert!(dropped.elapsed() <= Duration::from_secs(3), "bounded join");
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
