//! End-to-end test for GAP 3: **real subtitles + real audio meter** reach the
//! composited program (features `ffmpeg` + `overlay`).
//!
//! Two things are proven against real on-disk artifacts (no tautology):
//!
//! 1. **Subtitle burn-in.** The same pipeline is run twice over an identical
//!    clip — once with an SRT track attached, once without — and a frame is
//!    extracted at the *same* media time from each, while the cue is active.
//!    The bottom-centre subtitle band differs between the two runs (the
//!    burned-in cue is the only change), while a control band elsewhere is
//!    byte-identical. The cue text reaches actual output pixels.
//! 2. **Real audio meter.** The dB meter is driven from the decoded program
//!    audio (multiview-audio's ballistics DSP), not a constant. A clip with a loud
//!    1 kHz tone fills the meter track with opaque green far more than an
//!    otherwise-identical clip with a silent track — the on-screen bar reflects
//!    the real audio. We measure the meter track's green dominance (G − R),
//!    which the green fill raises and the underlying video does not.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_overlay::subtitle::CueTrack;

/// Generate a 2-second `testsrc` clip with an audio track. `tone` selects a
/// 1 kHz sine at full level; otherwise the audio is silence. Both encode video
/// with LGPL `mpeg2video` (+ `mp2` audio) into an MPEG-TS container.
fn generate_clip_with_audio(path: &Path, tone: bool) {
    let audio_in = if tone {
        "sine=frequency=1000:sample_rate=48000:duration=2"
    } else {
        "anullsrc=sample_rate=48000:channel_layout=stereo:duration=2"
    };
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=2",
            "-f",
            "lavfi",
            "-i",
            audio_in,
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-c:a",
            "mp2",
            "-shortest",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the input clip");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(path.exists(), "input clip was not written");
}

/// Extract a single PNG frame at `at_secs` of `video` into `png`. The seek is an
/// **output** seek (`-ss` after `-i`) so it decodes to the exact instant on a
/// short MPEG-TS with sparse keyframes rather than snapping to a keyframe.
fn extract_frame(video: &Path, at_secs: f64, png: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video)
        .arg("-ss")
        .arg(format!("{at_secs}"))
        .args(["-frames:v", "1"])
        .arg(png)
        .status()
        .expect("spawn ffmpeg to extract a frame");
    assert!(status.success(), "frame extraction failed");
    assert!(png.exists(), "extracted frame PNG was not written");
}

/// Decode a PNG to raw rgb24 with `ffmpeg`, returning `(width, height, bytes)`.
fn decode_rgb24(png: &Path) -> (u32, u32, Vec<u8>) {
    let dims = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(png)
        .output()
        .expect("ffprobe dims");
    let dims = String::from_utf8_lossy(&dims.stdout);
    let dims = dims.trim();
    let (w_str, h_str) = dims.split_once('x').expect("WxH dims");
    let w: u32 = w_str.trim().parse().expect("width");
    let h: u32 = h_str.trim().parse().expect("height");

    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(png)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .output()
        .expect("ffmpeg decode rgb24");
    assert!(out.status.success(), "ffmpeg rgb24 decode failed");
    let bytes = out.stdout;
    let expect_len = usize::try_from(w).unwrap() * usize::try_from(h).unwrap() * 3;
    assert_eq!(bytes.len(), expect_len, "rgb24 buffer size mismatch");
    (w, h, bytes)
}

/// A rectangular region of an rgb24 frame.
struct Region {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

/// The byte offset of pixel `(x, y)` in a `width`-wide rgb24 buffer (no `as`).
fn px_index(x: u32, y: u32, width: u32) -> usize {
    let x = usize::try_from(x).unwrap();
    let y = usize::try_from(y).unwrap();
    let width = usize::try_from(width).unwrap();
    (y * width + x) * 3
}

/// Mean of `f` over every pixel `(r, g, b)` in `region` of the rgb24 `png`.
fn region_mean(png: &Path, region: &Region, f: impl Fn(u8, u8, u8) -> f64) -> f64 {
    let (w, h, rgb) = decode_rgb24(png);
    let x1 = region.x1.min(w);
    let y1 = region.y1.min(h);
    if region.x0 >= x1 || region.y0 >= y1 {
        return 0.0;
    }
    let mut sum = 0.0_f64;
    let mut count = 0.0_f64;
    for y in region.y0..y1 {
        for x in region.x0..x1 {
            let i = px_index(x, y, w);
            sum += f(rgb[i], rgb[i + 1], rgb[i + 2]);
            count += 1.0;
        }
    }
    if count == 0.0 {
        0.0
    } else {
        sum / count
    }
}

/// Mean absolute per-pixel rgb difference between two frames over `region`.
fn region_mean_absdiff(a: &Path, b: &Path, region: &Region) -> f64 {
    let (wa, ha, ra) = decode_rgb24(a);
    let (wb, hb, rb) = decode_rgb24(b);
    assert_eq!((wa, ha), (wb, hb), "frames differ in size");
    let x1 = region.x1.min(wa);
    let y1 = region.y1.min(ha);
    let _ = hb;
    let mut sum = 0.0_f64;
    let mut count = 0.0_f64;
    for y in region.y0..y1 {
        for x in region.x0..x1 {
            let i = px_index(x, y, wa);
            for k in 0..3 {
                sum += f64::from(ra[i + k].abs_diff(rb[i + k]));
                count += 1.0;
            }
        }
    }
    if count == 0.0 {
        0.0
    } else {
        sum / count
    }
}

/// The config: one real file source filling the canvas, with an HLS output
/// requesting LGPL `mpeg2video`.
fn config_text(clip: &Path, out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 480
fps = "25/1"
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
id = "in_a"
kind = "file"
path = "{clip}"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        clip = clip.display(),
        playlist = out_playlist.display(),
    )
}

