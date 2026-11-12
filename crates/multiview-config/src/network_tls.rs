//! ACME certificate configuration for the future network front door.
//! No credential values are accepted here: fields ending in `_env` are names only.

use std::{collections::HashSet, net::IpAddr, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// The new front-door network section. Socket settings belong to the listener slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Public DNS name or IP address for the front door.
    pub public_host: String,
    /// ACME certificate settings, when issuance is configured.
    pub tls: NetworkTlsConfig,
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

impl NetworkConfig {
    /// Validate front-door certificate shape before runtime startup.
    ///
    /// # Errors
    /// Returns a field-specific validation error on malformed configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        domain("public_host", &self.public_host, AcmeChallenge::TlsAlpn01)?;
        self.tls.validate()?;
        if !self
            .tls
            .domains
            .iter()
            .any(|name| name == &self.public_host)
        {
            return Err(fail("domains", "must include network.public_host"));
        }
        Ok(())
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
            match parsed.host() {
                Some(url::Host::Ipv4(ip)) => {
                    if ip.is_link_local()
                        || ip.is_multicast()
                        || ip.is_unspecified()
                        || ip.is_broadcast()
                    {
                        return Err(fail(
                            "directory",
                            "link-local, multicast and unspecified addresses are forbidden",
                        ));
                    }
                    if (ip.is_private() || ip.is_loopback()) && self.trust_anchor.is_none() {
                        return Err(fail(
                            "trust_anchor",
                            "private ACME directories require a PEM trust anchor",
                        ));
                    }
                }
                Some(url::Host::Ipv6(ip)) => {
                    if ip.is_multicast()
                        || ip.is_unspecified()
                        || (ip.segments()[0] & 0xffc0) == 0xfe80
                        || ip.to_ipv4_mapped().is_some()
                    {
                        return Err(fail(
                            "directory",
                            "link-local, multicast and IPv4-mapped addresses are forbidden",
                        ));
                    }
                    if (ip.is_unique_local() || ip.is_loopback()) && self.trust_anchor.is_none() {
                        return Err(fail(
                            "trust_anchor",
                            "private ACME directories require a PEM trust anchor",
                        ));
                    }
                }
                Some(url::Host::Domain(host)) => {
                    if host == "." || host.is_empty() || !host.contains('.') && host != "localhost"
                    {
                        return Err(fail("directory", "must use a DNS name or IP address"));
                    }
                    if host == "localhost" && self.trust_anchor.is_none() {
                        return Err(fail(
                            "trust_anchor",
                            "local ACME directories require a PEM trust anchor",
                        ));
                    }
                }
                None => return Err(fail("directory", "must contain a host")),
            }
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
