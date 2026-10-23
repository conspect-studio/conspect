//! Typed multicast **address model** + a total RFC-5771 classifier (MC-0;
//! ADR-0040 §8).
//!
//! This is the pure, default-build validation seam for IP multicast. It answers
//! one question — *is this address a usable multicast media group, and if not,
//! why?* — at config load, before any socket is opened, so a media group in the
//! control block (`224.0.0.0/24`) or a reserved SSM head (`232.0.0.0`) fails
//! with an actionable reason instead of silently receiving nothing on the wire.
//!
//! ## What is accepted
//!
//! * IPv6 (primary, ADR-0042): a `ff00::/8` group whose flags nibble marks
//!   source-specific multicast (`ff3x::/32`, SSM) or whose scope nibble is one of
//!   the assigned multicast scopes — site-local (`ff05::/16`), organization-local
//!   (`ff08::/16`), or global (`ff0e::/16`).
//! * IPv4 (legacy peer): the IANA/RFC-5771 blocks minus the control and reserved
//!   slices — in practice `224.0.1.0/24`, `224.2/16` (SDP/SAP), `224.3/16`
//!   and `224.4/16` (AD-HOC II), `232/8` (SSM), `233/8` (GLOP/AD-HOC III),
//!   and admin-scoped `239/8`.
//!
//! ## What is refused (and why)
//!
//! * [`RejectReason::LinkLocalControl`] — `224.0.0.0/24` (RFC 5771 §4, the
//!   local network control block) and the IPv6 link-local scope `ff02::/16`.
//!   These never route and are never a media group.
//! * [`RejectReason::InterfaceLocalControl`] — the IPv6 loopback-style scope
//!   `ff01::/16` (RFC 4291 §2.7).
//! * [`RejectReason::ReservedBlock`] — blocks IANA has never allocated for a
//!   media group: `232.0.0.0/24` (the reserved head of the SSM block,
//!   RFC 4607 §7), `233.252.0.0/24` (`MCAST-TEST-NET`, RFC 6676), the AD-HOC
//!   control block `224.0.2.0/16`, the reserved `224.5.0.0`–`231.255.255.255`
//!   and `234.0.0.0`–`238.255.255.255`, the IPv6 reserved scopes `0x0`, `0x3`,
//!   and `0xf` (plus unsupported and unassigned scopes), and
//!   `ff3x::4000:0000`.
//! * [`RejectReason::NotMulticast`] — anything outside `224.0.0.0/4` and
//!   `ff00::/8` (unicast, loopback, the unspecified address, IPv4-mapped forms).
//!
//! [`MulticastGroup::validate`] adds the one *conditional* rule: a
//! source-specific group (`232/8` or `ff3x::/32`) is accepted only with a
//! non-empty source list ([`RejectReason::SourcesRequired`]) — an SSM join with
//! no source is an any-source join that receives nothing, the classic silent
//! `232.x` misconfiguration.
//!
//! ## One table, two readers
//!
//! The IPv4 registry is data ([`V4_BLOCKS`]) rather than control flow, and
//! `classify_v4` is a longest-prefix lookup over it. The integration test reads
//! the **same** table to verify lookup behavior, while independent literal
//! RFC-5771 endpoint expectations pin the table's contents. The IPv6
//! side is small enough (a flags nibble and a scope nibble) to stay a `match`,
//! with a property test over all 256 nibble pairs.
//!
//! ## The deliberate IPv6 conservatism
//!
//! RFC 7346 assigns scope `0x3` as realm-local (for specified network
//! technologies) and `0x4` as admin-local; this media classifier conservatively
//! declines both, since its supported media scopes are site-local (`0x5`),
//! organization-local (`0x8`) and global (`0xe`). Scopes `0x6`, `0x7`, and
//! `0x9`–`0xd` are unassigned. The **flags** nibble `0x3` identifies SSM but
//! does not override any of these scope restrictions.
//!
//! IPv6 is first-class here: `ff02::1%eth0`-style zone-qualified literals are
//! the [`MulticastIface`] form, and an IPv4-mapped IPv6 address
//! (`::ffff:239.1.1.1`) is deliberately **not** treated as an IPv4 multicast
//! group — the family a document wrote is the family that is validated.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A validated IPv4/IPv6 multicast media group.
///
/// Constructing one is the validation: [`TryFrom<IpAddr>`](Self::try_from),
/// [`TryFrom<Ipv4Addr>`], [`TryFrom<Ipv6Addr>`] and [`FromStr`] all run the
/// total RFC-5771 classifier and refuse a control, reserved, or non-multicast
/// address with a [`RejectedGroup`] carrying the reason. Serialises as the bare
/// address string, never a nested object — the "typed newtype, never a raw
/// string on the wire" rule (ADR-0040 §1) applies to *structure*, and the
/// address itself is its canonical literal.
///
/// A group is not yet *bound*: [`validate`](Self::validate) adds the SSM
/// source-list rule, and the transport (UDP vs RTP) / port / interface live in
/// the typed options that arrive with MC-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MulticastGroup {
    addr: IpAddr,
    class: MulticastClass,
}

