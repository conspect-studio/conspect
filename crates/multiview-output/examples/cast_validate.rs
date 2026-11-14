//! Validate a Cast packager directory the way a player reads it.
//!
//! Run: `cargo run -p multiview-output --example cast_validate -- <dir>
//! [--json] [--window N]`
//!
//! Prints the report of `multiview_output::hls::cast_check::check_cast_dir`
//! and exits 1 when it holds errors (2 on a usage error). `--window N`
//! raises the largest allowed playlist window (default 10).
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use multiview_output::hls::cast_check::{check_cast_dir_with, CastCheckOptions, CastCheckReport};

/// A JSON string literal.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    format!("[{}]", quoted.join(","))
}

fn to_json(report: &CastCheckReport) -> String {
    format!(
        "{{\"ok\":{},\"errors\":{},\"warnings\":{},\"video_segments\":{},\"audio_segments\":{},\
         \"codecs\":{},\"target_duration\":{},\"max_extinf\":{:.6},\"media_sequence\":{},\
         \"discontinuity_sequence\":{},\"discontinuities\":{},\"has_audio\":{},\
         \"has_program_date_time\":{},\"first_video_start\":{:.6},\"first_audio_start\":{:.6}}}",
        report.is_ok(),
        json_list(&report.errors),
        json_list(&report.warnings),
        report.video_segments,
        report.audio_segments,
        json_string(&report.codecs),
        report.target_duration,
        report.max_extinf,
        report.media_sequence,
        report.discontinuity_sequence,
        report.discontinuities,
        report.has_audio,
        report.has_program_date_time,
        report.first_video_start,
        report.first_audio_start,
    )
}

fn print_text(report: &CastCheckReport) {
    println!(
        "codecs            {}\nvideo segments    {}\naudio segments    {}\ntarget duration   {} s\n\
         max EXTINF        {:.6} s\nmedia sequence    {}\ndiscontinuities   {} listed, sequence \
         {}\nprogram date time {}\nfirst start       video {:.6} s, audio {:.6} s",
        report.codecs,
        report.video_segments,
        report.audio_segments,
        report.target_duration,
        report.max_extinf,
        report.media_sequence,
        report.discontinuities,
        report.discontinuity_sequence,
        if report.has_program_date_time { "yes" } else { "no" },
        report.first_video_start,
        report.first_audio_start,
    );
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for error in &report.errors {
        println!("error: {error}");
    }
    println!(
        "{}",
        if report.is_ok() {
            "OK: the directory is a valid cast stream".to_owned()
        } else {
            format!("FAILED: {} error(s)", report.errors.len())
        }
    );
}

fn main() -> ExitCode {
    let mut dir: Option<PathBuf> = None;
    let mut json = false;
    let mut options = CastCheckOptions::default();
    let mut usage_error = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--json" {
            json = true;
        } else if arg == "--window" {
            match args.next().and_then(|value| value.parse().ok()) {
                Some(window) => options.max_window = window,
                None => usage_error = true,
            }
        } else if dir.is_none() && !arg.starts_with("--") {
            dir = Some(PathBuf::from(arg));
        } else {
            usage_error = true;
        }
    }
    let Some(dir) = dir.filter(|_| !usage_error) else {
        eprintln!("usage: cast_validate <dir> [--json] [--window N]");
        return ExitCode::from(2);
    };
    let report = check_cast_dir_with(&dir, options);
    if json {
        println!("{}", to_json(&report));
    } else {
        print_text(&report);
    }
    if report.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
