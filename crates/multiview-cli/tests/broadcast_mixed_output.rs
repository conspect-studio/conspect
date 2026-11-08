//! Real pipeline acceptance for mixed-codec HLS + two SRT listeners.
//! Run locally with `--features ffmpeg,srt-libsrt,gpl-codecs` (GPL only in test
//! build); deploy host uses `--features nvidia` (LGPL hardware encoders).
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
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn fixture(path: &Path, frequency: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=size=320x180:rate=30000/1001:duration=15")
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={frequency}:sample_rate=48000:duration=15"
        ))
        .args([
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("generate deterministic source");
    assert!(status.success());
}
fn config(
    clips: &[std::path::PathBuf; 6],
    playlist: &Path,
    port_hevc: u16,
    port_avc: u16,
) -> String {
    format!(
        r##"
schema_version = 1
[canvas]
width = 320
height = 180
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "broadcast_fixture"
kind = "file"
path = "{clip0}"
[[sources]]
id = "tone_1"
kind = "file"
path = "{clip1}"
[[sources]]
id = "tone_2"
kind = "file"
path = "{clip2}"
[[sources]]
id = "tone_3"
kind = "file"
path = "{clip3}"
[[sources]]
id = "tone_4"
kind = "file"
path = "{clip4}"
[[sources]]
id = "tone_5"
kind = "file"
path = "{clip5}"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "broadcast_fixture"
[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "h264"
segment_ms = 1000
[outputs.audio]
mode = "tracks"
tracks = ["track-1", "track-2", "track-3", "track-4", "track-5"]
[[outputs]]
kind = "srt"
url = "srt://[::]:{port_hevc}?mode=listener"
codec = "hevc"
[outputs.audio]
mode = "tracks"
tracks = ["track-1", "track-2", "track-3", "track-4", "track-5"]
[[outputs]]
kind = "srt"
url = "srt://[::]:{port_avc}?mode=listener"
codec = "h264"
[outputs.audio]
mode = "tracks"
tracks = ["track-1", "track-2", "track-3", "track-4", "track-5"]
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "broadcast_fixture"
channels = {{ kind = "stereo" }}
include_in_program_bus = true
[[audio.routes]]
input_id = "tone_1"
channels = {{ kind = "stereo" }}
target_track = "track-1"
language = "eng"
include_in_program_bus = false
[[audio.routes]]
input_id = "tone_2"
channels = {{ kind = "stereo" }}
target_track = "track-2"
language = "eng"
include_in_program_bus = false
[[audio.routes]]
input_id = "tone_3"
channels = {{ kind = "stereo" }}
target_track = "track-3"
language = "eng"
include_in_program_bus = false
[[audio.routes]]
input_id = "tone_4"
channels = {{ kind = "stereo" }}
target_track = "track-4"
language = "eng"
include_in_program_bus = false
[[audio.routes]]
input_id = "tone_5"
channels = {{ kind = "stereo" }}
target_track = "track-5"
language = "eng"
include_in_program_bus = false
"##,
        clip0 = clips[0].display(),
        clip1 = clips[1].display(),
        clip2 = clips[2].display(),
        clip3 = clips[3].display(),
        clip4 = clips[4].display(),
        clip5 = clips[5].display(),
        playlist = playlist.display()
    )
}
/// Decode the selected AAC stream to PCM, not merely inspect headers.
fn decoded_audio_samples(path: &Path, stream_index: usize) -> Result<usize, String> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-probesize",
            "2000000",
            "-analyzeduration",
            "5000000",
            "-i",
        ])
        .arg(path)
        .args(["-map"])
        .arg(format!("0:a:{stream_index}"))
        .args(["-t", "3", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
        .output()
        .expect("decode AAC PCM");
    if !output.status.success() {
        return Err(format!(
            "AAC stream {stream_index} decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output.stdout.len() / 4)
}
fn probe_audio_count(path: &Path) -> usize {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-probesize",
            "2000000",
            "-analyzeduration",
            "5000000",
            "-show_entries",
            "stream=index,codec_type,codec_name",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("probe AAC streams");
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ffprobe audio JSON");
    value["streams"].as_array().map_or(0, Vec::len)
}
fn probe_frames(path: &Path) -> (String, Vec<i64>) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name:frame=best_effort_timestamp",
            "-show_frames",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let codec = value["streams"][0]["codec_name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let pts = value["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|frame| frame["best_effort_timestamp"].as_i64())
        .collect();
    (codec, pts)
}
/// The output PTS uses an exact 90 kHz clock: 30000/1001 fps = 3003 ticks.
/// B-frame decode order can differ, so compare sorted presentation timestamps.
fn cadence_faults(pts: &[i64], label: &str, minimum: usize) -> Vec<String> {
    let mut faults = Vec::new();
    if pts.len() < minimum {
        faults.push(format!(
            "{label}: only {} decoded frames, need {minimum}",
            pts.len()
        ));
    }
    let mut sorted = pts.to_vec();
    sorted.sort_unstable();
    for (index, window) in sorted.windows(2).enumerate() {
        if window[1] - window[0] != 3003 {
            faults.push(format!(
                "{label}: frame {index} PTS delta {} != 3003",
                window[1] - window[0]
            ));
            if faults.len() >= 12 {
                break;
            }
        }
    }
    faults
}

#[tokio::test]
async fn mixed_codec_pipeline_receives_decodable_media_not_just_headers() {
    const TICKS: u64 = 300; // 10s diagnostic @ exact 30000/1001.
    let dir = tempfile::tempdir().unwrap();
    let clips: [std::path::PathBuf; 6] =
        std::array::from_fn(|index| dir.path().join(format!("source{index}.ts")));
    for (index, clip) in clips.iter().enumerate() {
        fixture(clip, 440 + u32::try_from(index).unwrap() * 110);
    }
    let playlist = dir.path().join("hls/index.m3u8");
    let base = 25_000 + u16::try_from(std::process::id() % 10_000).unwrap();
    let config =
        MultiviewConfig::load_from_toml(&config(&clips, &playlist, base, base + 1)).unwrap();
    config.validate().unwrap();
    let mut pipeline = Pipeline::build(&config)
        .expect("three bindings must build without first-codec substitution");
    pipeline.enable_program_audio();
    let hevc_path = dir.path().join("hevc.ts");
    let avc_path = dir.path().join("avc.ts");
    let receive = |path: std::path::PathBuf, port: u16| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            Command::new("timeout")
                .args([
                    "16",
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-i",
                ])
                .arg(format!(
                    "srt://[::1]:{port}?mode=caller&connect_timeout=5000"
                ))
                .args(["-t", "6", "-c", "copy", "-f", "mpegts"])
                .arg(&path)
                .status()
                .expect("receive SRT")
        })
    };
    let h = receive(hevc_path.clone(), base);
    let a = receive(avc_path.clone(), base + 1);
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("paced output pipeline");
    assert_eq!(report.frames, TICKS);
    assert!(!report.faltered, "output clock must not falter");
    assert!(
        h.join().unwrap().success(),
        "HEVC SRT peer must receive actual bytes"
    );
    assert!(
        a.join().unwrap().success(),
        "H264 SRT peer must receive actual bytes"
    );
    let (hevc_codec, hevc_pts) = probe_frames(&hevc_path);
    let (avc_codec, avc_pts) = probe_frames(&avc_path);
    let mut faults = cadence_faults(&hevc_pts, "HEVC SRT", 100);
    for (label, path) in [("HEVC SRT", &hevc_path), ("H264 SRT", &avc_path)] {
        let count = probe_audio_count(path);
        if count != 5 {
            faults.push(format!("{label}: {count} AAC tracks != 5"));
        }
        for index in 0..count.min(5) {
            match decoded_audio_samples(path, index) {
                Ok(samples) if samples >= 48_000 => {}
                Ok(samples) => faults.push(format!(
                    "{label}: AAC track {index} only {samples} decoded PCM samples"
                )),
                Err(error) => faults.push(format!("{label}: {error}")),
            }
        }
    }
    faults.extend(cadence_faults(&avc_pts, "H264 SRT", 100));
    if hevc_codec != "hevc" {
        faults.push(format!("HEVC SRT codec {hevc_codec} != hevc"));
    }
    if avc_codec != "h264" {
        faults.push(format!("H264 SRT codec {avc_codec} != h264"));
    }
    if !playlist.exists() {
        faults.push("HLS manifest missing".to_owned());
    }
    let segment = dir.path().join("hls/seg0.ts");
    if segment.exists() {
        let (hls_codec, hls_pts) = probe_frames(&segment);
        if hls_codec != "h264" {
            faults.push(format!("HLS codec {hls_codec} != h264"));
        }
        faults.extend(cadence_faults(&hls_pts, "H264 HLS segment", 10));
    } else {
        faults.push("HLS first segment missing".to_owned());
    }
    assert!(
        faults.is_empty(),
        "mixed-output acceptance failed:\n{}",
        faults.join("\n")
    );
}
