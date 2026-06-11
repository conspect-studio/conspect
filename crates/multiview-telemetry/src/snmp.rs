//! SNMP trap path: enterprise MIB/OID model, BER encoding, and trap delivery
//! (`snmp` feature).
//!
//! Broadcast NMS integrations expect alarms as SNMP traps/notifications keyed by
//! an enterprise MIB. This module lives behind the off-by-default `snmp` Cargo
//! feature and is split into a **pure, golden-tested** core and a thin,
//! compile-only transport:
//!
//! * The object-identifier model ([`Oid`]), the SNMP value subset Multiview emits
//!   ([`VarBindValue`]), a single `OID = value` binding ([`VarBind`]), and the
//!   notification payload ([`Trap`]) / wire PDU ([`TrapPdu`]).
//! * The Multiview enterprise MIB OIDs in [`mib`] (the notification and object
//!   identifiers every emitted trap is keyed by).
//! * A **pure** ASN.1 BER (X.690) encoder — [`encode_length`], [`encode_integer`],
//!   [`encode_oid`], [`VarBindValue::encode_ber`], [`VarBind::encode_ber`],
//!   [`encode_trap_pdu`], and the `SNMPv2c` message wrapper
//!   [`encode_trap_v2c_message`] — so the exact bytes an NMS receives are
//!   golden-vector tested with no I/O.
//! * [`Trap::from_alarm`], which maps a `core::alarm::AlarmRecord` (X.733) onto
//!   the raise/clear notification OID plus the standard severity / scope /
//!   description var-binds.
//!
//! The actual UDP datagram send is **gated** behind the `transport` submodule
//! (still under the `snmp` feature): it uses `std::net` only, pulls in no native
//! deps, and keeps `unsafe_code` forbidden. Sending is best-effort and is owned
//! by the telemetry/control plane — it must never back-pressure the engine
//! (invariant #10).
//!
//! Severity is carried as an `SMIv2` integer mirroring the X.733 scale via
//! [`severity_index`], so an NMS can threshold/colour traps consistently with
//! [`crate::syslog`] and the alarm roll-up.
use multiview_core::alarm::{AlarmRecord, PerceivedSeverity};
use serde::{Deserialize, Serialize};

/// The IANA Private Enterprise root arc, `1.3.6.1.4.1` (`iso.org.dod.internet.
/// private.enterprises`). Concrete object OIDs hang off a registered enterprise
/// number below this arc.
pub const ENTERPRISES_ARC: &[u32] = &[1, 3, 6, 1, 4, 1];

/// An SNMP object identifier: a non-empty sequence of unsigned sub-identifiers.
///
/// Stored as the decoded arc (e.g. `1.3.6.1.4.1`), not the dotted string, so it
/// can be compared and extended structurally. Serialised as the sub-identifier
/// list (tagged per repo conventions is moot — this is a transparent newtype).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Oid(Vec<u32>);

impl Oid {
    /// Construct an OID from its sub-identifier arc.
    ///
    /// # Errors
    ///
    /// Returns [`SnmpError::EmptyOid`] if `arc` is empty — a valid OID has at
    /// least one sub-identifier.
    pub fn new(arc: impl Into<Vec<u32>>) -> Result<Self, SnmpError> {
        let arc = arc.into();
        if arc.is_empty() {
            return Err(SnmpError::EmptyOid);
        }
        Ok(Self(arc))
    }

    /// Borrow the sub-identifier arc.
    #[must_use]
    pub fn arc(&self) -> &[u32] {
        &self.0
    }

    /// Build a child OID by appending sub-identifiers to this one.
    #[must_use]
    pub fn child(&self, sub: impl IntoIterator<Item = u32>) -> Self {
        let mut arc = self.0.clone();
        arc.extend(sub);
        Self(arc)
    }

