//! The Cast packager (`hls::cast`) and its validator (`hls::cast_check`):
//! synthetic H.264 access units and AAC frames in, a directory of CMAF HLS
//! out, read back the way a player reads it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_output::hls::cast::{
    purge_dir, AudioUnit, CastAudioConfig, CastPackager, CastPackagerConfig, CastVideoConfig,
    VideoUnit,
};
use multiview_output::hls::cast_check::{check_cast_dir, CastCheckReport};
use multiview_output::hls::cmaf;
use multiview_output::hls::{
    AudioRendition, MasterPlaylist, MediaPlaylist, Segment, SegmentType, VariantStream,
};
use multiview_output::SharedEpoch;

const SPS: [u8; 27] = [
    0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
];
/// A second SPS: Main profile, level 3.1 (`avc1.4D401F`).
const SPS_B: [u8; 10] = [0x67, 0x4D, 0x40, 0x1F, 0xEC, 0xA0, 0x50, 0x17, 0xFC, 0xB8];
const PPS: [u8; 6] = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

/// How the synthetic stream is framed.
#[derive(Clone, Copy, PartialEq)]
enum Framing {
    AnnexB,
    Avcc,
}

fn frame_nals(index: i64, key: bool, sps: Option<&[u8]>) -> Vec<Vec<u8>> {
    // ASCII hex: no zero bytes, so the payload never emulates a start code.
    let tag = format!("{index:016x}").into_bytes();
    let mut nals = vec![vec![0x09, 0xF0]];
    if key {
        if let Some(sps) = sps {
            nals.push(sps.to_vec());
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
    nals
}

fn frame_bytes(index: i64, key: bool, sps: Option<&[u8]>, framing: Framing) -> Vec<u8> {
    let mut out = Vec::new();
    for (n, nal) in frame_nals(index, key, sps).iter().enumerate() {
        match framing {
            Framing::AnnexB if n % 2 == 0 => out.extend_from_slice(&[0, 0, 0, 1]),
            Framing::AnnexB => out.extend_from_slice(&[0, 0, 1]),
            Framing::Avcc => {
                out.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
            }
        }
        out.extend_from_slice(nal);
    }
    out
}

fn aac_frame(index: i64, adts: bool) -> Vec<u8> {
    let mut payload = vec![0x21, 0x10];
    payload.extend_from_slice(&index.to_be_bytes());
    payload.extend(std::iter::repeat_n(0x77, 150));
    if !adts {
        return payload;
    }
    // AAC-LC, 48 kHz, stereo, no CRC.
    let len = payload.len() + 7;
    let mut out = vec![
        0xFF,
        0xF1,
        0x4C,
        0x80 | u8::try_from(len >> 11).unwrap(),
        u8::try_from((len >> 3) & 0xFF).unwrap(),
        u8::try_from((len & 7) << 5).unwrap() | 0x1F,
        0xFC,
    ];
    out.extend_from_slice(&payload);
    out
}

const VIDEO_TB: Rational = Rational::new(1001, 30_000);
const AUDIO_TB: Rational = Rational::new(1, 48_000);

fn config(dir: &Path, audio: bool) -> CastPackagerConfig {
    CastPackagerConfig {
        dir: dir.to_path_buf(),
        generation: 7,
        window: 10,
        target_segment_ns: 2_000_000_000,
        video: CastVideoConfig {
            time_base: VIDEO_TB,
            width: 1920,
            height: 1080,
            bit_rate: Some(6_000_000),
            extradata: None,
        },
        audio: audio.then_some(CastAudioConfig {
            time_base: AUDIO_TB,
            sample_rate: 48_000,
            channels: 2,
            bit_rate: Some(192_000),
            audio_specific_config: None,
        }),
        epoch: None,
    }
}

/// Drives a packager with a synthetic 29.97 fps / 48 kHz stream.
struct Feeder {
    packager: CastPackager,
    framing: Framing,
    adts: bool,
    gop: i64,
    /// SPS carried in-band on keyframes (`None`: no in-band sets).
    sps: Option<Vec<u8>>,
    next_audio: i64,
    audio: bool,
    /// The frame index whose unit reports `config_changed`.
    config_changed_at: Option<i64>,
}

impl Feeder {
    fn new(config: CastPackagerConfig) -> Self {
        let audio = config.audio.is_some();
        Self {
            packager: CastPackager::new(config).unwrap(),
            framing: Framing::AnnexB,
            adts: false,
            gop: 30,
            sps: Some(SPS.to_vec()),
            next_audio: 0,
            audio,
            config_changed_at: None,
        }
    }

    fn video(&mut self, index: i64) -> multiview_output::Result<()> {
        let key = index % self.gop == 0;
        let data = frame_bytes(index, key, self.sps.as_deref(), self.framing);
        self.packager.push_video(VideoUnit {
            pts: index,
            dts: index,
            keyframe: key,
            config_changed: self.config_changed_at == Some(index),
            data: &data,
        })
    }

    fn audio_frame(&mut self, index: i64) -> multiview_output::Result<()> {
        let data = aac_frame(index, self.adts);
        self.packager.push_audio(AudioUnit {
            pts: index * 1024,
            data: &data,
        })
    }

    /// Push the audio frames that start before `video_index` plus a skew
    /// (in audio frames, may be negative): audio arrives a little early or
    /// a little late relative to video.
    fn audio_up_to(&mut self, video_index: i64, skew: i64) -> multiview_output::Result<()> {
        if !self.audio {
            return Ok(());
        }
        // Video frame `i` starts at i * 1001 / 30000 s = i * 1601.6 samples.
        let limit = video_index * 1001 * 48_000 / 30_000 + skew * 1024;
        while self.next_audio * 1024 < limit {
            self.audio_frame(self.next_audio)?;
            self.next_audio += 1;
        }
        Ok(())
    }

    /// Feed video frames `range` with audio interleaved, alternating between
    /// audio running two frames ahead and two frames behind.
    fn run(&mut self, range: std::ops::Range<i64>) -> multiview_output::Result<()> {
        for index in range {
            let skew = if (index / 45) % 2 == 0 { 2 } else { -2 };
            self.audio_up_to(index, skew)?;
            self.video(index)?;
        }
        Ok(())
    }
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Temporary files left behind (there must never be any).
fn temporaries(dir: &Path) -> Vec<String> {
    files(dir)
        .into_iter()
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "tmp"))
        .collect()
}

fn count(dir: &Path, prefix: &str) -> usize {
    files(dir).iter().filter(|n| n.starts_with(prefix)).count()
}

/// `baseMediaDecodeTime` of a media segment file.
fn tfdt(dir: &Path, name: &str) -> u64 {
    let bytes = std::fs::read(dir.join(name)).unwrap();
    let tfdt = cmaf::find_box(&bytes, &[b"moof", b"traf", b"tfdt"]).unwrap();
    u64::from_be_bytes(tfdt[4..12].try_into().unwrap())
}

/// The segment URIs a media playlist lists, in order.
fn uris(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn starting_mid_stream_writes_nothing_before_the_first_keyframe() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    // Frames 31..60 are non-keyframes; audio for that span arrives too.
    for index in 31..60 {
        feeder.audio_up_to(index, 2).unwrap();
        feeder.video(index).unwrap();
    }
    assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));
    feeder.next_audio = 31 * 1001 * 48 / 30 / 1024;
    feeder.run(60..60 + 30 * 9).unwrap();
    let names = files(dir.path());
    assert!(names.contains(&"master.m3u8".to_owned()), "{names:?}");
    assert_eq!(temporaries(dir.path()), Vec::<String>::new());

    let video = read(dir.path(), "video.m3u8");
    let first = &uris(&video)[0];
    assert_eq!(first, "seg-v-7-0.m4s");
    // The first keyframe is frame 60: tfdt = 60 * 1001 in the 30000 timescale.
    assert_eq!(tfdt(dir.path(), first), 60 * 1001);
    let audio = read(dir.path(), "audio.m3u8");
    let first_audio = tfdt(dir.path(), &uris(&audio)[0]);
    // The first audio frame at or after 2.002 s: ceil(96096 / 1024) * 1024.
    assert_eq!(first_audio, 94 * 1024);
    // Keyframes 60, 120, .., 300 arrived: four segments are closed.
    assert_eq!(feeder.packager.published().video_segments, 4);
    assert_eq!(feeder.packager.published().audio_segments, 4);
    assert!(feeder.packager.is_ready());
}

