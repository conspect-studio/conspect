//! Full SCTE-35 journal decode against spec-shaped sections. Each fixture
//! carries a real CRC_32, so `crc_ok` proves the fixture bytes themselves.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use base64::Engine as _;
use multiview_input::scte::decode::{
    decode_section, pts_add, pts_delta, Command, DescriptorBody, PTS_MODULUS,
};

fn b64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(text).unwrap()
}

/// SCTE-35 2023r1 sample: `time_signal` Placement Opportunity Start.
const TIME_SIGNAL_POS: &str =
    "/DA0AAAAAAAA///wBQb+cr0AUAAeAhxDVUVJSAAAjn/PAAGlmbAICAAAAAAsoKGKNAIAmsnRfg==";
/// SCTE-35 2023r1 sample: `time_signal` Placement Opportunity End.
const TIME_SIGNAL_POE: &str =
    "/DAvAAAAAAAA///wBQb+dGKQoAAZAhdDVUVJSAAAjn+fCAgAAAAALKChijUCAKnMZ1g=";
/// SCTE-35 2023r1 sample: `splice_insert` with a break duration.
const SPLICE_INSERT: &str = "/DAvAAAAAAAA///wFAVIAACPf+/+c2nALv4AUsz1AAAAAAAKAAhDVUVJAAABNWLbowo=";

#[test]
fn time_signal_placement_opportunity_start_decodes_every_field() {
    let d = decode_section(&b64(TIME_SIGNAL_POS)).unwrap();
    assert!(d.crc_ok, "fixture CRC {}", d.crc_32);
    assert_eq!(d.parse_error, None);
    assert_eq!(d.splice_command_name, "time_signal");
    assert_eq!(d.pts_time(), Some(0x0_72BD_0050));
    assert_eq!(d.tier, 0xFFF);
    let s = d.primary_segmentation().unwrap();
    assert_eq!(s.segmentation_event_id, 0x4800_008E);
    assert_eq!(s.segmentation_type_id, Some(0x34));
    assert_eq!(
        s.segmentation_type_name.as_deref(),
        Some("Provider Placement Opportunity Start")
    );
    assert_eq!(s.segmentation_duration, Some(0x0001_A599_B0));
    assert_eq!(s.segmentation_duration_ms, Some(307_000));
    let upid = s.upid.as_ref().unwrap();
    assert_eq!(upid.upid_type_name, "AiringID");
    assert_eq!(upid.value, "0x000000002CA0A18A");
    assert_eq!((s.segment_num, s.segments_expected), (Some(2), Some(0)));
    assert_eq!(d.display_name(), "Provider Placement Opportunity Start");
    assert_eq!(d.duration_ticks(), Some(0x0001_A599_B0));
}

#[test]
fn time_signal_placement_opportunity_end_has_restrictions() {
    let d = decode_section(&b64(TIME_SIGNAL_POE)).unwrap();
    assert!(d.crc_ok, "fixture CRC {}", d.crc_32);
    let s = d.primary_segmentation().unwrap();
    assert_eq!(s.segmentation_type_id, Some(0x35));
    let restrictions = s.delivery_restrictions.as_ref().unwrap();
    assert!(restrictions.no_regional_blackout);
    assert_eq!(restrictions.device_restrictions_name, "None");
}

#[test]
fn splice_insert_decodes_break_duration_and_avail_descriptor() {
    let d = decode_section(&b64(SPLICE_INSERT)).unwrap();
    assert!(d.crc_ok, "fixture CRC {}", d.crc_32);
    let Command::SpliceInsert(insert) = &d.splice_command else {
        panic!("not an insert: {:?}", d.splice_command);
    };
    assert_eq!(insert.splice_event_id, 0x4800_008F);
    assert_eq!(insert.out_of_network, Some(true));
    assert_eq!(insert.splice_immediate, Some(false));
    let duration = insert.break_duration.as_ref().unwrap();
    assert!(duration.auto_return);
    assert_eq!(duration.duration, 0x0052_CCF5);
    assert!(matches!(
        d.descriptors[0].body,
        DescriptorBody::Avail { provider_avail_id: 0x135 }
    ));
    assert_eq!(d.display_name(), "Splice Insert (Out of Network)");
}

#[test]
fn a_failed_crc_keeps_the_decode_and_says_so() {
    let mut bytes = b64(TIME_SIGNAL_POS);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let d = decode_section(&bytes).unwrap();
    assert!(!d.crc_ok);
    assert_eq!(d.primary_segmentation().unwrap().segmentation_type_id, Some(0x34));
}

#[test]
fn truncation_never_panics_at_any_length() {
    let bytes = b64(TIME_SIGNAL_POS);
    for len in 0..bytes.len() {
        let _ = decode_section(&bytes[..len]);
    }
    assert!(decode_section(&[0x00; 20]).is_err());
}

