//! Audio-aware packet-fed mux tests (the `ffmpeg` feature) — AUD-4 slice 2.
//!
//! Encode-once-mux-many fans the SAME coded packets to N mux-only sinks; with
//! program audio (AUD-4) each muxer registers a SECOND elementary stream and the
//! kind-tagged packets route to the matching `stream_index`. These tests build a
//! short run of real video packets (`mpeg2video`) and real audio packets
//! (native `aac`), tag them [`StreamKind::Video`]/[`StreamKind::Audio`], and
//! assert that:
//!
//! * a single-container [`PacketMuxSink::file`] fed both, via
//!   [`PacketMuxSink::run_av`] with a video + an audio [`MuxStream`], writes a
//!   container with EXACTLY one video and one audio stream (header-pinning,
//!   ADR-R005 §3.3); and
//! * a segmented (HLS) sink does the same per `.ts` segment.
//!
//! Licensing: `mpeg2video` + native `aac` are both LGPL — never x264/x265/fdk.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{
    AudioEncodeTarget, AudioEncoder, EncodedPacket, StreamCodecParameters, VideoEncodeTarget,
    VideoEncoder,
};
use multiview_output::sink::{
    AudioSelection, MuxStream, PacketMuxOutcome, PacketMuxSink, PacketSource, TrackSelection,
};
use multiview_output::Result;
use multiview_output::SharedEpoch;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: i64 = 30;
const SAMPLE_RATE: u32 = 48_000;
const VIDEO_FRAMES: i64 = 12;

/// Keyframe interval (GOP). With static content the encoder inserts a keyframe
/// only at each GOP boundary (no scene-change keyframes), so the segmenter cuts
/// `VIDEO_FRAMES / GOP` segments of `GOP` frames each — large enough for
/// `ffprobe` to analyze (a 1-frame TS segment can be too small to probe).
const GOP: u32 = 4;

/// Build a flat mid-gray planar-`yuv420p` frame stamped at `pts`. Static content
/// keeps the only keyframes at the GOP boundaries (deterministic segmentation).
fn gray_frame(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, WIDTH, HEIGHT);
    for value in frame.data_mut(0).iter_mut() {
        *value = 128;
    }
    for plane in 1..3 {
        for value in frame.data_mut(plane).iter_mut() {
            *value = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode `VIDEO_FRAMES` synthetic frames once with `mpeg2video`, returning the
/// kind-tagged video packets plus the codec-params snapshot + time-base a mux
/// sink registers its video stream from.
fn encode_video() -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    encode_video_at(2_000_000)
}

fn encode_video_at(bit_rate: usize) -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    encode_video_frames(bit_rate, VIDEO_FRAMES)
}

fn encode_video_frames(
    bit_rate: usize,
    frame_count: i64,
) -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let target = VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, FPS),
        bit_rate,
        gop: GOP,
        cuda_device: None,
    };
    let mut encoder = VideoEncoder::new(&target).expect("open video encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_encoder(&encoder);
    let mut packets = Vec::new();
    for tick in 0..frame_count {
        encoder
            .send_frame(&gray_frame(tick))
            .expect("send video frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv video") {
            packets.push(EncodedPacket::from_packet(pkt));
        }
    }
    encoder.send_eof().expect("video eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain video") {
        packets.push(EncodedPacket::from_packet(pkt));
    }
    (packets, params, time_base)
}