/// Package the same content with the given framing; return every file.
fn package(framing: Framing, adts: bool) -> Vec<(String, Vec<u8>)> {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.framing = framing;
    feeder.adts = adts;
    feeder.run(0..300).unwrap();
    files(dir.path())
        .into_iter()
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext != "m3u8"))
        .map(|name| {
            let bytes = std::fs::read(dir.path().join(&name)).unwrap();
            (name, bytes)
        })
        .collect()
}

#[test]
fn avcc_and_adts_input_produce_byte_identical_segments() {
    let reference = package(Framing::AnnexB, false);
    assert!(reference.len() >= 8, "{}", reference.len());
    let other = package(Framing::Avcc, true);
    assert_eq!(reference.len(), other.len());
    for (a, b) in reference.iter().zip(&other) {
        assert_eq!(a.0, b.0);
        assert!(a.1 == b.1, "{} differs", a.0);
    }
}

#[test]
fn purge_dir_removes_only_the_packager_file_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
    std::fs::write(dir.path().join("stale.m4s.tmp"), b"x").unwrap();
    std::fs::create_dir(dir.path().join("sub.mp4")).unwrap();
    assert!(files(dir.path()).len() > 8);
    purge_dir(dir.path());
    assert_eq!(files(dir.path()), ["keep.txt", "sub.mp4"]);

    // remove_files deletes exactly what one generation wrote.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..900).unwrap();
    std::fs::write(dir.path().join("seg-v-8-0.m4s"), b"other generation").unwrap();
    feeder.packager.remove_files();
    assert_eq!(files(dir.path()), ["seg-v-8-0.m4s"]);

    // A newer generation that took over the directory keeps its playlists.
    let dir = tempfile::tempdir().unwrap();
    let mut old = Feeder::new(config(dir.path(), true));
    old.run(0..200).unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.generation = 8;
    let mut new = Feeder::new(cfg);
    new.run(0..200).unwrap();
    old.packager.remove_files();
    let left = files(dir.path());
    assert!(!left.iter().any(|name| name.contains("-7-")), "{left:?}");
    assert!(read(dir.path(), "video.m3u8").contains("seg-v-8-0.m4s"));
    let report = check_cast_dir(dir.path());
    assert_eq!(report.errors, Vec::<String>::new());
}

/// Validate the directory; panic with the findings when it has errors.
fn validate(dir: &Path) -> CastCheckReport {
    let report = check_cast_dir(dir);
    assert!(
        report.errors.is_empty(),
        "validator errors: {:#?}\nvideo.m3u8:\n{}\naudio.m3u8:\n{}",
        report.errors,
        std::fs::read_to_string(dir.join("video.m3u8")).unwrap_or_default(),
        std::fs::read_to_string(dir.join("audio.m3u8")).unwrap_or_default(),
    );
    report
}

fn extinfs(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter_map(|line| line.strip_prefix("#EXTINF:"))
        .map(|value| value.trim_end_matches(',').to_owned())
        .collect()
}

