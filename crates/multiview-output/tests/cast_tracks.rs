//! The Cast packager with every output slot (#418): several audio renditions
//! and `WebVTT` subtitle renditions, read back by the validator.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::Arc;

use multiview_core::time::{MediaTime, Rational};
use multiview_output::hls::cast::{
    is_packager_file_name, purge_dir, AudioUnit, CastAudioConfig, CastAudioRendition, CastPackager,
    CastPackagerConfig, CastVideoConfig, VideoUnit, MAX_CAST_AUDIO_RENDITIONS,
    MAX_CAST_SUBTITLE_RENDITIONS,
};
use multiview_output::hls::cast_check::{check_cast_dir, CastCheckReport};
use multiview_output::hls::{subtitle_feed, SubtitleCue, SubtitleFeed, SubtitleRendition};

const SPS: [u8; 27] = [
    0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
];
const PPS: [u8; 6] = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
const VIDEO_TB: Rational = Rational::new(1001, 30_000);
const AUDIO_TB: Rational = Rational::new(1, 48_000);
const GOP: i64 = 30;

fn frame(index: i64) -> Vec<u8> {
    let key = index % GOP == 0;
    let tag = format!("{index:016x}").into_bytes();
    let mut nals = vec![vec![0x09, 0xF0]];
    if key {
        nals.push(SPS.to_vec());
        nals.push(PPS.to_vec());
        let mut idr = vec![0x65, 0x88, 0x84];
        idr.extend_from_slice(&tag);
        idr.extend(std::iter::repeat_n(0x5A, 400));
        nals.push(idr);
    } else {
        let mut slice = vec![0x41, 0x9A];
        slice.extend_from_slice(&tag);
        slice.extend(std::iter::repeat_n(0x33, 80));
        nals.push(slice);
    }
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&nal);
    }
    out
}

fn aac(index: i64, rendition: usize) -> Vec<u8> {
    let mut payload = vec![0x21, 0x10, u8::try_from(rendition).unwrap()];
    payload.extend_from_slice(&index.to_be_bytes());
    payload.extend(std::iter::repeat_n(0x77, 100));
    payload
}

fn audio_config(bit_rate: u64) -> CastAudioConfig {
    CastAudioConfig {
        time_base: AUDIO_TB,
        sample_rate: 48_000,
        channels: 2,
        bit_rate: Some(bit_rate),
        audio_specific_config: None,
    }
}

fn rendition(name: &str, language: Option<&str>, default: bool, rate: u64) -> CastAudioRendition {
    CastAudioRendition {
        config: audio_config(rate),
        name: name.to_owned(),
        language: language.map(str::to_owned),
        default,
    }
}

fn subs(name: &str, language: Option<&str>) -> SubtitleRendition {
    SubtitleRendition {
        name: name.to_owned(),
        language: language.map(str::to_owned),
    }
}

fn make_config(
    dir: &Path,
    audio: Vec<CastAudioRendition>,
    subtitles: Vec<SubtitleRendition>,
) -> (CastPackagerConfig, SubtitleFeed) {
    let (feed, receiver) = subtitle_feed();
    let config = CastPackagerConfig {
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
        audio,
        subtitles,
        subtitle_feed: Some(Arc::new(receiver)),
        epoch: None,
        wall_anchor: None,
    };
    (config, feed)
}

/// The usual shape: program + an English track, one English subtitle.
fn two_audio_one_sub(dir: &Path) -> (CastPackagerConfig, SubtitleFeed) {
    make_config(
        dir,
        vec![
            rendition("Program", None, true, 192_000),
            rendition("English", Some("en"), false, 128_000),
        ],
        vec![subs("English CC", Some("en"))],
    )
}

struct Feeder {
    packager: CastPackager,
    renditions: usize,
    next_audio: Vec<i64>,
    /// Renditions currently fed.
    fed: Vec<bool>,
}

