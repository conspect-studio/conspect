//! Startup advisory for multicast configured inside a container without host networking.
use std::{
    fs,
    net::{IpAddr, SocketAddr},
    path::Path,
};

use multiview_config::{MultiviewConfig, Output, SourceKind};

/// Container and network signals injectable for deterministic tests.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct NetworkEnvironment<'a> {
    /// Container marker files and procfs cgroup/environ contents.
    pub dockerenv: bool,
    /// Podman/container marker presence.
    pub containerenv: bool,
    /// Procfs cgroup content, if available.
    pub cgroup: Option<&'a str>,
    /// PID 1 environment bytes, if accessible.
    pub pid1_environ: Option<&'a [u8]>,
    /// Kubernetes service host variable presence.
    pub kubernetes_service_host: bool,
    /// Codespaces variable presence.
    pub codespaces: bool,
    /// Remote Containers variable presence.
    pub remote_containers: bool,
    /// Current network namespace inode (diagnostic only; not a host-network signal).
    pub self_netns_inode: Option<u64>,
    /// PID 1 network namespace inode (PID 1 may be the container's init).
    pub pid1_netns_inode: Option<u64>,
    /// Local non-loopback IPv4 and IPv6 addresses, when readable.
    pub ipv4_addresses: Option<&'a [IpAddr]>,
}

/// Whether any available signal indicates container execution.
#[must_use]
pub fn in_container(env: &NetworkEnvironment<'_>) -> bool {
    env.dockerenv
        || env.containerenv
        || env.cgroup.is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            ["docker", "containerd", "kubepods", "lxc", "podman"]
                .iter()
                .any(|x| s.contains(x))
        })
        || env
            .pid1_environ
            .is_some_and(|b| b.split(|x| *x == 0).any(|v| v.starts_with(b"container=")))
        || env.kubernetes_service_host
        || env.codespaces
        || env.remote_containers
}

fn is_default_bridge_address(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let octets = v.octets();
            (octets[0] == 172 && octets[1] == 17) || (octets[0] == 10 && octets[1] == 88)
        }
        IpAddr::V6(v) => v.octets()[0] == 0xfd,
    }
}

/// Whether the visible addresses are exclusively known container-bridge addresses.
/// Unreadable, empty, or loopback-only lists are unknown, not evidence of a bridge.
#[must_use]
pub fn has_container_bridge_networking(env: &NetworkEnvironment<'_>) -> bool {
    let Some(addresses) = env.ipv4_addresses else {
        return false;
    };
    addresses.iter().any(is_default_bridge_address)
        && addresses.iter().all(|ip| {
            ip.is_loopback()
                || ip.is_unspecified()
                || matches!(ip, IpAddr::V6(v) if v.is_unicast_link_local())
                || is_default_bridge_address(ip)
        })
}

/// Heuristic host-network classification from visible non-loopback addresses.
///
/// Matching PID 1's network namespace does *not* prove host networking: PID 1
/// usually lives inside the same container. Only known default bridge addresses
/// (Docker 172.17/16, Podman 10.88/16, default Docker IPv6 ULA `fd00::/8`)
/// indicate isolation. Other addresses suggest host networking, but custom
/// bridges can use any subnet and a host can itself use these bridge ranges;
/// neither namespace identity nor addresses can prove the topology. Unreadable
/// or empty address lists remain unknown: the caller must not warn for those.
#[must_use]
pub fn has_host_networking(env: &NetworkEnvironment<'_>) -> bool {
    let Some(addresses) = env.ipv4_addresses else {
        return false;
    };
    addresses.iter().any(|ip| {
        if ip.is_loopback() || ip.is_unspecified() {
            return false;
        }
        !is_default_bridge_address(ip) && !matches!(ip, IpAddr::V6(v) if v.is_unicast_link_local())
    })
}

fn multicast_host(host: &str) -> bool {
    let cleaned = host.trim_start_matches('@').trim_matches(['[', ']']);
    cleaned.parse::<IpAddr>().is_ok_and(|ip| ip.is_multicast())
}
fn aes67_multicast(group_port: &str) -> bool {
    group_port
        .parse::<SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_multicast())
}
fn url_multicast(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest
        .trim_start_matches('@')
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    multicast_host(host)
}

/// True when a configured source/output targets a multicast group.
/// Keep this predicate as the single extension point for future typed transports.
#[must_use]
pub fn config_uses_multicast(config: &MultiviewConfig) -> bool {
    config.sources.iter().any(|s| match &s.kind {
        SourceKind::Ts { url } => url_multicast(url),
        SourceKind::Aes67 { multicast, .. } => multicast.as_deref().is_some_and(aes67_multicast),
        _ => false,
    }) || config.outputs.iter().any(|o| match o {
        Output::Aes67 { multicast, .. } => aes67_multicast(multicast),
        _ => false,
    })
}