#[test]
fn thirty_seconds_of_2997_with_audio_is_a_valid_cast_stream() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    // 30 s: 900 frames; the validator must pass at every publish on the way.
    for chunk in 0..15 {
        feeder.run(chunk * 60..(chunk + 1) * 60).unwrap();
        if feeder.packager.published().master {
            validate(dir.path());
        }
    }
    feeder.run(900..901).unwrap();
    let report = validate(dir.path());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.video_segments, 10);
    assert_eq!(report.audio_segments, 10);
    assert_eq!(report.target_duration, 2);
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.2");
    assert_eq!(report.media_sequence, 5, "15 segments closed, 10 listed");
    assert_eq!(report.discontinuities, 0);
    assert!(report.has_audio && report.has_program_date_time);
    // The longest segment is an audio one of 94 frames: 94 * 1024 / 48000.
    assert!(
        (report.max_extinf - 2.005_333).abs() < 1e-6,
        "{}",
        report.max_extinf
    );
    assert!(feeder.packager.is_ready());
    assert_eq!(feeder.packager.published().video_segments, 15);
    assert_eq!(feeder.packager.published().audio_segments, 15);

    let video = read(dir.path(), "video.m3u8");
    assert!(
        video.starts_with(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:5\n\
         #EXT-X-MAP:URI=\"init-v-7-0.mp4\"\n#EXT-X-PROGRAM-DATE-TIME:"
        ),
        "{video}"
    );
    assert_eq!(extinfs(&video), vec!["2.002000"; 10]);
    assert_eq!(uris(&video)[0], "seg-v-7-5.m4s");
    assert_eq!(uris(&video)[9], "seg-v-7-14.m4s");
    assert_eq!(video.matches("#EXT-X-MAP").count(), 1);
    let audio = read(dir.path(), "audio.m3u8");
    assert_eq!(uris(&audio)[0], "seg-a-7-5.m4s");
    assert_eq!(
        audio.matches("#EXT-X-MAP:URI=\"init-a-7-0.mp4\"").count(),
        1
    );
    // 2.002 s = 93.84 AAC frames: segments hold 93 or 94 frames.
    for extinf in extinfs(&audio) {
        assert!(extinf == "2.005333" || extinf == "1.984000", "{extinf}");
    }
    for text in [&video, &audio] {
        assert!(!text.contains("EXT-X-DISCONTINUITY"), "{text}");
        assert!(!text.contains("EXT-X-ENDLIST"), "{text}");
    }
    let master = read(dir.path(), "master.m3u8");
    assert_eq!(
        master,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Program\",DEFAULT=YES,AUTOSELECT=YES,\
         CHANNELS=\"2\",URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=6192000,AVERAGE-BANDWIDTH=6192000,\
         CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=29.970,AUDIO=\"aud\"\n\
         video.m3u8\n"
    );
    // Retention: the listed window plus one more window.
    assert_eq!(count(dir.path(), "seg-v-"), 15);
    assert!(count(dir.path(), "seg-v-") <= 22 && count(dir.path(), "seg-a-") <= 22);
    assert_eq!(count(dir.path(), "init-"), 2);
    assert_eq!(temporaries(dir.path()), Vec::<String>::new());
}

#[test]
fn changed_parameter_sets_cut_early_and_declare_a_discontinuity() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..150).unwrap();
    // A take at frame 150 (a keyframe, 1.001 s into segment 2): new SPS.
    feeder.sps = Some(SPS_B.to_vec());
    feeder.run(150..400).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.4D401F,mp4a.40.2");
    assert_eq!(report.discontinuities, 1);

    let video = read(dir.path(), "video.m3u8");
    // Segments: 0..60, 60..120, 120..150 (cut early), 150..210, 210..270, ..
    assert_eq!(
        extinfs(&video)[..4],
        ["2.002000", "2.002000", "1.001000", "2.002000"]
    );
    let lines: Vec<&str> = video.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "#EXT-X-DISCONTINUITY")
        .unwrap();
    assert_eq!(lines[at + 1], "#EXT-X-MAP:URI=\"init-v-7-1.mp4\"");
    assert!(lines[at + 2].starts_with("#EXT-X-PROGRAM-DATE-TIME:"));
    assert_eq!(lines[at + 4], "seg-v-7-3.m4s");
    assert_eq!(tfdt(dir.path(), "seg-v-7-3.m4s"), 150 * 1001);
    let audio = read(dir.path(), "audio.m3u8");
    let lines: Vec<&str> = audio.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "#EXT-X-DISCONTINUITY")
        .unwrap();
    assert_eq!(lines[at + 1], "#EXT-X-MAP:URI=\"init-a-7-0.mp4\"");
    assert_eq!(lines[at + 4], "seg-a-7-3.m4s");
    assert_eq!(audio.matches("#EXT-X-DISCONTINUITY").count(), 1);
    assert!(read(dir.path(), "master.m3u8").contains("CODECS=\"avc1.4D401F,mp4a.40.2\""));
    assert_eq!(count(dir.path(), "init-v-"), 2);

    // Once the old init's segments leave the retention window it is deleted,
    // and the discontinuity moves into EXT-X-DISCONTINUITY-SEQUENCE.
    feeder.run(400..1800).unwrap();
    let report = validate(dir.path());
    assert_eq!(
        (report.discontinuities, report.discontinuity_sequence),
        (0, 1)
    );
    assert_eq!(
        files(dir.path())
            .iter()
            .filter(|n| n.starts_with("init-v-"))
            .count(),
        1
    );
    assert!(read(dir.path(), "video.m3u8").contains("#EXT-X-MAP:URI=\"init-v-7-1.mp4\""));
    assert!(read(dir.path(), "audio.m3u8").contains("#EXT-X-DISCONTINUITY-SEQUENCE:1"));
}

