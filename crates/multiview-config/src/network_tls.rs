//! ACME certificate configuration for the future network front door.
//! No credential values are accepted here: fields ending in `_env` are names only.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Default `network.https_port`.
pub const DEFAULT_HTTPS_PORT: u16 = 443;
/// Default `network.workers`: OS threads of the front door's own runtime.
pub const DEFAULT_WORKERS: u16 = 2;
/// Most `network.workers` accepted.
pub const MAX_WORKERS: u16 = 64;
/// Default `network.udp.recv_buffer_bytes` (`SO_RCVBUF` request).
pub const DEFAULT_RECV_BUFFER_BYTES: u32 = 8 * 1024 * 1024;
/// Most `network.udp.recv_buffer_bytes` accepted (256 MiB).
pub const MAX_RECV_BUFFER_BYTES: u32 = 256 * 1024 * 1024;
/// Default `network.udp.new_flow_pps_per_source`.
pub const DEFAULT_NEW_FLOW_PPS_PER_SOURCE: u32 = 500;
/// Default `network.udp.new_flow_burst`.
pub const DEFAULT_NEW_FLOW_BURST: u32 = 200;
/// Default `network.udp.new_flow_global_pps`.
pub const DEFAULT_NEW_FLOW_GLOBAL_PPS: u32 = 50_000;
/// Default `network.udp.stun_pps_per_source`.
pub const DEFAULT_STUN_PPS_PER_SOURCE: u32 = 50;
/// Default `network.udp.stun_global_pps`.
pub const DEFAULT_STUN_GLOBAL_PPS: u32 = 5_000;
/// Default `network.udp.nft_floor_status_file`: where the shipped systemd
/// unit's `ExecStartPre=+` writes `nft -j list table inet conspect_floor`
/// (`deploy/systemd/conspect.service`), for the engine (running unprivileged,
/// unable to list the ruleset itself) to read as a pure file parse. The
/// directory is created root-owned (0755) by that root step, so the service
/// user cannot replace or redirect the file; the engine also only trusts a
/// root-owned regular file that is not group/other-writable.
pub const DEFAULT_NFT_FLOOR_STATUS_FILE: &str = "/run/conspect-floor/nft-floor.json";
/// Default `network.quic.retry_above_handshakes` (QUIC Retry, RFC 9000
/// section 8.1.2).
pub const DEFAULT_RETRY_ABOVE_HANDSHAKES: usize = 8;
/// Default `network.quic.max_connections`.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 1024;
/// Most `network.quic.max_connections` accepted.
pub const MAX_MAX_CONNECTIONS: u32 = 65_536;

/// Default `network.http_redirect_port` in `direct` mode.
pub const DEFAULT_HTTP_REDIRECT_PORT: u16 = 80;
/// Default `network.proxy_listen` in `behind_proxy` mode.
pub const DEFAULT_PROXY_LISTEN: &str = "[::1]:8443";
/// Most `network.trusted_proxies` entries accepted.
pub const MAX_TRUSTED_PROXIES: usize = 64;

const fn default_workers() -> u16 {
    DEFAULT_WORKERS
}

const fn default_true() -> bool {
    true
}

/// How the front door owns TCP (design note sections 4 and 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// The engine owns the TCP door (`[bind]:tcp_port`) and the port-80 redirect.
    Direct,
    /// A reverse proxy owns TCP 443 and forwards to `proxy_listen` with PROXY v2.
    BehindProxy,
}

/// `[network.ice_tcp]`: passive ICE-TCP candidates on the TCP door (RFC 6544).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IceTcpConfig {
    /// Accept RFC 4571-framed ICE-TCP connections on the TCP door. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for IceTcpConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// One `network.trusted_proxies` entry: an address and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedProxy {
    /// The network address (IPv4-mapped IPv6 normalised to IPv4).
    pub addr: IpAddr,
    /// Prefix length (32/128 for a single address).
    pub prefix: u8,
}

impl TrustedProxy {
    /// Parse an IP literal or a CIDR prefix (`"::1"`, `"10.0.0.0/8"`).
    ///
    /// # Errors
    /// A validation error naming the entry.
    pub fn parse(entry: &str) -> Result<Self, ConfigError> {
        let bad = || {
            ConfigError::Validation(format!(
                "network.trusted_proxies entry {entry:?} is not an IP literal or CIDR prefix"
            ))
        };
        let (ip, prefix) = match entry.split_once('/') {
            Some((ip, prefix)) => (ip, Some(prefix.parse::<u8>().map_err(|_| bad())?)),
            None => (entry, None),
        };
        let addr = unmap_ip(ip.parse::<IpAddr>().map_err(|_| bad())?);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return Err(bad());
        }
        Ok(Self { addr, prefix })
    }

    /// Whether `ip` (IPv4-mapped normalised) falls inside this prefix.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, unmap_ip(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_match(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_match(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn unmap_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn prefix_match(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let mut bits = u32::from(prefix);
    for (a, b) in net.iter().zip(ip) {
        if bits == 0 {
            return true;
        }
        let take = bits.min(8);
        let mask = u8::MAX.checked_shl(8 - take).unwrap_or(0);
        if a & mask != b & mask {
            return false;
        }
        bits -= take;
    }
    true
}