impl MulticastGroup {
    /// Validate `addr` and wrap it as a media group.
    ///
    /// # Errors
    ///
    /// Returns [`RejectedGroup`] when `addr` is not a usable multicast media
    /// group (see the [module docs](self) for the accepted/refused blocks).
    pub fn new(addr: IpAddr) -> Result<Self, RejectedGroup> {
        classify(addr)
            .map(|class| Self { addr, class })
            .ok_or_else(|| reject(addr))
    }

    /// The wrapped address.
    #[must_use]
    pub const fn addr(self) -> IpAddr {
        self.addr
    }

    /// How this group classifies (SSM, admin-scoped, or globally routed).
    ///
    /// Stored at construction, so there is no fallback branch to hide a
    /// mis-classified value.
    #[must_use]
    pub const fn class(self) -> MulticastClass {
        self.class
    }

    /// Whether this group needs a source list (SSM: `232/8`, `ff3x::/32`).
    #[must_use]
    pub const fn requires_sources(self) -> bool {
        matches!(self.class, MulticastClass::SourceSpecific)
    }

    /// Whether this group is usable as a **media** group at all — always true
    /// for a constructed value, and stated explicitly so callers do not pattern
    /// off the [class](Self::class) enum.
    #[must_use]
    pub const fn is_usable_as_media_group(self) -> bool {
        // Only one state exists: a constructed group passed the classifier.
        true
    }

    /// The conditional rule an already-classified group still has to satisfy:
    /// a source-specific (SSM) group requires a non-empty `sources` list.
    ///
    /// Returns the [`RejectReason`] to surface when the group is unusable as
    /// configured, or `None` when every rule passes.
    #[must_use]
    pub fn validate(self, sources: &[IpAddr]) -> Option<RejectReason> {
        if self.requires_sources() && sources.is_empty() {
            Some(RejectReason::SourcesRequired)
        } else {
            None
        }
    }
}

impl TryFrom<IpAddr> for MulticastGroup {
    type Error = RejectedGroup;

    fn try_from(addr: IpAddr) -> Result<Self, Self::Error> {
        Self::new(addr)
    }
}

impl TryFrom<Ipv4Addr> for MulticastGroup {
    type Error = RejectedGroup;

    fn try_from(addr: Ipv4Addr) -> Result<Self, Self::Error> {
        Self::new(IpAddr::V4(addr))
    }
}

impl TryFrom<Ipv6Addr> for MulticastGroup {
    type Error = RejectedGroup;

    fn try_from(addr: Ipv6Addr) -> Result<Self, Self::Error> {
        Self::new(IpAddr::V6(addr))
    }
}

impl From<MulticastGroup> for String {
    fn from(group: MulticastGroup) -> Self {
        group.addr.to_string()
    }
}

impl TryFrom<String> for MulticastGroup {
    type Error = RejectedGroup;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::from_str(&text)
    }
}

impl FromStr for MulticastGroup {
    type Err = RejectedGroup;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let addr = IpAddr::from_str(text).map_err(|_| RejectedGroup::Parse {
            value: text.to_owned(),
            reason: RejectReason::Parse,
        })?;
        Self::new(addr)
    }
}