#[test]
fn a_config_changed_keyframe_with_the_same_sets_only_cuts_early() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.config_changed_at = Some(150);
    feeder.run(0..400).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 0);
    let video = read(dir.path(), "video.m3u8");
    assert_eq!(
        extinfs(&video)[..4],
        ["2.002000", "2.002000", "1.001000", "2.002000"]
    );
    assert_eq!(count(dir.path(), "init-v-"), 1);
}

#[test]
fn video_only_has_no_audio_rendition() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), false);
    cfg.video.bit_rate = None;
    let mut feeder = Feeder::new(cfg);
    feeder.run(0..700).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.640028");
    assert!(!report.has_audio);
    assert_eq!((report.video_segments, report.audio_segments), (10, 0));
    let master = read(dir.path(), "master.m3u8");
    assert!(
        !master.contains("EXT-X-MEDIA") && !master.contains("AUDIO="),
        "{master}"
    );
    // Measured: 60 frames (2 IDR of ~620 bytes, 58 of ~140) per 2.002 s,
    // peak * 1.1; the average is below the peak.
    let bandwidth: u64 = master
        .split("BANDWIDTH=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!((30_000..60_000).contains(&bandwidth), "{bandwidth}");
    assert!(master.contains(",AVERAGE-BANDWIDTH="), "{master}");
    assert!(!files(dir.path())
        .iter()
        .any(|n| n.contains("-a-") || n == "audio.m3u8"));
    assert!(feeder.packager.is_ready());
}

#[test]
fn audio_that_never_arrives_is_an_error_not_a_growing_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.audio = false; // audio is configured but no frame is ever pushed
    let mut failed_at = None;
    for index in 0..600 {
        if let Err(error) = feeder.video(index) {
            assert!(
                error.to_string().contains("segments behind video"),
                "{error}"
            );
            failed_at = Some(index);
            break;
        }
    }
    // Segments close at frames 60, 120, 180 and 240: the fourth is too many.
    assert_eq!(failed_at, Some(240));
    assert_eq!(
        files(dir.path()),
        Vec::<String>::new(),
        "nothing is published without audio"
    );
}

#[test]
fn audio_far_ahead_of_video_is_an_error_not_a_growing_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..90).unwrap();
    let mut failed_at = None;
    for index in feeder.next_audio..feeder.next_audio + 2000 {
        if let Err(error) = feeder.audio_frame(index) {
            assert!(error.to_string().contains("10 s of audio"), "{error}");
            failed_at = Some(index);
            break;
        }
    }
    // Segment 0 (94 audio frames) is published; the buffer holds the audio
    // after it. 10 s is 468.75 frames.
    let failed_at = failed_at.expect("the buffer is bounded");
    assert_eq!(failed_at, 94 + 469);
}

#[test]
fn ten_minutes_keep_the_file_count_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    let mut most = 0;
    for chunk in 0..300 {
        feeder.run(chunk * 60..(chunk + 1) * 60).unwrap();
        most = most.max(files(dir.path()).len());
        assert!(count(dir.path(), "seg-v-") <= 22 && count(dir.path(), "seg-a-") <= 22);
    }
    // 20 + 20 segments, 2 inits, 3 playlists.
    assert_eq!(most, 45);
    let report = validate(dir.path());
    assert_eq!(report.media_sequence, 289);
    assert_eq!(feeder.packager.published().video_segments, 299);
}

/// A good directory to corrupt: 12 s, with audio.
fn good_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..361).unwrap();
    validate(dir.path());
    dir
}

fn rewrite(dir: &Path, name: &str, edit: impl FnOnce(String) -> String) {
    let text = read(dir, name);
    let edited = edit(text.clone());
    assert_ne!(text, edited, "the corruption of {name} changed nothing");
    std::fs::write(dir.join(name), edited).unwrap();
}

fn patch(dir: &Path, name: &str, edit: impl FnOnce(&mut Vec<u8>)) {
    let mut bytes = std::fs::read(dir.join(name)).unwrap();
    edit(&mut bytes);
    std::fs::write(dir.join(name), bytes).unwrap();
}

/// Byte offset of the payload of the box at `path` within the file.
fn offset_of(bytes: &[u8], path: &[&[u8; 4]]) -> usize {
    let payload = cmaf::find_box(bytes, path).unwrap();
    // The payload is a sub-slice of `bytes`: locate it by its address range.
    let range = bytes.as_ptr_range();
    let start = payload.as_ptr();
    assert!(range.contains(&start) || start == range.end);
    bytes.len() - (range.end.addr() - start.addr())
}

fn assert_reports(dir: &Path, needle: &str) {
    let report = check_cast_dir(dir);
    assert!(
        report.errors.iter().any(|e| e.contains(needle)),
        "expected an error containing {needle:?}, got {:#?}",
        report.errors
    );
}

