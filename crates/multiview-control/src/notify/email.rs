//! The email notifier: a pure builder over an alarm transition.
//!
//! [`EmailMessage::build`] turns an alarm transition plus addressing
//! ([`EmailEnvelope`]) into a fully-formed message value — from/to, a
//! severity-aware subject line, and a plain-text body — with **no network I/O**.
//! That makes the rendered message exhaustively unit-testable.
//!
//! The actual SMTP send uses `lettre` and lives behind the off-by-default
//! `email` feature (`EmailMessage::to_lettre`). The default build pulls no
//! SMTP/native dependency and stays cargo-deny-clean. As with the webhook
//! notifier the send is driven off the request path; it never sits on the
//! engine's data plane (invariant #10).
use multiview_core::alarm::{AlarmRecord, PerceivedSeverity};

use super::AlarmTransitionKind;

/// The from/to addressing for an alarm email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailEnvelope {
    /// The `From` mailbox.
    pub from: String,
    /// The `To` mailbox.
    pub to: String,
}

impl EmailEnvelope {
    /// Construct an envelope from a sender and recipient mailbox.
    #[must_use]
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

/// A fully-rendered alarm email, built purely from an alarm transition.
///
/// Holds the addressing plus the rendered `subject` and plain-text `body`.
/// Constructed with [`EmailMessage::build`]; converted to a `lettre::Message`
/// only behind the `email` feature (`EmailMessage::to_lettre`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    /// The `From` mailbox.
    pub from: String,
    /// The `To` mailbox.
    pub to: String,
    /// The rendered subject line.
    pub subject: String,
    /// The rendered plain-text body.
    pub body: String,
}

/// The human-readable severity word used in subject lines.
const fn severity_word(severity: PerceivedSeverity) -> &'static str {
    match severity {
        PerceivedSeverity::Cleared => "CLEARED",
        PerceivedSeverity::Indeterminate => "INDETERMINATE",
        PerceivedSeverity::Warning => "WARNING",
        PerceivedSeverity::Minor => "MINOR",
        PerceivedSeverity::Major => "MAJOR",
        PerceivedSeverity::Critical => "CRITICAL",
        // `PerceivedSeverity` is `#[non_exhaustive]`; an unknown future variant
        // renders generically rather than failing the build.
        _ => "ALARM",
    }
}

impl EmailMessage {
    /// Build the alarm email for `record` undergoing `transition`, addressed by
    /// `envelope`.
    ///
    /// This is a **pure** function: it renders the subject and body but performs
    /// no I/O. The subject leads with the severity and transition so it sorts and
    /// pages sensibly; the body carries the structured record fields.
    #[must_use]
    pub fn build(
        envelope: &EmailEnvelope,
        transition: AlarmTransitionKind,
        record: &AlarmRecord,
    ) -> Self {
        let subject = format!(
            "[{severity}] Alarm {transition} — {kind:?} ({id})",
            severity = severity_word(record.severity),
            transition = transition.as_str(),
            kind = record.kind,
            id = record.id.as_str(),
        );
        let acked = if record.ack.is_acked() {
            "acknowledged"
        } else {
            "unacknowledged"
        };
        let body = format!(
            "Alarm {transition}\n\
             id: {id}\n\
             kind: {kind:?}\n\
             severity: {severity}\n\
             scope: {scope:?}\n\
             raised_at_ns: {raised}\n\
             dwell_ns: {dwell}\n\
             latched: {latched}\n\
             ack: {acked}\n",
            transition = transition.as_str(),
            id = record.id.as_str(),
            kind = record.kind,
            severity = severity_word(record.severity),
            scope = record.scope,
            raised = record.raised_at.as_nanos(),
            dwell = record.dwell.as_nanos(),
            latched = record.latched,
        );
        Self {
            from: envelope.from.clone(),
            to: envelope.to.clone(),
            subject,
            body,
        }
    }

    /// Convert this rendered message into a `lettre::Message` ready to send.
    ///
    /// Available only with the `email` feature.
    ///
    /// # Errors
    ///
    /// [`lettre::error::Error`] if an address is malformed or the message cannot
    /// be assembled.
    #[cfg(feature = "email")]
    pub fn to_lettre(&self) -> Result<lettre::Message, lettre::error::Error> {
        use lettre::message::header::ContentType;
        use lettre::Message;
        let from = self
            .from
            .parse()
            .map_err(|_| lettre::error::Error::MissingFrom)?;
        let to = self
            .to
            .parse()
            .map_err(|_| lettre::error::Error::MissingTo)?;
        Message::builder()
            .from(from)
            .to(to)
            .subject(self.subject.clone())
            .header(ContentType::TEXT_PLAIN)
            .body(self.body.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::alarm::{
        AckState, AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity,
    };
    use multiview_core::time::MediaTime;

    use super::{AlarmTransitionKind, EmailEnvelope, EmailMessage};

    fn record(severity: PerceivedSeverity) -> AlarmRecord {
        AlarmRecord::new(
            AlarmId::new("blk-9"),
            AlarmKind::Black,
            severity,
            AlarmScope::Tile { index: 2 },
            MediaTime::from_nanos(1_000),
        )
    }

    #[test]
    fn subject_leads_with_severity_and_transition() {
        let env = EmailEnvelope::new("noc@multiview", "ops@example");
        let msg = EmailMessage::build(
            &env,
            AlarmTransitionKind::Raised,
            &record(PerceivedSeverity::Critical),
        );
        assert_eq!(msg.from, "noc@multiview");
        assert_eq!(msg.to, "ops@example");
        assert!(
            msg.subject.starts_with("[CRITICAL] Alarm raised"),
            "subject was {:?}",
            msg.subject
        );
        // The body carries the structured fields, including the alarm id and the
        // unacknowledged ack state.
        assert!(msg.body.contains("id: blk-9"));
        assert!(msg.body.contains("severity: CRITICAL"));
        assert!(msg.body.contains("ack: unacknowledged"));
    }

    #[test]
    fn body_reflects_acked_state() {
        let env = EmailEnvelope::new("noc@multiview", "ops@example");
        let mut rec = record(PerceivedSeverity::Major);
        rec.ack = AckState::acked("alice", MediaTime::from_nanos(2_000));
        let msg = EmailMessage::build(&env, AlarmTransitionKind::Acked, &rec);
        assert!(msg.body.contains("ack: acknowledged"));
        assert!(msg.subject.starts_with("[MAJOR] Alarm acked"));
    }

    #[cfg(feature = "email")]
    #[test]
    fn to_lettre_builds_a_sendable_message_with_valid_addresses() {
        let env = EmailEnvelope::new("noc@multiview.example", "ops@example.com");
        let msg = EmailMessage::build(
            &env,
            AlarmTransitionKind::Raised,
            &record(PerceivedSeverity::Critical),
        );
        let mime = msg.to_lettre().expect("valid addresses build a message");
        // The formatted MIME carries the rendered subject and recipient.
        let formatted = String::from_utf8(mime.formatted()).expect("ascii headers");
        assert!(formatted.contains("To: ops@example.com"));
        assert!(formatted.contains("Subject: [CRITICAL] Alarm raised"));
    }

    #[cfg(feature = "email")]
    #[test]
    fn to_lettre_rejects_a_malformed_address() {
        let env = EmailEnvelope::new("not an address", "ops@example.com");
        let msg = EmailMessage::build(
            &env,
            AlarmTransitionKind::Raised,
            &record(PerceivedSeverity::Major),
        );
        assert!(msg.to_lettre().is_err(), "a malformed From must error");
    }
}
