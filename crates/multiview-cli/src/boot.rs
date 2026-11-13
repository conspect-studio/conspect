//! Cold-start resolution for the Boot/Loaded/Running model (ADR-W024 §4).
//!
//! `multiview run` starts from its config file (**Boot**). The boot file's
//! `[control] start` policy decides where the starting **Running** state comes
//! from:
//!
//! * `start = "boot"` (the default): Running is the boot document itself; any
//!   persisted `active.toml` from a previous run is ignored (and superseded by
//!   this run's first persist).
//! * `start = "resume"`: Running is the last persisted
//!   `<config-dir>/.multiview/active.toml` when it reads, parses, AND
//!   validates — otherwise the run falls back to the boot document with a
//!   warning naming the reason (surfaced on `GET /api/v1/config/boot-model`
//!   as `resume_fallback`).
//!
//! **Loaded** stays the boot snapshot in BOTH modes: revert-to-start targets
//! the deliberate cold-start baseline, never the resumed state. The boot file
//! also stays the ADR-W020 watch target, so an external boot-file edit during
//! a resumed run still hot-applies (diffed against the resumed baseline).

use std::path::Path;

use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_resume_config, BootModel};

/// The resolved starting state of a run: the document the engine is built
/// from (**Running**), the immutable boot snapshot (**Loaded**), and what the
/// resume resolution actually did.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// The starting Running state: the engine is built from it, the control
    /// stores are seeded from it, and the ADR-W020 watcher's baseline is it.
    pub running: MultiviewConfig,
    /// The immutable Loaded snapshot (the boot document at process start) —
    /// the revert-to-start target in both modes.
    pub loaded: MultiviewConfig,
    /// The raw boot-file TEXT as read at process start: the watcher's
    /// initial last-observed content (review m4 — under a resume the
    /// UNCHANGED boot file must never clobber the resumed baseline; only a
    /// real content change applies, and an edit landing in the boot window
    /// IS a content change against this text).
    pub boot_text: String,
    /// The `[control] start` policy the boot file declared.
    pub start: StartMode,
    /// Whether Running actually came from a valid persisted `active.toml`.
    pub resumed: bool,
    /// Why a `start = "resume"` run fell back to boot, if it did.
    pub resume_fallback: Option<String>,
    /// Accepted layout library and selected identity from the atomic record.
    pub desired: Option<multiview_control::boot_model::DesiredState>,
    /// Set when the accepted state could not start and this run started from
    /// the last-good copy instead (#317): the reason and where a copy of the
    /// rejected state was kept (`desired.json` itself is unchanged, #324).
    /// Raised as the `accepted-state-rejected` health warning.
    pub boot_fallback: Option<crate::last_good::Fallback>,
    /// A fallback decided but not yet committed (nothing written yet): set by
    /// [`resolve_start_config`], committed by [`commit_fallback`] after the
    /// engine was built from last-good.
    pub pending_fallback: Option<PendingFallback>,
    /// The exact `desired.json` bytes this start was resolved from (kept
    /// verbatim if a later build failure falls back).
    pub accepted_text: Option<String>,
}

impl StartConfig {
    /// The reason of a decided-but-uncommitted fallback, if any.
    #[must_use]
    pub fn pending_fallback_reason(&self) -> Option<&str> {
        self.pending_fallback.as_ref().map(|p| p.reason.as_str())
    }

