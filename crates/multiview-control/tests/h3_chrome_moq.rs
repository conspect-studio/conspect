//! #382: a real headless Chrome plays `MoQ` from the HTTP/3 front door.
//!
//! The production pieces end to end: [`Runtime::external`] (lease, single-use
//! ticket, fence), [`MoqWt`] on an [`H3Endpoint`], and in the browser the
//! app's own `connectMoq` (which offers every `@moq/net` subprotocol in
//! `wt-available-protocols`, exactly as `moqPlayback.ts`) plus the
//! `@moq/watch` player. The session must negotiate `moqt-16` and paint VP8
//! frames for 15 s (beyond the 10 s header deadline). A second part opens raw `new WebTransport(url, {
//! protocols })` sessions, each with a fresh lease, and requires `ready` with
//! the expected `wt.protocol`.
//!
//! Opt-in (`--features moq-browser-integration`): needs node, `web/`
//! dependencies (`npm --prefix web ci`), Playwright's chromium (or
//! `CONSPECT_WEB_BROWSER`) and ffmpeg with libvpx (or `CONSPECT_WEB_FFMPEG`).
#![cfg(feature = "moq-browser-integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use multiview_control::moq_startup::MoqWt;
use multiview_h3::{EndpointParts, H3Endpoint, H3Limits, WtHandler};
use multiview_moq::av::AvWindow;
use multiview_moq::provider::{Feed, Fence, Runtime};
use multiview_moq::{Packet, Window};
use tokio_rustls::rustls;

struct Cert {
    der: rustls::pki_types::CertificateDer<'static>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

/// P-256, valid 10 days: Chrome pins a `serverCertificateHashes`
/// certificate only when it is ECDSA and valid for at most 14 days.
fn short_cert() -> Cert {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "::1".to_owned()]).unwrap();
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(10);
    let cert = params.self_signed(&key).unwrap();
    Cert {
        der: cert.der().clone(),
        key: rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    }
}

async fn endpoint(cert: &Cert, wt: Arc<dyn WtHandler>) -> (H3Endpoint, SocketAddr) {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der.clone()], cert.key.clone_key())
        .unwrap();
    let socket = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let socket = noq::Runtime::wrap_udp_socket(&noq::TokioRuntime, socket).unwrap();
    let (_signal, watch) = tokio::sync::watch::channel(false);
    let site = Arc::new(multiview_control::h3_site::H3Site::new(
        axum::Router::new(),
        watch,
    ));
    let endpoint = H3Endpoint::start(EndpointParts {
        socket,
        endpoint: noq::EndpointConfig::default(),
        incoming: (64, 4 * 1024 * 1024),
        tls: Arc::new(tls),
        runtime: tokio::runtime::Handle::current(),
        site,
        wt: Some(wt),
        limits: H3Limits::default(),
        peers: None,
        refuse_source: None,
        retry_above_handshakes: None,
        retry_counter: None,
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    (endpoint, addr)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// 2 s of 320x180 VP8 at 30 fps (keyframe every 30), as IVF frame payloads.
fn vp8_frames(dir: &std::path::Path) -> Vec<bytes::Bytes> {
    let ffmpeg = std::env::var_os("CONSPECT_WEB_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let out = dir.join("fixture.ivf");
    let status = std::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30",
        ])
        .args([
            "-t",
            "2",
            "-c:v",
            "libvpx",
            "-g",
            "30",
            "-deadline",
            "realtime",
        ])
        .args(["-f", "ivf"])
        .arg(&out)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg failed");
    let data = std::fs::read(&out).unwrap();
    let header = usize::from(u16::from_le_bytes([data[6], data[7]]));
    let mut at = header;
    let mut frames = Vec::new();
    while at + 12 <= data.len() {
        let size =
            usize::try_from(u32::from_le_bytes(data[at..at + 4].try_into().unwrap())).unwrap();
        at += 12;
        frames.push(bytes::Bytes::copy_from_slice(&data[at..at + size]));
        at += size;
    }
    assert_eq!(frames.len(), 60);
    frames
}

/// Admits one single-rendition lease and feeds it real VP8 at 30 fps,
/// one packet per window, PTS from the tick counter.
fn admit_playing(
    runtime: &Runtime,
    frames: Vec<bytes::Bytes>,
) -> (String, tokio::task::JoinHandle<()>) {
    let (fence, windows) = Fence::channel(8);
    let feed = Feed {
        generation: 1,
        catalog: bytes::Bytes::from_static(
            br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#,
        ),
        audio_tracks: vec![],
        windows,
        demand: Box::new(()),
    };
    let admission = runtime
        .admit(feed, Arc::clone(&fence), Duration::from_secs(60))
        .unwrap();
    let feeder = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_micros(33_333));
        for n in 0u64.. {
            tick.tick().await;
            let payload = frames[usize::try_from(n % 60).unwrap()].clone();
            let packet = Packet {
                verified_rap: payload[0] & 1 == 0,
                payload,
                pts_us: 1_000_000 + n * 1_000_000 / 30,
            };
            let window = AvWindow {
                video: Window {
                    generation: 1,
                    packets: vec![packet],
                },
                audio: std::collections::BTreeMap::new(),
            };
            if fence.try_offer(window).is_err() {
                return;
            }
        }
    });
    (admission.url, feeder)
}

