//! Property tests for the pure NMOS JSON models (IS-04/05/08) and the IS-10
//! authorization claims/validation logic.
//!
//! These pin the serde round-trip and validation invariants the NMOS Node API
//! relies on, exhaustively over generated inputs — no sockets, no async.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use multiview_control::nmos::is04::{MediaFormat, Receiver, ResourceCore};
use multiview_control::nmos::is05::{
    Activation, ActivationMode, ConnectionRequest, ConnectionState, TransportParams,
};
use multiview_control::nmos::is08::{ChannelMap, ChannelSource};
use multiview_control::nmos::is10::{Is10Claims, NmosAccess, NmosApiClaim};
use multiview_control::Role;
use proptest::prelude::*;

fn any_media_format() -> impl Strategy<Value = MediaFormat> {
    prop_oneof![
        Just(MediaFormat::Video),
        Just(MediaFormat::Audio),
        Just(MediaFormat::Data),
    ]
}

fn any_transport_params() -> impl Strategy<Value = TransportParams> {
    (
        proptest::option::of("[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}"),
        proptest::option::of(any::<u16>()),
        proptest::option::of("[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}"),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(
            |(destination_ip, destination_port, source_ip, rtp_enabled)| TransportParams {
                destination_ip,
                destination_port,
                source_ip,
                rtp_enabled,
            },
        )
}

proptest! {
    /// An IS-04 receiver round-trips through JSON exactly, with the core fields
    /// flattened onto the resource object.
    #[test]
    fn is04_receiver_round_trips(
        id in "[a-z0-9-]{1,24}",
        label in "[ -~]{0,32}",
        format in any_media_format(),
        subscribed in proptest::option::of("[a-z0-9-]{1,24}"),
    ) {
        let rcv = Receiver {
            core: ResourceCore::new(id, "1700000000:0", label),
            device_id: "dev-1".to_owned(),
            format,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            subscribed_sender: subscribed,
        };
        let json = serde_json::to_value(&rcv).expect("serializes");
        let back: Receiver = serde_json::from_value(json).expect("deserializes");
        prop_assert_eq!(back, rcv);
    }

    /// An IS-05 connection request round-trips through JSON for any transport
    /// params and activation mode.
    #[test]
    fn is05_connection_request_round_trips(
        params in proptest::collection::vec(any_transport_params(), 0..3),
        enable in proptest::option::of(any::<bool>()),
    ) {
        let req = ConnectionRequest {
            master_enable: enable,
            activation: Activation::immediate(),
            transport_params: params,
            sender_id: Some("snd-1".to_owned()),
            transport_file: None,
        };
        let json = serde_json::to_value(&req).expect("serializes");
        let back: ConnectionRequest = serde_json::from_value(json).expect("deserializes");
        prop_assert_eq!(back, req);
    }

    /// Staging an immediate-activation request always moves the staged params to
    /// active and clears the staged slot; a non-immediate request never does.
    #[test]
    fn is05_immediate_activation_applies_params(
        params in proptest::collection::vec(any_transport_params(), 0..3),
        immediate in any::<bool>(),
    ) {
        let mut state = ConnectionState::default();
        let activation = if immediate {
            Activation::immediate()
        } else {
            Activation {
                mode: Some(ActivationMode::ActivateScheduledAbsolute),
                requested_time: Some("1700001000:0".to_owned()),
            }
        };
        let req = ConnectionRequest {
            master_enable: Some(true),
            activation,
            transport_params: params.clone(),
            sender_id: None,
            transport_file: None,
        };
        state.stage(req);
        let activated = state.activate_if_immediate();
        prop_assert_eq!(activated, immediate);
        if immediate {
            prop_assert_eq!(&state.active, &params);
            prop_assert!(state.staged.is_none());
        } else {
            prop_assert!(state.active.is_empty());
            prop_assert!(state.staged.is_some());
        }
    }

    /// An IS-08 channel map round-trips and validation accepts exactly the maps
    /// whose every routed channel references a declared input.
    #[test]
    fn is08_channel_map_round_trips_and_validates(
        inputs in proptest::collection::vec("[a-z]{1,6}", 1..4),
        route_to in proptest::sample::select(vec![0usize, 1, 2, 99]),
    ) {
        let mut map = ChannelMap::new(inputs.clone());
        // Route out-0 to the input at index `route_to` (99 = a phantom input).
        let input_id = inputs.get(route_to).cloned().unwrap_or_else(|| "phantom".to_owned());
        map.assign("out-0", ChannelSource::routed(input_id.clone(), 0));

        let json = serde_json::to_value(&map).expect("serializes");
        let back: ChannelMap = serde_json::from_value(json).expect("deserializes");
        prop_assert_eq!(&back, &map);

        let valid = inputs.contains(&input_id);
        prop_assert_eq!(map.validate().is_ok(), valid);
    }

    /// IS-10 expiry is enforced exactly at the boundary: a token is valid iff
    /// `now < exp` (and issuer/audience match).
    #[test]
    fn is10_expiry_is_enforced(now in 0i64..3_000_000_000, exp in 0i64..3_000_000_000) {
        let mut access = BTreeMap::new();
        access.insert("connection".to_owned(), NmosAccess::Write);
        let claims = Is10Claims {
            iss: "iss".to_owned(),
            sub: "sub".to_owned(),
            aud: vec!["multiview".to_owned()],
            exp,
            iat: 0,
            x_nmos_api: NmosApiClaim { version: "1.0".to_owned(), access },
        };
        let result = claims.validate(now, "iss", "multiview");
        prop_assert_eq!(result.is_ok(), exp > now);
    }

    /// IS-10 access maps deterministically onto a role: write→Operator,
    /// read→Viewer, and a token never escalates to Admin.
    #[test]
    fn is10_access_maps_to_role(write in any::<bool>()) {
        let mut access = BTreeMap::new();
        let level = if write { NmosAccess::Write } else { NmosAccess::Read };
        access.insert("connection".to_owned(), level);
        let claims = Is10Claims {
            iss: "iss".to_owned(),
            sub: "sub".to_owned(),
            aud: vec!["multiview".to_owned()],
            exp: 2_000_000_000,
            iat: 0,
            x_nmos_api: NmosApiClaim { version: "1.0".to_owned(), access },
        };
        let role = claims.role_for("connection").expect("granted");
        let expected = if write { Role::Operator } else { Role::Viewer };
        prop_assert_eq!(role, expected);
        prop_assert_ne!(role, Role::Admin);
    }
}
