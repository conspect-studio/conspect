//! Real encoded HLS survival across an over-budget overlay frame.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::too_many_lines, clippy::format_push_string)] // Integration fixture builds TOML and inspects all three real sink renditions.

use std::{path::Path, process::Command, time::Duration};

use multiview_cli::{live_overlays::publish_set, pipeline::Pipeline};
use multiview_compositor::overlay::subpass::overlay_fallback_counts;
use multiview_config::MultiviewConfig;
use multiview_overlay::subtitle::CueTrack;

fn probe(path: &Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .args(["-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe executable");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("ffprobe JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoded_hls_and_three_audio_renditions_survive_complex_overlay() {
    const TICKS: u64 = 40;
    let dir = tempfile::tempdir().expect("temporary fixture");
    let playlist = dir.path().join("index.m3u8");
    let mut config = String::from(
        "schema_version = 1\n[canvas]\nwidth = 640\nheight = 360\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#101014\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"grid\"\ncolumns = [\"1fr\", \"1fr\", \"1fr\"]\nrows = [\"1fr\", \"1fr\"]\nareas = [\"a b c\", \"d e f\"]\n",
    );
    for id in ["a", "b", "c", "d", "e", "f"] {
        config.push_str(&format!("[[sources]]\nid = \"{id}\"\nkind = \"bars\"\n[[cells]]\nid = \"cell_{id}\"\narea = \"{id}\"\n[cells.source]\ninput_id = \"{id}\"\n"));
    }
    config.push_str(&format!(
        "[[outputs]]\nkind = \"hls\"\npath = \"{}\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n[outputs.audio]\nmode = \"auto\"\n[audio]\nsample_rate_hz = 48000\n",
        playlist.display()
    ));
    for (id, track) in [("a", ""), ("b", "track-b"), ("c", "track-c")] {
        config.push_str(&format!(
            "[[audio.routes]]\ninput_id = \"{id}\"\nchannels = {{ kind = \"stereo\" }}\n"
        ));
        if !track.is_empty() {
            config.push_str(&format!("target_track = \"{track}\"\n"));
        }
    }
    let config = MultiviewConfig::load_from_toml(&config).expect("fixture config");
    config.validate().expect("valid fixture");
    let cue = CueTrack::parse_srt("1\n00:00:00,000 --> 00:00:03,000\nCAPTION PROOF\n")
        .expect("caption track");
    let mut pipeline = Pipeline::build(&config)
        .expect("native pipeline")
        .with_subtitles(cue);
    pipeline.enable_program_audio();
    let slot = pipeline.overlay_apply_slot();
    let fallback_before = overlay_fallback_counts();
    let controller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let clocks: Vec<_> = (0..80)
            .map(|i| {
                serde_json::from_value(serde_json::json!({
                    "id": format!("complex_{i}"), "kind": "clock", "target": "canvas",
                    "face": "analog", "x": 320, "y": 180, "radius": 8
                }))
                .expect("valid overlay")
            })
            .collect();
        publish_set(&slot, clocks);
        std::thread::sleep(Duration::from_millis(400));
        publish_set(&slot, Vec::new());
    });
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("native encode and HLS sink survive");
    controller.join().expect("overlay controller");
    let fallback_after = overlay_fallback_counts();
    let over_budget = fallback_after[0] - fallback_before[0];
    assert!(over_budget > 0 && over_budget < TICKS, "ordinary frames must bracket a real complex fallback: {fallback_before:?} -> {fallback_after:?}");
    assert_eq!(report.frames, TICKS, "output tick contract");
    assert!(!report.faltered, "no output ticks may falter");
    let program = dir.path().join("program.ts");
    let frames = probe(
        &program,
        &[
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames:frame=pts",
            "-show_frames",
        ],
    );
    let video: Vec<_> = frames["frames"]
        .as_array()
        .expect("decoded frames")
        .iter()
        .filter(|f| f["media_type"] == "video")
        .collect();
    assert_eq!(
        video.len(),
        usize::try_from(TICKS).expect("test tick count fits usize"),
        "encoded video must reach native sink"
    );
    let pts: Vec<i64> = video
        .iter()
        .map(|f| f["pts"].as_i64().expect("encoded PTS"))
        .collect();
    assert!(
        pts.windows(2).all(|pair| pair[1] - pair[0] == 3600),
        "25fps tick PTS must remain exact: {pts:?}"
    );
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).expect("HLS master");
    assert_eq!(
        master.matches("#EXT-X-MEDIA:TYPE=AUDIO").count(),
        3,
        "program plus two discrete renditions remain in the native sink: {master}"
    );
    for line in master
        .lines()
        .filter(|line| line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO"))
    {
        let uri = line
            .split("URI=\"")
            .nth(1)
            .and_then(|part| part.split('"').next())
            .expect("audio rendition URI");
        let media =
            std::fs::read_to_string(dir.path().join(uri)).expect("audio rendition playlist");
        let segment = media
            .lines()
            .find(|line| {
                Path::new(line)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
            })
            .expect("audio segment");
        let streams = probe(
            &dir.path().join(segment),
            &["-show_entries", "stream=codec_type"],
        );
        assert!(
            streams["streams"]
                .as_array()
                .expect("streams")
                .iter()
                .any(|s| s["codec_type"] == "audio"),
            "audio rendition must contain encoded packets: {uri}"
        );
    }
}
