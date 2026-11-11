//! Open a local capture device through libavdevice and read a few packets —
//! a hardware smoke check for `Demuxer::open_device` (macOS avfoundation,
//! Windows dshow). Usage: `device_probe <format> <device> [key=value ...]`.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::time::{Duration, Instant};

use multiview_ffmpeg::{DemuxOptions, Demuxer};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(format), Some(device)) = (args.next(), args.next()) else {
        eprintln!("usage: device_probe <format> <device> [key=value ...]");
        std::process::exit(2);
    };
    let opts: Vec<(String, String)> = args
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect();
    let started = Instant::now();
    let mut demux = match Demuxer::open_device(
        &format,
        &device,
        // Per-read deadline only; the open itself is not bounded by libav.
        DemuxOptions::new().with_rw_timeout(Duration::from_secs(5)),
        &opts,
    ) {
        Ok(d) => d,
        Err(e) => {
            println!("OPEN_FAILED after {:?}: {e}", started.elapsed());
            std::process::exit(1);
        }
    };
    println!(
        "opened in {:?}; streams: {:?}",
        started.elapsed(),
        demux.streams()
    );
    let mut packets = 0_u32;
    let t = Instant::now();
    while packets < 60 && t.elapsed() < Duration::from_secs(10) {
        match demux.read_packet() {
            Ok(Some(_)) => packets += 1,
            Ok(None) => break,
            Err(e) => {
                println!("READ_FAILED: {e}");
                break;
            }
        }
    }
    println!("read {packets} packets in {:?}", t.elapsed());
}