const fn default_recv_buffer_bytes() -> u32 {
    DEFAULT_RECV_BUFFER_BYTES
}

const fn default_new_flow_pps_per_source() -> u32 {
    DEFAULT_NEW_FLOW_PPS_PER_SOURCE
}

const fn default_new_flow_burst() -> u32 {
    DEFAULT_NEW_FLOW_BURST
}

const fn default_new_flow_global_pps() -> u32 {
    DEFAULT_NEW_FLOW_GLOBAL_PPS
}

const fn default_validated_steering() -> bool {
    true
}

const fn default_stun_pps_per_source() -> u32 {
    DEFAULT_STUN_PPS_PER_SOURCE
}

const fn default_stun_global_pps() -> u32 {
    DEFAULT_STUN_GLOBAL_PPS
}

fn default_nft_floor_status_file() -> PathBuf {
    PathBuf::from(DEFAULT_NFT_FLOOR_STATUS_FILE)
}

const fn default_retry_above_handshakes() -> usize {
    DEFAULT_RETRY_ABOVE_HANDSHAKES
}

const fn default_max_connections() -> u32 {
    DEFAULT_MAX_CONNECTIONS
}

/// The front-door `[network]` section (ADR-0107).
///
/// Slice 1 (the shared UDP socket) reads `https_port`, `advertise` and
/// `workers`; slice 4 (the TCP door) reads `mode`, `tcp_port`,
/// `http_redirect_port`, `bind`, `trusted_proxies`, `proxy_listen`,
/// `ice_tcp` and `public_host` (the redirect target); slice 5 (ACME) reads
/// `tls`. `tls` requires `public_host`; `public_host` alone is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Public DNS name or IP address for the front door.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_host: Option<String>,
    /// ACME certificate settings, when issuance is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<NetworkTlsConfig>,
    /// The UDP port of the shared front-door socket (QUIC/MoQ and WebRTC),
    /// bound dual-stack on `[::]`. **Setting it is what turns the slice-1 front
    /// door on**; absent, `[network]` carries only ACME settings and the legacy
    /// listeners stay as they are. Typically 443; a port below 1024 needs
    /// `CAP_NET_BIND_SERVICE` (or systemd socket activation, a later slice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_port: Option<u16>,
    /// Public addresses for ICE host candidates (IP literals, IPv6 first). When
    /// non-empty it replaces `webrtc.advertised_addresses`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advertise: Vec<String>,
    /// OS threads of the front door's own Tokio runtime. It never shares the
    /// runtime that drives the output clock (invariant #1). Default 2.
    #[serde(default = "default_workers")]
    pub workers: u16,
    /// Who owns TCP (slice 4). **Absent = no TCP listener at all** (slice-1
    /// behaviour: the UDP door only), so a saved state with only `https_port`
    /// starts unchanged while another server holds TCP 443.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkMode>,
    /// The TCP port of the door in `direct` mode; defaults to `https_port`.
    /// (A deviation from design section 10, which has one port for both: it lets
    /// an operator stage the TCP door on a second port while another server
    /// still owns TCP 443.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_port: Option<u16>,
    /// The plaintext 80 -> 443 redirect listener in `direct` mode (default 80;
    /// `0` disables). Ignored in `behind_proxy` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_redirect_port: Option<u16>,
    /// The IPv6 address the TCP door and the redirect bind in `direct` mode
    /// (default `[::]`, dual-stack). An IPv4 literal is rejected (IPv6-first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// PROXY v2 sources trusted in `behind_proxy` mode: IP literals or CIDR
    /// prefixes. Required (non-empty) in that mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<String>,
    /// The TCP listener a proxy forwards to in `behind_proxy` mode (default
    /// `[::1]:8443`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_listen: Option<String>,
    /// `[network.ice_tcp]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_tcp: Option<IceTcpConfig>,
    /// `[network.udp]`: the dispatcher's receive buffer and new-flow limits
    /// (slice 3, design note section 10).
    #[serde(default)]
    pub udp: NetworkUdpConfig,
    /// `[network.quic]`: the noq endpoint's Retry policy and connection cap
    /// (slice 3, design note section 10).
    #[serde(default)]
    pub quic: NetworkQuicConfig,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            public_host: None,
            tls: None,
            https_port: None,
            advertise: Vec::new(),
            workers: DEFAULT_WORKERS,
            mode: None,
            tcp_port: None,
            http_redirect_port: None,
            bind: None,
            trusted_proxies: Vec::new(),
            proxy_listen: None,
            ice_tcp: None,
            udp: NetworkUdpConfig::default(),
            quic: NetworkQuicConfig::default(),
        }
    }
}

