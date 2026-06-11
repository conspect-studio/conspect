//! Golden-vector and severity-mapping tests for the RFC 5424 syslog formatter.
//!
//! The wire format is pinned against worked examples derived from RFC 5424 §6.5
//! so a regression in the header layout, PRI arithmetic, structured-data
//! escaping, or NIL handling fails loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::PerceivedSeverity;
use multiview_telemetry::syslog::{Facility, SdElement, Severity, SyslogMessage};

#[test]
fn pri_is_facility_times_eight_plus_severity() {
    // RFC 5424 §6.2.1: PRIVAL = facility * 8 + severity.
    // local0 (16) * 8 + Warning (4) = 132.
    assert_eq!(Facility::Local0.code(), 16);
    assert_eq!(Severity::Warning.code(), 4);
    assert_eq!(Facility::Local0.prival(Severity::Warning), 132);

    // kernel (0) * 8 + Emergency (0) = 0  (the documented minimum).
    assert_eq!(Facility::Kernel.prival(Severity::Emergency), 0);
    // local7 (23) * 8 + Debug (7) = 191 (the documented maximum).
    assert_eq!(Facility::Local7.prival(Severity::Debug), 191);
}

#[test]
fn severity_codes_match_rfc5424_numerical_scale() {
    // RFC 5424 §6.2.1 Table 2.
    assert_eq!(Severity::Emergency.code(), 0);
    assert_eq!(Severity::Alert.code(), 1);
    assert_eq!(Severity::Critical.code(), 2);
    assert_eq!(Severity::Error.code(), 3);
    assert_eq!(Severity::Warning.code(), 4);
    assert_eq!(Severity::Notice.code(), 5);
    assert_eq!(Severity::Informational.code(), 6);
    assert_eq!(Severity::Debug.code(), 7);
}

#[test]
fn alarm_severity_maps_to_syslog_severity() {
    // X.733 perceived severity -> RFC 5424 severity. Critical faults page as
    // Critical(2); Major as Error(3); Minor as Warning(4); Warning as Notice(5);
    // Indeterminate as Informational(6); a Cleared (no-alarm) condition is an
    // informational clear, NOT an active fault.
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Critical),
        Severity::Critical
    );
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Major),
        Severity::Error
    );
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Minor),
        Severity::Warning
    );
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Warning),
        Severity::Notice
    );
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Indeterminate),
        Severity::Informational
    );
    assert_eq!(
        Severity::from_alarm(PerceivedSeverity::Cleared),
        Severity::Informational
    );
}

#[test]
fn alarm_severity_mapping_is_monotonic() {
    // A worse X.733 severity must never map to a *less* urgent (numerically
    // higher) syslog severity. Syslog codes are inverted (lower = worse), so a
    // higher PerceivedSeverity must map to a <= syslog code.
    let ladder = [
        PerceivedSeverity::Cleared,
        PerceivedSeverity::Indeterminate,
        PerceivedSeverity::Warning,
        PerceivedSeverity::Minor,
        PerceivedSeverity::Major,
        PerceivedSeverity::Critical,
    ];
    for window in ladder.windows(2) {
        let lower = Severity::from_alarm(window[0]).code();
        let higher = Severity::from_alarm(window[1]).code();
        assert!(
            higher <= lower,
            "{:?}->{higher} must be at least as urgent as {:?}->{lower}",
            window[1],
            window[0],
        );
    }
}

#[test]
fn golden_full_message_with_structured_data() {
    // Adapted from RFC 5424 §6.5 Example 3 (timestamp/host/app/proc/msgid/SD).
    // PRI = local4(20)*8 + Notice(5) = 165.
    let msg = SyslogMessage::new(Facility::Local4, Severity::Notice)
        .timestamp("2003-10-11T22:14:15.003Z")
        .hostname("mymachine.example.com")
        .app_name("evntslog")
        .proc_id("8710")
        .msg_id("ID47")
        .structured_data(vec![SdElement::new("exampleSDID@32473")
            .param("iut", "3")
            .param("eventSource", "Application")
            .param("eventID", "1011")])
        .message("An application event log entry...");

    assert_eq!(
        msg.to_rfc5424(),
        "<165>1 2003-10-11T22:14:15.003Z mymachine.example.com evntslog 8710 ID47 \
         [exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"] \
         An application event log entry..."
    );
}

