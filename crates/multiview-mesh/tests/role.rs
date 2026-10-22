//! Role determination (ADR-0051 §4, brief §9.2): a machine is `Direct` when it
//! has its own internet path, `Relay` when it is online AND opted-in to relay for
//! neighbours, and `Leaf` when it has no internet path and depends on an adopted
//! relaying neighbour (carrying the `via` peer). The role is a pure function of
//! (own connectivity, relay-opt-in, adopted-relay-available) — no sockets.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_mesh::peer::PeerKey;
use multiview_mesh::role::{determine_role, Connectivity, MeshRole, RoleInputs};

fn via(byte: u8) -> PeerKey {
    PeerKey::from_digest([byte; 32])
}

#[test]
fn online_without_relay_optin_is_direct() {
    let role = determine_role(&RoleInputs {
        connectivity: Connectivity::Online,
        relay_enabled: false,
        via: None,
    });
    assert_eq!(role, MeshRole::Direct);
    assert!(role.via().is_none(), "Direct has no via-peer");
}

#[test]
fn online_with_relay_optin_is_relay() {
    let role = determine_role(&RoleInputs {
        connectivity: Connectivity::Online,
        relay_enabled: true,
        via: None,
    });
    assert_eq!(role, MeshRole::Relay);
}

#[test]
fn offline_with_an_adopted_relay_is_leaf_via_that_peer() {
    let v = via(0x07);
    let role = determine_role(&RoleInputs {
        connectivity: Connectivity::Offline,
        relay_enabled: false,
        via: Some(v.clone()),
    });
    assert_eq!(role, MeshRole::Leaf { via: v.clone() });
    assert_eq!(role.via(), Some(&v), "a leaf carries the via-peer");
}

#[test]
fn offline_without_an_adopted_relay_is_direct_but_disconnected() {
    // No relay available: the machine is not leafed (it has nobody to relay
    // through). It is still Direct in role — it simply cannot reach the server,
    // which is the lease/ladder's concern, not the mesh role's. Crucially it is
    // NOT a Leaf (a Leaf must name a via-peer).
    let role = determine_role(&RoleInputs {
        connectivity: Connectivity::Offline,
        relay_enabled: false,
        via: None,
    });
    assert_eq!(role, MeshRole::Direct);
    assert!(role.via().is_none());
}

#[test]
fn an_offline_relayer_optin_still_leafs_when_a_via_exists() {
    // A machine that is offline cannot actually relay for others (it has no path),
    // so even with relay_enabled it leafs through its adopted neighbour when one
    // exists — relay-opt-in is moot while offline.
    let v = via(0x08);
    let role = determine_role(&RoleInputs {
        connectivity: Connectivity::Offline,
        relay_enabled: true,
        via: Some(v.clone()),
    });
    assert_eq!(role, MeshRole::Leaf { via: v });
}

#[test]
fn the_role_renders_a_stable_kebab_tag() {
    assert_eq!(MeshRole::Direct.kind_str(), "direct");
    assert_eq!(MeshRole::Relay.kind_str(), "relay");
    assert_eq!(MeshRole::Leaf { via: via(0x01) }.kind_str(), "leaf");
}
