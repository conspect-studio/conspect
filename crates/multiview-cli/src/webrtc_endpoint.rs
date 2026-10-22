//! Shared `[webrtc]` config → `multiview_webrtc::config::EndpointConfig` mapping
//! (ADR-0048 §1/§9) — feature `webrtc-native`.
//!
//! Both the WHIP **ingest** wiring ([`crate::webrtc_ingest`]) and the WHEP
//! **egress** preview wiring ([`crate::whep`]) bind the single native str0m
//! endpoint from the same `[webrtc]` config section: the dual-stack UDP port, the
//! advertised host candidates, the session caps / GC horizons, the CORS
//! allow-list, and the STUN/TURN ICE servers (incl. long-term and ephemeral-REST
//! TURN credentials, ADR-0048 §5.1). `multiview-webrtc` never depends on
//! `multiview-config`, so the translation lives here in the cli — once, shared by
//! both drivers, so the TURN/ICE mapping is never duplicated.

use multiview_webrtc::config::{EndpointConfig, IceServer, TurnCredentials};

/// Map the config `[webrtc]` section onto the crate's plain [`EndpointConfig`]
/// (ADR-0048 §9): the single dual-stack UDP port, the advertised host candidates,
/// the session caps/GC horizons, the CORS allow-list, and the STUN/TURN ICE
/// servers.
#[must_use]
pub fn endpoint_config_from(config: &multiview_config::MultiviewConfig) -> EndpointConfig {
    let w = &config.webrtc;
    let advertised_addresses = w
        .advertised_addresses
        .iter()
        // A bare IP literal becomes a candidate; a hostname (no literal IP) is
        // dropped here — str0m candidates are IPs (DNS resolution is a deploy
        // concern, not a candidate). Config validation already vetted the shape.
        .filter_map(|a| a.parse::<std::net::IpAddr>().ok())
        .collect();
    let ice_servers = w.ice_servers.iter().filter_map(ice_server_from).collect();
    EndpointConfig {
        udp_port: w.udp_port,
        advertised_addresses,
        max_sessions: w.max_sessions,
        session_idle_timeout: std::time::Duration::from_millis(w.session_idle_timeout.millis()),
        tombstone_ttl: multiview_webrtc::config::DEFAULT_TOMBSTONE_TTL,
        cors_allow_origins: w.cors_allow_origins.clone(),
        ice_servers,
    }
}

/// Map one config ICE-server entry onto the crate's [`IceServer`]. The URL's
/// `stun:`/`turn:`/`turns:` scheme + bracketed authority is parsed to a
/// `SocketAddr`; an unparseable entry is dropped (config validation vetted the
/// shape, but a hostname-only TURN URL is not a candidate transport address
/// here). `None` skips the entry rather than failing the whole run.
#[must_use]
pub fn ice_server_from(server: &multiview_config::IceServerConfig) -> Option<IceServer> {
    let addr = parse_ice_url_addr(&server.url)?;
    match server.kind {
        multiview_config::IceServerKindConfig::Stun => Some(IceServer::stun(addr)),
        multiview_config::IceServerKindConfig::Turn => {
            let creds = match (&server.password, &server.static_auth_secret) {
                (Some(password), _) => {
                    let mut c = TurnCredentials::long_term(
                        server.username.clone().unwrap_or_default(),
                        password.clone(),
                    );
                    c.realm.clone_from(&server.realm);
                    c
                }
                (None, Some(secret)) => {
                    let mut c = TurnCredentials::ephemeral_rest(
                        server.username.clone().unwrap_or_default(),
                        secret.clone(),
                    );
                    c.realm.clone_from(&server.realm);
                    c
                }
                // Config validation rejects a credential-less TURN server, so this
                // is unreachable for a validated config; skip rather than panic.
                (None, None) => return None,
            };
            Some(IceServer::turn(addr, creds))
        }
        // `IceServerKindConfig` is `#[non_exhaustive]`: a future kind we cannot
        // map is dropped (the run continues; it is not a candidate transport).
        _ => None,
    }
}

/// Parse a `stun:`/`turn:`/`turns:` URL's transport address into a `SocketAddr`.
/// Strips the scheme and an optional `?transport=` query; brackets an IPv6
/// authority. `None` when the host part is not an IP literal (a DNS name is not a
/// candidate transport address here).
#[must_use]
pub fn parse_ice_url_addr(url: &str) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("stun:")
        .or_else(|| url.strip_prefix("turns:"))
        .or_else(|| url.strip_prefix("turn:"))
        .unwrap_or(url);
    // Drop a `?transport=udp` suffix.
    let authority = rest.split('?').next().unwrap_or(rest);
    authority.parse::<std::net::SocketAddr>().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use multiview_webrtc::config::IceServerKind;

    /// A `[webrtc]` section with a STUN + a long-term TURN + an ephemeral-REST
    /// TURN server, built via the real config-as-code (TOML) path (the structs are
    /// `#[non_exhaustive]`, so a literal is not constructible from the cli crate —
    /// parsing is also closer to how a real config arrives).
    const CONFIG_WITH_ICE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "25/1"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[webrtc]
udp_port = 8189

[[webrtc.ice_servers]]
kind = "stun"
url = "stun:[2001:db8::53]:3478"

[[webrtc.ice_servers]]
kind = "turn"
url = "turn:[2001:db8::55]:3478"
username = "u"
password = "p"
realm = "example.org"

[[webrtc.ice_servers]]
kind = "turn"
url = "turns:[2001:db8::56]:5349"
username = "rest"
static_auth_secret = "topsecret"
"##;

    fn parse(text: &str) -> multiview_config::MultiviewConfig {
        multiview_config::MultiviewConfig::load_from_toml(text).expect("config parses")
    }

    #[test]
    fn maps_stun_and_turn_long_term_and_ephemeral() {
        let config = parse(CONFIG_WITH_ICE);
        let mapped: Vec<IceServer> = config
            .webrtc
            .ice_servers
            .iter()
            .filter_map(ice_server_from)
            .collect();
        assert_eq!(mapped.len(), 3, "all three ICE servers map");
        assert_eq!(mapped[0].kind, IceServerKind::Stun);
        assert_eq!(mapped[1].kind, IceServerKind::Turn);
        assert_eq!(mapped[2].kind, IceServerKind::Turn);
        assert_eq!(
            mapped[1].addr,
            "[2001:db8::55]:3478".parse().unwrap(),
            "the bracketed IPv6 authority parses (IPv6-first)"
        );
    }

    #[test]
    fn parse_ice_url_addr_handles_scheme_and_transport_query() {
        assert_eq!(
            parse_ice_url_addr("turn:[2001:db8::1]:3478?transport=udp"),
            Some("[2001:db8::1]:3478".parse().unwrap())
        );
        assert!(parse_ice_url_addr("turn:turn.example.org:3478").is_none());
    }

    #[test]
    fn endpoint_config_threads_ice_servers_from_webrtc_section() {
        let config = parse(CONFIG_WITH_ICE);
        let ep = endpoint_config_from(&config);
        assert_eq!(
            ep.ice_servers.len(),
            3,
            "every configured ICE server reaches EndpointConfig"
        );
        assert_eq!(ep.udp_port, 8189);
        assert!(
            ep.ice_servers.iter().any(|s| s.kind == IceServerKind::Turn),
            "the TURN servers are present for the in-driver TURN client"
        );
    }
}