    /// The bare IANA enterprise arc (`1.3.6.1.4.1`) as an infallible fallback.
    ///
    /// Used only by the [`mib`] constructors so they can stay panic-free even on
    /// the impossible empty-arc path; the arcs they pass are always non-empty.
    pub(crate) fn fallback_enterprise() -> Self {
        Self(ENTERPRISES_ARC.to_vec())
    }

    /// Render the OID in conventional dotted-decimal notation
    /// (e.g. `"1.3.6.1.4.1"`).
    #[must_use]
    pub fn to_dotted(&self) -> String {
        // No `Vec` indexing / `as` conversions: map each sub-id to its decimal
        // string and join with '.'.
        self.0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// The subset of SNMP/`SMIv2` value types Multiview binds in a trap.
///
/// Serialised **tagged** (`#[serde(tag = "type")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so further `SMIv2` types can be added without a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum VarBindValue {
    /// `INTEGER` (`SMIv2` `Integer32`).
    Integer {
        /// The signed 32-bit value.
        value: i32,
    },
    /// `Counter32` — a non-negative monotonically increasing counter.
    Counter32 {
        /// The counter value.
        value: u32,
    },
    /// `Gauge32` / `Unsigned32`.
    Gauge32 {
        /// The gauge value.
        value: u32,
    },
    /// `TimeTicks` in hundredths of a second.
    TimeTicks {
        /// Hundredths of a second since the relevant epoch.
        ticks: u32,
    },
    /// `OCTET STRING` carrying UTF-8 text.
    OctetString {
        /// The string payload.
        value: String,
    },
    /// `OBJECT IDENTIFIER`.
    ObjectId {
        /// The referenced OID.
        oid: Oid,
    },
}

impl VarBindValue {
    /// Encode this value as a single ASN.1 BER TLV (X.690), using the `SMIv2`
    /// application tags for the SNMP-specific types (RFC 2578).
    ///
    /// * `Integer` -> universal `INTEGER` (`0x02`), minimal two's-complement.
    /// * `Counter32` -> `[APPLICATION 1]` (`0x41`), unsigned minimal bytes.
    /// * `Gauge32` -> `[APPLICATION 2]` (`0x42`), unsigned minimal bytes.
    /// * `TimeTicks` -> `[APPLICATION 3]` (`0x43`), unsigned minimal bytes.
    /// * `OctetString` -> universal `OCTET STRING` (`0x04`), raw UTF-8 bytes.
    /// * `ObjectId` -> universal `OBJECT IDENTIFIER` (`0x06`).
    #[must_use]
    pub fn encode_ber(&self) -> Vec<u8> {
        match self {
            Self::Integer { value } => encode_integer(i64::from(*value)),
            Self::Counter32 { value } => encode_unsigned_app(TAG_COUNTER32, *value),
            Self::Gauge32 { value } => encode_unsigned_app(TAG_GAUGE32, *value),
            Self::TimeTicks { ticks } => encode_unsigned_app(TAG_TIMETICKS, *ticks),
            Self::OctetString { value } => encode_tlv(TAG_OCTET_STRING, value.as_bytes()),
            Self::ObjectId { oid } => encode_oid(oid),
        }
    }
}

/// One SNMP variable binding: an object [`Oid`] paired with its [`VarBindValue`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarBind {
    /// The bound object identifier.
    pub oid: Oid,
    /// The bound value.
    pub value: VarBindValue,
}

impl VarBind {
    /// Construct a variable binding.
    #[must_use]
    pub fn new(oid: Oid, value: VarBindValue) -> Self {
        Self { oid, value }
    }

    /// Encode this binding as the ASN.1 BER `SEQUENCE { name OID, value }`
    /// (X.690 / RFC 3416 `VarBind`).
    #[must_use]
    pub fn encode_ber(&self) -> Vec<u8> {
        let mut content = encode_oid(&self.oid);
        content.extend(self.value.encode_ber());
        encode_tlv(TAG_SEQUENCE, &content)
    }
}

