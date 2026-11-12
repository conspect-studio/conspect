#![cfg(unix)]
use multiview_webengine::cdp_wire::{encode, Decoder, WireError};
use serde_json::json;

#[test]
fn fragmented_and_adjacent_objects_are_delivered_individually() {
    let mut decoder = Decoder::new(128);
    let first = encode(&json!({"id": 1, "result": {"text": "a\u{0000}b"}}), 128).expect("encode");
    let second = encode(&json!({"method": "Page.loadEventFired"}), 128).expect("encode");
    let mut messages = Vec::new();
    for byte in first.into_iter().chain(second) {
        if let Some(message) = decoder.push(byte).expect("valid fragment") {
            messages.push(message);
        }
    }
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages.first(),
        Some(&json!({"id": 1,"result":{"text":"a\u{0000}b"}}))
    );
    decoder.finish().expect("complete EOF");
}

#[test]
fn oversized_and_invalid_messages_poison_the_session() {
    let mut decoder = Decoder::new(2);
    assert_eq!(decoder.push(b'{'), Ok(None));
    assert_eq!(decoder.push(b'}'), Ok(None));
    assert_eq!(decoder.push(b' '), Err(WireError::TooLarge));
    assert_eq!(decoder.push(0), Err(WireError::Poisoned));
    let mut decoder = Decoder::new(128);
    assert_eq!(decoder.push(0), Err(WireError::Empty));
    let mut decoder = Decoder::new(128);
    for byte in b"[]" {
        decoder.push(*byte).expect("fragment");
    }
    assert_eq!(decoder.push(0), Err(WireError::InvalidMessage));
}

#[test]
fn eof_never_accepts_a_partial_response() {
    let mut decoder = Decoder::new(128);
    decoder.push(b'{').expect("fragment");
    assert_eq!(decoder.finish(), Err(WireError::Truncated));
}

#[test]
fn outgoing_messages_are_bounded_before_payload_allocation() {
    assert_eq!(encode(&json!({"x":"long"}), 2), Err(WireError::TooLarge));
    assert_eq!(encode(&json!(7), 10), Err(WireError::InvalidMessage));
    assert_eq!(encode(&json!({}), 2).expect("exact bound"), b"{}\0");
}