#[test]
fn golden_message_with_nil_fields_and_no_structured_data() {
    // All optional header fields absent => NILVALUE "-"; no SD => "-".
    // PRI = local0(16)*8 + Critical(2) = 130.
    let msg = SyslogMessage::new(Facility::Local0, Severity::Critical);
    assert_eq!(msg.to_rfc5424(), "<130>1 - - - - - -");
}

#[test]
fn golden_message_no_sd_but_with_msg() {
    // No structured data => "-" placeholder, then SP, then the message.
    let msg = SyslogMessage::new(Facility::Daemon, Severity::Error)
        .timestamp("2026-06-03T00:00:00Z")
        .hostname("multiview")
        .app_name("multiview")
        .proc_id("1")
        .msg_id("BLACK")
        .message("tile 3 black for 4500ms");
    // daemon(3)*8 + Error(3) = 27.
    assert_eq!(
        msg.to_rfc5424(),
        "<27>1 2026-06-03T00:00:00Z multiview multiview 1 BLACK - tile 3 black for 4500ms"
    );
}

#[test]
fn structured_data_param_values_are_escaped() {
    // RFC 5424 §6.3.3: within a PARAM-VALUE, the characters '"', '\' and ']'
    // MUST be escaped with a preceding backslash.
    let sd = SdElement::new("ctx@1").param("path", r#"a"b\c]d"#);
    let msg =
        SyslogMessage::new(Facility::Local0, Severity::Informational).structured_data(vec![sd]);
    assert_eq!(
        msg.to_rfc5424(),
        r#"<134>1 - - - - - [ctx@1 path="a\"b\\c\]d"]"#
    );
}

#[test]
fn multiple_structured_data_elements_concatenate_without_separator() {
    // RFC 5424 §6.3: multiple SD-ELEMENTs are concatenated with no separator.
    let msg = SyslogMessage::new(Facility::Local0, Severity::Informational).structured_data(vec![
        SdElement::new("a@1").param("k", "v"),
        SdElement::new("b@2"),
    ]);
    assert_eq!(msg.to_rfc5424(), r#"<134>1 - - - - - [a@1 k="v"][b@2]"#);
}

#[test]
fn empty_string_optional_field_is_treated_as_nil() {
    // A caller passing "" for an optional header field yields the NILVALUE.
    let msg = SyslogMessage::new(Facility::Local0, Severity::Informational)
        .hostname("")
        .app_name("");
    assert_eq!(msg.to_rfc5424(), "<134>1 - - - - - -");
}

/// IPv6-first transport: a `UdpSender` pointed at an **IPv6** loopback collector
/// must bind an IPv6 ephemeral socket and deliver the datagram. The previous
/// hard-coded `0.0.0.0:0` (IPv4) bind could not reach an `[::1]` collector, so
/// this would have failed before the fix.
#[cfg(feature = "syslog")]
#[test]
fn udp_sender_reaches_an_ipv6_collector() {
    use multiview_telemetry::syslog::UdpSender;
    use std::net::{Ipv6Addr, UdpSocket};

    // A real IPv6 loopback receiver on an OS-assigned port.
    let receiver = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).expect("bind [::1]:0 receiver");
    let collector = receiver.local_addr().expect("receiver local addr");
    assert!(collector.is_ipv6(), "receiver must be IPv6 loopback");

    let sender = UdpSender::connect(collector).expect("connect to [::1] collector");
    let msg = SyslogMessage::new(Facility::Local0, Severity::Informational).message("ipv6-ok");
    sender.send(&msg).expect("send over IPv6");

    let mut buf = [0u8; 256];
    let (n, from) = receiver.recv_from(&mut buf).expect("recv the datagram");
    assert!(from.is_ipv6(), "datagram source must be IPv6, got {from}");
    let wire = std::str::from_utf8(&buf[..n]).expect("utf-8 syslog wire");
    assert_eq!(wire, msg.to_rfc5424());
}