/// An SNMPv2c-style trap / notification payload (pure value type).
///
/// Carries the mandatory `sysUpTime.0` ticks, the notification's
/// `snmpTrapOID.0` ([`trap_oid`](Trap::trap_oid)), and the object
/// [`bindings`](Trap::bindings). BER encoding and transport are deferred to a
/// later iteration behind the `snmp` feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trap {
    /// `sysUpTime.0` value, in hundredths of a second.
    pub sys_up_time: u32,
    /// The `snmpTrapOID.0` identifying the notification type.
    pub trap_oid: Oid,
    /// The variable bindings carried by the notification.
    pub bindings: Vec<VarBind>,
}

impl Trap {
    /// Construct a trap from its uptime, trap OID, and bindings.
    #[must_use]
    pub fn new(sys_up_time: u32, trap_oid: Oid, bindings: Vec<VarBind>) -> Self {
        Self {
            sys_up_time,
            trap_oid,
            bindings,
        }
    }

    /// Build a raise/clear notification from a `core::alarm::AlarmRecord`.
    ///
    /// The notification OID is [`mib::alarm_clear_oid`] when the record's
    /// severity is [`PerceivedSeverity::Cleared`] (the X.733 clear) and
    /// [`mib::alarm_raise_oid`] for any active severity, so an NMS can
    /// raise/clear-correlate without inspecting the payload. The standard alarm
    /// object var-binds are bound in MIB order: severity index, the probe kind,
    /// the scope, the alarm id, and the dwell (milliseconds) as a `TimeTicks`-
    /// adjacent gauge.
    ///
    /// `sys_up_time` is the agent's `sysUpTime.0` in hundredths of a second at
    /// emission; it is the responsibility of the caller (the telemetry plane),
    /// never the engine hot path, to supply it.
    #[must_use]
    pub fn from_alarm(record: &AlarmRecord, sys_up_time: u32) -> Self {
        let trap_oid = if record.severity.is_active() {
            mib::alarm_raise_oid()
        } else {
            mib::alarm_clear_oid()
        };
        let bindings = vec![
            VarBind::new(
                mib::severity_object_oid(),
                VarBindValue::Integer {
                    value: severity_index(record.severity),
                },
            ),
            VarBind::new(
                mib::kind_object_oid(),
                VarBindValue::OctetString {
                    value: alarm_kind_label(record.kind).to_owned(),
                },
            ),
            VarBind::new(
                mib::scope_object_oid(),
                VarBindValue::OctetString {
                    value: scope_label(&record.scope),
                },
            ),
            VarBind::new(
                mib::alarm_id_object_oid(),
                VarBindValue::OctetString {
                    value: record.id.as_str().to_owned(),
                },
            ),
            VarBind::new(
                mib::dwell_object_oid(),
                VarBindValue::Gauge32 {
                    value: dwell_millis(record),
                },
            ),
        ];
        Self::new(sys_up_time, trap_oid, bindings)
    }
}

/// A render-ready SNMPv2-Trap PDU: the request id plus the variable bindings.
///
/// The `error-status` and `error-index` fields mandated by RFC 3416 are always
/// zero for a notification, so they are implied by [`encode_trap_pdu`] rather
/// than stored. The two mandatory leading var-binds (`sysUpTime.0` and
/// `snmpTrapOID.0`) are also the caller's to prepend via [`Trap::to_pdu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrapPdu {
    /// The SNMP `request-id` correlating this notification.
    pub request_id: i32,
    /// The ordered variable bindings carried by the notification.
    pub var_binds: Vec<VarBind>,
}

