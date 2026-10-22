//! SAP multicast **group set** + **scope selector** (RFC 2974 §3, RFC 2365
//! admin scoping; ADR-0041 §7, brief §4).
//!
//! All SAP traffic uses UDP [`SAP_PORT`] (9875) with [`SAP_TTL`] (255); scope is
//! expressed by **address choice**, never by TTL. A listener joins the
//! [`receive_group_set`] (the four IPv4 SAP groups — including the AES67 / Dante
//! de-facto `239.255.255.255` — plus the IPv6 SAP group) so it discovers the
//! most sessions. An announcer picks its SAP group from the **scope of the media
//! address** via [`announce_group_for`] — hard-coding the global group for a
//! site-local stream is the #1 "VLC shows nothing" bug.
//!
//! The [`SapGroup`] (the signalling group on 9875) and the [`MediaGroup`] (the
//! SDP `c=` address the media rides on) are **two distinct typed values**: they
//! are different addresses and must never be confused.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The well-known UDP port all SAP traffic uses (RFC 2974 §3).
pub const SAP_PORT: u16 = 9875;

/// The TTL / hop-limit SAP announcements SHOULD use (RFC 2974 §3). Scope is
/// carried by the group **address**, not by this TTL.
pub const SAP_TTL: u32 = 255;

/// IPv4 **global**-scope SAP group (IANA `SAPv1`; the session range is
/// `224.2.128.0–224.2.255.255`). RFC 2974 §3.
pub const SAP_V4_GLOBAL: Ipv4Addr = Ipv4Addr::new(224, 2, 127, 254);

/// IPv4 **organization-local** SAP group — the top of the RFC 2365 org-local
/// scope `239.192.0.0/14` (`.195`, not `.192`).
pub const SAP_V4_ORG_LOCAL: Ipv4Addr = Ipv4Addr::new(239, 195, 255, 255);

/// IPv4 **local**-scope SAP group — the top of the RFC 2365 local scope
/// `239.255.0.0/16`. This is the **de-facto AES67 / Dante / RAVENNA** group and
/// is standards-sanctioned (RFC 2974 §3 designates the top of the scope zone).
pub const SAP_V4_LOCAL: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);

/// IPv4 **link-local** SAP group (`224.0.0.0/24`). RFC 2974 §3.
pub const SAP_V4_LINK_LOCAL: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 255);

/// The canonical IPv6 SAP group — **site-local** scope (`FF05::2:7FFE`), the
/// IPv6 analogue of the IPv4 local scope AES67 uses. RFC 2974 §3 defines the
/// template `FF0X::2:7FFE` (`X` = scope nibble); [`ipv6_sap_group`] builds the
/// group for any scope.
pub const SAP_V6_SITE_LOCAL: Ipv6Addr = Ipv6Addr::new(0xff05, 0, 0, 0, 0, 0, 2, 0x7ffe);

/// The multicast group an SDP `c=` line advertises for the **media** stream.
///
/// A distinct newtype from [`SapGroup`] so the SAP signalling group can never be
/// confused with the media group (they are different addresses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaGroup(IpAddr);

impl MediaGroup {
    /// Wrap a media multicast address.
    #[must_use]
    pub const fn new(addr: IpAddr) -> Self {
        Self(addr)
    }

    /// The wrapped media address.
    #[must_use]
    pub const fn addr(&self) -> IpAddr {
        self.0
    }
}

/// The multicast group SAP announcements for a session are **sent to / received
/// on** (on [`SAP_PORT`]).
///
/// A distinct newtype from [`MediaGroup`] (see [`announce_group_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SapGroup(IpAddr);

impl SapGroup {
    /// Wrap a SAP signalling multicast address.
    #[must_use]
    pub const fn new(addr: IpAddr) -> Self {
        Self(addr)
    }

    /// The wrapped SAP group address.
    #[must_use]
    pub const fn addr(&self) -> IpAddr {
        self.0
    }
}

/// The full set of SAP groups a listener joins to discover the most sessions.
///
/// The four IPv4 SAP groups (global, org-local, local, link-local) — of which
/// `239.255.255.255` is the AES67 / Dante de-facto group — plus the IPv6
/// site-local SAP group. Joining only the RFC global group silently misses every
/// AES67 / Dante session (brief §4). The obsolete `SAPv0` group `224.2.127.255` is
/// deliberately **not** included.
#[must_use]
pub fn receive_group_set() -> Vec<IpAddr> {
    vec![
        IpAddr::V4(SAP_V4_GLOBAL),
        IpAddr::V4(SAP_V4_ORG_LOCAL),
        IpAddr::V4(SAP_V4_LOCAL),
        IpAddr::V4(SAP_V4_LINK_LOCAL),
        IpAddr::V6(SAP_V6_SITE_LOCAL),
    ]
}

/// Select the SAP group to **announce on** from the scope of the `media` group
/// (RFC 2365 admin scoping / RFC 2974 §3 — the top address of the scope zone),
/// never from a TTL.
///
/// IPv4: `239.255.0.0/16` → [`SAP_V4_LOCAL`], `239.192.0.0/14` →
/// [`SAP_V4_ORG_LOCAL`], `224.0.0.0/24` → [`SAP_V4_LINK_LOCAL`], else →
/// [`SAP_V4_GLOBAL`]. IPv6: `FF0X::2:7FFE` with `X` = the media address' scope
/// nibble.
#[must_use]
pub fn announce_group_for(media: MediaGroup) -> SapGroup {
    match media.addr() {
        IpAddr::V4(v4) => SapGroup(IpAddr::V4(sap_group_v4_for(v4))),
        IpAddr::V6(v6) => SapGroup(IpAddr::V6(sap_group_v6_for(v6))),
    }
}

/// The IPv4 SAP group for an IPv4 media address' admin scope (RFC 2365).
fn sap_group_v4_for(media: Ipv4Addr) -> Ipv4Addr {
    let [a, b, c, _] = media.octets();
    if a == 239 && b == 255 {
        SAP_V4_LOCAL
    } else if a == 239 && (192..=195).contains(&b) {
        // Organization-local scope 239.192.0.0/14 (second octet 192..=195).
        SAP_V4_ORG_LOCAL
    } else if a == 224 && b == 0 && c == 0 {
        SAP_V4_LINK_LOCAL
    } else {
        SAP_V4_GLOBAL
    }
}

/// The IPv6 SAP group for an IPv6 media address, mirroring its scope nibble.
fn sap_group_v6_for(media: Ipv6Addr) -> Ipv6Addr {
    // The scope is the low nibble of the address' second byte (IPv6 multicast
    // format: byte 0 = 0xFF, byte 1 = flags<<4 | scope).
    let [_, byte1, ..] = media.octets();
    ipv6_sap_group(byte1 & 0x0f)
}

/// Build the IPv6 SAP group `FF0X::2:7FFE` for scope nibble `X` (RFC 2974 §3).
#[must_use]
pub fn ipv6_sap_group(scope_nibble: u8) -> Ipv6Addr {
    Ipv6Addr::new(0xff00 | u16::from(scope_nibble), 0, 0, 0, 0, 0, 2, 0x7ffe)
}