/// A planar-float stereo frame of `samples` samples of low-level tone, stamped
/// at `pts` in the audio (`1/sample_rate`) time-base.
fn tone_frame(samples: usize, pts: i64) -> Audio {
    let mut frame = Audio::new(
        Sample::F32(SampleType::Planar),
        samples,
        ChannelLayout::STEREO,
    );
    frame.set_rate(SAMPLE_RATE);
    for ch in 0..2 {
        let plane = frame.plane_mut::<f32>(ch);
        for (i, sample) in plane.iter_mut().enumerate() {
            // A quiet, deterministic ramp — enough for AAC to emit real packets
            // without depending on a transcendental fn (and `as` is banned).
            let n = i32::try_from(i % 64).unwrap_or(0);
            *sample = f32::from(i16::try_from(n).unwrap_or(0)) / 4096.0;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode roughly the video duration of `aac` audio once, returning the
/// kind-tagged audio packets plus its codec-params snapshot + time-base.
fn encode_audio() -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    encode_audio_at(128_000)
}

fn encode_audio_at(bit_rate: usize) -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let target = AudioEncodeTarget {
        codec_name: "aac".to_owned(),
        format: Sample::F32(SampleType::Planar),
        channel_layout: ChannelLayout::STEREO,
        sample_rate: SAMPLE_RATE,
        bit_rate,
    };
    let mut encoder = AudioEncoder::new(&target).expect("open aac encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_audio_encoder(&encoder);
    let frame_size = if encoder.frame_size() == 0 {
        1024
    } else {
        encoder.frame_size()
    };
    let frame_samples = usize::try_from(frame_size).unwrap_or(1024);
    // Cover ~the video span: VIDEO_FRAMES/FPS seconds of audio.
    let total_samples = (u64::try_from(VIDEO_FRAMES).unwrap_or(0) * u64::from(SAMPLE_RATE))
        / u64::try_from(FPS).unwrap_or(1);
    let blocks = (total_samples / u64::from(frame_size)) + 1;

    let mut packets = Vec::new();
    let mut pts: i64 = 0;
    for _ in 0..blocks {
        encoder
            .send_frame(&tone_frame(frame_samples, pts))
            .expect("send audio frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv audio") {
            packets.push(EncodedPacket::from_audio_packet(pkt));
        }
        pts = pts.saturating_add(i64::from(frame_size));
    }
    encoder.send_eof().expect("audio eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain audio") {
        packets.push(EncodedPacket::from_audio_packet(pkt));
    }
    (packets, params, time_base)
}

/// A [`PacketSource`] over a pre-built vector (drains front-to-back).
struct VecPackets {
    packets: std::vec::IntoIter<EncodedPacket>,
}

impl PacketSource for VecPackets {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}

/// A packet's PTS on a common nanosecond timeline (exact i128 rational scale, no
/// float / no `as`), for interleaving the two streams the way the real pipeline
/// does per tick. An absent PTS sorts at the start.
fn pts_ns(pkt: &EncodedPacket, tb: Rational) -> i128 {
    let pts = i128::from(pkt.pts().unwrap_or(0));
    pts * i128::from(tb.num) * 1_000_000_000 / i128::from(tb.den)
}

/// Interleave the video + audio packets by their PTS on a common timeline (a
/// stable sort, so a video packet that ties an audio packet at the same instant
/// keeps the video first — the first segment must open on a video keyframe).
/// This mirrors the cli's per-tick A/V interleave; concatenating instead would
/// dump all audio into the final HLS segment.
fn interleave(
    video: Vec<EncodedPacket>,
    v_tb: Rational,
    audio: Vec<EncodedPacket>,
    a_tb: Rational,
) -> Vec<EncodedPacket> {
    let mut tagged: Vec<(i128, EncodedPacket)> = Vec::new();
    for pkt in video {
        tagged.push((pts_ns(&pkt, v_tb), pkt));
    }
    for pkt in audio {
        tagged.push((pts_ns(&pkt, a_tb), pkt));
    }
    tagged.sort_by_key(|(ns, _)| *ns);
    tagged.into_iter().map(|(_, pkt)| pkt).collect()
}

/// Count the DISTINCT stream indices of media type `kind` (`"v"` / `"a"`) in
/// `path`. MPEG-TS makes `ffprobe` list each stream twice (container scan +
/// post-analysis), so we dedupe by index rather than counting lines.
fn distinct_stream_indices(path: &Path, kind: &str) -> std::collections::BTreeSet<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-select_streams",
            kind,
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed on {}",
        path.display()
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<i64>().ok())
        .collect()
}

/// Assert `path` carries exactly one video stream and one audio stream.
fn assert_one_video_one_audio(path: &Path) {
    let video = distinct_stream_indices(path, "v");
    let audio = distinct_stream_indices(path, "a");
    assert_eq!(
        video.len(),
        1,
        "exactly one video stream in {}: {video:?}",
        path.display()
    );
    assert_eq!(
        audio.len(),
        1,
        "exactly one audio stream in {}: {audio:?}",
        path.display()
    );
}