impl Trap {
    /// Project this trap into a wire [`TrapPdu`], prepending the two mandatory
    /// var-binds RFC 3416 requires of every `SNMPv2` notification:
    /// `sysUpTime.0` (`TimeTicks`) and `snmpTrapOID.0` (the notification OID).
    #[must_use]
    pub fn to_pdu(&self, request_id: i32) -> TrapPdu {
        let mut var_binds = Vec::with_capacity(self.bindings.len() + 2);
        var_binds.push(VarBind::new(
            mib::sys_up_time_oid(),
            VarBindValue::TimeTicks {
                ticks: self.sys_up_time,
            },
        ));
        var_binds.push(VarBind::new(
            mib::snmp_trap_oid(),
            VarBindValue::ObjectId {
                oid: self.trap_oid.clone(),
            },
        ));
        var_binds.extend(self.bindings.iter().cloned());
        TrapPdu {
            request_id,
            var_binds,
        }
    }
}

/// The Multiview enterprise SNMP MIB: the object/notification OIDs every emitted
/// trap is keyed by.
///
/// The Private Enterprise Number ([`mib::PEN`]) is a **placeholder** (`99999`)
/// until a number is registered with IANA; the *structure* below is the pinned
/// contract. All object OIDs descend from [`mib::enterprise_oid`] so an NMS can
/// load a single MIB module.
pub mod mib {
    use super::{Oid, ENTERPRISES_ARC};

    /// Placeholder IANA Private Enterprise Number for Multiview.
    ///
    /// Replace with the registered PEN before shipping a published MIB; the OID
    /// structure layered above it does not change.
    pub const PEN: u32 = 99999;

    /// Sub-arc of the enterprise root holding alarm objects (`multiviewAlarm`).
    const ALARM_GROUP: u32 = 1;
    /// Sub-arc holding notification (trap) types (`multiviewNotifications`).
    const NOTIF_GROUP: u32 = 0;

    /// Internal helper: build a Multiview OID, falling back to the bare enterprise
    /// arc if (impossibly) the constant arc were empty. The `ENTERPRISES_ARC`
    /// constant is non-empty, so [`Oid::new`] never actually errors here; we
    /// avoid `unwrap`/`expect` regardless (telemetry must not panic).
    fn multiview_oid(tail: &[u32]) -> Oid {
        let mut arc = ENTERPRISES_ARC.to_vec();
        arc.push(PEN);
        arc.extend_from_slice(tail);
        Oid::new(arc).unwrap_or_else(|_| Oid::fallback_enterprise())
    }

    /// The Multiview enterprise root OID (`1.3.6.1.4.1.<PEN>`).
    #[must_use]
    pub fn enterprise_oid() -> Oid {
        multiview_oid(&[])
    }

    /// `multiviewAlarmRaise` notification OID.
    #[must_use]
    pub fn alarm_raise_oid() -> Oid {
        multiview_oid(&[NOTIF_GROUP, 1])
    }

    /// `multiviewAlarmClear` notification OID.
    #[must_use]
    pub fn alarm_clear_oid() -> Oid {
        multiview_oid(&[NOTIF_GROUP, 2])
    }

    /// `multiviewAlarmSeverity` object OID (the X.733 severity index).
    #[must_use]
    pub fn severity_object_oid() -> Oid {
        multiview_oid(&[ALARM_GROUP, 1])
    }

    /// `multiviewAlarmKind` object OID (the probe/fault class label).
    #[must_use]
    pub fn kind_object_oid() -> Oid {
        multiview_oid(&[ALARM_GROUP, 2])
    }

    /// `multiviewAlarmScope` object OID (a human-readable scope label).
    #[must_use]
    pub fn scope_object_oid() -> Oid {
        multiview_oid(&[ALARM_GROUP, 3])
    }

    /// `multiviewAlarmId` object OID (the stable alarm instance id).
    #[must_use]
    pub fn alarm_id_object_oid() -> Oid {
        multiview_oid(&[ALARM_GROUP, 4])
    }

    /// `multiviewAlarmDwellMillis` object OID (how long the condition has dwelt).
    #[must_use]
    pub fn dwell_object_oid() -> Oid {
        multiview_oid(&[ALARM_GROUP, 5])
    }

