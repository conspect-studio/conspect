//! The `multiview validate` subcommand: load a config, run
//! [`MultiviewConfig::validate`] (grid-solve + cross-references), and render a
//! clear human report.
//!
//! [`validate_config`] is total: a missing file, malformed TOML, or a failed
//! semantic invariant all produce a [`ValidationReport`] whose
//! [`ValidationReport::is_ok`] is `false` and whose [`ValidationReport::render`]
//! explains *why*. It returns `Err` only for an unexpected internal fault, so
//! the binary can always print a report and pick an exit code from the
//! report's status.
use std::path::{Path, PathBuf};

use multiview_config::MultiviewConfig;

/// The outcome of validating one configuration document.
///
/// Carries the resolved path, the pass/fail status, and — on failure — the
/// reason. [`ValidationReport::render`] turns it into the text the binary
/// prints; [`ValidationReport::is_ok`] drives the process exit code.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValidationReport {
    /// The config path that was validated.
    pub path: PathBuf,
    /// The validation outcome.
    pub status: ValidationStatus,
    /// Non-fatal advisories from the parsed document (e.g. a clock that sets
    /// both `timezone` and `tz_offset_minutes` — legal, but the offset is
    /// ignored). Empty when the document did not parse or nothing applies.
    /// Rendered as `WARN` lines so the operator sees them.
    pub warnings: Vec<String>,
    /// What the engine will REALLY start from (#317): the accepted state
    /// (`<config-dir>/.multiview/desired.json`) when one exists, else the boot
    /// document, checked with the start-up admission rule. `None` when the boot
    /// document itself did not validate.
    pub start: Option<StartCheck>,
}

/// The start-up check of a [`ValidationReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StartCheck {
    /// The document the engine starts from: the accepted-state file, or the boot
    /// file when no accepted state exists.
    pub source: PathBuf,
    /// Whether that is the accepted state (`desired.json`).
    pub accepted: bool,
    /// `None` when the engine can start from it, else the exact reason start-up
    /// would give.
    pub refusal: Option<String>,
    /// With a refusal: whether a start would fall back to the last-good copy
    /// (`true`) or fail (`false`). Either way the preflight exits non-zero.
    pub falls_back: bool,
}

/// Whether a config validated, and if not, the human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationStatus {
    /// The document parsed and passed every semantic invariant, plus a short
    /// summary of what was validated (canvas, cell/source/output counts).
    Ok {
        /// One-line summary of the validated document.
        summary: String,
    },
    /// The file could not be read (e.g. missing / permission denied).
    Unreadable {
        /// The underlying read error, rendered.
        reason: String,
    },
    /// The file was read but is not a valid config (parse or semantic error).
    Invalid {
        /// The first violated invariant or parse error, rendered.
        reason: String,
    },
}

impl ValidationReport {
    /// Whether the document validated cleanly.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.status, ValidationStatus::Ok { .. })
            && self
                .start
                .as_ref()
                .is_none_or(|start| start.refusal.is_none())
    }

    /// Render the report as the multi-line text the binary prints. Non-fatal
    /// advisories follow the status line as `WARN` lines.
    #[must_use]
    pub fn render(&self) -> String {
        let path = self.path.display();
        let mut out = match &self.status {
            ValidationStatus::Ok { summary } => {
                format!("OK   {path}\n     {summary}")
            }
            ValidationStatus::Unreadable { reason } => {
                format!("FAIL {path}\n     could not read file: {reason}")
            }
            ValidationStatus::Invalid { reason } => {
                format!("FAIL {path}\n     invalid configuration: {reason}")
            }
        };
        if let Some(start) = &self.start {
            let what = if start.accepted {
                "accepted state (the engine starts from this, not the boot file)"
            } else {
                "boot document (no accepted state yet)"
            };
            let source = start.source.display();
            let line = match &start.refusal {
                None => format!("\nOK   start: {what}\n     {source}"),
                Some(reason) if start.falls_back => format!(
                    "\nFAIL start: {what}\n     {source}\n     the engine would REJECT this state and start from the last-good copy instead (raising the accepted-state-rejected warning): {reason}"
                ),
                Some(reason) => format!(
                    "\nFAIL start: {what}\n     {source}\n     the engine would refuse to start (no usable last-good copy to fall back to): {reason}"
                ),
            };
            out.push_str(&line);
        }
        for warning in &self.warnings {
            out.push_str("\nWARN ");
            out.push_str(warning);
        }
        out
    }
}

