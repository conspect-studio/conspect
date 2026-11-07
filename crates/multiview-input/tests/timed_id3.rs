// Fixture construction deliberately fails loudly on impossible fixture sizes.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use multiview_input::id3::{marker_descriptor, parse, Id3Error};

fn sync(n: u32) -> [u8; 4] {
    [21, 14, 7, 0].map(|shift| u8::try_from((n >> shift) & 127).expect("masked seven bits"))
}
fn frame(version: u8, id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    let len = u32::try_from(payload.len()).expect("fixture length fits u32");
    out.extend(if version == 4 {
        sync(len)
    } else {
        len.to_be_bytes()
    });
    out.extend([0, 0]);
    out.extend(payload);
    out
}
fn tag(version: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![b'I', b'D', b'3', version, 0, 0];
    out.extend(sync(
        u32::try_from(body.len()).expect("fixture length fits u32"),
    ));
    out.extend(body);
    out
}
#[test]
fn both_versions_preserve_unknown_frames_without_break_semantics() {
    for version in [3, 4] {
        let data = tag(
            version,
            &frame(version, b"TXXX", b"\x03vendor\0ad-like-label"),
        );
        let parsed = parse(&data).unwrap();
        assert_eq!(parsed.version, version);
        assert_eq!(parsed.frames.len(), 1);
        assert_eq!(parsed.frames[0].id, *b"TXXX");
        assert_eq!(parsed.frames[0].payload, b"\x03vendor\0ad-like-label");
        assert_eq!(parsed.frames[0].transport_timestamp_90khz, None);
    }
}
#[test]
fn apple_private_timestamp_is_exact_33_bit_not_a_boundary() {
    for timestamp in [0, 1, (1u64 << 33) - 1] {
        let mut payload = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
        payload.extend(timestamp.to_be_bytes());
        let parsed = parse(&tag(4, &frame(4, b"PRIV", &payload))).unwrap();
        assert_eq!(parsed.frames[0].transport_timestamp_90khz, Some(timestamp));
    }
}
#[test]
fn malformed_apple_timestamp_is_not_silently_masked() {
    for bytes in [vec![0; 7], (1u64 << 33).to_be_bytes().to_vec(), vec![0; 9]] {
        let mut payload = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
        payload.extend(bytes);
        assert_eq!(
            parse(&tag(4, &frame(4, b"PRIV", &payload))),
            Err(Id3Error::InvalidTimestamp)
        );
    }
}
#[test]
fn sizes_flags_and_truncation_fail_closed() {
    let valid = tag(4, &frame(4, b"ABCD", b"metadata"));
    for n in 0..valid.len() {
        assert!(parse(&valid[..n]).is_err());
    }
    let mut flagged = valid.clone();
    flagged[5] = 0x80;
    assert_eq!(parse(&flagged), Err(Id3Error::UnsupportedFlags));
    let mut frame_flagged = valid.clone();
    frame_flagged[19] = 1;
    assert_eq!(parse(&frame_flagged), Err(Id3Error::UnsupportedFlags));
    let mut nonsync = valid.clone();
    nonsync[6] = 0x80;
    assert_eq!(parse(&nonsync), Err(Id3Error::InvalidSize));
    let mut frame_nonsync = valid.clone();
    frame_nonsync[14] = 0x80;
    assert_eq!(parse(&frame_nonsync), Err(Id3Error::InvalidSize));
    let mut trailing = valid;
    trailing.push(1);
    assert_eq!(parse(&trailing), Err(Id3Error::InvalidSize));
}
#[test]
fn rejects_invalid_headers_boundaries_and_private_frames() {
    for version in [0, 2, 5, 255] {
        assert!(parse(&tag(version, &[])).is_err());
    }
    for version in [3, 4] {
        for flags in [0x80, 0x40, 0x10, 1] {
            let mut bytes = tag(version, &[]);
            bytes[5] = flags;
            assert!(parse(&bytes).is_err());
        }
        for flags in [1, 2, 4, 8, 0x20, 0x40, 0x80] {
            let mut body = frame(version, b"X123", b"data");
            body[9] = flags;
            assert!(parse(&tag(version, &body)).is_err());
        }
        let mut body = frame(version, b"X123", b"data");
        body[4..8].copy_from_slice(&[255; 4]);
        assert!(parse(&tag(version, &body)).is_err());
        body[4..8].copy_from_slice(&sync(100));
        assert!(parse(&tag(version, &body)).is_err());
        assert!(parse(&tag(version, b"X123")).is_err());
        assert!(parse(&tag(version, &frame(version, b"x123", b"data"))).is_err());
        assert!(parse(&tag(version, &frame(version, b"X123", b""))).is_err());
        assert!(parse(&tag(version, &frame(version, b"PRIV", b"no-terminator"))).is_err());
        let payload = vec![0xfe; 130];
        assert_eq!(
            parse(&tag(version, &frame(version, b"X123", &payload)))
                .unwrap()
                .frames[0]
                .payload,
            payload
        );
    }
    let mut header = tag(4, &[]);
    header[6..10].copy_from_slice(&[127; 4]);
    assert_eq!(parse(&header), Err(Id3Error::TooLarge));
    for index in [0, 1, 2, 4] {
        let mut header = tag(4, &[]);
        header[index] = 255;
        assert!(parse(&header).is_err());
    }
}
#[test]
fn bounded_frames_payloads_and_zero_padding() {
    let mut body = frame(3, b"PRIV", b"unknown-owner\0opaque");
    body.extend([0; 13]);
    assert_eq!(parse(&tag(3, &body)).unwrap().frames.len(), 1);
    body.push(1);
    assert_eq!(parse(&tag(3, &body)), Err(Id3Error::InvalidFrame));
    let max_payload = vec![1; multiview_input::id3::MAX_FRAME_PAYLOAD_BYTES];
    assert!(parse(&tag(4, &frame(4, b"ABCD", &max_payload))).is_ok());
    let over_payload = vec![1; multiview_input::id3::MAX_FRAME_PAYLOAD_BYTES + 1];
    assert_eq!(
        parse(&tag(4, &frame(4, b"ABCD", &over_payload))),
        Err(Id3Error::TooLarge)
    );
    assert!(parse(&tag(4, &vec![0; 65_536])).is_ok());
    assert_eq!(
        parse(&tag(4, &frame(4, b"ABCD", b"x").repeat(256)))
            .unwrap()
            .frames
            .len(),
        256
    );
    let many = frame(4, b"ABCD", b"x").repeat(257);
    assert_eq!(parse(&tag(4, &many)), Err(Id3Error::TooLarge));
    assert_eq!(parse(&tag(4, &vec![0; 65_537])), Err(Id3Error::TooLarge));
}