    /// The standard `sysUpTime.0` OID (`1.3.6.1.2.1.1.3.0`, SNMPv2-MIB).
    #[must_use]
    pub fn sys_up_time_oid() -> Oid {
        Oid::new(vec![1, 3, 6, 1, 2, 1, 1, 3, 0]).unwrap_or_else(|_| Oid::fallback_enterprise())
    }

    /// The standard `snmpTrapOID.0` OID (`1.3.6.1.6.3.1.1.4.1.0`, SNMPv2-MIB).
    #[must_use]
    pub fn snmp_trap_oid() -> Oid {
        Oid::new(vec![1, 3, 6, 1, 6, 3, 1, 1, 4, 1, 0])
            .unwrap_or_else(|_| Oid::fallback_enterprise())
    }
}

/// ASN.1 universal/application/context BER tag bytes (X.690 / RFC 2578, 3416).
const TAG_INTEGER: u8 = 0x02;
/// `OCTET STRING` universal tag.
const TAG_OCTET_STRING: u8 = 0x04;
/// `OBJECT IDENTIFIER` universal tag.
const TAG_OID: u8 = 0x06;
/// Constructed `SEQUENCE` / `SEQUENCE OF` tag.
const TAG_SEQUENCE: u8 = 0x30;
/// `SMIv2` `Counter32` (`[APPLICATION 1]`, primitive).
const TAG_COUNTER32: u8 = 0x41;
/// `SMIv2` `Gauge32` / `Unsigned32` (`[APPLICATION 2]`, primitive).
const TAG_GAUGE32: u8 = 0x42;
/// `SMIv2` `TimeTicks` (`[APPLICATION 3]`, primitive).
const TAG_TIMETICKS: u8 = 0x43;
/// Context tag `[7]` (constructed) identifying an `SNMPv2-Trap-PDU` (RFC 3416).
const TAG_TRAP_PDU: u8 = 0xA7;

/// Encode an ASN.1 BER length (X.690 §8.1.3): short form for `0..=127`, else the
/// long form `0x8N` followed by `N` big-endian length bytes.
#[must_use]
pub fn encode_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        // Short form: a single byte holds the length directly. `len < 128` fits
        // a `u8` without a lossy conversion.
        return vec![u8::try_from(len).unwrap_or(0)];
    }
    // Long form: strip leading zero bytes from the big-endian length.
    let raw = len.to_be_bytes();
    let first_significant = raw.iter().position(|&b| b != 0).unwrap_or(raw.len());
    let significant = raw.get(first_significant..).unwrap_or(&[]);
    let count = significant.len();
    // `count` is at most size_of::<usize>() (<= 8), well under 0x7F.
    let mut out = Vec::with_capacity(count + 1);
    out.push(0x80 | u8::try_from(count).unwrap_or(0));
    out.extend_from_slice(significant);
    out
}

/// Wrap `content` in a BER TLV with the given primitive/constructed `tag`.
fn encode_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 2);
    out.push(tag);
    out.extend(encode_length(content.len()));
    out.extend_from_slice(content);
    out
}

/// Encode a signed `INTEGER` (X.690 §8.3) with minimal two's-complement content.
#[must_use]
pub fn encode_integer(value: i64) -> Vec<u8> {
    encode_tlv(TAG_INTEGER, &minimal_signed(value))
}

/// Encode an unsigned `SMIv2` application integer (`Counter32`/`Gauge32`/
/// `TimeTicks`) with minimal content (no sign-bit padding — these are unsigned).
fn encode_unsigned_app(tag: u8, value: u32) -> Vec<u8> {
    encode_tlv(tag, &minimal_unsigned(value))
}

