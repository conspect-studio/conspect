//! "A start that cannot build the saved state must not be an outage" (#317
//! part 2), without ever overwriting the operator's saved state (#324).
//!
//! Next to the accepted state (`.multiview/desired.json`) the run keeps
//! `.multiview/last-good.json`: a byte copy of the accepted envelope the engine
//! last **started** from. "Started" means the pipeline built AND its output
//! clock emitted its first tick (the run's state slot received its first
//! publish). It is written only then, off the output clock, with the same
//! atomic tmp → fsync → rename → dir-fsync writer every state file uses.
//!
//! At start, when the accepted state fails start-up admission (the same
//! `multiview_config::boot_admission` the control plane applies before it
//! persists) or the pipeline build fails with a STATE-class error, the run:
//! 1. logs the exact reason. Nothing is written while deciding; only once the
//!    engine has been BUILT from last-good does the run go on to:
//! 2. keep a COPY of the rejected bytes as
//!    `.multiview/rejected/desired-<unix-ms>-<n>.json` (deduplicated: identical
//!    bytes already kept are reused; bounded: newest [`MAX_REJECTED`] kept);
//! 3. write the persisted marker `.multiview/boot-fallback.json`, so the
//!    alarm survives restarts until an operator acknowledges it
//!    (`POST /api/v1/health/accepted-state-rejected/ack` or
//!    `multiview acknowledge-fallback <config>`);
//! 4. start from `last-good.json`, seeding the control plane's in-memory
//!    accepted snapshot with it so the stores and `GET` routes agree with what
//!    the engine runs — `desired.json` itself is NEVER moved or overwritten
//!    (#324): every restart tries it first again, so a fault that goes away
//!    (a newer build, a fixed host) heals with no operator action;
//! 5. raise a latched `accepted-state-rejected` health warning naming what was
//!    rejected, why, and where the copy was kept.
//!
//! An ENVIRONMENT-class build error (an OS error of a host kind — permission
//! denied, a full or read-only volume, a busy resource — or a refused thread
//! spawn) never falls back at all: it is retried and then fails, since it
//! would break every state alike (see [`crate::boot::FailureClass`]). Socket
//! binds are not part of the build: a listener's `EADDRINUSE` fails start-up
//! later and never reaches this decision.
//!
//! With no last-good copy (a first install) the start fails as before with the
//! clear error: a first install must never invent a state.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use multiview_config::MultiviewConfig;
use multiview_control::boot_model::{
    boot_fallback_marker_path, clear_boot_fallback_marker, load_desired_file,
    read_boot_fallback_marker, state_dir_for, write_atomic, AtomicWriteError, BootFallbackMarker,
    DesiredState,
};

/// The last-good copy's file name inside the state directory.
pub const LAST_GOOD_FILE: &str = "last-good.json";
/// The directory (inside the state directory) that keeps rejected states.
pub const REJECTED_DIR: &str = multiview_control::boot_model::REJECTED_DIR;
/// How many rejected accepted states are kept (oldest pruned first).
pub const MAX_REJECTED: usize = 5;

/// The last-good copy for the boot config at `boot_path`.
#[must_use]
pub fn last_good_path_for(boot_path: &Path) -> PathBuf {
    state_dir_for(boot_path).join(LAST_GOOD_FILE)
}

/// What a fallback start did, for the alarm and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fallback {
    /// Why the accepted state could not start (the exact start-up reason).
    pub reason: String,
    /// Where a copy of the rejected accepted state was kept (`None`: the copy
    /// could not be written — `desired.json` still holds the bytes).
    pub kept_at: Option<PathBuf>,
    /// Why the kept copy or the persisted marker could not be written, if
    /// either failed (#324: the start proceeds anyway — nothing is lost because
    /// `desired.json` is untouched — but the operator must be told the alarm
    /// will not survive a restart).
    pub persist_error: Option<String>,
    /// Unix milliseconds of the start that fell back.
    pub since_unix_ms: i64,
}

impl Fallback {
    /// A fallback for `reason`, made safe to persist, publish and log (#324):
    /// the reason embeds an engine error that can carry a source/output URL
    /// with credentials (`srt://…?passphrase=`, `rtmp://user:pass@…`) and can
    /// be arbitrarily long, so it is redacted
    /// ([`multiview_control::catalogue::netdiag::redact_text`]: URL userinfo,
    /// query strings, `key=`-style values and long token runs) and bounded to
    /// [`multiview_control::MAX_WARNING_TEXT`] before anything stores it.
    #[must_use]
    pub fn new(reason: &str, since_unix_ms: i64) -> Self {
        Self {
            reason: safe_text(reason),
            kept_at: None,
            persist_error: None,
            since_unix_ms,
        }
    }

