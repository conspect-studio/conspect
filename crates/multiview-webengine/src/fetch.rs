//! URL-producer fetch: real video bytes via the embedded `fetch_host.cjs`
//! (contract: `docs/architecture/web-fetch-host-contract.md`) committed into the
//! same [`ArtifactStore`] as captures. Management-worker calls only.
//!
//! The OS egress boundary is a trusted-controller wrapper that is REQUIRED: with
//! no valid wrapper and token nothing is reserved, spawned or written. Success
//! needs the host's `done {ok:true,bytes}` marker whose count equals the bytes the
//! pump actually forwarded AND a zero exit, before the pending write completes.
//! Transfer, cancellation, deadline and process-group cleanup share the bounded
//! nonblocking pump in [`crate::runner`].
use crate::{
    artifact::{Artifact, ArtifactId, ArtifactStore, Provenance},
    runner::{run_producer, CommitUncertain, Control, Line, LineSplit, StatusSink},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

/// Embedded host; run with an explicit `run()` because `-e` has no main module.
const HOST: &str = include_str!("fetch_host.cjs");
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const HOST_TIMEOUT_MS: u128 = 120_000;
/// Hosts accepted as `YouTube` in an allowlist.
pub const YOUTUBE_HOSTS: [&str; 3] = ["youtube.com", "youtu.be", "youtube-nocookie.com"];
const MAX_TEXT: usize = 300;

/// Trusted tools and egress boundary chosen by the controller, never by a user.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FetchTools {
    /// Node executable.
    pub node: PathBuf,
    /// `yt-dlp` executable.
    pub ytdlp: PathBuf,
    /// LGPL-compatible `FFmpeg` executable.
    pub ffmpeg: PathBuf,
    /// Controller-provisioned OS egress wrapper (netns/firewall launcher).
    pub egress_wrapper: PathBuf,
    /// Attestation token the wrapper must echo for `--probe` (>=16 `[\w.-]`).
    pub egress_token: String,
}

impl FetchTools {
    /// Require absolute normalised executables and a well-formed egress token.
    /// # Errors
    /// Rejects relative/non-normal/non-executable paths or a weak token.
    pub fn validate(&self) -> io::Result<()> {
        for path in [&self.node, &self.ytdlp, &self.ffmpeg, &self.egress_wrapper] {
            if !executable(path) {
                return Err(io::Error::other("untrusted or missing fetch tool path"));
            }
        }
        let t = &self.egress_token;
        if t.len() < 16
            || t.len() > 256
            || !t
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            return Err(io::Error::other("egress token required"));
        }
        Ok(())
    }
}
fn executable(path: &Path) -> bool {
    path.is_absolute()
        && !path.to_str().is_none_or(|s| s.contains('\0'))
        && path.components().all(|c| {
            !matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Requested video codec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// H.264 (default).
    #[default]
    H264,
    /// VP9.
    Vp9,
    /// AV1.
    Av1,
}

/// One bounded URL fetch request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    /// Absolute `https` URL; passed only on stdin / as a single argv element.
    pub url: String,
    /// Allowed host suffixes; must include a `YouTube` host.
    pub host_allowlist: Vec<String>,
    /// Codec preference.
    #[serde(default)]
    pub codec: Codec,
    /// Maximum frame height, 144 through 4320.
    pub max_height: u32,
    /// Maximum source duration in seconds, 1 through 86400.
    pub max_duration_secs: u32,
    /// Whether to extract title/uploader/duration.
    pub extract_metadata: bool,
}

impl Fetch {
    /// Default `YouTube`-only allowlist.
    #[must_use]
    pub fn youtube_allowlist() -> Vec<String> {
        YOUTUBE_HOSTS.iter().map(|h| (*h).to_owned()).collect()
    }
    /// Validate URL syntax, allowlist and limits before any process starts.
    /// # Errors
    /// Rejects non-https, userinfo, ports, IP literals, hosts outside the
    /// allowlist, allowlists without `YouTube`, and out-of-range limits.
    pub fn validate(&self) -> io::Result<()> {
        let bad = |m: &'static str| Err(io::Error::other(m));
        let list = &self.host_allowlist;
        if list.is_empty()
            || list.len() > 32
            || !list.iter().all(|a| host_chars(a))
            || !list.iter().any(|a| YOUTUBE_HOSTS.contains(&a.as_str()))
        {
            return bad("fetch allowlist invalid");
        }
        if !(144..=4320).contains(&self.max_height)
            || !(1..=86_400).contains(&self.max_duration_secs)
        {
            return bad("fetch limits invalid");
        }
        let url = &self.url;
        if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return bad("fetch url invalid");
        }
        let Some(rest) = url.strip_prefix("https://") else {
            return bad("fetch url must be https");
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') || authority.contains('[') {
            return bad("fetch url userinfo or ip literal forbidden");
        }
        let host = match authority.rsplit_once(':') {
            Some((h, "443")) => h,
            Some(_) => return bad("fetch url port forbidden"),
            None => authority,
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty()
            || !host_chars(&host)
            || host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            return bad("fetch url host invalid");
        }
        if !list
            .iter()
            .any(|a| host == *a || host.ends_with(&format!(".{a}")))
        {
            return bad("fetch host not allowlisted");
        }
        Ok(())
    }
}
fn host_chars(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
}

/// Sanitised, bounded source metadata. Sideband only: never written to the
/// artifact's provenance.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FetchMeta {
    /// Title, control characters removed, at most 300 chars.
    pub title: Option<String>,
    /// Uploader, same bounds.
    pub uploader: Option<String>,
    /// Duration in seconds when finite and sane.
    pub duration: Option<f64>,
}

