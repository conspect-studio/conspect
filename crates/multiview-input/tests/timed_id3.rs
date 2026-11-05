// Fixture construction deliberately fails loudly on impossible fixture sizes.
#![allow(clippy::expect_used)]

use multiview_input::id3::{parse, Id3Error};

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