/// `[network.udp]` (design note section 10): the front door's receive buffer
/// and new-flow limits. Every field is optional with a default equal to
/// `multiview-frontdoor`'s crate default, so an old `[network]` section
/// without `[network.udp]` parses unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkUdpConfig {
    /// `SO_RCVBUF` request; the kernel may clamp it (`rmem_max`), which is
    /// logged at startup.
    #[serde(default = "default_recv_buffer_bytes")]
    pub recv_buffer_bytes: u32,
    /// Sustained new-flow datagrams per second per source key (IPv4 address
    /// or IPv6 /64).
    #[serde(default = "default_new_flow_pps_per_source")]
    pub new_flow_pps_per_source: u32,
    /// Burst per source key.
    #[serde(default = "default_new_flow_burst")]
    pub new_flow_burst: u32,
    /// Sustained new-flow datagrams per second for all sources together.
    #[serde(default = "default_new_flow_global_pps")]
    pub new_flow_global_pps: u32,
    /// Validated-flow steering (Linux `SO_REUSEPORT` + CBPF). `false` forces
    /// one socket.
    #[serde(default = "default_validated_steering")]
    pub validated_steering: bool,
    /// Sustained well-formed STUN datagrams per second per source, from
    /// unvalidated sources.
    #[serde(default = "default_stun_pps_per_source")]
    pub stun_pps_per_source: u32,
    /// Sustained STUN datagrams per second from unvalidated sources, all
    /// sources together.
    #[serde(default = "default_stun_global_pps")]
    pub stun_global_pps: u32,
    /// Where the shipped systemd unit's `ExecStartPre=+` writes
    /// `nft -j list table inet conspect_floor` for the engine to read (it
    /// runs unprivileged and cannot list the ruleset itself). Default
    /// `/run/conspect-floor/nft-floor.json`; only a root-owned regular file
    /// that is not group/other-writable is trusted (a status report, never
    /// an enforcement input).
    #[serde(default = "default_nft_floor_status_file")]
    pub nft_floor_status_file: PathBuf,
}

impl Default for NetworkUdpConfig {
    fn default() -> Self {
        Self {
            recv_buffer_bytes: DEFAULT_RECV_BUFFER_BYTES,
            new_flow_pps_per_source: DEFAULT_NEW_FLOW_PPS_PER_SOURCE,
            new_flow_burst: DEFAULT_NEW_FLOW_BURST,
            new_flow_global_pps: DEFAULT_NEW_FLOW_GLOBAL_PPS,
            validated_steering: true,
            stun_pps_per_source: DEFAULT_STUN_PPS_PER_SOURCE,
            stun_global_pps: DEFAULT_STUN_GLOBAL_PPS,
            nft_floor_status_file: default_nft_floor_status_file(),
        }
    }
}

impl NetworkUdpConfig {
    /// Validate ranges (design note section 10): every rate non-zero,
    /// `recv_buffer_bytes` within `(0, MAX_RECV_BUFFER_BYTES]`.
    ///
    /// # Errors
    /// Returns a field-specific validation error.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.recv_buffer_bytes == 0 || self.recv_buffer_bytes > MAX_RECV_BUFFER_BYTES {
            return Err(network_fail(
                "udp.recv_buffer_bytes",
                "must be between 1 and 256 MiB",
            ));
        }
        for (field, value) in [
            ("udp.new_flow_pps_per_source", self.new_flow_pps_per_source),
            ("udp.new_flow_burst", self.new_flow_burst),
            ("udp.new_flow_global_pps", self.new_flow_global_pps),
            ("udp.stun_pps_per_source", self.stun_pps_per_source),
            ("udp.stun_global_pps", self.stun_global_pps),
        ] {
            if value == 0 {
                return Err(network_fail(field, "must be non-zero"));
            }
        }
        Ok(())
    }
}

/// `[network.quic]` (design note section 10): the noq endpoint's Retry
/// policy and connection cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkQuicConfig {
    /// Above this many handshakes in flight, an unvalidated `Incoming` that
    /// may still retry is sent a QUIC Retry (RFC 9000 section 8.1.2) instead
    /// of accepted.
    #[serde(default = "default_retry_above_handshakes")]
    pub retry_above_handshakes: usize,
    /// Cap on established QUIC (`MoQ`) peers.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for NetworkQuicConfig {
    fn default() -> Self {
        Self {
            retry_above_handshakes: DEFAULT_RETRY_ABOVE_HANDSHAKES,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

impl NetworkQuicConfig {
    /// Validate `max_connections` is in `1..=65536` (design note section
    /// 10). `retry_above_handshakes` has no invalid value: 0 means "retry
    /// every unvalidated handshake", which is a legitimate (if aggressive)
    /// choice.
    ///
    /// # Errors
    /// Returns a validation error when `max_connections` is out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_connections == 0 || self.max_connections > MAX_MAX_CONNECTIONS {
            return Err(network_fail(
                "quic.max_connections",
                "must be between 1 and 65536",
            ));
        }
        Ok(())
    }
}

/// ACME authority selected as data, not a separate protocol implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CertificateAuthority {
    /// Let's Encrypt.
    Letsencrypt,
    /// Google Trust Services public ACME.
    Google,
    /// Operator-supplied ACME directory.
    Custom,
}

