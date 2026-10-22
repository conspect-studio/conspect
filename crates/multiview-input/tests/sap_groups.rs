//! SAP multicast **group set** + **scope selector** tests (RFC 2974 §3 / RFC
//! 2365; ADR-0041 §7, brief §4).
//!
//! The listener joins the full group set (the four IPv4 SAP groups — including
//! the AES67/Dante de-facto `239.255.255.255` — plus the IPv6 SAP group) so it
//! discovers the most sessions; the announcer picks its SAP group from the
//! **scope of the media address**, never from a TTL. The SAP group and the SDP
//! `c=` media group are modelled as two **distinct typed values**.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use multiview_input::sap::groups::{
    announce_group_for, receive_group_set, MediaGroup, SapGroup, SAP_PORT, SAP_TTL,
};

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

#[test]
fn sap_port_and_ttl_are_the_rfc_constants() {
    assert_eq!(SAP_PORT, 9875, "SAP rides UDP 9875 (RFC 2974 §3)");
    assert_eq!(SAP_TTL, 255, "SAP TTL SHOULD be 255 (RFC 2974 §3)");
}

#[test]
fn receive_set_is_the_four_ipv4_groups_plus_the_ipv6_group() {
    let set = receive_group_set();
    // The RFC 2974 §3 / RFC 2365 IPv4 SAP groups (global, org-local, local,
    // link-local) + the IPv6 SAP group. `239.255.255.255` is the local-scope
    // group AND the de-facto AES67/Dante group — present, never omitted.
    let expected = [
        v4(224, 2, 127, 254),   // global (IANA SAPv1)
        v4(239, 195, 255, 255), // org-local (top of 239.192/14)
        v4(239, 255, 255, 255), // local / AES67 / Dante
        v4(224, 0, 0, 255),     // link-local
        IpAddr::V6(Ipv6Addr::new(0xff05, 0, 0, 0, 0, 0, 2, 0x7ffe)), // IPv6 site-local SAP group
    ];
    for group in expected {
        assert!(set.contains(&group), "receive set must include {group}");
    }
    assert_eq!(
        set.len(),
        expected.len(),
        "receive set is exactly the four IPv4 groups + the IPv6 group (no dupes, no SAPv0)"
    );
    assert!(
        set.contains(&v4(239, 255, 255, 255)),
        "the AES67/Dante group 239.255.255.255 must be joined or every AES67 session is missed"
    );
    // The obsolete SAPv0 group MUST NOT be present.
    assert!(
        !set.contains(&v4(224, 2, 127, 255)),
        "224.2.127.255 is obsolete SAPv0 and must never be joined"
    );
}

#[test]
fn announce_group_is_selected_from_the_ipv4_media_scope() {
    // Local scope 239.255/16 -> 239.255.255.255 (the #1 "VLC shows nothing" bug
    // is hard-coding the global group for a site-local stream).
    assert_eq!(
        announce_group_for(MediaGroup::new(v4(239, 255, 12, 34))),
        SapGroup::new(v4(239, 255, 255, 255)),
    );
    // Org-local scope 239.192/14 -> 239.195.255.255.
    assert_eq!(
        announce_group_for(MediaGroup::new(v4(239, 193, 1, 1))),
        SapGroup::new(v4(239, 195, 255, 255)),
    );
    // Link-local 224.0.0/24 -> 224.0.0.255.
    assert_eq!(
        announce_group_for(MediaGroup::new(v4(224, 0, 0, 9))),
        SapGroup::new(v4(224, 0, 0, 255)),
    );
    // Anything else -> the global group.
    assert_eq!(
        announce_group_for(MediaGroup::new(v4(233, 1, 2, 3))),
        SapGroup::new(v4(224, 2, 127, 254)),
    );
}

#[test]
fn announce_group_mirrors_the_ipv6_media_scope_nibble() {
    // The IPv6 SAP group is FF0X::2:7FFE where X is the media address' scope
    // nibble (the low nibble of the 2nd byte).
    let cases = [
        (0xff3e_u16, 0xff0e_u16), // global media -> global SAP group
        (0xff05, 0xff05),         // site-local
        (0xff08, 0xff08),         // org-local
        (0xff02, 0xff02),         // link-local
    ];
    for (media_first, sap_first) in cases {
        let media = IpAddr::V6(Ipv6Addr::new(media_first, 0, 0, 0, 0, 0, 0, 1));
        let expected = IpAddr::V6(Ipv6Addr::new(sap_first, 0, 0, 0, 0, 0, 2, 0x7ffe));
        assert_eq!(
            announce_group_for(MediaGroup::new(media)),
            SapGroup::new(expected),
            "IPv6 media scope nibble selects the SAP group scope",
        );
    }
}

#[test]
fn sap_group_and_media_group_are_distinct_types() {
    // The types are distinct so the SAP signalling group can never be confused
    // with the SDP `c=` media group (they are different addresses).
    let media = MediaGroup::new(v4(239, 255, 0, 1));
    let sap = announce_group_for(media);
    assert_eq!(media.addr(), v4(239, 255, 0, 1));
    assert_eq!(sap.addr(), v4(239, 255, 255, 255));
    assert_ne!(media.addr(), sap.addr());
}
