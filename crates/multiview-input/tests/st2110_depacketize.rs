//! Golden on-wire vector tests for the ST 2110 RTP depacketizers (RFC 3550
//! header, -20 video SRD, -30 AES67 audio, -40 ANC) and the ST 2022-6 HBRMT
//! parser. These run in the DEFAULT (pure-Rust) build — the codecs are
//! socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: test fixtures hand-assemble byte vectors with small, statically
    // in-range length fields; `as` on those tiny constants cannot truncate.
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::st2022_6::Hbrmt;
use multiview_input::st2110::rtp::{seq_after, seq_distance, RtpError, RtpPacket, RTP_VERSION};
use multiview_input::st2110::v20::{V20Error, V20Payload};
use multiview_input::st2110::v30::{Aes3Format, SampleDepth, V30Error, V30Payload};
use multiview_input::st2110::v40::V40Payload;

// ---------------------------------------------------------------------------
// RFC 3550 RTP fixed header
// ---------------------------------------------------------------------------

/// A minimal V2 RTP packet: marker set, PT 96, seq 0x1234, ts 0xDEADBEEF,
/// ssrc 0x01020304, then a 4-byte payload.
fn rtp_golden() -> Vec<u8> {
    let mut p = Vec::new();
    p.push(RTP_VERSION << 6); // V=2, no padding/extension, CC=0
    p.push(0x80 | 0x60); // marker + PT 96 (0x60)
    p.extend_from_slice(&0x1234u16.to_be_bytes()); // sequence
    p.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // timestamp
    p.extend_from_slice(&0x0102_0304u32.to_be_bytes()); // ssrc
    p.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // payload
    p
}

#[test]
fn rtp_decodes_fixed_header_and_payload() {
    let bytes = rtp_golden();
    let pkt = RtpPacket::parse(&bytes).expect("valid RTP");
    assert!(pkt.header.marker);
    assert_eq!(pkt.header.payload_type, 96);
    assert_eq!(pkt.header.sequence, 0x1234);
    assert_eq!(pkt.header.timestamp, 0xDEAD_BEEF);
    assert_eq!(pkt.header.ssrc, 0x0102_0304);
    assert_eq!(pkt.header.csrc_count, 0);
    assert!(!pkt.header.has_extension);
    assert_eq!(pkt.payload, &[0xAA, 0xBB, 0xCC, 0xDD]);
}

#[test]
fn rtp_skips_csrc_list() {
    let mut bytes = rtp_golden();
    // Rebuild with CC=2 and two CSRC words inserted after the fixed header.
    bytes[0] = (RTP_VERSION << 6) | 2;
    let mut with_csrc = bytes[..12].to_vec();
    with_csrc.extend_from_slice(&0x1111_1111u32.to_be_bytes());
    with_csrc.extend_from_slice(&0x2222_2222u32.to_be_bytes());
    with_csrc.extend_from_slice(&[0x01, 0x02]); // payload
    let pkt = RtpPacket::parse(&with_csrc).expect("valid RTP w/ CSRC");
    assert_eq!(pkt.header.csrc_count, 2);
    assert_eq!(pkt.payload, &[0x01, 0x02]);
}

#[test]
fn rtp_strips_padding() {
    let mut bytes = rtp_golden();
    bytes[0] = (RTP_VERSION << 6) | 0b0010_0000; // set P bit
                                                 // payload becomes [0xAA, 0xBB, pad=2] -> 2 padding bytes incl. count
    let len = bytes.len();
    bytes[len - 1] = 0x02; // last byte = padding length 2
    let pkt = RtpPacket::parse(&bytes).expect("valid padded RTP");
    // 4 payload bytes minus 2 padding = 2 kept.
    assert_eq!(pkt.payload, &[0xAA, 0xBB]);
}

#[test]
fn rtp_rejects_bad_version() {
    let mut bytes = rtp_golden();
    bytes[0] = 0b0100_0000; // version 1
    assert!(matches!(
        RtpPacket::parse(&bytes),
        Err(RtpError::BadVersion(1))
    ));
}