/// The lease body the control API returns (`MoqLease`), for the page's
/// lease API stub.
fn lease_json(url: &str, protocol: &str, n: usize) -> serde_json::Value {
    serde_json::json!({
        "lease_id": format!("lease-{n}"),
        "generation": 1,
        "expires_in_ms": 60_000,
        "url": url,
        "broadcast": "media",
        "protocol": protocol,
        "video": { "codec": "vp8", "coded_width": 320, "coded_height": 180 },
        "audio": [],
        "video_alternatives": [],
    })
}

async fn probe(config: serde_json::Value) -> (bool, Vec<serde_json::Value>) {
    let root = repo_root();
    let node = std::env::var_os("CONSPECT_WEB_NODE").unwrap_or_else(|| "node".into());
    let output = tokio::process::Command::new(node)
        .arg(root.join("web/scripts/wt-chrome-probe.mjs"))
        .arg(config.to_string())
        .current_dir(root.join("web"))
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(180), output)
        .await
        .expect("the browser probe timed out")
        .expect("node runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("probe stdout:\n{stdout}");
    eprintln!("probe stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    let lines = stdout
        .lines()
        .filter(|l| l.starts_with('{'))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (output.status.success(), lines)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chrome_plays_moq_over_the_h3_front_door() {
    // The endpoint's refusal log and the MoQ session log are the evidence
    // when this fails.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,multiview_h3=info,multiview_moq=info")
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let frames = vp8_frames(dir.path());
    let cert = short_cert();
    let runtime = Runtime::external("https://localhost/moq".into(), "moq-transport-16").unwrap();
    let wt: Arc<dyn WtHandler> = Arc::new(MoqWt::new(Arc::clone(&runtime), None).unwrap());
    let (endpoint, addr) = endpoint(&cert, wt).await;
    let at = |url: &str| {
        // The lease URL names the configured host; the browser dials [::1].
        url.replacen(
            "https://localhost/",
            &format!("https://[::1]:{}/", addr.port()),
            1,
        )
    };

    // 1. The app's MoQ playback (`startMoqPlaybackWithRefresh`) for 15 s:
    //    past the 10 s header deadline that used to reset every session.
    let (url, feeder) = admit_playing(&runtime, frames.clone());
    let (ok, lines) = probe(serde_json::json!({
        "certDer": hex(&cert.der),
        "moq": { "leases": [lease_json(&at(&url), "moq-transport-16", 0)], "playMs": 15_000 },
    }))
    .await;
    feeder.abort();
    assert!(ok, "{lines:?}");
    let moq = &lines[0];
    assert_eq!(moq["phase"], "played", "{moq}");
    assert_eq!(moq["error"], serde_json::Value::Null, "{moq}");
    assert_eq!(
        moq["leasesUsed"], 1,
        "one lease, no fallback or retry: {moq}"
    );
    // The browser offered the full @moq/net list and got the configured one.
    let transport = &moq["transports"][0];
    assert_eq!(transport["ready"], true, "{moq}");
    assert_eq!(transport["protocol"], "moqt-16", "{moq}");
    assert!(
        transport["offered"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "moqt-16"),
        "{moq}"
    );
    let in_window = moq["framesInWindow"].as_u64().unwrap();
    // 30 fps for 15 s is 450; a loaded runner may drop paints, never stall.
    assert!(
        in_window >= 225,
        "painted {in_window} frames in 15 s: {moq}"
    );
    assert!(moq["uniqueTimestamps"].as_u64().unwrap() >= 225, "{moq}");

    // 2. Raw WebTransport, one fresh single-use ticket per attempt.
    // `None` expected: refused (400, before the ticket is used).
    let offers: [(Option<&[&str]>, Option<&str>); 4] = [
        // No offer: the moqt-16 runtime cannot run without wt-protocol.
        (None, None),
        (Some(&["moqt-16"]), Some("moqt-16")),
        (
            Some(&[
                "moq-lite-06",
                "moq-lite-05",
                "moqt-18",
                "moqt-16",
                "moqt-15",
            ]),
            Some("moqt-16"),
        ),
        (Some(&["moq-transport-16"]), None),
    ];
    for (offer, expect) in offers {
        let (url, feeder) = admit_playing(&runtime, frames.clone());
        let (ok, lines) = probe(serde_json::json!({
            "url": at(&url),
            "certDer": hex(&cert.der),
            "attempts": [{ "protocols": offer, "expect": expect }],
        }))
        .await;
        feeder.abort();
        assert!(ok, "offer {offer:?}: {lines:?}");
        match expect {
            Some(protocol) => {
                assert_eq!(lines[0]["ok"], true, "{lines:?}");
                assert_eq!(lines[0]["protocol"], protocol, "{lines:?}");
            }
            None => assert_eq!(lines[0]["ok"], false, "{lines:?}"),
        }
    }
    let counters = endpoint.counters();
    assert_eq!(
        multiview_h3::Counters::get(&counters.wt_sessions),
        3,
        "{counters:?}"
    );
    assert_eq!(
        multiview_h3::Counters::get(&counters.wt_refused),
        2,
        "{counters:?}"
    );
    assert_eq!(
        multiview_h3::Counters::get(&counters.header_deadline_rejections),
        0,
        "{counters:?}"
    );
    drop(dir);
    endpoint.shutdown().await;
}
