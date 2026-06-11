//! Golden-vector tests for the SNMP BER (ASN.1 Basic Encoding Rules) encoder.
//!
//! Gated on the `snmp` Cargo feature. The byte vectors here are hand-derived
//! from X.690 (BER) and RFC 2578 (`SMIv2` application types) so a regression in
//! tag bytes, minimal-length integer encoding, OID sub-identifier packing, or
//! the SNMPv2-Trap PDU/message framing fails loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "snmp")]

use multiview_core::alarm::PerceivedSeverity;
use multiview_telemetry::snmp::{
    encode_integer, encode_length, encode_oid, encode_trap_pdu, encode_trap_v2c_message, mib, Oid,
    Trap, TrapPdu, VarBind, VarBindValue,
};

#[test]
fn length_short_form_is_a_single_byte() {
    // X.690 §8.1.3.4: lengths < 128 use the short form (one byte == the length).
    assert_eq!(encode_length(0), vec![0x00]);
    assert_eq!(encode_length(1), vec![0x01]);
    assert_eq!(encode_length(127), vec![0x7F]);
}

#[test]
fn length_long_form_above_127() {
    // X.690 §8.1.3.5: 128..=255 -> 0x81 then one length byte.
    assert_eq!(encode_length(128), vec![0x81, 0x80]);
    assert_eq!(encode_length(255), vec![0x81, 0xFF]);
    // 256 -> 0x82 then two big-endian length bytes.
    assert_eq!(encode_length(256), vec![0x82, 0x01, 0x00]);
    assert_eq!(encode_length(300), vec![0x82, 0x01, 0x2C]);
}

#[test]
fn integer_minimal_twos_complement_encoding() {
    // X.690 §8.3: INTEGER (tag 0x02), minimal two's-complement content.
    // 0 -> 02 01 00
    assert_eq!(encode_integer(0), vec![0x02, 0x01, 0x00]);
    // 127 -> 02 01 7F
    assert_eq!(encode_integer(127), vec![0x02, 0x01, 0x7F]);
    // 128 needs a leading 0x00 so the sign bit is not set: 02 02 00 80
    assert_eq!(encode_integer(128), vec![0x02, 0x02, 0x00, 0x80]);
    // 256 -> 02 02 01 00
    assert_eq!(encode_integer(256), vec![0x02, 0x02, 0x01, 0x00]);
    // -1 -> 02 01 FF
    assert_eq!(encode_integer(-1), vec![0x02, 0x01, 0xFF]);
    // -128 -> 02 01 80
    assert_eq!(encode_integer(-128), vec![0x02, 0x01, 0x80]);
    // -129 -> 02 02 FF 7F
    assert_eq!(encode_integer(-129), vec![0x02, 0x02, 0xFF, 0x7F]);
}

#[test]
fn oid_encoding_packs_first_two_arcs_and_base128() {
    // X.690 §8.19. 1.3.6.1 -> first two arcs 1,3 combine to 40*1+3 = 43 = 0x2B,
    // then 6, then 1: 06 03 2B 06 01.
    let oid = Oid::new(vec![1, 3, 6, 1]).unwrap();
    assert_eq!(encode_oid(&oid), vec![0x06, 0x03, 0x2B, 0x06, 0x01]);
}

#[test]
fn oid_encoding_uses_base128_for_large_subids() {
    // Sub-identifier 99999 = 0x1869F. Base-128: 99999 = 6*128^2 + 12*128 + 31
    //   -> 0x06, 0x0C, 0x1F with continuation bits on all but the last:
    //      0x86 0x8D 0x1F.
    // OID 1.3.6.1.4.1.99999:
    //   43(0x2B) 6 1 4 1 [86 8D 1F]  => 8 content bytes.
    let oid = Oid::new(vec![1, 3, 6, 1, 4, 1, 99999]).unwrap();
    assert_eq!(
        encode_oid(&oid),
        vec![0x06, 0x08, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x86, 0x8D, 0x1F]
    );
}

#[test]
fn varbind_value_application_tags_are_correct() {
    // SMIv2 (RFC 2578) application tags:
    //   Counter32 = [APPLICATION 1] = 0x41, content unsigned (no sign padding).
    //   Gauge32   = [APPLICATION 2] = 0x42.
    //   TimeTicks = [APPLICATION 3] = 0x43.
    let counter = VarBindValue::Counter32 { value: 0x80 };
    // 0x80 fits in one byte for an unsigned application type (no 0x00 pad).
    assert_eq!(counter.encode_ber(), vec![0x41, 0x01, 0x80]);

    let gauge = VarBindValue::Gauge32 { value: 300 };
    // 300 = 0x012C -> two bytes, no pad.
    assert_eq!(gauge.encode_ber(), vec![0x42, 0x02, 0x01, 0x2C]);

    let ticks = VarBindValue::TimeTicks { ticks: 0 };
    assert_eq!(ticks.encode_ber(), vec![0x43, 0x01, 0x00]);
}

#[test]
fn varbind_octet_string_and_integer_encode() {
    let s = VarBindValue::OctetString {
        value: "AB".to_owned(),
    };
    // OCTET STRING tag 0x04, len 2, "AB" = 0x41 0x42.
    assert_eq!(s.encode_ber(), vec![0x04, 0x02, 0x41, 0x42]);

    let i = VarBindValue::Integer { value: -129 };
    assert_eq!(i.encode_ber(), vec![0x02, 0x02, 0xFF, 0x7F]);

    let oid = Oid::new(vec![1, 3, 6, 1]).unwrap();
    let o = VarBindValue::ObjectId { oid };
    assert_eq!(o.encode_ber(), vec![0x06, 0x03, 0x2B, 0x06, 0x01]);
}

