//! Device-reachable Cast media URLs (DEV-D2, ADR-M011).
//!
//! Cast devices **ignore DHCP-provided DNS and resolve via hardcoded public
//! resolvers**, so the media URL handed to a device must use an **IP-literal
//! host** (or a *publicly* resolvable name): `.local` and bare LAN names
//! never resolve on the device, and a loopback is unreachable from it. The
//! operator names that base once (`control.cast_media_base`); this module
//! validates it and joins it with the DEV-D1 `/hls/{output-id}` delivery
//! mounts into the `contentId` a session LOADs.
//!
//! IPv6 note (conventions §10 / ADR-0042): the base is accepted in IPv6 form
//! first (`http://[2001:db8::7]:8080`), but Cast devices are effectively
//! IPv4-legacy in practice — an IPv4-literal base is the one deliberate
//! legacy-interop carve-out, documented as such, and CASTV2-over-IPv6 is a
//! hardware-validation item (DEV-D4), never an assumption.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use thiserror::Error;

/// The default CASTV2 TLS port. Cast **groups** advertise non-default ports —
/// an explicit `host:port` is always honoured over this default.
pub const DEFAULT_CAST_PORT: u16 = 8009;

/// Why a Cast media base (or device authority) was rejected.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CastMediaError {
    /// The base is not an `http://`/`https://` URL.
    #[error("cast media base {base:?} must be an http:// or https:// URL")]
    NotHttp {
        /// The rejected base.
        base: String,
    },
    /// The base host is a loopback the device cannot reach.
    #[error(
        "cast media base {base:?} is a loopback — the Cast device cannot reach it; use an \
         address of this host on a network the device can reach"
    )]
    Loopback {
        /// The rejected base.
        base: String,
    },
    /// The base host cannot resolve on a Cast device (mDNS `.local` or a
    /// bare LAN name — the devices resolve via hardcoded public DNS).
    #[error(
        "cast media base host {host:?} will not resolve on a Cast device (they ignore LAN DNS \
         and resolve via hardcoded public resolvers): use an IP literal, or a publicly \
         resolvable name"
    )]
    UnresolvableHost {
        /// The rejected host.
        host: String,
    },
    /// The base carries a path/query — only `scheme://host[:port]` is a base.
    #[error(
        "cast media base {base:?} must not carry a path or query (the /hls mounts are \
             appended automatically)"
    )]
    HasPath {
        /// The rejected base.
        base: String,
    },
    /// The base has no host at all.
    #[error("cast media base {base:?} has no host")]
    EmptyHost {
        /// The rejected base.
        base: String,
    },
}

/// The HLS segment container of a rendition — what the LOAD's
/// `hlsVideoSegmentFormat` declares (receivers assume MPEG-TS unless told).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HlsSegmentFormat {
    /// MPEG-TS segments (what the DEV-D1 segmenter writes today — see
    /// `multiview_output::hls::live::LivePlaylist`, MPEG-TS only).
    MpegTs,
    /// Fragmented MP4 / CMAF segments (ADR-0007's target container; signal
    /// this once a rendition actually serves CMAF).
    Fmp4,
}

impl HlsSegmentFormat {
    /// The `hlsVideoSegmentFormat` wire token.
    #[must_use]
    pub const fn wire_token(self) -> &'static str {
        match self {
            Self::MpegTs => "mpeg2_ts",
            Self::Fmp4 => "fmp4",
        }
    }
}

/// One castable rendition: the device-reachable playlist URL and its segment
/// format (what a session LOADs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastMediaTarget {
    /// The device-reachable playlist URL (IP-literal or public-name host).
    pub url: String,
    /// The rendition's HLS segment container.
    pub format: HlsSegmentFormat,
}

/// A validated, normalized Cast media base: `scheme://host[:port]` with an
/// IP-literal (or publicly-resolvable) non-loopback host and no path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastMediaBase(String);

impl CastMediaBase {
    /// Parse and validate a base URL (see the module docs for the host
    /// rules). A single trailing `/` is tolerated and stripped.
    ///
    /// # Errors
    ///
    /// [`CastMediaError`] naming the violated rule.
    pub fn parse(base: &str) -> Result<Self, CastMediaError> {
        let trimmed = base.strip_suffix('/').unwrap_or(base);
        let rest = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("https://"))
            .ok_or_else(|| CastMediaError::NotHttp {
                base: base.to_owned(),
            })?;
        if rest.contains('/') || rest.contains('?') {
            return Err(CastMediaError::HasPath {
                base: base.to_owned(),
            });
        }
        let host = authority_host(rest);
        if host.is_empty() {
            return Err(CastMediaError::EmptyHost {
                base: base.to_owned(),
            });
        }
        validate_device_reachable_host(&host, base)?;
        Ok(Self(trimmed.to_owned()))
    }

    /// The normalized base (no trailing slash).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Join a DEV-D1 mount route (e.g. `/hls/program`) and a playlist file
    /// name into the device-reachable media URL.
    #[must_use]
    pub fn join(&self, mount_route: &str, playlist: &str) -> String {
        let route = mount_route.trim_matches('/');
        format!("{}/{route}/{playlist}", self.0)
    }
}

