//! Shared outbound-dial address guard — SSRF defence (SEC-02/SEC-04,
//! CWE-918/CWE-20).
//!
//! Managed devices ([`Zowietek`](super::DeviceDriver::Zowietek)) and ad-hoc
//! Cast sessions are dialled by the control plane at an operator-supplied
//! address. Without validation a Write-role principal can point that dial at
//! the cloud-metadata endpoint (169.254.169.254), a loopback service, or an
//! internal LAN host — an authenticated SSRF that also exfiltrates the device's
//! stored credentials in cleartext. This module is the single screen both the
//! config-load validator ([`super::Device::validate`]) and the control-plane
//! dial sites consult.
//!
//! ## Two layers
//!
//! 1. **Config load ([`screen_config_literal`])** — deployment-independent and
//!    offline. It rejects the **never-legitimate** literal dial targets
//!    (loopback, link-local incl. IMDS, unspecified, multicast/broadcast, and
//!    their IPv4-mapped forms) and — for device-management URLs — enforces the
//!    `http`/`https` [scheme allowlist](parse_device_address). Private/ULA LAN
//!    literals are **accepted** here: a managed device legitimately lives on the
//!    LAN, so whether its private address may be dialled is a runtime policy
//!    decision, not a config-syntax one.
//! 2. **Dial time ([`screen_ip`] / [`screen_resolved`])** — screens the
//!    **resolved** IP against the [`DialPolicy`]. The default is **non-breaking**
//!    (a self-hosted LAN appliance keeps reaching its devices out of the box):
//!    every *allowlistable* range (private/ULA/carrier) is dialable, while the
//!    never-legitimate ranges (loopback, link-local incl. IMDS, unspecified,
//!    multicast/broadcast) are **always** blocked — no default and no operator
//!    allowlist re-enables them. An operator allowlist **tightens** the dial to
//!    its own device subnet(s). Screening the resolved IP (not the hostname) is
//!    the only way to defeat DNS-rebind: a public name that answers with a
//!    loopback/metadata address is rejected once resolved, unconditionally; with
//!    an allowlist set, a rebind to any out-of-subnet private range is caught
//!    too. The caller then dials the vetted IP it screened.
//!
//! IPv6-first (ADR-0042): bracketed IPv6 URL/authority literals are handled, and
//! IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is unwrapped before classification so it
//! cannot smuggle a blocked IPv4 target past the screen.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;
use thiserror::Error;

/// Why an address was refused for an outbound dial.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddressError {
    /// The address string is empty.
    #[error("address is empty")]
    Empty,
    /// A device-management URL used a scheme outside the `http`/`https`
    /// allowlist (e.g. `file:`, `gopher:`, `ftp:`).
    #[error("address {address:?} must be an http:// or https:// URL")]
    Scheme {
        /// The rejected address.
        address: String,
    },
    /// The address carried no host (e.g. `http:///path`, `[]:8009`).
    #[error("address {address:?} has no host")]
    NoHost {
        /// The rejected address.
        address: String,
    },
    /// A Cast authority was not a well-formed `host[:port]`.
    #[error("address {address:?} is not a valid host[:port]")]
    Authority {
        /// The rejected address.
        address: String,
    },
    /// The dial target resolves to a blocked address (SSRF guard).
    #[error(
        "dial target {ip} is a blocked {class} address (SSRF guard); \
         only globally-routable hosts are dialled{}",
        if class.allowlistable() {
            " unless the operator adds its subnet to control.device_dial_allow"
        } else {
            ""
        }
    )]
    Blocked {
        /// The blocked resolved IP.
        ip: IpAddr,
        /// Which reserved range it fell in.
        class: BlockClass,
    },
    /// An operator allowlist CIDR did not parse.
    #[error("dial allowlist entry {cidr:?} is not a valid CIDR")]
    Cidr {
        /// The rejected CIDR string.
        cidr: String,
    },
    /// The host produced no resolvable address, or resolution failed.
    #[error("host {host:?} did not resolve to a dialable address: {reason}")]
    Unresolvable {
        /// The host that failed to resolve.
        host: String,
        /// The underlying reason.
        reason: String,
    },
}

