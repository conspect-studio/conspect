//! A live MPEG-TS source can change its audio stream mid-connection (an ad
//! insert or origin switch moves it to new PIDs) or lose audio while video
//! carries on. Either way the demuxer keeps delivering packets, so no read
//! fails and no end-of-stream arrives: a decoder locked to the old stream index
//! skips them forever and the source goes silent with nothing logged.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_ffmpeg::{AudioFileDecoder, FfmpegError};

const RATE: u32 = 48_000;

/// Write `seconds` of MPEG-TS at PTS offset `offset_s`, on PIDs from `pid`.
fn clip(path: &Path, seconds: u32, offset_s: u32, pid: u32, audio: bool) {
    let dur = seconds.to_string();
    let video = format!("testsrc=size=160x90:rate=25:duration={dur}");
    let tone = format!("sine=frequency=440:sample_rate={RATE}:duration={dur}");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", &video]);
    if audio {
        cmd.args(["-f", "lavfi", "-i", &tone, "-map", "0:v", "-map", "1:a"]);
        cmd.args(["-c:a", "aac", "-ac", "2"]);
    }
    cmd.args(["-c:v", "mpeg2video", "-f", "mpegts"]);
    cmd.args(["-mpegts_start_pid", &pid.to_string()]);
    cmd.args(["-output_ts_offset", &offset_s.to_string()]);
    cmd.arg(path);
    assert!(cmd.status().expect("run ffmpeg").success(), "ffmpeg failed");
}

fn joined(dir: &Path, parts: &[&Path]) -> PathBuf {
    let out = dir.join("joined.ts");
    let bytes: Vec<u8> = parts.iter().flat_map(|p| fs::read(p).unwrap()).collect();
    fs::write(&out, bytes).unwrap();
    out
}

fn decoded_seconds(decoder: &mut AudioFileDecoder) -> Result<f64, FfmpegError> {
    let mut samples = 0usize;
    while let Some(block) = decoder.next_block()? {
        samples += block.frame_count();
    }
    Ok(f64::from(u32::try_from(samples).unwrap()) / f64::from(RATE))
}

/// Audio moves to new PIDs: the decoder follows it to the end.
#[test]
fn live_audio_follows_a_change_of_audio_stream() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.ts"), dir.path().join("b.ts"));
    clip(&a, 4, 10, 256, true);
    clip(&b, 40, 14, 400, true);
    let path = joined(dir.path(), &[&a, &b]);
    let mut decoder = AudioFileDecoder::open_live(&path, RATE, 2).unwrap();
    let seconds = decoded_seconds(&mut decoder).unwrap();
    assert!(
        seconds > 30.0,
        "only {seconds:.2} s decoded: audio on the new stream was skipped"
    );
}

/// Audio stops while video carries on: the connection must end with an error
/// the caller reconnects on, not ride on silently.
#[test]
fn live_audio_reports_starvation_when_only_video_remains() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.ts"), dir.path().join("b.ts"));
    clip(&a, 4, 10, 256, true);
    clip(&b, 60, 14, 256, false);
    let path = joined(dir.path(), &[&a, &b]);
    let mut decoder = AudioFileDecoder::open_live(&path, RATE, 2).unwrap();
    let result = decoded_seconds(&mut decoder);
    assert!(
        matches!(result, Err(FfmpegError::AudioStarved { .. })),
        "expected AudioStarved, got {result:?}"
    );
}

/// A file keeps its old behaviour: `open` never switches streams.
#[test]
fn a_file_open_stays_on_its_first_audio_stream() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.ts"), dir.path().join("b.ts"));
    clip(&a, 4, 10, 256, true);
    clip(&b, 40, 14, 400, true);
    let path = joined(dir.path(), &[&a, &b]);
    let mut decoder = AudioFileDecoder::open(&path, RATE, 2).unwrap();
    let seconds = decoded_seconds(&mut decoder).unwrap();
    assert!(
        seconds < 5.0,
        "a file decode changed behaviour: {seconds:.2} s"
    );
}
