//! UDP flood generator for front-door chaos runs (issue #289): run it from a
//! second host against a real door.
//!
//! ```text
//! cargo run --release -p multiview-frontdoor --example udp443_flood -- \
//!     --target [2001:db8::10]:443 --pps 600000 --secs 60 --threads 8 \
//!     --sources v6-rotating=2001:db8:: --shapes all
//! ```
//!
//! `--sources one|many-v4|v6-rotating[=prefix]` (default `one`);
//! `--shapes all|garbage,initial,vn,short,stun,rtp` (default `all`). Prints
//! the achieved datagrams per second each second.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use multiview_frontdoor::flood::{Flood, FloodConfig, Shape, Sources};

const USAGE: &str = "usage: udp443_flood --target [addr]:port [--pps N] [--secs N] \
                     [--threads N] [--sources one|many-v4|v6-rotating[=prefix]] \
                     [--shapes all|garbage,initial,vn,short,stun,rtp]";

struct Args {
    target: SocketAddr,
    pps: u64,
    secs: u64,
    threads: usize,
    sources: Sources,
    shapes: Vec<Shape>,
}

fn parse() -> Result<Args, String> {
    let mut target = None;
    let mut pps = 10_000u64;
    let mut secs = 10u64;
    let mut threads = 4usize;
    let mut sources = None;
    let mut shapes = Shape::ALL.to_vec();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--target" => {
                target = Some(
                    value()?
                        .parse::<SocketAddr>()
                        .map_err(|e| format!("--target: {e}"))?,
                );
            }
            "--pps" => pps = value()?.parse().map_err(|e| format!("--pps: {e}"))?,
            "--secs" => secs = value()?.parse().map_err(|e| format!("--secs: {e}"))?,
            "--threads" => threads = value()?.parse().map_err(|e| format!("--threads: {e}"))?,
            "--sources" => {
                let v = value()?;
                sources = Some(match v.as_str() {
                    "one" => None,
                    "many-v4" => Some(Sources::ManyV4Loopback),
                    "v6-rotating" => Some(Sources::RotatingV6 {
                        prefix: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
                    }),
                    other => {
                        let prefix = other
                            .strip_prefix("v6-rotating=")
                            .ok_or_else(|| format!("--sources: unknown {other:?}"))?
                            .parse()
                            .map_err(|e| format!("--sources prefix: {e}"))?;
                        Some(Sources::RotatingV6 { prefix })
                    }
                });
            }
            "--shapes" => {
                let v = value()?;
                if v != "all" {
                    shapes = v
                        .split(',')
                        .map(str::parse)
                        .collect::<Result<Vec<Shape>, String>>()?;
                }
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown flag {other:?}\n{USAGE}")),
        }
    }
    let target = target.ok_or_else(|| format!("--target is required\n{USAGE}"))?;
    let sources = sources.flatten().unwrap_or_else(|| {
        // One source: an ephemeral socket of the target's family.
        let bind = if target.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        Sources::One(bind.parse().unwrap_or(target))
    });
    Ok(Args {
        target,
        pps,
        secs,
        threads,
        sources,
        shapes,
    })
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let flood = match Flood::start(FloodConfig {
        target: args.target,
        pps: args.pps,
        threads: args.threads,
        sources: args.sources.clone(),
        shapes: args.shapes.clone(),
    }) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("flood setup failed ({:?}): {e}", args.sources);
            std::process::exit(1);
        }
    };
    let start = Instant::now();
    let mut last = 0u64;
    for second in 1..=args.secs {
        let next = start + Duration::from_secs(second);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let sent = flood.sent();
        println!("{second:>4} s  {:>9} pps", sent.saturating_sub(last));
        last = sent;
    }
    let total = flood.stop();
    println!(
        "sent {total} datagrams in {:.1} s to {} ({:?}, shapes {:?})",
        start.elapsed().as_secs_f64(),
        args.target,
        args.sources,
        args.shapes
    );
}