/// The reserved range a blocked address fell in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockClass {
    /// Loopback (`127.0.0.0/8`, `::1`).
    Loopback,
    /// Link-local (`169.254.0.0/16` incl. cloud-metadata `169.254.169.254`,
    /// `fe80::/10`).
    LinkLocal,
    /// The unspecified address (`0.0.0.0/8`, `::`).
    Unspecified,
    /// Multicast (`224.0.0.0/4`, `ff00::/8`).
    Multicast,
    /// The IPv4 limited broadcast address (`255.255.255.255`).
    Broadcast,
    /// RFC 1918 private space (`10/8`, `172.16/12`, `192.168/16`).
    Private,
    /// IPv6 unique-local addresses (`fc00::/7`).
    UniqueLocal,
    /// RFC 6598 shared / carrier-grade-NAT space (`100.64.0.0/10`).
    Carrier,
}

impl BlockClass {
    /// Whether an operator [`DialPolicy`] allowlist may re-enable this class.
    ///
    /// Only the LAN ranges a real device might sit in are allowlistable. The
    /// never-legitimate ranges (loopback, link-local incl. the cloud-metadata
    /// IP, unspecified, multicast, broadcast) stay blocked no matter what an
    /// operator lists — dialling them is never a legitimate device operation.
    #[must_use]
    pub const fn allowlistable(self) -> bool {
        matches!(self, Self::Private | Self::UniqueLocal | Self::Carrier)
    }

    /// A short human token for the error message.
    const fn token(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Private => "private",
            Self::UniqueLocal => "unique-local",
            Self::Carrier => "carrier-grade-NAT",
        }
    }
}

impl std::fmt::Display for BlockClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.token())
    }
}

/// The outbound-dial policy: whether an *allowlistable* (private/ULA/carrier)
/// resolved IP may be dialled. The never-legitimate ranges are refused
/// regardless — see [`BlockClass::allowlistable`].
///
/// The default ([`DialPolicy::default`] / [`DialPolicy::allow_lan`]) is
/// **non-breaking**: `allow` is [`None`], so every allowlistable LAN range is
/// dialable and a self-hosted appliance keeps reaching its devices out of the
/// box. [`DialPolicy::from_cidrs`] — built from `control.device_dial_allow` —
/// sets `allow` to `Some(cidrs)`, which **tightens** the dial: an allowlistable
/// IP is then reachable only inside one of those CIDRs (locking the dial to the
/// real device subnet closes the authenticated internal-dial vector, SEC-04).
/// `Some([])` (an empty allowlist) denies every allowlistable range — the
/// strictest lock-down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialPolicy {
    /// `None` ⇒ allow every allowlistable range (the non-breaking default);
    /// `Some(cidrs)` ⇒ allow an allowlistable IP only inside one of `cidrs`.
    allow: Option<Vec<IpNet>>,
}

impl DialPolicy {
    /// The non-breaking default: allow every allowlistable (private/ULA/carrier)
    /// LAN range, while the never-legitimate ranges stay blocked. Equivalent to
    /// [`DialPolicy::default`].
    #[must_use]
    pub fn allow_lan() -> Self {
        Self { allow: None }
    }

    /// The strictest lock-down: deny every allowlistable (private/ULA/carrier)
    /// range too, dialling only globally-routable hosts. Equivalent to an empty
    /// operator allowlist (`Some([])`).
    ///
    /// This is the fail-closed fallback when an operator-supplied allowlist
    /// cannot be parsed: an operator who asked to *tighten* the dial is never
    /// silently widened back to the LAN default (which would ignore their
    /// tightening intent). The never-legitimate ranges are refused by
    /// [`screen_ip`] regardless.
    #[must_use]
    pub fn deny_all_allowlistable() -> Self {
        Self {
            allow: Some(Vec::new()),
        }
    }