impl Feeder {
    fn new(config: CastPackagerConfig) -> Self {
        let renditions = config.audio.len();
        Self {
            packager: CastPackager::new(config).unwrap(),
            renditions,
            next_audio: vec![0; renditions],
            fed: vec![true; renditions],
        }
    }

    fn start_at(&mut self, video_index: i64) {
        let at = video_index * 1001 * 48_000 / 30_000 / 1024;
        self.next_audio.fill(at);
    }

    fn video(&mut self, index: i64) {
        let data = frame(index);
        self.packager
            .push_video(VideoUnit {
                pts: index,
                dts: index,
                keyframe: index % GOP == 0,
                config_changed: false,
                data: &data,
            })
            .unwrap();
    }

    fn run(&mut self, range: std::ops::Range<i64>) {
        for index in range {
            let limit = index * 1001 * 48_000 / 30_000 + 2 * 1024;
            for rendition in 0..self.renditions {
                while self.next_audio[rendition] * 1024 < limit {
                    let n = self.next_audio[rendition];
                    if self.fed[rendition] {
                        let data = aac(n, rendition);
                        self.packager
                            .push_audio(
                                rendition,
                                AudioUnit {
                                    pts: n * 1024,
                                    data: &data,
                                },
                            )
                            .unwrap();
                    }
                    self.next_audio[rendition] = n + 1;
                }
            }
            self.video(index);
        }
    }
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn validate(dir: &Path) -> CastCheckReport {
    let report = check_cast_dir(dir);
    assert!(
        report.errors.is_empty(),
        "validator errors: {:#?}\nmaster:\n{}\nsubs0:\n{}",
        report.errors,
        std::fs::read_to_string(dir.join("master.m3u8")).unwrap_or_default(),
        std::fs::read_to_string(dir.join("subs0.m3u8")).unwrap_or_default(),
    );
    report
}

fn uris(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `(sequence, discontinuity)` of every listed segment.
fn shape(playlist: &str) -> Vec<(u64, bool)> {
    let mut sequence: u64 = playlist
        .lines()
        .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .unwrap()
        .parse()
        .unwrap();
    let mut discontinuity = false;
    let mut out = Vec::new();
    for line in playlist.lines() {
        if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if !line.starts_with('#') && !line.is_empty() {
            out.push((sequence, discontinuity));
            sequence += 1;
            discontinuity = false;
        }
    }
    out
}

fn extinfs(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(str::to_owned)
        .collect()
}

fn ms(ms: i64) -> MediaTime {
    MediaTime::from_nanos(ms * 1_000_000)
}

#[test]
fn two_audio_renditions_and_a_subtitle_with_a_cue_across_a_segment_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (config, feed) = two_audio_one_sub(dir.path());
    let mut feeder = Feeder::new(config);
    // A closed cue from 1.5 s to 2.5 s spans the boundary at 2.002 s.
    assert!(feed.offer(SubtitleCue {
        slot: 0,
        start: ms(1_500),
        end: ms(2_500),
        lines: vec!["Hello <world>".to_owned()],
    }));
    feed.mark_accepted_through(ms(2_600));
    feeder.run(0..200);
    let report = validate(dir.path());
    assert_eq!(report.audio_renditions, 2);
    assert_eq!(report.subtitle_renditions, 1);
    assert_eq!(report.subtitle_segments, report.video_segments);
    assert!(report.subtitle_cues >= 2, "{report:?}");
    // CODECS lists the AAC string once.
    assert_eq!(report.codecs, "avc1.640028,mp4a.40.2");

    let master = read(dir.path(), "master.m3u8");
    assert!(
        master.contains(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Program\",DEFAULT=YES,AUTOSELECT=YES,\
         CHANNELS=\"2\",URI=\"audio.m3u8\"\n"
        ),
        "{master}"
    );
    assert!(
        master.contains(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=NO,AUTOSELECT=NO,\
         CHANNELS=\"2\",URI=\"audio1.m3u8\",LANGUAGE=\"en\"\n"
        ),
        "{master}"
    );
    assert!(
        master.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English CC\",LANGUAGE=\"en\",\
         DEFAULT=NO,AUTOSELECT=YES,URI=\"subs0.m3u8\"\n"
        ),
        "{master}"
    );
    // BANDWIDTH = video + the largest audio rendition (a player plays one).
    assert!(
        master.contains("#EXT-X-STREAM-INF:BANDWIDTH=6192000,AVERAGE-BANDWIDTH=6192000,"),
        "{master}"
    );
    assert!(
        master.contains(",AUDIO=\"aud\",SUBTITLES=\"subs\"\nvideo.m3u8\n"),
        "{master}"
    );