#[test]
fn apple_transport_timestamp_priv_is_named_a_clock_marker() {
    let mut payload = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
    payload.extend(1_000_u64.to_be_bytes());
    let parsed = parse(&tag(4, &frame(4, b"PRIV", &payload))).unwrap();
    let d = marker_descriptor(&parsed).expect("descriptor");
    assert_eq!(d.name, "Timestamp (ID3)");
    assert_eq!(d.type_id.as_deref(), Some("PRIV"));
    assert_eq!(
        d.detail.as_deref(),
        Some("com.apple.streaming.transportStreamTimestamp")
    );
}

#[test]
fn unrecognized_priv_owner_is_named_by_frame_id_and_owner_text() {
    let payload = b"com.example.vendor\0opaque-bytes";
    let parsed = parse(&tag(4, &frame(4, b"PRIV", payload))).unwrap();
    let d = marker_descriptor(&parsed).expect("descriptor");
    assert_eq!(d.name, "ID3 PRIV");
    assert_eq!(d.type_id.as_deref(), Some("PRIV"));
    assert_eq!(d.detail.as_deref(), Some("com.example.vendor"));
}

#[test]
fn txxx_description_names_the_frame_with_its_description() {
    // encoding byte 3 (UTF-8), description "ad-marker", null, value.
    let parsed = parse(&tag(4, &frame(4, b"TXXX", b"\x03ad-marker\0value"))).unwrap();
    let d = marker_descriptor(&parsed).expect("descriptor");
    assert_eq!(d.name, "ID3 TXXX");
    assert_eq!(d.type_id.as_deref(), Some("TXXX"));
    assert_eq!(d.detail.as_deref(), Some("ad-marker"));
}

#[test]
fn unknown_frame_id_is_named_without_claiming_more_than_the_evidence() {
    let parsed = parse(&tag(4, &frame(4, b"X123", b"opaque"))).unwrap();
    let d = marker_descriptor(&parsed).expect("descriptor");
    assert_eq!(d.name, "ID3 X123");
    assert_eq!(d.type_id.as_deref(), Some("X123"));
    assert_eq!(d.detail, None);
}

fn priv_tag(frames: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (owner, data) in frames {
        let mut payload = owner.as_bytes().to_vec();
        payload.push(0);
        payload.extend_from_slice(data);
        body.extend_from_slice(b"PRIV");
        let size = u32::try_from(payload.len()).unwrap();
        body.extend_from_slice(&[
            0,
            0,
            u8::try_from(size >> 7).unwrap(),
            u8::try_from(size & 0x7f).unwrap(),
        ]);
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&payload);
    }
    let size = u32::try_from(body.len()).unwrap();
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&[
        0,
        0,
        u8::try_from(size >> 7).unwrap(),
        u8::try_from(size & 0x7f).unwrap(),
    ]);
    tag.extend_from_slice(&body);
    tag
}

#[test]
fn elemental_utc_priv_is_a_clock_reference_paired_with_the_apple_pts() {
    let bytes = priv_tag(&[
        (
            multiview_input::id3::APPLE_TS_OWNER,
            &900_000_u64.to_be_bytes(),
        ),
        (
            multiview_input::id3::ELEMENTAL_UTC_OWNER,
            b"2024-03-21T10:02:11Z",
        ),
    ]);
    let tag = multiview_input::id3::parse(&bytes).unwrap();
    assert!(multiview_input::id3::is_clock_only(&tag));
    let clock = multiview_input::id3::clock_reference(&tag).unwrap();
    assert_eq!(clock.utc_ns, 1_711_015_331_000_000_000);
    assert_eq!(clock.pts_90khz, Some(900_000));
    assert_eq!(clock.resolution_ms, 1000);
    let decoded = multiview_input::id3::decode_tag(&tag);
    assert_eq!(
        decoded.frames[1].owner.as_deref(),
        Some(multiview_input::id3::ELEMENTAL_UTC_OWNER)
    );
    assert_eq!(decoded.frames[1].utc_ns, Some(1_711_015_331_000_000_000));
    assert_eq!(decoded.frames[0].transport_timestamp_90khz, Some(900_000));
}

#[test]
fn other_priv_owners_are_events_not_clocks() {
    let bytes = priv_tag(&[("com.example.marker", b"\x01\x02")]);
    let tag = multiview_input::id3::parse(&bytes).unwrap();
    assert!(!multiview_input::id3::is_clock_only(&tag));
    assert!(multiview_input::id3::clock_reference(&tag).is_none());
    assert_eq!(
        multiview_input::id3::decode_tag(&tag).frames[0]
            .data_hex
            .as_deref(),
        Some("0102")
    );
}
