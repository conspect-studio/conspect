#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Tests for `AsyncAPI` 3.0 document generation from the multiview-events types
//! (ADR-RT006). Asserts the generated document:
//!   - is valid JSON with `asyncapi: "3.0.0"`
//!   - contains the versioned envelope schema
//!   - declares the documented channels (`ws`, `sse`) + messages for key event types
//!   - is deterministic (calling twice yields identical output)

use multiview_events::asyncapi;
use serde_json::Value;

fn generated_doc() -> Value {
    let json_str = asyncapi::generate_asyncapi_document();
    serde_json::from_str(&json_str).unwrap()
}

#[test]
fn asyncapi_version_is_3_0_0() {
    let doc = generated_doc();
    assert_eq!(
        doc.get("asyncapi").unwrap(),
        "3.0.0",
        "asyncAPI version must be 3.0.0"
    );
}

#[test]
fn document_has_info_block() {
    let doc = generated_doc();
    let info = doc.get("info").unwrap();
    assert!(info.get("title").is_some(), "info block must have a title");
    assert!(
        info.get("version").is_some(),
        "info block must have a version"
    );
}

#[test]
fn document_declares_ws_and_sse_channels() {
    let doc = generated_doc();
    let channels = doc.get("channels").unwrap().as_object().unwrap();
    assert!(
        channels.contains_key("ws"),
        "channels must include `ws` for /api/v1/ws"
    );
    assert!(
        channels.contains_key("sse"),
        "channels must include `sse` for /api/v1/events"
    );
}

#[test]
fn ws_channel_has_websocket_binding() {
    let doc = generated_doc();
    let ws_channel = doc.pointer("/channels/ws").unwrap().as_object().unwrap();
    // The WS binding is injected by post-process because asyncapi-rust lacks it.
    let bindings = ws_channel.get("bindings").unwrap().as_object().unwrap();
    assert!(
        bindings.contains_key("ws"),
        "ws channel must have a ws binding (post-processed)"
    );
}

#[test]
fn messages_block_contains_envelope_schema() {
    let doc = generated_doc();
    // AsyncAPI 3.0: reusable messages live under components.messages.
    let messages = doc
        .pointer("/components/messages")
        .unwrap()
        .as_object()
        .unwrap();
    assert!(
        messages.contains_key("Envelope"),
        "messages must contain an Envelope message definition"
    );
    let envelope_msg = &messages["Envelope"];
    // The payload schema references or inlines the envelope fields.
    let payload = envelope_msg.get("payload").unwrap();
    assert!(
        payload.is_object(),
        "Envelope message must have a payload schema"
    );
}

#[test]
fn messages_block_contains_key_event_types() {
    let doc = generated_doc();
    let messages = doc
        .pointer("/components/messages")
        .unwrap()
        .as_object()
        .unwrap();

    // The core data events from the brief §3 (ADR-RT002 discriminated union).
    for key in &[
        "TileState",
        "AudioMeter",
        "OutputStatus",
        "Alert",
        "InputConnection",
        "JobProgress",
    ] {
        assert!(
            messages.contains_key(*key),
            "messages must contain `{key}` (event type from the wire contract)"
        );
    }
}

#[test]
fn shed_load_message_and_schema_are_present() {
    let doc = generated_doc();
    // The shed-load message must be a reusable message under components.messages.
    let messages = doc
        .pointer("/components/messages")
        .unwrap()
        .as_object()
        .unwrap();
    assert!(
        messages.contains_key("ShedLoad"),
        "components.messages must contain a ShedLoad message (shed.load wire event)"
    );
    let payload_ref = doc
        .pointer("/components/messages/ShedLoad/payload/$ref")
        .and_then(Value::as_str)
        .expect("ShedLoad message must $ref its payload schema");
    assert_eq!(payload_ref, "#/components/schemas/ShedLoad");

    // The payload schema + its enum/union dependencies must be registered.
    for schema in &["ShedLoad", "ShedReason", "ShedScope"] {
        assert!(
            doc.pointer(&format!("/components/schemas/{schema}"))
                .is_some(),
            "components.schemas must contain `{schema}`"
        );
    }
    // The reason enum carries the stable snake_case labels (incl. the live
    // encoder-overload shed) so the wire contract is pinned.
    let reason = doc
        .pointer("/components/schemas/ShedReason/enum")
        .and_then(Value::as_array)
        .expect("ShedReason must be a string enum");
    for label in &[
        "pinned",
        "display_bound",
        "no_better_home",
        "anti_storm",
        "encoder_overload",
    ] {
        assert!(
            reason.iter().any(|v| v.as_str() == Some(*label)),
            "ShedReason enum must include `{label}`"
        );
    }
}

#[test]
fn envelope_schema_has_required_fields() {
    let doc = generated_doc();
    let envelope_payload = doc
        .pointer("/components/messages/Envelope/payload")
        .unwrap();
    let empty = vec![];
    let required = envelope_payload
        .get("required")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();

    // The mandatory envelope fields from the realtime-api brief §2.
    for field in &["v", "t", "topic", "seq", "ts"] {
        assert!(
            required.contains(field),
            "envelope schema must mark `{field}` as required"
        );
    }
}

#[test]
fn document_is_deterministic() {
    // Re-running the generator must yield an identical string (no timestamps,
    // no non-deterministic ordering — idempotency requirement from SUR-6 acceptance).
    let first = asyncapi::generate_asyncapi_document();
    let second = asyncapi::generate_asyncapi_document();
    assert_eq!(
        first, second,
        "AsyncAPI document generation must be deterministic"
    );
}