#[test]
fn rtp_rejects_short() {
    assert!(matches!(
        RtpPacket::parse(&[0x80, 0x60]),
        Err(RtpError::TooShort { .. })
    ));
}

#[test]
fn rtp_seq_helpers() {
    assert_eq!(seq_distance(10, 13), 3);
    assert_eq!(seq_distance(0xFFFF, 1), 2); // wrap-around distance
    assert!(seq_after(10, 11));
    assert!(seq_after(0xFFFF, 0)); // wrap is "after"
    assert!(!seq_after(11, 10));
    assert!(!seq_after(5, 5));
}

// ---------------------------------------------------------------------------
// ST 2110-20 video (extended sequence + SRD headers)
// ---------------------------------------------------------------------------

/// One SRD: ext seq 0x0007, single SRD header (length 4, line 17, offset 0,
/// no continuation, field 0), then 4 sample bytes.
fn v20_single_srd() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0x0007u16.to_be_bytes()); // extended sequence
    p.extend_from_slice(&4u16.to_be_bytes()); // SRD length 4
    p.extend_from_slice(&17u16.to_be_bytes()); // C=0, line 17
    p.extend_from_slice(&0u16.to_be_bytes()); // F=0, offset 0
    p.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // sample data
    p
}

#[test]
fn v20_decodes_single_srd() {
    let payload = v20_single_srd();
    let decoded = V20Payload::parse(&payload, 0x00FF).expect("valid -20");
    assert_eq!(decoded.full_sequence, (0x0007u32 << 16) | 0x00FF);
    assert_eq!(decoded.segments.len(), 1);
    let seg = &decoded.segments[0];
    assert_eq!(seg.line_number, 17);
    assert_eq!(seg.offset, 0);
    assert!(!seg.field);
    let range = seg.data_range();
    assert_eq!(&payload[range], &[0x11, 0x22, 0x33, 0x44]);
}

#[test]
fn v20_decodes_two_continued_srds() {
    let mut p = Vec::new();
    p.extend_from_slice(&0x0000u16.to_be_bytes()); // ext seq
                                                   // SRD 1 header: length 2, C=1 + line 1
    p.extend_from_slice(&2u16.to_be_bytes());
    p.extend_from_slice(&(0x8000u16 | 1).to_be_bytes()); // C=1, line 1
    p.extend_from_slice(&0u16.to_be_bytes()); // offset 0
                                              // SRD 2 header: length 3, C=0 + line 2, field set
    p.extend_from_slice(&3u16.to_be_bytes());
    p.extend_from_slice(&2u16.to_be_bytes()); // C=0, line 2
    p.extend_from_slice(&0x8000u16.to_be_bytes()); // F=1, offset 0
                                                   // data: 2 bytes then 3 bytes
    p.extend_from_slice(&[0xA0, 0xA1]);
    p.extend_from_slice(&[0xB0, 0xB1, 0xB2]);

    let decoded = V20Payload::parse(&p, 0).expect("valid two-SRD -20");
    assert_eq!(decoded.segments.len(), 2);
    assert_eq!(decoded.segments[0].line_number, 1);
    assert!(!decoded.segments[0].field);
    assert_eq!(&p[decoded.segments[0].data_range()], &[0xA0, 0xA1]);
    assert_eq!(decoded.segments[1].line_number, 2);
    assert!(decoded.segments[1].field);
    assert_eq!(&p[decoded.segments[1].data_range()], &[0xB0, 0xB1, 0xB2]);
}

#[test]
fn v20_rejects_length_overrun() {
    let mut p = Vec::new();
    p.extend_from_slice(&0u16.to_be_bytes()); // ext seq
    p.extend_from_slice(&100u16.to_be_bytes()); // SRD length 100 (too big)
    p.extend_from_slice(&1u16.to_be_bytes()); // C=0, line 1
    p.extend_from_slice(&0u16.to_be_bytes()); // offset 0
    p.extend_from_slice(&[0x00, 0x01]); // only 2 bytes of data
    assert!(matches!(
        V20Payload::parse(&p, 0),
        Err(V20Error::Length { .. })
    ));
}