/// Gather container/network environment using procfs and standard marker files.
#[must_use]
pub fn current_environment() -> NetworkEnvironment<'static> {
    // Keep procfs buffers owned/leaked for the lifetime of this one-shot startup probe.
    let cgroup = fs::read_to_string("/proc/1/cgroup")
        .ok()
        .map(|s| &*Box::leak(s.into_boxed_str()));
    let environ = fs::read("/proc/1/environ")
        .ok()
        .map(|s| &*Box::leak(s.into_boxed_slice()));
    let self_inode = fs::metadata("/proc/self/ns/net").ok().map(|m| inode(&m));
    let pid_inode = fs::metadata("/proc/1/ns/net").ok().map(|m| inode(&m));
    let ips = local_ipv4_addresses();
    let ips: Option<&'static [IpAddr]> = ips.map(|ips| &*Box::leak(ips.into_boxed_slice()));
    NetworkEnvironment {
        dockerenv: Path::new("/.dockerenv").exists(),
        containerenv: Path::new("/run/.containerenv").exists(),
        cgroup,
        pid1_environ: environ,
        kubernetes_service_host: std::env::var_os("KUBERNETES_SERVICE_HOST").is_some(),
        codespaces: std::env::var_os("CODESPACES").is_some(),
        remote_containers: std::env::var_os("REMOTE_CONTAINERS").is_some(),
        self_netns_inode: self_inode,
        pid1_netns_inode: pid_inode,
        ipv4_addresses: ips,
    }
}
#[cfg(unix)]
fn inode(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}
#[cfg(not(unix))]
fn inode(_: &fs::Metadata) -> u64 {
    0
}
fn local_ipv4_addresses() -> Option<Vec<IpAddr>> {
    let ipv4 = parse_fib_trie(&fs::read_to_string("/proc/net/fib_trie").ok()?);
    let ipv6 = parse_if_inet6(&fs::read_to_string("/proc/net/if_inet6").ok()?)?;
    Some(ipv4.into_iter().chain(ipv6).collect())
}

fn parse_if_inet6(text: &str) -> Option<Vec<IpAddr>> {
    let mut result = Vec::new();
    for line in text.lines() {
        let raw = line.split_whitespace().next()?;
        if raw.len() != 32 {
            return None;
        }
        let mut octets = [0_u8; 16];
        for (i, octet) in octets.iter_mut().enumerate() {
            *octet = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).ok()?;
        }
        let ip = IpAddr::V6(std::net::Ipv6Addr::from(octets));
        if !ip.is_loopback() && !result.contains(&ip) {
            result.push(ip);
        }
    }
    Some(result)
}

fn parse_fib_trie(text: &str) -> Vec<IpAddr> {
    let mut result = Vec::new();
    let mut candidate = None;
    for line in text.lines() {
        if let Some((_, raw)) = line.split_once("|-- ") {
            candidate = raw.trim().parse::<IpAddr>().ok().filter(IpAddr::is_ipv4);
        } else if line.contains("/32 host LOCAL") {
            if let Some(ip) = candidate.take().filter(|ip| !ip.is_loopback()) {
                if !result.contains(&ip) {
                    result.push(ip);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn container_network_and_multicast_predicates_require_both_halves() {
        let ips = ["172.17.0.2".parse().unwrap()];
        let env = NetworkEnvironment {
            dockerenv: true,
            ipv4_addresses: Some(&ips),
            ..Default::default()
        };
        assert!(in_container(&env));
        assert!(!has_host_networking(&env));
        let real_ips = [
            "192.168.1.24".parse().unwrap(),
            "10.10.3.15".parse().unwrap(),
        ];
        let host = NetworkEnvironment {
            self_netns_inode: Some(3),
            pid1_netns_inode: Some(3),
            ipv4_addresses: Some(&real_ips),
            ..Default::default()
        };
        assert!(has_host_networking(&host));
        assert!(!in_container(&host));
    }
    #[test]
    fn unknown_network_inputs_never_claim_host_networking() {
        assert!(!has_host_networking(&NetworkEnvironment::default()));
    }

    #[test]
    fn parses_procfs_interface_addresses() {
        let ipv6 = "00000000000000000000000000000001 01 80 10 80       lo";
        assert_eq!(parse_if_inet6(ipv6).unwrap(), Vec::<IpAddr>::new());
        assert_eq!(parse_if_inet6("broken"), None);
    }

    #[test]
    fn parses_fib_trie_only_local_addresses() {
        let fixture = "|-- 172.17.0.2\n /32 host LOCAL\n|-- 172.17.0.2\n /24 link\n|-- 10.0.0.4\n";
        assert_eq!(
            parse_fib_trie(fixture),
            vec!["172.17.0.2".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn parses_multicast_urls() {
        assert!(url_multicast("udp://@239.1.2.3:5000"));
        assert!(url_multicast("rtp://[ff3e::1]:5000"));
        assert!(!url_multicast("udp://127.0.0.1:5000"));
    }
}