    /// Build a policy from operator-declared CIDR strings (e.g.
    /// `["192.168.0.0/16", "fd00:db8::/32"]`) that **tightens** the dial: an
    /// allowlistable IP is dialable only inside one of these CIDRs. An empty
    /// list denies every allowlistable range (the strictest lock-down).
    ///
    /// A bare IP with no prefix length is accepted as a `/32` (IPv4) or `/128`
    /// (IPv6) host route.
    ///
    /// # Errors
    ///
    /// [`AddressError::Cidr`] naming the first entry that is not a valid CIDR or
    /// IP literal.
    pub fn from_cidrs<I, S>(cidrs: I) -> Result<Self, AddressError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allow = Vec::new();
        for entry in cidrs {
            let text = entry.as_ref().trim();
            let net = text
                .parse::<IpNet>()
                .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| AddressError::Cidr {
                    cidr: text.to_owned(),
                })?;
            allow.push(net);
        }
        Ok(Self { allow: Some(allow) })
    }

    /// Whether `ip` is permitted by the allowlist: the non-breaking default
    /// ([`None`]) permits any allowlistable range; a set allowlist permits only
    /// the CIDRs it lists. Never-legitimate ranges are refused by [`screen_ip`]
    /// before this is consulted, so this governs only allowlistable ranges.
    #[must_use]
    pub fn allows(&self, ip: IpAddr) -> bool {
        match &self.allow {
            None => true,
            Some(nets) => nets.iter().any(|net| net.contains(&ip)),
        }
    }
}

/// A parsed dial host: an IP literal (screened directly) or a DNS name
/// (screened after resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostSpec {
    /// An IP literal — screen it directly with [`screen_ip`] /
    /// [`screen_config_literal`].
    Ip(IpAddr),
    /// A DNS name — resolve it and screen the answer with [`screen_resolved`].
    Name(String),
}

/// Classify `ip` against the reserved ranges, unwrapping IPv4-mapped IPv6 first
/// so `::ffff:127.0.0.1` cannot bypass the IPv4 screen. Returns [`None`] for a
/// globally-routable address (documentation ranges are treated as routable —
/// the test suite uses them as public stand-ins).
#[must_use]
pub fn classify(ip: IpAddr) -> Option<BlockClass> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            // Native IPv6 specials first, before unwrapping any embedded IPv4
            // (so `::1` is loopback, not the `::/96`-embedded `0.0.0.1`).
            if v6.is_loopback() {
                return Some(BlockClass::Loopback);
            }
            if v6.is_unspecified() {
                return Some(BlockClass::Unspecified);
            }
            if v6.is_multicast() {
                return Some(BlockClass::Multicast);
            }
            if is_v6_link_local(v6) {
                return Some(BlockClass::LinkLocal);
            }
            if is_v6_unique_local(v6) {
                return Some(BlockClass::UniqueLocal);
            }
            // IPv4-mapped (`::ffff:a.b.c.d`) and the deprecated IPv4-compatible
            // (`::a.b.c.d`) forms embed an IPv4 target — classify it as IPv4.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return classify_v4(v4);
            }
            None
        }
    }
}

/// Classify an IPv4 address against the reserved ranges.
fn classify_v4(v4: Ipv4Addr) -> Option<BlockClass> {
    if v4.is_broadcast() {
        return Some(BlockClass::Broadcast);
    }
    // `0.0.0.0/8` ("this host on this network") — `is_unspecified` only covers
    // the exact `0.0.0.0`, so widen to the whole block.
    if v4.octets()[0] == 0 {
        return Some(BlockClass::Unspecified);
    }
    if v4.is_loopback() {
        return Some(BlockClass::Loopback);
    }
    if v4.is_link_local() {
        return Some(BlockClass::LinkLocal);
    }
    if v4.is_private() {
        return Some(BlockClass::Private);
    }
    if is_v4_carrier(v4) {
        return Some(BlockClass::Carrier);
    }
    if v4.is_multicast() {
        return Some(BlockClass::Multicast);
    }
    None
}

/// IPv6 link-local `fe80::/10` (stable-Rust-safe; `Ipv6Addr::is_unicast_link_local`
/// is still unstable).
fn is_v6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// IPv6 unique-local `fc00::/7` (covers `fc00::/8` and `fd00::/8`;
/// `Ipv6Addr::is_unique_local` is still unstable).
fn is_v6_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// RFC 6598 shared address space `100.64.0.0/10` (`Ipv4Addr::is_shared` is still
/// unstable).
fn is_v4_carrier(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    a == 100 && (64..=127).contains(&b)
}