/// Supported ACME validation methods. HTTP-01 is deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcmeChallenge {
    /// Validate a DNS TXT record.
    #[serde(rename = "dns-01")]
    Dns01,
    /// Validate a TLS `ClientHello` on TCP 443.
    #[serde(rename = "tls-alpn-01")]
    TlsAlpn01,
}

/// Front-door certificate request. Secrets are referenced by environment-variable name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTlsConfig {
    /// Authority profile.
    pub ca: CertificateAuthority,
    /// Select the authority's staging directory.
    #[serde(default)]
    pub staging: bool,
    /// Validation method.
    pub challenge: AcmeChallenge,
    /// Subject alternative names.
    pub domains: Vec<String>,
    /// Optional ACME contact email.
    #[serde(default)]
    pub contact_email: Option<String>,
    /// Profile to request only if advertised by the directory.
    #[serde(default)]
    pub acme_profile: Option<String>,
    /// Directory URL for a custom authority.
    #[serde(default)]
    pub directory: Option<String>,
    /// PEM root path trusted for the custom directory connection.
    #[serde(default)]
    pub trust_anchor: Option<PathBuf>,
    /// Admit a custom directory on a loopback, RFC 1918, unique-local or CGNAT
    /// address (or `localhost`). Off by default; only valid with `ca = "custom"`
    /// and only together with `trust_anchor`. Link-local, multicast,
    /// unspecified, broadcast and IPv4-mapped/compatible addresses stay refused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_private_directory: bool,
    /// Environment variable holding the EAB key identifier.
    #[serde(default)]
    pub eab_kid_env: Option<String>,
    /// Environment variable holding the EAB HMAC secret.
    #[serde(default)]
    pub eab_hmac_env: Option<String>,
    /// Absolute path of the private 0700 ACME state directory.
    pub state_dir: PathBuf,
    /// DNS provider settings (required for DNS-01 only).
    #[serde(default)]
    pub dns: Option<DnsProviderConfig>,
}

/// DNS provider configuration; additional implementations can add variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase", deny_unknown_fields)]
pub enum DnsProviderConfig {
    /// Cloudflare DNS API.
    Cloudflare {
        /// Scoped bearer token environment-variable name.
        #[serde(default)]
        token_env: Option<String>,
        /// Global-key account email environment-variable name.
        #[serde(default)]
        email_env: Option<String>,
        /// Global API key environment-variable name.
        #[serde(default)]
        api_key_env: Option<String>,
        /// DNS propagation deadline in seconds.
        #[serde(default = "default_propagation_timeout")]
        propagation_timeout_s: u64,
    },
}

const fn default_propagation_timeout() -> u64 {
    120
}

fn fail(field: &str, message: &str) -> ConfigError {
    ConfigError::Validation(format!("network.tls.{field}: {message}"))
}

/// A validation error for a `[network]` sub-table other than `tls`:
/// `path` is relative to `network.` (e.g. `udp.new_flow_burst`).
fn network_fail(path: &str, message: &str) -> ConfigError {
    ConfigError::Validation(format!("network.{path}: {message}"))
}

fn env_name(field: &str, value: &str) -> Result<(), ConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && (value.as_bytes().first().is_some_and(u8::is_ascii_uppercase) || value.starts_with('_'));
    if !valid
        || !value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(fail(
            field,
            "must be an environment-variable NAME (never a literal credential)",
        ));
    }
    Ok(())
}

fn domain(field: &str, name: &str, challenge: AcmeChallenge) -> Result<(), ConfigError> {
    if name.parse::<IpAddr>().is_ok() {
        return if challenge == AcmeChallenge::TlsAlpn01 {
            Ok(())
        } else {
            Err(fail(field, "IP addresses require tls-alpn-01"))
        };
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(fail(field, "DNS names must use lowercase ASCII"));
    }
    let bare = name.strip_prefix("*.").unwrap_or(name);
    if bare != name && challenge != AcmeChallenge::Dns01 {
        return Err(fail(field, "wildcards require dns-01"));
    }
    if bare.len() > 253
        || bare.is_empty()
        || !bare.contains('.')
        || bare.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(fail(
            field,
            "must be a valid DNS name or supported IP address",
        ));
    }
    Ok(())
}

/// Screen a custom-directory IP literal. `Ok(true)` means the address is in
/// a private range that needs `allow_private_directory`; never-legitimate
/// ranges are refused outright.
fn directory_ip_is_private(ip: IpAddr) -> Result<bool, ConfigError> {
    use crate::device::net_guard::{classify, BlockClass};
    if let IpAddr::V6(v6) = ip {
        if !v6.is_loopback()
            && !v6.is_unspecified()
            && (v6.to_ipv4_mapped().is_some() || v6.to_ipv4().is_some())
        {
            return Err(fail(
                "directory",
                "IPv4-mapped and IPv4-compatible addresses are forbidden",
            ));
        }
    }
    match classify(ip) {
        None => Ok(false),
        Some(
            BlockClass::Loopback
            | BlockClass::Private
            | BlockClass::UniqueLocal
            | BlockClass::Carrier,
        ) => Ok(true),
        Some(_) => Err(fail(
            "directory",
            "link-local, multicast, unspecified and broadcast addresses are forbidden",
        )),
    }
}

