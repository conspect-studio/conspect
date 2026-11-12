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
use multiview_control::boot_model::{load_desired, load_resume_config, BootModel};

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
    /// the last-good copy instead (#317): the reason and where the rejected
    /// state was kept. Raised as the `accepted-state-rejected` health warning.
    pub boot_fallback: Option<crate::last_good::Fallback>,
}

impl StartConfig {
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
        BootModel::new(
            resolved,
            self.loaded.clone(),
            self.running.clone(),
            self.start,
            self.resumed,
            self.resume_fallback.clone(),
        )
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
    match load_desired(boot_path) {
        Ok(Some(desired)) => {
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
                    });
                }
                Err(error) => {
                    let reason = format!("accepted desired state cannot be resumed: {error}");
                    return start_from_last_good(boot, boot_text, boot_path, reason);
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            let reason = format!("accepted desired state cannot be read: {error}");
            return start_from_last_good(boot, boot_text, boot_path, reason);
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
            }
        }
    })
}

/// Start from the last-good copy because the accepted state cannot start
/// (`reason`). Without a usable last-good copy this is the original error: a
/// first install never invents a state.
///
/// # Errors
/// `reason` when there is no usable last-good copy; an I/O reason when the
/// rejected state could not be kept (it is never discarded unkept).
pub fn start_from_last_good(
    boot: MultiviewConfig,
    boot_text: String,
    boot_path: &Path,
    reason: String,
) -> Result<StartConfig, String> {
    start_from_last_good_with(boot, boot_text, boot_path, reason, |_| Ok(()))
        .map(|(start, ())| start)
}

/// [`start_from_last_good`] that first proves the last-good state really
/// builds (`build`), BEFORE the operator's accepted state is moved aside: an
/// environment fault that breaks every state (a missing encoder) never
/// quarantines the accepted state.
///
/// # Errors
/// As [`start_from_last_good`], plus the last-good build failure.
pub fn start_from_last_good_with<T>(
    boot: MultiviewConfig,
    boot_text: String,
    boot_path: &Path,
    reason: String,
    build: impl FnOnce(&MultiviewConfig) -> Result<T, String>,
) -> Result<(StartConfig, T), String> {
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
    let pipeline_built = build(&running)
        .map_err(|error| format!("{reason}; the last-good state does not build either: {error}"))?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let kept_at = crate::last_good::quarantine_and_restore(boot_path, &last_good, now_ms).map_err(
        |error| format!("{reason}; and the fallback could not keep the rejected state: {error}"),
    )?;
    let fallback = crate::last_good::Fallback { reason, kept_at };
    tracing::error!(
        boot = %boot_path.display(),
        reason = %fallback.reason,
        kept_at = %fallback.kept_at.display(),
        "the accepted state cannot start; starting from the last state that started"
    );
    Ok((
        StartConfig {
            running,
            loaded: boot,
            boot_text,
            start: StartMode::Resume,
            resumed: true,
            resume_fallback: None,
            desired: Some(last_good),
            boot_fallback: Some(fallback),
        },
        pipeline_built,
    ))
}

/// The pipeline build failed for a start resolved from the accepted state:
/// fall back to the last-good copy when that is a DIFFERENT state (a last-good
/// identical to what just failed cannot help) and it builds. Called before the
/// output clock exists.
///
/// # Errors
/// `build_error` when there is nothing different to fall back to, or the
/// fallback's own failure.
pub fn fallback_after_build_failure<T>(
    start: &StartConfig,
    boot_path: &Path,
    build_error: &str,
    build: impl FnOnce(&MultiviewConfig) -> Result<T, String>,
) -> Result<(StartConfig, T), String> {
    let from_accepted = start.desired.is_some() && start.boot_fallback.is_none();
    let differs = crate::last_good::load_last_good(boot_path).is_some_and(|last_good| {
        start.desired.as_ref().is_some_and(|accepted| {
            serde_json::to_value(accepted).ok() != serde_json::to_value(&last_good).ok()
        })
    });
    if !from_accepted || !differs {
        return Err(build_error.to_owned());
    }
    start_from_last_good_with(
        start.loaded.clone(),
        start.boot_text.clone(),
        boot_path,
        format!("building the pipeline from the accepted state failed: {build_error}"),
        build,
    )
}

/// Side-effect-free preflight of what `multiview run` would start from (used by
/// `multiview validate`): the accepted state when one exists (spliced exactly
/// as start-up splices it), else the start policy's document, checked with the
/// start-up admission rule. Never writes, never falls back.
///
/// # Errors
/// The exact reason start-up would reject the accepted state; the message says
/// whether a last-good copy exists to fall back to.
pub fn preflight_start(
    boot: &MultiviewConfig,
    boot_text: &str,
    boot_path: &Path,
) -> Result<(), String> {
    let running = match load_desired(boot_path) {
        Ok(Some(desired)) => splice_storeless_sections(desired.config, boot),
        Ok(None) => {
            return resolve_start_config(boot.clone(), boot_text.to_owned(), boot_path)
                .and_then(|start| crate::last_good::admit(&start.running));
        }
        Err(error) => {
            return Err(with_last_good_note(
                &format!("accepted desired state cannot be read: {error}"),
                boot_path,
            ))
        }
    };
    crate::last_good::admit(&running).map_err(|error| {
        with_last_good_note(
            &format!("accepted desired state cannot be resumed: {error}"),
            boot_path,
        )
    })
}

fn with_last_good_note(reason: &str, boot_path: &Path) -> String {
    if crate::last_good::load_last_good(boot_path).is_some() {
        format!("{reason} (a start would fall back to the last-good copy and raise an alarm)")
    } else {
        format!("{reason} (no last-good copy: a start would fail)")
    }
}
