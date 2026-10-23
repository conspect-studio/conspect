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
    RejectedGroup, V4_BLOCKS,
};
use proptest::prelude::*;

/// Whether `addr` falls inside `base/prefix` (IPv4), with the base as a `u32`.
fn in_net_v4_bits(addr: Ipv4Addr, base: u32, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (addr.to_bits() & mask) == (base & mask)
}

/// The class the oracle assigns to `addr`, or `None` when it is not an accepted
/// media group — a longest-prefix lookup over the **production** registry table
/// ([`V4_BLOCKS`]).
///
/// Reading the implementation's own table is deliberate: the table is the spec
/// ([`MulticastGroup`] documents it as the single source of truth), so this test
/// pins that `classify` *implements* the table, and the table's content is
/// pinned separately by [`V4_REGISTRY_ROWS`] and the table-driven accept/reject
/// cases. A hand-written second table is what drifted here, repeatedly.
fn v4_oracle(addr: Ipv4Addr) -> Option<MulticastClass> {
    V4_BLOCKS
        .iter()
        .filter(|(base, prefix, _)| in_net_v4_bits(addr, *base, *prefix))
        .max_by_key(|(_, prefix, _)| *prefix)
        .and_then(|(_, _, class)| *class)
}

/// The blocks the registry table must contain, exactly as the IANA IPv4
/// Multicast Address Space Registry / RFC 5771 defines them. Each row is
/// `(prefix, class, first, last-equivalent probe)` and is asserted against
/// [`V4_BLOCKS`] below, so a silently dropped or re-classified block fails a
/// named test rather than drifting.
const V4_REGISTRY_ROWS: &[(&str, u8, Option<MulticastClass>)] = &[
    ("224.0.0.0", 24, None),
    ("224.0.1.0", 24, Some(MulticastClass::Routed)),
    // AD-HOC Block I 224.0.2.0-224.0.255.255, tiled by seven CIDR rows.
    ("224.0.2.0", 23, Some(MulticastClass::Routed)),
    ("224.0.4.0", 22, Some(MulticastClass::Routed)),
    ("224.0.8.0", 21, Some(MulticastClass::Routed)),
    ("224.0.16.0", 20, Some(MulticastClass::Routed)),
    ("224.0.32.0", 19, Some(MulticastClass::Routed)),
    ("224.0.64.0", 18, Some(MulticastClass::Routed)),
    ("224.0.128.0", 17, Some(MulticastClass::Routed)),
    ("224.1.0.0", 16, None),
    ("224.2.0.0", 16, Some(MulticastClass::Routed)),
    ("224.3.0.0", 16, Some(MulticastClass::Routed)),
    ("224.4.0.0", 16, Some(MulticastClass::Routed)),
    ("224.252.0.0", 14, Some(MulticastClass::Routed)),
    ("224.0.0.0", 8, None),
    ("225.0.0.0", 8, None),
    ("226.0.0.0", 7, None),
    ("228.0.0.0", 6, None),
    ("232.0.0.0", 8, Some(MulticastClass::SourceSpecific)),
    ("232.0.0.0", 24, None),
    ("233.0.0.0", 8, Some(MulticastClass::Routed)),
    ("233.252.0.0", 24, None),
    ("234.0.0.0", 8, Some(MulticastClass::Routed)),
    ("235.0.0.0", 8, None),
    ("236.0.0.0", 7, None),
    ("238.0.0.0", 8, None),
    ("239.0.0.0", 8, Some(MulticastClass::AdminScoped)),
];

/// The registry table carries exactly the IANA rows, with their classes.
#[test]
fn v4_registry_table_matches_iana() {
    for (base, prefix, class) in V4_REGISTRY_ROWS {
        let base: Ipv4Addr = base.parse().expect("literal");
        assert!(
            V4_BLOCKS.contains(&(base.to_bits(), *prefix, *class)),
            "registry table is missing {base}/{prefix} => {class:?}"
        );
    }
    // ... and no extra row beyond the table-driven boundary probes: the rows
    // are the accepted entries plus the reserved carve-outs.
    assert_eq!(
        V4_REGISTRY_ROWS.len(),
        V4_BLOCKS
            .iter()
            .filter(|(base, prefix, class)| {
                V4_REGISTRY_ROWS.iter().any(|(b, p, c)| {
                    let b: Ipv4Addr = b.parse().expect("literal");
                    b.to_bits() == *base && *p == *prefix && *c == *class
                })
            })
            .count(),
        "the registry table has a row V4_REGISTRY_ROWS does not name"
    );
}

