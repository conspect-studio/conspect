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
#[path = "common/broadcast_fixture.rs"]
mod broadcast_fixture;

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
fn probe_media(path: &Path) -> (String, Vec<i64>, usize) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type:frame=best_effort_timestamp,media_type",
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
    let streams = json["streams"].as_array().unwrap();
    let codec = streams
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .and_then(|stream| stream["codec_name"].as_str())
        .unwrap_or_default()
        .to_owned();
    let audio = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio" && stream["codec_name"] == "aac")
        .count();
    let pts = json["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| frame["media_type"] == "video")
        .filter_map(|frame| frame["best_effort_timestamp"].as_i64())
        .collect();
    (codec, pts, audio)
}
#[tokio::test]
async fn live_clock_survives_slow_ipv4_peer_and_ipv6_reconnect() {
    // A live 30s run (not run_for): the hot producer must not await one peer.
    let dir = tempfile::tempdir().unwrap();
    let clips = broadcast_fixture::paths(dir.path());
    broadcast_fixture::generate(&clips, 35);
    let playlist = dir.path().join("hls/index.m3u8");
    let port = 30_000 + u16::try_from(std::process::id() % 9_000).unwrap();
    let config = MultiviewConfig::load_from_toml(&broadcast_fixture::config(
        &clips,
        &playlist,
        port,
        port + 1,
    ))
    .unwrap();
    config.validate().unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    pipeline.enable_program_audio();
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
    let (codec, pts, audio_count) = probe_media(&healthy_path);
    let mut faults = Vec::new();
    if report.frames < 850 {
        faults.push(format!("hot clock only {} ticks /30s", report.frames));
    }
    if report.faltered {
        faults.push(format!("hot clock dropped {} ticks", report.dropped));
    }
    if codec != "h264" {
        faults.push(format!("IPv6 healthy codec {codec} !=h264"));
    }
    if audio_count != 5 {
        faults.push(format!("IPv6 healthy AAC tracks {audio_count} !=5"));
    }
    if pts.len() <= 150 {
        faults.push(format!("IPv6 healthy decoded frames {} <=150", pts.len()));
    }
    let mut sorted = pts;
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[1] - pair[0] != 3003 {
            faults.push(format!(
                "IPv6 healthy video PTS delta {} !=3003",
                pair[1] - pair[0]
            ));
            break;
        }
    }
    if !playlist.exists() {
        faults.push("HLS unavailable during slow peer".to_owned());
    }
    assert!(
        faults.is_empty(),
        "live mixed-output acceptance failed:\n{}",
        faults.join("\n")
    );
}