#[test]
fn document_references_realtime_paths() {
    let doc = generated_doc();
    // Both WS and SSE server paths must appear somewhere in the document.
    let doc_str = serde_json::to_string(&doc).unwrap();
    assert!(
        doc_str.contains("/api/v1/ws"),
        "document must reference the WS endpoint /api/v1/ws"
    );
    assert!(
        doc_str.contains("/api/v1/events"),
        "document must reference the SSE endpoint /api/v1/events"
    );
}

#[test]
fn audio_meter_message_notes_high_rate() {
    // AudioMeter is the sole high-rate conflated topic; its message description
    // must mention this so integrators know to expect conflation.
    let doc = generated_doc();
    let meter_msg = doc.pointer("/components/messages/AudioMeter").unwrap();
    let desc = meter_msg
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    assert!(
        desc.to_lowercase().contains("conflat")
            || desc.to_lowercase().contains("high-rate")
            || desc.to_lowercase().contains("high rate"),
        "AudioMeter message must document its high-rate / conflated nature; got: {desc:?}"
    );
}

// --- AsyncAPI 3.0 structural-validity gate ---
//
// The eight assertions below mirror the eight governance errors reported by
// `@asyncapi/cli validate` against the AsyncAPI 3.0 JSON Schema. A document that
// passes these passes the CI `asyncapi validate` gate; each test names the exact
// spec rule it guards so a future regression points straight at the cause.

#[test]
fn no_top_level_messages_block() {
    // AsyncAPI 3.0 has no top-level `messages` field (Spectral:
    // "Property \"messages\" is not expected to be here"). Reusable messages live
    // under `components.messages`.
    let doc = generated_doc();
    assert!(
        doc.get("messages").is_none(),
        "AsyncAPI 3.0 forbids a top-level `messages` block; it must live under components.messages"
    );
}

#[test]
fn reusable_messages_live_under_components() {
    // The relocated message catalog must carry the Envelope and the key event
    // messages under components.messages.
    let doc = generated_doc();
    let messages = doc
        .pointer("/components/messages")
        .and_then(Value::as_object)
        .expect("components.messages must exist");
    for key in &[
        "Envelope",
        "TileState",
        "AudioMeter",
        "OutputStatus",
        "Alert",
        "InputConnection",
        "JobProgress",
        "TallyEvent",
    ] {
        assert!(
            messages.contains_key(*key),
            "components.messages must contain `{key}`"
        );
    }
}

#[test]
fn channels_reference_components_messages() {
    // Each channel's `messages` map must $ref into components.messages (not a
    // top-level #/messages/... pointer, which no longer exists).
    let doc = generated_doc();
    for channel in &["ws", "sse"] {
        let pointer = format!("/channels/{channel}/messages/EnvelopeMessage/$ref");
        let r = doc
            .pointer(&pointer)
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            r, "#/components/messages/Envelope",
            "channel `{channel}` must reference the Envelope under components.messages"
        );
    }
}

#[test]
fn operation_messages_belong_to_their_channel() {
    // Spectral `asyncapi3-operation-messages-from-referred-channel`: each operation
    // message $ref MUST start with `<operation.channel.$ref>/messages`.
    let doc = generated_doc();
    for (op, channel) in &[("subscribe-ws", "ws"), ("subscribe-sse", "sse")] {
        let channel_ref = doc
            .pointer(&format!("/operations/{op}/channel/$ref"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(channel_ref, format!("#/channels/{channel}"));
        let msg_ref = doc
            .pointer(&format!("/operations/{op}/messages/0/$ref"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            msg_ref.starts_with(&format!("{channel_ref}/messages")),
            "operation `{op}` message {msg_ref:?} must belong to channel {channel_ref:?}"
        );
    }
}

#[test]
fn tally_target_discriminator_is_a_string() {
    // AsyncAPI 3.0 Schema Object `discriminator` is the property NAME (a string),
    // not an OpenAPI-style `{ propertyName }` object.
    let doc = generated_doc();
    let discriminator = doc
        .pointer("/components/schemas/TallyTarget/discriminator")
        .expect("TallyTarget must declare a discriminator");
    assert_eq!(
        discriminator.as_str(),
        Some("kind"),
        "discriminator must be the string property name `kind`, got {discriminator:?}"
    );
}

#[test]
fn external_docs_url_is_absolute() {
    // info.externalDocs.url must be a valid absolute URI (format: uri).
    let doc = generated_doc();
    let url = doc
        .pointer("/info/externalDocs/url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        url.starts_with("https://") || url.starts_with("http://"),
        "externalDocs.url must be an absolute URI, got {url:?}"
    );
}

#[test]
fn envelope_data_oneof_refs_resolve() {
    // The Envelope payload's `data.oneOf` references must all resolve to existing
    // entries in components.schemas (so the resolved document validates).
    let doc = generated_doc();
    let one_of = doc
        .pointer("/components/messages/Envelope/payload/properties/data/oneOf")
        .and_then(Value::as_array)
        .expect("Envelope payload data.oneOf must be an array");
    assert!(!one_of.is_empty(), "data.oneOf must not be empty");
    for entry in one_of {
        let r = entry
            .get("$ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = r
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("oneOf entry must ref components.schemas, got {r:?}"));
        assert!(
            doc.pointer(&format!("/components/schemas/{name}"))
                .is_some(),
            "data.oneOf references missing schema `{name}`"
        );
    }
}

#[test]
fn no_dangling_top_level_message_refs() {
    // After relocation, no `$ref` anywhere may point at the removed top-level
    // `#/messages/...` namespace.
    let doc = generated_doc();
    let serialized = serde_json::to_string(&doc).unwrap();
    assert!(
        !serialized.contains("#/messages/"),
        "no $ref may target the removed top-level #/messages/ namespace"
    );
}
