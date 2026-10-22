//! Failing-first tests for the sans-IO TURN client against an in-process fake
//! TURN server. No socket: the client emits datagrams + timeouts and consumes
//! received datagrams, so the Allocate / Refresh / `CreatePermission` /
//! `ChannelBind` round-trips are driven entirely in memory (RFC 5766 / RFC 8656).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

mod fake_turn;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use fake_turn::FakeTurnServer;
use multiview_webrtc::turn::message::{Method, StunMessage};
use multiview_webrtc::turn::{TurnClient, TurnCredential, TurnOutput, TurnState};

const SERVER: &str = "[2001:db8::1]:3478";
const RELAY: &str = "[2001:db8::1]:49152";

fn drive_to_allocated(
    client: &mut TurnClient,
    server: &mut FakeTurnServer,
    now: Instant,
) -> SocketAddr {
    // Pump the sans-IO loop: take the client's outgoing datagram, feed it to the
    // fake server, feed the reply back, until the allocation succeeds. Bounded so
    // a regression cannot loop forever.
    for _ in 0..16 {
        match client.poll_output(now) {
            TurnOutput::Transmit { payload, .. } => {
                if let Some(reply) = server.handle(&payload) {
                    client
                        .handle_input(&reply, now)
                        .expect("client accepts the server reply");
                }
            }
            TurnOutput::Timeout(_) | TurnOutput::Idle => {
                if let TurnState::Allocated { relay, .. } = client.state() {
                    return relay;
                }
            }
            // `TurnOutput` is `#[non_exhaustive]`; a future variant is a no-op here.
            _ => {}
        }
        if let TurnState::Allocated { relay, .. } = client.state() {
            return relay;
        }
    }
    panic!("client did not reach Allocated within the bound");
}

#[test]
fn allocate_succeeds_after_the_401_challenge_and_learns_the_relay() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let relay_addr: SocketAddr = RELAY.parse().unwrap();
    let cred = TurnCredential::static_credential("alice", "s3cret", None);
    let mut server = FakeTurnServer::new(server_addr, relay_addr, "alice", "s3cret", "example.org");
    let mut client = TurnClient::new(server_addr, cred);

    // The very first Allocate is sent without credentials (RFC 5766 §6.1): the
    // server replies 401 with REALM + NONCE, and the client retries with auth.
    let relay = drive_to_allocated(&mut client, &mut server, now);
    assert_eq!(relay, relay_addr, "client learns the XOR-RELAYED-ADDRESS");
    assert!(matches!(client.state(), TurnState::Allocated { .. }));
    assert!(
        server.saw_authenticated_allocate(),
        "the retried Allocate carried MESSAGE-INTEGRITY the server accepted"
    );
}

#[test]
fn create_permission_round_trips_for_a_peer() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let relay_addr: SocketAddr = RELAY.parse().unwrap();
    let cred = TurnCredential::static_credential("alice", "s3cret", None);
    let mut server = FakeTurnServer::new(server_addr, relay_addr, "alice", "s3cret", "example.org");
    let mut client = TurnClient::new(server_addr, cred);
    drive_to_allocated(&mut client, &mut server, now);

    let peer: SocketAddr = "[2001:db8::99]:5000".parse().unwrap();
    client.create_permission(peer, now);
    // Pump the CreatePermission exchange.
    for _ in 0..8 {
        if let TurnOutput::Transmit { payload, .. } = client.poll_output(now) {
            if let Some(reply) = server.handle(&payload) {
                client.handle_input(&reply, now).unwrap();
            }
        } else {
            break;
        }
    }
    assert!(
        server.has_permission(&peer),
        "the server installed the permission"
    );
    assert!(client.has_permission(&peer));
}

#[test]
fn refresh_extends_the_allocation_lifetime() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let relay_addr: SocketAddr = RELAY.parse().unwrap();
    let cred = TurnCredential::static_credential("alice", "s3cret", None);
    let mut server = FakeTurnServer::new(server_addr, relay_addr, "alice", "s3cret", "example.org");
    let mut client = TurnClient::new(server_addr, cred);
    drive_to_allocated(&mut client, &mut server, now);

    // Jump near the allocation's expiry; the client must want to refresh.
    let later = now + Duration::from_secs(540);
    client.poll_output(later);
    let refresh_count_before = server.refresh_count();
    for _ in 0..8 {
        if let TurnOutput::Transmit { payload, .. } = client.poll_output(later) {
            if let Some(reply) = server.handle(&payload) {
                client.handle_input(&reply, later).unwrap();
            }
        } else {
            break;
        }
    }
    assert!(
        server.refresh_count() > refresh_count_before,
        "client sent a Refresh as the lifetime neared expiry"
    );
}

#[test]
fn allocate_requests_an_ipv6_relay_family() {
    // Defect D2 (IPv6-first): the very first Allocate the client emits must carry
    // REQUESTED-ADDRESS-FAMILY=IPv6 so an IPv6 peer's CreatePermission does not
    // fail 443/440 against an IPv4-only relay default. We inspect the first
    // datagram off the wire rather than the fake server.
    use multiview_webrtc::turn::message::{AddressFamily, Attribute, StunMessage};
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let cred = TurnCredential::static_credential("alice", "s3cret", None);
    let mut client = TurnClient::new(server_addr, cred);
    let TurnOutput::Transmit { payload, .. } = client.poll_output(now) else {
        panic!("the client emits an Allocate first");
    };
    let msg = StunMessage::parse(&payload).expect("the Allocate parses");
    assert_eq!(msg.method(), Method::Allocate);
    assert!(
        msg.attributes()
            .iter()
            .any(|a| matches!(a, Attribute::RequestedAddressFamily(AddressFamily::Ipv6))),
        "Allocate requests an IPv6 relay (IPv6-first): {:?}",
        msg.attributes()
    );
}

#[test]
fn relayed_send_wraps_application_data_toward_a_peer() {
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let relay_addr: SocketAddr = RELAY.parse().unwrap();
    let cred = TurnCredential::static_credential("alice", "s3cret", None);
    let mut server = FakeTurnServer::new(server_addr, relay_addr, "alice", "s3cret", "example.org");
    let mut client = TurnClient::new(server_addr, cred);
    drive_to_allocated(&mut client, &mut server, now);

    let peer: SocketAddr = "[2001:db8::99]:5000".parse().unwrap();
    let app = [0x01u8, 0x02, 0x03, 0x04];
    // The client wraps an outbound datagram destined for `peer` into a Send
    // indication addressed to the TURN server.
    let wire = client.wrap_send(peer, &app);
    let parsed = StunMessage::parse(&wire).expect("Send indication is valid STUN");
    assert_eq!(parsed.method(), Method::Send);
    assert_eq!(parsed.peer_address(), Some(peer));
    assert_eq!(parsed.data(), Some(app.as_slice()));

    // And it unwraps an inbound Data indication back into (peer, payload).
    let inbound = server.make_data_indication(peer, &app);
    let (from, payload) = client
        .unwrap_data(&inbound)
        .expect("Data indication unwraps");
    assert_eq!(from, peer);
    assert_eq!(payload, app);
}
