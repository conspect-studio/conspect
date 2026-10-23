use multiview_cli::network_warn::{has_host_networking, in_container, NetworkEnvironment};
use std::net::IpAddr;

#[test]
fn container_without_host_net_is_recognized_but_host_net_is_not_a_mismatch() {
    let bridge: [IpAddr; 1] = ["172.17.0.2".parse().unwrap()];
    let isolated = NetworkEnvironment {
        dockerenv: true,
        ipv4_addresses: Some(&bridge),
        ..Default::default()
    };
    assert!(in_container(&isolated));
    assert!(!has_host_networking(&isolated));

    let shared = NetworkEnvironment {
        dockerenv: true,
        self_netns_inode: Some(5),
        pid1_netns_inode: Some(5),
        ipv4_addresses: Some(&bridge),
        ..Default::default()
    };
    assert!(has_host_networking(&shared));
}