    /// The control plane's [`BootModel`] for this resolution, rooted at the
    /// boot file `boot_path` (the watch + promote target).
    ///
    /// The path is **canonicalized** (MINOR-D, ADR-W024 §6): when the config is
    /// reached through a symlink, `promote` must rewrite the real target file
    /// (the atomic temp-file + `rename(2)` would otherwise replace the symlink
    /// itself, breaking it). Canonicalization resolves `.`/`..`/symlinks once at
    /// startup; if it fails (e.g. the file is transiently absent) the given path
    /// is kept verbatim.
    #[must_use]
    pub fn to_boot_model(&self, boot_path: &Path) -> BootModel {
        let resolved = std::fs::canonicalize(boot_path).unwrap_or_else(|_| boot_path.to_path_buf());
        let model = BootModel::new(
            resolved,
            self.loaded.clone(),
            self.running.clone(),
            self.start,
            self.resumed,
            // #317: a run that started from last-good says so on
            // `GET /api/v1/config/boot-model` (`resume_fallback`), not only in
            // the health warning.
            self.boot_fallback
                .as_ref()
                .map(crate::last_good::Fallback::message)
                .or_else(|| self.resume_fallback.clone()),
        );
        // #324: a fallback leaves `desired.json` untouched, so the control
        // plane must not lazily load it — it would serve (and its stores would
        // be seeded from) a state the engine is NOT running. Seed the accepted
        // snapshot with the last-good envelope that does run.
        match (&self.boot_fallback, &self.desired) {
            (Some(_), Some(running)) => model.with_accepted_override(running.clone()),
            _ => model,
        }
    }
}

/// Splice the storeless restart-only sections from the BOOT document into a
/// resumed Running document (ADR-W024 review M1): `control`, `placement`,
/// `walls`, `routing`, and `schema_version` have no control store — the boot
/// file is their durable truth, and a restart is exactly when they take effect.
/// Without the splice, a boot-file `[control] listen` edit would be silently
/// lost on the very restart the operator performed to apply it (the stale
/// `active.toml` copy would win).
///
/// `salvos` and `tally_profiles` are deliberately NOT spliced (ADR-W024 round
/// 6): they ARE runtime-mutable through their definition routes (a pure
/// control-plane store edit), so they are store-backed running state composed
/// INTO `active.toml` — exactly like `sources`/`overlays`/`outputs`. A resume
/// must restore the runtime-edited definitions from `active.toml`, not discard
/// them back to the boot file's copy. A boot-file edit to a salvo/tally during
/// a resumed run still takes effect: the ADR-W020 watcher diffs the boot file
/// against the resumed baseline and resyncs the definition store (the same path
/// a boot-file `sources` edit rides), so the boot file stays authoritative for
/// external edits without clobbering live drift on every restart.
fn splice_storeless_sections(
    mut running: MultiviewConfig,
    boot: &MultiviewConfig,
) -> MultiviewConfig {
    running.schema_version = boot.schema_version;
    running.control.clone_from(&boot.control);
    running.placement.clone_from(&boot.placement);
    running.walls.clone_from(&boot.walls);
    running.routing.clone_from(&boot.routing);
    running.events = boot.events;
    running
}

