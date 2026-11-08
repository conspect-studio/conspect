//! Shared six-source, five-discrete-track fixture for real output acceptance.
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn generate(clips: &[PathBuf; 6], seconds: u32) {
    for (index, clip) in clips.iter().enumerate() {
        let frequency = 440 + u32::try_from(index).expect("six clips") * 110;
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
            .arg(format!(
                "testsrc=size=320x180:rate=30000/1001:duration={seconds}"
            ))
            .args(["-f", "lavfi", "-i"])
            .arg(format!(
                "sine=frequency={frequency}:sample_rate=48000:duration={seconds}"
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
            .arg(clip)
            .status()
            .expect("generate distinct tone clip");
        assert!(status.success(), "fixture clip {index}");
    }
}
pub(crate) fn paths(dir: &Path) -> [PathBuf; 6] {
    std::array::from_fn(|index| dir.join(format!("source{index}.ts")))
}
pub(crate) fn config(clips: &[PathBuf; 6], playlist: &Path, hevc: u16, avc: u16) -> String {
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
url = "srt://[::]:{hevc}?mode=listener"
codec = "hevc"
[outputs.audio]
mode = "tracks"
tracks = ["track-1", "track-2", "track-3", "track-4", "track-5"]
[[outputs]]
kind = "srt"
url = "srt://[::]:{avc}?mode=listener"
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
