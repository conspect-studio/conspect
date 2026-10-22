//! Argument-parsing tests for the `multiview node` subcommand (DEV-B5,
//! [ADR-0045]).
//!
//! These assert the clap grammar only — subcommand selection and the `node`
//! flags. Parsing is pure and side-effect-free, exercised directly via
//! [`Cli::parse_from`] / [`clap::Parser::try_parse_from`] without spawning a
//! process or touching a network/display.
//!
//! [ADR-0045]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0045.md
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use clap::Parser as _;
use multiview_cli::cli::{Cli, Command};

#[test]
fn node_subcommand_parses_config_path() {
    let cli = Cli::parse_from(["multiview", "node", "node.toml"]);
    match cli.command {
        Command::Node(args) => {
            assert_eq!(args.config, PathBuf::from("node.toml"));
        }
        other => panic!("expected Node, got {other:?}"),
    }
}

#[test]
fn node_subcommand_plan_only_defaults_true() {
    // This slice ships the software bootstrap/plan surface; the live ingest +
    // DRM-master path is a hardware follow-on, so `--plan-only` defaults true.
    let cli = Cli::parse_from(["multiview", "node", "node.toml"]);
    match cli.command {
        Command::Node(args) => {
            assert!(
                args.plan_only,
                "plan_only must default to true (the live path is a hardware follow-on)"
            );
        }
        other => panic!("expected Node, got {other:?}"),
    }
}

#[test]
fn node_subcommand_accepts_explicit_plan_only() {
    let cli = Cli::parse_from(["multiview", "node", "node.toml", "--plan-only"]);
    match cli.command {
        Command::Node(args) => assert!(args.plan_only),
        other => panic!("expected Node, got {other:?}"),
    }
}

#[test]
fn node_subcommand_missing_config_is_a_parse_error() {
    let err = Cli::try_parse_from(["multiview", "node"]).unwrap_err();
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "a missing node config path must be a clap usage error, not a panic"
    );
}
