//! R3 chaos (front-door slice 2, #288; invariants #1 and #10): the REAL engine
//! tick loop (`EngineRuntime` + `RealtimePacer` + `MonotonicTimeSource` on its
//! own program runtime, the harness of `multiview-frontdoor`'s chaos tests)
//! keeps its cadence while HTTP/3 clients hammer the front door's h3 endpoint,
//! which runs on the front door's own runtime exactly as the run wires it
//! (`FrontDoorRun::start_h3` on the QUIC lane of the shared socket).
//!
//! The hammer, from a separate client runtime, for `FRONTDOOR_CHAOS_SECS`
//! (default 6 s): [`CLIENTS`] concurrent h3 connections, each looping
//! GETs, 1 MiB POSTs, an oversized header block (declared 1 MiB, rejected by
//! the guard) and WebTransport CONNECTs (the first admitted, every further
//! one on that connection `429`; the handler holds a session until it
//! closes, as `MoqWt` does); every [`CHURN_EVERY`]th round the connection is
//! dropped and a new one opened (connection churn). Each client task keeps
//! one client endpoint (socket) for its connections, as a browser does.
//!
//! Asserted (the bounds of `chaos_flood.rs` / `tcp_chaos.rs`, sized for a
//! loaded 2-core CI runner; never a tight percentile on its own):
//! * idle and hammer runs: one frame per tick (`elapsed / period` within 2),
//!   consecutive indices and the exact PTS from the tick; on Linux 0 repeats
//!   the stall witness does not explain (`common::assert_cadence`);
//! * tick lateness p99 is reported for both runs, NOT asserted: issue #321
//!   retired percentile comparisons from `chaos_flood.rs` (the assertion
//!   class that fails on a loaded host without the code under test slipping),
//!   and the `tcp_chaos.rs` p99 bound compares against a door-off arm carrying
//!   the same client load, which has no QUIC equivalent (no server, no
//!   crypto). Measured: 20 parallel copies of this test on a 56-core host put
//!   the hammer p99 up to 15 ms over idle while every run kept every tick,
//!   0 repeats and exact PTS. A tick a whole period late is a repeat, which
//!   `assert_cadence` does judge;
//! * RSS grows by less than [`RSS_BOUND_KB`] while the hammer runs (from the
//!   warmed-up baseline: churned connections and sessions are released);
//! * the hammer really ran (requests, 429s and guard rejections counted) and
//!   the endpoint answers a fresh GET afterwards;
//! * #382: [`LONG_SESSIONS`] WebTransport sessions opened before the idle
//!   run stay up through idle and hammer (past the 10 s header deadline that
//!   used to reset every CONNECT stream), each receiving the handler's
//!   server-opened streams the whole time, with no header-deadline reset.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::print_stderr,
    clippy::doc_markdown
)]

#[path = "../../multiview-frontdoor/tests/common/witness.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http;
use multiview_config::MultiviewConfig;
use multiview_h3::client::{self, Client};
use multiview_h3::{AdmitToken, BoxFuture, Counters, WtHandler, WtSession};

use common::{assert_cadence, report, run_clock, ClockRun};

/// Concurrent hammer connections.
const CLIENTS: usize = 32;
/// Long-lived sessions held across the idle and hammer runs (#382).
const LONG_SESSIONS: usize = 8;
/// Every this many rounds a client drops its connection and opens a new one.
const CHURN_EVERY: u64 = 4;
/// RSS growth bound during the hammer.
const RSS_BOUND_KB: u64 = 64 * 1024;
const POST_BYTES: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

