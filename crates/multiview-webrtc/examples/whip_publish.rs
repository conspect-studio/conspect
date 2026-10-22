//! Example: a TURN-enabled WHIP-publish handshake, driven sans-IO in memory.
//!
//! This shows the shape a `whip_push` output ([ADR-0049]) wires: build an
//! [`EndpointConfig`] carrying a STUN + a TURN server (the operator's
//! NAT-traversal requirement), create a sendonly [`Session`], gather host +
//! relay candidates, and produce the WHIP offer SDP. It runs offline (no socket,
//! no live TURN server) so it doubles as runnable documentation.
//!
//! Run with: `cargo run -p multiview-webrtc --features native --example whip_publish`
//!
//! [ADR-0049]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0049.md
// An example is runnable documentation; printing the SDP it builds to stdout is
// the whole point, so the workspace `print_stdout` deny is relaxed here.
#![allow(clippy::print_stdout)]

use std::time::Instant;

use multiview_webrtc::config::{EndpointConfig, IceServer, TurnCredentials};
use multiview_webrtc::session::SessionId;
use multiview_webrtc::signalling::{SignalKind, SignalledAnswer};
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let now = Instant::now();

    // 1. Endpoint config with a STUN server and a TURN relay (coturn-style
    //    ephemeral REST credentials — the shared secret is a SECRET the config
    //    redactor strips). IPv6-first addresses per ADR-0042.
    let config = EndpointConfig {
        udp_port: 8189,
        ice_servers: vec![
            IceServer::stun("[2001:db8::53]:3478".parse()?),
            IceServer::turn(
                "[2001:db8::55]:3478".parse()?,
                TurnCredentials::ephemeral_rest("multiview", "shared-secret-from-1password"),
            ),
        ],
        ..EndpointConfig::default()
    };
    config.validate()?;
    println!(
        "endpoint config validated; {} ICE server(s)",
        config.ice_servers.len()
    );
    println!("bind (dual-stack): {}", config.bind_addr());

    // 2. The TURN credential resolved for this allocation (ephemeral REST):
    //    username = "<expiry>:multiview", password = base64(HMAC-SHA1(...)).
    if let Some(turn) = config.turn_servers().next() {
        if let Some(creds) = &turn.credentials {
            let resolved = creds.resolve(1_700_000_000);
            println!("TURN username for this allocation: {}", resolved.username);
        }
    }

    // 3. A sendonly publisher session offering video + audio.
    let mut session = Session::new(&SessionConfig::default(), now);
    // A candidate must be a *concrete* reachable address, never the unspecified
    // `[::]` bind address: the live endpoint advertises its bound port on each
    // gathered/advertised IP. Here we use a concrete v6 host + the relay address
    // a TURN Allocate would yield (illustrative; the live endpoint learns it from
    // the TURN client). The relay is the NAT-traversal last resort, ordered
    // lowest in priority.
    let host: std::net::SocketAddr = "[2001:db8::15]:8189".parse()?;
    session.add_host_candidate(host)?;
    session.add_relay_candidate("[2001:db8::55]:50000".parse()?, host)?;

    let offer = session.create_offer(&[MediaKind::Video, MediaKind::Audio])?;
    println!("\n--- WHIP offer SDP ({} bytes) ---", offer.len());
    println!("{offer}");

    // 4. The WHIP server would answer; here we model the resource the client
    //    PATCHes/DELETEs. The signalling types are framework-agnostic.
    let answer = SignalledAnswer::created(
        SignalKind::Whip,
        "program",
        &SessionId::random(),
        "v=0\r\n... (the WHIP server's answer SDP) ...".to_owned(),
    );
    println!(
        "\nWHIP resource: {}",
        answer.location.as_deref().unwrap_or("<none>")
    );
    Ok(())
}
