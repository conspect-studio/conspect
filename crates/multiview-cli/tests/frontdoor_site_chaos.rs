//! Invariants #1 and #10 with the engine serving the site itself (slice 6,
//! issue #292): HLS segments, the UI and the port-80 redirect now come from
//! the engine's front door instead of nginx. While the REAL engine tick loop
//! (`EngineRuntime` + `RealtimePacer`, 30 fps) runs on its own program
//! runtime, the door is hit with:
//!
//! * stalled readers: TLS clients that request a 4 MiB segment (and the UI's
//!   largest bundle) and never read the body, holding connections open;
//! * a stream of full segment downloads and `Range` requests;
//! * a port-80 flood of slow and garbage request lines.
//!
//! Asserted (counts and bounds, never tight percentiles; RULES "timing
//! faults"): one frame per tick within 2 of elapsed/period in both runs, no
//! more repeated frames under load than idle + 1, and the per-connection
//! buffering stays bounded: RSS grows by less than 64 MiB with 64 stalled
//! 4 MiB downloads open (whole-file buffering would need 256 MiB).
#![cfg(all(feature = "tls", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::as_conversions
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, RealtimePacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const FPS: i64 = 30;
const SEGMENT_BYTES: usize = 4 * 1024 * 1024;
const STALLED: usize = 64;
const RSS_BOUND_KB: u64 = 64 * 1024;

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn build_drive() -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "site-chaos".to_owned(),
            canvas: Canvas {
                width: 64,
                height: 36,
                fps_num: 30,
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
        Nv12Image::solid(
            64,
            36,
            16,
            128,
            128,
            ColorInfo::default().resolve_defaults(1920, 1080),
        )
        .unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[derive(Debug)]
struct ClockRun {
    ticks: u64,
    expected: u64,
    repeats: u64,
    max_late_ms: f64,
}

/// The real tick loop on its own 2-worker program runtime for `duration`.
fn run_clock(duration: Duration) -> ClockRun {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("program")
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
        let mut runtime = EngineRuntime::new(
            OutputClock::new(Rational::new(FPS, 1)).unwrap(),
            build_drive(),
            Arc::clone(&time),
            RealtimePacer,
        );
        let seed = runtime.seed_nanos();
        let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
        let worst = Arc::new(Mutex::new(0i64));
        let w = Arc::clone(&worst);
        let ts = Arc::clone(&time);
        let stop = StopSignal::new();
        let stop2 = stop.clone();
        let repeats = runtime.frames_repeated_counter();
        let started = Instant::now();
        let ticks = Arc::new(AtomicU64::new(0));
        let t2 = Arc::clone(&ticks);
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            stop2.stop();
        });
        runtime
            .run(
                &publisher,
                &stop,
                move |f: &CompositedFrame| {
                    let index = i64::try_from(f.tick.index).unwrap_or(i64::MAX);
                    let late = ts.now_nanos() - (seed + index * 1_000_000_000 / FPS);
                    let mut w = w.lock().unwrap();
                    *w = (*w).max(late);
                    t2.fetch_add(1, Ordering::Relaxed);
                    f.tick.index
                },
                |_| None,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        stopper.await.unwrap();
        let max_late_ms = *worst.lock().unwrap() as f64 / 1e6;
        ClockRun {
            ticks: ticks.load(Ordering::Relaxed),
            expected: (elapsed.as_nanos() as i64 / (1_000_000_000 / FPS)) as u64,
            repeats: repeats.load(Ordering::Relaxed),
            max_late_ms,
        }
    })
}

fn tls_client() -> tokio_rustls::TlsConnector {
    #[derive(Debug)]
    struct AcceptAny;
    impl rustls::client::danger::ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAny))
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

async fn https(
    connector: &tokio_rustls::TlsConnector,
    addr: SocketAddr,
    request: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await?;
    tls.write_all(request.as_bytes()).await?;
    Ok(tls)
}

