//! The `multiview` command-line grammar (clap derive).
//!
//! [`Cli`] is the top-level parser; [`Command`] is the subcommand union. Parsing
//! is pure and side-effect-free, so it is unit/integration-tested directly via
//! [`Cli::parse_from`] / [`clap::Parser::try_parse_from`] without spawning a
//! process.
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Conspect live media production, routing, and monitoring — command-line interface.
///
/// The default build is pure-software (no GPU, no `FFmpeg`); hardware backends are
/// compiled in via the `nvidia` / `apple` / `linux-vaapi` / `full` feature
/// presets (see the crate manifest), not selected at runtime.
#[derive(Debug, Parser)]
#[command(name = "multiview", version, about, long_about = None)]
#[non_exhaustive]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Parse arguments from an explicit iterator (the testable entrypoint).
    ///
    /// Panics on a usage error the same way [`clap::Parser::parse`] does; tests
    /// that want to assert on the error use [`clap::Parser::try_parse_from`].
    #[must_use]
    pub fn parse_from<I, T>(iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::parse_from(iter)
    }
}

/// The `multiview` subcommands.
///
/// Intentionally **not** `#[non_exhaustive]`: this is the binary's dispatch
/// point, and the `match` over it must stay exhaustive so adding a subcommand is
/// a compile error until it is wired up.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Load a config, validate it (grid-solve + cross-references), then check
    /// what the engine will REALLY start from: the accepted state
    /// (`<config-dir>/.multiview/desired.json`) when one exists, else the
    /// config, against the start-up admission rule. Prints a human-readable
    /// report and exits non-zero with start-up's own reason when either fails.
    /// Pure and side-effect-free (the deploy preflight).
    Validate(ValidateArgs),
    /// Load + validate a config, build the engine, attach built-in test-pattern
    /// sources, and run. In `--software` mode this drives the FFmpeg-free output
    /// clock for `--ticks` ticks (or until Ctrl-C) and reports cadence/frames.
    Run(RunArgs),
    /// Run as a **display node** (DEV-B5, ADR-0045): a small box that ingests
    /// one stream from a central Conspect instance, hardware-decodes it, and presents
    /// the single-source canvas straight to an attached HDMI/DisplayPort head
    /// via the `display-kms` scanout sink + ALSA HDMI audio.
    ///
    /// This slice ships the software **bootstrap/plan** surface (`--plan-only`,
    /// the default): it loads + validates the node config, resolves the
    /// keypair-bound device identity + display pairing code, and prints the
    /// resolved enrollment + presentation plan. The live ingest → scanout path
    /// and the real enrollment HTTP are hardware/network follow-ons.
    Node(NodeArgs),
    /// Front-door TLS certificates (ACME, ADR-0107 section 5a): status, the
    /// one-time setup code, credentials and issuance. Talks to the running
    /// engine's loopback `[control] listen` (bearer from
    /// `MULTIVIEW_CONTROL_TOKEN`); `status` and `set-credential` also work
    /// offline from `network.tls.state_dir`.
    #[command(subcommand)]
    Tls(TlsCommand),
}

/// Default config path for `multiview tls` (the deploy image's config).
pub const DEFAULT_CONFIG: &str = "/etc/multiview/multiview.toml";

/// The `multiview tls` subcommands.
#[derive(Debug, Subcommand)]
pub enum TlsCommand {
    /// Show the certificate state, CA, names, expiry, fingerprint, alarm,
    /// last error and reasons. Reads `state_dir` offline when the engine is
    /// not running.
    Status(TlsStatusArgs),
    /// Mint a fresh one-time setup code (valid 1 h, single use).
    SetupCode(TlsConfigArg),
    /// Store a credential named by the config, read from stdin (no echo on a
    /// TTY; never from the command line), then start issuance.
    SetCredential(TlsSetCredentialArgs),
    /// Start an issuance attempt now (recovery).
    Issue(TlsConfigArg),
}

/// `--config` for `multiview tls` subcommands.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct TlsConfigArg {
    /// The engine's TOML config (for `[control] listen` and `[network.tls]`).
    #[arg(long, value_name = "PATH", default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,
}