    // Every playlist lists the same sequence numbers and durations.
    let video = read(dir.path(), "video.m3u8");
    for name in ["audio.m3u8", "audio1.m3u8", "subs0.m3u8"] {
        assert_eq!(shape(&read(dir.path(), name)), shape(&video), "{name}");
    }
    // Subtitle segments last exactly as long as their video segment (audio
    // segments are AAC-frame aligned instead).
    assert_eq!(extinfs(&read(dir.path(), "subs0.m3u8")), extinfs(&video));
    let subs = read(dir.path(), "subs0.m3u8");
    assert!(!subs.contains("EXT-X-MAP"), "{subs}");
    assert_eq!(uris(&subs)[0], "sub0-7-0.vtt");
    assert_eq!(uris(&read(dir.path(), "audio1.m3u8"))[0], "seg-a1-7-0.m4s");
    assert!(dir.path().join("init-a1-7-0.mp4").exists());

    // The cue carries its total interval in both segments it overlaps; the
    // map is the video segment start at 90 kHz on the media timeline.
    let first = read(dir.path(), "sub0-7-0.vtt");
    assert_eq!(
        first,
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n\
         00:00:01.500 --> 00:00:02.500\nHello &lt;world&gt;\n"
    );
    let second = read(dir.path(), "sub0-7-1.vtt");
    assert_eq!(
        second,
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:180180,LOCAL:00:00:02.002\n\n\
         00:00:01.500 --> 00:00:02.500\nHello &lt;world&gt;\n"
    );
    // A segment with no cue is a header-only file.
    let third = read(dir.path(), "sub0-7-2.vtt");
    assert_eq!(
        third,
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:360360,LOCAL:00:00:04.004\n"
    );
    let published = feeder.packager.published();
    assert_eq!(published.video_segments, published.subtitle_segments);
}

#[test]
fn a_gap_is_a_discontinuity_in_every_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _feed) = two_audio_one_sub(dir.path());
    let mut feeder = Feeder::new(config);
    feeder.run(0..200);
    // Frames 200..=214 are lost; audio keeps flowing.
    feeder.run(215..500);
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);
    let video = shape(&read(dir.path(), "video.m3u8"));
    assert_eq!(video.iter().filter(|(_, d)| *d).count(), 1);
    for name in ["audio.m3u8", "audio1.m3u8", "subs0.m3u8"] {
        assert_eq!(shape(&read(dir.path(), name)), video, "{name}");
    }
}

#[test]
fn missing_audio_in_one_rendition_drops_the_pair_for_all() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _feed) = two_audio_one_sub(dir.path());
    let mut feeder = Feeder::new(config);
    feeder.run(0..200);
    // Rendition 1 stops for 5 s; rendition 0 keeps flowing.
    feeder.fed[1] = false;
    feeder.run(200..350);
    feeder.fed[1] = true;
    feeder.next_audio[1] = 350 * 1001 * 48_000 / 30_000 / 1024;
    feeder.run(350..700);
    let report = validate(dir.path());
    assert_eq!(report.discontinuities, 1);
    let video = read(dir.path(), "video.m3u8");
    let shape_v = shape(&video);
    for name in ["audio.m3u8", "audio1.m3u8", "subs0.m3u8"] {
        assert_eq!(shape(&read(dir.path(), name)), shape_v, "{name}");
    }
    // The uncovered video segments are not published at all (in no
    // playlist): the run resumes at keyframe 360 for everyone.
    let files: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let count = |prefix: &str| files.iter().filter(|n| n.starts_with(prefix)).count();
    assert_eq!(count("seg-v-"), count("seg-a-"));
    assert_eq!(count("seg-v-"), count("seg-a1-"));
    assert_eq!(count("seg-v-"), count("sub0-"));
    let published = feeder.packager.published();
    assert_eq!(published.video_segments, published.audio_segments);
}

