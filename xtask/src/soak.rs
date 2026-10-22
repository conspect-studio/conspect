//! `cargo xtask soak-report` — render the acceptance-soak verdict (DEV-C4) from
//! a captured metrics series produced by `scripts/soak-acceptance.sh`.
//!
//! The capture is a small JSON document: one offset series per disciplined
//! clock-source leg (`multiview_clock_servo_offset_nanoseconds` samples scraped
//! over the run) plus the output-tick counts sampled across the invariant-#1
//! chaos (PTP/WS kill) window. The pass/fail maths is the tested, dependency-
//! free [`multiview_telemetry::soak`] analyzer — this module is only the
//! capture-shape glue + the human/CI-readable rendering, so the same logic that
//! CI exercises is what a hardware soak is judged by.

use std::fmt::Write as _;

use multiview_telemetry::clock::ClockSourceLabel;
use multiview_telemetry::soak::{cadence_uninterrupted, evaluate_offset, SoakReport};
use serde::Deserialize;

/// Errors rendering a soak report from a capture document.
#[derive(Debug, thiserror::Error)]
pub enum SoakReportError {
    /// The capture file could not be read.
    #[error("reading the soak capture {path}: {source}")]
    Read {
        /// The capture path that failed to read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The capture JSON could not be parsed.
    #[error("parsing the soak capture: {0}")]
    Parse(#[from] serde_json::Error),
    /// A leg named an unknown clock source.
    #[error("unknown clock source {0:?} (expected \"ptp\" or \"system\")")]
    UnknownSource(String),
}

/// A captured soak metrics series: per-leg offset samples + the chaos-window
/// output-tick counts. Mirrors the JSON `scripts/soak-acceptance.sh` emits.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakCapture {
    /// One offset series per disciplined clock-source leg.
    #[serde(default)]
    pub offsets: Vec<OffsetLeg>,
    /// The output-tick counts sampled across the PTP/WS kill window.
    pub cadence: CadenceCapture,
}

/// One clock-source leg's offset samples, in nanoseconds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffsetLeg {
    /// The clock-source label (`"ptp"` or `"system"`).
    pub source: String,
    /// The `|offset|` samples scraped over the run, in nanoseconds.
    pub samples_ns: Vec<i64>,
}

/// The output-tick counts sampled at a fixed wall interval across the chaos
/// window, plus the per-sample floor each interval must advance by.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CadenceCapture {
    /// Monotonic output-tick counts, one per wall-interval sample.
    pub tick_samples: Vec<u64>,
    /// The minimum ticks every interval must have advanced (the cadence floor).
    pub expected_min_delta: u64,
}

/// Map a capture's source string to the typed label.
fn parse_source(source: &str) -> Result<ClockSourceLabel, SoakReportError> {
    match source {
        "ptp" => Ok(ClockSourceLabel::Ptp),
        "system" => Ok(ClockSourceLabel::System),
        other => Err(SoakReportError::UnknownSource(other.to_owned())),
    }
}

/// Build the [`SoakReport`] from a parsed capture via the telemetry analyzer.
///
/// # Errors
/// Returns [`SoakReportError::UnknownSource`] if a leg names a clock source
/// other than `"ptp"` or `"system"`.
pub fn analyze_capture(capture: &SoakCapture) -> Result<SoakReport, SoakReportError> {
    let mut report = SoakReport::default();
    for leg in &capture.offsets {
        let source = parse_source(&leg.source)?;
        if let Some(verdict) = evaluate_offset(source, &leg.samples_ns) {
            report.add_offset(verdict);
        }
    }
    report.set_cadence(cadence_uninterrupted(
        &capture.cadence.tick_samples,
        capture.cadence.expected_min_delta,
    ));
    Ok(report)
}

/// Render a parsed capture into a human/CI-readable report and the pass/fail.
/// Returns `(text, passed)`.
///
/// # Errors
/// Propagates [`SoakReportError::UnknownSource`] from [`analyze_capture`].
pub fn render_capture(capture: &SoakCapture) -> Result<(String, bool), SoakReportError> {
    let report = analyze_capture(capture)?;
    let mut out = String::from("DEV-C4 acceptance-soak report\n");
    for v in report.offsets() {
        let _ = writeln!(
            out,
            "  [{}] offset p99 = {} ns (bound {} ns) — {}",
            v.source.label(),
            v.p99_abs_ns,
            v.threshold_ns,
            if v.pass { "PASS" } else { "FAIL" },
        );
    }
    let cadence = report.cadence_ok().unwrap_or(false);
    let _ = writeln!(
        out,
        "  [cadence] output ticks uninterrupted across the chaos window — {}",
        if cadence { "PASS" } else { "FAIL" },
    );
    let passed = report.passed();
    let _ = writeln!(out, "  VERDICT: {}", if passed { "PASS" } else { "FAIL" });
    Ok((out, passed))
}

/// Read a capture JSON file and render its report + pass/fail.
///
/// # Errors
/// Returns [`SoakReportError::Read`] if the file cannot be read,
/// [`SoakReportError::Parse`] if the JSON is malformed, or
/// [`SoakReportError::UnknownSource`] for an unknown clock-source leg.
pub fn report_from_file(path: &str) -> Result<(String, bool), SoakReportError> {
    let json = std::fs::read_to_string(path).map_err(|source| SoakReportError::Read {
        path: path.to_owned(),
        source,
    })?;
    let capture: SoakCapture = serde_json::from_str(&json)?;
    render_capture(&capture)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn capture(json: &str) -> SoakCapture {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_clean_capture_passes() {
        let c = capture(
            r#"{"offsets":[{"source":"ptp","samples_ns":[0,0,0,0]}],
                "cadence":{"tick_samples":[0,30,60,90],"expected_min_delta":30}}"#,
        );
        let (text, passed) = render_capture(&c).unwrap();
        assert!(passed, "{text}");
        assert!(text.contains("VERDICT: PASS"));
    }

    #[test]
    fn an_offset_breach_fails_the_verdict() {
        let c = capture(
            r#"{"offsets":[{"source":"ptp","samples_ns":[200000,200000]}],
                "cadence":{"tick_samples":[0,30,60],"expected_min_delta":30}}"#,
        );
        let (_text, passed) = render_capture(&c).unwrap();
        assert!(!passed);
    }

    #[test]
    fn a_cadence_stall_fails_the_verdict() {
        let c = capture(
            r#"{"offsets":[{"source":"ptp","samples_ns":[0,0]}],
                "cadence":{"tick_samples":[0,30,30,60],"expected_min_delta":30}}"#,
        );
        let (_text, passed) = render_capture(&c).unwrap();
        assert!(!passed);
    }

    #[test]
    fn an_unknown_source_is_an_error_not_a_silent_pass() {
        let c = capture(
            r#"{"offsets":[{"source":"gps","samples_ns":[0]}],
                "cadence":{"tick_samples":[0,30],"expected_min_delta":30}}"#,
        );
        assert!(matches!(
            render_capture(&c),
            Err(SoakReportError::UnknownSource(_))
        ));
    }
}