/// The minimal big-endian two's-complement byte representation of `value`,
/// per X.690 §8.3.2 (no redundant leading `0x00`/`0xFF` octets, but a single
/// leading pad octet is kept when needed to preserve the sign bit).
fn minimal_signed(value: i64) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();
    // Trim a leading byte while the top two bytes are a redundant sign extension
    // (0x00 followed by a clear high bit, or 0xFF followed by a set high bit),
    // never emptying the vector.
    while bytes.len() > 1 {
        let first = bytes.first().copied().unwrap_or(0);
        let second = bytes.get(1).copied().unwrap_or(0);
        let redundant_zero = first == 0x00 && (second & 0x80) == 0;
        let redundant_ones = first == 0xFF && (second & 0x80) != 0;
        if redundant_zero || redundant_ones {
            bytes.remove(0);
        } else {
            break;
        }
    }
    bytes
}

/// The minimal big-endian byte representation of an unsigned value (no leading
/// `0x00` padding; the application tag already conveys "unsigned").
fn minimal_unsigned(value: u32) -> Vec<u8> {
    if value == 0 {
        return vec![0x00];
    }
    let raw = value.to_be_bytes();
    let first = raw.iter().position(|&b| b != 0).unwrap_or(raw.len());
    raw.get(first..).unwrap_or(&[]).to_vec()
}

/// Encode an [`Oid`] as a BER `OBJECT IDENTIFIER` (X.690 §8.19): the first two
/// sub-identifiers are packed as `40 * first + second`, and every value is then
/// emitted base-128, most-significant group first, with the continuation bit set
/// on all but the final octet of each sub-identifier.
#[must_use]
pub fn encode_oid(oid: &Oid) -> Vec<u8> {
    let arc = oid.arc();
    let mut content = Vec::new();
    let first = arc.first().copied().unwrap_or(0);
    let mut iter = arc.iter().skip(1);
    match iter.next() {
        Some(&second) => {
            // X.690 §8.19.4: combine the first two arcs. Both are small (<= 6
            // for the first, < 40 for the second per the standard), so the sum
            // never overflows u32.
            let combined = first.saturating_mul(40).saturating_add(second);
            push_base128(combined, &mut content);
            for &sub in iter {
                push_base128(sub, &mut content);
            }
        }
        None => {
            // A degenerate single-arc OID: emit it directly (cannot happen for a
            // well-formed OID, which always has >= 2 arcs, but stay total).
            push_base128(first, &mut content);
        }
    }
    encode_tlv(TAG_OID, &content)
}

/// Append `value` to `out` in base-128, high-order group first, with the
/// continuation bit (`0x80`) set on every octet except the last.
fn push_base128(value: u32, out: &mut Vec<u8>) {
    // Collect the 7-bit groups least-significant first, then reverse.
    let mut groups = [0_u8; 5];
    let mut len = 0_usize;
    let mut remaining = value;
    loop {
        // The low 7 bits; `& 0x7F` keeps this in 0..=127 so the `as`-free cast
        // via `try_from` cannot fail.
        let group = u8::try_from(remaining & 0x7F).unwrap_or(0);
        if let Some(slot) = groups.get_mut(len) {
            *slot = group;
        }
        len = len.saturating_add(1);
        remaining >>= 7;
        if remaining == 0 {
            break;
        }
    }
    // Emit most-significant group first, setting the continuation bit on all but
    // the final (least-significant) group.
    for i in (0..len).rev() {
        let group = groups.get(i).copied().unwrap_or(0);
        if i == 0 {
            out.push(group);
        } else {
            out.push(group | 0x80);
        }
    }
}

/// Encode a [`TrapPdu`] as the BER `SNMPv2-Trap-PDU` (RFC 3416): a context-`[7]`
/// constructed value holding `request-id`, `error-status` (0), `error-index`
/// (0), and the `variable-bindings` `SEQUENCE OF VarBind`.
#[must_use]
pub fn encode_trap_pdu(pdu: &TrapPdu) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(encode_integer(i64::from(pdu.request_id)));
    body.extend(encode_integer(0)); // error-status: noError
    body.extend(encode_integer(0)); // error-index
    let mut vb_list = Vec::new();
    for vb in &pdu.var_binds {
        vb_list.extend(vb.encode_ber());
    }
    body.extend(encode_tlv(TAG_SEQUENCE, &vb_list));
    encode_tlv(TAG_TRAP_PDU, &body)
}

