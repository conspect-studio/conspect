//! MC-0 (ADR-0040 §8; backlog `docs/development/multicast-transport-backlog.md`):
//! the typed multicast **address model** + total RFC-5771 classifier
//! (`multiview_config::multicast::address`).
//!
//! Pure Rust, default (GPU-free) build, no NIC. The classifier is the single
//! validation seam every later typed-multicast slice (MC-1..MC-12) consults at
//! config load, so its contract is pinned here:
//!
//! * **Accept** `224.0.0.0/4` minus the control/reserved blocks, and `ff00::/8`
//!   minus the reserved/interface-local/link-local scopes. IPv6 is the primary
//!   path (ADR-0042); the IPv4 form is the legacy peer.
//! * **Reject** `224.0.0.0/24` (local network control — never a media group),
//!   the IANA reserved blocks (`232.0.0.0/24`, `233.252.0.0/24`, scope `0x0` and
//!   `0xf` under `ff00::/8`, and `ff3x::4000:0000`), and — at config load — the
//!   interface-local (`ff01::/16`) and link-local (`ff02::/16`) scopes.
//! * `232/8` (IPv4 SSM) and `ff3x::/32` (IPv6 SSM) are accepted **only** with a
//!   non-empty source list, enforced through [`MulticastGroup::validate`].
//! * `239/8` (IPv4) / non-global-scope IPv6 classify as **admin-scoped**.
//!
//! The IPv4 property test walks the **whole** `224.0.0.0/4` space against an
//! explicit block table (the oracle), and the IPv6 property test walks every
//! scope nibble; a block the classifier does not know therefore fails here
//! instead of silently becoming "accepted".
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use multiview_config::multicast::{
    classify, MulticastClass, MulticastGroup, MulticastIface, PacketCount, RejectReason,
    RejectedGroup,
};
use proptest::prelude::*;

/// Record one IPv4 block in [`V4_BLOCKS`]. A macro keeps the registry table
/// compact and readable — `rustfmt` shreds an inline tuple table per field.
macro_rules! v4_block {
    ($base:literal, $prefix:literal, $class:expr) => {
        (ipv4($base), $prefix, $class)
    };
}

/// The IPv4 multicast blocks the classifier must know, in registry order: the
/// block's first address, its prefix length, and the classification every
/// member shares. `None` marks a block that is **not** an accepted media group.
///
/// The table covers `224.0.0.0/4` exactly, and the property test walks that
/// whole space against it — so a block the classifier accepts but the table
/// does not know (or vice versa) fails there rather than drifting silently.
fn v4_blocks() -> &'static [(Ipv4Addr, u8, Option<MulticastClass>)] {
    static BLOCKS: std::sync::OnceLock<Vec<(Ipv4Addr, u8, Option<MulticastClass>)>> =
        std::sync::OnceLock::new();
    BLOCKS.get_or_init(|| {
        vec![
            // Local Network Control Block (RFC 5771 §4/§6) — never a media group.
            v4_block!("224.0.0.0", 24, None),
            // Internetwork Control Block — routed.
            v4_block!("224.0.1.0", 24, Some(MulticastClass::Routed)),
            // AD-HOC Block I — no static media allocation.
            v4_block!("224.0.2.0", 23, None),
            // SDP/SAP blocks (RFC 2974 §3) — the SAPv1 session range is accepted.
            v4_block!("224.2.0.0", 24, Some(MulticastClass::Routed)),
            v4_block!("224.2.1.0", 24, Some(MulticastClass::Routed)),
            v4_block!("224.2.127.0", 24, Some(MulticastClass::Routed)),
            v4_block!("224.2.128.0", 24, Some(MulticastClass::Routed)),
            // AD-HOC Blocks II/III + the ASM bulk, `224.3.0.0`–`231.255.255.255`
            // (RFC 5771 withholds no gap between 224.3 and 231).
            v4_block!("224.3.0.0", 16, Some(MulticastClass::Routed)),
            v4_block!("224.4.0.0", 14, Some(MulticastClass::Routed)),
            v4_block!("224.8.0.0", 13, Some(MulticastClass::Routed)),
            v4_block!("224.16.0.0", 12, Some(MulticastClass::Routed)),
            v4_block!("224.32.0.0", 11, Some(MulticastClass::Routed)),
            v4_block!("224.64.0.0", 10, Some(MulticastClass::Routed)),
            v4_block!("224.128.0.0", 9, Some(MulticastClass::Routed)),
            v4_block!("225.0.0.0", 8, Some(MulticastClass::Routed)),
            v4_block!("226.0.0.0", 7, Some(MulticastClass::Routed)),
            v4_block!("228.0.0.0", 6, Some(MulticastClass::Routed)),
            v4_block!("230.0.0.0", 7, Some(MulticastClass::Routed)),
            v4_block!("231.0.0.0", 8, Some(MulticastClass::Routed)),
            // GLOP addressing — routed.
            v4_block!("233.0.0.0", 8, Some(MulticastClass::Routed)),
            // MCAST-TEST-NET (RFC 6676) — reserved.
            v4_block!("233.252.0.0", 24, None),
            // Unicast-prefix-based (RFC 6034) — routed.
            v4_block!("234.0.0.0", 7, Some(MulticastClass::Routed)),
            // IANA reservations (236.0.0.0/7 and 238.0.0.0/8).
            v4_block!("236.0.0.0", 7, None),
            v4_block!("238.0.0.0", 8, None),
            // Admin-scoped ASM (RFC 2365).
            v4_block!("239.0.0.0", 8, Some(MulticastClass::AdminScoped)),
        ]
    })
}