fn secs() -> u64 {
    std::env::var("FRONTDOOR_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6)
}

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

fn p99(r: &ClockRun) -> i64 {
    let mut late: Vec<i64> = r.ticks.iter().map(common::TickRecord::lateness).collect();
    late.sort_unstable();
    late[((late.len() as f64 - 1.0) * 0.99) as usize]
}

/// Admits every CONNECT and holds the session until it closes.
struct HoldWt;

impl WtHandler for HoldWt {
    fn admit(
        &self,
        _: &http::request::Parts,
        _: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode> {
        Ok(Box::new(()))
    }
    fn accept(
        &self,
        session: WtSession,
        _: AdmitToken,
        _: http::request::Parts,
        _: SocketAddr,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            use web_transport_trait::{SendStream as _, Session as _};
            // Like a MoQ viewer: one server-opened stream per 100 ms while
            // the session lives, never awaiting more than a write's flow
            // control (a slow client only ever slows its own task).
            let feed = async {
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                loop {
                    tick.tick().await;
                    let Ok(mut uni) = session.open_uni().await else {
                        return;
                    };
                    if uni.write(b"chaos-media").await.is_err() || uni.finish().is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                () = feed => {}
                _ = session.closed() => {}
            }
        })
    }
    fn protocols(&self) -> &[&'static str] {
        &["moqt-16"]
    }
}

#[derive(Default)]
struct Tally {
    get_ok: AtomicU64,
    post_ok: AtomicU64,
    wt_200: AtomicU64,
    wt_429: AtomicU64,
    oversize_sent: AtomicU64,
    connects: AtomicU64,
    errors: AtomicU64,
    err_get: AtomicU64,
    err_post: AtomicU64,
    err_wt: AtomicU64,
}

fn site() -> axum::Router {
    axum::Router::new()
        .route("/hello", axum::routing::get(|| async { "hello" }))
        .route(
            "/sink",
            axum::routing::post(|body: axum::body::Bytes| async move { body.len().to_string() }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2 * POST_BYTES))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

async fn connect(udp: u16) -> Option<Client> {
    Client::connect(
        "[::1]:0".parse().unwrap(),
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, udp)),
        "localhost",
        client::insecure_client_config(provider(), b"h3").unwrap(),
        TIMEOUT,
    )
    .await
    .ok()
}

/// A HEADERS frame declaring 1 MiB with 4 KiB sent: the guard stops it.
async fn oversize(c: &Client) -> bool {
    let Ok((mut send, _recv)) = c.conn.open_bi().await else {
        return false;
    };
    let mut frame = Vec::new();
    multiview_h3::varint::encode(0x01, &mut frame);
    multiview_h3::varint::encode(1 << 20, &mut frame);
    frame.extend(std::iter::repeat_n(0u8, 4096));
    send.write_all(&frame).await.is_ok()
}