// ---------------------------------------------------------------------------
// ST 2110-30 audio (interleaved PCM groups)
// ---------------------------------------------------------------------------

#[test]
fn v30_decodes_stereo_l16() {
    let fmt = Aes3Format::new(2, SampleDepth::L16).expect("ok");
    // Two sample groups: group0 = (L=+1, R=-1), group1 = (L=+256, R=0x7FFF).
    let mut p = Vec::new();
    p.extend_from_slice(&1i16.to_be_bytes());
    p.extend_from_slice(&(-1i16).to_be_bytes());
    p.extend_from_slice(&256i16.to_be_bytes());
    p.extend_from_slice(&0x7FFFi16.to_be_bytes());
    let decoded = V30Payload::parse(&p, fmt).expect("valid -30");
    assert_eq!(decoded.group_count(), 2);
    assert_eq!(decoded.sample(0, 0), Some(1));
    assert_eq!(decoded.sample(0, 1), Some(-1));
    assert_eq!(decoded.sample(1, 0), Some(256));
    assert_eq!(decoded.sample(1, 1), Some(0x7FFF));
    assert_eq!(decoded.sample(0, 2), None); // out-of-range channel
    assert_eq!(decoded.sample(2, 0), None); // out-of-range group
}

#[test]
fn v30_decodes_l24_sign_extends() {
    let fmt = Aes3Format::new(1, SampleDepth::L24).expect("ok");
    // 24-bit max negative 0x800000 = -8388608; 24-bit +1 = 0x000001.
    let p = [0x80, 0x00, 0x00, 0x00, 0x00, 0x01];
    let decoded = V30Payload::parse(&p, fmt).expect("valid L24");
    assert_eq!(decoded.group_count(), 2);
    assert_eq!(decoded.sample(0, 0), Some(-8_388_608));
    assert_eq!(decoded.sample(1, 0), Some(1));
}

#[test]
fn v30_rejects_partial_group() {
    let fmt = Aes3Format::new(2, SampleDepth::L16).expect("ok");
    // 6 bytes is 1.5 stereo L16 groups (group size 4) -> partial.
    let p = [0u8; 6];
    assert!(matches!(
        V30Payload::parse(&p, fmt),
        Err(V30Error::PartialGroup { .. })
    ));
}

#[test]
fn v30_zero_channels_rejected() {
    assert!(matches!(
        Aes3Format::new(0, SampleDepth::L16),
        Err(V30Error::ZeroChannels)
    ));
}

// ---------------------------------------------------------------------------
// ST 2110-40 ANC (RFC 8331; 10-bit-packed)
// ---------------------------------------------------------------------------

/// Build one ANC packet (DID 0x41 SDID 0x05 = AFD) carrying a single user-data
/// word `0x09`, with the full RFC 8331 payload header. We pack the 10-bit
/// symbols MSB-first using a tiny bit-writer to mirror the parser.
fn v40_one_afd_packet() -> Vec<u8> {
    // Payload header: ext seq (2) | length (2) | ANC_Count(1)=1 | F(1)=0 | rsvd(2)
    let mut header = Vec::new();
    header.extend_from_slice(&0x0003u16.to_be_bytes()); // ext seq
    header.extend_from_slice(&0u16.to_be_bytes()); // length (unused by parser)
    header.push(1); // ANC_Count = 1
    header.push(0); // F=0 + reserved
    header.push(0);
    header.push(0);
    assert_eq!(header.len(), 8);

    // Bit-pack the ANC packet: C(1)=0, Line(11)=9, HOffset(12)=0, S(1)=0,
    // StreamNum(7)=0, DID(10)=0x041, SDID(10)=0x005, DataCount(10)=1,
    // UDW(10)=0x009, Checksum(10)=0.  Parity bits left zero (parser strips low 8).
    let mut bits = BitWriter::new();
    bits.write(0, 1); // C
    bits.write(9, 11); // line 9
    bits.write(0, 12); // h offset
    bits.write(0, 1); // S
    bits.write(0, 7); // stream num
    bits.write(0x041, 10); // DID 0x41
    bits.write(0x005, 10); // SDID 0x05
    bits.write(0x001, 10); // data count 1
    bits.write(0x009, 10); // udw 0x09
    bits.write(0x000, 10); // checksum
    let mut out = header;
    out.extend_from_slice(&bits.finish());
    out
}

struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: usize,
}
impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }
    fn write(&mut self, value: u16, width: usize) {
        for i in (0..width).rev() {
            let bit = ((value >> i) & 1) as u8;
            let byte_index = self.bit_pos / 8;
            let bit_in_byte = 7 - (self.bit_pos % 8);
            if byte_index >= self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[byte_index] |= bit << bit_in_byte;
            self.bit_pos += 1;
        }
    }
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[test]
fn v40_decodes_one_afd_packet() {
    let payload = v40_one_afd_packet();
    let decoded = V40Payload::parse(&payload, 0x0042).expect("valid -40");
    assert_eq!(decoded.full_sequence, (0x0003u32 << 16) | 0x0042);
    assert!(!decoded.field);
    assert_eq!(decoded.packets.len(), 1);
    let anc = &decoded.packets[0];
    assert!(!anc.chroma);
    assert_eq!(anc.line_number, 9);
    assert_eq!(anc.horizontal_offset, 0);
    assert_eq!(anc.did, 0x41);
    assert_eq!(anc.sdid, 0x05);
    assert_eq!(anc.user_data, vec![0x09]);
}

#[test]
fn v40_rejects_short_header() {
    assert!(V40Payload::parse(&[0u8; 4], 0).is_err());
}

// ---------------------------------------------------------------------------
// ST 2022-6 HBRMT
// ---------------------------------------------------------------------------

#[test]
fn st2022_6_parses_header_and_sdi() {
    // HBRMT header: byte0=0, FRCount=7, byte2 has S bit set (video ts present),
    // byte3=0, byte4=MAP(2)|FRAME(1), byte5=FRATE(3)|SAMPLE(4), bytes6,7=0,
    // then 4-byte video timestamp, then SDI octets.
    // byte0=Ext|F|VSID, FRCount=7, byte2 S-bit set, byte3=0,
    // byte4=MAP(2)|FRAME(1), byte5=FRATE(3)|SAMPLE(4), bytes6,7=0.
    let mut p = vec![0x00, 7, 0b0100_0000, 0x00, (2 << 4) | 1, (3 << 4) | 4];
    p.push(0x00);
    p.push(0x00);
    p.extend_from_slice(&0xCAFE_F00Du32.to_be_bytes()); // video timestamp
    p.extend_from_slice(&[0xDE, 0xAD]); // SDI octets

    let h = Hbrmt::parse(&p).expect("valid HBRMT");
    assert_eq!(h.frame_count, 7);
    assert!(h.has_video_timestamp);
    assert_eq!(h.video_timestamp, Some(0xCAFE_F00D));
    assert_eq!(h.format.map, 2);
    assert_eq!(h.format.frame, 1);
    assert_eq!(h.format.frate, 3);
    assert_eq!(h.format.sample, 4);
    assert_eq!(h.sdi, &[0xDE, 0xAD]);
}

#[test]
fn st2022_6_without_video_timestamp() {
    let mut p = vec![0u8; 8];
    p[1] = 3; // FRCount
              // S bit clear -> no video ts
    p.extend_from_slice(&[0x01, 0x02, 0x03]); // SDI
    let h = Hbrmt::parse(&p).expect("valid HBRMT no-ts");
    assert!(!h.has_video_timestamp);
    assert_eq!(h.video_timestamp, None);
    assert_eq!(h.sdi, &[0x01, 0x02, 0x03]);
}