/// Resolve the starting Running state for a run booted from `boot` (already
/// parsed + validated) at `boot_path` (ADR-W024 §4).
///
/// Under `start = "resume"` the persisted `active.toml` next to the boot file
/// becomes Running when it is valid — with the storeless restart-only
/// sections (`control`, `placement`, `walls`, `routing`, `schema_version`)
/// spliced from the BOOT document (review M1: a boot-file edit to a
/// restart-only section must take effect on restart). `salvos`/`tally_profiles`
/// are store-backed running state (ADR-W024 round 6) and resume FROM
/// `active.toml`, never spliced. A missing/unreadable/invalid file — or a
/// splice that no longer validates — falls back to the boot document with a
/// `tracing::warn!` and the reason recorded in
/// [`StartConfig::resume_fallback`]. The default `boot` policy never reads
/// `active.toml`.
/// # Errors
/// A corrupt accepted record or incompatible boot splice prevents startup.
#[allow(clippy::too_many_lines)]
// reason: one ordered decision (accepted state → last-good fallback → start
// policy → active.toml resume → boot); splitting it scatters the precedence.
pub fn resolve_start_config(
    boot: MultiviewConfig,
    boot_text: String,
    boot_path: &Path,
) -> Result<StartConfig, String> {
    // Accepted intent outranks the observational active snapshot, including
    // when the boot file selected its normal boot policy.
    // #317: the accepted state must pass the SAME start-up admission the
    // control plane applies before it persists. When it does not (or cannot be
    // read), start from the last state that started instead of failing — and
    // fail exactly as before when there is none.
    match read_accepted(boot_path) {
        Accepted::Present { text, parsed } => {
            let rejection = match *parsed {
                Ok(desired) => {
                    let running = splice_storeless_sections(desired.config.clone(), &boot);
                    match crate::last_good::admit(&running) {
                        Ok(()) => {
                            return Ok(StartConfig {
                                running,
                                loaded: boot,
                                boot_text,
                                start: StartMode::Resume,
                                resumed: true,
                                resume_fallback: None,
                                desired: Some(desired),
                                boot_fallback: None,
                                pending_fallback: None,
                                accepted_text: Some(text.clone()),
                            });
                        }
                        Err(error) => {
                            format!("accepted desired state cannot be resumed: {error}")
                        }
                    }
                }
                Err(error) => format!("accepted desired state cannot be read: {error}"),
            };
            return plan_last_good(boot, boot_text, boot_path, rejection, text);
        }
        Accepted::Absent => {}
        // An I/O error reading the file (EMFILE, EIO …) is NOT evidence the
        // saved state is bad: fail as before, never roll back over a blip.
        Accepted::Unreadable(error) => {
            return Err(format!("accepted desired state cannot be read: {error}"));
        }
    }
    let start = boot
        .control
        .as_ref()
        .map_or(StartMode::Boot, |control| control.start);
    if start != StartMode::Resume {
        return Ok(StartConfig {
            running: boot.clone(),
            loaded: boot,
            boot_text,
            start,
            resumed: false,
            resume_fallback: None,
            desired: None,
            boot_fallback: None,
            pending_fallback: None,
            accepted_text: None,
        });
    }
    Ok(match load_resume_config(boot_path) {
        Ok(active) => {
            let running = splice_storeless_sections(active, &boot);
            match running.validate() {
                Ok(()) => {
                    tracing::info!(
                        boot = %boot_path.display(),
                        "start = \"resume\": starting from the persisted Running state \
                         (active.toml) with the restart-only sections from the boot \
                         file; the boot file stays the Loaded snapshot and the watch \
                         target"
                    );
                    StartConfig {
                        running,
                        loaded: boot,
                        boot_text,
                        start,
                        resumed: true,
                        resume_fallback: None,
                        desired: None,
                        boot_fallback: None,
                        pending_fallback: None,
                        accepted_text: None,
                    }
                }
                Err(error) => {
                    let reason = format!(
                        "the persisted Running state does not validate once the boot \
                         file's restart-only sections are spliced in: {error}"
                    );
                    tracing::warn!(
                        boot = %boot_path.display(),
                        reason = %reason,
                        "start = \"resume\" requested but the spliced Running state is \
                         unusable; falling back to the boot document"
                    );
                    StartConfig {
                        running: boot.clone(),
                        loaded: boot,
                        boot_text,
                        start,
                        resumed: false,
                        resume_fallback: Some(reason),
                        desired: None,
                        boot_fallback: None,
                        pending_fallback: None,
                        accepted_text: None,
                    }
                }
            }
        }
        Err(reason) => {
            tracing::warn!(
                boot = %boot_path.display(),
                reason = %reason,
                "start = \"resume\" requested but the persisted Running state is \
                 unusable; falling back to the boot document"
            );
            StartConfig {
                running: boot.clone(),
                loaded: boot,
                boot_text,
                start,
                resumed: false,
                resume_fallback: Some(reason),
                desired: None,
                boot_fallback: None,
                pending_fallback: None,
                accepted_text: None,
            }
        }
    })
}

/// What was read from `desired.json`.
enum Accepted {
    /// No accepted state (a first install, or one that never committed).
    Absent,
    /// The file could not be read (an I/O error, retried once).
    Unreadable(String),
    /// The exact bytes read, and whether they parse and validate.
    Present {
        text: String,
        parsed: Box<Result<multiview_control::boot_model::DesiredState, String>>,
    },
}

fn read_accepted(boot_path: &Path) -> Accepted {
    let path = multiview_control::boot_model::desired_path_for(boot_path);
    let mut last = String::new();
    for _ in 0..2 {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let parsed = multiview_control::boot_model::parse_desired_text(&text, &path);
                return Accepted::Present {
                    text,
                    parsed: Box::new(parsed),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Accepted::Absent,
            Err(e) => last = format!("reading {}: {e}", path.display()),
        }
    }
    Accepted::Unreadable(last)
}

