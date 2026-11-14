//! The thin **subprocess** half of the `YouTube` resolver (ADR-0015 §6): discover
//! and spawn `yt-dlp`, capture its output, and hand the JSON to the pure
//! [`super::resolve::parse_info_dict`]. The spawn is a thin shell — all parsing
//! correctness lives in the pure layer.
//!
//! Hardening (ADR-0015 §6): spawned with an **argument vector** (no shell, no
//! interpolation); a **hard timeout** kills a hung process rather than awaiting
//! it (invariant #10); stderr is captured for diagnosis.
//!
//! Player clients (issue #420): no client is pinned. The first attempt uses
//! `yt-dlp`'s own current default clients (they track what `YouTube` serves);
//! only when that attempt fails does one fallback attempt try the explicit
//! [`FALLBACK_PLAYER_CLIENTS`] list. A pinned client went stale on 14 Nov 2026
//! (`web_safari` answered "No video formats found!"), which is why the default
//! comes first. When a `node` binary is found, `--js-runtimes node:<path>` is
//! passed so the `n`-signature can be solved (`yt-dlp` enables only Deno by
//! default); an older `yt-dlp` that rejects the flag is retried without it.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::{resolve::parse_info_dict, ResolvedHls, YoutubeError};

/// The explicit player clients the single fallback attempt asks for when the
/// default-client attempt fails (issue #420). `ios` is deliberately absent: it
/// needs a PO token for live HLS (ADR-0015 §5).
pub const FALLBACK_PLAYER_CLIENTS: &str = "default,web_safari,tv";

/// Upper bound on the bytes kept from one `yt-dlp -J` stdout. A live
/// info-dict is well under 2 MiB; anything larger is refused rather than
/// buffered without bound.
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;

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
    /// Hard per-invocation deadline; on expiry the child is killed. A resolve
    /// runs at most four invocations (default clients and fallback clients,
    /// each possibly repeated once without `--js-runtimes` on an old
    /// `yt-dlp`), each under this deadline.
    pub timeout: Duration,
    /// The JavaScript runtime handed to `yt-dlp` as `--js-runtimes node:<path>`
    /// (an absolute path). `None` passes no runtime flag. [`Self::default`] and
    /// [`Self::new`] fill it from [`find_node`].
    pub js_runtime: Option<String>,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            yt_dlp_path: "yt-dlp".to_owned(),
            timeout: DEFAULT_TIMEOUT,
            js_runtime: find_node(),
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
            js_runtime: find_node(),
        }
    }

    /// Replace the JavaScript runtime path (`None` disables the flag).
    #[must_use]
    pub fn with_js_runtime(mut self, js_runtime: Option<String>) -> Self {
        self.js_runtime = js_runtime;
        self
    }
}

/// Find an executable `node` on `PATH`, returning its absolute path. Pure
/// filesystem lookup (no subprocess). `None` when absent.
#[must_use]
pub fn find_node() -> Option<String> {
    find_on_path("node", std::env::var_os("PATH").as_deref())
}

/// Find `name` as an executable regular file in the `PATH`-style `path` list.
#[must_use]
pub fn find_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<String> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
        .and_then(|found| found.to_str().map(str::to_owned))
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// The argument vector of one `yt-dlp -J` attempt (without the program name):
/// `-J --no-warnings [--js-runtimes node:<path>] [--extractor-args
/// youtube:player_client=<clients>] -- <url>`. `url` is always after `--`, so
/// a URL starting with `-` can never be read as an option.
#[must_use]
pub fn resolve_args(
    js_runtime: Option<&str>,
    player_clients: Option<&str>,
    url: &str,
) -> Vec<String> {
    let mut args = vec!["-J".to_owned(), "--no-warnings".to_owned()];
    if let Some(node) = js_runtime {
        args.push("--js-runtimes".to_owned());
        args.push(format!("node:{node}"));
    }
    if let Some(clients) = player_clients {
        args.push("--extractor-args".to_owned());
        args.push(format!("youtube:player_client={clients}"));
    }
    args.push("--".to_owned());
    args.push(url.to_owned());
    args
}

/// Whether a failed `yt-dlp` run rejected the `--js-runtimes` option itself
/// (an older `yt-dlp`), so the attempt is worth repeating without it.
fn rejected_js_runtimes(stderr: &str) -> bool {
    stderr.contains("no such option") && stderr.contains("js-runtimes")
}

/// Truncate captured stderr to a bounded length for logging, so a misbehaving
/// `yt-dlp` cannot flood the log. (Secret material is never placed on the
/// command line — cookies are secret-ref only, ADR-M006 — so there is nothing to
/// redact from our own argv; this only bounds size.)
fn bounded_stderr(stderr: &[u8]) -> String {
    bounded_text(&String::from_utf8_lossy(stderr))
}

/// [`bounded_stderr`] over text already decoded.
fn bounded_text(text: &str) -> String {
    const MAX: usize = 2048;
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
    // The same capped, deadline-bounded run as `-J` (both pipes drained under
    // byte caps; the child is killed on overflow or timeout).
    let stdout = match run_once(config, &["--version".to_owned()]).await {
        Ok(stdout) => stdout,
        Err(YoutubeError::Resolve(stderr)) => {
            return Err(YoutubeError::Unavailable(format!(
                "yt-dlp --version exited unsuccessfully: {}",
                super::redact_urls(&stderr)
            )))
        }
        Err(YoutubeError::Unavailable(reason)) => {
            return Err(YoutubeError::Unavailable(
                reason.replace("yt-dlp -J", "yt-dlp --version"),
            ))
        }
        Err(other) => return Err(other),
    };
    // A version is one short line; anything else is not a usable yt-dlp.
    let version = stdout.trim();
    if version.is_empty() || version.len() > 64 || version.contains(char::is_control) {
        return Err(YoutubeError::Unavailable(
            "yt-dlp --version produced no usable version".to_owned(),
        ));
    }
    Ok(version.to_owned())
}