impl fmt::Display for MulticastGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.addr.fmt(f)
    }
}

/// A local interface to send from / join on: an IPv6 literal with an optional
/// zone, or an IPv4 literal.
///
/// The transport field is deliberately **not** a free-form NIC name (libav's
/// `localaddr=` wants an address literal), so `eth0` is refused and
/// `fe80::1%eth0` / `ff02::1%eth0` / `10.20.0.5` are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MulticastIface {
    addr: IpAddr,
    zone: Option<String>,
}

impl MulticastIface {
    /// The interface address literal (the zone is carried separately).
    #[must_use]
    pub const fn addr(&self) -> IpAddr {
        self.addr
    }

    /// The IPv6 zone/scope name (`eth0` in `ff02::1%eth0`), if one was given.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.zone.as_deref()
    }
}

impl FromStr for MulticastIface {
    type Err = RejectedGroup;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (addr_text, zone) = match text.split_once('%') {
            Some((addr, zone)) => (addr, Some(zone)),
            None => (text, None),
        };
        let addr = IpAddr::from_str(addr_text).map_err(|_| RejectedGroup::Parse {
            value: text.to_owned(),
            reason: RejectReason::Parse,
        })?;
        // A zone only means anything for a scoped IPv6 literal; `fe80::1%eth0`
        // and `ff02::1%eth0` are the real forms, a bare IPv4 zone is a typo, and
        // an empty `%` names nothing.
        if let Some(zone) = zone {
            if !matches!(addr, IpAddr::V6(_)) || zone.is_empty() {
                return Err(RejectedGroup::Parse {
                    value: text.to_owned(),
                    reason: RejectReason::Parse,
                });
            }
        }
        Ok(Self {
            addr,
            zone: zone.map(str::to_owned),
        })
    }
}

impl From<MulticastIface> for String {
    fn from(iface: MulticastIface) -> Self {
        iface.to_string()
    }
}

impl TryFrom<String> for MulticastIface {
    type Error = RejectedGroup;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::from_str(&text)
    }
}

impl fmt::Display for MulticastIface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.addr)?;
        if let Some(zone) = &self.zone {
            write!(f, "%{zone}")?;
        }
        Ok(())
    }
}

/// A count of 188-byte TS packets — libav's `fifo_size=` unit.
///
/// A newtype, not a bare integer, so a byte-size and a packet-count can never be
/// swapped at a call site (the classic `fifo_packets`-vs-`buffer_bytes` bug) and
/// the unit is stated by the type rather than a field-name convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PacketCount(u64);

impl PacketCount {
    /// Wrap a packet count.
    #[must_use]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// The raw count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for PacketCount {
    fn from(count: u64) -> Self {
        Self(count)
    }
}

impl fmt::Display for PacketCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The kind of multicast media group an address is, or `None` when the address
/// is not an accepted media group (see [`classify`]).
///
/// This is the full RFC-5771 vocabulary, not just accept/reject: the ingest path
/// needs to know an SSM group from an ASM one to require `sources`, and the
/// warning path (MC-6/MC-7) needs the `239/8`-vs-`232/8` admin-scope guidance
/// ADR-0040 §8 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum MulticastClass {
    /// Source-specific multicast — IPv4 `232/8`, IPv6 `ff3x::/32`. Requires a
    /// non-empty source list (`MLDv2` `INCLUDE` / `IGMPv3`).
    SourceSpecific,
    /// Admin-scoped ASM — IPv4 `239/8` (RFC 2365), IPv6 site-local `ff05::/16`.
    /// The preferred in-plant ASM range.
    AdminScoped,
    /// Routed group — IPv4 `224.0.1.x`/`224.2.x`/`224.3.x`/`224.4.x`/`233/8`,
    /// IPv6 organization-local (`ff08`) and global (`ff0e`).
    Routed,
}

