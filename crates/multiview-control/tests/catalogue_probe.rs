//! Deep-probe tests (slice C): a local in-process HTTP fixture server bound to
//! `[::1]` serves HLS playlists/segments; the probe's dial guard is built with the
//! TEST-ONLY seam `DialGuard::for_tests_allowing_loopback_ports` (feature
//! `_test-seams`, never in a release preset, never reachable from config) so exactly
//! the fixture's port is dialable — every other loopback port and range stays blocked.
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_control::catalogue::guard::DialGuard;
use multiview_control::catalogue::probe::engine::HttpProbeRunner;
use multiview_control::catalogue::probe::{ProbeRequest, ProbeResult, ProbeService, Verdict};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A route's behaviour.
#[derive(Clone)]
pub enum Reply {
    Ok(Vec<u8>, &'static str),
    Status(u16),
    Redirect(String),
    /// Playlist whose media sequence advances on each request.
    Live,
}

#[derive(Clone, Default)]
pub struct Routes(Arc<Mutex<HashMap<String, Reply>>>);

impl Routes {
    pub fn set(&self, path: &str, r: Reply) {
        self.0.lock().unwrap().insert(path.to_owned(), r);
    }
}

pub struct Fixture {
    pub port: u16,
    pub routes: Routes,
    pub hits: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
}

impl Fixture {
    pub fn url(&self, path: &str) -> String {
        format!("http://[::1]:{}{path}", self.port)
    }
}

async fn handle(
    mut sock: tokio::net::TcpStream,
    routes: Routes,
    hits: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
    seq: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 8192];
    let Ok(n) = sock.read(&mut buf).await else {
        return;
    };
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut lines = text.lines();
    let path = lines
        .next()
        .unwrap_or("")
        .split(' ')
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    let headers: HashMap<String, String> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        })
        .collect();
    hits.lock().unwrap().push((path.clone(), headers));
    let reply = routes.0.lock().unwrap().get(&path).cloned();
    let (status, extra, body, ctype): (u16, String, Vec<u8>, &str) = match reply {
        None | Some(Reply::Status(404)) => (404, String::new(), b"nope".to_vec(), "text/plain"),
        Some(Reply::Status(s)) => (s, String::new(), b"x".to_vec(), "text/plain"),
        Some(Reply::Redirect(to)) => (302, format!("Location: {to}\r\n"), Vec::new(), "text/plain"),
        Some(Reply::Ok(b, c)) => (200, String::new(), b, c),
        Some(Reply::Live) => {
            let s = seq.fetch_add(1, Ordering::SeqCst);
            let now = "2026-11-07T05:00:00Z";
            let pl = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{s}\n#EXT-X-PROGRAM-DATE-TIME:{now}\n#EXTINF:1.0,\nseg{s}.ts\n#EXTINF:1.0,\nseg{}.ts\n#EXT-X-CUE-OUT:15\n", s + 1);
            (
                200,
                String::new(),
                pl.into_bytes(),
                "application/vnd.apple.mpegurl",
            )
        }
    };
    let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len());
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.shutdown().await;
}

pub async fn fixture() -> Fixture {
    let listener = TcpListener::bind("[::1]:0").await.expect("bind [::1]");
    let port = listener.local_addr().unwrap().port();
    let routes = Routes::default();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seq = Arc::new(AtomicU64::new(100));
    let (r, h) = (routes.clone(), Arc::clone(&hits));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(handle(sock, r.clone(), Arc::clone(&h), Arc::clone(&seq)));
        }
    });
    Fixture { port, routes, hits }
}

/// The verdict a stage-1..3 fixture yields: with the decode stage compiled in, a
/// playlist whose segments are not served is (honestly) `no_media`; without it the
/// verdict rests on playlist liveness alone.
pub const fn verdict_when_liveness_fails_media_absent() -> Verdict {
    if cfg!(feature = "catalogue-ffmpeg") {
        Verdict::NoMedia
    } else {
        Verdict::Degraded
    }
}

pub fn service_for(f: &Fixture) -> Arc<ProbeService> {
    let guard = DialGuard::for_tests_allowing_loopback_ports(&[f.port]);
    let runner = HttpProbeRunner::new(guard).expect("client");
    ProbeService::new(Arc::new(runner), "test-host".to_owned())
}