#[test]
fn live_renditions_prune_and_restart_without_stale_segment_urls() {
    let dir = tempfile::tempdir().expect("tempdir");
    for run in 0..2 {
        let (video, video_params, video_tb) = encode_video();
        let (audio, audio_params, audio_tb) = encode_audio();
        let packets = interleave(video, video_tb, audio, audio_tb);
        let mut source = VecPackets {
            packets: packets.into_iter(),
        };
        let sink = PacketMuxSink::segment_live(
            dir.path(),
            "seg",
            dir.path().join("index.m3u8"),
            2,
            SharedEpoch::new(),
        )
        .with_audio_selection(AudioSelection {
            program: true,
            tracks: vec![TrackSelection {
                index: 0,
                language: None,
                title: Some("Clean".to_owned()),
            }],
        });
        // The same AAC packets feed the program and the clean track for this
        // lifecycle test; the CLI test checks distinct decoded content.
        let mut combined = Vec::new();
        for packet in source.packets {
            if packet.kind() == multiview_ffmpeg::StreamKind::Audio {
                combined.push(EncodedPacket::with_kind(
                    packet.to_owned_packet(),
                    multiview_ffmpeg::StreamKind::AudioTrack(0),
                ));
            }
            combined.push(packet);
        }
        source = VecPackets {
            packets: combined.into_iter(),
        };
        sink.run_av_tracks(
            &mut source,
            MuxStream::new(&video_params, video_tb),
            Some(MuxStream::new(&audio_params, audio_tb)),
            &[MuxStream::new(&audio_params, audio_tb)],
        )
        .expect("live run");
        let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
        let mut playlists = vec!["index.m3u8".to_owned()];
        playlists.extend(master_media_uris(&master));
        for playlist in playlists {
            let text = std::fs::read_to_string(dir.path().join(&playlist)).expect("playlist");
            let referenced: Vec<_> = text
                .lines()
                .filter(|line| {
                    Path::new(line)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
                })
                .collect();
            assert_eq!(referenced.len(), 2, "{playlist}: {text}");
            for uri in referenced {
                assert!(
                    dir.path().join(uri).exists(),
                    "{playlist} references missing {uri}"
                );
            }
            if run == 1 {
                assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1"), "{text}");
                assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:4"), "{text}");
            }
        }
    }
}

/// A master writer that always fails (the publish of the restarted run's master
/// must not replace the previous master, and its media must survive).
fn failing_master() -> multiview_output::hls::ManifestWriter {
    std::sync::Arc::new(
        |_path, _bytes, _fence: &multiview_output::hls::PublishFence| {
            Err(multiview_output::Error::Output(
                "injected master publish failure".to_owned(),
            ))
        },
    )
}