#[test]
fn the_validator_reports_each_corruption() {
    // EXTINF above the target duration.
    let dir = good_dir();
    rewrite(dir.path(), "video.m3u8", |t| {
        t.replacen("#EXTINF:2.002000,", "#EXTINF:2.502000,", 1)
    });
    assert_reports(dir.path(), "exceeds EXT-X-TARGETDURATION");
    assert_reports(dir.path(), "but the samples last");

    // The first sample is not an IDR (NAL type 5 -> 1), flags untouched.
    let dir = good_dir();
    patch(dir.path(), "seg-v-7-2.m4s", |bytes| {
        let mdat = offset_of(bytes, &[b"mdat"]);
        assert_eq!(bytes[mdat + 4], 0x65);
        bytes[mdat + 4] = 0x61;
    });
    assert_reports(
        dir.path(),
        "seg-v-7-2.m4s: the first sample does not start with an IDR",
    );

    // The first sample is flagged non-sync.
    let dir = good_dir();
    patch(dir.path(), "seg-v-7-2.m4s", |bytes| {
        let trun = offset_of(bytes, &[b"moof", b"traf", b"trun"]);
        bytes[trun + 20..trun + 24].copy_from_slice(&cmaf::SAMPLE_FLAGS_NON_SYNC.to_be_bytes());
    });
    assert_reports(
        dir.path(),
        "seg-v-7-2.m4s: the first sample is not a sync sample",
    );

    // A tfdt gap with no discontinuity declared.
    let dir = good_dir();
    patch(dir.path(), "seg-v-7-3.m4s", |bytes| {
        let tfdt = offset_of(bytes, &[b"moof", b"traf", b"tfdt"]);
        bytes[tfdt + 11] ^= 0x40;
    });
    assert_reports(dir.path(), "seg-v-7-3.m4s: tfdt");
    assert_reports(dir.path(), "does not continue the previous segment");

    // CODECS missing from the master, and CODECS that contradict the init.
    let dir = good_dir();
    rewrite(dir.path(), "master.m3u8", |t| {
        t.replace("CODECS=\"avc1.640028,mp4a.40.2\",", "")
    });
    assert_reports(dir.path(), "EXT-X-STREAM-INF has no CODECS");
    let dir = good_dir();
    rewrite(dir.path(), "master.m3u8", |t| {
        t.replace("avc1.640028", "avc1.64001F")
    });
    assert_reports(
        dir.path(),
        "but the init segments declare \"avc1.640028,mp4a.40.2\"",
    );

    // A trun sample size that no longer matches the mdat.
    let dir = good_dir();
    patch(dir.path(), "seg-a-7-1.m4s", |bytes| {
        let trun = offset_of(bytes, &[b"moof", b"traf", b"trun"]);
        bytes[trun + 19] ^= 0x01;
    });
    assert_reports(dir.path(), "seg-a-7-1.m4s: the trun sample sizes sum to");

    // A NAL length that walks past the sample.
    let dir = good_dir();
    patch(dir.path(), "seg-v-7-1.m4s", |bytes| {
        let mdat = offset_of(bytes, &[b"mdat"]);
        bytes[mdat + 3] = bytes[mdat + 3].wrapping_add(1);
    });
    assert_reports(dir.path(), "seg-v-7-1.m4s: sample 0: a NAL length");

    // The audio playlist lists a different sequence range.
    let dir = good_dir();
    rewrite(dir.path(), "audio.m3u8", |t| {
        t.replace("#EXT-X-MEDIA-SEQUENCE:0", "#EXT-X-MEDIA-SEQUENCE:3")
    });
    assert_reports(dir.path(), "but audio.m3u8 lists 3..=");

    // Audio that starts 2 s away from the video of the same sequence number.
    let dir = good_dir();
    rewrite(dir.path(), "audio.m3u8", |t| {
        let first = uris(&t)[0].clone();
        let at = t.find("#EXT-X-PROGRAM-DATE-TIME").unwrap();
        let end = t.find(&first).unwrap() + first.len() + 1;
        // The first entry is removed but the media sequence still says 0.
        format!("{}{}", &t[..at], &t[end..])
    });
    assert_reports(dir.path(), "more than 25 ms apart");

    // A discontinuity in one rendition only; ENDLIST; a missing segment.
    let dir = good_dir();
    rewrite(dir.path(), "audio.m3u8", |t| {
        t.replacen(
            "#EXT-X-PROGRAM-DATE-TIME",
            "#EXT-X-DISCONTINUITY\n#EXT-X-PROGRAM-DATE-TIME",
            1,
        )
    });
    assert_reports(
        dir.path(),
        "the video discontinuity sequence is 0 but the audio one is 1",
    );
    let dir = good_dir();
    rewrite(dir.path(), "video.m3u8", |t| format!("{t}#EXT-X-ENDLIST\n"));
    assert_reports(dir.path(), "must not carry EXT-X-ENDLIST");
    let dir = good_dir();
    std::fs::remove_file(dir.path().join("seg-a-7-4.m4s")).unwrap();
    assert_reports(dir.path(), "segment seg-a-7-4.m4s cannot be read");
    let dir = good_dir();
    std::fs::remove_file(dir.path().join("master.m3u8")).unwrap();
    assert_reports(dir.path(), "master.m3u8: cannot be read");
}

#[test]
fn program_date_time_follows_the_shared_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let epoch = SharedEpoch::new();
    // Media nanoseconds -> wall: media 0 is 2026-01-02T03:04:05.000Z.
    let wall_at_zero = 1_767_323_045_000_000_000_i64;
    epoch.set(WallClockRef::new(
        wall_at_zero,
        0,
        Rational::new(1_000_000_000, 1),
    ));
    let mut cfg = config(dir.path(), true);
    cfg.epoch = Some(epoch.clone());
    let mut feeder = Feeder::new(cfg);
    // Start at frame 300 (10.010 s).
    feeder.next_audio = 300 * 1001 * 48 / 30 / 1024 - 3;
    feeder.run(300..520).unwrap();
    // A stepped epoch after the anchor was taken changes nothing.
    epoch.set_stepped(WallClockRef::new(5, 0, Rational::new(1_000_000_000, 1)));
    feeder.run(520..700).unwrap();
    validate(dir.path());

    let pdts = |name: &str| -> Vec<String> {
        read(dir.path(), name)
            .lines()
            .filter_map(|l| l.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
            .map(str::to_owned)
            .collect()
    };
    let video = pdts("video.m3u8");
    // wall_at(10.010 s), then + 2.002 s per segment (milliseconds floored).
    assert_eq!(
        video,
        [
            "2026-01-02T03:04:15.010Z",
            "2026-01-02T03:04:17.012Z",
            "2026-01-02T03:04:19.014Z",
            "2026-01-02T03:04:21.016Z",
            "2026-01-02T03:04:23.018Z",
            "2026-01-02T03:04:25.020Z",
        ]
    );
    let audio = pdts("audio.m3u8");
    // The first audio frame at or after 10.010 s is frame 470 (10.02667 s).
    assert_eq!(audio[0], "2026-01-02T03:04:15.026Z");
    assert_eq!(audio.len(), video.len());
}