#[test]
fn defaults_and_names_are_made_valid() {
    let dir = tempfile::tempdir().unwrap();
    // No rendition marked default, a repeated name, a repeated subtitle name.
    let (config, _feed) = make_config(
        dir.path(),
        vec![
            rendition("Program", None, false, 128_000),
            rendition("Program", Some("fr"), false, 128_000),
            rendition("Deutsch", Some("de"), true, 128_000),
        ],
        vec![subs("CC", None), subs("CC", Some("fr"))],
    );
    let mut feeder = Feeder::new(config);
    feeder.run(0..200);
    let report = validate(dir.path());
    assert_eq!(report.audio_renditions, 3);
    let master = read(dir.path(), "master.m3u8");
    assert_eq!(master.matches("DEFAULT=YES").count(), 1, "{master}");
    assert!(
        master.contains("NAME=\"Deutsch\",DEFAULT=YES"),
        "the first marked rendition is the default: {master}"
    );
    assert!(master.contains("NAME=\"Program (2)\""), "{master}");
    assert!(master.contains("NAME=\"CC (2)\""), "{master}");
    assert!(master.contains("URI=\"audio2.m3u8\""), "{master}");
    assert!(master.contains("URI=\"subs1.m3u8\""), "{master}");

    // None marked: rendition 0 is the default.
    let dir = tempfile::tempdir().unwrap();
    let (config, _feed) = make_config(
        dir.path(),
        vec![
            rendition("A", None, false, 128_000),
            rendition("B", None, false, 128_000),
        ],
        Vec::new(),
    );
    let mut feeder = Feeder::new(config);
    feeder.run(0..120);
    validate(dir.path());
    let master = read(dir.path(), "master.m3u8");
    assert!(master.contains("NAME=\"A\",DEFAULT=YES"), "{master}");
    assert!(!master.contains("SUBTITLES"), "{master}");
}

#[test]
fn too_many_renditions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let many = (0..=MAX_CAST_AUDIO_RENDITIONS)
        .map(|i| rendition(&format!("A{i}"), None, false, 128_000))
        .collect();
    let (config, _feed) = make_config(dir.path(), many, Vec::new());
    let err = CastPackager::new(config).unwrap_err().to_string();
    assert!(err.contains("17 audio renditions"), "{err}");
    let many = (0..=MAX_CAST_SUBTITLE_RENDITIONS)
        .map(|i| subs(&format!("S{i}"), None))
        .collect();
    let (config, _feed) = make_config(dir.path(), Vec::new(), many);
    let err = CastPackager::new(config).unwrap_err().to_string();
    assert!(err.contains("9 subtitle renditions"), "{err}");
    let max = (0..MAX_CAST_SUBTITLE_RENDITIONS)
        .map(|i| subs(&format!("S{i}"), None))
        .collect();
    let (config, _feed) = make_config(dir.path(), Vec::new(), max);
    CastPackager::new(config).unwrap();
}