fn master_media_uris(master: &str) -> Vec<String> {
    master
        .lines()
        .filter_map(|line| {
            if line.starts_with("#EXT-X-MEDIA:") {
                line.split("URI=\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .map(str::to_owned)
            } else if !line.starts_with('#')
                && Path::new(line)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
            {
                Some(line.to_owned())
            } else {
                None
            }
        })
        .collect()
}

fn assert_manifest_and_media_readable(dir: &Path, uri: &str) {
    let text = std::fs::read_to_string(dir.join(uri)).expect("old rendition manifest must survive");
    let segments: Vec<_> = text
        .lines()
        .filter(|line| {
            Path::new(line)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
        })
        .collect();
    assert!(!segments.is_empty(), "{uri} has no media");
    for segment in segments {
        assert!(
            std::fs::read(dir.join(segment)).is_ok(),
            "{uri} references missing {segment}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // reason: one complete three-generation restart scenario.
fn restart_removes_obsolete_track_and_advertises_configured_bandwidth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (video, v_params, v_tb) = encode_video_at(900_000);
    let (audio, a_params, a_tb) = encode_audio_at(96_000);
    let combined = interleave(video, v_tb, audio, a_tb);
    let mut track_packets = Vec::new();
    for packet in combined {
        if packet.kind() == multiview_ffmpeg::StreamKind::Audio {
            track_packets.push(EncodedPacket::with_kind(
                packet.to_owned_packet(),
                multiview_ffmpeg::StreamKind::AudioTrack(0),
            ));
        } else {
            track_packets.push(packet);
        }
    }
    let mut first = VecPackets {
        packets: track_packets.into_iter(),
    };
    let sink = PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .with_audio_selection(AudioSelection {
        program: false,
        tracks: vec![TrackSelection {
            index: 0,
            language: None,
            title: Some("Old".into()),
        }],
    });
    sink.run_av_tracks(
        &mut first,
        MuxStream::new(&v_params, v_tb),
        None,
        &[MuxStream::new(&a_params, a_tb)],
    )
    .unwrap();
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    assert!(master.contains("BANDWIDTH=996000"), "{master}");
    assert!(master.contains("RESOLUTION=160x120"), "{master}");
    assert!(master.contains("FRAME-RATE=30.000"), "{master}");
    assert!(master.contains("CODECS=\"mp2v,mp4a.40.2\""), "{master}");
    let first_uris = master_media_uris(&master);
    assert_eq!(first_uris.len(), 2, "{master}");
    let old_track_uri = first_uris
        .iter()
        .find(|uri| uri.starts_with("audio-track0-g"))
        .unwrap();
    let old_video_uri = first_uris
        .iter()
        .find(|uri| uri.starts_with("video-g"))
        .unwrap();
    assert_manifest_and_media_readable(dir.path(), old_track_uri);
    assert_manifest_and_media_readable(dir.path(), old_video_uri);
    let first_track = std::fs::read_to_string(dir.path().join(old_track_uri)).unwrap();
    let first_index = first_track
        .lines()
        .find(|line| line.starts_with("#EXT-X-MEDIA-SEQUENCE:"))
        .unwrap();

    let (video, v_params, v_tb) = encode_video_at(750_000);
    let mut second = VecPackets {
        packets: video.into_iter(),
    };
    let sink = PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    );
    sink.run_av_tracks(&mut second, MuxStream::new(&v_params, v_tb), None, &[])
        .unwrap();
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    assert!(master.contains("BANDWIDTH=750000"), "{master}");
    assert!(
        !master.contains("AUDIO=\"audio\""),
        "video-only master must not reference an audio group: {master}"
    );
    assert!(
        !master.contains("audio-track0.m3u8"),
        "removed track must not be advertised: {master}"
    );
    assert_manifest_and_media_readable(dir.path(), old_track_uri);
    assert_manifest_and_media_readable(dir.path(), old_video_uri);
    assert!(
        dir.path().join("index.m3u8").exists(),
        "legacy index must remain"
    );
    let (video, v_params, v_tb) = encode_video_at(1_100_000);
    let (audio, a_params, a_tb) = encode_audio_at(128_000);
    let packets = interleave(video, v_tb, audio, a_tb)
        .into_iter()
        .map(|packet| {
            if packet.kind() == multiview_ffmpeg::StreamKind::Audio {
                EncodedPacket::with_kind(
                    packet.to_owned_packet(),
                    multiview_ffmpeg::StreamKind::AudioTrack(0),
                )
            } else {
                packet
            }
        })
        .collect::<Vec<_>>();
    let mut third = VecPackets {
        packets: packets.into_iter(),
    };
    let sink = PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .with_audio_selection(AudioSelection {
        program: false,
        tracks: vec![TrackSelection {
            index: 0,
            language: None,
            title: Some("New".into()),
        }],
    });
    sink.run_av_tracks(
        &mut third,
        MuxStream::new(&v_params, v_tb),
        None,
        &[MuxStream::new(&a_params, a_tb)],
    )
    .unwrap();
    let new_track_uri =
        master_media_uris(&std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap())
            .into_iter()
            .find(|uri| uri.starts_with("audio-track0-g"))
            .unwrap();
    assert_ne!(new_track_uri, *old_track_uri);
    let track = std::fs::read_to_string(dir.path().join(new_track_uri)).unwrap();
    assert!(track.contains("#EXT-X-DISCONTINUITY"), "{track}");
    assert_ne!(
        first_index,
        track
            .lines()
            .find(|line| line.starts_with("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap()
    );
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    assert!(master.contains("BANDWIDTH=1228000"), "{master}");
    assert!(master.contains("NAME=\"New\""), "{master}");
    assert!(!master.contains("NAME=\"Old\""), "{master}");
}

#[test]
fn failed_restart_retains_prior_master_and_every_cached_media_uri() {
    struct FailBeforeFirstSegment;
    impl PacketSource for FailBeforeFirstSegment {
        fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
            Err(multiview_output::Error::Output(
                "injected publish failure".into(),
            ))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (video, params, tb) = encode_video_at(750_000);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .run_av_tracks(&mut source, MuxStream::new(&params, tb), None, &[])
    .unwrap();
    let old_master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    let mut failing = FailBeforeFirstSegment;
    assert!(PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new()
    )
    .run_av_tracks(&mut failing, MuxStream::new(&params, tb), None, &[])
    .is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap(),
        old_master
    );
    for uri in master_media_uris(&old_master) {
        assert_manifest_and_media_readable(dir.path(), &uri);
    }
}

#[test]
fn failed_master_publish_keeps_old_master_and_all_cached_media() {
    let dir = tempfile::tempdir().unwrap();
    let (video, params, tb) = encode_video_at(750_000);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .run_av_tracks(&mut source, MuxStream::new(&params, tb), None, &[])
    .unwrap();
    let old_master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    // Fail the master publish of the restarted run (after its renditions exist).
    let (video, params, tb) = encode_video_at(850_000);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    assert!(PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new()
    )
    .with_master_writer(failing_master())
    .run_av_tracks(&mut source, MuxStream::new(&params, tb), None, &[])
    .is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap(),
        old_master
    );
    for uri in master_media_uris(&old_master) {
        assert_manifest_and_media_readable(dir.path(), &uri);
    }
    assert_all_media_references_exist(dir.path());
}

#[test]
fn live_restart_reaps_prior_generations_without_erasing_current_window() {
    let dir = tempfile::tempdir().unwrap();
    for generation in 0..8 {
        let (video, v_params, v_tb) = encode_video_at(750_000);
        let mut source = VecPackets {
            packets: video.into_iter(),
        };
        PacketMuxSink::segment_live(
            dir.path(),
            "seg",
            dir.path().join("index.m3u8"),
            2,
            SharedEpoch::new(),
        )
        .run_av_tracks(&mut source, MuxStream::new(&v_params, v_tb), None, &[])
        .unwrap();
        let ts_count = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ts"))
            .count();
        assert!(
            ts_count <= 18,
            "restart {generation} retained {ts_count} files"
        );
        let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
        let video_uri = master_media_uris(&master)
            .into_iter()
            .find(|uri| uri.starts_with("video-g"))
            .unwrap();
        let video_playlist = std::fs::read_to_string(dir.path().join(video_uri)).unwrap();
        for uri in video_playlist.lines().filter(|line| {
            Path::new(line)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
        }) {
            assert!(
                dir.path().join(uri).exists(),
                "listed segment absent: {uri}"
            );
        }
    }
}

#[test]
fn failed_master_publish_preserves_sparse_audio_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (video, v_params, v_tb) = encode_video_frames(750_000, 4 * 35);
    let (audio, a_params, a_tb) = encode_audio();
    let audio = audio
        .into_iter()
        .filter(|packet| pts_ns(packet, a_tb) < 100_000_000)
        .collect();
    let mut source = VecPackets {
        packets: interleave(video, v_tb, audio, a_tb).into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .with_audio_selection(AudioSelection {
        program: true,
        tracks: vec![],
    })
    .run_av_tracks(
        &mut source,
        MuxStream::new(&v_params, v_tb),
        Some(MuxStream::new(&a_params, a_tb)),
        &[],
    )
    .unwrap();
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    let (video, v_params, v_tb) = encode_video_frames(850_000, 4 * 35);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    assert!(PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new()
    )
    .with_master_writer(failing_master())
    .run_av_tracks(&mut source, MuxStream::new(&v_params, v_tb), None, &[])
    .is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap(),
        master
    );
    for uri in master_media_uris(&master) {
        assert_manifest_and_media_readable(dir.path(), &uri);
    }
    assert_all_media_references_exist(dir.path());
}

#[test]
fn sparse_audio_keeps_all_manifest_references_and_bounds_stalled_retention() {
    let dir = tempfile::tempdir().unwrap();
    let (video, v_params, v_tb) = encode_video_frames(750_000, 4 * 55);
    let (audio, a_params, a_tb) = encode_audio();
    // One audio GOP, then no more audio across several complete grace windows.
    let audio = audio
        .into_iter()
        .filter(|packet| pts_ns(packet, a_tb) < 100_000_000)
        .collect();
    let packets = interleave(video, v_tb, audio, a_tb);
    let mut source = VecPackets {
        packets: packets.into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .with_audio_selection(AudioSelection {
        program: true,
        tracks: vec![],
    })
    .run_av_tracks(
        &mut source,
        MuxStream::new(&v_params, v_tb),
        Some(MuxStream::new(&a_params, a_tb)),
        &[],
    )
    .unwrap();
    let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
    let audio_uri = master_media_uris(&master)
        .into_iter()
        .find(|uri| uri.starts_with("audio-program-g"))
        .unwrap();
    let first_audio = std::fs::read_to_string(dir.path().join(&audio_uri)).unwrap();
    let old_segment = first_audio
        .lines()
        .find(|line| Path::new(line).extension().is_some_and(|ext| ext == "ts"))
        .unwrap();
    assert!(
        dir.path().join(old_segment).exists(),
        "stalled audio manifest lost {old_segment}"
    );
    assert_all_media_references_exist(dir.path());
    let count = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ts"))
        .count();
    assert!(count <= 20, "stalled audio retained {count} segments");

    // Restart with the audio track removed. The old generation remains readable
    // until its bounded grace expires, then its references retire before TS GC.
    let (video, v_params, v_tb) = encode_video_frames(750_000, 4 * 55);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .run_av_tracks(&mut source, MuxStream::new(&v_params, v_tb), None, &[])
    .unwrap();
    assert_all_media_references_exist(dir.path());
    assert!(
        !dir.path().join(&audio_uri).exists(),
        "expired audio manifest must retire"
    );
    assert!(
        !dir.path().join(old_segment).exists(),
        "retired audio reference must release TS"
    );
    let count = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ts"))
        .count();
    assert!(count <= 20, "retired generation retained {count} segments");
}

fn assert_all_media_references_exist(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        if entry.path().extension().is_none_or(|ext| ext != "m3u8") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap();
        for uri in text
            .lines()
            .filter(|line| Path::new(line).extension().is_some_and(|ext| ext == "ts"))
        {
            assert!(
                dir.join(uri).is_file(),
                "{} references missing {uri}",
                entry.path().display()
            );
        }
    }
}

#[test]
fn unknown_video_bitrate_omits_master_instead_of_advertising_one_bps() {
    let dir = tempfile::tempdir().unwrap();
    let (video, v_params, v_tb) = encode_video_at(0);
    assert_eq!(v_params.bit_rate(), None);
    let mut source = VecPackets {
        packets: video.into_iter(),
    };
    PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        dir.path().join("index.m3u8"),
        2,
        SharedEpoch::new(),
    )
    .run_av_tracks(&mut source, MuxStream::new(&v_params, v_tb), None, &[])
    .unwrap();
    assert!(!dir.path().join("master.m3u8").exists());
    assert!(dir.path().join("index.m3u8").exists());
    assert!(
        !master_media_uris(
            &std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap_or_default()
        )
        .is_empty()
            || std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("video-g"))
    );
}

