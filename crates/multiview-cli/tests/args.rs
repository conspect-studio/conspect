//! Argument-parsing tests for the `multiview` binary.
//!
//! These assert the clap grammar: subcommand selection, required/positional
//! arguments, and the `run` flags (`--headless`, `--ticks`). Parsing is pure
//! and side-effect-free, so it is exercised directly via [`Cli::parse_from`].
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use clap::Parser as _;
use multiview_cli::cli::{Cli, Command, RunArgs};
use multiview_core::time::Rational;

/// Build a `RunArgs` for tick-budget tests (the fields exercised are public).
fn run_args(ticks: Option<u64>, duration: Option<u64>) -> RunArgs {
    let mut cmdline = vec![
        "multiview".to_owned(),
        "run".to_owned(),
        "c.toml".to_owned(),
    ];
    if let Some(t) = ticks {
        cmdline.push("--ticks".to_owned());
        cmdline.push(t.to_string());
    }
    if let Some(d) = duration {
        cmdline.push("--duration".to_owned());
        cmdline.push(d.to_string());
    }
    match Cli::parse_from(cmdline).command {
        Command::Run(parsed) => parsed,
        Command::Validate(_) => panic!("expected Run"),
    }
}

#[test]
fn validate_subcommand_parses_path() {
    let cli = Cli::parse_from(["multiview", "validate", "examples/2x2.toml"]);
    match cli.command {
        Command::Validate(args) => {
            assert_eq!(args.config, PathBuf::from("examples/2x2.toml"));
        }
        other @ Command::Run(_) => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn run_subcommand_defaults() {
    let cli = Cli::parse_from(["multiview", "run", "examples/2x2.toml"]);
    match cli.command {
        Command::Run(args) => {
            assert_eq!(args.config, PathBuf::from("examples/2x2.toml"));
            assert!(!args.software, "software must default to false");
            assert_eq!(args.ticks, None, "ticks must default to None (run forever)");
        }
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn run_subcommand_software_with_ticks() {
    let cli = Cli::parse_from([
        "multiview",
        "run",
        "examples/2x2.toml",
        "--software",
        "--ticks",
        "120",
    ]);
    match cli.command {
        Command::Run(args) => {
            assert!(args.software);
            assert_eq!(args.ticks, Some(120));
        }
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn run_subcommand_headless_is_a_back_compat_alias_for_software() {
    // `--headless` is retained as a hidden alias and must map onto `software`.
    let cli = Cli::parse_from(["multiview", "run", "examples/2x2.toml", "--headless"]);
    match cli.command {
        Command::Run(args) => assert!(
            args.software,
            "the --headless alias must set the software flag"
        ),
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn run_subcommand_subtitles_flag_parses() {
    // Default: no subtitles attached.
    let cli = Cli::parse_from(["multiview", "run", "examples/2x2.toml"]);
    match cli.command {
        Command::Run(args) => assert_eq!(
            args.subtitles, None,
            "subtitles must default to None when --subtitles is absent"
        ),
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }

    // Explicit --subtitles captures the file path.
    let cli = Cli::parse_from([
        "multiview",
        "run",
        "examples/2x2.toml",
        "--subtitles",
        "captions.srt",
    ]);
    match cli.command {
        Command::Run(args) => assert_eq!(
            args.subtitles,
            Some(PathBuf::from("captions.srt")),
            "--subtitles FILE must be captured as the cue-track path"
        ),
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn missing_config_is_a_parse_error() {
    let err = Cli::try_parse_from(["multiview", "validate"]).unwrap_err();
    // clap reports the missing required positional argument.
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "a missing config path must be a clap usage error, not a panic"
    );
}

#[test]
fn unknown_subcommand_is_a_parse_error() {
    let err = Cli::try_parse_from(["multiview", "frobnicate", "x"]).unwrap_err();
    assert!(matches!(
        err.kind(),
        clap::error::ErrorKind::InvalidSubcommand | clap::error::ErrorKind::UnknownArgument
    ));
}

#[test]
fn ticks_must_be_a_number() {
    let err =
        Cli::try_parse_from(["multiview", "run", "c.toml", "--ticks", "notanumber"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}

#[test]
fn duration_seconds_resolve_to_exact_whole_ticks() {
    // 5 s at exactly 25 fps -> 125 ticks (exact integer, never a float fps).
    let args = run_args(None, Some(5));
    assert_eq!(args.tick_budget(Rational::new(25, 1)), Some(125));
    // 5 s at 30000/1001 (29.97) -> floor(5 * 30000 / 1001) = 149 ticks.
    assert_eq!(args.tick_budget(Rational::new(30_000, 1001)), Some(149));
}

#[test]
fn ticks_takes_precedence_over_duration() {
    let args = run_args(Some(7), Some(100));
    assert_eq!(
        args.tick_budget(Rational::new(25, 1)),
        Some(7),
        "--ticks must win over --duration when both are given"
    );
}

#[test]
fn no_bound_means_run_forever() {
    let args = run_args(None, None);
    assert_eq!(
        args.tick_budget(Rational::new(25, 1)),
        None,
        "neither --ticks nor --duration means an unbounded run"
    );
}