pub async fn run(svc: &Arc<ProbeService>, principal: &str, req: ProbeRequest) -> ProbeResult {
    let job = svc.submit(principal, req).expect("admitted");
    for _ in 0..400 {
        if let Some(r) = svc.get(&job.id) {
            if r.state == "done" {
                return r;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("probe did not finish");
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

#[tokio::test]
async fn dial_guard_blocks_loopback_other_than_the_fixture_port() {
    let f = fixture().await;
    let svc = service_for(&f);
    // Same host, different (closed) loopback port: blocked, never dialled.
    let r = run(
        &svc,
        "p",
        req(format!(
            "http://[::1]:{}/x.m3u8",
            f.port.wrapping_add(1).max(1024)
        )),
    )
    .await;
    assert_eq!(r.verdict, Some(Verdict::Blocked), "{r:?}");
    // IMDS and IPv4 loopback literals.
    for u in [
        "http://169.254.169.254/latest/meta-data",
        "http://127.0.0.1:1/x.m3u8",
        "http://[::ffff:127.0.0.1]:1/x",
    ] {
        let r = run(&svc, "p", req(u.to_owned())).await;
        assert_eq!(r.verdict, Some(Verdict::Blocked), "{u}: {r:?}");
        assert!(r.stages.iter().any(|s| s.name == "guard"));
    }
}

#[tokio::test]
async fn redirect_to_loopback_or_imds_is_rescreened_and_blocked() {
    let f = fixture().await;
    let svc = service_for(&f);
    for (i, target) in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:9/evil.m3u8",
        "http://[::1]:1/evil.m3u8",
    ]
    .iter()
    .enumerate()
    {
        let path = format!("/redir{i}.m3u8");
        f.routes.set(&path, Reply::Redirect((*target).to_owned()));
        let r = run(&svc, &format!("p-{i}"), req(f.url(&path))).await;
        assert_eq!(r.verdict, Some(Verdict::Blocked), "{target}: {r:?}");
        // The FIRST hop (the fixture) really was dialled and answered 302; the block
        // happened on the re-screened redirect target, not on hop one.
        let served = f
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == path)
            .count();
        assert_eq!(
            served, 1,
            "{target}: fixture served the redirect exactly once"
        );
    }
    // Redirect loop is bounded (max 5 hops), not a hang.
    f.routes
        .set("/loop.m3u8", Reply::Redirect(f.url("/loop.m3u8")));
    let r = run(&svc, "loop", req(f.url("/loop.m3u8"))).await;
    assert_eq!(r.verdict, Some(Verdict::Dead));
}

#[tokio::test]
async fn http_status_maps_to_verdicts() {
    let f = fixture().await;
    let svc = service_for(&f);
    f.routes.set("/geo.m3u8", Reply::Status(403));
    f.routes.set("/legal.m3u8", Reply::Status(451));
    f.routes.set("/gone.m3u8", Reply::Status(410));
    assert_eq!(
        run(&svc, "a", req(f.url("/geo.m3u8"))).await.verdict,
        Some(Verdict::GeoBlockedHere)
    );
    assert_eq!(
        run(&svc, "a", req(f.url("/legal.m3u8"))).await.verdict,
        Some(Verdict::GeoBlockedHere)
    );
    assert_eq!(
        run(&svc, "a", req(f.url("/gone.m3u8"))).await.verdict,
        Some(Verdict::Dead)
    );
    assert_eq!(
        run(&svc, "a", req(f.url("/missing.m3u8"))).await.verdict,
        Some(Verdict::Dead)
    );
}

#[tokio::test]
async fn headers_are_replayed_and_live_playlist_events_are_reported() {
    let f = fixture().await;
    let svc = service_for(&f);
    f.routes.set("/live.m3u8", Reply::Live);
    let mut r0 = req(f.url("/live.m3u8"));
    r0.user_agent = Some("TestUA/1".into());
    r0.referrer = Some("https://ref.example/".into());
    let r = run(&svc, "a", r0).await;
    assert!(
        r.stages.iter().any(|s| s.name == "playlist"
            && matches!(
                s.status,
                multiview_control::catalogue::probe::StageStatus::Ok
            )),
        "{r:?}"
    );
    {
        let hits = f.hits.lock().unwrap();
        let first = hits.iter().find(|(p, _)| p == "/live.m3u8").unwrap();
        assert_eq!(
            first.1.get("user-agent").map(String::as_str),
            Some("TestUA/1")
        );
        assert_eq!(
            first.1.get("referer").map(String::as_str),
            Some("https://ref.example/")
        );
    }
    assert!(
        r.events.iter().any(|e| e.kind == "cue_out"),
        "events: {:?}",
        r.events
    );
    assert!(r.probed_from == "test-host" && r.probed_at.is_some());
    // Sequence advances across reloads but the PDT is far from now: liveness is a
    // `StalePdt` failure, so the verdict can never be `live`.
    assert!(
        r.stages
            .iter()
            .any(|s| s.name == "liveness" && s.detail.as_deref() == Some("StalePdt")),
        "{r:?}"
    );
    assert_eq!(
        r.verdict,
        Some(verdict_when_liveness_fails_media_absent()),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_stalled_playlist_and_a_vod_are_degraded_and_a_master_is_followed() {
    let f = fixture().await;
    let svc = service_for(&f);
    let stalled = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:1,\na.ts\n";
    f.routes.set(
        "/stall.m3u8",
        Reply::Ok(stalled.as_bytes().to_vec(), "application/vnd.apple.mpegurl"),
    );
    assert_eq!(
        run(&svc, "a", req(f.url("/stall.m3u8"))).await.verdict,
        Some(verdict_when_liveness_fails_media_absent())
    );
    let vod = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
    f.routes.set(
        "/vod.m3u8",
        Reply::Ok(vod.as_bytes().to_vec(), "application/vnd.apple.mpegurl"),
    );
    assert_eq!(
        run(&svc, "a", req(f.url("/vod.m3u8"))).await.verdict,
        Some(verdict_when_liveness_fails_media_absent())
    );
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\nmid/v.m3u8\n";
    f.routes.set(
        "/master.m3u8",
        Reply::Ok(master.as_bytes().to_vec(), "application/vnd.apple.mpegurl"),
    );
    f.routes.set("/mid/v.m3u8", Reply::Live);
    let mut r0 = req(f.url("/master.m3u8"));
    r0.quality = Some("720p".into());
    let r = run(&svc, "a", r0).await;
    assert_eq!(r.facts.advertised_bps, Some(3_000_000));
    assert!(
        f.hits
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "/mid/v.m3u8"),
        "variant nearest 720p was fetched"
    );
    assert!(!f.hits.lock().unwrap().iter().any(|(p, _)| p == "/low.m3u8"));
}

#[tokio::test]
async fn results_are_cached_for_the_same_url_and_headers() {
    let f = fixture().await;
    let svc = service_for(&f);
    f.routes.set("/c.m3u8", Reply::Status(404));
    let a = run(&svc, "a", req(f.url("/c.m3u8"))).await;
    let n = f.hits.lock().unwrap().len();
    let b = svc.submit("a", req(f.url("/c.m3u8"))).unwrap();
    assert!(b.cached && b.state == "done" && b.id == a.id, "{b:?}");
    assert_eq!(
        f.hits.lock().unwrap().len(),
        n,
        "no new dial on a cache hit"
    );
    // Different headers ⇒ different cache key.
    let mut r2 = req(f.url("/c.m3u8"));
    r2.user_agent = Some("other".into());
    let c = svc.submit("a", r2).unwrap();
    assert!(!c.cached);
}

#[tokio::test]
async fn per_principal_rate_limit_is_60_per_minute() {
    let f = fixture().await;
    let svc = service_for(&f);
    f.routes.set("/r.m3u8", Reply::Status(404));
    let mut accepted = 0;
    let mut limited = 0;
    for i in 0..70 {
        match svc.submit("greedy", req(f.url(&format!("/r.m3u8?i={i}")))) {
            Ok(_) => accepted += 1,
            Err(multiview_control::ControlError::TooManyRequests(_)) => limited += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((accepted, limited), (60, 10));
    // Another principal is unaffected.
    assert!(svc.submit("polite", req(f.url("/r.m3u8?z=1"))).is_ok());
}

/// Stage 4/5 on REAL media (needs libav + the ffmpeg CLI to synthesize a clip):
/// two 2 s MPEG-TS segments (testsrc + sine) are served as a live HLS; the probe must
/// demux **and decode** them, report codec/geometry/audio facts and a JPEG thumbnail,
/// and mark the verdict as resting on decoded media.
#[cfg(feature = "catalogue-ffmpeg")]
#[tokio::test]
async fn real_segments_are_demuxed_decoded_and_summarised() {
    let dir = tempfile::tempdir().unwrap();
    let seg = dir.path().join("seg.ts");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "2",
            "-c:v",
            "mpeg2video",
            "-b:v",
            "800k",
            "-c:a",
            "mp2",
            "-b:a",
            "96k",
            "-f",
            "mpegts",
        ])
        .arg(&seg)
        .status();
    let Ok(status) = status else {
        eprintln!("ffmpeg CLI not installed; skipping synthesized-media test");
        return;
    };
    assert!(status.success());
    let bytes = std::fs::read(&seg).unwrap();
    let f = fixture().await;
    let svc = service_for(&f);
    for i in 100..110 {
        f.routes.set(
            &format!("/seg{i}.ts"),
            Reply::Ok(bytes.clone(), "video/mp2t"),
        );
    }
    f.routes.set("/real.m3u8", Reply::Live);
    let r = run(&svc, "a", req(f.url("/real.m3u8"))).await;
    assert!(r.media_checked, "{r:?}");
    let media = r.stages.iter().find(|s| s.name == "media").unwrap();
    assert!(
        matches!(
            media.status,
            multiview_control::catalogue::probe::StageStatus::Ok
        ),
        "{media:?}"
    );
    assert_eq!(r.facts.video_codec.as_deref(), Some("mpeg2video"));
    assert_eq!((r.facts.width, r.facts.height), (Some(640), Some(360)));
    assert_eq!(r.facts.audio.len(), 1);
    assert_eq!(r.facts.audio[0].sample_rate, 48_000);
    assert!(
        r.facts.measured_bps.is_some_and(|b| b > 100_000),
        "{:?}",
        r.facts.measured_bps
    );
    let thumb = r.facts.thumbnail_jpeg_base64.as_deref().expect("thumbnail");
    use base64::Engine as _;
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(thumb)
        .unwrap();
    assert_eq!(&jpeg[..2], &[0xff, 0xd8], "JPEG SOI");
    assert!(r.verdict_basis.contains("decoded media"));
}

/// Garbage served as a segment: the playlist is fine but nothing decodes ⇒ `no_media`.
#[cfg(feature = "catalogue-ffmpeg")]
#[tokio::test]
async fn undecodable_segments_yield_no_media() {
    let f = fixture().await;
    let svc = service_for(&f);
    for i in 100..110 {
        f.routes.set(
            &format!("/seg{i}.ts"),
            Reply::Ok(vec![0x42; 20_000], "video/mp2t"),
        );
    }
    f.routes.set("/junk.m3u8", Reply::Live);
    let r = run(&svc, "a", req(f.url("/junk.m3u8"))).await;
    assert!(r.media_checked);
    assert_eq!(r.verdict, Some(Verdict::NoMedia), "{r:?}");
}

/// Without the decode feature the probe says so honestly and labels the basis.
#[cfg(not(feature = "catalogue-ffmpeg"))]
#[tokio::test]
async fn without_libav_stage_four_is_skipped_and_labelled() {
    let f = fixture().await;
    let svc = service_for(&f);
    f.routes.set("/live.m3u8", Reply::Live);
    let r = run(&svc, "a", req(f.url("/live.m3u8"))).await;
    assert!(!r.media_checked);
    let media = r.stages.iter().find(|s| s.name == "media").unwrap();
    assert!(matches!(
        media.status,
        multiview_control::catalogue::probe::StageStatus::Skipped
    ));
    assert!(
        r.verdict_basis.contains("stages 1-3 only"),
        "{}",
        r.verdict_basis
    );
}

#[tokio::test]
async fn encrypted_segments_are_not_decoded_and_the_basis_says_why() {
    // A live AES-128 playlist: liveness is judged, media is not decoded, and
    // the stated basis names encryption rather than a build limitation.
    let f = fixture().await;
    let svc = service_for(&f);
    let pl = |seq: u64| {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{seq}\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:1,\na.ts\n"
        )
    };
    f.routes.set(
        "/enc.m3u8",
        Reply::Ok(pl(1).into_bytes(), "application/vnd.apple.mpegurl"),
    );
    let r = run(&svc, "a", req(f.url("/enc.m3u8"))).await;
    assert!(!r.media_checked);
    let media = r.stages.iter().find(|s| s.name == "media").unwrap();
    assert!(matches!(
        media.status,
        multiview_control::catalogue::probe::StageStatus::Skipped
    ));
    assert!(r.verdict_basis.contains("encrypted"), "{}", r.verdict_basis);
    assert!(
        !f.hits.lock().unwrap().iter().any(|(p, _)| p == "/a.ts"),
        "no encrypted segment is fetched"
    );
}
