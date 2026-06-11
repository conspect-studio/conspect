//! Developer automation for the Multiview workspace: `cargo xtask <task>`.
//!
//! This is a developer-facing command-line tool (not engine or data-plane code), so
//! it legitimately writes to stdout/stderr and sets a process exit code. The
//! workspace-wide bans on those are relaxed here with justified `#[allow]`s.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // reason: xtask is a dev CLI whose entire job is to report to the terminal.
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use utoipa::OpenApi;

/// Relative path (from the workspace root) of the generated `OpenAPI` document.
const OPENAPI_OUT: &str = "docs/api/openapi.json";

/// Relative path (from the workspace root) of the generated `AsyncAPI` document.
const ASYNCAPI_OUT: &str = "docs/api/asyncapi.json";

/// Relative path (from the workspace root, gitignored) of the emitted iptv soak
/// source manifest. `.multiview-build/` is the transient build working dir and
/// is never committed — the resolved real stream URLs MUST NOT enter git.
const IPTV_SOAK_OUT: &str = ".multiview-build/iptv-soak-sources.json";

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    match task.as_str() {
        "help" => {
            println!("xtask — Multiview developer automation");
            println!("  gen-openapi    write the OpenAPI 3.1 document to {OPENAPI_OUT}");
            println!("  gen-asyncapi   write the AsyncAPI 3.0 document to {ASYNCAPI_OUT}");
            println!(
                "  soak-iptv      resolve a quirk-tagged, liveness-probed set of REAL iptv-org"
            );
            println!("                 test sources → {IPTV_SOAK_OUT} (needs network; build with");
            println!("                 `--features net`). Aliases: iptv-sources.");
            ExitCode::SUCCESS
        }
        "gen-openapi" => match gen_openapi() {
            Ok(path) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("gen-openapi failed: {err}");
                ExitCode::FAILURE
            }
        },
        "gen-asyncapi" => match gen_asyncapi() {
            Ok(path) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("gen-asyncapi failed: {err}");
                ExitCode::FAILURE
            }
        },
        "soak-iptv" | "iptv-sources" => match soak_iptv() {
            Ok(path) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("soak-iptv failed: {err}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unknown task: {other}");
            ExitCode::from(2)
        }
    }
}

/// Resolve a quirk-tagged, liveness-probed soak source set from iptv-org and
/// write the manifest to the gitignored [`IPTV_SOAK_OUT`] path; print the
/// summary table. Requires the `net` feature (a live network fetch + probe);
/// without it the task explains how to enable it rather than producing a stub.
#[cfg(feature = "net")]
fn soak_iptv() -> Result<PathBuf, IptvSoakError> {
    use std::time::Duration;
    use xtask::iptv::{select_sources, Blocklist, HttpCatalog, HttpProber, Plan};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(IptvSoakError::Runtime)?;

    // Load a local blocklist if one is present at the workspace root; otherwise
    // run with an empty blocklist (NSFW is filtered unconditionally regardless).
    let blocklist_path = workspace_root().join("blocklist.json");
    let blocklist = match std::fs::read_to_string(&blocklist_path) {
        Ok(json) => Blocklist::from_json(&json).map_err(IptvSoakError::Selection)?,
        Err(_) => Blocklist::empty(),
    };

    // Seed from wall-clock so each run draws a fresh stratified sample; the
    // sampler stays deterministic for any fixed seed (proven in the tests).
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let plan = Plan {
        seed,
        oversample: 256,
        keep_live: 24,
        keep_dead: 4,
    };

    let catalog = HttpCatalog::new(Duration::from_secs(30));
    let prober = HttpProber::new(Duration::from_secs(8));

    let manifest = runtime
        .block_on(select_sources(&catalog, &prober, &blocklist, &plan))
        .map_err(IptvSoakError::Selection)?;

    let out = workspace_root().join(IPTV_SOAK_OUT);
    manifest.write_to(&out).map_err(IptvSoakError::Selection)?;
    print!("{}", manifest.summary_table());
    Ok(out)
}