impl NetworkConfig {
    /// Validate front-door certificate shape before runtime startup.
    ///
    /// # Errors
    /// Returns a field-specific validation error on malformed configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_socket()?;
        self.udp.validate()?;
        self.quic.validate()?;
        self.validate_tcp()?;
        if let Some(public_host) = &self.public_host {
            domain("public_host", public_host, AcmeChallenge::TlsAlpn01).map_err(|_| {
                ConfigError::Validation(
                    "network.public_host must be a lowercase DNS name or an IP literal".to_owned(),
                )
            })?;
        }
        // Slice 4 relaxed "both or neither": `public_host` alone is valid (the
        // redirect target); `[network.tls]` still needs it.
        match (&self.public_host, &self.tls) {
            (Some(public_host), Some(tls)) => {
                tls.validate()?;
                if !tls.domains.iter().any(|name| name == public_host) {
                    return Err(fail("domains", "must include network.public_host"));
                }
                Ok(())
            }
            (None, Some(_)) => Err(ConfigError::Validation(
                "[network.tls] needs network.public_host".to_owned(),
            )),
            (_, None) => Ok(()),
        }
    }

    /// The `[network.ice_tcp] enabled` value (default true).
    #[must_use]
    pub fn ice_tcp_enabled(&self) -> bool {
        self.ice_tcp.unwrap_or_default().enabled
    }

    /// The TCP port of the `direct` door (`tcp_port`, else `https_port`).
    #[must_use]
    pub fn effective_tcp_port(&self) -> Option<u16> {
        self.tcp_port.or(self.https_port)
    }

    /// The redirect listener port in `direct` mode (`None` = disabled).
    #[must_use]
    pub fn effective_redirect_port(&self) -> Option<u16> {
        if self.mode != Some(NetworkMode::Direct) {
            return None;
        }
        match self
            .http_redirect_port
            .unwrap_or(DEFAULT_HTTP_REDIRECT_PORT)
        {
            0 => None,
            port => Some(port),
        }
    }

    /// The parsed `bind` address (default `::`).
    ///
    /// # Errors
    /// A validation error for anything but an IPv6 literal.
    pub fn bind_ip(&self) -> Result<Ipv6Addr, ConfigError> {
        let Some(bind) = self.bind.as_deref() else {
            return Ok(Ipv6Addr::UNSPECIFIED);
        };
        let bare = bind
            .strip_prefix('[')
            .and_then(|b| b.strip_suffix(']'))
            .unwrap_or(bind);
        match bare.parse::<IpAddr>() {
            Ok(IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => Ok(v6),
            Ok(_) => Err(ConfigError::Validation(format!(
                "network.bind {bind:?} is IPv4: the front door is IPv6-first; use \"[::]\" \
                 (dual-stack) or an IPv6 address"
            ))),
            Err(_) => Err(ConfigError::Validation(format!(
                "network.bind {bind:?} is not an IPv6 address"
            ))),
        }
    }

    /// The parsed `proxy_listen` address (default `[::1]:8443`).
    ///
    /// # Errors
    /// A validation error for a malformed or unspecified-IPv4 address.
    pub fn proxy_listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        let text = self.proxy_listen.as_deref().unwrap_or(DEFAULT_PROXY_LISTEN);
        let addr: SocketAddr = text.parse().map_err(|_| {
            ConfigError::Validation(format!(
                "network.proxy_listen {text:?} is not a socket address"
            ))
        })?;
        if addr.ip() == IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED) {
            return Err(ConfigError::Validation(
                "network.proxy_listen must not be 0.0.0.0: bind [::] (dual-stack) or a \
                 specific address"
                    .to_owned(),
            ));
        }
        if addr.port() == 0 {
            return Err(ConfigError::Validation(
                "network.proxy_listen needs a non-zero port".to_owned(),
            ));
        }
        Ok(addr)
    }

    /// The parsed `trusted_proxies`.
    ///
    /// # Errors
    /// A validation error naming the first malformed entry.
    pub fn trusted_proxy_list(&self) -> Result<Vec<TrustedProxy>, ConfigError> {
        self.trusted_proxies
            .iter()
            .map(|e| TrustedProxy::parse(e))
            .collect()
    }

    /// Validate the slice-4 TCP keys.
    fn validate_tcp(&self) -> Result<(), ConfigError> {
        self.bind_ip()?;
        if self.trusted_proxies.len() > MAX_TRUSTED_PROXIES {
            return Err(ConfigError::Validation(format!(
                "network.trusted_proxies holds at most {MAX_TRUSTED_PROXIES} entries"
            )));
        }
        self.trusted_proxy_list()?;
        self.proxy_listen_addr()?;
        if self.tcp_port == Some(0) {
            return Err(ConfigError::Validation(
                "network.tcp_port must not be 0".to_owned(),
            ));
        }
        let Some(mode) = self.mode else {
            return Ok(());
        };
        if self.https_port.is_none() {
            return Err(ConfigError::Validation(
                "network.mode needs network.https_port: the TCP door is part of the front door"
                    .to_owned(),
            ));
        }
        match mode {
            NetworkMode::Direct => {
                if let Some(redirect) = self.effective_redirect_port() {
                    if self.public_host.is_none() {
                        return Err(ConfigError::Validation(
                            "network.mode = \"direct\" with the redirect listener needs \
                             network.public_host (the redirect target never comes from the \
                             Host header); set it, or http_redirect_port = 0"
                                .to_owned(),
                        ));
                    }
                    if Some(redirect) == self.effective_tcp_port() {
                        return Err(ConfigError::Validation(
                            "network.http_redirect_port must differ from the TCP door port"
                                .to_owned(),
                        ));
                    }
                }
            }
            NetworkMode::BehindProxy => {
                if self.trusted_proxies.is_empty() {
                    return Err(ConfigError::Validation(
                        "network.mode = \"behind_proxy\" needs network.trusted_proxies (PROXY \
                         v2 is accepted from those sources only)"
                            .to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate the slice-1 socket keys (`https_port`, `workers`, `advertise`).
    fn validate_socket(&self) -> Result<(), ConfigError> {
        if self.https_port == Some(0) {
            return Err(ConfigError::Validation(
                "network.https_port must not be 0: the front door needs a stable port".to_owned(),
            ));
        }
        if self.workers == 0 || self.workers > MAX_WORKERS {
            return Err(ConfigError::Validation(format!(
                "network.workers must be between 1 and {MAX_WORKERS}"
            )));
        }
        for address in &self.advertise {
            let ip: IpAddr = address.parse().map_err(|_| {
                ConfigError::Validation(format!(
                    "network.advertise entry {address:?} is not an IP literal"
                ))
            })?;
            if ip.is_unspecified() {
                return Err(ConfigError::Validation(format!(
                    "network.advertise entry {address:?} is unspecified"
                )));
            }
        }
        Ok(())
    }

    /// The advertised IP literals, IPv6 first (ADR-0042).
    #[must_use]
    pub fn advertised_ips(&self) -> Vec<IpAddr> {
        let mut ips: Vec<IpAddr> = self
            .advertise
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect();
        ips.sort_by_key(IpAddr::is_ipv4);
        ips
    }
}

impl NetworkTlsConfig {
    /// Resolve the selected CA directory, without making a network request.
    ///
    /// # Errors
    /// Returns a validation error if a custom directory is absent.
    pub fn directory_url(&self) -> Result<&str, ConfigError> {
        match self.ca {
            CertificateAuthority::Letsencrypt => Ok(if self.staging {
                "https://acme-staging-v02.api.letsencrypt.org/directory"
            } else {
                "https://acme-v02.api.letsencrypt.org/directory"
            }),
            CertificateAuthority::Google => Ok(if self.staging {
                "https://dv.acme-v02.test-api.pki.goog/directory"
            } else {
                "https://dv.acme-v02.api.pki.goog/directory"
            }),
            CertificateAuthority::Custom => self
                .directory
                .as_deref()
                .ok_or_else(|| fail("directory", "custom CA requires a directory URL")),
        }
    }

    /// Reject invalid settings and any apparent literal secrets.
    ///
    /// # Errors
    /// Returns a field-specific validation error for an invalid value.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.domains.is_empty() {
            return Err(fail("domains", "must contain at least one name"));
        }
        let mut seen = HashSet::new();
        for name in &self.domains {
            domain("domains", name, self.challenge)?;
            if name.parse::<IpAddr>().is_ok() && self.ca == CertificateAuthority::Google {
                return Err(fail(
                    "domains",
                    "Google Public CA does not validate IP identifiers",
                ));
            }
            if name.parse::<IpAddr>().is_ok()
                && self.ca == CertificateAuthority::Letsencrypt
                && self.acme_profile.as_deref() != Some("shortlived")
            {
                return Err(fail(
                    "acme_profile",
                    "Let's Encrypt IP certificates require shortlived",
                ));
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(fail("domains", "contains a duplicate name"));
            }
        }
        if !self.state_dir.is_absolute()
            || self
                .state_dir
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(fail(
                "state_dir",
                "must be an absolute path without '..' components",
            ));
        }
        if let Some(path) = &self.trust_anchor {
            if !path.is_absolute() {
                return Err(fail("trust_anchor", "must be an absolute PEM path"));
            }
            if self.ca != CertificateAuthority::Custom {
                return Err(fail(
                    "trust_anchor",
                    "only custom CA accepts a trust anchor",
                ));
            }
        }
        if self.ca != CertificateAuthority::Custom && self.directory.is_some() {
            return Err(fail("directory", "only custom CA accepts a directory URL"));
        }
        if self.ca == CertificateAuthority::Custom {
            if self.staging {
                return Err(fail("staging", "custom CA has no staging directory"));
            }
            let url = self.directory_url()?;
            let parsed =
                url::Url::parse(url).map_err(|_| fail("directory", "must be a valid HTTPS URL"))?;
            if parsed.scheme() != "https"
                || parsed.host().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
                || parsed.query().is_some()
            {
                return Err(fail(
                    "directory",
                    "must be an HTTPS URL without credentials, query or fragment",
                ));
            }
            let private = match parsed.host() {
                Some(url::Host::Ipv4(ip)) => directory_ip_is_private(IpAddr::V4(ip))?,
                Some(url::Host::Ipv6(ip)) => directory_ip_is_private(IpAddr::V6(ip))?,
                Some(url::Host::Domain(host)) => {
                    if host == "." || host.is_empty() || !host.contains('.') && host != "localhost"
                    {
                        return Err(fail("directory", "must use a DNS name or IP address"));
                    }
                    host == "localhost"
                }
                None => return Err(fail("directory", "must contain a host")),
            };
            if private && self.trust_anchor.is_none() {
                return Err(fail(
                    "trust_anchor",
                    "private ACME directories require a PEM trust anchor (and allow_private_directory = true)",
                ));
            }
            if private && !self.allow_private_directory {
                return Err(fail(
                    "allow_private_directory",
                    "a loopback, private, unique-local or CGNAT ACME directory needs allow_private_directory = true",
                ));
            }
        } else if self.allow_private_directory {
            return Err(fail(
                "allow_private_directory",
                "only custom CA accepts allow_private_directory",
            ));
        }
        if let Some(profile) = &self.acme_profile {
            if profile.is_empty()
                || !profile
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(fail(
                    "acme_profile",
                    "must contain ASCII letters, digits or hyphens",
                ));
            }
        }
        if let Some(email) = &self.contact_email {
            if email.trim() != email
                || email.contains(char::is_whitespace)
                || email.split_once('@').is_none()
            {
                return Err(fail("contact_email", "must be an email address"));
            }
        }
        if self.eab_kid_env.is_some() != self.eab_hmac_env.is_some() {
            return Err(fail(
                "eab_kid_env/eab_hmac_env",
                "both EAB environment names are required together",
            ));
        }
        if self.ca == CertificateAuthority::Google && self.eab_kid_env.is_none() {
            return Err(fail("eab_kid_env", "Google requires EAB credentials"));
        }
        for (field, name) in [
            ("eab_kid_env", &self.eab_kid_env),
            ("eab_hmac_env", &self.eab_hmac_env),
        ] {
            if let Some(name) = name {
                env_name(field, name)?;
            }
        }
        match (&self.challenge, &self.dns) {
            (
                AcmeChallenge::Dns01,
                Some(DnsProviderConfig::Cloudflare {
                    token_env,
                    email_env,
                    api_key_env,
                    propagation_timeout_s,
                }),
            ) => {
                let token = token_env.is_some();
                let key = email_env.is_some() && api_key_env.is_some();
                if token == key || (email_env.is_some() != api_key_env.is_some()) {
                    return Err(fail(
                        "dns",
                        "configure either token_env or both email_env and api_key_env",
                    ));
                }
                for (field, name) in [
                    ("dns.token_env", token_env),
                    ("dns.email_env", email_env),
                    ("dns.api_key_env", api_key_env),
                ] {
                    if let Some(name) = name {
                        env_name(field, name)?;
                    }
                }
                if !(1..=3600).contains(propagation_timeout_s) {
                    return Err(fail(
                        "dns.propagation_timeout_s",
                        "must be between 1 and 3600",
                    ));
                }
            }
            (AcmeChallenge::Dns01, None) => {
                return Err(fail("dns", "dns-01 requires a DNS provider"))
            }
            (AcmeChallenge::TlsAlpn01, Some(_)) => {
                return Err(fail("dns", "tls-alpn-01 must not configure a DNS provider"))
            }
            (AcmeChallenge::TlsAlpn01, None) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod socket_tests {
    use super::*;

    #[test]
    fn a_socket_only_section_is_valid_and_defaults() {
        let n: NetworkConfig = toml::from_str("").unwrap();
        assert_eq!(n, NetworkConfig::default());
        assert_eq!((n.https_port, n.workers), (None, 2));
        n.validate().unwrap();
        let n: NetworkConfig = toml::from_str(
            "https_port = 8443\nadvertise = [\"203.0.113.10\", \"2001:db8::10\"]\nworkers = 3",
        )
        .unwrap();
        n.validate().unwrap();
        assert_eq!(
            n.advertised_ips(),
            vec![
                "2001:db8::10".parse::<IpAddr>().unwrap(),
                "203.0.113.10".parse().unwrap()
            ]
        );
    }

    #[test]
    fn rejects_bad_socket_values_and_unknown_keys() {
        for bad in [
            "https_port = 0",
            "workers = 0",
            "workers = 65",
            "advertise = [\"host.example\"]",
            "advertise = [\"::\"]",
        ] {
            let n: NetworkConfig = toml::from_str(bad).unwrap();
            assert!(n.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<NetworkConfig>("bogus = 1").is_err());
    }

    #[test]
    fn udp_and_quic_sections_are_optional_and_default() {
        let n: NetworkConfig = toml::from_str("").unwrap();
        assert_eq!(n.udp, NetworkUdpConfig::default());
        assert_eq!(n.quic, NetworkQuicConfig::default());
        assert_eq!(n.udp.recv_buffer_bytes, DEFAULT_RECV_BUFFER_BYTES);
        assert!(n.udp.validated_steering);
        assert_eq!(
            n.udp.nft_floor_status_file,
            PathBuf::from(DEFAULT_NFT_FLOOR_STATUS_FILE)
        );
        assert_eq!(
            n.quic.retry_above_handshakes,
            DEFAULT_RETRY_ABOVE_HANDSHAKES
        );
        assert_eq!(n.quic.max_connections, DEFAULT_MAX_CONNECTIONS);
        n.validate().unwrap();

        let n: NetworkConfig = toml::from_str(
            "[udp]\nrecv_buffer_bytes = 4194304\nnew_flow_pps_per_source = 10\nnew_flow_burst = 5\n\
             new_flow_global_pps = 100\nvalidated_steering = false\nstun_pps_per_source = 10\n\
             stun_global_pps = 200\nnft_floor_status_file = \"/tmp/floor.json\"\n\
             [quic]\nretry_above_handshakes = 16\nmax_connections = 2048\n",
        )
        .unwrap();
        n.validate().unwrap();
        assert_eq!(n.udp.recv_buffer_bytes, 4_194_304);
        assert!(!n.udp.validated_steering);
        assert_eq!(
            n.udp.nft_floor_status_file,
            PathBuf::from("/tmp/floor.json")
        );
        assert_eq!(n.quic.retry_above_handshakes, 16);
        assert_eq!(n.quic.max_connections, 2048);
    }

    #[test]
    fn rejects_bad_udp_and_quic_values_and_unknown_keys() {
        for (bad, message) in [
            (
                "[udp]\nrecv_buffer_bytes = 0",
                "network.udp.recv_buffer_bytes: must be between 1 and 256 MiB",
            ),
            (
                "[udp]\nrecv_buffer_bytes = 300000000",
                "network.udp.recv_buffer_bytes: must be between 1 and 256 MiB",
            ),
            (
                "[udp]\nnew_flow_pps_per_source = 0",
                "network.udp.new_flow_pps_per_source: must be non-zero",
            ),
            (
                "[udp]\nnew_flow_burst = 0",
                "network.udp.new_flow_burst: must be non-zero",
            ),
            (
                "[udp]\nnew_flow_global_pps = 0",
                "network.udp.new_flow_global_pps: must be non-zero",
            ),
            (
                "[udp]\nstun_pps_per_source = 0",
                "network.udp.stun_pps_per_source: must be non-zero",
            ),
            (
                "[udp]\nstun_global_pps = 0",
                "network.udp.stun_global_pps: must be non-zero",
            ),
            (
                "[quic]\nmax_connections = 0",
                "network.quic.max_connections: must be between 1 and 65536",
            ),
            (
                "[quic]\nmax_connections = 100000",
                "network.quic.max_connections: must be between 1 and 65536",
            ),
        ] {
            let n: NetworkConfig = toml::from_str(bad).unwrap();
            let err = n.validate().unwrap_err().to_string();
            assert!(err.contains(message), "{bad}: {err}");
            assert!(!err.contains("network.tls."), "{bad}: {err}");
        }
        assert!(toml::from_str::<NetworkConfig>("[udp]\nbogus = 1").is_err());
        assert!(toml::from_str::<NetworkConfig>("[quic]\nbogus = 1").is_err());
        // retry_above_handshakes = 0 is a legitimate (aggressive) choice, not an error.
        let n: NetworkConfig = toml::from_str("[quic]\nretry_above_handshakes = 0").unwrap();
        n.validate().unwrap();
    }

    #[test]
    fn tls_needs_public_host_but_public_host_stands_alone() {
        // Slice 4 (issue #290) relaxed "both or neither": `public_host` alone is
        // now valid (the port-80 redirect target), so this half of the test was
        // changed from "rejected" to "accepted". `tls` without it is still
        // rejected below.
        let n: NetworkConfig = toml::from_str("public_host = \"a.example.org\"").unwrap();
        n.validate().unwrap();
        let n: NetworkConfig = toml::from_str(
            "[tls]\nca = \"letsencrypt\"\nchallenge = \"tls-alpn-01\"\ndomains = [\"a.example.org\"]\nstate_dir = \"/var/lib/conspect/tls\"",
        )
        .unwrap();
        assert!(n
            .validate()
            .unwrap_err()
            .to_string()
            .contains("public_host"));
    }
}
