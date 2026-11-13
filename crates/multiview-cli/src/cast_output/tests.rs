//! Unit tests of the cast sink without an encoder: the gate, the directory
//! ownership rule and the runner driven by a synthetic H.264 + AAC stream.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_output::hls::cast::{CastAudioConfig, CastVideoConfig};
use multiview_output::hls::cast_check::{check_cast_dir, CastCheckReport};
use multiview_output::SharedEpoch;

use super::runner::{CastInput, CastInputKind, CastRunner, CastSetup};
use super::{
    claim_cast_dir, inspect_cast_dir, prepare_cast_dir, unix_nanos, CastDirState, CastGate,
    CastGateError, CAST_DIR_MARKER,
};

const SPS: [u8; 27] = [
    0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
];
const PPS: [u8; 6] = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
const VIDEO_TB: Rational = Rational::new(1001, 30_000);
const AUDIO_TB: Rational = Rational::new(1, 48_000);
/// Frames per GOP of the synthetic stream (1.001 s).
const GOP: i64 = 30;

/// One Annex B access unit: AUD, on a keyframe SPS + PPS + IDR, else a
/// non-IDR slice. The payload is ASCII so it never emulates a start code.
fn frame_bytes(index: i64, key: bool) -> Vec<u8> {
    let tag = format!("{index:016x}").into_bytes();
    let mut nals: Vec<Vec<u8>> = vec![vec![0x09, 0xF0]];
    if key {
        nals.push(SPS.to_vec());
        nals.push(PPS.to_vec());
        let mut idr = vec![0x65, 0x88, 0x84];
        idr.extend_from_slice(&tag);
        idr.extend(std::iter::repeat_n(0x5A, 600));
        nals.push(idr);
    } else {
        let mut slice = vec![0x41, 0x9A];
        slice.extend_from_slice(&tag);
        slice.extend(std::iter::repeat_n(0x33, 120));
        nals.push(slice);
    }
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&nal);
    }
    out
}

fn aac_frame(index: i64) -> Vec<u8> {
    let mut payload = vec![0x21, 0x10];
    payload.extend_from_slice(&index.to_be_bytes());
    payload.extend(std::iter::repeat_n(0x77, 150));
    payload
}

fn setup(dir: &Path, gate: &Arc<CastGate>, epoch: SharedEpoch, audio: bool) -> CastSetup {
    CastSetup {
        id: "cast-1".to_owned(),
        dir: dir.to_path_buf(),
        gate: Arc::clone(gate),
        epoch,
        video: CastVideoConfig {
            time_base: VIDEO_TB,
            width: 1920,
            height: 1080,
            bit_rate: Some(4_000_000),
            extradata: None,
        },
        audio: audio.then_some(CastAudioConfig {
            time_base: AUDIO_TB,
            sample_rate: 48_000,
            channels: 2,
            bit_rate: Some(128_000),
            audio_specific_config: None,
        }),
    }
}

/// A synthetic 29.97 fps / 48 kHz stream fed to a runner.
struct Stream {
    next_audio: i64,
    audio: bool,
    /// Never send a keyframe (to make the packager fail).
    no_keyframes: bool,
}

impl Stream {
    const fn new(audio: bool) -> Self {
        Self {
            next_audio: 0,
            audio,
            no_keyframes: false,
        }
    }

    /// Feed video frame `index` and the audio frames that start before the
    /// end of it.
    fn frame(&mut self, runner: &mut CastRunner, index: i64, now: Instant) {
        let key = index % GOP == 0 && !self.no_keyframes;
        let data = frame_bytes(index, key);
        runner.handle(
            CastInput {
                kind: CastInputKind::Video,
                pts: index,
                dts: index,
                keyframe: key,
                config_changed: false,
                data: &data,
            },
            now,
        );
        if !self.audio {
            return;
        }
        // Video frame `i` ends at (i + 1) * 1601.6 samples.
        let limit = (index + 1) * 1001 * 48_000 / 30_000;
        while self.next_audio * 1024 < limit {
            let data = aac_frame(self.next_audio);
            runner.handle(
                CastInput {
                    kind: CastInputKind::Audio,
                    pts: self.next_audio * 1024,
                    dts: self.next_audio * 1024,
                    keyframe: false,
                    config_changed: false,
                    data: &data,
                },
                now,
            );
            self.next_audio += 1;
        }
    }

    fn run(&mut self, runner: &mut CastRunner, range: std::ops::Range<i64>, now: Instant) {
        for index in range {
            self.frame(runner, index, now);
        }
    }