#[test]
fn the_mpegts_map_wraps_at_33_bits_like_the_media_timeline() {
    // Start past 2^33 / 90 kHz (26.5 h) of media time: MPEGTS is reduced
    // modulo 2^33 (Shaka adds the rollover itself from the segment time).
    let dir = tempfile::tempdir().unwrap();
    let (config, feed) = two_audio_one_sub(dir.path());
    let mut feeder = Feeder::new(config);
    let start = 2_862_150; // a keyframe at 95 497.07 s (> 2^33 / 90 kHz)
    feeder.start_at(start);
    let start_ns = start * 1001 * 1_000_000_000 / 30_000;
    assert!(feed.offer(SubtitleCue {
        slot: 0,
        start: MediaTime::from_nanos(start_ns + 500_000_000),
        end: MediaTime::from_nanos(start_ns + 1_000_000_000),
        lines: vec!["late night".to_owned()],
    }));
    feed.mark_accepted_through(MediaTime::from_nanos(start_ns + 1_000_000_000));
    feeder.run(start..start + 200);
    validate(dir.path());
    let vtt = read(dir.path(), "sub0-7-0.vtt");
    let expected = (u64::try_from(start).unwrap() * 3003) % (1 << 33);
    assert!(
        vtt.starts_with(&format!(
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{expected},LOCAL:00:00:00.000\n\n\
             00:00:00.500 --> 00:00:01.000\nlate night\n"
        )),
        "{vtt}"
    );
}