/// The `AudioSpecificConfig` in the audio init segment's `esds`.
fn init_asc(dir: &Path) -> Vec<u8> {
    let bytes = std::fs::read(dir.join("init-a-7-0.mp4")).unwrap();
    let at = bytes.windows(4).position(|w| w == b"esds").unwrap();
    // esds payload: version/flags(4), then 03 len, ES_ID(2) flags(1), 04 len,
    // 13 bytes of DecoderConfig, 05 len, the config.
    let esds = &bytes[at + 4..];
    let len = usize::from(esds[4 + 2 + 3 + 2 + 13 + 1]);
    esds[4 + 2 + 3 + 2 + 13 + 2..][..len].to_vec()
}

#[test]
fn in_band_parameter_sets_win_over_extradata() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    // Extradata declares SPS_B (Annex B form); the stream carries SPS.
    let mut extradata = vec![0, 0, 0, 1];
    extradata.extend_from_slice(&SPS_B);
    extradata.extend_from_slice(&[0, 0, 0, 1]);
    extradata.extend_from_slice(&PPS);
    cfg.video.extradata = Some(extradata);
    let mut feeder = Feeder::new(cfg);
    feeder.run(0..400).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.2");
    assert_eq!(report.discontinuities, 0);
    assert_eq!(count(dir.path(), "init-v-"), 1);
}

#[test]
fn a_stream_with_parameter_sets_only_in_extradata_packages() {
    // NVENC style: AVCC framing, an avcC record as extradata, no in-band sets.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.video.extradata = Some(cmaf::avc_decoder_configuration_record(&SPS, &PPS));
    let mut feeder = Feeder::new(cfg);
    feeder.framing = Framing::Avcc;
    feeder.sps = None;
    // Starts mid-GOP: the stream begins at the first keyframe (frame 30).
    feeder.run(7..400).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.2");
    assert_eq!(tfdt(dir.path(), "seg-v-7-0.m4s"), 30 * 1001);
    let init = std::fs::read(dir.path().join("init-v-7-0.mp4")).unwrap();
    assert!(init.windows(SPS.len()).any(|w| w == SPS));

    // In-band sets that differ from the extradata appear later: init change.
    feeder.sps = Some(SPS_B.to_vec());
    feeder.run(400..700).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.4D401F,mp4a.40.2");
    assert_eq!(report.discontinuities, 1);
}

#[test]
fn no_parameter_sets_anywhere_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), false));
    feeder.sps = None;
    let mut failed_at = None;
    for index in 0..400 {
        if let Err(error) = feeder.video(index) {
            assert!(
                error
                    .to_string()
                    .contains("no H.264 parameter sets in-band or in extradata"),
                "{error}"
            );
            failed_at = Some(index);
            break;
        }
    }
    assert_eq!(failed_at, Some(299), "the 300th skipped unit reports it");
    assert_eq!(files(dir.path()), Vec::<String>::new());

    // Unusable extradata is refused at construction.
    let mut cfg = config(dir.path(), false);
    cfg.video.extradata = Some(vec![1, 2, 3, 4]);
    let error = CastPackager::new(cfg).unwrap_err().to_string();
    assert!(error.contains("extradata holds no SPS and PPS"), "{error}");
}

#[test]
fn the_audio_config_comes_from_the_configuration_then_adts_then_rate_and_channels() {
    // Raw frames, nothing supplied: AAC-LC built from 48 kHz / 2 channels.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    assert_eq!(init_asc(dir.path()), [0x11, 0x90]);

    // ADTS frames, nothing supplied: taken from the first ADTS header (the
    // configuration claims 6 channels; the header says AAC-LC 48 kHz stereo).
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.audio.as_mut().unwrap().channels = 6;
    let mut feeder = Feeder::new(cfg);
    feeder.adts = true;
    feeder.run(0..200).unwrap();
    assert_eq!(init_asc(dir.path()), [0x11, 0x90]);
    validate(dir.path());

    // A supplied config wins over the ADTS header (HE-AAC v1 signalling).
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.audio.as_mut().unwrap().audio_specific_config = Some(vec![0x2B, 0x11, 0x88, 0x00]);
    let mut feeder = Feeder::new(cfg);
    feeder.adts = true;
    feeder.run(0..200).unwrap();
    assert_eq!(init_asc(dir.path()), [0x2B, 0x11, 0x88, 0x00]);
    let report = validate(dir.path());
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.5");
}

/// The URI of the segment that follows each `EXT-X-DISCONTINUITY`, and the
/// tag line right after the discontinuity tag.
fn discontinuities(playlist: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = playlist.lines().collect();
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if *line == "#EXT-X-DISCONTINUITY" {
            let uri = lines[at..].iter().find(|l| !l.starts_with('#')).unwrap();
            found.push(((*uri).to_owned(), lines[at + 1].to_owned()));
        }
    }
    found
}