fn section(command_type: u8, command: &[u8], descriptors: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFC, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xF0];
    s.push(u8::try_from(command.len()).unwrap());
    s.push(command_type);
    s.extend_from_slice(command);
    s.extend_from_slice(&u16::try_from(descriptors.len()).unwrap().to_be_bytes());
    s.extend_from_slice(descriptors);
    let length = u16::try_from(s.len() + 4 - 3).unwrap();
    s[1] = 0x30 | u8::try_from(length >> 8).unwrap();
    s[2] = u8::try_from(length & 0xFF).unwrap();
    let crc = multiview_input::mpegts::crc::crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

fn segmentation(upid_type: u8, upid: &[u8], type_id: u8) -> Vec<u8> {
    let mut body = b"CUEI".to_vec();
    body.extend_from_slice(&[0, 0, 0, 7, 0x7F, 0xBF, upid_type]);
    body.push(u8::try_from(upid.len()).unwrap());
    body.extend_from_slice(upid);
    body.extend_from_slice(&[type_id, 1, 1]);
    let mut d = vec![0x02, u8::try_from(body.len()).unwrap()];
    d.extend_from_slice(&body);
    d
}

fn upid_of(upid_type: u8, upid: &[u8]) -> String {
    let bytes = section(0x06, &[0x7F], &segmentation(upid_type, upid, 0x10));
    let d = decode_section(&bytes).unwrap();
    assert!(d.crc_ok);
    assert_eq!(d.parse_error, None);
    d.primary_segmentation().unwrap().upid.clone().unwrap().value
}

#[test]
fn upids_decode_per_type() {
    assert_eq!(upid_of(0x03, b"ABCD01234567"), "ABCD01234567");
    assert_eq!(upid_of(0x09, b"SIGNAL:abc"), "SIGNAL:abc");
    assert_eq!(upid_of(0x0F, b"urn:uuid:1"), "urn:uuid:1");
    assert_eq!(upid_of(0x08, &[0, 0, 0, 0, 0x2C, 0xA0, 0xA1, 0x8A]), "0x000000002CA0A18A");
    assert_eq!(
        upid_of(0x06, &[0xAB; 12]),
        "ABAB-ABAB-ABAB-ABAB-ABAB-ABAB"
    );
    assert_eq!(
        upid_of(0x04, &[0x11; 32]),
        "11111111.11111111.11111111.11111111.11111111.11111111.11111111.11111111"
    );
    assert_eq!(
        upid_of(0x10, &[0xAB; 16]),
        "abababab-abab-abab-abab-abababababab"
    );
    assert_eq!(
        upid_of(0x0C, b"ADFR\x01\x02"),
        "format_identifier=ADFR private_data=0x0102"
    );
}

#[test]
fn eidr_renders_as_a_doi_with_its_check_character() {
    // 10.5240/7791-8534-2C23-9030-8610-5, a published EIDR title identifier.
    let eidr = [0x14, 0x78, 0x77, 0x91, 0x85, 0x34, 0x2C, 0x23, 0x90, 0x30, 0x86, 0x10];
    assert_eq!(upid_of(0x0A, &eidr), "10.5240/7791-8534-2C23-9030-8610-5");
}

#[test]
fn mid_decodes_each_inner_upid() {
    let mid = [&[0x03, 12][..], b"ABCD01234567", &[0x0F, 3], b"a:b"].concat();
    assert_eq!(upid_of(0x0D, &mid), "Ad-ID:ABCD01234567, URI:a:b");
}

#[test]
fn null_bandwidth_private_and_immediate_commands_decode() {
    let d = decode_section(&section(0x00, &[], &[])).unwrap();
    assert!(matches!(d.splice_command, Command::SpliceNull));
    let d = decode_section(&section(0x07, &[], &[])).unwrap();
    assert!(matches!(d.splice_command, Command::BandwidthReservation));
    let d = decode_section(&section(0xFF, &[0x43, 0x55, 0x45, 0x49, 0xAA], &[])).unwrap();
    let Command::PrivateCommand { identifier, private_bytes } = d.splice_command else {
        panic!("not private");
    };
    assert_eq!((identifier.as_str(), private_bytes.as_str()), ("43554549", "aa"));
    let d = decode_section(&section(0x06, &[0x7F], &[])).unwrap();
    assert!(d.is_immediate());
    assert_eq!(d.splice_pts(), None);
}

#[test]
fn encrypted_sections_stop_at_the_clear_header() {
    let mut bytes = section(0x06, &[0x7F], &[]);
    bytes[4] = 0x82;
    let d = decode_section(&bytes).unwrap();
    assert!(d.encrypted_packet);
    assert_eq!(d.encryption_algorithm, 1);
    assert!(matches!(d.splice_command, Command::Opaque { .. }));
}

#[test]
fn pts_math_wraps_on_the_33_bit_circle() {
    assert_eq!(pts_add(PTS_MODULUS - 10, 25), 15);
    assert_eq!(pts_add(5, 7), 12);
    assert_eq!(pts_delta(15, PTS_MODULUS - 10), 25);
    assert_eq!(pts_delta(PTS_MODULUS - 10, 15), -25);
    assert_eq!(pts_delta(100, 100), 0);
}
