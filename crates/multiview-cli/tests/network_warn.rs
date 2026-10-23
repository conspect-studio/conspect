#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use multiview_cli::network_warn::{
    config_uses_multicast, has_container_bridge_networking, has_host_networking, in_container,
    NetworkEnvironment,
};
use multiview_config::{MultiviewConfig, Output, SourceKind};
use multiview_control::{emit_network_warnings, EngineStateSnapshot, NetworkMismatchView};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use std::net::IpAddr;

fn warning_count(env: &NetworkEnvironment<'_>, multicast: bool) -> usize {
    let mut config = MultiviewConfig::load_from_toml(include_str!("../../../examples/pip.toml"))
        .expect("example config parses");
    if multicast {
        config.sources[0].kind = SourceKind::Ts {
            url: "udp://@239.1.2.3:5000".to_owned(),
        };
    }
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
    let mut subscriber = publisher.subscribe();
    let mismatch = (in_container(env)
        && has_container_bridge_networking(env)
        && config_uses_multicast(&config))
    .then_some(NetworkMismatchView);
    let emitted = emit_network_warnings(&publisher, mismatch, 123);
    let received = usize::from(subscriber.try_recv().is_ok());
    assert_eq!(received, emitted, "exactly the emitted events are received");
    assert!(subscriber.try_recv().is_err(), "no duplicate warning");
    emitted
}

#[test]
fn equal_pid1_inodes_in_a_bridge_container_still_warn_once_for_multicast() {
    let bridge: [IpAddr; 1] = ["172.17.0.2".parse().unwrap()];
    let env = NetworkEnvironment {
        dockerenv: true,
        self_netns_inode: Some(5),
        pid1_netns_inode: Some(5),
        ipv4_addresses: Some(&bridge),
        ..Default::default()
    };
    assert!(in_container(&env));
    assert!(!has_host_networking(&env));
    assert_eq!(warning_count(&env, true), 1);
    assert_eq!(warning_count(&env, false), 0);
}

#[test]
fn aes67_bare_ipv6_and_ipv4_multicast_warn_once_on_bridge() {
    let bridge: [IpAddr; 1] = ["172.17.0.2".parse().unwrap()];
    let env = NetworkEnvironment {
        dockerenv: true,
        ipv4_addresses: Some(&bridge),
        ..Default::default()
    };
    for group in ["[ff3e::1]:5004", "239.1.2.3:5004"] {
        let mut config =
            MultiviewConfig::load_from_toml(include_str!("../../../examples/pip.toml"))
                .expect("example config parses");
        config.sources[0].kind = SourceKind::Aes67 {
            sdp: "m=audio 5004 RTP/AVP 98".to_owned(),
            session_id: None,
            multicast: Some(group.to_owned()),
            link_offset_ms: None,
            ptp_domain: None,
        };
        assert!(config_uses_multicast(&config), "AES67 source {group}");
        config.sources[0].kind = SourceKind::Ts {
            url: "udp://127.0.0.1:5000".to_owned(),
        };
        let output: Output = toml::from_str(&format!(
            "kind = \"aes67\"\nlabel = \"Studio AES67\"\nmulticast = \"{group}\"\n"
        ))
        .expect("AES67 output parses");
        config.outputs.push(output);
        assert!(config_uses_multicast(&config), "AES67 output {group}");
        let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
        let mut subscriber = publisher.subscribe();
        let mismatch = (in_container(&env)
            && has_container_bridge_networking(&env)
            && config_uses_multicast(&config))
        .then_some(NetworkMismatchView);
        assert_eq!(
            emit_network_warnings(&publisher, mismatch, 123),
            1,
            "{group}"
        );
        assert!(subscriber.try_recv().is_ok(), "{group}");
        assert!(subscriber.try_recv().is_err(), "no duplicate for {group}");
    }
}

#[test]
fn host_network_container_with_real_interfaces_does_not_warn() {
    let addresses = [
        "192.168.1.24".parse().unwrap(),
        "10.10.3.15".parse().unwrap(),
    ];
    let env = NetworkEnvironment {
        dockerenv: true,
        self_netns_inode: Some(5),
        pid1_netns_inode: Some(5),
        ipv4_addresses: Some(&addresses),
        ..Default::default()
    };
    assert!(in_container(&env));
    assert!(has_host_networking(&env));
    assert_eq!(warning_count(&env, true), 0);
}

#[test]
fn podman_and_ipv6_bridge_only_containers_warn_but_unknown_never_warns() {
    for address in ["10.88.0.12", "fd00::abcd"] {
        let addresses = [address.parse().unwrap()];
        let env = NetworkEnvironment {
            containerenv: true,
            ipv4_addresses: Some(&addresses),
            ..Default::default()
        };
        assert!(!has_host_networking(&env), "{address}");
        assert_eq!(warning_count(&env, true), 1, "{address}");
    }
    assert_eq!(warning_count(&NetworkEnvironment::default(), true), 0);
    let unreadable = NetworkEnvironment {
        dockerenv: true,
        ..Default::default()
    };
    assert_eq!(warning_count(&unreadable, true), 0);
}
