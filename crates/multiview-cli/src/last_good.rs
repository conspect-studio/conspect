//! "A start that cannot build the saved state must not be an outage" (#317 part 2).
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
//! persists) or the pipeline build itself fails, the run:
//! 1. logs the exact reason;
//! 2. keeps the operator's rejected state on disk as
//!    `.multiview/rejected/desired-<unix-ms>.json` (bounded: newest
//!    [`MAX_REJECTED`] kept) — never silently discarded;
//! 3. starts from `last-good.json`, which is ALSO restored as the accepted
//!    state so the control plane, the stores and the next restart agree with
//!    what the engine runs;
//! 4. raises a latched `accepted-state-rejected` health warning naming what was
//!    rejected, why, and where it was kept (the UI health banner and
//!    `GET /api/v1/health` show it).
//!
//! With no last-good copy (a first install) the start fails as before with the
//! clear error: a first install must never invent a state.

use std::path::{Path, PathBuf};

use multiview_config::MultiviewConfig;
use multiview_control::boot_model::{
    desired_path_for, load_desired_file, state_dir_for, write_atomic, DesiredState,
};

/// The last-good copy's file name inside the state directory.
pub const LAST_GOOD_FILE: &str = "last-good.json";
/// The directory (inside the state directory) that keeps rejected states.
pub const REJECTED_DIR: &str = "rejected";
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
    /// Where the rejected accepted state was kept.
    pub kept_at: PathBuf,
}

impl Fallback {
    /// The operator-facing warning text.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "The saved configuration could not start ({}). The engine started from the last configuration that ran; the rejected one is kept at {}.",
            self.reason,
            self.kept_at.display()
        )
    }
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

/// Keep the rejected accepted state under `rejected/` (bounded) and restore the
/// last-good envelope as the accepted state. Returns where the rejected copy
/// was kept.
///
/// # Errors
/// An I/O failure; the caller then refuses to start (never discards the
/// operator's state without having kept it).
pub fn quarantine_and_restore(
    boot_path: &Path,
    last_good: &DesiredState,
    now_ms: i64,
) -> Result<PathBuf, String> {
    let desired = desired_path_for(boot_path);
    let rejected_dir = state_dir_for(boot_path).join(REJECTED_DIR);
    std::fs::create_dir_all(&rejected_dir)
        .map_err(|e| format!("creating {}: {e}", rejected_dir.display()))?;
    let text = std::fs::read_to_string(&desired)
        .map_err(|e| format!("reading {}: {e}", desired.display()))?;
    let mut kept = rejected_dir.join(format!("desired-{now_ms}.json"));
    let mut n = 1_u32;
    while kept.exists() {
        kept = rejected_dir.join(format!("desired-{now_ms}-{n}.json"));
        n = n.saturating_add(1);
    }
    // Keep first (atomic, durable), THEN replace: a crash between the two
    // leaves both the rejected state kept and the accepted file unchanged.
    write_atomic(&kept, &text).map_err(|e| format!("keeping {}: {e}", kept.display()))?;
    prune_rejected(&rejected_dir);
    let json = serde_json::to_string(last_good)
        .map_err(|e| format!("serializing the last-good state: {e}"))?;
    write_atomic(&desired, &json)
        .map_err(|e| format!("restoring {} from last-good: {e}", desired.display()))?;
    Ok(kept)
}

/// Keep only the newest [`MAX_REJECTED`] `desired-*.json` files.
fn prune_rejected(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name().to_str().is_some_and(|n| {
                n.starts_with("desired-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(MAX_REJECTED);
    for (_, path) in files.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "could not prune a kept rejected state");
        }
    }
}

/// Record the accepted state the engine has now STARTED from as last-good: a
/// byte copy of `desired.json`, written atomically. Call only after the first
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