/// Why an address or interface literal was refused as a multicast media group.
///
/// The variants are what the operator is told; each is a distinct remediation
/// (pick another range, add sources, use a literal, fix the typo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RejectReason {
    /// The literal is not a multicast group address at all (unicast, loopback,
    /// the unspecified address, an IPv4-mapped form).
    NotMulticast,
    /// `224.0.0.0/24` or IPv6 link-local `ff02::/16` — local-network control,
    /// never a media group.
    LinkLocalControl,
    /// IPv6 interface-local `ff01::/16` — control, never a media group.
    InterfaceLocalControl,
    /// A block IANA has not allocated for media (reserved control slices, the
    /// IPv6 reserved scopes, the reserved SSM heads).
    ReservedBlock,
    /// A source-specific (`232/8`, `ff3x::/32`) group configured with no
    /// sources.
    SourcesRequired,
    /// The string was not an address (or interface) literal at all.
    Parse,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotMulticast => "not a multicast address",
            Self::LinkLocalControl => {
                "link-local control block (224.0.0.0/24, ff02::/16) is not a media group"
            }
            Self::InterfaceLocalControl => {
                "interface-local control block (ff01::/16) is not a media group"
            }
            Self::ReservedBlock => "reserved block, not allocated for media",
            Self::SourcesRequired => "a source-specific (232/8, ff3x::/32) group requires sources",
            Self::Parse => "not a valid address literal",
        })
    }
}

/// A refused multicast group: either the literal was not an address, or the
/// address failed the RFC-5771 classifier.
///
/// Implements [`std::error::Error`] so a config-load validator (and the CLI) can
/// carry it in the crate error taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RejectedGroup {
    /// A literal that failed to parse as an address or interface.
    #[error("multicast address {value:?}: {reason}")]
    Parse {
        /// The offending literal.
        value: String,
        /// Why it was refused (always [`RejectReason::Parse`]).
        reason: RejectReason,
    },
    /// A non-multicast address.
    #[error("{addr} is rejected as a multicast group: {reason}")]
    NotMulticast {
        /// The offending address.
        addr: IpAddr,
        /// Why it was refused (always [`RejectReason::NotMulticast`]).
        reason: RejectReason,
    },
    /// A multicast address in a control or reserved block.
    #[error("{addr} is rejected as a multicast group: {reason}")]
    Rejected {
        /// The offending address.
        addr: IpAddr,
        /// The class the address would otherwise have had, if any.
        class: Option<MulticastClass>,
        /// Why it was refused.
        reason: RejectReason,
    },
}

impl RejectedGroup {
    /// The reason an operator can act on.
    #[must_use]
    pub const fn reason(&self) -> RejectReason {
        match self {
            Self::Parse { .. } => RejectReason::Parse,
            Self::NotMulticast { .. } => RejectReason::NotMulticast,
            Self::Rejected { reason, .. } => *reason,
        }
    }

    /// The offending address, when one was parsed.
    #[must_use]
    pub const fn addr(&self) -> Option<IpAddr> {
        match self {
            Self::Parse { .. } => None,
            Self::NotMulticast { addr, .. } | Self::Rejected { addr, .. } => Some(*addr),
        }
    }
}

/// Build the refusal for an address that did not classify.
fn reject(addr: IpAddr) -> RejectedGroup {
    match reason_for(addr) {
        RejectReason::NotMulticast => RejectedGroup::NotMulticast {
            addr,
            reason: RejectReason::NotMulticast,
        },
        reason => RejectedGroup::Rejected {
            addr,
            class: None,
            reason,
        },
    }
}