/// Screen a resolved dial IP against the full policy (dial-time guard).
///
/// A globally-routable address passes. A never-legitimate range is always
/// refused; a private/ULA/carrier range is refused unless `policy` allowlists
/// it.
///
/// # Errors
///
/// [`AddressError::Blocked`] naming the IP and its class.
pub fn screen_ip(ip: IpAddr, policy: &DialPolicy) -> Result<(), AddressError> {
    match classify(ip) {
        None => Ok(()),
        Some(class) => {
            if class.allowlistable() && policy.allows(ip) {
                Ok(())
            } else {
                Err(AddressError::Blocked { ip, class })
            }
        }
    }
}

/// Screen a config-load literal dial target — the **deployment-independent**
/// layer. This is the SAME screen the dial sites run ([`screen_ip`]), evaluated
/// under the non-breaking default [`DialPolicy::allow_lan`], not a weaker one:
/// the never-legitimate ranges (loopback, link-local incl. IMDS, unspecified,
/// multicast/broadcast, IPv4-mapped forms) are refused, while private/ULA/carrier
/// LAN literals are accepted (a managed device legitimately lives on the LAN —
/// whether its private address may actually be *dialled* is the runtime
/// [`DialPolicy`] decision the dial sites apply, and can only ever *tighten*,
/// never re-admit a never-legitimate literal).
///
/// # Errors
///
/// [`AddressError::Blocked`] for a never-legitimate range.
pub fn screen_config_literal(ip: IpAddr) -> Result<(), AddressError> {
    // Config load has no operator dial policy yet, so it screens with the
    // permissive default — identical to the dial-time screen minus any operator
    // tightening (which never re-admits a never-legitimate literal anyway).
    screen_ip(ip, &DialPolicy::allow_lan())
}

/// Screen every IP a host resolved to, fail-closed: a single blocked answer
/// rejects the whole set (a DNS answer cannot smuggle a private target in among
/// public ones), and an empty answer is refused.
///
/// # Errors
///
/// [`AddressError::Blocked`] for the first blocked IP; [`AddressError::Empty`]
/// for an empty answer.
pub fn screen_resolved<I>(addrs: I, policy: &DialPolicy) -> Result<(), AddressError>
where
    I: IntoIterator<Item = IpAddr>,
{
    let mut any = false;
    for ip in addrs {
        any = true;
        screen_ip(ip, policy)?;
    }
    if any {
        Ok(())
    } else {
        Err(AddressError::Empty)
    }
}

/// A host-name resolver seam, so a caller can resolve a name and screen the
/// answer in one step ([`resolve_and_screen`]). An IP literal resolves to
/// itself.
pub trait HostResolver {
    /// Resolve `host` to its candidate IP addresses.
    ///
    /// # Errors
    ///
    /// An [`std::io::Error`] if resolution fails.
    fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>>;
}

/// Resolve `host` and screen every answer against `policy`, returning the vetted
/// addresses to dial. Screening the resolved answer (not the name) is what
/// defeats DNS-rebind.
///
/// # Errors
///
/// [`AddressError::Unresolvable`] if resolution fails or yields nothing;
/// [`AddressError::Blocked`] if any resolved IP is blocked (fail-closed).
pub fn resolve_and_screen<R: HostResolver + ?Sized>(
    host: &str,
    policy: &DialPolicy,
    resolver: &R,
) -> Result<Vec<IpAddr>, AddressError> {
    let addrs = resolver
        .resolve(host)
        .map_err(|e| AddressError::Unresolvable {
            host: host.to_owned(),
            reason: e.to_string(),
        })?;
    if addrs.is_empty() {
        return Err(AddressError::Unresolvable {
            host: host.to_owned(),
            reason: "no addresses".to_owned(),
        });
    }
    for &ip in &addrs {
        screen_ip(ip, policy)?;
    }
    Ok(addrs)
}