#[test]
fn the_validator_reports_subtitle_and_rendition_faults() {
    let good = || {
        let dir = tempfile::tempdir().unwrap();
        let (config, feed) = two_audio_one_sub(dir.path());
        let mut feeder = Feeder::new(config);
        assert!(feed.offer(SubtitleCue {
            slot: 0,
            start: ms(2_500),
            end: ms(3_000),
            lines: vec!["x".to_owned()],
        }));
        feed.mark_accepted_through(ms(3_000));
        feeder.run(0..200);
        validate(dir.path());
        dir
    };
    let errors_after = |edit: &dyn Fn(&Path)| {
        let dir = good();
        edit(dir.path());
        check_cast_dir(dir.path()).errors
    };
    let rewrite = |dir: &Path, name: &str, from: &str, to: &str| {
        let text = read(dir, name);
        assert!(text.contains(from), "{name}: {text}");
        std::fs::write(dir.join(name), text.replacen(from, to, 1)).unwrap();
    };
    let has = |errors: &[String], needle: &str| {
        assert!(
            errors.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {errors:#?}"
        );
    };

    // An unwrapped (full 64-bit) MPEGTS.
    let errors = errors_after(&|d| {
        rewrite(d, "sub0-7-1.vtt", "MPEGTS:180180", "MPEGTS:8590114772");
    });
    has(&errors, "is not reduced modulo 2^33");
    // A map that does not match the video segment start.
    let errors = errors_after(&|d| rewrite(d, "sub0-7-1.vtt", "MPEGTS:180180", "MPEGTS:270270"));
    has(&errors, "but the video segment starts at 180180");
    // No map at all; no WEBVTT header.
    let errors = errors_after(&|d| {
        rewrite(d, "sub0-7-2.vtt", "X-TIMESTAMP-MAP", "X-NOT-A-MAP");
    });
    has(&errors, "sub0-7-2.vtt: no X-TIMESTAMP-MAP");
    let errors = errors_after(&|d| rewrite(d, "sub0-7-0.vtt", "WEBVTT", "WEBVTX"));
    has(&errors, "sub0-7-0.vtt: does not start with a WEBVTT header");
    // A cue that ends before it starts.
    let errors = errors_after(&|d| {
        rewrite(
            d,
            "sub0-7-1.vtt",
            "00:00:02.500 --> 00:00:03.000",
            "00:00:03.000 --> 00:00:02.500",
        );
    });
    has(&errors, "does not end after it starts");
    // A discontinuity only in the subtitle playlist.
    let errors = errors_after(&|d| {
        rewrite(d, "subs0.m3u8", "#EXTINF", "#EXT-X-DISCONTINUITY\n#EXTINF");
    });
    has(&errors, "but the subs0.m3u8 one is 1");
    // A different EXTINF.
    let errors =
        errors_after(&|d| rewrite(d, "subs0.m3u8", "#EXTINF:2.002000", "#EXTINF:1.001000"));
    has(&errors, "subs0.m3u8: sub0-7-0.vtt: EXTINF 1.001");
    // The second audio rendition lists another range.
    let errors = errors_after(&|d| {
        rewrite(
            d,
            "audio1.m3u8",
            "#EXT-X-MEDIA-SEQUENCE:0",
            "#EXT-X-MEDIA-SEQUENCE:3",
        );
    });
    has(&errors, "but audio1.m3u8 lists 3..=");
    // Two defaults; a repeated name.
    let errors = errors_after(&|d| {
        rewrite(
            d,
            "master.m3u8",
            "NAME=\"English\",DEFAULT=NO",
            "NAME=\"English\",DEFAULT=YES",
        );
    });
    has(&errors, "2 audio renditions are DEFAULT=YES");
    let errors = errors_after(&|d| {
        rewrite(d, "master.m3u8", "NAME=\"English\"", "NAME=\"Program\"");
    });
    has(&errors, "two audio renditions are named \"Program\"");
    // The variant does not reference the subtitle group.
    let errors = errors_after(&|d| rewrite(d, "master.m3u8", ",SUBTITLES=\"subs\"", ""));
    has(&errors, "the variant's SUBTITLES group None");
    // A missing .vtt segment.
    let errors = errors_after(&|d| std::fs::remove_file(d.join("sub0-7-2.vtt")).unwrap());
    has(&errors, "segment sub0-7-2.vtt cannot be read");
}

#[test]
fn the_new_file_names_are_packager_names_and_purged() {
    for name in [
        "audio1.m3u8",
        "audio15.m3u8",
        "subs0.m3u8",
        "subs7.m3u8",
        "seg-a1-7-0.m4s",
        "seg-a15-12-345.m4s",
        "init-a1-7-0.mp4",
        "sub0-7-0.vtt",
        "sub1-7-3.vtt",
        "sub7-1-99.vtt",
        "sub1-7-3.vtt.tmp",
        "subs2.m3u8.tmp",
        "seg-a2-7-1.m4s.tmp",
    ] {
        assert!(is_packager_file_name(name), "{name}");
    }
    for name in [
        // Rendition 0 never carries its index.
        "audio0.m3u8",
        "seg-a0-7-0.m4s",
        "init-a0-7-0.mp4",
        // Out of range or non-canonical indices.
        "audio16.m3u8",
        "audio01.m3u8",
        "subs8.m3u8",
        "subs01.m3u8",
        "sub8-7-0.vtt",
        "sub01-7-0.vtt",
        "seg-a16-7-0.m4s",
        // Malformed.
        "subs.m3u8",
        "sub-7-0.vtt",
        "sub0-7.vtt",
        "sub0-7-0.vtt.bak",
        "subsx.m3u8",
        "audio1x.m3u8",
        "seg-a1-7-.m4s",
        "seg-ax-7-0.m4s",
        "notes.vtt",
        "sub0-7-0.tmp",
    ] {
        assert!(!is_packager_file_name(name), "{name}");
    }

    let dir = tempfile::tempdir().unwrap();
    let ours = [
        "audio1.m3u8",
        "subs0.m3u8",
        "sub0-7-3.vtt",
        "seg-a1-7-0.m4s",
        "init-a1-7-0.mp4",
    ];
    let theirs = ["audio0.m3u8", "captions.vtt", "subs.m3u8"];
    for name in ours.iter().chain(&theirs) {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    purge_dir(dir.path());
    for name in ours {
        assert!(!dir.path().join(name).exists(), "{name}");
    }
    for name in theirs {
        assert!(dir.path().join(name).exists(), "{name}");
    }
}

#[test]
fn remove_files_and_retention_cover_every_rendition() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _feed) = two_audio_one_sub(dir.path());
    let mut feeder = Feeder::new(config);
    // 10 minutes: the files stay bounded by the retention window (2 x 10).
    feeder.run(0..18_000);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let count = |prefix: &str| names.iter().filter(|n| n.starts_with(prefix)).count();
    for prefix in ["seg-v-", "seg-a-", "seg-a1-", "sub0-"] {
        assert!(count(prefix) <= 20, "{prefix}: {}", count(prefix));
    }
    validate(dir.path());
    feeder.packager.remove_files();
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