/// Read `pipe` to its end, keeping at most `cap` bytes; `Err` once more than
/// `cap` arrive (the caller then kills the child). Never allocates past `cap`.
async fn read_capped<R>(pipe: Option<R>, cap: usize) -> Result<Vec<u8>, ()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut kept = Vec::new();
    let mut chunk = vec![0_u8; 8192];
    loop {
        let n = pipe.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            return Ok(kept);
        }
        if kept.len().saturating_add(n) > cap {
            return Err(());
        }
        kept.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
}

/// Upper bound on the stderr bytes read from one `yt-dlp` run; only the
/// first [`bounded_stderr`] characters are kept for the message.
const MAX_STDERR_BYTES: usize = 256 * 1024;

/// One `yt-dlp -J` run: `Ok(stdout)` on success, else the classified error
/// ([`YoutubeError::Unavailable`] for spawn failure/timeout,
/// [`YoutubeError::Resolve`] carrying bounded stderr for a non-zero exit or
/// an over-limit output). Both pipes are drained concurrently under byte caps
/// while the child runs, so memory stays bounded; an overflow on one pipe
/// stops keeping its bytes, and the run ends when the other pipe closes or at
/// the deadline, whichever is first. On overflow, timeout or cancellation the
/// child is killed (`kill_on_drop`) and reaped by tokio.
async fn run_once(config: &ResolverConfig, args: &[String]) -> Result<String, YoutubeError> {
    let mut child = Command::new(&config.yt_dlp_path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| YoutubeError::Unavailable(e.to_string()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // One deadline over draining both pipes AND reaping the child.
    let run = async {
        let (out, err) = tokio::join!(
            read_capped(stdout, MAX_STDOUT_BYTES),
            read_capped(stderr, MAX_STDERR_BYTES)
        );
        let (Ok(out), Ok(err)) = (out, err) else {
            return Err(YoutubeError::Resolve(format!(
                "yt-dlp -J output exceeded {MAX_STDOUT_BYTES} bytes"
            )));
        };
        let status = child
            .wait()
            .await
            .map_err(|e| YoutubeError::Unavailable(e.to_string()))?;
        Ok((status, out, err))
    };
    // On timeout or overflow `run` is dropped, and with it `child`, which
    // kills the process (`kill_on_drop`).
    let (status, out, err) = timeout(config.timeout, run)
        .await
        .map_err(|_| YoutubeError::Unavailable("yt-dlp -J timed out".to_owned()))??;
    if !status.success() {
        return Err(YoutubeError::Resolve(bounded_stderr(&err)));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// One attempt with a given client list, retrying once without
/// `--js-runtimes` when this `yt-dlp` does not know that option.
async fn attempt(
    config: &ResolverConfig,
    clients: Option<&str>,
    url: &str,
) -> Result<ResolvedHls, YoutubeError> {
    let js = config.js_runtime.as_deref();
    let first = run_once(config, &resolve_args(js, clients, url)).await;
    let json = match first {
        Err(YoutubeError::Resolve(stderr)) if js.is_some() && rejected_js_runtimes(&stderr) => {
            run_once(config, &resolve_args(None, clients, url)).await?
        }
        other => other?,
    };
    parse_info_dict(&json)
}

/// Resolve a `YouTube` watch/live URL to a live HLS master by spawning
/// `yt-dlp -J` and parsing its info-dict.
///
/// Attempt 1 uses `yt-dlp`'s default player clients. When it fails with an
/// extraction error or no HLS master, attempt 2 asks for
/// [`FALLBACK_PLAYER_CLIENTS`]. A spawn failure/timeout (binary missing or
/// hung) and a "not live" classification are final: another client cannot
/// fix either. Each invocation is bounded by [`ResolverConfig::timeout`]; a
/// hung process is killed. `--live-from-start` is deliberately **not** passed
/// (we tail the live edge). `url` is a single argument after `--` with no
/// shell interpolation. When both attempts fail the error names both reasons.
///
/// # Errors
///
/// - [`YoutubeError::Unavailable`] if `yt-dlp` cannot be spawned or times out.
/// - [`YoutubeError::Resolve`] if every attempt exits non-zero.
/// - the [`parse_info_dict`] errors ([`YoutubeError::Json`] /
///   [`YoutubeError::NotLive`] / [`YoutubeError::NoHlsMaster`]) on its output.
pub async fn resolve(config: &ResolverConfig, url: &str) -> Result<ResolvedHls, YoutubeError> {
    let first = match attempt(config, None, url).await {
        Ok(resolved) => return Ok(resolved),
        Err(e @ (YoutubeError::Unavailable(_) | YoutubeError::NotLive(_))) => return Err(e),
        Err(e) => e,
    };
    match attempt(config, Some(FALLBACK_PLAYER_CLIENTS), url).await {
        Ok(resolved) => Ok(resolved),
        Err(YoutubeError::Resolve(second)) => {
            let first = match first {
                YoutubeError::Resolve(text) => text,
                other => other.to_string(),
            };
            Err(YoutubeError::Resolve(bounded_text(&format!(
                "{first} | fallback clients ({FALLBACK_PLAYER_CLIENTS}): {second}"
            ))))
        }
        Err(other) => Err(other),
    }
}
