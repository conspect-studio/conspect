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
fn frame_bytes(index: i64, key: bool, sets: bool) -> Vec<u8> {
    let tag = format!("{index:016x}").into_bytes();
    let mut nals: Vec<Vec<u8>> = vec![vec![0x09, 0xF0]];
    if key {
        if sets {
            nals.push(SPS.to_vec());
            nals.push(PPS.to_vec());
        }
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
    /// Keyframes carry no SPS / PPS (with no extradata the packager then
    /// fails: it cannot build an init segment).
    no_sets: bool,
    /// When set: the wall clock (Unix ns) of media time zero; each packet
    /// arrives at its media time on that clock.
    wall_at_zero: Option<i64>,
}

impl Stream {
    const fn new(audio: bool) -> Self {
        Self {
            next_audio: 0,
            audio,
            no_sets: false,
            wall_at_zero: None,
        }
    }

    /// Feed video frame `index` and the audio frames that start before the
    /// end of it.
    fn frame(&mut self, runner: &mut CastRunner, index: i64, now: Instant) {
        let key = index % GOP == 0;
        let data = frame_bytes(index, key, !self.no_sets);
        let unix_ns = self
            .wall_at_zero
            .map_or_else(unix_nanos, |zero| zero + frame_ns(index));
        runner.handle_at(
            CastInput {
                kind: CastInputKind::Video,
                pts: index,
                dts: index,
                keyframe: key,
                config_changed: false,
                data: &data,
            },
            now,
            unix_ns,
        );
        if !self.audio {
            return;
        }
        // Video frame `i` ends at (i + 1) * 1601.6 samples.
        let limit = (index + 1) * 1001 * 48_000 / 30_000;
        while self.next_audio * 1024 < limit {
            let data = aac_frame(self.next_audio);
            runner.handle_at(
                CastInput {
                    kind: CastInputKind::Audio,
                    pts: self.next_audio * 1024,
                    dts: self.next_audio * 1024,
                    keyframe: false,
                    config_changed: false,
                    data: &data,
                },
                now,
                unix_ns,
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

/// Nanoseconds of media at video frame `index`.
fn frame_ns(index: i64) -> i64 {
    index * 1001 * 1_000_000_000 / 30_000
}

#[test]
fn inactive_writes_nothing_and_a_preroll_activation_publishes_a_full_window_at_once() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    let gate = Arc::new(CastGate::new());
    prepare_cast_dir(&dir, "cast-1", &gate);
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    let now = Instant::now();
    // 23 s of run with no cast: not even the directory exists.
    stream.run(&mut runner, 0..690, now);
    assert!(!dir.exists(), "an inactive cast output touches no disk");
    assert!(!gate.status().ready);

    // Activate with pre-roll: the very next packet publishes the window.
    let generation = gate.activate(true).unwrap();
    stream.frame(&mut runner, 690, now);
    let status = gate.status();
    assert!(status.active && status.ready, "{status:?}");
    assert_eq!(status.generation, generation);
    assert!((9..=10).contains(&status.video_segments), "{status:?}");
    assert_eq!(status.audio_segments, status.video_segments);
    let report = validated(&dir);
    assert!((9..=10).contains(&report.video_segments), "{report:?}");
    assert_eq!(report.audio_segments, report.video_segments);
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.2");
    assert_eq!(report.discontinuities, 0);
    assert_eq!(listed_generation(&dir), generation);
    // The window starts 20 to 21 s before the activation, at an IDR.
    let behind = frame_ns(690) as f64 / 1e9 - report.first_video_start;
    assert!((19.9..=21.1).contains(&behind), "{behind}");
    assert!(dir.join("master.m3u8").is_file() && dir.join(CAST_DIR_MARKER).is_file());

    // The stream continues seamlessly from the pre-roll.
    stream.run(&mut runner, 691..1000, now);
    let report = validated(&dir);
    assert_eq!((report.video_segments, report.discontinuities), (10, 0));
    assert!(report.media_sequence > 0, "the window slides");
}

#[test]
fn without_preroll_the_stream_starts_at_the_first_keyframe_after_the_activation() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    let gate = Arc::new(CastGate::new());
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    let now = Instant::now();
    stream.run(&mut runner, 0..695, now);
    gate.activate(false).unwrap();
    stream.run(&mut runner, 695..760, now);
    // Nothing of the buffered 23 s is used: no segment is closed yet.
    assert!(!gate.status().ready);
    assert_eq!(gate.status().video_segments, 0);
    stream.run(&mut runner, 760..1100, now);
    let report = validated(&dir);
    // The first keyframe at or after frame 695 is frame 720.
    let first_frame = report.first_video_start * 30_000.0 / 1001.0;
    assert!((first_frame - 720.0).abs() < 0.01, "{first_frame}");
    assert!(
        gate.status().ready && report.video_segments >= 5,
        "{report:?}"
    );
    assert_eq!(report.discontinuities, 0);
}

#[test]
fn deactivation_empties_the_directory_and_a_new_activation_is_a_new_generation() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    let gate = Arc::new(CastGate::new());
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    let now = Instant::now();
    stream.run(&mut runner, 0..300, now);
    let first = gate.activate(true).unwrap();
    stream.run(&mut runner, 300..600, now);
    validated(&dir);
    assert_eq!(listed_generation(&dir), first);
    std::fs::write(dir.join("notes.txt"), b"keep").unwrap();

    // Deactivate: the next packet removes every stream file (and only those).
    gate.deactivate();
    stream.frame(&mut runner, 600, now);
    assert_eq!(files(&dir), ["notes.txt"]);
    assert!(dir.join(CAST_DIR_MARKER).is_file());
    assert!(!gate.status().active && !gate.status().ready);
    // With a quiet feed the idle poll does the same.
    let second = gate.activate(false).unwrap();
    stream.run(&mut runner, 601..800, now);
    assert!(files(&dir).len() > 4);
    gate.deactivate();
    runner.poll();
    assert_eq!(files(&dir), ["notes.txt"]);
    assert!(second > first);

    // Re-activate with pre-roll: the ring refilled while inactive.
    stream.run(&mut runner, 800..1500, now);
    let third = gate.activate(true).unwrap();
    assert!(third > second);
    stream.frame(&mut runner, 1500, now);
    let report = validated(&dir);
    assert!(gate.status().ready && (9..=10).contains(&report.video_segments));
    assert_eq!(listed_generation(&dir), third);
    assert_eq!(report.discontinuities, 0);
    // A deactivation and a new activation between two packets (the sink
    // never saw the inactive state) is still a new stream.
    gate.deactivate();
    let fourth = gate.activate(false).unwrap();
    stream.run(&mut runner, 1501..1900, now);
    let report = validated(&dir);
    assert_eq!(listed_generation(&dir), fourth);
    let first_frame = report.first_video_start * 30_000.0 / 1001.0;
    assert!(first_frame >= 1501.0, "{first_frame}");
    let names = files(&dir);
    assert!(
        !names
            .iter()
            .any(|name| name.contains(&format!("-{third}-"))),
        "{names:?}"
    );
    // End of run: nothing of the stream is left.
    runner.finish();
    assert_eq!(files(&dir), ["notes.txt"]);
    assert_eq!(gate.activate(true), Err(CastGateError::Ended));
}

/// Unix milliseconds of an `EXT-X-PROGRAM-DATE-TIME` value
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub(crate) fn pdt_unix_ms(text: &str) -> i64 {
    let number = |range: std::ops::Range<usize>| -> i64 { text[range].parse().unwrap() };
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + number(11..13) * 3_600 + number(14..16) * 60 + number(17..19);
    seconds * 1_000 + number(20..23)
}

/// `(PDT in Unix ms, EXTINF in ms)` of the last segment `playlist` lists.
pub(crate) fn last_segment_pdt(dir: &Path, playlist: &str) -> (i64, i64) {
    let text = std::fs::read_to_string(dir.join(playlist)).unwrap();
    let pdt = text
        .lines()
        .filter_map(|l| l.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
        .next_back()
        .map(pdt_unix_ms)
        .expect("a PDT line");
    let extinf: f64 = text
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .next_back()
        .and_then(|l| l.trim_end_matches(',').parse().ok())
        .expect("an EXTINF line");
    #[allow(clippy::cast_possible_truncation)]
    (pdt, (extinf * 1000.0).round() as i64)
}

#[test]
fn a_preroll_stream_is_dated_by_arrival_time_with_and_without_an_epoch() {
    assert_eq!(pdt_unix_ms("1970-01-01T00:00:01.250Z"), 1_250);
    assert_eq!(pdt_unix_ms("2026-01-02T03:04:05.000Z"), 1_767_323_045_000);
    // The wall clock of media time zero.
    let wall_at_zero = 1_767_323_045_000_000_000_i64;
    for with_epoch in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cast-1");
        let gate = Arc::new(CastGate::new());
        let epoch = SharedEpoch::new();
        if with_epoch {
            epoch.set(WallClockRef::new(
                wall_at_zero,
                0,
                Rational::new(1_000_000_000, 1),
            ));
        }
        let mut runner = CastRunner::new(setup(&dir, &gate, epoch, true));
        let mut stream = Stream::new(true);
        stream.wall_at_zero = Some(wall_at_zero);
        let now = Instant::now();
        stream.run(&mut runner, 0..690, now);
        gate.activate(true).unwrap();
        stream.frame(&mut runner, 690, now);
        let report = validated(&dir);
        assert!(report.has_program_date_time);
        // Frame 690 arrived "now". The last listed segment ends at a GOP
        // boundary at most one open segment (< 1.5 s here) before it, and
        // its date is the arrival time of that boundary: 20 s of buffered
        // packets are not stamped with the activation time.
        let wall_now_ms = (wall_at_zero + frame_ns(690)) / 1_000_000;
        for playlist in ["video.m3u8", "audio.m3u8"] {
            let (pdt, duration) = last_segment_pdt(&dir, playlist);
            let behind = wall_now_ms - (pdt + duration);
            assert!(
                (-50..=1_500).contains(&behind),
                "{playlist} epoch={with_epoch}: live edge {behind} ms behind the wall clock"
            );
            if playlist == "video.m3u8" {
                // A whole number of GOPs (1001 ms), to the millisecond.
                let off = behind
                    .rem_euclid(1_001)
                    .min(1_001 - behind.rem_euclid(1_001));
                assert!(
                    off <= 3,
                    "epoch={with_epoch}: {behind} ms is not on a GOP boundary"
                );
            }
        }
        // The first segment is dated ~20 s in the past, never in the future.
        let text = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
        let first = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .map(pdt_unix_ms)
            .unwrap();
        assert!(
            (19_900..=21_100).contains(&(wall_now_ms - first)),
            "{first}"
        );
    }
}

#[test]
fn lost_packets_clear_the_preroll_and_are_declared_as_a_discontinuity() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    let gate = Arc::new(CastGate::new());
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    let now = Instant::now();
    // Inactive: 10 s buffered, then the delivery drops packets.
    stream.run(&mut runner, 0..300, now);
    gate.dropped_counter().fetch_add(40, Ordering::AcqRel);
    stream.skip_to(340);
    stream.run(&mut runner, 340..500, now);
    gate.activate(true).unwrap();
    stream.frame(&mut runner, 500, now);
    stream.run(&mut runner, 501..640, now);
    let report = validated(&dir);
    // Nothing from before the gap is in the stream: it starts at the first
    // IDR after it (frame 360), with no discontinuity inside.
    let first_frame = report.first_video_start * 30_000.0 / 1001.0;
    assert!((first_frame - 360.0).abs() < 0.01, "{first_frame}");
    assert_eq!(report.discontinuities, 0);

    // Active: a drop is declared in the stream and the directory validates.
    gate.dropped_counter().fetch_add(25, Ordering::AcqRel);
    stream.skip_to(665);
    stream.run(&mut runner, 665..1000, now);
    let report = validated(&dir);
    assert_eq!(report.discontinuities, 1, "{report:?}");
    assert_eq!(gate.status().dropped_packets, 65);
    assert!(gate.status().ready);
    assert_eq!(gate.status().restarts, 0, "a gap is not an error");
}

#[test]
fn a_packager_error_restarts_a_new_generation_at_most_once_per_second() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("cast-1");
    let gate = Arc::new(CastGate::new());
    let mut runner = CastRunner::new(setup(&dir, &gate, SharedEpoch::new(), true));
    let mut stream = Stream::new(true);
    stream.no_sets = true;
    let start = Instant::now();
    let generation = gate.activate(false).unwrap();
    // No parameter sets anywhere: the packager gives up after 300 units.
    // 60 s of stream at real-time spacing.
    for index in 0..1800 {
        let now = start + Duration::from_millis(u64::try_from(index).unwrap() * 1001 / 30);
        stream.frame(&mut runner, index, now);
    }
    let status = gate.status();
    assert!(status.active && !status.ready, "{status:?}");
    let error = status.last_error.clone().unwrap();
    assert!(error.contains("no H.264 parameter sets"), "{error}");
    // Each failure takes 300 video units (10 s) to show: bounded.
    assert!((1..=7).contains(&status.restarts), "{status:?}");
    assert_eq!(
        files(&dir),
        Vec::<String>::new(),
        "a failed stream leaves no files"
    );

    // With the clock standing still nothing restarts at all.
    let frozen = start + Duration::from_secs(61);
    let before = gate.status().restarts;
    // (one restart may be pending from the last failure)
    for index in 1800..4800 {
        stream.frame(&mut runner, index, frozen);
    }
    assert!(gate.status().restarts <= before + 1, "{:?}", gate.status());

    // The stream heals: the next restart publishes under a NEW generation.
    stream.no_sets = false;
    let later = frozen + Duration::from_secs(5);
    stream.run(&mut runner, 4800..5300, later);
    let report = validated(&dir);
    let status = gate.status();
    assert!(status.ready && report.video_segments >= 5, "{status:?}");
    assert_eq!(status.generation, generation, "the activation is the same");
    assert!(listed_generation(&dir) > generation, "new file names");
    assert_eq!(report.discontinuities, 0);
}
