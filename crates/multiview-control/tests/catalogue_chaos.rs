//! Chaos tests for the deep probe (R3): hostile fixtures on `[::1]` prove the bounds
//! hold and nothing blocks — slow-drip body, endless/oversized playlist, a lying
//! `Content-Length`, redirects to loopback/IMDS, and 50+ concurrent probes.
//!
//! The runtime is single-threaded (`#[tokio::test]` default) on purpose: a
//! "reactor ticker" measures the largest gap between its 20 ms ticks while the
//! hostile probes run, so any blocking call on the control-plane runtime shows up as
//! a latency spike. (Invariants #1/#10: probes never touch the data plane; here we
//! prove they do not even stall the control-plane reactor.)
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::type_complexity,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    clippy::items_after_statements,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::unreadable_literal
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_control::catalogue::guard::DialGuard;
use multiview_control::catalogue::probe::engine::HttpProbeRunner;
use multiview_control::catalogue::probe::{
    ProbeRequest, ProbeResult, ProbeService, Verdict, MAX_CONCURRENT, MAX_QUEUED, WALL_CLOCK,
};
use multiview_control::ControlError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Hostile {
    port: u16,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}

impl Hostile {
    fn url(&self, path: &str) -> String {
        format!("http://[::1]:{}{path}", self.port)
    }
}

async fn serve(
    mut sock: tokio::net::TcpStream,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
) {
    let mut buf = vec![0u8; 4096];
    let Ok(n) = sock.read(&mut buf).await else {
        return;
    };
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    let path = text
        .lines()
        .next()
        .unwrap_or("")
        .split(' ')
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    total.fetch_add(1, Ordering::SeqCst);
    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
    max_active.fetch_max(now, Ordering::SeqCst);
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _g = Guard(active);
    let ok =
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nConnection: close\r\n";
    if path.starts_with("/drip") {
        // Slowloris response: valid headers and NO Content-Length (so only the wall
        // clock can stop it), then one byte every 400 ms, forever.
        let _ = sock
            .write_all(
                format!(
                    "{ok}
#EXTM3U
"
                )
                .as_bytes(),
            )
            .await;
        loop {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if sock.write_all(b"#").await.is_err() {
                return;
            }
        }
    } else if path.starts_with("/endless") {
        // No Content-Length: an endless stream of playlist lines as fast as accepted.
        let _ = sock
            .write_all(format!("{ok}\r\n#EXTM3U\n#EXT-X-TARGETDURATION:1\n").as_bytes())
            .await;
        let chunk =
            "#EXTINF:1.0,\nsegment-with-a-long-name-0000000000000000000000.ts\n".repeat(512);
        loop {
            if sock.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
        }
    } else if path.starts_with("/lie") {
        // Declares an enormous body, sends nothing.
        let _ = sock
            .write_all(format!("{ok}Content-Length: 999999999999\r\n\r\n").as_bytes())
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    } else if path.starts_with("/hold") {
        // Holds the connection a while, then a fast failure — used to measure concurrency.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = sock
            .write_all(b"HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    } else if path.starts_with("/imds") {
        let _ = sock.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    } else if path.starts_with("/hugeseg-master") {
        let body =
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:1,\n/hugeseg.ts\n";
        let _ = sock
            .write_all(format!("{ok}Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await;
    } else if path.starts_with("/hugeseg.ts") {
        // A 1 GiB segment, streamed.
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n")
            .await;
        let chunk = vec![0x47u8; 64 * 1024];
        for _ in 0..16_384 {
            if sock.write_all(&chunk).await.is_err() {
                return;
            }
        }
    } else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    }
}

async fn hostile() -> Hostile {
    let listener = TcpListener::bind("[::1]:0").await.expect("bind [::1]");
    let port = listener.local_addr().unwrap().port();
    let (active, max_active, total) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (a, m, t) = (
        Arc::clone(&active),
        Arc::clone(&max_active),
        Arc::clone(&total),
    );
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(sock, Arc::clone(&a), Arc::clone(&m), Arc::clone(&t)));
        }
    });
    Hostile {
        port,
        active,
        max_active,
        total,
    }
}

fn svc(h: &Hostile) -> Arc<ProbeService> {
    let runner =
        HttpProbeRunner::new(DialGuard::for_tests_allowing_loopback_ports(&[h.port])).unwrap();
    ProbeService::new(Arc::new(runner), "chaos-host".to_owned())
}

fn req(url: String) -> ProbeRequest {
    ProbeRequest {
        url,
        user_agent: None,
        referrer: None,
        quality: None,
        channel_country: None,
    }
}

async fn wait_done(s: &Arc<ProbeService>, id: &str, within: Duration) -> ProbeResult {
    let start = Instant::now();
    loop {
        if let Some(r) = s.get(id) {
            if r.state == "done" {
                return r;
            }
        }
        assert!(
            start.elapsed() < within,
            "probe {id} not done within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Spawns a 20 ms ticker and returns a handle yielding the largest observed gap.
fn ticker() -> (
    tokio::task::JoinHandle<Duration>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = Arc::clone(&stop);
    let h = tokio::spawn(async move {
        let mut worst = Duration::ZERO;
        let mut last = Instant::now();
        while !s.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            worst = worst.max(last.elapsed());
            last = Instant::now();
        }
        worst
    });
    (h, stop)
}

#[tokio::test]
async fn slow_drip_body_is_cut_at_the_wall_clock() {
    let h = hostile().await;
    let s = svc(&h);
    let (tick, stop) = ticker();
    let started = Instant::now();
    let job = s.submit("p", req(h.url("/drip.m3u8"))).unwrap();
    let r = wait_done(&s, &job.id, WALL_CLOCK + Duration::from_secs(5)).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(19) && took <= WALL_CLOCK + Duration::from_secs(3),
        "wall clock enforced: {took:?}"
    );
    assert_eq!(r.verdict, Some(Verdict::Dead), "{r:?}");
    assert_eq!(r.verdict_basis, "timeout");
    stop.store(true, Ordering::SeqCst);
    let worst = tick.await.unwrap();
    assert!(
        worst < Duration::from_millis(250),
        "reactor stalled for {worst:?} during a slow drip"
    );
    // The dripping connection is gone (the probe future was dropped).
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        h.active.load(Ordering::SeqCst),
        0,
        "hostile connection released"
    );
}

