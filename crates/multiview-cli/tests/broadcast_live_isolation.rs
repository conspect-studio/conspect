//! Live-clock and slow-peer isolation acceptance, separate from finite output.
#![cfg(all(
    feature = "ffmpeg",
    feature = "srt-libsrt",
    any(feature = "gpl-codecs", feature = "nvidia")
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_engine::StopSignal;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

/// Always resume a suspended test-owned child before kill/wait, even on panic.
struct OwnedClient(Child);
impl Drop for OwnedClient {
    fn drop(&mut self) {
        let pid = self.0.id().to_string();
        let _ = Command::new("kill").args(["-CONT", &pid]).status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fixture(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=30000/1001:duration=35",
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}
fn config(clip: &Path, playlist: &Path, slow: u16, healthy: u16) -> String {
    format!(
        r##"
schema_version=1
[canvas]
width=320
height=180
fps="30000/1001"
pixel_format="nv12"
background="#101014"
[canvas.color]
profile="sdr-bt709-limited"
[layout]
kind="grid"
columns=["1fr"]
rows=["1fr"]
areas=["a"]
[[sources]]
id="isolated_source"
kind="file"
path="{clip}"
[[cells]]
id="cell_a"
area="a"
[cells.source]
input_id="isolated_source"
[[outputs]]
kind="hls"
path="{playlist}"
codec="h264"
segment_ms=1000
[[outputs]]
kind="srt"
url="srt://[::]:{slow}?mode=listener"
codec="hevc"
[[outputs]]
kind="srt"
url="srt://[::]:{healthy}?mode=listener"
codec="h264"
"##,
        clip = clip.display(),
        playlist = playlist.display()
    )
}
fn decode_pts(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=best_effort_timestamp",
            "-show_frames",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    json["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|frame| frame["best_effort_timestamp"].as_i64())
        .collect()
}
#[tokio::test]
async fn live_clock_survives_slow_ipv4_peer_and_ipv6_reconnect() {
    // A live 30s run (not run_for): the hot producer must not await one peer.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.ts");
    fixture(&source);
    let playlist = dir.path().join("hls/index.m3u8");
    let port = 30_000 + u16::try_from(std::process::id() % 9_000).unwrap();
    let config =
        MultiviewConfig::load_from_toml(&config(&source, &playlist, port, port + 1)).unwrap();
    config.validate().unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    let stop = StopSignal::new();
    let stopper = stop.clone();
    let stopped = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        stopper.stop();
    });
    let slow = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        // Attach over IPv4, then suspend the OWN ffmpeg process after it has
        // opened its input. A stopped reader holds the SRT socket without
        // draining it. The child PID is scoped to this test only.
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(format!(
                "srt://127.0.0.1:{port}?mode=caller&connect_timeout=5000"
            ))
            .args(["-map", "0:v:0", "-f", "null", "-"])
            .spawn()
            .unwrap();
        let child = OwnedClient(child);
        std::thread::sleep(Duration::from_secs(3));
        let pid = child.0.id().to_string();
        let stopped = Command::new("kill").args(["-STOP", &pid]).status().unwrap();
        assert!(stopped.success(), "stop only owned SRT client");
        std::thread::sleep(Duration::from_secs(8));
        let resumed = Command::new("kill").args(["-CONT", &pid]).status().unwrap();
        assert!(resumed.success(), "resume owned SRT client for cleanup");
        drop(child);
    });
    let healthy_path = dir.path().join("healthy.ts");
    let healthy = std::thread::spawn({
        let path = healthy_path.clone();
        move || {
            std::thread::sleep(Duration::from_secs(4));
            Command::new("timeout")
                .args([
                    "--signal=INT",
                    "14s",
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-i",
                ])
                .arg(format!(
                    "srt://[::1]:{}?mode=caller&connect_timeout=5000",
                    port + 1
                ))
                .args(["-map", "0", "-c", "copy", "-f", "mpegts"])
                .arg(path)
                .status()
                .unwrap()
        }
    });
    let report = pipeline.run_until(&stop).await.unwrap();
    stopped.join().unwrap();
    let _ = slow.join().unwrap();
    let _ = healthy.join().unwrap();
    assert!(
        report.frames >= 850,
        "30s live clock produced {} frames, expected ~899",
        report.frames
    );
    assert!(
        !report.faltered,
        "live hot clock shed {} frames",
        report.dropped
    );
    let pts = decode_pts(&healthy_path);
    assert!(
        pts.len() > 150,
        "healthy IPv6 peer decoded only {} frames",
        pts.len()
    );
    let mut sorted = pts;
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        assert_eq!(pair[1] - pair[0], 3003, "healthy peer video gap");
    }
    assert!(playlist.exists(), "HLS remained available during slow peer");
}
