//! The outbound-dial guard for catalogue egress (ADR-M013 applied to IPTV-ORG).
//!
//! Every probe dial — the first URL **and every redirect hop** — is screened with
//! [`net_guard`] before a connection is made: an IP-literal host is screened here
//! (reqwest dials literals without consulting any resolver), a DNS name is screened
//! by [`ScreeningResolver`] on the *resolved* addresses (defeats DNS rebinding) and
//! only the vetted addresses are dialled.
//!
//! Test seam: `DialGuard::for_tests_allowing_loopback_ports` (feature
//! `_test-seams`, never compiled into a release preset, never reachable from
//! config) lets an in-process fixture server on `[::1]:<port>` be dialled while
//! every *other* loopback port and every other blocked range stays blocked.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use multiview_config::device::net_guard::{self, DialPolicy};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Screens dial targets.
#[derive(Debug, Clone)]
pub struct DialGuard {
    policy: Arc<DialPolicy>,
    /// Loopback ports that may be dialled (test seam only; empty in production).
    loopback_ports: Arc<Vec<u16>>,
}

impl DialGuard {
    /// The production guard: the operator [`DialPolicy`], nothing else.
    #[must_use]
    pub fn new(policy: Arc<DialPolicy>) -> Self {
        Self {
            policy,
            loopback_ports: Arc::new(Vec::new()),
        }
    }

    /// TEST SEAM: additionally allow dialling loopback on exactly these ports.
    #[cfg(feature = "_test-seams")]
    #[must_use]
    pub fn for_tests_allowing_loopback_ports(ports: &[u16]) -> Self {
        Self {
            policy: Arc::new(DialPolicy::deny_all_allowlistable()),
            loopback_ports: Arc::new(ports.to_vec()),
        }
    }

    /// Screen one URL before dialling it.
    ///
    /// # Errors
    ///
    /// A message when the scheme is not http(s), the host is missing, or an IP
    /// literal falls in a blocked range.
    pub fn screen_url(&self, url: &url::Url) -> Result<(), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("scheme {:?} is not dialled", url.scheme()));
        }
        let port = url.port_or_known_default().unwrap_or(0);
        match url.host() {
            Some(url::Host::Ipv4(v4)) => self.screen_ip(IpAddr::V4(v4), port),
            Some(url::Host::Ipv6(v6)) => self.screen_ip(IpAddr::V6(v6), port),
            Some(url::Host::Domain(_)) => Ok(()),
            None => Err("url has no host".to_owned()),
        }
    }

    /// Screen one resolved address before dialling it.
    ///
    /// # Errors
    ///
    /// A message when the address falls in a blocked range.
    pub fn screen_addr(&self, ip: IpAddr, port: u16) -> Result<(), String> {
        self.screen_ip(ip, port)
    }

    fn screen_ip(&self, ip: IpAddr, port: u16) -> Result<(), String> {
        if ip.is_loopback() && self.loopback_ports.contains(&port) {
            return Ok(());
        }
        net_guard::screen_ip(ip, &self.policy).map_err(|e| e.to_string())
    }

    /// The reqwest DNS resolver that screens every resolved address.
    #[must_use]
    pub fn resolver(&self) -> Arc<ScreeningResolver> {
        Arc::new(ScreeningResolver {
            policy: Arc::clone(&self.policy),
        })
    }
}

/// reqwest DNS resolver that refuses any answer containing a blocked address.
#[derive(Debug)]
pub struct ScreeningResolver {
    policy: Arc<DialPolicy>,
}

impl reqwest::dns::Resolve for ScreeningResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = Arc::clone(&self.policy);
        Box::pin(async move {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|e| -> BoxError { Box::new(e) })?
                .collect();
            net_guard::screen_resolved(resolved.iter().map(SocketAddr::ip), &policy)
                .map_err(|e| -> BoxError { Box::new(e) })?;
            let addrs: reqwest::dns::Addrs = Box::new(resolved.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> DialGuard {
        DialGuard::new(Arc::new(DialPolicy::allow_lan()))
    }

    #[test]
    fn blocks_loopback_imds_and_non_http() {
        let g = guard();
        for u in [
            "http://[::1]:8080/x",
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://[::ffff:127.0.0.1]/x",
            "http://2130706433/x",
            "ftp://example.com/x",
        ] {
            assert!(g.screen_url(&url::Url::parse(u).unwrap()).is_err(), "{u}");
        }
        assert!(g
            .screen_url(&url::Url::parse("https://[2001:db8::1]/x").unwrap())
            .is_ok());
        assert!(g
            .screen_url(&url::Url::parse("https://example.com/x").unwrap())
            .is_ok());
    }
}