    /// Skip to video frame `index` (packets were lost in between).
    fn skip_to(&mut self, index: i64) {
        self.next_audio = index * 1001 * 48_000 / 30_000 / 1024 + 1;
    }
}

/// The file names in `dir`, sorted, without the ownership marker.
fn files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != CAST_DIR_MARKER)
        .collect();
    names.sort();
    names
}

fn validated(dir: &Path) -> CastCheckReport {
    let report = check_cast_dir(dir);
    assert_eq!(report.errors, Vec::<String>::new(), "{:?}", files(dir));
    report
}

/// The first `seg-v-<gen>-<n>.m4s` generation listed by `video.m3u8`.
fn listed_generation(dir: &Path) -> u64 {
    let text = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
    let line = text.lines().find(|l| l.starts_with("seg-v-")).unwrap();
    line.split('-').nth(2).unwrap().parse().unwrap()
}

#[test]
fn the_gate_activates_once_per_stream_with_a_new_generation_each_time() {
    let gate = CastGate::new();
    assert_eq!(gate.activation(), None);
    assert!(!gate.status().active && gate.status().generation == 0);
    let first = gate.activate(true).unwrap();
    assert!(first > 0);
    assert_eq!(gate.activation(), Some((first, true)));
    // Idempotent while active: same generation, the pre-roll choice stays.
    assert_eq!(gate.activate(false).unwrap(), first);
    assert_eq!(gate.activation(), Some((first, true)));
    gate.deactivate();
    gate.deactivate();
    assert_eq!(gate.activation(), None);
    let second = gate.activate(false).unwrap();
    assert!(second > first, "{second} > {first}");
    assert_eq!(gate.activation(), Some((second, false)));
    // Progress belongs to one generation only.
    gate.begin_generation(second);
    gate.publish_progress(5, 5, true);
    let status = gate.status();
    assert!(status.ready && status.video_segments == 5 && status.generation == second);
    gate.deactivate();
    let third = gate.activate(true).unwrap();
    let status = gate.status();
    assert!(!status.ready && status.video_segments == 0 && status.generation == third);
    // Generations are unique across gates too (file names never collide).
    let other = CastGate::new().activate(true).unwrap();
    assert!(other > third);
    // A disabled or ended gate refuses, with the reason.
    gate.disable("cast output \"x\": no".to_owned());
    assert_eq!(gate.activation(), None);
    assert_eq!(
        gate.activate(true),
        Err(CastGateError::Disabled("cast output \"x\": no".to_owned()))
    );
    let ended = CastGate::new();
    ended.end();
    assert_eq!(ended.activate(true), Err(CastGateError::Ended));
}

#[test]
fn a_directory_is_used_only_when_absent_empty_or_marked_with_this_id() {
    let root = tempfile::tempdir().unwrap();
    // Absent: created 0700 and marked.
    let fresh = root.path().join("state/cast/cast-1");
    assert_eq!(inspect_cast_dir(&fresh, "cast-1"), Ok(CastDirState::Absent));
    assert!(!fresh.exists(), "inspecting creates nothing");
    claim_cast_dir(&fresh, "cast-1").unwrap();
    assert_eq!(
        std::fs::read_to_string(fresh.join(CAST_DIR_MARKER)).unwrap(),
        "cast-1"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    assert_eq!(inspect_cast_dir(&fresh, "cast-1"), Ok(CastDirState::Owned));
    claim_cast_dir(&fresh, "cast-1").unwrap();
    // Empty: marked.
    let empty = root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert_eq!(inspect_cast_dir(&empty, "cast-1"), Ok(CastDirState::Empty));
    claim_cast_dir(&empty, "cast-1").unwrap();
    assert!(empty.join(CAST_DIR_MARKER).is_file());
    // Another output's marker, foreign files, a file, a symlinked marker:
    // refused with the fix in the text, and nothing is written.
    let fix = "holds files this engine did not write; point `dir` at an empty directory or \
               remove `dir`";
    let err = claim_cast_dir(&fresh, "cast-2").unwrap_err();
    assert!(
        err.contains("cast output \"cast-2\"") && err.contains(fix),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join(CAST_DIR_MARKER)).unwrap(),
        "cast-1"
    );
    let used = root.path().join("recordings");
    std::fs::create_dir(&used).unwrap();
    std::fs::write(used.join("holiday.mp4"), b"keep").unwrap();
    let err = claim_cast_dir(&used, "cast-1").unwrap_err();
    assert!(
        err.contains(fix) && err.contains(&used.display().to_string()),
        "{err}"
    );
    assert_eq!(files(&used), ["holiday.mp4"]);
    assert!(!used.join(CAST_DIR_MARKER).exists());
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(claim_cast_dir(&file, "cast-1").unwrap_err().contains(fix));
    #[cfg(unix)]
    {
        let linked = root.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(root.path().join("marker-target"), b"cast-1").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("marker-target"),
            linked.join(CAST_DIR_MARKER),
        )
        .unwrap();
        assert!(claim_cast_dir(&linked, "cast-1").unwrap_err().contains(fix));
        // A symlink to a directory is not a cast directory either.
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&empty, &alias).unwrap();
        assert!(claim_cast_dir(&alias, "cast-1").unwrap_err().contains(fix));
    }
}