    /// The operator-facing warning text (redacted and bounded).
    #[must_use]
    pub fn message(&self) -> String {
        let mut message = format!(
            "The saved configuration could not start ({}). The engine started from the last configuration that ran; the saved configuration is unchanged in .multiview/desired.json and is tried again on every restart.",
            self.reason
        );
        if let Some(kept) = &self.kept_at {
            let _ = write!(message, " A copy is kept at {}.", kept.display());
        }
        if let Some(error) = &self.persist_error {
            let _ = write!(
                message,
                " The start-up fallback could not be recorded on disk ({error}); this warning will not survive a restart."
            );
        }
        multiview_control::bounded_warning_text(&message)
    }

    /// The persisted marker for this fallback.
    #[must_use]
    pub fn marker(&self) -> BootFallbackMarker {
        BootFallbackMarker::new(
            self.reason.clone(),
            self.kept_at.as_ref().map(|p| p.display().to_string()),
            self.since_unix_ms,
            self.message(),
        )
    }
}

/// Redact credentials and bound the length of text that leaves the process
/// in the start-up fallback warning (#324).
fn safe_text(text: &str) -> String {
    multiview_control::bounded_warning_text(&multiview_control::catalogue::netdiag::redact_text(
        text,
    ))
}

/// An atomic state-file writer: production uses
/// [`multiview_control::boot_model::write_atomic_checked`]; tests inject
/// `ENOSPC` / `AfterRename`.
pub type AtomicWriter<'a> = dyn Fn(&Path, &str) -> Result<(), AtomicWriteError> + 'a;

/// Write `content` to `path` with `writer`, treating an `AfterRename` failure
/// as written when the file reads back byte-equal (the rename is visible; only
/// its directory sync failed — warned, not fatal).
fn write_checked(path: &Path, content: &str, writer: &AtomicWriter<'_>) -> Result<(), String> {
    match writer(path, content) {
        Ok(()) => Ok(()),
        Err(AtomicWriteError::AfterRename(error))
            if std::fs::read_to_string(path).is_ok_and(|now| now == content) =>
        {
            tracing::warn!(path = %path.display(), %error, "written, but the directory sync failed");
            Ok(())
        }
        Err(error) => Err(format!("writing {}: {error}", path.display())),
    }
}

/// Keep a COPY of the rejected accepted-state bytes under `rejected/`
/// (bounded) and return where (#324: `desired.json` itself is never touched).
/// Identical bytes already kept are reused instead of written again, so a
/// state that keeps failing on every restart does not churn the bounded
/// directory (and push out older, different rejected states).
///
/// # Errors
/// The directory or the copy could not be written.
pub fn keep_rejected_copy(
    boot_path: &Path,
    rejected_text: &str,
    now_ms: i64,
) -> Result<PathBuf, String> {
    keep_rejected_copy_with(
        boot_path,
        rejected_text,
        now_ms,
        &multiview_control::boot_model::write_atomic_checked,
    )
}

/// [`keep_rejected_copy`] with an injectable writer.
///
/// # Errors
/// See [`keep_rejected_copy`].
pub fn keep_rejected_copy_with(
    boot_path: &Path,
    rejected_text: &str,
    now_ms: i64,
    writer: &AtomicWriter<'_>,
) -> Result<PathBuf, String> {
    let rejected_dir = state_dir_for(boot_path).join(REJECTED_DIR);
    if let Some(existing) = rejected_files(&rejected_dir)
        .into_iter()
        .find(|p| std::fs::read_to_string(p).is_ok_and(|kept| kept == rejected_text))
    {
        // The bound holds on a reuse too (files may have been dropped in by
        // hand or by an older build); the reused copy is never pruned.
        prune_rejected(&rejected_dir, &existing);
        return Ok(existing);
    }
    create_private_dir(&rejected_dir)?;
    // `desired-<13-digit ms>-<3-digit n>.json`: lexical order = time order.
    let mut n = 0_u32;
    let mut kept = rejected_dir.join(format!("desired-{now_ms:013}-{n:03}.json"));
    while kept.exists() {
        n = n.saturating_add(1);
        kept = rejected_dir.join(format!("desired-{now_ms:013}-{n:03}.json"));
    }
    write_checked(&kept, rejected_text, writer)
        .map_err(|e| format!("keeping a copy of the rejected state: {e}"))?;
    prune_rejected(&rejected_dir, &kept);
    Ok(kept)
}