/// The SSM block, `232.0.0.0/8` (RFC 4607 §7).
const V4_SSM: (Ipv4Addr, u8) = (Ipv4Addr::new(232, 0, 0, 0), 8);

/// The reserved head of the SSM block, `232.0.0.0/24` — not a media group.
const V4_SSM_RESERVED: (Ipv4Addr, u8) = (Ipv4Addr::new(232, 0, 0, 0), 24);

/// Whether `addr` falls inside `base/prefix` (IPv4).
fn in_net_v4(addr: Ipv4Addr, base: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (addr.to_bits() & mask) == (base.to_bits() & mask)
}

/// The class the oracle assigns to `addr`, or `None` when it is not an accepted
/// media group. Built from [`V4_BLOCKS`] plus the `232/8` SSM carve-out, with
/// the most-specific (longest-prefix) block always winning — so a narrow
/// reservation (`232.0.0.0/24`, `233.252.0.0/24`) overrides the wider block it
/// sits inside.
fn v4_oracle(addr: Ipv4Addr) -> Option<MulticastClass> {
    let (ssm_base, ssm_prefix) = V4_SSM;
    let (res_base, res_prefix) = V4_SSM_RESERVED;
    let ssm = in_net_v4(addr, ssm_base, ssm_prefix).then(|| {
        if in_net_v4(addr, res_base, res_prefix) {
            (res_prefix, None)
        } else {
            (ssm_prefix, Some(MulticastClass::SourceSpecific))
        }
    });
    v4_blocks()
        .iter()
        .filter(|(base, prefix, _)| in_net_v4(addr, *base, *prefix))
        .map(|(_, prefix, class)| (*prefix, *class))
        .chain(ssm)
        .max_by_key(|(prefix, _)| *prefix)
        .and_then(|(_, class)| class)
}

/// IPv6 multicast flags/scope-nibble classification (RFC 4291 §2.7,
/// RFC 5771 §11). The **flags** nibble `0x3` marks SSM (`ff3x::/32`,
/// RFC 4607 §7); otherwise the **scope** nibble alone decides.
fn v6_oracle(flags: u8, scope: u8, reserved_ssm: bool) -> Option<MulticastClass> {
    if flags == 0x3 {
        return if reserved_ssm {
            None // ff3x::4000:0000–ff3x::7fff:ffff (RFC 4607 §7)
        } else {
            Some(MulticastClass::SourceSpecific)
        };
    }
    match scope {
        0x5 => Some(MulticastClass::AdminScoped),  // site-local
        0x8 | 0xe => Some(MulticastClass::Routed), // organization-local, global
        // Reserved (0x0, 0x1 interface-local, 0x2 link-local, 0x3 without the
        // SSM flags, 0xf) or unassigned (0x4, 0x6, 0x7, 0x9-0xd).
        _ => None,
    }
}