#[test]
fn single_varbind_wraps_oid_and_value_in_a_sequence() {
    // A VarBind is SEQUENCE { name OID, value }. name 1.3.6.1 (06 03 2B 06 01),
    // value INTEGER 0 (02 01 00). Inner = 5 + 3 = 8 bytes; SEQUENCE 0x30 0x08.
    let vb = VarBind::new(
        Oid::new(vec![1, 3, 6, 1]).unwrap(),
        VarBindValue::Integer { value: 0 },
    );
    assert_eq!(
        vb.encode_ber(),
        vec![0x30, 0x08, 0x06, 0x03, 0x2B, 0x06, 0x01, 0x02, 0x01, 0x00]
    );
}

#[test]
fn trap_pdu_has_v2_trap_tag_and_carries_request_error_fields() {
    // SNMPv2-Trap-PDU is [7] = context tag 0xA7, holding:
    //   request-id INTEGER, error-status INTEGER(0), error-index INTEGER(0),
    //   variable-bindings SEQUENCE OF VarBind.
    let pdu = TrapPdu {
        request_id: 1,
        var_binds: vec![VarBind::new(
            Oid::new(vec![1, 3, 6, 1]).unwrap(),
            VarBindValue::Integer { value: 0 },
        )],
    };
    let bytes = encode_trap_pdu(&pdu);
    // Outer tag is the SNMPv2-Trap context tag.
    assert_eq!(bytes[0], 0xA7);
    // request-id (02 01 01), error-status (02 01 00), error-index (02 01 00),
    // then the var-bind list SEQUENCE (0x30 ...).
    // Skip tag+len (2 bytes) and check the leading INTEGER triple.
    assert_eq!(&bytes[2..5], &[0x02, 0x01, 0x01], "request-id");
    assert_eq!(&bytes[5..8], &[0x02, 0x01, 0x00], "error-status");
    assert_eq!(&bytes[8..11], &[0x02, 0x01, 0x00], "error-index");
    assert_eq!(bytes[11], 0x30, "var-bind list is a SEQUENCE");
}

#[test]
fn trap_v2c_message_is_a_well_formed_sequence() {
    // SNMPv2c message = SEQUENCE { version INTEGER(1), community OCTET STRING,
    //   data (the Trap-PDU) }. version 1 == "SNMPv2c".
    let trap = Trap::new(
        4242,
        Oid::new(vec![1, 3, 6, 1, 4, 1, 99999, 0, 1]).unwrap(),
        vec![],
    );
    let pdu = TrapPdu {
        request_id: 7,
        var_binds: trap.bindings.clone(),
    };
    let msg = encode_trap_v2c_message(1, "public", &pdu);
    // Top-level SEQUENCE.
    assert_eq!(msg[0], 0x30);
    // version INTEGER 1 follows the SEQUENCE header (tag+len).
    assert_eq!(&msg[2..5], &[0x02, 0x01, 0x01], "version == SNMPv2c(1)");
    // community OCTET STRING "public" (6 bytes).
    assert_eq!(&msg[5..7], &[0x04, 0x06], "community OCTET STRING len 6");
    assert_eq!(&msg[7..13], b"public");
    // The Trap-PDU then begins with its context tag 0xA7.
    assert_eq!(msg[13], 0xA7, "embedded SNMPv2-Trap-PDU");
}

#[test]
fn mib_oids_hang_off_the_multiview_enterprise_arc() {
    // The notification OIDs and object OIDs must descend from the Multiview
    // enterprise arc (1.3.6.1.4.1.<pen>). The exact PEN is a placeholder until
    // registered, but the structure is pinned.
    let alarm_raise = mib::alarm_raise_oid();
    let alarm_clear = mib::alarm_clear_oid();
    let sev = mib::severity_object_oid();
    let pen = mib::enterprise_oid();
    assert!(
        alarm_raise.arc().starts_with(pen.arc()),
        "raise OID must descend from the enterprise arc"
    );
    assert!(alarm_clear.arc().starts_with(pen.arc()));
    assert!(sev.arc().starts_with(pen.arc()));
    // Raise and clear are distinct notification types.
    assert_ne!(alarm_raise, alarm_clear);
}

#[test]
fn raise_trap_carries_severity_and_clear_trap_uses_clear_oid() {
    use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope};
    use multiview_core::time::MediaTime;

    let record = AlarmRecord::new(
        AlarmId::new("probe-7"),
        AlarmKind::Black,
        PerceivedSeverity::Major,
        AlarmScope::Tile { index: 3 },
        MediaTime::ZERO,
    );

    // A raise trap names the raise notification OID and binds the live severity.
    let raise = Trap::from_alarm(&record, 1234);
    assert_eq!(raise.trap_oid, mib::alarm_raise_oid());
    // It must bind the severity index (Major == 5) somewhere in its var-binds.
    let has_major = raise.bindings.iter().any(|b| {
        b.oid == mib::severity_object_oid() && matches!(b.value, VarBindValue::Integer { value: 5 })
    });
    assert!(has_major, "raise trap must bind the X.733 severity index");

    // A clear trap names the clear notification OID and binds severity 1 (clear).
    let mut cleared = record.clone();
    cleared.severity = PerceivedSeverity::Cleared;
    let clear = Trap::from_alarm(&cleared, 1234);
    assert_eq!(clear.trap_oid, mib::alarm_clear_oid());
    let has_clear = clear.bindings.iter().any(|b| {
        b.oid == mib::severity_object_oid() && matches!(b.value, VarBindValue::Integer { value: 1 })
    });
    assert!(has_clear, "clear trap must bind the clear severity index");
}