/// Encode a complete `SNMPv2c` trap **message** (RFC 3416 §1, RFC 1901): a
/// top-level `SEQUENCE { version INTEGER, community OCTET STRING, data PDU }`.
/// `version` is the SNMP message version (`1` denotes `SNMPv2c`).
#[must_use]
pub fn encode_trap_v2c_message(version: i32, community: &str, pdu: &TrapPdu) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(encode_integer(i64::from(version)));
    body.extend(encode_tlv(TAG_OCTET_STRING, community.as_bytes()));
    body.extend(encode_trap_pdu(pdu));
    encode_tlv(TAG_SEQUENCE, &body)
}

/// A stable, lower-snake label for an [`AlarmKind`](multiview_core::alarm::AlarmKind),
/// bound into the `multiviewAlarmKind` var-bind. Matches the serde tag so the SNMP
/// and JSON/event surfaces agree.
fn alarm_kind_label(kind: multiview_core::alarm::AlarmKind) -> &'static str {
    use multiview_core::alarm::AlarmKind;
    match kind {
        AlarmKind::Black => "black",
        AlarmKind::Freeze => "freeze",
        AlarmKind::Silence => "silence",
        AlarmKind::OverLevel => "over_level",
        AlarmKind::Clip => "clip",
        AlarmKind::PhaseInvert => "phase_invert",
        AlarmKind::LoudnessViolation => "loudness_violation",
        AlarmKind::CaptionLoss => "caption_loss",
        AlarmKind::FormatMismatch => "format_mismatch",
        AlarmKind::SignalLoss => "signal_loss",
        // `AlarmKind` is `#[non_exhaustive]`: a future probe class maps to a
        // safe placeholder rather than failing to build.
        _ => "unknown",
    }
}

/// A human-readable scope label bound into the `multiviewAlarmScope` var-bind.
fn scope_label(scope: &multiview_core::alarm::AlarmScope) -> String {
    use multiview_core::alarm::AlarmScope;
    match scope {
        AlarmScope::Probe { id } => format!("probe:{id}"),
        AlarmScope::Tile { index } => format!("tile:{index}"),
        AlarmScope::Group { name } => format!("group:{name}"),
        AlarmScope::System => "system".to_owned(),
        // `AlarmScope` is `#[non_exhaustive]`; keep total.
        _ => "unknown".to_owned(),
    }
}

/// The dwell of an alarm in whole milliseconds, saturated into a `Gauge32`.
fn dwell_millis(record: &AlarmRecord) -> u32 {
    // `MediaTime` carries i64 nanoseconds; convert to ms and clamp to the
    // Gauge32 range without a lossy `as` cast.
    let nanos = record.dwell.as_nanos();
    let millis = nanos / 1_000_000;
    u32::try_from(millis.max(0)).unwrap_or(u32::MAX)
}

/// Map an X.733 [`PerceivedSeverity`] to the `SMIv2` integer convention used by
/// common alarm MIBs (`1 = clear`, ascending to `6 = critical`).
///
/// This mirrors the X.733 total order so a trap's severity var-bind sorts the
/// same way as the in-process roll-up.
#[must_use]
pub const fn severity_index(severity: PerceivedSeverity) -> i32 {
    match severity {
        PerceivedSeverity::Cleared => 1,
        PerceivedSeverity::Warning => 3,
        PerceivedSeverity::Minor => 4,
        PerceivedSeverity::Major => 5,
        PerceivedSeverity::Critical => 6,
        // `Indeterminate` is index 2; the same arm catches any future
        // `#[non_exhaustive]` severity, mapping it to `indeterminate` rather than
        // to a clear or a critical.
        _ => 2,
    }
}

