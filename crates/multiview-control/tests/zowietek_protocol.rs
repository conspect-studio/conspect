//! Socket-free tests over the **grounded** `ZowieTek` wire fixtures copied from
//! upstream issue `aperim/multiview#140` (comment of 2026-06-14).
//!
//! Every fixture below is the shape the vendor API was measured to accept or
//! return. Nothing here touches a socket: the typed models are exercised
//! directly against those bytes, so a driver change that drifts from the
//! grounded protocol fails here before it fails on hardware (DEV-A4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value};

use multiview_control::devices::zowietek::protocol::{
    ChangeWorkmode, DecodeTable, DecoderState, NullDataCall, StreamplaySwitch, WorkmodeIdBody,
};
use multiview_control::devices::zowietek::WorkMode;

// ---- Fixtures, verbatim from the upstream issue comment --------------------

/// `workmode` / `get_workmode_id` reply — the device is in decoder mode.
const WORKMODE_ID_DECODER_BODY: &str = r#"{"workmode_id":1}"#;
/// `workmode` / `get_workmode_id` reply — the device is in encoder mode.
const WORKMODE_ID_ENCODER_BODY: &str = r#"{"workmode_id":0}"#;

/// `streamplay` / `streamplay_get_all` reply `data` array.
const STREAMPLAY_GET_ALL_BODY: &str = r#"[
  {"index":2,"name":"production","switch":1,"streamplay_status":"playing",
   "url":"rtsp://[fd00:db8::42]:8554/main/av"},
  {"index":3,"name":"spare","switch":0,"streamplay_status":"idle",
   "url":"rtsp://[fd00:db8::42]:8554/sub/av"}
]"#;

/// The exact request bodies the grounded protocol requires.
const CHANGE_WORKMODE_REQUEST: &str =
    r#"{"group":"workmode","opt":"change_workmode","data":{"workmode_id":1}}"#;
const STREAMPLAY_SWITCH_REQUEST: &str =
    r#"{"group":"streamplay","opt":"streamplay_switch","data":{"index":3,"switch":1}}"#;
const GET_WORKMODE_ID_REQUEST: &str = r#"{"group":"workmode","opt":"get_workmode_id","data":null}"#;
const STREAMPLAY_GET_ALL_REQUEST: &str =
    r#"{"group":"streamplay","opt":"streamplay_get_all","data":null}"#;
const GET_DECODER_STATE_REQUEST: &str =
    r#"{"group":"streamplay","opt":"get_decoder_state","data":null}"#;

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

// ---- Reads -----------------------------------------------------------------

#[test]
fn the_workmode_read_fixture_maps_ids_to_modes() {
    let decoder: WorkmodeIdBody =
        serde_json::from_str(WORKMODE_ID_DECODER_BODY).expect("grounded decoder reply");
    assert_eq!(decoder.workmode_id, 1, "the fixture reports workmode_id 1");
    assert_eq!(decoder.workmode(), Some(WorkMode::Decoder));

    let encoder: WorkmodeIdBody =
        serde_json::from_str(WORKMODE_ID_ENCODER_BODY).expect("grounded encoder reply");
    assert_eq!(encoder.workmode_id, 0, "the fixture reports workmode_id 0");
    assert_eq!(encoder.workmode(), Some(WorkMode::Encoder));

    // The mapping is the grounded one and is symmetric.
    assert_eq!(WorkMode::Decoder.wire_id(), 1);
    assert_eq!(WorkMode::Encoder.wire_id(), 0);
    assert_eq!(WorkMode::from_wire_id(1), Some(WorkMode::Decoder));
    assert_eq!(WorkMode::from_wire_id(0), Some(WorkMode::Encoder));
    assert_eq!(WorkMode::from_wire_id(7), None);
}

#[test]
fn the_decode_table_fixture_exposes_the_active_and_spare_indices() {
    let table = DecodeTable::parse(&fixture(STREAMPLAY_GET_ALL_BODY));

    assert_eq!(table.rows().len(), 2, "both grounded rows are parsed");
    assert_eq!(
        table.active_index(),
        Some(2),
        "the fixture's production index is the single active decode"
    );
    assert!(table.contains(3), "the spare index is in the table");
    assert!(!table.contains(9), "an absent index is not in the table");

    let spare = table.row(3).expect("spare row");
    assert_eq!(spare.name.as_deref(), Some("spare"));
    assert_eq!(spare.switch, Some(0));
    assert!(!spare.is_active());
    assert_eq!(spare.streamplay_status.as_deref(), Some("idle"));
    assert_eq!(spare.kind(), "rtsp", "kind derives from the grounded url");
}

#[test]
fn an_empty_decode_table_grounds_no_index() {
    let table = DecodeTable::parse(&json!({"unexpected": "shape"}));
    assert!(table.is_empty());
    assert_eq!(table.active_index(), None);
    assert!(!table.contains(0));
}

#[test]
fn the_decoder_state_read_is_lossless_and_invents_no_fields() {
    // The comment grounds the call, not a field set: wrapping any reply must
    // round-trip it untouched rather than assert ungrounded fields.
    let raw = json!({"index": 2, "some": "vendor-specific"});
    let state = DecoderState::new(raw.clone());
    assert_eq!(state.raw(), &raw);
    assert_eq!(
        serde_json::to_value(&state).expect("transparent"),
        raw,
        "the wrapper serializes back to the exact vendor body"
    );
}

// ---- Requests --------------------------------------------------------------

#[test]
fn the_workmode_switch_request_matches_the_grounded_fixture() {
    let body = ChangeWorkmode::to(WorkMode::Decoder).body();
    assert_eq!(body, fixture(CHANGE_WORKMODE_REQUEST));
    assert_eq!(
        serde_json::to_value(ChangeWorkmode { workmode_id: 1 }).expect("serialize"),
        json!({"workmode_id": 1}),
        "the data payload is exactly {{workmode_id:N}}"
    );
}

#[test]
fn the_index_scoped_switch_request_matches_the_grounded_fixture() {
    let body = StreamplaySwitch::activate(3).body();
    assert_eq!(body, fixture(STREAMPLAY_SWITCH_REQUEST));
    assert_eq!(
        serde_json::to_value(StreamplaySwitch::activate(3)).expect("serialize"),
        json!({"index": 3, "switch": 1})
    );
    assert_eq!(
        serde_json::to_value(StreamplaySwitch {
            index: 3,
            switch: 0
        })
        .expect("serialize"),
        json!({"index": 3, "switch": 0})
    );
}

#[test]
fn the_read_calls_match_their_grounded_group_and_opt() {
    assert_eq!(
        NullDataCall::GetWorkmodeId.body(),
        fixture(GET_WORKMODE_ID_REQUEST)
    );
    assert_eq!(
        NullDataCall::StreamplayGetAll.body(),
        fixture(STREAMPLAY_GET_ALL_REQUEST)
    );
    assert_eq!(
        NullDataCall::GetDecoderState.body(),
        fixture(GET_DECODER_STATE_REQUEST)
    );
}

#[test]
fn the_decode_table_enumerate_opt_is_not_the_legacy_opt() {
    // Regression guard for correction (1): a bare `"opt":"streamplay"` returns
    // an empty `data`, so the grounded opt must never regress to the group name.
    let call = NullDataCall::StreamplayGetAll;
    assert_eq!(call.group(), "streamplay");
    assert_eq!(call.opt(), "streamplay_get_all");
    assert_ne!(
        call.opt(),
        call.group(),
        "the legacy driver sent the group name as the opt and read an empty table"
    );
}
