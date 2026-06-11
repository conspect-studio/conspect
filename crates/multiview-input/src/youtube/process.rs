//! The thin **subprocess** half of the `YouTube` resolver (ADR-0015 §6): discover
//! and spawn `yt-dlp`, capture its output, and hand the JSON to the pure
//! [`super::resolve::parse_info_dict`]. The spawn is a thin shell — all parsing
//! correctness lives in the pure layer.
//!
//! Hardening (ADR-0015 §6): spawned with an **argument vector** (no shell, no
//! interpolation); a **hard timeout** kills a hung process rather than awaiting
//! it (invariant #10); stderr is captured into `tracing` for diagnosis. The
//! `web_safari` player client is pinned (PO-token-free live HLS); `ios` is
//! avoided.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::{resolve::parse_info_dict, ResolvedHls, YoutubeError};

/// The pinned player client: a current default that yields PO-token-free live
/// HLS (ADR-0015 §5). `ios` is deliberately avoided.
const PLAYER_CLIENT_ARG: &str = "youtube:player_client=web_safari";

/// Default hard deadline for one `yt-dlp` invocation. A slower/hung process is
/// killed, never awaited (invariant #10).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How a `yt-dlp` invocation is located and bounded.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResolverConfig {
    /// The `yt-dlp` binary to run — an explicit path, or just `"yt-dlp"` to
    /// resolve it on `PATH`.
    pub yt_dlp_path: String,
    /// Hard per-invocation deadline; on expiry the child is killed.
    pub timeout: Duration,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            yt_dlp_path: "yt-dlp".to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl ResolverConfig {
    /// Build a config for an explicit `yt-dlp` path and per-invocation timeout.
    ///
    /// (`ResolverConfig` is `#[non_exhaustive]`; this constructor lets callers in
    /// other crates build one without a struct literal.)
    #[must_use]
    pub fn new(yt_dlp_path: impl Into<String>, timeout: Duration) -> Self {
        Self {
            yt_dlp_path: yt_dlp_path.into(),
            timeout,
        }
    }
}

/// Truncate captured stderr to a bounded length for logging, so a misbehaving
/// `yt-dlp` cannot flood the log. (Secret material is never placed on the
/// command line — cookies are secret-ref only, ADR-M006 — so there is nothing to
/// redact from our own argv; this only bounds size.)
fn bounded_stderr(stderr: &[u8]) -> String {
    const MAX: usize = 2048;
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    match trimmed.char_indices().nth(MAX) {
        Some((byte_idx, _)) => match trimmed.get(..byte_idx) {
            Some(head) => format!("{head}… (truncated)"),
            None => trimmed.to_owned(),
        },
        None => trimmed.to_owned(),
    }
}

/// Probe `yt-dlp --version`, returning the trimmed version string when the binary
/// is present and runnable, or [`YoutubeError::Unavailable`] when it is absent —
/// **never** an error that crashes the engine (mirrors the NDI capability model).
///
/// # Errors
///
/// [`YoutubeError::Unavailable`] when the binary cannot be spawned, times out, or
/// exits non-zero (the capability is simply reported unavailable).
pub async fn probe_version(config: &ResolverConfig) -> Result<String, YoutubeError> {
    let spawn = Command::new(&config.yt_dlp_path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = match timeout(config.timeout, spawn).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(YoutubeError::Unavailable(e.to_string())),
        Err(_) => {
            return Err(YoutubeError::Unavailable(
                "yt-dlp --version timed out".to_owned(),
            ))
        }
    };

    if !output.status.success() {
        return Err(YoutubeError::Unavailable(format!(
            "yt-dlp --version exited unsuccessfully: {}",
            bounded_stderr(&output.stderr)
        )));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err(YoutubeError::Unavailable(
            "yt-dlp --version produced no output".to_owned(),
        ));
    }
    Ok(version)
}

/// Resolve a `YouTube` watch/live URL to a live HLS master by spawning
/// `yt-dlp -J` and parsing its info-dict.
///
/// The invocation pins the `web_safari` player client, disables warnings, and is
/// bounded by [`ResolverConfig::timeout`]; a hung process is killed. `--live-from-start`
/// is deliberately **not** passed (we tail the live edge). `url` is passed as a
/// single argument with no shell interpolation.
///
/// # Errors
///
/// - [`YoutubeError::Unavailable`] if `yt-dlp` cannot be spawned or times out.
/// - [`YoutubeError::Resolve`] if it exits non-zero (extraction failure).
/// - the [`parse_info_dict`] errors ([`YoutubeError::Json`] /
///   [`YoutubeError::NotLive`] / [`YoutubeError::NoHlsMaster`]) on its output.
pub async fn resolve(config: &ResolverConfig, url: &str) -> Result<ResolvedHls, YoutubeError> {
    let spawn = Command::new(&config.yt_dlp_path)
        .args([
            "-J",
            "--no-warnings",
            "--extractor-args",
            PLAYER_CLIENT_ARG,
            "--",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = match timeout(config.timeout, spawn).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(YoutubeError::Unavailable(e.to_string())),
        Err(_) => return Err(YoutubeError::Unavailable("yt-dlp -J timed out".to_owned())),
    };

    if !output.status.success() {
        return Err(YoutubeError::Resolve(bounded_stderr(&output.stderr)));
    }

    let json = String::from_utf8_lossy(&output.stdout);
    parse_info_dict(&json)
}