/// Errors raised while constructing or delivering SNMP types.
///
/// `#[non_exhaustive]` so new variants can be added without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnmpError {
    /// An [`Oid`] was constructed from an empty sub-identifier arc.
    #[error("an OID must have at least one sub-identifier")]
    EmptyOid,

    /// A trap datagram could not be delivered to its NMS collector.
    ///
    /// Delivery is best-effort and this error is surfaced to the telemetry /
    /// control plane only; it never reaches the engine data plane (invariant
    /// #10).
    #[error("snmp trap transport error: {0}")]
    Transport(String),
}

/// Best-effort UDP delivery of encoded `SNMPv2c` traps to an NMS collector.
///
/// Gated behind the `snmp` feature (the whole module is). Uses `std::net` only —
/// no native deps, `unsafe_code` stays forbidden. The sender is owned by the
/// telemetry/control plane; it must never be invoked from the engine's protected
/// output path (invariant #10), and the standard SNMP trap port is `162/udp`.
#[derive(Debug)]
pub struct TrapSender {
    socket: std::net::UdpSocket,
    community: String,
    request_id: std::sync::atomic::AtomicI32,
}

impl TrapSender {
    /// The IANA-assigned SNMP trap port.
    pub const TRAP_PORT: u16 = 162;

    /// Bind an ephemeral local UDP socket and connect it to `collector`.
    ///
    /// `community` is the `SNMPv2c` community string sent with every trap.
    ///
    /// IPv6-first (operator directive): the ephemeral local socket is bound in
    /// the address family of the resolved `collector` — `[::]:0` for an IPv6
    /// collector, `0.0.0.0:0` only when the collector resolves to IPv4 — so an
    /// IPv6 NMS is reachable (a hard-coded `0.0.0.0` socket cannot send to one).
    /// A user-supplied IPv4 collector still works; we never *default* to IPv4.
    ///
    /// # Errors
    ///
    /// Returns [`SnmpError::Transport`] if the collector address cannot be
    /// resolved, the socket cannot be bound, or the connect fails.
    pub fn connect(
        collector: impl std::net::ToSocketAddrs,
        community: impl Into<String>,
    ) -> Result<Self, SnmpError> {
        // Resolve first so the ephemeral local bind matches the collector's
        // family (IPv6-first). `connect` is then a no-op family check.
        let mut addrs = collector
            .to_socket_addrs()
            .map_err(|e| SnmpError::Transport(e.to_string()))?;
        let target = addrs
            .next()
            .ok_or_else(|| SnmpError::Transport("collector resolved to no address".to_owned()))?;
        let local: std::net::SocketAddr = if target.is_ipv6() {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        };
        let socket =
            std::net::UdpSocket::bind(local).map_err(|e| SnmpError::Transport(e.to_string()))?;
        socket
            .connect(target)
            .map_err(|e| SnmpError::Transport(e.to_string()))?;
        Ok(Self {
            socket,
            community: community.into(),
            request_id: std::sync::atomic::AtomicI32::new(1),
        })
    }

    /// Encode `trap` as an `SNMPv2c` message and send it as one UDP datagram.
    ///
    /// Each call uses a fresh, wrapping `request-id`. SNMP traps are
    /// unacknowledged and lossy by design — appropriate for best-effort
    /// telemetry that must never back-pressure the engine.
    ///
    /// # Errors
    ///
    /// Returns [`SnmpError::Transport`] if the datagram cannot be sent.
    pub fn send(&self, trap: &Trap) -> Result<(), SnmpError> {
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pdu = trap.to_pdu(request_id);
        // version 1 == SNMPv2c.
        let datagram = encode_trap_v2c_message(1, &self.community, &pdu);
        self.socket
            .send(&datagram)
            .map(|_| ())
            .map_err(|e| SnmpError::Transport(e.to_string()))
    }
}