/// Classify `addr` as an accepted multicast media group, or `None` when it is
/// not one (non-multicast, or a control/reserved block).
///
/// Total: every [`IpAddr`] gets an answer, never a panic. Use
/// [`MulticastGroup::try_from`] when the *reason* is needed.
#[must_use]
pub fn classify(addr: IpAddr) -> Option<MulticastClass> {
    match addr {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

/// One contiguous IPv4 multicast block: the network address as a `u32`, the
/// prefix length, and the classification every member shares (`None` = withheld
/// from media). Longest-prefix match, so a narrow reservation carves out of the
/// wider block it sits inside.
pub type V4Block = (u32, u8, Option<MulticastClass>);

/// The IPv4 multicast registry as data — the single source of truth
/// [`classify_v4`] *and* `tests/multicast_address.rs` read (the test imports
/// this, so there is no second hand-written table to drift).
///
/// Derived from the IANA IPv4 Multicast Address Space Registry / RFC 5771 §3.
/// Each block below cites its registry designation; reserved blocks are never
/// usable media groups. `239/8` is the RFC 2365 admin-scoped block.
pub const V4_BLOCKS: &[V4Block] = &[
    // RFC 5771 §3/§4: Local Network Control, 224.0.0.0–224.0.0.255.
    (0xe0_00_00_00, 24, None),
    // RFC 5771 §3/§5: Internetwork Control, 224.0.1.0–224.0.1.255.
    (0xe0_00_01_00, 24, Some(MulticastClass::Routed)),
    // RFC 5771 §3/§6: AD-HOC I, 224.0.2.0–224.0.255.255;
    // RFC 5771 §3: reserved, 224.1.0.0–224.1.255.255.
    (0xe0_00_02_00, 16, None),
    (0xe0_01_00_00, 16, None),
    // RFC 5771 §3/§7: SDP/SAP, 224.2.0.0–224.2.255.255.
    (0xe0_02_00_00, 16, Some(MulticastClass::Routed)),
    // RFC 5771 §3/§6: AD-HOC II, 224.3.0.0–224.4.255.255.
    // Separate aligned /16s: 224.3.0.0/15 would also include 224.2/16.
    (0xe0_03_00_00, 16, Some(MulticastClass::Routed)),
    (0xe0_04_00_00, 16, Some(MulticastClass::Routed)),
    // RFC 5771 §3 / IANA registry: 224.5.0.0–224.255.255.255 RESERVED.
    (0xe0_00_00_00, 8, None),
    // RFC 5771 §3 / IANA registry: 225.0.0.0–231.255.255.255 RESERVED.
    (0xe1_00_00_00, 8, None),
    (0xe2_00_00_00, 7, None),
    (0xe4_00_00_00, 6, None),
    (0xe8_00_00_00, 5, None),
    // RFC 5771 §3/§8: SSM, 232.0.0.0–232.255.255.255.
    (0xe8_00_00_00, 8, Some(MulticastClass::SourceSpecific)),
    // IANA SSM reserved head, 232.0.0.0–232.0.0.255 (RFC 4607 §7).
    (0xe8_00_00_00, 24, None),
    // RFC 5771 §3/§9: GLOP, 233.0.0.0–233.251.255.255;
    // RFC 5771 §3/§9.2: AD-HOC III, 233.252.0.0–233.255.255.255.
    (0xe9_00_00_00, 8, Some(MulticastClass::Routed)),
    // RFC 5771 §9.2: MCAST-TEST-NET, 233.252.0.0–233.252.0.255.
    (0xe9_fc_00_00, 24, None),
    // RFC 5771 §3 / IANA registry: 234.0.0.0–238.255.255.255 RESERVED.
    (0xea_00_00_00, 7, None),
    (0xec_00_00_00, 7, None),
    (0xee_00_00_00, 8, None),
    // RFC 5771 §3/§10: admin-scoped, 239.0.0.0–239.255.255.255.
    (0xef_00_00_00, 8, Some(MulticastClass::AdminScoped)),
];

/// The RFC-5771 IPv4 classifier: longest-prefix match over [`V4_BLOCKS`].
///
/// The table carries no two rows with the same base and prefix, so the match is
/// order-independent.
fn classify_v4(addr: Ipv4Addr) -> Option<MulticastClass> {
    let bits = addr.to_bits();
    let mut best: Option<(u8, Option<MulticastClass>)> = None;
    for (base, prefix, class) in V4_BLOCKS {
        if in_net_v4_bits(bits, *base, *prefix) && best.is_none_or(|(widest, _)| *prefix > widest) {
            best = Some((*prefix, *class));
        }
    }
    best.and_then(|(_, class)| class)
}

/// Whether `bits` (an IPv4 address in network order) falls inside `base/prefix`.
fn in_net_v4_bits(bits: u32, base: u32, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (bits & mask) == (base & mask)
}

/// The RFC-4291 §2.7 IPv6 classifier, keyed on the **flags** nibble (SSM) and
/// otherwise the **scope** nibble.
///
/// The scope is validated **before** the SSM flag: RFC 4291 §2.7 / RFC 7346
/// do not let `ff3x` override an interface-local, link-local or reserved scope.
/// Flags `0x3` with a zero plen and prefix mark SSM (`ff3x::/32`,
/// RFC 3306 §6 / RFC 4607 §1); other valid prefix-based groups use their scope.
/// Otherwise site-local (`0x5`) is the admin-scoped media scope,
/// organization-local (`0x8`) and global (`0xe`) are routed, and every other
/// nibble — reserved `0x0`/`0xf`, interface-local (`0x1`), link-local (`0x2`),
/// unsupported realm-local (`0x3`)/admin-local (`0x4`), and unassigned
/// `0x6`/`0x7`/`0x9`–`0xd` — is refused.
fn classify_v6(addr: Ipv6Addr) -> Option<MulticastClass> {
    if !addr.is_multicast() {
        return None;
    }
    let [_, nibbles, ..] = addr.octets();
    let flags = nibbles >> 4;
    let scope = nibbles & 0x0f;
    // RFC 4291 §2.7 / RFC 7346: an SSM flag never legitimizes a
    // prohibited or unassigned scope (including ff30, ff31, ff32).
    if !matches!(scope, 0x5 | 0x8 | 0xe) {
        return None;
    }
    if flags == 0x3 {
        // RFC 3306 §4 as updated by RFC 7371 §4.1: the low four bits of
        // octet 2 are reserved and must be zero, and plen cannot exceed 64.
        // RFC 7371 §3/§4.1 requires ignoring ff2 (the high four bits) on
        // receipt; do not treat it as part of the reserved field.
        let octets = addr.octets();
        if octets[2] & 0x0f != 0 || octets[3] > 64 {
            return None;
        }
        // RFC 3306 §6 and RFC 4607 §1: only zero plen and zero network
        // prefix denote ff3x::/32 SSM. Prefix-based groups are ASM.
        if octets[2..12].iter().all(|byte| *byte == 0) {
            return (!is_reserved_ssm_group_id(addr)).then_some(MulticastClass::SourceSpecific);
        }
    }
    // Only the scopes the standards name are media groups: site-local (0x5),
    // organization-local (0x8), and global (0xe). Every other nibble is either
    // reserved (0x0/0xf), local control (0x1/0x2), unsupported realm-local
    // (0x3)/admin-local (0x4), or unassigned (0x6/0x7/0x9–0xd).
    match scope {
        0x5 => Some(MulticastClass::AdminScoped),
        0x8 | 0xe => Some(MulticastClass::Routed),
        _ => None,
    }
}

/// RFC 4607 §1 and RFC 3307 §§4.1–4.3: SSM group IDs `0–0x3fff_ffff`
/// belong to permanent addresses (P=T=0), `0x4000_0000–0x7fff_ffff`
/// are permanent IDs reserved for IANA (including the null ID), and only
/// `0x8000_0000–0xffff_ffff` are available for dynamic SSM allocation.
/// See <https://www.rfc-editor.org/rfc/rfc4607#section-1> and
/// <https://www.rfc-editor.org/rfc/rfc3307#section-4>.
fn is_reserved_ssm_group_id(addr: Ipv6Addr) -> bool {
    let [.., b12, b13, b14, b15] = addr.octets();
    u32::from_be_bytes([b12, b13, b14, b15]) < 0x8000_0000
}

/// The [`RejectReason`] for an address that [`classify`] refused.
fn reason_for(addr: IpAddr) -> RejectReason {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            if !v4.is_multicast() {
                RejectReason::NotMulticast
            } else if a == 224 && b == 0 && c == 0 {
                // 224.0.0.0/24 — the local network control block.
                RejectReason::LinkLocalControl
            } else {
                RejectReason::ReservedBlock
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_multicast() {
                let [_, nibbles, ..] = v6.octets();
                let scope = nibbles & 0x0f;
                // The reason is scope-specific even when the SSM flag is set.
                match scope {
                    0x1 => RejectReason::InterfaceLocalControl,
                    0x2 => RejectReason::LinkLocalControl,
                    _ => RejectReason::ReservedBlock,
                }
            } else {
                RejectReason::NotMulticast
            }
        }
    }
}