/// Download progress as reported by the host (at most ~2/s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchProgress {
    /// Bytes downloaded so far, when known.
    pub downloaded: Option<u64>,
    /// Expected total, when known.
    pub total: Option<u64>,
}

/// Sideband report returned beside the committed artifact.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FetchReport {
    /// Bytes the pump forwarded (equal to the artifact size).
    pub bytes: u64,
    /// Source metadata when the host produced it.
    pub meta: Option<FetchMeta>,
}

fn clean(v: Option<&serde_json::Value>) -> Option<String> {
    let s: String = v?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect();
    Some(s)
}
fn number(v: Option<&serde_json::Value>) -> Option<u64> {
    v?.as_u64()
}

/// Bounded parse of the host's NDJSON event stream.
struct Events<F: FnMut(FetchProgress)> {
    split: LineSplit,
    meta: Option<FetchMeta>,
    done: Option<(bool, u64)>,
    error: Option<String>,
    progress: F,
}
impl<F: FnMut(FetchProgress)> Events<F> {
    fn line(&mut self, line: &Line<'_>) {
        let Line::Text(text) = line else { return };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(text) else {
            return;
        };
        match v.get("event").and_then(serde_json::Value::as_str) {
            Some("meta") => {
                self.meta = Some(FetchMeta {
                    title: clean(v.get("title")),
                    uploader: clean(v.get("uploader")),
                    duration: v
                        .get("duration")
                        .and_then(serde_json::Value::as_f64)
                        .filter(|d| d.is_finite() && (0.0..=1.0e7).contains(d)),
                });
            }
            Some("progress") => (self.progress)(FetchProgress {
                downloaded: number(v.get("downloaded")),
                total: number(v.get("total")),
            }),
            Some("done") => {
                self.done = Some((
                    v.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
                    number(v.get("bytes")).unwrap_or(u64::MAX),
                ));
            }
            Some("error") => {
                let code: String = v
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .chars()
                    .filter(|c| c.is_ascii_lowercase() || *c == '_')
                    .take(32)
                    .collect();
                self.error = Some(code);
            }
            _ => {}
        }
    }
}
impl<F: FnMut(FetchProgress)> StatusSink for Events<F> {
    fn push(&mut self, bytes: &[u8]) {
        let mut split = std::mem::take(&mut self.split);
        split.push(bytes, |l| self.line(&l));
        self.split = split;
    }
}

/// Run one fetch and commit only a verified complete download.
/// # Errors
/// As [`run_fetch_with_pending`].
#[allow(clippy::too_many_arguments)]
pub fn run_fetch(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &FetchTools,
    fetch: &Fetch,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
    progress: impl FnMut(FetchProgress),
) -> io::Result<(Artifact, FetchReport)> {
    run_fetch_with_pending(
        store,
        provenance,
        maximum_bytes,
        tools,
        fetch,
        control,
        allow,
        |_| Ok(()),
        progress,
    )
}