/// A decided-but-not-yet-committed fallback: the rejected bytes (a copy is
/// kept on commit) and the last-good envelope the engine runs instead.
#[derive(Debug, Clone)]
pub struct PendingFallback {
    reason: String,
    rejected_text: String,
}

/// Plan a start from the last-good copy because the accepted state cannot
/// start (`reason`). NOTHING is written here: the copy + marker are written in
/// [`commit_fallback`], only after the engine was built from last-good. Without
/// a usable last-good copy this is the original error (a first install never
/// invents a state).
fn plan_last_good(
    boot: MultiviewConfig,
    boot_text: String,
    boot_path: &Path,
    reason: String,
    rejected_text: String,
) -> Result<StartConfig, String> {
    let Some(last_good) = crate::last_good::load_last_good(boot_path) else {
        return Err(reason);
    };
    let running = splice_storeless_sections(last_good.config.clone(), &boot);
    if let Err(error) = crate::last_good::admit(&running) {
        tracing::error!(%error, "the last-good state no longer starts either");
        return Err(format!(
            "{reason}; the last-good state cannot start either: {error}"
        ));
    }
    if serde_json::to_string(&last_good).ok().as_deref() == Some(rejected_text.as_str()) {
        return Err(reason);
    }
    Ok(StartConfig {
        running,
        loaded: boot,
        boot_text,
        start: StartMode::Resume,
        resumed: true,
        resume_fallback: None,
        desired: Some(last_good),
        boot_fallback: None,
        pending_fallback: Some(PendingFallback {
            reason,
            rejected_text,
        }),
        accepted_text: None,
    })
}

/// What kind of fault a failed engine build is (#324). The start-up fallback
/// only helps when the fault is in the saved STATE: a host/environment fault
/// during the BUILD (a directory the process may not create, a full or
/// read-only volume, a thread that cannot be spawned, an expired lease)
/// breaks every state alike, so falling back would only replace the
/// operator's configuration for nothing.
///
/// Scope: this classifies the engine BUILD only. No socket is bound during
/// `Pipeline::build` / `SoftwareEngine::build` — the control-plane and `MoQ`
/// listeners bind after [`commit_fallback`], and AES67 senders bind when the
/// run drives them — so an `EADDRINUSE` there fails start-up (or skips that
/// output) and never reaches the fallback decision at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureClass {
    /// The saved configuration itself cannot be built; a different (last-good)
    /// one may. Also the default for an error nobody classified: since #324 the
    /// fallback never changes `desired.json`, so a misclassified environment
    /// fault costs at most a run from last-good plus a warning — never data.
    State,
    /// The host/environment failed (I/O, sockets, threads, licence lease): the
    /// same configuration is retried with backoff and never rolled back.
    Environment,
}

/// Classify an OS error raised while building the engine by its kind (#324):
/// the kinds a host produces (address in use/unavailable, permission denied,
/// a full, read-only or over-quota volume, a busy resource, a transient
/// `EAGAIN`/`EINTR`/timeout, a refused connection, out of memory) are
/// [`FailureClass::Environment`]; anything else — `NotFound`, `InvalidInput`,
/// `InvalidData`, … usually means the configuration names something wrong —
/// is [`FailureClass::State`], the safe default (see there).
#[must_use]
pub fn io_failure_class(kind: std::io::ErrorKind) -> FailureClass {
    use std::io::ErrorKind as K;
    match kind {
        K::AddrInUse
        | K::AddrNotAvailable
        | K::PermissionDenied
        | K::StorageFull
        | K::ReadOnlyFilesystem
        | K::ResourceBusy
        | K::WouldBlock
        | K::Interrupted
        | K::TimedOut
        | K::ConnectionRefused
        | K::OutOfMemory
        | K::QuotaExceeded => FailureClass::Environment,
        _ => FailureClass::State,
    }
}

/// A classified engine build failure (#324).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildFailure {
    /// Whether the fault is in the state or the environment.
    pub class: FailureClass,
    /// The operator-facing error text.
    pub message: String,
}

