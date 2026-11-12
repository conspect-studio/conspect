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
    /// the last-good copy instead (#317): the reason and where the rejected
    /// state was kept. Raised as the `accepted-state-rejected` health warning.
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
        BootModel::new(
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
        )
    }
}

/// Splice the storeless restart-only sections from the BOOT document into a
/// resumed Running document (ADR-W024 review M1): `control`, `placement`,
/// `walls`, `schema_version`, `events` and the routing CROSSPOINT tables
/// (`routing.video` / `audio` / `subtitle`) have no control store — the boot
/// file is their durable truth, and a restart is exactly when they take effect.
/// Without the splice, a boot-file `[control] listen` edit would be silently
/// lost on the very restart the operator performed to apply it (the stale
/// `active.toml` copy would win).
///
/// `routing.output` is **store-backed** (ADR-0108 §6.1, #329): an applied
/// output take is committed into the accepted state (`persist_feed`), so
/// those rows come from `running` — accepted rows win per output. A boot-file
/// `[[routing.output]]` row for an output the running document has no row for
/// seeds it (the operator's file declares the starting feed). Boot rows for an
/// output the running document does not declare are dropped (they would not
/// validate). See [`splice_output_routing`].
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
    running.routing = splice_output_routing(&running, boot);
    running.events = boot.events;
    running
}

/// The routing table a resumed Running document starts with: the boot file's
/// crosspoint tables (video/audio/subtitle, restart-only) plus the output
/// rows of `running` (store-backed: the accepted feeds), seeded per output by
/// a boot row where `running` has none. `None` when every table is empty.
fn splice_output_routing(
    running: &MultiviewConfig,
    boot: &MultiviewConfig,
) -> Option<multiview_config::routing::RoutingTable> {
    let mut table = boot.routing.clone().unwrap_or_default();
    let accepted: &[multiview_config::OutputCrosspoint] = running
        .routing
        .as_ref()
        .map_or(&[], |t| t.output.as_slice());
    let declared: std::collections::HashSet<String> = running
        .outputs
        .iter()
        .map(multiview_config::Output::id)
        .collect();
    let mut output: Vec<multiview_config::OutputCrosspoint> = accepted.to_vec();
    for seed in std::mem::take(&mut table.output) {
        if declared.contains(&seed.output) && !output.iter().any(|xp| xp.output == seed.output) {
            output.push(seed);
        }
    }
    table.output = output;
    let empty = table.output.is_empty()
        && table.video.is_empty()
        && table.audio.is_empty()
        && table.subtitle.is_empty();
    (!empty).then_some(table)
}

/// Resolve the starting Running state for a run booted from `boot` (already
/// parsed + validated) at `boot_path` (ADR-W024 §4).
///
/// The accepted state (`desired.json`) outranks everything when present. Under
/// `start = "resume"` the persisted `active.toml` next to the boot file
/// becomes Running when it is valid. In both (and for a last-good fallback)
/// the storeless restart-only sections (`control`, `placement`, `walls`,
/// `events`, `schema_version`, the routing crosspoint tables) are spliced from
/// the BOOT document (review M1: a boot-file edit to a restart-only section
/// must take effect on restart), while `routing.output` rows are store-backed:
/// the resumed document's rows (the applied takes) win per output and a boot
/// row only seeds an output that has none (#329; ADR-0108 §6.1). `salvos`/`tally_profiles`
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

/// A decided-but-not-yet-committed fallback: the rejected bytes and the
/// last-good envelope that will replace them once the engine built from it.
#[derive(Debug, Clone)]
pub struct PendingFallback {
    reason: String,
    rejected_text: String,
    last_good: multiview_control::boot_model::DesiredState,
}

/// Plan a start from the last-good copy because the accepted state cannot
/// start (`reason`). NOTHING is written here: the swap happens in
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
        desired: Some(last_good.clone()),
        boot_fallback: None,
        pending_fallback: Some(PendingFallback {
            reason,
            rejected_text,
            last_good,
        }),
        accepted_text: None,
    })
}