/// The reason an IPv6 group is refused, given its flags/scope nibbles.
fn v6_reason(flags: u8, scope: u8) -> RejectReason {
    if flags == 0x3 {
        // Only the reserved sub-range can be refused at this point.
        return RejectReason::ReservedBlock;
    }
    match scope {
        0x1 => RejectReason::InterfaceLocalControl,
        0x2 => RejectReason::LinkLocalControl,
        _ => RejectReason::ReservedBlock,
    }
}

/// Build an IPv6 multicast address with the given flags/scope nibbles and a
/// random low-order body (`ff<flags><scope>:<body0>:…:<body6>`).
fn v6_group(flags: u8, scope: u8, body: [u16; 7]) -> (Ipv6Addr, bool) {
    let addr = Ipv6Addr::new(
        0xff00 | (u16::from(flags) << 4) | u16::from(scope),
        body[0],
        body[1],
        body[2],
        body[3],
        body[4],
        body[5],
        body[6],
    );
    // The reserved SSM sub-range (ff3x::4000:0000–ff3x::7fff:ffff) is the
    // trailing 32 bits of the address — the `body[5]:body[6]` pair — with its
    // top two bits set to `0b01`.
    let field = (u32::from(body[5]) << 16) | u32::from(body[6]);
    (addr, (field >> 30) == 0b01)
}

/// Parse an IPv4 literal, panicking with the text on a typo.
fn ipv4(text: &str) -> Ipv4Addr {
    Ipv4Addr::from_str(text).unwrap_or_else(|_| panic!("`{text}` is an IPv4 literal"))
}

/// Parse an IP literal, panicking with the text on a typo.
fn ip(text: &str) -> IpAddr {
    IpAddr::from_str(text).unwrap_or_else(|_| panic!("`{text}` is an IP literal"))
}

/// The reject reason carried by an error, for table comparison.
///
/// Uses the public accessor so the helper needs no wildcard arm over a
/// `#[non_exhaustive]` enum.
fn reason_of(err: &RejectedGroup) -> RejectReason {
    err.reason()
}