#[tokio::test]
async fn endless_and_lying_playlists_hit_the_transfer_bound_fast() {
    let h = hostile().await;
    let s = svc(&h);
    for (i, p) in ["/endless.m3u8", "/lie.m3u8"].iter().enumerate() {
        let started = Instant::now();
        let job = s.submit(&format!("p{i}"), req(h.url(p))).unwrap();
        let r = wait_done(&s, &job.id, Duration::from_secs(10)).await;
        assert_eq!(r.verdict, Some(Verdict::Dead), "{p}: {r:?}");
        assert!(
            r.bytes <= 16 * 1024 * 1024,
            "{p}: transfer bound held ({} bytes)",
            r.bytes
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{p}: cut off quickly, took {:?}",
            started.elapsed()
        );
        assert!(
            r.stages
                .iter()
                .any(|st| st.detail.as_deref().is_some_and(|d| d.contains("16 MiB"))),
            "{p}: {r:?}"
        );
    }
}

#[tokio::test]
async fn redirect_to_imds_is_blocked_even_under_load() {
    let h = hostile().await;
    let s = svc(&h);
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(
            s.submit(&format!("p{i}"), req(h.url(&format!("/imds-{i}.m3u8"))))
                .unwrap()
                .id,
        );
    }
    for id in ids {
        let r = wait_done(&s, &id, Duration::from_secs(10)).await;
        assert_eq!(r.verdict, Some(Verdict::Blocked), "{r:?}");
    }
}

#[tokio::test]
async fn fifty_concurrent_probes_respect_the_semaphore_and_never_stall_the_reactor() {
    let h = hostile().await;
    let s = svc(&h);
    let (tick, stop) = ticker();
    let submit_started = Instant::now();
    let mut ids = Vec::new();
    for i in 0..50 {
        // Distinct principals so the per-principal rate limit is not what we measure.
        ids.push(
            s.submit(
                &format!("principal-{i}"),
                req(h.url(&format!("/hold-{i}.m3u8"))),
            )
            .unwrap()
            .id,
        );
    }
    assert!(
        submit_started.elapsed() < Duration::from_millis(500),
        "submit never blocks: {:?}",
        submit_started.elapsed()
    );
    assert!(s.in_flight() <= 50);
    for id in &ids {
        let r = wait_done(&s, id, Duration::from_secs(30)).await;
        assert_eq!(r.verdict, Some(Verdict::Dead));
    }
    assert!(
        h.max_active.load(Ordering::SeqCst) <= MAX_CONCURRENT,
        "at most {MAX_CONCURRENT} probes dial at once, saw {}",
        h.max_active.load(Ordering::SeqCst)
    );
    assert_eq!(h.total.load(Ordering::SeqCst), 50);
    assert_eq!(s.in_flight(), 0, "everything drained");
    stop.store(true, Ordering::SeqCst);
    let worst = tick.await.unwrap();
    assert!(
        worst < Duration::from_millis(250),
        "reactor stalled for {worst:?} with 50 concurrent probes"
    );
}

#[tokio::test]
async fn the_queue_overflows_to_429_without_blocking_and_recovers() {
    let h = hostile().await;
    let s = svc(&h);
    let started = Instant::now();
    let (mut ok, mut shed) = (0usize, 0usize);
    for i in 0..(MAX_CONCURRENT + MAX_QUEUED + 20) {
        match s.submit(
            &format!("flood-{i}"),
            req(h.url(&format!("/drip-{i}.m3u8"))),
        ) {
            Ok(_) => ok += 1,
            Err(ControlError::TooManyRequests(_)) => shed += 1,
            Err(e) => panic!("unexpected {e}"),
        }
    }
    assert_eq!(
        (ok, shed),
        (MAX_CONCURRENT + MAX_QUEUED, 20),
        "4 running + 64 queued admitted, the rest 429"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "overflow is shed instantly, not queued: {:?}",
        started.elapsed()
    );
    assert_eq!(s.in_flight(), MAX_CONCURRENT + MAX_QUEUED);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        h.max_active.load(Ordering::SeqCst) <= MAX_CONCURRENT,
        "only 4 dial while 64 wait"
    );
}

#[cfg(feature = "catalogue-ffmpeg")]
#[tokio::test]
async fn a_one_gib_segment_is_cut_at_16_mib() {
    let h = hostile().await;
    let s = svc(&h);
    let job = s.submit("p", req(h.url("/hugeseg-master.m3u8"))).unwrap();
    let r = wait_done(&s, &job.id, WALL_CLOCK + Duration::from_secs(5)).await;
    assert!(
        r.bytes <= 17 * 1024 * 1024,
        "transfer bound held: {} bytes",
        r.bytes
    );
    assert_ne!(r.verdict, Some(Verdict::Live), "{r:?}");
}
