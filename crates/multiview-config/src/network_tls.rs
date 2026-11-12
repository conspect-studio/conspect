//! ACME certificate configuration for the future network front door.
//! No credential values are accepted here: fields ending in `_env` are names only.

use std::{collections::HashSet, net::IpAddr, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Default `network.https_port`.
pub const DEFAULT_HTTPS_PORT: u16 = 443;
/// Default `network.workers`: OS threads of the front door's own runtime.
pub const DEFAULT_WORKERS: u16 = 2;
/// Most `network.workers` accepted.
pub const MAX_WORKERS: u16 = 64;

const fn default_workers() -> u16 {
    DEFAULT_WORKERS
}

/// The front-door `[network]` section (ADR-0107).
///
/// Slice 1 (the shared UDP socket) reads `https_port`, `advertise` and
/// `workers`; slice 5 (ACME) reads `public_host` and `tls`. `public_host` and
/// `tls` come together: both or neither.
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
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            public_host: None,
            tls: None,
            https_port: None,
            advertise: Vec::new(),
            workers: DEFAULT_WORKERS,
        }
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
        match (&self.public_host, &self.tls) {
            (None, None) => Ok(()),
            (Some(public_host), Some(tls)) => {
                domain("public_host", public_host, AcmeChallenge::TlsAlpn01)?;
                tls.validate()?;
                if !tls.domains.iter().any(|name| name == public_host) {
                    return Err(fail("domains", "must include network.public_host"));
                }
                Ok(())
            }
            (Some(_), None) => Err(ConfigError::Validation(
                "network.public_host is set without [network.tls]".to_owned(),
            )),
            (None, Some(_)) => Err(ConfigError::Validation(
                "[network.tls] needs network.public_host".to_owned(),
            )),
        }
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
    fn public_host_and_tls_come_together() {
        let n: NetworkConfig = toml::from_str("public_host = \"a.example.org\"").unwrap();
        assert!(n
            .validate()
            .unwrap_err()
            .to_string()
            .contains("network.tls"));
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
