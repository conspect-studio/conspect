//! Package H.264 Annex B + ADTS AAC elementary streams with the Cast
//! packager (the real-decoder proof helper).
//!
//! Run: `cargo run -p multiview-output --example cast_pack_es -- <v.h264>
//! <a.aac|-> <out-dir> [--fps N/D] [--size WxH] [--rate HZ] [--channels N]
//! [--window N] [--extradata] [--drop-video A-B] [--drop-audio A-B]
//! [--loops N] [--realtime]`
//!
//! The video stream is split into access units (at AUD NAL units, or at a
//! slice with `first_mb_in_slice == 0`), the audio stream into ADTS frames;
//! timestamps are assigned by frame index and both are fed in timestamp
//! order. `--extradata` strips the in-band SPS/PPS and hands them to the
//! packager as `avcC` extradata (the NVENC shape). `--drop-video` /
//! `--drop-audio` leave out an index range (inclusive) to exercise gaps.
//! `--loops N` repeats the input N times on one continuous timeline and
//! `--realtime` paces the feed at the frame rate, so the directory is a live
//! stream a player can follow (the Shaka / Cast receiver proof).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::exit,
    clippy::too_many_lines
)]

use std::path::PathBuf;

use multiview_core::time::Rational;
use multiview_output::hls::cast::{
    AudioUnit, CastAudioConfig, CastPackager, CastPackagerConfig, CastVideoConfig, VideoUnit,
};
use multiview_output::hls::cmaf;

/// `(offset of the start code, offset of the NAL header)` of every NAL unit.
fn nal_offsets(data: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            found.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

/// Split an Annex B stream into access units (byte ranges).
fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let nals = nal_offsets(data);
    let mut units = Vec::new();
    let mut unit_start = nals.first().map_or(0, |n| n.0);
    let mut has_vcl = false;
    for &(start, header) in &nals {
        let kind = data[header] & 0x1F;
        let vcl = (1..=5).contains(&kind);
        // first_mb_in_slice is ue(v): zero iff the first payload bit is set.
        let first_slice = vcl && data.get(header + 1).is_some_and(|b| b & 0x80 != 0);
        let boundary = match kind {
            9 => start > unit_start,
            6..=8 => has_vcl,
            _ => first_slice && has_vcl,
        };
        if boundary {
            units.push(&data[unit_start..start]);
            unit_start = start;
            has_vcl = false;
        }
        has_vcl |= vcl;
    }
    if has_vcl {
        units.push(&data[unit_start..]);
    }
    units
}

/// Split an ADTS stream into frames (header included).
fn adts_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut rest = data;
    while let Some(header) = cmaf::parse_adts_header(rest) {
        if header.frame_len > rest.len() {
            break;
        }
        frames.push(&rest[..header.frame_len]);
        rest = &rest[header.frame_len..];
    }
    frames
}