async fn hammer(udp: u16, stop: Arc<AtomicBool>, tally: Arc<Tally>) {
    let auth = format!("localhost:{udp}");
    let endpoint = noq::Endpoint::client("[::1]:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client::insecure_client_config(provider(), b"h3").unwrap());
    let body = vec![0x5au8; POST_BYTES];
    let mut round = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let c = Client::connect_on(
            &endpoint,
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, udp)),
            "localhost",
            TIMEOUT,
        )
        .await
        .ok();
        let Some(mut c) = c else {
            tally.errors.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        };
        tally.connects.fetch_add(1, Ordering::Relaxed);
        // Hold the admitted session's CONNECT stream for this connection.
        let mut held = None;
        while !stop.load(Ordering::Relaxed) {
            round += 1;
            {
                match c
                    .fetch(
                        client::request(http::Method::GET, &auth, "/hello").unwrap(),
                        &[],
                        TIMEOUT,
                    )
                    .await
                {
                    Ok((r, _)) if r.status() == 200 => {
                        tally.get_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    other => {
                        if tally.err_get.fetch_add(1, Ordering::Relaxed) < 3 {
                            eprintln!("GET error: {:?}", other.map(|(r, _)| r.status()));
                        }
                        tally.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            {
                match c
                    .fetch(
                        client::request(http::Method::POST, &auth, "/sink").unwrap(),
                        &body,
                        TIMEOUT,
                    )
                    .await
                {
                    Ok((r, b)) if r.status() == 200 && b == POST_BYTES.to_string().as_bytes() => {
                        tally.post_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    other => {
                        if tally.err_post.fetch_add(1, Ordering::Relaxed) < 3 {
                            eprintln!("POST error: {:?}", other.map(|(r, _)| r.status()));
                        }
                        tally.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if oversize(&c).await {
                tally.oversize_sent.fetch_add(1, Ordering::Relaxed);
            }
            {
                match c
                    .open(
                        client::connect_request(&auth, "/moq/?ticket=chaos", Some("\"moqt-16\""))
                            .unwrap(),
                        TIMEOUT,
                    )
                    .await
                {
                    Ok((r, stream)) if r.status() == 200 => {
                        tally.wt_200.fetch_add(1, Ordering::Relaxed);
                        held = Some(stream);
                    }
                    Ok((r, _)) if r.status() == 429 => {
                        tally.wt_429.fetch_add(1, Ordering::Relaxed);
                    }
                    other => {
                        if tally.err_wt.fetch_add(1, Ordering::Relaxed) < 3 {
                            eprintln!("WT error: {:?}", other.map(|(r, _)| r.status()));
                        }
                        tally.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if round % CHURN_EVERY == 0 {
                break;
            }
        }
        drop(held);
        drop(c);
    }
}

#[test]
fn h3_hammer_does_not_slip_the_output_clock() {
    let duration = Duration::from_secs(secs());
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![ck.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into()),
        )
        .unwrap();
    let udp_port = std::net::UdpSocket::bind("[::]:0")
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
https_port = {udp_port}
"##
    ))
    .unwrap();
    let run = multiview_cli::frontdoor::FrontDoorRun::start_with(
        &config,
        multiview_frontdoor::ListenSockets::empty(),
    )
    .expect("front door starts");
    let udp = run.local_addr().port();
    let (_signal, watch) = tokio::sync::watch::channel(false);
    run.set_h3_moq(multiview_cli::frontdoor::H3Moq {
        wt: Arc::new(HoldWt),
        on_up: Box::new(|| {}),
        fallback_tls: None,
    });
    run.start_h3(
        Arc::new(multiview_control::h3_site::H3Site::new(site(), watch)),
        multiview_cli::frontdoor::H3Tls {
            config: Some(Arc::new(tls)),
            ready: None,
            source: "chaos test",
        },
    );
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("h3-clients")
        .enable_all()
        .build()
        .unwrap();
    // The endpoint is up and serves before the clock runs.
    let started = std::time::Instant::now();
    while !run.h3_running() {
        assert!(started.elapsed() < TIMEOUT, "the h3 endpoint never came up");
        std::thread::sleep(Duration::from_millis(10));
    }
    client_rt.block_on(async {
        let mut c = connect(udp).await.expect("h3 connect");
        let auth = format!("localhost:{udp}");
        let (r, _) = c
            .fetch(
                client::request(http::Method::GET, &auth, "/hello").unwrap(),
                &[],
                TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    });

    // Long-lived sessions (#382): opened now, read for the whole test.
    let long_stop = Arc::new(AtomicBool::new(false));
    let long_opened = std::time::Instant::now();
    let long: Vec<_> = (0..LONG_SESSIONS)
        .map(|_| client_rt.spawn(long_session(udp, Arc::clone(&long_stop))))
        .collect();

    // 1. Idle: the endpoint up, no hammer clients.
    let idle = run_clock(duration, |_| {});
    report("h3 idle", &idle);

    // 2. Hammer.
    let stop = Arc::new(AtomicBool::new(false));
    let tally = Arc::new(Tally::default());
    let tasks: Vec<_> = (0..CLIENTS)
        .map(|_| client_rt.spawn(hammer(udp, Arc::clone(&stop), Arc::clone(&tally))))
        .collect();
    // Steady state before the clock and the RSS baseline (3 s: measured ramp
    // of the 32 client and server connections, their bounded QUIC windows and
    // the allocator's arenas); what is asserted is that the hammer does not
    // GROW memory after that.
    std::thread::sleep(Duration::from_secs(3));
    let rss_before = rss_kb();
    let rss_thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                samples.push(rss_kb());
                std::thread::sleep(Duration::from_millis(500));
            }
            samples
        })
    };
    let loaded = run_clock(duration, |_| {});
    let rss_after = rss_kb();
    stop.store(true, Ordering::Relaxed);
    long_stop.store(true, Ordering::Relaxed);
    let long_held = long_opened.elapsed();
    let samples = rss_thread.join().unwrap();
    eprintln!("h3 chaos: RSS samples (kB, every 500 ms): {samples:?}");
    client_rt.block_on(async {
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(30), task).await;
        }
    });
    let long_results: Vec<LongResult> = client_rt.block_on(async {
        let mut out = Vec::new();
        for task in long {
            out.push(
                tokio::time::timeout(Duration::from_secs(30), task)
                    .await
                    .expect("a long session task hung")
                    .unwrap(),
            );
        }
        out
    });
    eprintln!("h3 chaos: long sessions held {long_held:?}: {long_results:?}");
    report("h3 hammer", &loaded);
    let counters = run_counters(&run);
    let deadline_resets = run
        .h3_counters()
        .map_or(0, |c| Counters::get(&c.header_deadline_rejections));
    if let Some(c) = run.h3_counters() {
        eprintln!("h3 chaos: endpoint counters {c:?}");
    }
    eprintln!(
        "h3 chaos: connects {} get {} post {} wt 200 {} wt 429 {} oversize {} errors {}; \
         endpoint {counters:?}; rss {rss_before} -> {rss_after} kB; p99 idle {:.3} ms hammer {:.3} ms",
        tally.connects.load(Ordering::Relaxed),
        tally.get_ok.load(Ordering::Relaxed),
        tally.post_ok.load(Ordering::Relaxed),
        tally.wt_200.load(Ordering::Relaxed),
        tally.wt_429.load(Ordering::Relaxed),
        tally.oversize_sent.load(Ordering::Relaxed),
        tally.errors.load(Ordering::Relaxed),
        p99(&idle) as f64 / 1e6,
        p99(&loaded) as f64 / 1e6,
    );

    // 3. The endpoint stays responsive after the hammer.
    client_rt.block_on(async {
        let mut c = connect(udp).await.expect("h3 connect after the hammer");
        let auth = format!("localhost:{udp}");
        let (r, body) = c
            .fetch(
                client::request(http::Method::GET, &auth, "/hello").unwrap(),
                &[],
                TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(body, b"hello");
    });
    client_rt.block_on(run.shutdown());

    if !common::WITNESS_RELIABLE {
        eprintln!("h3 chaos: repeats NOT judged on this platform (stall witness unreliable)");
    }
    assert_cadence("h3 idle", &idle);
    assert_cadence("h3 hammer", &loaded);
    assert!(
        rss_after.saturating_sub(rss_before) < RSS_BOUND_KB,
        "RSS grew {rss_before} -> {rss_after} kB under the h3 hammer"
    );
    // The hammer really ran, on every path.
    assert!(tally.get_ok.load(Ordering::Relaxed) >= 100, "GETs ran");
    assert!(
        tally.post_ok.load(Ordering::Relaxed) >= 32,
        "1 MiB POSTs ran"
    );
    assert!(
        tally.wt_200.load(Ordering::Relaxed) >= 8,
        "sessions admitted"
    );
    assert!(
        tally.wt_429.load(Ordering::Relaxed) >= 8,
        "second CONNECTs got 429"
    );
    assert!(
        tally.connects.load(Ordering::Relaxed) > CLIENTS as u64,
        "connections churned"
    );
    assert!(counters.0 > 0, "the header guard rejected oversized blocks");
    // #382: every long session lived to the end, past the header deadline,
    // and kept receiving (one stream per 100 ms; a loaded runner may lag,
    // a reset session stops at ~100 by 10 s).
    let header_deadline = multiview_h3::H3Limits::default().header_deadline;
    assert!(
        long_held > header_deadline + Duration::from_secs(2),
        "{long_held:?}"
    );
    for r in &long_results {
        assert!(
            r.alive_at_end,
            "a long session ended early: {long_results:?}"
        );
        assert!(
            r.streams * 100 >= (long_held.as_millis() as u64) / 4,
            "a long session stopped receiving: {long_results:?}"
        );
    }
    assert_eq!(
        deadline_resets, 0,
        "no stream was reset by the header deadline"
    );
}

#[derive(Debug)]
struct LongResult {
    /// The connection stayed up and streams kept arriving in the last 3 s.
    alive_at_end: bool,
    streams: u64,
}

/// One WebTransport session held until `stop`, counting the server's
/// streams; `alive_at_end` is false if the session or connection ended
/// before `stop`.
async fn long_session(udp: u16, stop: Arc<AtomicBool>) -> LongResult {
    let auth = format!("localhost:{udp}");
    let mut c = connect(udp).await.expect("long session connect");
    let (r, connect_stream) = c
        .open(
            client::connect_request(&auth, "/moq/?ticket=long", Some("\"moqt-16\"")).unwrap(),
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // The session is alive while its server-opened streams keep arriving:
    // a reset CONNECT stream ends the session, not the connection.
    let mut alive = true;
    let mut recent = std::collections::VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if c.conn.close_reason().is_some() {
            alive = false;
            break;
        }
        recent.push_back(c.webtransport_uni_streams());
        if recent.len() > 15 {
            recent.pop_front();
        }
    }
    let streams = c.webtransport_uni_streams();
    // Streams in the last 3 s (15 samples of 200 ms; 30 expected).
    let tail = streams - recent.front().copied().unwrap_or(0);
    alive &= tail > 0;
    drop(connect_stream);
    LongResult {
        alive_at_end: alive,
        streams,
    }
}

/// `(header_guard_rejections, wt_429, connections_accepted)` while the
/// endpoint ran (read before shutdown through the run's endpoint).
fn run_counters(run: &multiview_cli::frontdoor::FrontDoorRun) -> (u64, u64, u64) {
    let Some(c) = run.h3_counters() else {
        return (0, 0, 0);
    };
    (
        Counters::get(&c.header_guard_rejections),
        Counters::get(&c.wt_429),
        Counters::get(&c.connections_accepted),
    )
}