proptest! {
    /// The classifier is **total** and agrees with the RFC-5771 block table for
    /// every address in the whole IPv4 space — no panics, no unclassified
    /// multicast address, and every non-multicast address is `None`.
    #[test]
    fn v4_classifier_matches_the_registry(bits in any::<u32>()) {
        let addr = Ipv4Addr::from_bits(bits);
        let class = classify(IpAddr::V4(addr));
        // The table is the oracle: an accepted class is exactly a table row.
        prop_assert_eq!(class, v4_oracle(addr), "ip={:?}", addr);
        // A refusal outside 224.0.0.0/4 is "not multicast", inside it a block.
        if class.is_some() {
            prop_assert!(addr.is_multicast(), "ip={:?}", addr);
        }
        match MulticastGroup::try_from(IpAddr::V4(addr)) {
            Ok(group) => {
                prop_assert_eq!(Some(group.class()), v4_oracle(addr), "ip={:?}", addr);
                prop_assert_eq!(group.addr(), IpAddr::V4(addr), "ip={:?}", addr);
            }
            Err(RejectedGroup::NotMulticast { .. }) => {
                prop_assert!(!addr.is_multicast(), "ip={:?}", addr);
            }
            Err(RejectedGroup::Parse { .. }) => {
                prop_assert!(false, "an IpAddr never fails to parse: {addr}");
            }
            Err(RejectedGroup::Rejected { class, reason, .. }) => {
                prop_assert!(addr.is_multicast(), "ip={:?}", addr);
                prop_assert_eq!(class, v4_oracle(addr), "ip={:?}", addr);
                let expected = if addr.octets()[0] == 224 && addr.octets()[1] == 0 {
                    RejectReason::LinkLocalControl
                } else {
                    RejectReason::ReservedBlock
                };
                prop_assert_eq!(reason, expected, "ip={:?}", addr);
            }
            Err(other) => prop_assert!(false, "unexpected refusal {:?} for {:?}", other, addr),
        }
    }

    /// The IPv6 classifier keys on the **flags** nibble (SSM, `ff3x::/32`) and
    /// otherwise on the **scope** nibble: for every flags/scope combination both
    /// the accept/refuse decision and the class/reason match the oracle
    /// (ADR-0042 puts IPv6 first).
    #[test]
    fn v6_flags_and_scope_nibbles_are_classified(
        flags in 0u8..=0xfu8,
        scope in 0u8..=0xfu8,
        body in proptest::array::uniform7(any::<u16>()),
    ) {
        let (addr, reserved_ssm) = v6_group(flags, scope, body);
        let expected = v6_oracle(flags, scope, reserved_ssm);
        prop_assert_eq!(classify(IpAddr::V6(addr)), expected, "ip={:?}", addr);
        match MulticastGroup::try_from(IpAddr::V6(addr)) {
            Ok(group) => {
                prop_assert_eq!(Some(group.class()), expected, "ip={:?}", addr);
                prop_assert_eq!(group.addr(), IpAddr::V6(addr), "ip={:?}", addr);
                prop_assert_eq!(group.to_string(), addr.to_string(), "ip={:?}", addr);
            }
            Err(RejectedGroup::NotMulticast { .. }) => {
                prop_assert!(false, "ff00::/8 must classify as multicast: {addr}");
            }
            Err(RejectedGroup::Parse { .. }) => {
                prop_assert!(false, "an IpAddr never fails to parse: {addr}");
            }
            Err(RejectedGroup::Rejected { class, reason, .. }) => {
                prop_assert_eq!(class, expected, "ip={:?}", addr);
                prop_assert_eq!(reason, v6_reason(flags, scope), "ip={:?}", addr);
            }
            Err(other) => prop_assert!(false, "unexpected refusal {:?} for {:?}", other, addr),
        }
    }

    /// Round-trip: every accepted group parses back to exactly itself, and every
    /// refused address stays refused (no family or scope drift).
    #[test]
    fn accepted_groups_round_trip(
        v4 in any::<u32>(),
        flags in 0u8..=0xfu8,
        scope in 0u8..=0xfu8,
        body in proptest::array::uniform7(any::<u16>()),
    ) {
        let addr_v4 = Ipv4Addr::from_bits(v4);
        let (addr_v6, _) = v6_group(flags, scope, body);
        let addrs = [IpAddr::V4(addr_v4), IpAddr::V6(addr_v6)];
        for addr in addrs {
            match classify(addr) {
                None => prop_assert!(MulticastGroup::try_from(addr).is_err(), "ip={:?}", addr),
                Some(class) => {
                    let group = MulticastGroup::try_from(addr).expect("classifier accepted it");
                    prop_assert_eq!(group.class(), class, "ip={:?}", addr);
                    prop_assert_eq!(group.addr(), addr, "ip={:?}", addr);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Table-driven acceptance: the exact blocks the issue names.
// ---------------------------------------------------------------------------

/// IPv4 blocks that **are** usable media groups (legacy peer), with the class
/// each must report. IPv6 leads — see [`V6_ACCEPTED`].
const V4_ACCEPTED: &[(&str, MulticastClass)] = &[
    ("239.255.1.10", MulticastClass::AdminScoped),
    ("239.192.0.1", MulticastClass::AdminScoped),
    ("224.0.1.1", MulticastClass::Routed),
    ("224.2.127.254", MulticastClass::Routed),
    ("231.255.255.255", MulticastClass::Routed),
    ("233.1.2.3", MulticastClass::Routed),
    ("232.1.2.3", MulticastClass::SourceSpecific),
    ("232.255.255.255", MulticastClass::SourceSpecific),
];

/// IPv6 groups that **are** usable media groups (the primary path, ADR-0042).
const V6_ACCEPTED: &[(&str, MulticastClass)] = &[
    ("ff08::1:10", MulticastClass::Routed),
    ("ff0e::1", MulticastClass::Routed),
    ("ff05::1:3", MulticastClass::AdminScoped),
    ("ff05::4000:1", MulticastClass::AdminScoped),
    ("ff3e::1:2:3", MulticastClass::SourceSpecific),
    ("ff3e::8000:1", MulticastClass::SourceSpecific),
];

/// Addresses that must be **refused** as a media group, with the reject reason.
const REJECTED: &[(&str, RejectReason)] = &[
    // Not multicast at all.
    ("10.0.0.1", RejectReason::NotMulticast),
    ("127.0.0.1", RejectReason::NotMulticast),
    ("::1", RejectReason::NotMulticast),
    ("::ffff:239.1.1.1", RejectReason::NotMulticast), // IPv4-mapped ≠ v4 multicast
    // IPv6 reserved / interface-local / link-local scopes.
    ("ff00::1", RejectReason::ReservedBlock),
    ("ff0f::1", RejectReason::ReservedBlock),
    ("ff01::1", RejectReason::InterfaceLocalControl),
    ("ff01::1234", RejectReason::InterfaceLocalControl),
    ("ff02::1", RejectReason::LinkLocalControl),
    ("ff02::fb", RejectReason::LinkLocalControl),
    // The IPv4 link-local control block (RFC 5771 §4) — never a media group.
    ("224.0.0.0", RejectReason::LinkLocalControl),
    ("224.0.0.251", RejectReason::LinkLocalControl),
    ("224.0.255.255", RejectReason::ReservedBlock),
    // IANA-reserved IPv4 blocks.
    ("224.0.2.0", RejectReason::ReservedBlock),
    ("232.0.0.0", RejectReason::ReservedBlock),
    ("232.0.0.1", RejectReason::ReservedBlock),
    ("233.252.0.1", RejectReason::ReservedBlock),
    ("236.0.0.1", RejectReason::ReservedBlock),
    ("238.0.0.1", RejectReason::ReservedBlock),
];

#[test]
fn accepted_ipv4_blocks_classify() {
    for (text, expected) in V4_ACCEPTED {
        let addr = ip(text);
        let group = MulticastGroup::try_from(addr)
            .unwrap_or_else(|err| panic!("{text} must be accepted, got {err}"));
        assert_eq!(group.class(), *expected, "{text}");
        assert_eq!(group.addr(), addr, "{text}");
        assert_eq!(classify(addr), Some(*expected), "{text}");
    }
}

#[test]
fn accepted_ipv6_blocks_classify() {
    for (text, expected) in V6_ACCEPTED {
        let addr = ip(text);
        let group = MulticastGroup::try_from(addr)
            .unwrap_or_else(|err| panic!("{text} must be accepted, got {err}"));
        assert_eq!(group.class(), *expected, "{text}");
        assert_eq!(group.addr(), addr, "{text}");
        assert_eq!(classify(addr), Some(*expected), "{text}");
    }
}

#[test]
fn rejected_blocks_carry_the_right_reason() {
    for (text, reason) in REJECTED {
        let addr = ip(text);
        let err = MulticastGroup::try_from(addr)
            .expect_err(&format!("{text} must be refused as a media group"));
        assert_eq!(reason_of(&err), *reason, "{text} ({err})");
        // The error names the offending address so an operator can fix it.
        assert!(err.to_string().contains(text), "{text} ({err})");
    }
}

#[test]
fn issue_named_blocks_are_rejected_verbatim() {
    // `224.0.0.0/24` — every member refused (link-local control).
    assert!(MulticastGroup::try_from(ip("224.0.0.0")).is_err());
    assert!(MulticastGroup::try_from(ip("224.0.0.17")).is_err());
    // `232.0.0.0` — the reserved head of the SSM block.
    let err = MulticastGroup::try_from(ip("232.0.0.0")).expect_err("232.0.0.0 is reserved");
    assert_eq!(reason_of(&err), RejectReason::ReservedBlock);
    // `FF3x::4000:0000` — the reserved IPv6 SSM address; one past it is fine.
    let reserved =
        MulticastGroup::try_from(ip("ff3e::4000:0000")).expect_err("reserved SSM address");
    assert_eq!(reason_of(&reserved), RejectReason::ReservedBlock);
    // The whole 4000::–7fff:: field is reserved; the next bit is a valid group.
    let also_reserved =
        MulticastGroup::try_from(ip("ff3e::4000:0001")).expect_err("reserved SSM address");
    assert_eq!(reason_of(&also_reserved), RejectReason::ReservedBlock);
    assert!(MulticastGroup::try_from(ip("ff3e::8000:0001")).is_ok());
}

#[test]
fn ssm_requires_a_non_empty_source_list() {
    let v4_ssm = MulticastGroup::try_from(ip("232.1.2.3")).expect("SSM");
    assert_eq!(v4_ssm.class(), MulticastClass::SourceSpecific);
    assert!(v4_ssm.requires_sources());
    assert_eq!(
        v4_ssm.validate(&[]),
        Some(RejectReason::SourcesRequired),
        "232/8 needs a source list"
    );
    assert_eq!(v4_ssm.validate(&[ip("192.0.2.7")]), None);

    let v6_ssm = MulticastGroup::try_from(ip("ff3e::1:2:3")).expect("SSM");
    assert!(v6_ssm.requires_sources());
    assert_eq!(v6_ssm.validate(&[]), Some(RejectReason::SourcesRequired));

    // ASM groups accept an empty source list (they are not source-filtered).
    let asm = MulticastGroup::try_from(ip("239.255.1.10")).expect("ASM");
    assert!(!asm.requires_sources());
    assert_eq!(asm.validate(&[]), None);
}

#[test]
fn from_str_reports_the_offending_text() {
    let err = MulticastGroup::from_str("not-an-ip").expect_err("parse");
    assert!(matches!(err, RejectedGroup::Parse { .. }), "{err}");
    assert!(err.to_string().contains("not-an-ip"), "{err}");

    let err = MulticastGroup::from_str("224.0.0.1").expect_err("refused");
    assert!(matches!(err, RejectedGroup::Rejected { .. }), "{err}");
}

#[test]
fn group_serde_round_trips_as_a_bare_address_string() {
    let group = MulticastGroup::try_from(ip("ff08::1:10")).expect("group");
    let json = serde_json::to_string(&group).expect("serialize");
    assert_eq!(json, "\"ff08::1:10\"");
    let back: MulticastGroup = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, group);
}

#[test]
fn iface_is_a_typed_literal_with_an_optional_zone() {
    let scoped = MulticastIface::from_str("ff02::1%eth0").expect("scoped literal");
    assert_eq!(scoped.to_string(), "ff02::1%eth0");
    assert_eq!(scoped.name(), Some("eth0"));
    assert_eq!(scoped.addr(), ip("ff02::1"));

    let bare = MulticastIface::from_str("ff08::5").expect("bare literal");
    assert_eq!(bare.name(), None);
    assert_eq!(bare.addr(), ip("ff08::5"));
    assert_eq!(bare.to_string(), "ff08::5");

    // A non-address interface token is refused (a NIC *name* is not a literal).
    assert!(MulticastIface::from_str("eth0").is_err());
    // A zone on an IPv4 literal, or an empty zone, is a typo.
    assert!(MulticastIface::from_str("10.20.0.5%eth0").is_err());
    assert!(MulticastIface::from_str("ff02::1%").is_err());
}

#[test]
fn packet_counts_are_typed_not_strings() {
    let packets = PacketCount::new(65_536);
    assert_eq!(packets.get(), 65_536);
    assert_eq!(packets.to_string(), "65536");
    assert_eq!(PacketCount::from(65_536_u64), packets);

    let json = serde_json::to_string(&packets).expect("serialize");
    assert_eq!(json, "65536");
    let back: PacketCount = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, packets);
}
