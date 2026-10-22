//! `RtpReceiver` multicast-join dispatch (ADR-0042 IPv6-first; `st2110` feature).
//!
//! The AES67 / ST 2110 RX wiring binds a receive socket and joins the media
//! multicast group named on the SDP `c=` line — which may be IPv4 or IPv6. So the
//! pipeline wants ONE family-agnostic call, `rx.join_multicast(group)`, that
//! dispatches on the address family. These tests bind a socket per family and
//! join a real group (this devcontainer's `eth0` is multicast-capable); they run
//! under the `st2110` feature only.
#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use multiview_input::st2110::transport::{MulticastInterface, RtpReceiver};

fn any_v4() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
}

fn any_v6() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
}

#[tokio::test]
async fn join_multicast_dispatches_ipv4_by_family() {
    let rx = RtpReceiver::bind(any_v4()).await.unwrap();
    // The AES67/Dante local-scope group.
    rx.join_multicast(
        IpAddr::V4(Ipv4Addr::new(239, 255, 255, 255)),
        MulticastInterface::Unspecified,
    )
    .expect("v4 group join dispatches to join_multicast_v4");
}

#[tokio::test]
async fn join_multicast_dispatches_ipv6_by_family() {
    let rx = RtpReceiver::bind(any_v6()).await.unwrap();
    // A global-scope IPv6 media group (ADR-0042 c=IN IP6 [ff3e::1]).
    rx.join_multicast(
        IpAddr::V6(Ipv6Addr::new(0xff3e, 0, 0, 0, 0, 0, 0, 1)),
        MulticastInterface::Unspecified,
    )
    .expect("v6 group join dispatches to join_multicast_v6");
}

#[tokio::test]
async fn join_multicast_v6_joins_an_ipv6_group_on_the_default_interface() {
    let rx = RtpReceiver::bind(any_v6()).await.unwrap();
    // The IPv6 SAP group, site-local scope.
    rx.join_multicast_v6(Ipv6Addr::new(0xff05, 0, 0, 0, 0, 0, 2, 0x7ffe), 0)
        .expect("explicit v6 join on interface 0 (default)");
}

#[tokio::test]
async fn join_multicast_threads_the_interface_index_to_the_v6_join() {
    // F6: IPv6 multicast is interface-scoped; the interface index must be
    // SUPPLIED to the join, not hardcoded to 0. Proven by threading a bogus
    // index through and observing the OS reject it — if the index were ignored
    // (old hardcoded 0), the join would spuriously succeed.
    let group = IpAddr::V6(Ipv6Addr::new(0xff3e, 0, 0, 0, 0, 0, 0, 1));

    // Unspecified interface (OS default) still joins on this multicast-capable
    // devcontainer, exactly like the old default.
    let rx = RtpReceiver::bind(any_v6()).await.unwrap();
    rx.join_multicast(group, MulticastInterface::Unspecified)
        .expect("unspecified interface joins on the OS default");

    // A bogus interface index reaches the OS and fails the join.
    let rx2 = RtpReceiver::bind(any_v6()).await.unwrap();
    assert!(
        rx2.join_multicast(group, MulticastInterface::Index(0xFFFF_FFF0))
            .is_err(),
        "a bogus interface index must reach the OS and fail (the index is plumbed, not hardcoded 0)"
    );
}