impl BuildFailure {
    /// A classified failure.
    #[must_use]
    pub fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    /// A fault in the saved configuration.
    #[must_use]
    pub fn state(message: impl Into<String>) -> Self {
        Self::new(FailureClass::State, message)
    }

    /// A fault in the host/environment.
    #[must_use]
    pub fn environment(message: impl Into<String>) -> Self {
        Self::new(FailureClass::Environment, message)
    }
}

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// An unclassified error is a [`FailureClass::State`] failure (see there why
/// that default is safe).
impl From<String> for BuildFailure {
    fn from(message: String) -> Self {
        Self::state(message)
    }
}

/// See [`From<String>`](BuildFailure#impl-From<String>-for-BuildFailure).
impl From<&str> for BuildFailure {
    fn from(message: &str) -> Self {
        Self::state(message)
    }
}

/// How often an environment-class BUILD failure is retried (#324), on the
/// SAME configuration, before start-up gives up. Runs at start-up before any
/// output clock exists (inv #1), so a plain blocking sleep is fine: nothing
/// else is scheduled yet that it could delay. It covers the engine build only:
/// the control-plane listener bind and the output-routing thread spawn happen
/// later (after the commit / when the run drives), outside this retry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// The pause before each retry; `delays.len()` retries, so
    /// `delays.len() + 1` attempts in total.
    pub delays: Vec<std::time::Duration>,
}

impl RetryPolicy {
    /// Production: 250 ms, 1 s, 2 s (3 retries, 4 attempts, ~3.25 s worst
    /// case) — long enough to ride out a transient host fault during the build
    /// (`EAGAIN` on a thread spawn, a volume being remounted), short enough
    /// that a supervisor restart loop stays responsive.
    #[must_use]
    pub fn startup() -> Self {
        Self::new(vec![
            std::time::Duration::from_millis(250),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(2),
        ])
    }

    /// A policy with explicit delays (tests use zero delays).
    #[must_use]
    pub fn new(delays: Vec<std::time::Duration>) -> Self {
        Self { delays }
    }

    /// Total attempts this policy makes.
    #[must_use]
    pub fn attempts(&self) -> usize {
        self.delays.len().saturating_add(1)
    }
}

/// Build `config`, retrying ONLY environment-class failures per `retry`.
/// Returns the last failure and how many attempts were made.
fn build_retrying<T>(
    config: &MultiviewConfig,
    retry: &RetryPolicy,
    build: &impl Fn(&MultiviewConfig) -> Result<T, BuildFailure>,
) -> Result<T, (BuildFailure, usize)> {
    let mut attempts = 0_usize;
    let mut delays = retry.delays.iter();
    loop {
        attempts = attempts.saturating_add(1);
        match build(config) {
            Ok(engine) => return Ok(engine),
            Err(failure) if failure.class == FailureClass::Environment => {
                let Some(delay) = delays.next() else {
                    return Err((failure, attempts));
                };
                tracing::warn!(
                    attempt = attempts,
                    error = %failure.message,
                    "building the engine failed with an environment error; retrying the same configuration"
                );
                std::thread::sleep(*delay);
            }
            Err(failure) => return Err((failure, attempts)),
        }
    }
}

/// The start-up error for an environment-class failure that outlived its
/// retries: it says plainly that nothing was rolled back.
fn environment_error(failure: &BuildFailure, attempts: usize) -> String {
    format!(
        "building the engine failed with an environment error (not a configuration fault), \
         after {attempts} attempts: {}; the saved configuration was not changed",
        failure.message
    )
}

/// Commit a planned fallback once the engine has been BUILT from last-good
/// (#324): the operator's `desired.json` is NEVER moved or overwritten. A
/// copy of the rejected bytes is kept under `rejected/` (deduplicated), the
/// persisted `boot-fallback.json` marker is written, and
/// [`StartConfig::boot_fallback`] is recorded (the warning). A start that does
/// not plan a fallback is returned unchanged, except that a start which ran
/// from the accepted state settles (removes) a stale marker
/// ([`crate::last_good::settle_marker`]).
///
/// If the copy or the marker cannot be written (`ENOSPC`, a read-only state
/// dir) the start STILL proceeds from last-good: `desired.json` is untouched so
/// nothing is lost, and availability comes first; the in-memory warning names
/// what could not be persisted.
///
/// # Errors
/// Never in practice; kept fallible so a future step that must refuse can.
pub fn commit_fallback(start: StartConfig, boot_path: &Path) -> Result<StartConfig, String> {
    commit_fallback_with(
        start,
        boot_path,
        &multiview_control::boot_model::write_atomic_checked,
    )
}