/// Remove a pending start-up fallback marker when THIS start ran from the
/// accepted state (#324): its claim — "the engine started from the last
/// configuration that ran" — is no longer true, so it must not raise the
/// warning again. Only a start resolved from the accepted state and built with
/// no fallback settles it; a start with no `desired.json` (or one that fell
/// back) leaves it alone. Start-up only (blocking file I/O, before the clock).
/// Returns whether a marker was removed; a removal failure is logged, never
/// fatal (at worst the warning is raised once more).
pub fn settle_marker(boot_path: &Path, start: &crate::boot::StartConfig) -> bool {
    if start.accepted_text.is_none()
        || start.boot_fallback.is_some()
        || start.pending_fallback.is_some()
    {
        return false;
    }
    let reason = match read_boot_fallback_marker(boot_path) {
        Ok(Some(marker)) => marker.reason,
        Ok(None) => return false,
        Err(error) => format!("(unreadable record: {error})"),
    };
    match clear_boot_fallback_marker(boot_path) {
        Ok(removed) => {
            if removed {
                tracing::warn!(
                    reason = %safe_text(&reason),
                    "the saved configuration started again: cleared the pending start-up fallback warning"
                );
            }
            removed
        }
        Err(error) => {
            tracing::warn!(%error, "could not clear a stale start-up fallback marker");
            false
        }
    }
}

/// Persist the start-up fallback marker for `fallback` (#324).
///
/// # Errors
/// The marker could not be written.
pub fn write_marker_with(
    boot_path: &Path,
    fallback: &Fallback,
    writer: &AtomicWriter<'_>,
) -> Result<(), String> {
    let json = serde_json::to_string(&fallback.marker())
        .map_err(|e| format!("serializing the start-up fallback marker: {e}"))?;
    write_checked(&boot_fallback_marker_path(boot_path), &json, writer)
        .map_err(|e| format!("recording the start-up fallback marker: {e}"))
}

/// Raise the `accepted-state-rejected` warning at start-up when one is due
/// (#324): from THIS start's fallback when it fell back (freshest, and it also
/// covers a marker that could not be written), else from a persisted marker an
/// earlier start left (a crash-restart, a `--ticks` run, a run without
/// `[control]`). Call after the warning ingest subscribed, before the output
/// clock exists (inv #1); publishes through the drop-oldest lane (inv #10).
/// Returns whether a warning was raised.
#[must_use = "whether a warning was raised (start-up may ignore it; tests assert it)"]
pub fn announce<S>(
    boot_path: &Path,
    start: &crate::boot::StartConfig,
    publisher: &multiview_engine::EnginePublisher<S, multiview_events::Event>,
) -> bool {
    let now_nanos = |ms: Option<i64>| {
        ms.map_or_else(
            || {
                i64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos()),
                )
                .unwrap_or(i64::MAX)
            },
            |ms| ms.saturating_mul(1_000_000),
        )
    };
    if let Some(fallback) = &start.boot_fallback {
        multiview_control::emit_accepted_state_rejected(
            publisher,
            &fallback.message(),
            now_nanos(Some(fallback.since_unix_ms)),
        );
        return true;
    }
    match read_boot_fallback_marker(boot_path) {
        Ok(Some(marker)) => {
            multiview_control::emit_accepted_state_rejected(
                publisher,
                // Read back from disk (possibly written by an older build):
                // redact + bound again.
                &safe_text(&marker.message),
                now_nanos(Some(marker.since_unix_ms)),
            );
            true
        }
        Ok(None) => false,
        // A marker that exists but cannot be read still means an earlier
        // start fell back: raise it rather than silently drop the alarm.
        Err(error) => {
            multiview_control::emit_accepted_state_rejected(
                publisher,
                &safe_text(&format!(
                    "An earlier start could not use the saved configuration and started from the last configuration that ran, but its record cannot be read ({error})."
                )),
                now_nanos(None),
            );
            true
        }
    }
}