fn pair(text: &str, separator: char) -> (i64, i64) {
    let (a, b) = text.split_once(separator).expect("expected two numbers");
    (a.parse().unwrap(), b.parse().unwrap())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: cast_pack_es <v.h264> <a.aac|-> <out-dir> [options]");
        std::process::exit(2);
    }
    let (mut fps, mut size, mut rate, mut channels) =
        ((30_000, 1001), (1280, 720), 48_000u32, 2u16);
    let (mut window, mut extradata) = (10usize, false);
    let (mut drop_video, mut drop_audio) = (None, None);
    let (mut loops, mut realtime) = (1usize, false);
    let mut options = args[3..].iter();
    while let Some(option) = options.next() {
        let mut value = || options.next().expect("option value").as_str();
        match option.as_str() {
            "--fps" => fps = pair(value(), '/'),
            "--size" => size = pair(value(), 'x'),
            "--rate" => rate = value().parse().unwrap(),
            "--channels" => channels = value().parse().unwrap(),
            "--window" => window = value().parse().unwrap(),
            "--extradata" => extradata = true,
            "--drop-video" => drop_video = Some(pair(value(), '-')),
            "--drop-audio" => drop_audio = Some(pair(value(), '-')),
            "--loops" => loops = value().parse().unwrap(),
            "--realtime" => realtime = true,
            other => panic!("unknown option {other}"),
        }
    }
    let video_bytes = std::fs::read(&args[0]).expect("read the H.264 stream");
    let audio_bytes = if args[1] == "-" {
        Vec::new()
    } else {
        std::fs::read(&args[1]).expect("read the ADTS stream")
    };
    let units = access_units(&video_bytes);
    let frames = adts_frames(&audio_bytes);
    let has_audio = !frames.is_empty();

    // `--extradata`: take the sets of the first unit out of band.
    let sets = units.first().and_then(|unit| {
        let parsed = cmaf::h264_access_unit(unit, None).ok()?;
        Some(cmaf::avc_decoder_configuration_record(
            &parsed.sps?,
            &parsed.pps?,
        ))
    });
    let mut packager = CastPackager::new(CastPackagerConfig {
        dir: PathBuf::from(&args[2]),
        generation: 1,
        window,
        target_segment_ns: 2_000_000_000,
        video: CastVideoConfig {
            time_base: Rational::new(fps.1, fps.0),
            width: u32::try_from(size.0).unwrap(),
            height: u32::try_from(size.1).unwrap(),
            bit_rate: None,
            extradata: if extradata { sets } else { None },
        },
        audio: has_audio.then_some(CastAudioConfig {
            time_base: Rational::new(1, i64::from(rate)),
            sample_rate: rate,
            channels,
            bit_rate: None,
            audio_specific_config: None,
        }),
        epoch: None,
        wall_anchor: None,
    })
    .expect("create the packager");

    let dropped = |range: Option<(i64, i64)>, index: i64| {
        range.is_some_and(|(from, to)| (from..=to).contains(&index))
    };
    let mut next_audio = 0i64;
    let started = std::time::Instant::now();
    let total_units = units.len() * loops.max(1);
    let total_frames = frames.len() * loops.max(1);
    for position in 0..total_units {
        let unit = &units[position % units.len()];
        let index = i64::try_from(position).unwrap();
        if realtime {
            // Frame `index` is due `index * den / num` seconds after the start.
            let due_ns = u64::try_from(index * fps.1).unwrap() * 1_000_000_000
                / u64::try_from(fps.0).unwrap();
            let due = std::time::Duration::from_nanos(due_ns);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        // Audio that starts before this video frame goes first.
        let limit = i128::from(index) * i128::from(fps.1) * i128::from(rate);
        while usize::try_from(next_audio).unwrap() < total_frames
            && i128::from(next_audio) * 1024 * i128::from(fps.0) < limit
        {
            if !dropped(drop_audio, next_audio) {
                let data = frames[usize::try_from(next_audio).unwrap() % frames.len()];
                packager
                    .push_audio(AudioUnit {
                        pts: next_audio * 1024,
                        data,
                    })
                    .expect("push audio");
            }
            next_audio += 1;
        }
        if dropped(drop_video, index) {
            continue;
        }
        let parsed = cmaf::h264_access_unit(unit, None).expect("parse the access unit");
        // `--extradata`: feed the AVCC sample, which carries no SPS/PPS.
        let data: &[u8] = if extradata { &parsed.data } else { unit };
        packager
            .push_video(VideoUnit {
                pts: index,
                dts: index,
                keyframe: parsed.idr,
                config_changed: false,
                data,
            })
            .expect("push video");
    }
    let progress = packager.published();
    println!(
        "access units {}, adts frames {}, published video {} audio {} master {} ready {}",
        total_units,
        total_frames,
        progress.video_segments,
        progress.audio_segments,
        progress.master,
        packager.is_ready()
    );
}
