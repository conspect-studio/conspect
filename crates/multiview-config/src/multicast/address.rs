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
//!   slices — in practice `224.0.1.0/24`, the `224.2.x` SAP blocks, the ASM
//!   bulk, `232/8` (SSM), `233/8`, `234/7`, and the admin-scoped `239/8`.
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
//!   RFC 4607 §7), `233.252.0.0/24` (`MCAST-TEST-NET`, RFC 6676), the
//!   interface-local control `224.0.2.0/23`, the `236/7`/`238/8` reservations,
//!   the IPv6 reserved scopes `0x0` and `0xf`, and `ff3x::4000:0000`.
//! * [`RejectReason::NotMulticast`] — anything outside `224.0.0.0/4` and
//!   `ff00::/8` (unicast, loopback, the unspecified address, IPv4-mapped forms).
//!
//! [`MulticastGroup::validate`] adds the one *conditional* rule: a
//! source-specific group (`232/8` or `ff3x::/32`) is accepted only with a
//! non-empty source list ([`RejectReason::SourcesRequired`]) — an SSM join with
//! no source is an any-source join that receives nothing, the classic silent
//! `232.x` misconfiguration.
//!
//! ## The deliberate IPv6 conservatism
//!
//! The IPv6 scopes `0x4`, `0x6`, `0x7`, and `0x9`–`0xd` are unassigned as media
//! scopes, and scope `0x3` is the reserved slot the old IPv4-style realm-local
//! assignment used (SSM is carried by the **flags** nibble `0x3` instead). They
//! are refused rather than waved through: an admin-scoped group belongs in
//! site-local `ff05::/16` or organization-local `ff08::/16`, so a group in an
//! unassigned nibble is a typo, and admitting it would defeat the point of
//! validating the address at config load.
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
    /// Globally routed group — the ASM bulk, IPv4 `224.0.1.x`/`224.2.x`/`233/8`/
    /// `234/7`, IPv6 organization-local (`ff08`) and global (`ff0e`).
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

/// Classify `addr` as a multicast media group, or `None` when it is not a
/// multicast address at all.
///
/// Total: every [`IpAddr`] is either accepted with a class or refused (`None`).
/// Use [`MulticastGroup::try_from`] when the *reason* is needed.
#[must_use]
pub fn classify(addr: IpAddr) -> Option<MulticastClass> {
    match addr {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

/// The RFC-5771 IPv4 classifier. `None` marks a block withheld from media.
fn classify_v4(addr: Ipv4Addr) -> Option<MulticastClass> {
    let [a, b, c, _] = addr.octets();
    if a == 224 && b == 0 {
        // 224.0.0.0/24 local network control; 224.0.1.0/24 internetwork
        // control (accepted); 224.0.2.0/23 AD-HOC Block I (control-reserved).
        return if c == 0x01 {
            Some(MulticastClass::Routed)
        } else {
            None
        };
    }
    if a == 232 {
        // 232.0.0.0/24 is reserved (RFC 4607 §7); 232.1/16..232/8 is SSM.
        return if b == 0 {
            None
        } else {
            Some(MulticastClass::SourceSpecific)
        };
    }
    if a == 233 && b == 252 {
        // 233.252.0.0/24 MCAST-TEST-NET (RFC 6676) is reserved.
        return None;
    }
    if a == 236 || a == 237 || a == 238 {
        // IANA reservations within 224.0.0.0/4 (the 236.0.0.0/7 block and
        // 238.0.0.0/8).
        return None;
    }
    if (224..=239).contains(&a) {
        return Some(if a == 239 {
            MulticastClass::AdminScoped
        } else {
            MulticastClass::Routed
        });
    }
    None
}

/// The RFC-4291 §2.7 IPv6 classifier, keyed on the **flags** nibble (SSM) and
/// otherwise the **scope** nibble.
///
/// The flags nibble `0x3` marks source-specific multicast (`ff3x::/32`,
/// RFC 4607 §7) and is checked **first**: it is what makes `ff3e::1` SSM even
/// though its scope (`0xe`) is global. Everything else is a pure function of the
/// scope: the reserved scopes (`0x0`, `0xf`), interface-local (`0x1`) and
/// link-local (`0x2`) control, the admin scopes (`0x4`–`0x7`, `0x9`–`0xd`),
/// organization-local (`0x8`), and global (`0xe`). Scope `0x3` without the SSM
/// flags is itself reserved.
fn classify_v6(addr: Ipv6Addr) -> Option<MulticastClass> {
    if !addr.is_multicast() {
        return None;
    }
    if is_reserved_ssm_subrange(addr) {
        // FF3X::4000:0000–FF3X::7FFF:FFFF is withheld from SSM (RFC 4607 §7).
        return None;
    }
    let [_, nibbles, ..] = addr.octets();
    let flags = nibbles >> 4;
    let scope = nibbles & 0x0f;
    if flags == 0x3 {
        return Some(MulticastClass::SourceSpecific);
    }
    // Only the scopes the standards name are media groups: site-local (0x5),
    // organization-local (0x8), and global (0xe). Every other nibble is either
    // reserved — scope 0x0, the interface-local 0x1 and link-local 0x2 control
    // scopes, scope 0x3 without the SSM flags, the reserved 0xf — or unassigned
    // (0x4, 0x6, 0x7, 0x9–0xd), so a group in one of them is a typo.
    match scope {
        0x5 => Some(MulticastClass::AdminScoped),
        0x8 | 0xe => Some(MulticastClass::Routed),
        _ => None,
    }
}

/// The reserved head of the IPv6 SSM block: `ff3x::4000:0000` through
/// `ff3x::7fff:ffff` (RFC 4607 §7 / the IANA SSM registry), i.e. a flags nibble
/// of `0x3` whose trailing 32-bit field has its two top bits set to `0b01`.
fn is_reserved_ssm_subrange(addr: Ipv6Addr) -> bool {
    let [_, nibbles, .., b12, b13, b14, b15] = addr.octets();
    if (nibbles >> 4) != 0x3 {
        return false;
    }
    // In `ff3x::4000:0000` notation the reserved field is the trailing 32 bits
    // (`::4000:0000`–`::7fff:ffff`), i.e. address octets 12..16.
    let field = u32::from_be_bytes([b12, b13, b14, b15]);
    (field >> 30) == 0b01
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
                let flags = nibbles >> 4;
                let scope = nibbles & 0x0f;
                if flags == 0x3 {
                    // Either the reserved SSM sub-range or a non-media scope;
                    // a usable SSM group would have been accepted already.
                    RejectReason::ReservedBlock
                } else {
                    match scope {
                        0x1 => RejectReason::InterfaceLocalControl,
                        0x2 => RejectReason::LinkLocalControl,
                        _ => RejectReason::ReservedBlock,
                    }
                }
            } else {
                RejectReason::NotMulticast
            }
        }
    }
}
