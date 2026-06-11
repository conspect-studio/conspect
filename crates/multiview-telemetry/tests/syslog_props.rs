//! Property tests for the RFC 5424 syslog formatter invariants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::syslog::{Facility, SdElement, Severity, SyslogMessage};
use proptest::prelude::*;

/// Every modelled (facility, severity) pair yields a PRIVAL in the RFC 5424
/// range `0..=191`, and the rendered header begins with `<PRIVAL>1`.
fn facilities() -> impl Strategy<Value = Facility> {
    prop_oneof![
        Just(Facility::Kernel),
        Just(Facility::Daemon),
        Just(Facility::Local0),
        Just(Facility::Local1),
        Just(Facility::Local2),
        Just(Facility::Local3),
        Just(Facility::Local4),
        Just(Facility::Local5),
        Just(Facility::Local6),
        Just(Facility::Local7),
    ]
}

fn severities() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Emergency),
        Just(Severity::Alert),
        Just(Severity::Critical),
        Just(Severity::Error),
        Just(Severity::Warning),
        Just(Severity::Notice),
        Just(Severity::Informational),
        Just(Severity::Debug),
    ]
}

proptest! {
    #[test]
    fn prival_in_range_and_header_prefix(facility in facilities(), severity in severities()) {
        let prival = facility.prival(severity);
        prop_assert!(prival <= 191, "PRIVAL {} exceeds the RFC 5424 ceiling", prival);
        let wire = SyslogMessage::new(facility, severity).to_rfc5424();
        let prefix = format!("<{prival}>1");
        prop_assert!(wire.starts_with(&prefix), "header {wire} lacks prefix {prefix}");
    }

    /// Escaping a param value never leaves an unescaped `"`, `\`, or `]` inside
    /// the value, and the rendered element is wrapped in a single `[`..`]`.
    #[test]
    fn structured_data_escaping_balances(value in ".{0,40}") {
        let wire = SyslogMessage::new(Facility::Local0, Severity::Informational)
            .structured_data(vec![SdElement::new("ctx@1").param("k", value)])
            .to_rfc5424();
        // The SD opens with `[ctx@1 k="` and closes with `"]`.
        prop_assert!(wire.contains("[ctx@1 k=\""));
        prop_assert!(wire.ends_with("\"]"));

        // Walk the value region and confirm every reserved char is preceded by a
        // backslash (which is itself escaped, so we skip the next char).
        let start = wire.find("k=\"").unwrap() + 3;
        let end = wire.len() - 2; // strip the closing `"]`
        let body: Vec<char> = wire[start..end].chars().collect();
        let mut i = 0;
        while i < body.len() {
            let c = body[i];
            if c == '\\' {
                // An escape consumes the following char verbatim.
                prop_assert!(i + 1 < body.len(), "dangling escape in {body:?}");
                i += 2;
                continue;
            }
            prop_assert!(
                c != '"' && c != ']',
                "unescaped reserved char {c:?} in {body:?}",
            );
            i += 1;
        }
    }
}