/// IPv6 multicast flags/scope-nibble classification (RFC 4291 §2.7,
/// RFC 7346 §2). Scope is checked first; flags `0x3` with zero plen and
/// prefix mark SSM (`ff3x::/32`, RFC 3306 §6 / RFC 4607 §1).
fn v6_oracle(flags: u8, scope: u8, body: [u16; 7]) -> Option<MulticastClass> {
    // RFC 4291 §2.7 and RFC 7346: flags do not override the scope.
    if !matches!(scope, 0x5 | 0x8 | 0xe) {
        return None;
    }
    // RFC 3306 §4 / RFC 7371 §4.1: P set without T is invalid,
    // regardless of the independent X and R flags.
    if flags & 0x4 != 0 || flags & 0x3 == 0x2 {
        return None;
    }
    if flags & 0x7 == 0x3 {
        // RFC 3306 §4 / RFC 7371 §4.1: rsvd must be zero and plen <= 64;
        // ff2 flag bits are ignored on receipt.
        if body[0] & 0x0f00 != 0 || body[0] & 0xff > 64 {
            return None;
        }
        // RFC 3306 §6: zero plen requires zero prefix; only that encoding
        // belongs to the RFC 4607 SSM range, never to prefix-based ASM.
        if body[0].to_be_bytes()[1] == 0 {
            if body[1..5].iter().any(|word| *word != 0) {
                return None;
            }
            let id = (u32::from(body[5]) << 16) | u32::from(body[6]);
            return (id >= 0x8000_0000).then_some(MulticastClass::SourceSpecific);
        }
    }
    match scope {
        0x5 => Some(MulticastClass::AdminScoped),  // site-local
        0x8 | 0xe => Some(MulticastClass::Routed), // organization-local, global
        // Reserved (0x0/0xf), local control (0x1/0x2), unsupported realm-/
        // admin-local (0x3/0x4), or unassigned (0x6/0x7/0x9-0xd).
        _ => None,
    }
}

/// The reason an IPv6 group is refused, given its flags/scope nibbles.
fn v6_reason(_flags: u8, scope: u8) -> RejectReason {
    match scope {
        0x1 => RejectReason::InterfaceLocalControl,
        0x2 => RejectReason::LinkLocalControl,
        _ => RejectReason::ReservedBlock,
    }
}