#[test]
fn file_sink_muxes_a_video_plus_audio_container() {
    let (video, v_params, v_tb) = encode_video();
    let (audio, a_params, a_tb) = encode_audio();
    assert!(!video.is_empty(), "expected real video packets");
    assert!(!audio.is_empty(), "expected real audio packets");

    let combined = interleave(video, v_tb, audio, a_tb);
    let mut source = VecPackets {
        packets: combined.into_iter(),
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("program.ts");
    let sink = PacketMuxSink::file(&out);
    let outcome = sink
        .run_av(
            &mut source,
            MuxStream::new(&v_params, v_tb),
            Some(MuxStream::new(&a_params, a_tb)),
        )
        .expect("run_av file");
    match outcome {
        PacketMuxOutcome::Single(stats) => {
            assert!(stats.packets > 0, "muxed at least one packet");
        }
        PacketMuxOutcome::Segment(_) => panic!("file sink must yield Single"),
    }

    assert_one_video_one_audio(&out);
}

#[test]
fn segment_sink_muxes_video_plus_audio_per_segment() {
    let (video, v_params, v_tb) = encode_video();
    let (audio, a_params, a_tb) = encode_audio();

    let combined = interleave(video, v_tb, audio, a_tb);
    let mut source = VecPackets {
        packets: combined.into_iter(),
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let sink = PacketMuxSink::segment(dir.path(), "seg");
    let outcome = sink
        .run_av(
            &mut source,
            MuxStream::new(&v_params, v_tb),
            Some(MuxStream::new(&a_params, a_tb)),
        )
        .expect("run_av segment");
    let result = match outcome {
        PacketMuxOutcome::Segment(result) => result,
        PacketMuxOutcome::Single(_) => panic!("segment sink must yield Segment"),
    };
    assert!(!result.segments.is_empty(), "wrote at least one segment");
    for seg in &result.segments {
        assert_one_video_one_audio(seg);
    }
}