/// Parse a device-management address and return its host (userinfo stripped;
/// brackets stripped for an IPv6 literal).
///
/// A device-management address is either an `http`/`https` **URL**
/// (`http://[fd00:db8::42]`) or a bare **authority** (`[fd00:db8::42]:5961`, as
/// discovery emits) — both are accepted. When a URL scheme is present it must be
/// `http`/`https` (blocking `file:`/`gopher:`/`dict:` SSRF gadgets); a scheme
/// with no host, or userinfo confusion (`http://trusted@169.254.169.254/`), is
/// resolved to the real host after the last `@`.
///
/// # Errors
///
/// [`AddressError::Empty`], [`AddressError::Scheme`] (a non-HTTP scheme), or
/// [`AddressError::NoHost`].
pub fn parse_device_address(address: &str) -> Result<HostSpec, AddressError> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(AddressError::Empty);
    }
    // A `scheme://` prefix must be http/https; no scheme ⇒ a bare authority.
    let authority = if let Some((scheme, rest)) = trimmed.split_once("://") {
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Err(AddressError::Scheme {
                address: address.to_owned(),
            });
        }
        rest.split(['/', '?', '#']).next().unwrap_or("")
    } else {
        trimmed
    };
    // Strip any `userinfo@` prefix — the real host is after the last `@`.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);
    let host = host_of_authority(authority).ok_or_else(|| AddressError::NoHost {
        address: address.to_owned(),
    })?;
    Ok(host_spec(host))
}

/// Parse a Cast device authority (`host[:port]`, IPv6 bracketed), returning the
/// host and the port (defaulting the CASTV2 port `8009`).
///
/// # Errors
///
/// [`AddressError::Empty`], [`AddressError::NoHost`], or
/// [`AddressError::Authority`] (malformed port / shape).
pub fn parse_cast_authority(address: &str) -> Result<(HostSpec, u16), AddressError> {
    /// The default CASTV2 TLS port.
    const DEFAULT_CAST_PORT: u16 = 8009;

    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(AddressError::Empty);
    }
    let authority_err = || AddressError::Authority {
        address: address.to_owned(),
    };
    let (host, port) = if let Some(rest) = trimmed.strip_prefix('[') {
        // Bracketed IPv6: `[host]` or `[host]:port`.
        let (host, after) = rest.split_once(']').ok_or_else(authority_err)?;
        if host.is_empty() {
            return Err(AddressError::NoHost {
                address: address.to_owned(),
            });
        }
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| authority_err())?,
            None if after.is_empty() => DEFAULT_CAST_PORT,
            None => return Err(authority_err()),
        };
        (host.to_owned(), port)
    } else if let Ok(v6) = trimmed.parse::<Ipv6Addr>() {
        // A bare (unbracketed) IPv6 literal with no port.
        (v6.to_string(), DEFAULT_CAST_PORT)
    } else {
        match trimmed.rsplit_once(':') {
            // A single `:` means `host:port`; a host with `:` unbracketed would
            // be a bare IPv6 literal, handled above.
            Some((host, port)) if !host.contains(':') => {
                if host.is_empty() {
                    return Err(AddressError::NoHost {
                        address: address.to_owned(),
                    });
                }
                (host.to_owned(), port.parse().map_err(|_| authority_err())?)
            }
            Some(_) => return Err(authority_err()),
            None => (trimmed.to_owned(), DEFAULT_CAST_PORT),
        }
    };
    Ok((host_spec(&host), port))
}

/// Classify a host string as an IP literal or a DNS name.
fn host_spec(host: &str) -> HostSpec {
    host.parse::<IpAddr>()
        .map_or_else(|_| HostSpec::Name(host.to_owned()), HostSpec::Ip)
}

/// Extract the host from an authority (`host`, `host:port`, `[v6]`, `[v6]:port`,
/// or a bare IPv6 literal). Returns [`None`] for an empty host.
fn host_of_authority(authority: &str) -> Option<&str> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: host runs to the closing bracket.
        let host = rest.split(']').next().unwrap_or_default();
        return (!host.is_empty()).then_some(host);
    }
    if authority.parse::<Ipv6Addr>().is_ok() {
        // A bare (unbracketed) IPv6 literal.
        return Some(authority);
    }
    let host = authority.split(':').next().unwrap_or_default();
    (!host.is_empty()).then_some(host)
}