/// The `net`-disabled stub: refuse cleanly with instructions (NOT a `todo!()`).
/// The pure selection logic is always compiled + unit-tested; only the live
/// fetch/probe needs the network crate, which is opt-in.
#[cfg(not(feature = "net"))]
fn soak_iptv() -> Result<PathBuf, IptvSoakError> {
    Err(IptvSoakError::NetFeatureDisabled)
}

/// Failure modes of the `soak-iptv` task.
#[derive(Debug)]
enum IptvSoakError {
    /// The `net` feature (live HTTP client) is not compiled in.
    #[cfg_attr(feature = "net", allow(dead_code))]
    NetFeatureDisabled,
    /// The async runtime could not be built (only with `net`).
    #[cfg_attr(not(feature = "net"), allow(dead_code))]
    Runtime(std::io::Error),
    /// The selection pipeline (fetch/parse/probe/emit) failed.
    #[cfg_attr(not(feature = "net"), allow(dead_code))]
    Selection(xtask::iptv::IptvError),
}

impl std::fmt::Display for IptvSoakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetFeatureDisabled => write!(
                f,
                "the live iptv-org fetch+probe needs the network: rebuild with \
                 `cargo run -p xtask --features net -- soak-iptv` (run where there is network)"
            ),
            Self::Runtime(source) => write!(f, "building the async runtime: {source}"),
            Self::Selection(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for IptvSoakError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NetFeatureDisabled => None,
            Self::Runtime(source) => Some(source),
            Self::Selection(source) => Some(source),
        }
    }
}

/// Serialize `multiview-control`'s utoipa [`OpenApi`] document to pretty JSON and
/// write it to `docs/api/openapi.json` (relative to the workspace root),
/// creating the directory if needed. Returns the path written.
fn gen_openapi() -> Result<PathBuf, GenError> {
    let doc = multiview_control::openapi::ApiDoc::openapi();
    let json = doc.to_pretty_json().map_err(GenError::Serialize)?;

    let out = workspace_root().join(OPENAPI_OUT);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|source| GenError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // Write a trailing newline so the file is POSIX-clean and diff-friendly.
    let mut contents = json;
    contents.push('\n');
    std::fs::write(&out, contents).map_err(|source| GenError::Write {
        path: out.clone(),
        source,
    })?;
    Ok(out)
}

/// Generate the `AsyncAPI` 3.0 document from [`multiview_events::asyncapi`] and
/// write it to `docs/api/asyncapi.json` (relative to the workspace root),
/// creating the directory if needed. Returns the path written.
///
/// The output is deterministic (re-running yields an identical file); the CI
/// drift-gate regenerates and fails on any diff (ADR-RT006 Decision).
fn gen_asyncapi() -> Result<PathBuf, GenError> {
    let contents = multiview_events::asyncapi::generate_asyncapi_document();

    let out = workspace_root().join(ASYNCAPI_OUT);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|source| GenError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // `generate_asyncapi_document` already appends a trailing newline.
    std::fs::write(&out, contents).map_err(|source| GenError::Write {
        path: out.clone(),
        source,
    })?;
    Ok(out)
}

/// The workspace root, derived from this crate's manifest dir (`xtask/`) at
/// compile time so the task can be run from any working directory.
fn workspace_root() -> &'static Path {
    // CARGO_MANIFEST_DIR is `<root>/xtask`; the workspace root is its parent.
    let xtask_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask_dir.parent().unwrap_or(xtask_dir)
}

/// Failure modes of `gen-openapi`.
#[derive(Debug)]
enum GenError {
    /// The `OpenAPI` document could not be serialized to JSON.
    Serialize(serde_json::Error),
    /// The output directory could not be created.
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The output file could not be written.
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(source) => write!(f, "serializing OpenAPI to JSON: {source}"),
            Self::CreateDir { path, source } => {
                write!(f, "creating directory {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "writing {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for GenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(source) => Some(source),
            Self::CreateDir { source, .. } | Self::Write { source, .. } => Some(source),
        }
    }
}