/// [`commit_fallback`] with an injectable atomic writer (the `ENOSPC` /
/// `AfterRename` test seam).
///
/// # Errors
/// See [`commit_fallback`].
pub fn commit_fallback_with(
    mut start: StartConfig,
    boot_path: &Path,
    writer: &crate::last_good::AtomicWriter<'_>,
) -> Result<StartConfig, String> {
    let Some(pending) = start.pending_fallback.take() else {
        // No fallback: if this start ran from the accepted state, a pending
        // marker from an earlier fallback is now false — settle it (#324).
        let _ = crate::last_good::settle_marker(boot_path, &start);
        return Ok(start);
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let mut persist_errors = Vec::new();
    let kept_at = match crate::last_good::keep_rejected_copy_with(
        boot_path,
        &pending.rejected_text,
        now_ms,
        writer,
    ) {
        Ok(kept) => Some(kept),
        Err(error) => {
            persist_errors.push(error);
            None
        }
    };
    // Redacted + bounded before anything persists, publishes or logs it.
    let mut fallback = crate::last_good::Fallback::new(&pending.reason, now_ms);
    fallback.kept_at = kept_at;
    if let Err(error) = crate::last_good::write_marker_with(boot_path, &fallback, writer) {
        persist_errors.push(error);
    }
    if !persist_errors.is_empty() {
        fallback.persist_error = Some(multiview_control::bounded_warning_text(
            &multiview_control::catalogue::netdiag::redact_text(&persist_errors.join("; ")),
        ));
    }
    tracing::error!(
        boot = %boot_path.display(),
        reason = %fallback.reason,
        kept_at = %fallback.kept_at.as_deref().map_or_else(|| "(not kept)".into(), |p| p.display().to_string()),
        persist_error = fallback.persist_error.as_deref().unwrap_or("none"),
        "the accepted state cannot start; started from the last state that started \
         (desired.json is unchanged)"
    );
    start.boot_fallback = Some(fallback);
    Ok(start)
}

/// Build the engine for `start`; if that fails on the ACCEPTED state with a
/// STATE-class error, plan a fallback to a different last-good and build that.
/// The returned start still carries any pending fallback: call
/// [`commit_fallback`] with it (the build succeeded, so recording is now safe).
/// Called before the output clock exists.
///
/// An ENVIRONMENT-class error on any attempt (accepted, boot or last-good) is
/// retried on the same configuration per `retry` and then fails start-up: no
/// fallback is planned and nothing is written (#324).
///
/// # Errors
/// The build error when there is nothing different to fall back to, the
/// fallback's own failure, or an environment failure after its retries.
/// Nothing on disk changed in any case.
pub fn build_with_fallback<T>(
    start: StartConfig,
    boot_path: &Path,
    retry: &RetryPolicy,
    build: impl Fn(&MultiviewConfig) -> Result<T, BuildFailure>,
) -> Result<(StartConfig, T), String> {
    let (error, attempts) = match build_retrying(&start.running, retry, &build) {
        Ok(engine) => return Ok((start, engine)),
        Err(failed) => failed,
    };
    if error.class == FailureClass::Environment {
        return Err(environment_error(&error, attempts));
    }
    let error = error.message;
    // A planned fallback that does not build either, or a start that did not
    // come from the accepted state: nothing else to try.
    if start.pending_fallback.is_some() {
        return Err(format!(
            "{}; the last-good state does not build either: {error}",
            start
                .pending_fallback
                .as_ref()
                .map_or_else(String::new, |p| p.reason.clone())
        ));
    }
    // Only a start resolved from the accepted state, with the exact bytes it
    // was built from, can fall back.
    let Some(rejected_text) = start.accepted_text.clone() else {
        return Err(error);
    };
    let reason = format!("building the engine from the accepted state failed: {error}");
    let planned = plan_last_good(
        start.loaded.clone(),
        start.boot_text.clone(),
        boot_path,
        reason.clone(),
        rejected_text,
    )
    // Nothing different to fall back to: the original build error, verbatim.
    .map_err(|planned| if planned == reason { error.clone() } else { planned })?;
    match build_retrying(&planned.running, retry, &build) {
        Ok(engine) => Ok((planned, engine)),
        Err((second, attempts)) if second.class == FailureClass::Environment => {
            Err(environment_error(&second, attempts))
        }
        Err((second, _)) => Err(format!(
            "{reason}; the last-good state does not build either: {}",
            second.message
        )),
    }
}

/// Side-effect-free preflight of what `multiview run` would do (used by
/// `multiview validate`): the SAME decision start-up makes
/// ([`resolve_start_config`] writes nothing), so "fall back" is only predicted
/// when the spliced last-good copy itself admits. A pipeline build failure is
/// not modelled (it needs the encoder/devices of the target host).
///
/// # Errors
/// The exact reason start-up would reject the accepted state, and whether a
/// start would fall back to the last-good copy or fail.
pub fn preflight_start(
    boot: &MultiviewConfig,
    boot_text: &str,
    boot_path: &Path,
) -> Result<(), PreflightRefusal> {
    match resolve_start_config(boot.clone(), boot_text.to_owned(), boot_path) {
        Ok(start) => match start.pending_fallback {
            Some(pending) => Err(PreflightRefusal {
                reason: pending.reason,
                falls_back: true,
            }),
            None => crate::last_good::admit(&start.running).map_err(|reason| PreflightRefusal {
                reason,
                falls_back: false,
            }),
        },
        Err(reason) => Err(PreflightRefusal {
            reason,
            falls_back: false,
        }),
    }
}

/// Why `multiview validate` predicts a start would not use the accepted state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PreflightRefusal {
    /// The exact start-up reason.
    pub reason: String,
    /// `true`: a start would fall back to the last-good copy (and raise the
    /// `accepted-state-rejected` warning); `false`: a start would fail.
    pub falls_back: bool,
}