#[test]
fn a_video_gap_mid_gop_closes_the_segment_and_resumes_at_the_next_keyframe() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    // Frames 200..=214 are lost; audio keeps flowing.
    feeder.run(215..500).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);

    let video = read(dir.path(), "video.m3u8");
    // 0..60, 60..120, 120..180, 180..200 (20 frames), then 240.. after the gap.
    assert_eq!(
        extinfs(&video)[..5],
        ["2.002000", "2.002000", "2.002000", "0.667333", "2.002000"]
    );
    assert_eq!(
        discontinuities(&video),
        [(
            "seg-v-7-4.m4s".to_owned(),
            "#EXT-X-MAP:URI=\"init-v-7-0.mp4\"".to_owned()
        )]
    );
    // Real timestamps, no rebasing: the next segment starts at frame 240.
    assert_eq!(tfdt(dir.path(), "seg-v-7-4.m4s"), 240 * 1001);
    let audio = read(dir.path(), "audio.m3u8");
    assert_eq!(
        discontinuities(&audio),
        [(
            "seg-a-7-4.m4s".to_owned(),
            "#EXT-X-MAP:URI=\"init-a-7-0.mp4\"".to_owned()
        )]
    );
    // The first AAC frame at or after frame 240 (384384 samples).
    assert_eq!(tfdt(dir.path(), "seg-a-7-4.m4s"), 376 * 1024);
    // PDT stays on the one anchor: it jumps by the real gap (0.667 -> 1.335 s).
    assert_eq!(count(dir.path(), "init-"), 2);
}

#[test]
fn an_audio_only_gap_declares_the_discontinuity_in_both_playlists() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    feeder.next_audio += 20; // 20 AAC frames (427 ms) are lost
    feeder.run(200..500).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);
    let video = discontinuities(&read(dir.path(), "video.m3u8"));
    let audio = discontinuities(&read(dir.path(), "audio.m3u8"));
    assert_eq!(video[0].0, "seg-v-7-4.m4s");
    assert_eq!(audio[0].0, "seg-a-7-4.m4s");
    // Video resumes at the first keyframe the audio covers again.
    assert_eq!(tfdt(dir.path(), "seg-v-7-4.m4s"), 240 * 1001);

    // Audio that steps backwards is a gap too.
    feeder.next_audio -= 30;
    feeder.run(500..800).unwrap();
    let report = validate(dir.path());
    assert_eq!(
        report.discontinuities + usize::try_from(report.discontinuity_sequence).unwrap(),
        2
    );
}

#[test]
fn a_gap_right_after_the_start_keeps_the_stream_valid() {
    // One sample, then a gap: the frame duration is unknown, nothing is
    // published for it and the stream simply starts at the next keyframe.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..1).unwrap();
    feeder.packager.note_gap();
    feeder.run(1..400).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 0);
    assert_eq!(tfdt(dir.path(), "seg-v-7-0.m4s"), 30 * 1001);

    // Three samples, then a gap: a 0.1 s segment, then a discontinuity.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..3).unwrap();
    assert_eq!(files(dir.path()), Vec::<String>::new());
    feeder.packager.note_gap();
    feeder.run(3..400).unwrap();
    let report = validate(dir.path());
    let video = read(dir.path(), "video.m3u8");
    assert_eq!(extinfs(&video)[..2], ["0.100100", "2.002000"]);
    assert_eq!(discontinuities(&video)[0].0, "seg-v-7-1.m4s");
    assert_eq!(report.discontinuities, 1);
    assert_eq!(report.target_duration, 2);
}

#[test]
fn fifty_gaps_in_a_row_stay_bounded_and_valid() {
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    let mut at = 0;
    let mut most = 0;
    for gap in 0..50 {
        feeder.run(at..at + 47).unwrap();
        at += 47;
        match gap % 3 {
            0 => at += 5,                    // video frames lost
            1 => feeder.next_audio += 9,     // audio frames lost
            _ => feeder.packager.note_gap(), // the caller knows
        }
        most = most.max(files(dir.path()).len());
        if feeder.packager.published().master {
            validate(dir.path());
        }
    }
    let published = feeder.packager.published();
    assert!(published.video_segments >= 40, "{published:?}");
    assert_eq!(published.video_segments, published.audio_segments);
    assert!(most <= 45, "{most} files");
    // The sequence run never restarts: the newest segment has the highest
    // number, and both playlists agree on it.
    let video = read(dir.path(), "video.m3u8");
    let last = uris(&video).pop().unwrap();
    assert_eq!(
        last,
        format!("seg-v-7-{}.m4s", published.video_segments - 1)
    );
    let report = validate(dir.path());
    assert!(report.discontinuities >= 5, "{report:?}");

    // Then steady state again: the window fills with full segments.
    feeder.run(at..at + 900).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.video_segments, 10);
    assert!(most.max(files(dir.path()).len()) <= 45);
}