/// [`run_fetch`] with the reservation hook of `run_capture_with_pending`:
/// `reserved` runs right after the store reserved the ID and before any process
/// spawns or byte is written. Validation (tools, egress wrapper, URL) happens
/// first, so an invalid request reserves nothing. A commit error carries
/// [`CommitUncertain`].
/// # Errors
/// Cancellation (`Interrupted`), expiry (`TimedOut`), quota (`StorageFull`),
/// validation, process or protocol failure, a missing/mismatched completion
/// marker, a nonzero exit, or persistence errors prevent success.
#[allow(clippy::too_many_arguments)]
pub fn run_fetch_with_pending(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &FetchTools,
    fetch: &Fetch,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
    reserved: impl FnMut(&ArtifactId) -> io::Result<()>,
    progress: impl FnMut(FetchProgress),
) -> io::Result<(Artifact, FetchReport)> {
    run_fetch_observed(
        store,
        provenance,
        maximum_bytes,
        tools,
        fetch,
        control,
        allow,
        reserved,
        progress,
        &mut |_| {},
    )
}

/// [`run_fetch_with_pending`] that also reports process-tree resource samples at 1 Hz.
/// # Errors
/// As [`run_fetch_with_pending`].
#[allow(clippy::too_many_arguments)]
pub fn run_fetch_observed(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &FetchTools,
    fetch: &Fetch,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
    mut reserved: impl FnMut(&ArtifactId) -> io::Result<()>,
    progress: impl FnMut(FetchProgress),
    observer: &mut dyn FnMut(crate::resource::ResourceSample),
) -> io::Result<(Artifact, FetchReport)> {
    tools.validate()?;
    fetch.validate()?;
    control.check()?;
    let mut pending = store.begin(provenance, maximum_bytes, allow)?;
    reserved(pending.id())?;
    let private = tempfile::Builder::new()
        .prefix("conspect-fetch-")
        .tempdir()?;
    let mut dirs = std::fs::DirBuilder::new();
    dirs.mode(0o700);
    let (home, temp) = (private.path().join("home"), private.path().join("tmp"));
    for dir in [&home, &temp] {
        dirs.create(dir)?;
    }
    let remaining = control.deadline.saturating_duration_since(Instant::now());
    let timeout_ms = remaining.as_millis().clamp(1, HOST_TIMEOUT_MS);
    let request = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "url": fetch.url,
        "ytdlp_path": tools.ytdlp,
        "ffmpeg_path": tools.ffmpeg,
        "egress_wrapper": tools.egress_wrapper,
        "egress_token": tools.egress_token,
        "host_allowlist": fetch.host_allowlist,
        "codec": fetch.codec,
        "max_height": fetch.max_height,
        "max_bytes": maximum_bytes.clamp(1, MAX_BYTES),
        "max_duration_secs": fetch.max_duration_secs,
        "timeout_ms": u64::try_from(timeout_ms).unwrap_or(120_000),
        "extract_metadata": fetch.extract_metadata,
    }))?;
    let mut command = Command::new(&tools.node);
    command
        .args(["-e", &format!("{HOST}\nrun();\n")])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .current_dir(&home);
    let events = Events {
        split: LineSplit::default(),
        meta: None,
        done: None,
        error: None,
        progress,
    };
    let run = run_producer(command, request, events, &mut pending, control, observer)?;
    drop(private);
    let mut events = run.sink;
    let mut final_line = std::mem::take(&mut events.split);
    final_line.finish(|line| events.line(&line));
    if let Some(code) = &events.error {
        return Err(io::Error::other(format!("fetch failed: {code}")));
    }
    if !run.exit.success() {
        return Err(io::Error::other("fetch producer failed"));
    }
    match events.done {
        Some((true, bytes)) if bytes == run.bytes && run.bytes > 0 => {}
        Some(_) => return Err(io::Error::other("fetch completion mismatch")),
        None => return Err(io::Error::other("fetch completion missing")),
    }
    control.check()?;
    let wrote = run.wrote;
    let artifact = pending.complete().map_err(|e| {
        if wrote {
            io::Error::new(e.kind(), CommitUncertain)
        } else {
            e
        }
    })?;
    Ok((
        artifact,
        FetchReport {
            bytes: run.bytes,
            meta: events.meta,
        },
    ))
}