#[cfg(test)]
mod failure_class_tests {
    #![allow(clippy::unwrap_used)]
    use super::{io_failure_class, FailureClass};

    /// #324: classification by OS error kind, driven with real `io::Error`s
    /// (a bound port, a held lock, a forbidden path) and raw errnos.
    #[test]
    fn host_io_kinds_are_environment_and_path_kinds_are_state() {
        use std::io::ErrorKind;
        // A real EADDRINUSE from the OS.
        let held = std::net::TcpListener::bind("[::1]:0").unwrap();
        let addr = held.local_addr().unwrap();
        let in_use = std::net::TcpListener::bind(addr).unwrap_err();
        assert_eq!(in_use.kind(), ErrorKind::AddrInUse);
        assert_eq!(io_failure_class(in_use.kind()), FailureClass::Environment);
        // The kinds the raw OS errors map to (portable).
        for kind in [
            ErrorKind::ResourceBusy,
            ErrorKind::PermissionDenied,
            ErrorKind::StorageFull,
            ErrorKind::ReadOnlyFilesystem,
            ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                io_failure_class(kind),
                FailureClass::Environment,
                "{kind:?}"
            );
        }
        // Linux errno numbers (they differ per OS: 11 is EDEADLK on macOS):
        // EBUSY (16), EACCES (13), ENOSPC (28), EROFS (30), EAGAIN (11).
        #[cfg(target_os = "linux")]
        for errno in [16, 13, 28, 30, 11] {
            let error = std::io::Error::from_raw_os_error(errno);
            assert_eq!(
                io_failure_class(error.kind()),
                FailureClass::Environment,
                "errno {errno}: {error}"
            );
        }
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::InvalidInput,
            ErrorKind::InvalidData,
            ErrorKind::NotADirectory,
            ErrorKind::Other,
        ] {
            assert_eq!(io_failure_class(kind), FailureClass::State, "{kind:?}");
        }
    }
}