/// Commit a planned fallback once the engine has been BUILT from last-good:
/// keep the rejected bytes under `rejected/`, restore last-good as the
/// accepted state, and record [`StartConfig::boot_fallback`] (the warning).
/// A start that does not plan a fallback is returned unchanged.
///
/// # Errors
/// The rejected state could not be kept or last-good not restored; the run
/// must not start (the operator's state is never discarded unkept).
pub fn commit_fallback(mut start: StartConfig, boot_path: &Path) -> Result<StartConfig, String> {
    let Some(pending) = start.pending_fallback.take() else {
        return Ok(start);
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let kept_at = crate::last_good::quarantine_and_restore(
        boot_path,
        &pending.rejected_text,
        &pending.last_good,
        now_ms,
    )
    .map_err(|error| {
        format!(
            "{}; and the fallback could not complete: {error}",
            pending.reason
        )
    })?;
    let fallback = crate::last_good::Fallback {
        reason: pending.reason,
        kept_at,
    };
    tracing::error!(
        boot = %boot_path.display(),
        reason = %fallback.reason,
        kept_at = %fallback.kept_at.display(),
        "the accepted state cannot start; started from the last state that started"
    );
    start.boot_fallback = Some(fallback);
    Ok(start)
}

/// Build the engine for `start`; if that fails on the ACCEPTED state, plan a
/// fallback to a different last-good and build that. The returned start still
/// carries any pending fallback: call [`commit_fallback`] with it (the build
/// succeeded, so the swap is now safe). Called before the output clock exists.
///
/// # Errors
/// The build error when there is nothing different to fall back to, or the
/// fallback's own failure. Nothing on disk changed in either case.
pub fn build_with_fallback<T>(
    start: StartConfig,
    boot_path: &Path,
    build: impl Fn(&MultiviewConfig) -> Result<T, String>,
) -> Result<(StartConfig, T), String> {
    let error = match build(&start.running) {
        Ok(engine) => return Ok((start, engine)),
        Err(error) => error,
    };
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
    match build(&planned.running) {
        Ok(engine) => Ok((planned, engine)),
        Err(second) => Err(format!(
            "{reason}; the last-good state does not build either: {second}"
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
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod splice_tests {
    use super::splice_storeless_sections;
    use multiview_config::{MultiviewConfig, VideoFeed};

    /// Two sources, two cells, two HLS outputs (`o1`, `o2`); `extra` appended.
    fn doc(extra: &str) -> MultiviewConfig {
        let text = format!(
            r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[outputs]]
id = "o1"
kind = "hls"
path = "/tmp/boot-splice/o1.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
id = "o2"
kind = "hls"
path = "/tmp/boot-splice/o2.m3u8"
codec = "mpeg2video"
segment_ms = 1000
{extra}"##
        );
        let config = MultiviewConfig::load_from_toml(&text).expect("parses");
        config.validate().expect("valid");
        config
    }

    fn feed_of(config: &MultiviewConfig, output: &str) -> Option<VideoFeed> {
        config
            .routing
            .as_ref()?
            .output
            .iter()
            .find(|xp| xp.output == output)
            .map(multiview_config::OutputCrosspoint::effective_video)
    }

    const BOOT_VIDEO_XP: &str = r#"
[[routing.video]]
cell = "cell_b"
source = { input_id = "in_b", kind = { kind = "video" } }
"#;

    #[test]
    fn the_accepted_output_row_wins_over_the_boot_row() {
        let boot = doc(&format!(
            "{BOOT_VIDEO_XP}\n[[routing.output]]\noutput = \"o1\"\nvideo = {{ source = \"in_a\" }}\n"
        ));
        let accepted = doc("[[routing.output]]\noutput = \"o1\"\nvideo = { source = \"in_b\" }\n");
        let running = splice_storeless_sections(accepted, &boot);
        assert_eq!(
            feed_of(&running, "o1"),
            Some(VideoFeed::Source("in_b".into())),
            "the persisted take survives the restart"
        );
        running.validate().expect("the spliced document validates");
    }

    #[test]
    fn a_boot_row_seeds_an_output_the_accepted_state_has_no_row_for() {
        let boot = doc(
            "[[routing.output]]\noutput = \"o1\"\nvideo = { source = \"in_a\" }\n\
             [[routing.output]]\noutput = \"o2\"\nvideo = { source = \"in_a\" }\n",
        );
        let accepted = doc("[[routing.output]]\noutput = \"o2\"\nvideo = { source = \"in_b\" }\n");
        let running = splice_storeless_sections(accepted, &boot);
        assert_eq!(
            feed_of(&running, "o1"),
            Some(VideoFeed::Source("in_a".into())),
            "no accepted row: the boot row seeds o1"
        );
        assert_eq!(
            feed_of(&running, "o2"),
            Some(VideoFeed::Source("in_b".into())),
            "accepted row wins per output"
        );
        assert_eq!(running.routing.as_ref().map(|t| t.output.len()), Some(2));
    }

    #[test]
    fn the_other_routing_tables_still_come_from_the_boot_file() {
        // Boot declares a video crosspoint and no output rows; the accepted
        // copy has an output row and NO video crosspoint (a stale copy).
        let boot = doc(BOOT_VIDEO_XP);
        let accepted = doc("[[routing.output]]\noutput = \"o2\"\nvideo = { source = \"in_a\" }\n");
        let running = splice_storeless_sections(accepted, &boot);
        let table = running.routing.as_ref().expect("a routing table");
        assert_eq!(table.video.len(), 1, "video crosspoints spliced from boot");
        assert_eq!(table.video[0].cell, "cell_b");
        assert_eq!(
            feed_of(&running, "o2"),
            Some(VideoFeed::Source("in_a".into()))
        );
        // And the reverse: a boot edit that removes a crosspoint takes effect,
        // and nothing at all routed stays `None` (today's absent table).
        let running = splice_storeless_sections(doc(BOOT_VIDEO_XP), &doc(""));
        assert!(running.routing.is_none(), "{:?}", running.routing);
    }

    #[test]
    fn a_boot_row_for_an_output_the_running_document_lacks_is_dropped() {
        let boot = doc("[[routing.output]]\noutput = \"o2\"\nvideo = { source = \"in_a\" }\n");
        let mut accepted = doc("");
        accepted.outputs.truncate(1);
        let running = splice_storeless_sections(accepted, &boot);
        assert!(running.routing.is_none(), "{:?}", running.routing);
    }
}