/// Arguments for `multiview tls status`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct TlsStatusArgs {
    /// Config path.
    #[command(flatten)]
    pub config: TlsConfigArg,
    /// Print the status JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `multiview tls set-credential`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct TlsSetCredentialArgs {
    /// The environment-variable NAME the config references (e.g.
    /// `CLOUDFLARE_API_TOKEN`). The value is read from stdin.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Config path.
    #[command(flatten)]
    pub config: TlsConfigArg,
}

/// Arguments for `multiview validate`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct ValidateArgs {
    /// Path to the TOML configuration document to validate.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,
}

/// Arguments for `multiview node`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct NodeArgs {
    /// Path to the TOML node config (the node-side bootstrap document:
    /// controller endpoint, enrollment token, identity dir, link offset, clock
    /// mode).
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,

    /// Resolve + validate the node config and identity and print the bootstrap
    /// plan **without** starting the live ingest → scanout path. This is the
    /// default in the current slice — the live path is a hardware follow-on, so
    /// the flag is accepted explicitly and defaults on.
    #[arg(long, default_value_t = true)]
    pub plan_only: bool,
}

/// Arguments for `multiview run`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct RunArgs {
    /// Path to the TOML configuration document to run.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,

    /// Run the FFmpeg-free software engine (CPU reference compositor, built-in
    /// test-pattern sources) with no GPU or `FFmpeg` dependency. This is the
    /// software end-to-end smoke of the output-clock invariant — the same
    /// pipeline, minus the libav decoders, so it serves the API/WebUI and the
    /// program preview just like the full build, only without external ingest.
    ///
    /// Without this flag (and built with the `ffmpeg` feature), `run` builds the
    /// full libav* pipeline: ingest -> per-tile framestores -> the engine drive
    /// loop -> encode the canvas once -> fan out to the configured file/HLS
    /// outputs.
    ///
    /// `--headless` is accepted as a back-compat alias.
    #[arg(long, alias = "headless")]
    pub software: bool,

    /// Stop after this many output ticks (frames). Omit to run until Ctrl-C (or,
    /// for a bounded run, give `--duration` instead).
    #[arg(long, value_name = "N")]
    pub ticks: Option<u64>,

    /// Stop after this many seconds of output (converted to an exact whole
    /// number of ticks at the canvas cadence). Mutually informative with
    /// `--ticks`; if both are given, `--ticks` wins.
    #[arg(long, value_name = "SECS")]
    pub duration: Option<u64>,

    /// Burn an external SRT/`WebVTT` subtitle file into the program: the active
    /// cue is rendered (bottom-centre) on every output frame while it is on
    /// screen. Requires the `ffmpeg` + `overlay` features; ignored otherwise.
    /// The format is chosen by the file extension (`.vtt` ⇒ `WebVTT`, else SRT).
    #[arg(long, value_name = "FILE")]
    pub subtitles: Option<PathBuf>,

    /// Mux a **program-audio** elementary stream alongside the video (AUD-4): the
    /// output container gains a second (AAC) stream carrying the mixed program
    /// bus. Default OFF — without this flag the output is video-only and
    /// byte-identical to before. The program audio is silence until per-source
    /// audio decode is wired (a later slice), but it is a real AAC stream.
    /// Requires the `ffmpeg` feature; ignored otherwise.
    #[arg(long)]
    pub program_audio: bool,
}

impl RunArgs {
    /// Resolve the bounded tick budget from `--ticks` / `--duration` at the
    /// given canvas `cadence` (frames per second, exact rational).
    ///
    /// `--ticks` takes precedence; otherwise `--duration` seconds is converted
    /// to an exact whole number of ticks (`secs * num / den`, rounded toward
    /// zero). Returns [`None`] for an unbounded run (neither bound supplied).
    #[must_use]
    pub fn tick_budget(&self, cadence: multiview_core::time::Rational) -> Option<u64> {
        if let Some(ticks) = self.ticks {
            return Some(ticks);
        }
        let secs = self.duration?;
        // ticks = secs * fps = secs * num / den, exact integer arithmetic on
        // i128 to avoid any float fps (invariant #3), clamped into u64.
        let num = i128::from(cadence.num);
        let den = i128::from(cadence.den).max(1);
        let ticks = (i128::from(secs).saturating_mul(num)) / den;
        Some(u64::try_from(ticks.max(0)).unwrap_or(u64::MAX))
    }
}