/// The host portion of an authority (`host[:port]`), brackets stripped for an
/// IPv6 literal.
fn authority_host(authority: &str) -> String {
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: the host runs to the closing bracket.
        rest.split(']').next().unwrap_or_default().to_owned()
    } else {
        authority.split(':').next().unwrap_or_default().to_owned()
    }
}

/// Enforce the device-reachability host rules: IP literals must not be
/// loopback; names must be neither mDNS (`.local`) nor single-label (LAN
/// DNS), because the device resolves via hardcoded public resolvers. A
/// multi-label public name is accepted — it MUST be publicly resolvable
/// (the documented caveat).
fn validate_device_reachable_host(host: &str, base: &str) -> Result<(), CastMediaError> {
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        if v6.is_loopback() {
            return Err(CastMediaError::Loopback {
                base: base.to_owned(),
            });
        }
        return Ok(());
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        if v4.is_loopback() {
            return Err(CastMediaError::Loopback {
                base: base.to_owned(),
            });
        }
        // An IPv4-literal base is the deliberate legacy-interop carve-out
        // (conventions §10): Cast devices are effectively IPv4-only today.
        return Ok(());
    }
    let lowered = host.to_ascii_lowercase();
    // The last DNS label decides: `local` covers both the mDNS suffix
    // (`tv.local`) and a literal `local`; a single-label name (no `.`) is a
    // bare LAN name. `localhost` is loopback by another name.
    let last_label = lowered.rsplit('.').next().unwrap_or(&lowered);
    if lowered == "localhost" || last_label == "local" || !lowered.contains('.') {
        return Err(CastMediaError::UnresolvableHost {
            host: host.to_owned(),
        });
    }
    Ok(())
}

/// Split a Cast device authority (`host[:port]`, IPv6 bracketed) into the
/// dial host (brackets stripped) and port, defaulting the CASTV2 port 8009.
/// Returns [`None`] for an empty host or a malformed port — never a guess.
#[must_use]
pub fn split_authority(address: &str) -> Option<(String, u16)> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    if let Some(rest) = address.strip_prefix('[') {
        // Bracketed IPv6: `[host]` or `[host]:port`.
        let (host, after) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => DEFAULT_CAST_PORT,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }
    match address.rsplit_once(':') {
        // A lone colon means `host:port`; more colons unbracketed would be a
        // bare IPv6 literal — require brackets for those (URL convention).
        Some((host, port)) if !host.contains(':') => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_owned(), port.parse().ok()?))
        }
        Some(_) => None,
        None => Some((address.to_owned(), DEFAULT_CAST_PORT)),
    }
}

/// The delivery map: output id → castable rendition, in declaration order.
///
/// Built by the binary from the DEV-D1 HLS mounts + the validated
/// [`CastMediaBase`]; read by the session routes (to resolve a requested
/// output) and the [`CastSessionFactory`](super::runtime::CastSessionFactory)
/// (to resolve a device's `display.assign`).
#[derive(Debug, Clone, Default)]
pub struct CastDelivery {
    /// Rendition targets keyed by output id (lookup).
    targets: BTreeMap<String, CastMediaTarget>,
    /// Output ids in declaration order (the `{ program = true }` default is
    /// the FIRST declared rendition).
    order: Vec<String>,
}

impl CastDelivery {
    /// An empty delivery map (no castable rendition).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a rendition for `output_id` (declaration order preserved; a
    /// duplicate id replaces the target but keeps its original position).
    pub fn insert(&mut self, output_id: &str, target: CastMediaTarget) {
        if self.targets.insert(output_id.to_owned(), target).is_none() {
            self.order.push(output_id.to_owned());
        }
    }

    /// The rendition for `output_id`, if that output is a served HLS mount.
    #[must_use]
    pub fn for_output(&self, output_id: &str) -> Option<&CastMediaTarget> {
        self.targets.get(output_id)
    }

    /// The first declared rendition — what `{ program = true }` casts (every
    /// HLS output is a rendition of the program canvas).
    #[must_use]
    pub fn first(&self) -> Option<&CastMediaTarget> {
        self.order.first().and_then(|id| self.targets.get(id))
    }

    /// The first declared output id (the `{ program = true }` resolution).
    #[must_use]
    pub fn first_output_id(&self) -> Option<&str> {
        self.order.first().map(String::as_str)
    }

    /// Whether no rendition is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Resolve a device's display assignment to a castable rendition:
    /// `Output(id)` → that rendition; `Program(true)` → the first declared
    /// rendition; a wall head is **not** an HLS rendition (ADR-M011 — the
    /// media path is an existing rendition) and never resolves.
    #[must_use]
    pub fn resolve_assign(
        &self,
        assign: &multiview_config::DisplayAssign,
    ) -> Option<&CastMediaTarget> {
        match assign {
            multiview_config::DisplayAssign::Program(true) => self.first(),
            multiview_config::DisplayAssign::Output(id) => self.for_output(id),
            // Not castable: `Program(false)`, a wall head (not an HLS
            // rendition), and — `DisplayAssign` being `#[non_exhaustive]` —
            // any future assignment kind until this map learns it.
            _ => None,
        }
    }
}