/// Build an IPv6 multicast address with the given flags/scope nibbles and a
/// random low-order body (`ff<flags><scope>:<body0>:…:<body6>`).
fn v6_group(flags: u8, scope: u8, body: [u16; 7]) -> Ipv6Addr {
    Ipv6Addr::new(
        0xff00 | (u16::from(flags) << 4) | u16::from(scope),
        body[0],
        body[1],
        body[2],
        body[3],
        body[4],
        body[5],
        body[6],
    )
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
                // Only `224.0.0.0/24` is the *link-local control* block; the
                // rest of the refused multicast space is a reservation.
                let [a, b, c, _] = addr.octets();
                let expected = if a == 224 && b == 0 && c == 0 {
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
        let addr = v6_group(flags, scope, body);
        let expected = v6_oracle(flags, scope, body);
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
        let addr_v6 = v6_group(flags, scope, body);
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
// Independent RFC 5771 §3 / IANA endpoint oracle: literal addresses and
// outcomes, deliberately not derived from V4_BLOCKS or the lookup algorithm.
// ---------------------------------------------------------------------------
#[test]
fn rfc5771_reserved_and_ad_hoc_endpoints() {
    const ENDPOINTS: &[(&str, Option<MulticastClass>)] = &[
        ("224.2.0.0", Some(MulticastClass::Routed)),
        ("224.2.255.255", Some(MulticastClass::Routed)),
        ("224.3.0.0", Some(MulticastClass::Routed)),
        ("224.3.255.255", Some(MulticastClass::Routed)),
        ("224.4.0.0", Some(MulticastClass::Routed)),
        ("224.4.0.1", Some(MulticastClass::Routed)),
        ("224.4.255.255", Some(MulticastClass::Routed)),
        ("224.5.0.0", None),
        ("224.5.255.255", None),
        ("224.6.0.0", None),
        ("224.6.1.1", None),
        ("224.251.255.255", None),
        ("224.252.0.0", Some(MulticastClass::Routed)),
        ("224.255.255.255", Some(MulticastClass::Routed)),
        ("225.0.0.0", None),
        ("225.1.2.3", None),
        ("231.255.255.255", None),
        ("232.0.0.0", None),
        ("232.0.1.0", Some(MulticastClass::SourceSpecific)),
        ("233.251.255.255", Some(MulticastClass::Routed)),
        ("233.252.0.0", None),
        ("234.0.0.0", Some(MulticastClass::Routed)),
        ("235.0.0.0", None),
        ("238.255.255.255", None),
        ("239.0.0.0", Some(MulticastClass::AdminScoped)),
    ];
    for &(text, expected) in ENDPOINTS {
        let addr = ip(text);
        assert_eq!(classify(addr), expected, "RFC 5771 §3: {text}");
        match expected {
            Some(class) => assert_eq!(MulticastGroup::try_from(addr).expect(text).class(), class),
            None => assert_eq!(
                MulticastGroup::try_from(addr).expect_err(text).reason(),
                RejectReason::ReservedBlock,
                "{text}"
            ),
        }
    }
}

#[test]
fn rfc4291_ssm_flags_cannot_override_prohibited_scope() {
    // RFC 4291 §2.7 + RFC 7346: multicast scope remains meaningful for SSM.
    for (text, reason) in [
        ("ff30::1234", RejectReason::ReservedBlock),
        ("ff31::1234", RejectReason::InterfaceLocalControl),
        ("ff32::1234", RejectReason::LinkLocalControl),
        ("ff33::1234", RejectReason::ReservedBlock),
        ("ff3f::1234", RejectReason::ReservedBlock),
    ] {
        let addr = ip(text);
        assert_eq!(classify(addr), None, "{text}");
        assert_eq!(
            MulticastGroup::try_from(addr).expect_err(text).reason(),
            reason
        );
        assert!(serde_json::from_str::<MulticastGroup>(&format!("\"{text}\"")).is_err());
    }
    assert_eq!(
        classify(ip("ff35::8000:1234")),
        Some(MulticastClass::SourceSpecific)
    );
    assert_eq!(
        classify(ip("ff3e::8000:1234")),
        Some(MulticastClass::SourceSpecific)
    );
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
    ("ff3e::8000:2", MulticastClass::SourceSpecific),
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
    // IANA RESERVED 224.1/16 is withheld (AD-HOC I 224.0.2.0-224.0.255.255
    // is allocated and accepted; see `iana_top_level_blocks_first_and_last`).
    ("224.1.0.0", RejectReason::ReservedBlock),
    ("224.1.255.255", RejectReason::ReservedBlock),
    // The reserved 224.5.0.0/16 slice.
    ("224.5.0.1", RejectReason::ReservedBlock),
    // IANA-reserved IPv4 blocks.
    ("232.0.0.0", RejectReason::ReservedBlock),
    ("232.0.0.1", RejectReason::ReservedBlock),
    ("233.252.0.1", RejectReason::ReservedBlock),
    ("236.0.0.1", RejectReason::ReservedBlock),
    ("238.0.0.1", RejectReason::ReservedBlock),
];

/// The classifier is **exhaustively** correct over the whole `224.0.0.0/4`
/// multicast space — all 2^28 addresses, not a sample.
///
/// `#[ignore]`d because it is the slow, deterministic proof rather than the
/// per-push check: the property test above samples the same space and is what
/// runs by default. Run it when the RFC-5771 table changes:
///
/// ```text
/// cargo test -p multiview-config --test multicast_address -- --ignored exhaustive
/// ```
#[test]
#[ignore = "exhaustive 2^28 sweep (minutes in debug); run when the RFC-5771 table changes"]
fn exhaustive_v4_multicast_sweep_matches_the_registry() {
    let mut checked: u64 = 0;
    for first in 224u8..=239 {
        for second in 0u8..=0xff {
            for third in 0u8..=0xff {
                for fourth in 0u8..=0xff {
                    let addr = Ipv4Addr::new(first, second, third, fourth);
                    assert_eq!(classify(IpAddr::V4(addr)), v4_oracle(addr), "{addr}");
                    checked += 1;
                }
            }
        }
    }
    // 224.0.0.0/4 spans sixteen /8s.
    assert_eq!(checked, 16 * (1 << 24), "the 224/4 space is 2^28 addresses");
}

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
    // `FF3x::4000:0000` — the reserved IPv6 SSM address; dynamic space begins at 8000:0000.
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
fn ipv6_ssm_group_id_allocation_boundaries() {
    // RFC 3307 §§4.1–4.3, RFC 4607 §1: permanent IDs cannot be SSM.
    for text in [
        "ff3e::",
        "ff3e::1",
        "ff3e::3fff:ffff",
        "ff3e::4000:0000",
        "ff3e::4000:0001",
        "ff3e::7fff:ffff",
    ] {
        assert_eq!(classify(ip(text)), None, "{text}");
        assert_eq!(
            MulticastGroup::try_from(ip(text)).unwrap_err().reason(),
            RejectReason::ReservedBlock,
            "{text}"
        );
        assert!(
            serde_json::from_str::<MulticastGroup>(&format!("\"{text}\"")).is_err(),
            "{text}"
        );
    }
    for text in ["ff3e::8000:0000", "ff3e::ffff:ffff"] {
        let group = MulticastGroup::try_from(ip(text)).expect(text);
        assert_eq!(group.class(), MulticastClass::SourceSpecific, "{text}");
        assert_eq!(
            group.validate(&[]),
            Some(RejectReason::SourcesRequired),
            "{text}"
        );
        assert_eq!(group.validate(&[ip("2001:db8::1")]), None, "{text}");
    }
}

#[test]
fn ipv6_prefix_based_groups_are_not_ssm_and_validate_encoding() {
    // RFC 3306 §§4, 6: plen=64 and embedded prefix 2001:db8::/64.
    for (text, class) in [
        ("ff3e:0040:2001:db8::8000:1", MulticastClass::Routed),
        ("ff35:0040:2001:db8::8000:1", MulticastClass::AdminScoped),
    ] {
        let group = MulticastGroup::try_from(ip(text)).expect(text);
        assert_eq!(group.class(), class, "{text}");
        assert!(!group.requires_sources(), "{text}");
        assert_eq!(group.validate(&[]), None, "{text}");
    }
    for text in [
        "ff3e:0041:2001:db8::8000:1", // plen=65
        "ff3e:0140:2001:db8::8000:1", // reserved field != 0
    ] {
        assert_eq!(classify(ip(text)), None, "{text}");
        assert_eq!(
            MulticastGroup::try_from(ip(text)).unwrap_err().reason(),
            RejectReason::ReservedBlock,
            "{text}"
        );
    }
}

/// Independent literal representatives of every IPv6 flags/plen/rsvd/prefix/
/// group-ID decision. The property oracle above separately checks combinations.
#[test]
fn ipv6_encoding_classes_have_literal_boundaries() {
    use MulticastClass::{AdminScoped, Routed, SourceSpecific};
    const CASES: &[(&str, Option<MulticastClass>)] = &[
        // Outside ff00::/8; non-SSM flags with each supported ASM scope.
        ("2001:db8::1", None),
        ("ff05::1", Some(AdminScoped)),
        ("ff08::1", Some(Routed)),
        ("ff0e::1", Some(Routed)),
        ("ff1e::1", Some(Routed)),
        ("ff2e::1", None), // P=1, T=0 violates RFC 3306 §4.
        ("ff4e::1", None), // R=1 unsupported.
        ("ff5e::1", None), // R=1 unsupported.
        ("ff6e::1", None), // P=1, T=0 (also R=1).
        ("ff7e::1", None), // R=1 embedded-RP, unsupported.
        ("ff8e::1", Some(Routed)),
        ("ff9e::1", Some(Routed)),
        ("ffae::1", None), // P=1, T=0 with X=1.
        ("ffbe::1", None), // RFC 7371 §4.1.2: X does not suppress SSM ID reservation.
        ("ffbe::8000:1", Some(SourceSpecific)),
        ("ffbe::7fff:ffff", None),
        ("ffbe::ffff:ffff", Some(SourceSpecific)),
        ("ffbe::8000:2", Some(SourceSpecific)),
        ("ffbe:40:2001:db8::1", Some(Routed)),
        ("ffbe:0:1::8000:1", None),
        ("ffce::1", None), // R=1 unsupported.
        ("ffde::1", None), // R=1 unsupported.
        ("ffee::1", None), // P=1, T=0 with X=R=1.
        ("fffe::1", None), // R=1 embedded-RP, unsupported.
        // Even with flags=3 and a valid dynamic ID, unsupported scopes lose.
        ("ff30::8000:1", None),
        ("ff31::8000:1", None),
        ("ff32::8000:1", None),
        ("ff33::8000:1", None),
        ("ff34::8000:1", None),
        ("ff36::8000:1", None),
        ("ff37::8000:1", None),
        ("ff39::8000:1", None),
        ("ff3a::8000:1", None),
        ("ff3b::8000:1", None),
        ("ff3c::8000:1", None),
        ("ff3d::8000:1", None),
        ("ff3f::8000:1", None),
        // Valid SSM encoding: zero rsvd, plen and prefix, dynamic group ID.
        ("ff35::8000:1", Some(SourceSpecific)),
        ("ff38::8000:1", Some(SourceSpecific)),
        ("ff3e::8000:1", Some(SourceSpecific)),
        // Permanent and IANA-reserved ID slices never become ASM.
        ("ff3e::", None),
        ("ff3e::3fff:ffff", None),
        ("ff3e::4000:1", None),
        ("ff3e::7fff:ffff", None),
        // Zero plen with a nonzero network-prefix byte is malformed, not ASM.
        ("ff3e:0:1::8000:1", None),
        ("ff3e::1:8000:1", None),
        ("ff35::1:0:8000:1", None),
        // Nonzero plen is prefix-based ASM, even if the prefix is zero.
        ("ff35:1::8000:1", Some(AdminScoped)),
        ("ff38:40:2001:db8::8000:1", Some(Routed)),
        ("ff3e:40::8000:1", Some(Routed)),
        ("ff3e:40:2001:db8::1", Some(Routed)),
        // rsvd!=0 and plen>64 reject, irrespective of prefix and ID.
        ("ff3e:100::8000:1", None),
        ("ffbe:100::8000:1", None),
        ("ff3e:41:2001:db8::8000:1", None),
        ("ffbe:41:2001:db8::8000:1", None),
        // RFC 7371 ff2 flags are ignored on receipt (not rsvd).
        ("ff3e:f000::8000:1", Some(SourceSpecific)),
        ("ffbe:f000::8000:1", Some(SourceSpecific)),
        ("ffbe:f000::1", None),
        ("ff3e:f040:2001:db8::8000:1", Some(Routed)),
        ("ff7e:40:2001:db8::8000:1", None),
        ("fffe:40:2001:db8::8000:1", None),
    ];
    for &(text, expected) in CASES {
        let addr = ip(text);
        assert_eq!(classify(addr), expected, "{text}");
        match expected {
            Some(class) => assert_eq!(
                MulticastGroup::try_from(addr).expect(text).class(),
                class,
                "{text}"
            ),
            None => assert!(MulticastGroup::try_from(addr).is_err(), "{text}"),
        }
    }
    for text in [
        "ff3e:0:1::8000:1",
        "ff3e::1:8000:1",
        "ff35::1:0:8000:1",
        "ffbe:0:1::8000:1",
    ] {
        let addr = ip(text);
        assert_eq!(
            MulticastGroup::try_from(addr).unwrap_err().reason(),
            RejectReason::ReservedBlock,
            "{text}"
        );
        assert!(
            serde_json::from_str::<MulticastGroup>(&format!("\"{text}\"")).is_err(),
            "{text}"
        );
    }
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

    let v6_ssm = MulticastGroup::try_from(ip("ff3e::8000:2")).expect("SSM");
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

/// Independent literal walk of EVERY top-level block of the IANA IPv4
/// Multicast Address Space Registry, 224.0.0.0 to 239.255.255.255, in order.
/// Each entry is `(registry section, first, last, expected)`; the addresses and
/// decisions are literals, not derived from `V4_BLOCKS` or the lookup.
/// Registry: <https://www.iana.org/assignments/multicast-addresses>.
#[test]
fn iana_top_level_blocks_first_and_last() {
    use MulticastClass::{AdminScoped, Routed, SourceSpecific};
    const WALK: &[(&str, &str, &str, Option<MulticastClass>)] = &[
        // "Local Network Control Block" 224.0.0.0-224.0.0.255: link-local control.
        ("Local Network Control", "224.0.0.0", "224.0.0.255", None),
        // "Internetwork Control Block" 224.0.1.0-224.0.1.255.
        (
            "Internetwork Control",
            "224.0.1.0",
            "224.0.1.255",
            Some(Routed),
        ),
        // "AD-HOC Block I" 224.0.2.0-224.0.255.255 (allocated, registered groups).
        ("AD-HOC I", "224.0.2.0", "224.0.255.255", Some(Routed)),
        // "RESERVED" 224.1.0.0-224.1.255.255.
        ("RESERVED 224.1/16", "224.1.0.0", "224.1.255.255", None),
        // "SDP/SAP Block" 224.2.0.0-224.2.255.255.
        ("SDP/SAP", "224.2.0.0", "224.2.255.255", Some(Routed)),
        // "AD-HOC Block II" 224.3.0.0-224.4.255.255.
        ("AD-HOC II", "224.3.0.0", "224.4.255.255", Some(Routed)),
        // "RESERVED" 224.5.0.0-224.251.255.255.
        (
            "RESERVED 224.5-224.251",
            "224.5.0.0",
            "224.251.255.255",
            None,
        ),
        // "DIS Transient Groups" 224.252.0.0-224.255.255.255 (224.252/14).
        (
            "DIS Transient",
            "224.252.0.0",
            "224.255.255.255",
            Some(Routed),
        ),
        // "RESERVED" 225.0.0.0-231.255.255.255.
        ("RESERVED 225-231", "225.0.0.0", "231.255.255.255", None),
        // "Source-Specific Multicast Block" 232.0.0.0/8; 232.0.0.0-232.0.0.255 reserved.
        ("SSM reserved head", "232.0.0.0", "232.0.0.255", None),
        ("SSM", "232.0.1.0", "232.255.255.255", Some(SourceSpecific)),
        // "GLOP Block" 233.0.0.0-233.251.255.255.
        ("GLOP", "233.0.0.0", "233.251.255.255", Some(Routed)),
        // "AD-HOC Block III" 233.252.0.0-233.255.255.255; MCAST-TEST-NET 233.252.0/24.
        ("MCAST-TEST-NET", "233.252.0.0", "233.252.0.255", None),
        ("AD-HOC III", "233.252.1.0", "233.255.255.255", Some(Routed)),
        // "Unicast-Prefix-based IPv4 Multicast Addresses" 234.0.0.0/8 (RFC 6034).
        (
            "Unicast-prefix-based",
            "234.0.0.0",
            "234.255.255.255",
            Some(Routed),
        ),
        // "Scoped Multicast Ranges": 235.0.0.0-238.255.255.255 Reserved.
        ("RESERVED 235-238", "235.0.0.0", "238.255.255.255", None),
        // "Scoped Multicast Ranges": 239.0.0.0/8 Organization-Local Scope.
        (
            "Admin-scoped",
            "239.0.0.0",
            "239.255.255.255",
            Some(AdminScoped),
        ),
    ];
    // Contiguity: each block starts right after the previous one ends.
    let mut next: u32 = Ipv4Addr::new(224, 0, 0, 0).to_bits();
    for &(name, first, last, expected) in WALK {
        let (f, l): (Ipv4Addr, Ipv4Addr) =
            (first.parse().expect("lit"), last.parse().expect("lit"));
        assert_eq!(f.to_bits(), next, "{name}: walk must be contiguous");
        next = l.to_bits().wrapping_add(1);
        for text in [first, last] {
            assert_eq!(classify(ip(text)), expected, "{name}: {text}");
        }
        // Interior probe of every block (midpoint).
        let mid = Ipv4Addr::from_bits(f.to_bits() + (l.to_bits() - f.to_bits()) / 2);
        assert_eq!(
            classify(IpAddr::V4(mid)),
            expected,
            "{name}: midpoint {mid}"
        );
    }
    assert_eq!(next, 0, "walk ends at 239.255.255.255");
}