#[test]
fn a_foreign_directory_disables_the_output_and_nothing_in_it_is_touched() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("videos");
    std::fs::create_dir(&dir).unwrap();
    // Names the packager itself would write, owned by someone else.
    let foreign = ["holiday.mp4", "master.m3u8", "seg-v-1-0.m4s", "video.m3u8"];
    for name in foreign {
        std::fs::write(dir.join(name), b"keep").unwrap();
    }
    let gate = Arc::new(CastGate::new());
    // Start of run.
    prepare_cast_dir(&dir, "cast-1", &gate);
    assert!(gate.is_disabled());
    let reason = gate.status().last_error.unwrap();
    assert!(
        reason.contains("cast output \"cast-1\"")
            && reason.contains("holds files this engine did not write"),
        "{reason}"
    );
    // Activate is refused with the same text; the runner writes nothing.
    assert_eq!(gate.activate(true), Err(CastGateError::Disabled(reason)));
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    stream.run(&mut runner, 0..300, Instant::now());
    gate.deactivate();
    stream.run(&mut runner, 300..330, Instant::now());
    runner.poll();
    // End of run.
    runner.finish();
    assert_eq!(files(&dir), foreign);
    for name in foreign {
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"keep");
    }
    assert!(!dir.join(CAST_DIR_MARKER).exists(), "no marker was written");
}

#[test]
fn a_marked_directory_is_purged_of_stale_stream_files_only() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    claim_cast_dir(&dir, "cast-1").unwrap();
    let stale = [
        "master.m3u8",
        "video.m3u8",
        "audio.m3u8",
        "seg-v-3-0.m4s",
        "seg-a-3-0.m4s",
        "init-v-3-0.mp4",
        "init-a-3-0.mp4",
        "seg-v-3-1.m4s.tmp",
    ];
    for name in stale {
        std::fs::write(dir.join(name), b"stale").unwrap();
    }
    std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
    std::fs::write(dir.join("holiday.mp4"), b"keep").unwrap();
    let gate = Arc::new(CastGate::new());
    prepare_cast_dir(&dir, "cast-1", &gate);
    assert!(!gate.is_disabled());
    assert_eq!(files(&dir), ["holiday.mp4", "notes.txt"]);
    assert!(dir.join(CAST_DIR_MARKER).is_file(), "the marker stays");

    // The same directory under another output's id: disabled, untouched.
    for name in stale {
        std::fs::write(dir.join(name), b"stale").unwrap();
    }
    let other = Arc::new(CastGate::new());
    prepare_cast_dir(&dir, "cast-2", &other);
    assert!(other.is_disabled());
    assert!(other.activate(false).is_err());
    assert_eq!(files(&dir).len(), stale.len() + 2);
    assert_eq!(
        std::fs::read_to_string(dir.join(CAST_DIR_MARKER)).unwrap(),
        "cast-1"
    );
    // A directory that stops being this output's between the start of the
    // run and an activation is not written to either.
    let gate = Arc::new(CastGate::new());
    let late = root.path().join("late");
    prepare_cast_dir(&late, "cast-1", &gate);
    assert!(!gate.is_disabled() && !late.exists());
    std::fs::create_dir(&late).unwrap();
    std::fs::write(late.join("holiday.mp4"), b"keep").unwrap();
    let mut runner = CastRunner::new(setup(&late, &gate, SharedEpoch::new(), true));
    gate.activate(false).unwrap();
    Stream::new(true).run(&mut runner, 0..300, Instant::now());
    assert!(gate.is_disabled() && gate.activation().is_none());
    assert_eq!(files(&late), ["holiday.mp4"]);
    assert!(!late.join(CAST_DIR_MARKER).exists());
}