#[test]
fn serving_the_site_from_the_door_never_slips_the_output_clock() {
    let dir = tempfile::tempdir().unwrap();
    // The HLS output directory with a big segment, and the TLS pair.
    let hls = dir.path().join("hls");
    std::fs::create_dir_all(&hls).unwrap();
    std::fs::write(hls.join("big.m4s"), vec![0x5au8; SEGMENT_BYTES]).unwrap();
    std::fs::write(hls.join("live.m3u8"), "#EXTM3U\n").unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    std::fs::write(dir.path().join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), signing_key.serialize_pem()).unwrap();
    let tcp_port = std::net::TcpListener::bind("[::1]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let redirect_port = std::net::TcpListener::bind("[::1]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = MultiviewConfig::load_from_toml(&format!(
        r##"schema_version = 1
[canvas]
width = 64
height = 36
fps = "30/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[network]
https_port = 0
mode = "direct"
bind = "[::1]"
tcp_port = {tcp_port}
http_redirect_port = {redirect_port}
public_host = "localhost"
cert_file = "{cert}"
key_file = "{key}"
"##,
        cert = dir.path().join("cert.pem").display(),
        key = dir.path().join("key.pem").display(),
    ))
    .unwrap();
    // `https_port = 0` is rejected by validation; the UDP door is not under
    // test here, so give it an ephemeral port directly.
    let mut config = config;
    config.network.as_mut().unwrap().https_port = Some(
        std::net::UdpSocket::bind("[::]:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
    );
    let door = multiview_cli::frontdoor::FrontDoorRun::start_with(
        &config,
        multiview_frontdoor::ListenSockets::empty(),
    )
    .expect("front door starts");
    assert!(door.has_tcp_door());
    // The site: the HLS router (and nothing else needs the full control plane
    // to load the door the same way), on the production serve floor.
    let app = axum::Router::new().nest(
        "/hls/program",
        multiview_output::hls::http::hls_router(hls.clone()),
    );
    let (_signal, watch) = tokio::sync::watch::channel(false);
    let options = multiview_control::ServeOptions::default()
        .with_header_read_timeout(Some(Duration::from_secs(20)));
    let source = multiview_control::static_certificate_source(
        &config.network.as_ref().unwrap().static_site_tls().unwrap(),
    )
    .unwrap();
    door.install_site(
        multiview_control::ConnectionServer::new(app, &options, watch),
        Some(multiview_control::site_server_config(Arc::new(source)).unwrap()),
    );
    let site: SocketAddr = format!("[::1]:{tcp_port}").parse().unwrap();
    let redirect: SocketAddr = format!("[::1]:{redirect_port}").parse().unwrap();

    // Sanity: a full segment comes back intact over the door.
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("clients")
        .enable_all()
        .build()
        .unwrap();
    let connector = tls_client();
    let got = client_rt.block_on(async {
        let mut tls = https(
            &connector,
            site,
            "GET /hls/program/big.m4s HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        tls.read_to_end(&mut out).await.ok();
        out
    });
    let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(got.starts_with(b"HTTP/1.1 200"), "{:?}", &got[..64]);
    assert_eq!(got.len() - head_end, SEGMENT_BYTES);

    let duration = Duration::from_secs(
        std::env::var("FRONTDOOR_SITE_CHAOS_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
    );
    let idle = run_clock(duration);
    eprintln!("site chaos idle: {idle:?}");
    let rss_before = rss_kb();

    // Load: stalled readers, downloads, ranges, and a port-80 flood.
    let running = Arc::new(AtomicBool::new(true));
    let downloads = Arc::new(AtomicU64::new(0));
    let redirects = Arc::new(AtomicU64::new(0));
    let stalled: Vec<_> = client_rt.block_on(async {
        let mut held = Vec::new();
        for _ in 0..STALLED {
            if let Ok(tls) = https(
                &connector,
                site,
                "GET /hls/program/big.m4s HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            {
                held.push(tls);
            }
        }
        held
    });
    assert!(stalled.len() >= STALLED / 2, "{} stalled", stalled.len());
    let mut tasks = Vec::new();
    for lane in 0..4 {
        let running = Arc::clone(&running);
        let downloads = Arc::clone(&downloads);
        let connector = connector.clone();
        tasks.push(client_rt.spawn(async move {
            while running.load(Ordering::Relaxed) {
                let req = if lane % 2 == 0 {
                    "GET /hls/program/big.m4s HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                } else {
                    "GET /hls/program/big.m4s HTTP/1.1\r\nHost: localhost\r\nRange: bytes=1000-200000\r\nConnection: close\r\n\r\n"
                };
                if let Ok(mut tls) = https(&connector, site, req).await {
                    let mut sink = vec![0u8; 64 * 1024];
                    while let Ok(n) = tls.read(&mut sink).await {
                        if n == 0 {
                            break;
                        }
                    }
                    downloads.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for lane in 0..4 {
        let running = Arc::clone(&running);
        let redirects = Arc::clone(&redirects);
        tasks.push(client_rt.spawn(async move {
            while running.load(Ordering::Relaxed) {
                if let Ok(mut s) = TcpStream::connect(redirect).await {
                    if lane == 0 {
                        // Slow request line: the 5 s deadline closes it.
                        let _ = s.write_all(b"GET /slow").await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        drop(s);
                    } else {
                        let _ = s.write_all(b"GET /a HTTP/1.1\r\n\r\n").await;
                        let mut out = Vec::new();
                        let _ = s.read_to_end(&mut out).await;
                        redirects.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    std::thread::sleep(Duration::from_millis(500));
    let loaded = run_clock(duration);
    let rss_loaded = rss_kb();
    running.store(false, Ordering::Relaxed);
    eprintln!(
        "site chaos loaded: {loaded:?}; stalled {} x {} MiB, downloads {}, redirects {}, rss {rss_before} -> {rss_loaded} kB",
        stalled.len(),
        SEGMENT_BYTES >> 20,
        downloads.load(Ordering::Relaxed),
        redirects.load(Ordering::Relaxed)
    );
    drop(stalled);
    client_rt.block_on(async {
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
        }
    });

    for (name, r) in [("idle", &idle), ("loaded", &loaded)] {
        assert!(
            r.ticks + 2 >= r.expected && r.ticks <= r.expected + 2,
            "{name}: one frame per tick: {r:?}"
        );
    }
    assert!(
        loaded.repeats <= idle.repeats + 1,
        "repeats under site load ({}) exceed idle ({}) + 1",
        loaded.repeats,
        idle.repeats
    );
    assert!(downloads.load(Ordering::Relaxed) >= 1, "the door served downloads");
    assert!(redirects.load(Ordering::Relaxed) >= 1, "the redirect served");
    assert!(
        rss_loaded.saturating_sub(rss_before) < RSS_BOUND_KB,
        "{} stalled 4 MiB downloads must not be buffered: rss {rss_before} -> {rss_loaded} kB",
        STALLED
    );
    let fd_door = door;
    client_rt.shutdown_timeout(Duration::from_secs(2));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(fd_door.shutdown());
}