#[test]
fn the_playlist_builder_extensions_leave_the_default_rendering_alone() {
    // Defaults: three EXTINF decimals, one playlist-level map, group "audio".
    let mut plain = MediaPlaylist::new(SegmentType::Fmp4);
    plain.set_target_duration(2);
    plain.push_segment(Segment::new("a.m4s", 2.002));
    let mut second = Segment::new("b.m4s", 2.0);
    second.discontinuity = true;
    plain.push_segment(second);
    assert_eq!(
        plain.render(),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.002,\na.m4s\n#EXT-X-DISCONTINUITY\n\
         #EXTINF:2.000,\nb.m4s\n"
    );

    // Per-segment maps: announced where they change and again after a
    // discontinuity; an evicted switch becomes the playlist-level map.
    let mut cast = MediaPlaylist::new(SegmentType::Fmp4);
    cast.set_target_duration(2);
    cast.set_extinf_decimals(6);
    cast.set_window(3);
    cast.push_segment(Segment::new("s0.m4s", 2.002).with_map_uri("i0.mp4"));
    let mut switched = Segment::new("s1.m4s", 94.0 * 1024.0 / 48_000.0).with_map_uri("i1.mp4");
    switched.discontinuity = true;
    cast.push_segment(switched);
    let mut restated = Segment::new("s2.m4s", 2.002).with_map_uri("i1.mp4");
    restated.discontinuity = true;
    cast.push_segment(restated);
    assert_eq!(
        cast.render(),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-MAP:URI=\"i0.mp4\"\n#EXTINF:2.002000,\ns0.m4s\n\
         #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"i1.mp4\"\n#EXTINF:2.005333,\ns1.m4s\n\
         #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"i1.mp4\"\n#EXTINF:2.002000,\ns2.m4s\n"
    );
    cast.push_segment(Segment::new("s3.m4s", 2.002).with_map_uri("i1.mp4"));
    cast.push_segment(Segment::new("s4.m4s", 2.002).with_map_uri("i1.mp4"));
    assert_eq!(
        cast.render(),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:2\n\
         #EXT-X-DISCONTINUITY-SEQUENCE:1\n#EXT-X-MAP:URI=\"i1.mp4\"\n\
         #EXT-X-DISCONTINUITY\n#EXTINF:2.002000,\ns2.m4s\n#EXTINF:2.002000,\ns3.m4s\n\
         #EXTINF:2.002000,\ns4.m4s\n"
    );

    let rendition = || AudioRendition {
        name: "Program".to_owned(),
        uri: "audio.m3u8".to_owned(),
        language: None,
        default: true,
    };
    let mut master = MasterPlaylist::new();
    master.push_variant(VariantStream::new("video.m3u8", 1000));
    master.push_audio(rendition());
    assert_eq!(
        master.render(),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Program\",DEFAULT=YES,AUTOSELECT=YES,\
         URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"audio\"\nvideo.m3u8\n"
    );
    let mut master = MasterPlaylist::new();
    master.push_variant(VariantStream::new("video.m3u8", 1000));
    master.set_audio_group_id("aud");
    master.push_audio_with_channels(rendition(), "2");
    assert!(master.render().contains(
        "GROUP-ID=\"aud\",NAME=\"Program\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"2\",\
         URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"aud\"\n"
    ));
}

#[test]
fn an_audio_outage_drops_the_uncovered_video_segments() {
    // Audio stops for 5 s while video keeps flowing: the segment the outage
    // starts in keeps the audio it has, the video segments with no audio at
    // all are not published (no empty audio segments), and both playlists
    // resume together, discontinuous, at the same sequence number.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    feeder.audio = false;
    for index in 200..350 {
        feeder.video(index).unwrap();
    }
    feeder.audio = true;
    feeder.next_audio = 350 * 1001 * 48 / 30 / 1024;
    feeder.run(350..700).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);
    let published = feeder.packager.published();
    assert_eq!(published.video_segments, published.audio_segments);
    let video = read(dir.path(), "video.m3u8");
    // 0, 60, 120 full; 180..240 published with the audio it had; 240..300
    // has no audio and is dropped; the run resumes at keyframe 360.
    assert_eq!(uris(&video).len(), 9);
    assert_eq!(discontinuities(&video)[0].0, "seg-v-7-4.m4s");
    assert_eq!(tfdt(dir.path(), "seg-v-7-3.m4s"), 180 * 1001);
    assert_eq!(tfdt(dir.path(), "seg-v-7-4.m4s"), 360 * 1001);
    let audio = read(dir.path(), "audio.m3u8");
    assert_eq!(discontinuities(&audio)[0].0, "seg-a-7-4.m4s");
    assert_eq!(count(dir.path(), "seg-v-"), count(dir.path(), "seg-a-"));
}

#[test]
fn a_video_outage_resumes_with_real_timestamps() {
    // Video stops for 4 s while audio keeps flowing.
    let dir = tempfile::tempdir().unwrap();
    let mut feeder = Feeder::new(config(dir.path(), true));
    feeder.run(0..200).unwrap();
    feeder.audio_up_to(320, 0).unwrap();
    feeder.run(320..700).unwrap();
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);
    let video = read(dir.path(), "video.m3u8");
    assert_eq!(extinfs(&video)[3], "0.667333");
    assert_eq!(discontinuities(&video)[0].0, "seg-v-7-4.m4s");
    assert_eq!(tfdt(dir.path(), "seg-v-7-4.m4s"), 330 * 1001);
    // PDT jumps by the real gap: 180 -> 330 frames is 5.005 s.
    let pdt: Vec<&str> = video
        .lines()
        .filter_map(|l| l.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:"))
        .collect();
    let seconds = |text: &str| -> f64 {
        let clock = text.split('T').nth(1).unwrap().trim_end_matches('Z');
        let parts: Vec<f64> = clock.split(':').map(|p| p.parse().unwrap()).collect();
        parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]
    };
    let jump = (seconds(pdt[4]) - seconds(pdt[3])).rem_euclid(86_400.0);
    assert!((jump - 5.005).abs() < 0.002, "{jump}");
}

#[test]
fn an_adts_stream_at_another_rate_than_configured_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut packager = CastPackager::new(config(dir.path(), true)).unwrap();
    // AAC-LC 44.1 kHz stereo header on a rendition configured for 48 kHz.
    let frame = [0xFF, 0xF1, 0x50, 0x80, 0x01, 0x7F, 0xFC, 9, 8, 7, 6];
    let error = packager
        .push_audio(AudioUnit {
            pts: 0,
            data: &frame,
        })
        .unwrap_err();
    assert!(error.to_string().contains("44100"), "{error}");
}