/// Run the real pipeline over `clip`, optionally with `subtitles`, returning the
/// produced `program.ts` path (the run's tempdir is returned to keep it alive).
async fn run_pipeline(
    base: &Path,
    name: &str,
    clip: &Path,
    subtitles: Option<CueTrack>,
    ticks: u64,
) -> PathBuf {
    let out_dir = base.join(name);
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    if let Some(track) = subtitles {
        pipeline = pipeline.with_subtitles(track);
    }
    let report = pipeline.run_for(ticks).await.expect("bounded run");
    assert_eq!(report.frames, ticks, "N ticks must produce N frames");
    assert!(!report.faltered, "GAP-3 wiring must not falter the output");

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written for {name}");
    program
}

#[tokio::test]
async fn subtitle_cue_burns_into_the_program() {
    const TICKS: u64 = 50; // 2 s @ 25 fps.

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip_with_audio(&clip, true);

    // A cue visible only from 1.0s..1.8s.
    let srt = "1\n00:00:01,000 --> 00:00:01,800\nMULTIVIEW SUBTITLE TEST\n";
    let cues = CueTrack::parse_srt(srt).expect("parse srt");
    assert_eq!(cues.len(), 1, "one cue parsed");

    // Identical pipeline, with vs without the subtitle track.
    let with_sub = run_pipeline(dir.path(), "with_sub", &clip, Some(cues), TICKS).await;
    let no_sub = run_pipeline(dir.path(), "no_sub", &clip, None, TICKS).await;

    // Extract the SAME media instant (1.3s, mid-cue) from both runs.
    let frame_on = dir.path().join("frame_on.png");
    let frame_off = dir.path().join("frame_off.png");
    extract_frame(&with_sub, 1.3, &frame_on);
    extract_frame(&no_sub, 1.3, &frame_off);

    // The bottom-centre subtitle band differs between the runs (the burned-in
    // cue is the only change); a control band higher up is byte-identical.
    let band = Region {
        x0: 120,
        y0: 380,
        x1: 520,
        y1: 440,
    };
    let control = Region {
        x0: 120,
        y0: 120,
        x1: 520,
        y1: 180,
    };
    let band_diff = region_mean_absdiff(&frame_on, &frame_off, &band);
    let control_diff = region_mean_absdiff(&frame_on, &frame_off, &control);

    assert!(
        control_diff < 0.01,
        "the control band must be identical between runs (got {control_diff:.4}); \
         the underlying video is the same, only the burned-in cue should differ"
    );
    assert!(
        band_diff > 2.0,
        "the burned-in subtitle must change the bottom-centre band \
         (band diff {band_diff:.3} vs control {control_diff:.3})"
    );
}

#[tokio::test]
async fn audio_meter_reflects_real_program_loudness() {
    const TICKS: u64 = 50;

    let dir = tempfile::tempdir().expect("tempdir");
    let tone_clip = dir.path().join("tone.ts");
    let silent_clip = dir.path().join("silent.ts");
    generate_clip_with_audio(&tone_clip, true);
    generate_clip_with_audio(&silent_clip, false);

    let tone_prog = run_pipeline(dir.path(), "tone", &tone_clip, None, TICKS).await;
    let silent_prog = run_pipeline(dir.path(), "silent", &silent_clip, None, TICKS).await;

    // Sample late (1.5s): the peak meter is well established by then.
    let tone_png = dir.path().join("tone.png");
    let silent_png = dir.path().join("silent.png");
    extract_frame(&tone_prog, 1.5, &tone_png);
    extract_frame(&silent_prog, 1.5, &silent_png);

    // The meter track sits down the right edge (x in [w-28, w-12], full vertical
    // extent). The green fill raises green-over-red dominance (G − R) where the
    // bar is filled; the underlying video does not. A loud tone fills far more of
    // the track than silence, so its green dominance is markedly higher.
    let meter = Region {
        x0: 640 - 28,
        y0: 40,
        x1: 640 - 12,
        y1: 480 - 40,
    };
    let green_dominance = |r: u8, g: u8, _b: u8| f64::from(g) - f64::from(r);
    let tone_gd = region_mean(&tone_png, &meter, green_dominance);
    let silent_gd = region_mean(&silent_png, &meter, green_dominance);

    assert!(
        tone_gd > silent_gd + 20.0,
        "a loud tone must fill the meter (green) more than silence \
         (tone G−R {tone_gd:.2} vs silent G−R {silent_gd:.2})"
    );
}