/// Load and validate the configuration document at `path`.
///
/// Reads the file, parses it as TOML into a [`MultiviewConfig`], and runs
/// [`MultiviewConfig::validate`] (unique ids, cell↔source bindings, output codecs,
/// grid solve, and the solved [`multiview_core::layout::Layout`]'s structural
/// check). Every failure mode — unreadable file, malformed TOML, a violated
/// invariant — is captured in the returned [`ValidationReport`] rather than
/// surfaced as an error, so the caller can print one consistent report.
///
/// # Errors
///
/// Currently infallible at the `Result` layer (all failures are reported in the
/// [`ValidationReport`]); the [`anyhow::Result`] wrapper reserves room for an
/// unexpected internal fault and keeps the app-boundary signature uniform.
pub fn validate_config(path: &Path) -> anyhow::Result<ValidationReport> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return Ok(ValidationReport {
                path: path.to_path_buf(),
                status: ValidationStatus::Unreadable {
                    reason: err.to_string(),
                },
                warnings: Vec::new(),
                start: None,
            });
        }
    };

    let config = match MultiviewConfig::load_from_toml(&text) {
        Ok(config) => config,
        Err(err) => {
            return Ok(ValidationReport {
                path: path.to_path_buf(),
                status: ValidationStatus::Invalid {
                    reason: format!("parse: {err}"),
                },
                warnings: Vec::new(),
                start: None,
            });
        }
    };

    // Non-fatal advisories apply to any *parsed* document (a semantically
    // invalid one still benefits — the operator fixes everything in one pass).
    let warnings = config_warnings(&config);

    match config.validate() {
        Ok(()) => {
            let summary = summarize(&config);
            let start = start_check(&config, &text, path);
            Ok(ValidationReport {
                path: path.to_path_buf(),
                status: ValidationStatus::Ok { summary },
                warnings,
                start: Some(start),
            })
        }
        Err(err) => Ok(ValidationReport {
            path: path.to_path_buf(),
            status: ValidationStatus::Invalid {
                reason: err.to_string(),
            },
            warnings,
            start: None,
        }),
    }
}

/// Resolve what `multiview run` would start from — the SAME
/// [`crate::boot::resolve_start_config`] start-up calls — and apply the SAME
/// start-up admission rule `Pipeline::build` enforces
/// ([`multiview_config::boot_admission`]). Read-only: nothing is written.
fn start_check(boot: &MultiviewConfig, text: &str, path: &Path) -> StartCheck {
    let desired = multiview_control::boot_model::state_dir_for(path).join("desired.json");
    let accepted = desired.exists();
    let source = if accepted {
        desired
    } else {
        path.to_path_buf()
    };
    let (refusal, falls_back) = match crate::boot::preflight_start(boot, text, path) {
        Ok(()) => (None, false),
        Err(refusal) => (Some(refusal.reason), refusal.falls_back),
    };
    StartCheck {
        source,
        accepted,
        refusal,
        falls_back,
    }
}

/// Collect every non-fatal advisory for a parsed config — currently the per-
/// source clock advisories ([`multiview_config::Source::clock_warnings`], e.g.
/// `timezone` + `tz_offset_minutes` both set). The `validate` subcommand renders
/// these as `WARN` report lines; `multiview run` startup emits each via
/// `tracing::warn!` before the engine starts.
#[must_use]
pub fn config_warnings(config: &MultiviewConfig) -> Vec<String> {
    config
        .sources
        .iter()
        .flat_map(multiview_config::Source::clock_warnings)
        .collect()
}

/// A one-line human summary of a validated document.
fn summarize(config: &MultiviewConfig) -> String {
    let cadence = config.canvas.fps.rational();
    format!(
        "{}x{} @ {}/{} fps, {} source(s), {} cell(s), {} output(s)",
        config.canvas.width,
        config.canvas.height,
        cadence.num,
        cadence.den,
        config.sources.len(),
        config.cells.len(),
        config.outputs.len(),
    )
}