/// Acknowledge a pending start-up fallback warning from the command line
/// (#324): remove the persisted marker. Returns the cleared warning's text
/// (a placeholder when the marker was unreadable), or `None` when nothing was
/// pending.
///
/// This silences the current alarm only. It does not change the saved
/// configuration, and it does not reach a running engine's banner (use the
/// API acknowledgement for that). Every later start that still cannot use the
/// saved configuration falls back again and raises the warning again, until
/// the configuration is fixed or replaced; a start that uses it clears the
/// warning by itself.
///
/// # Errors
/// The marker exists but could not be removed.
pub fn acknowledge(boot_path: &Path) -> Result<Option<String>, String> {
    let message = match read_boot_fallback_marker(boot_path) {
        Ok(marker) => marker.map(|m| safe_text(&m.message)),
        Err(error) => Some(format!("(an unreadable start-up fallback record: {error})")),
    };
    let removed = clear_boot_fallback_marker(boot_path).map_err(|e| {
        format!(
            "removing {}: {e}",
            boot_fallback_marker_path(boot_path).display()
        )
    })?;
    Ok(if removed { message } else { None })
}

/// Load the last-good envelope, if any. An unreadable/invalid copy is treated
/// as absent (logged): it can never be the reason a start fails.
#[must_use]
pub fn load_last_good(boot_path: &Path) -> Option<DesiredState> {
    match load_desired_file(&last_good_path_for(boot_path)) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "ignoring an unusable last-good state copy");
            None
        }
    }
}

/// The state directory's owner-only (0700) policy for the `rejected/` subdir.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("securing {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// The kept `desired-*.json` files in `dir`, sorted by name (= time).
fn rejected_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("desired-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
        })
        .collect();
    files.sort();
    files
}

/// Keep only the newest [`MAX_REJECTED`] `desired-*.json` files, ordered by
/// file name (the zero-padded millisecond stamp), never removing `keep`.
fn prune_rejected(dir: &Path, keep: &Path) {
    let files = rejected_files(dir);
    let excess = files.len().saturating_sub(MAX_REJECTED);
    // Skip `keep` BEFORE counting, so a kept file among the oldest (a dedupe
    // reuse, #324) still leaves exactly MAX_REJECTED.
    for path in files.into_iter().filter(|p| p != keep).take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "could not prune a kept rejected state");
        }
    }
}

/// Record the accepted envelope the engine has now STARTED from as last-good,
/// written atomically. Call only after the first
/// tick was emitted, off the output clock (a blocking task). A missing accepted
/// state (a run that never committed one) writes nothing.
///
/// # Errors
/// An I/O failure (the caller logs it; a run is never stopped for it).
pub fn record_started(boot_path: &Path, started: &DesiredState) -> Result<bool, String> {
    let json = serde_json::to_string(started)
        .map_err(|e| format!("serializing the started state: {e}"))?;
    let path = last_good_path_for(boot_path);
    if std::fs::read_to_string(&path).is_ok_and(|current| current == json) {
        return Ok(false);
    }
    write_atomic(&path, &json).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(true)
}

/// Record `started` as last-good once the output clock has emitted its first
/// tick: `tick_sequence` reads the run's wait-free state-slot sequence (bumped
/// once per emitted tick by the engine). A control-plane task: it polls that
/// counter every 100 ms (never touches the clock), then writes on the blocking
/// pool. It exits without writing if `stop` is raised first — a run that never
/// emitted a tick never becomes last-good.
pub fn spawn_recorder(
    boot_path: PathBuf,
    started: DesiredState,
    tick_sequence: impl Fn() -> u64 + Send + 'static,
    stop: multiview_engine::StopSignal,
) -> tokio::task::JoinHandle<()> {
    let baseline = tick_sequence();
    tokio::spawn(async move {
        while tick_sequence() == baseline {
            if stop.is_stopped() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let written =
            tokio::task::spawn_blocking(move || record_started(&boot_path, &started)).await;
        match written {
            Ok(Ok(true)) => {
                tracing::info!("first tick emitted: recorded the started state as last-good");
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not record the last-good state"),
            Err(error) => tracing::warn!(%error, "the last-good writer task failed"),
        }
    })
}

/// Can `running` boot? The start-up admission rule plus the caller's own build
/// attempt (`build` returns the build error text).
///
/// # Errors
/// The exact reason start-up refuses it.
pub fn admit(running: &MultiviewConfig) -> Result<(), String> {
    multiview_config::boot_admission(running).map_err(|e| e.to_string())
}
