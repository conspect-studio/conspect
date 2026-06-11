//! Golden on-wire vector tests for the MPEG-2 Transport Stream PSI/SI parsers
//! (CRC-32/MPEG-2, PAT, PMT, NIT, SDT, CAT, TDT, TOT) and the MPTS program
//! selection model. These run in the DEFAULT (pure-Rust) build — the parsers are
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

use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::mpegts::pmt::StreamType;
use multiview_input::mpegts::section::TableId;
use multiview_input::mpegts::selection::{ProgramSelection, SelectionError};
use multiview_input::mpegts::{Cat, MpegTsError, Nit, Pat, Pmt, Sdt, SelectedProgram, Tdt, Tot};

/// Append the correct big-endian CRC-32/MPEG-2 to a section body, producing a
/// complete, validatable section.
fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32_mpeg2(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    body
}

/// Build a long-form PSI section header + body (the caller has *not* set
/// `section_length`; this computes it). `table_id_ext`, `version`, `current`.
fn long_section(table_id: u8, table_id_ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
    // long-form fields after section_length: tide(2) + version_byte(1) +
    // section_number(1) + last_section_number(1) = 5, then body, then CRC(4).
    let section_length = 5 + body.len() + 4;
    let mut out = Vec::new();
    out.push(table_id);
    // syntax_indicator(1)=1, '0'(1)=0, reserved(2)=11, section_length top nibble.
    let b1 = 0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F);
    out.push(b1);
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&table_id_ext.to_be_bytes());
    // reserved(2)=11 + version(5) + current_next(1).
    let version_byte = 0b1100_0000 | ((version & 0x1F) << 1) | 0x01;
    out.push(version_byte);
    out.push(0x00); // section_number
    out.push(0x00); // last_section_number
    out.extend_from_slice(body);
    with_crc(out)
}

// ---------------------------------------------------------------------------
// CRC-32/MPEG-2
// ---------------------------------------------------------------------------

#[test]
fn crc32_mpeg2_known_vector() {
    // The ASCII string "123456789" has CRC-32/MPEG-2 check value 0x0376E6E7
    // (the canonical check value for this CRC variant).
    assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
}

#[test]
fn crc32_self_check_property() {
    // Running the CRC over a section that ends in its own appended CRC yields 0.
    let body = vec![0x00u8, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
    let full = with_crc(body);
    assert_eq!(crc32_mpeg2(&full), 0);
}

// ---------------------------------------------------------------------------
// PAT
// ---------------------------------------------------------------------------

#[test]
fn pat_decodes_programs_and_network() {
    // Program 0 -> NIT PID 0x0010; program 1 -> PMT PID 0x1000.
    let mut body = Vec::new();
    body.extend_from_slice(&0u16.to_be_bytes()); // program 0
    body.extend_from_slice(&(0xE000 | 0x0010u16).to_be_bytes()); // reserved bits + PID
    body.extend_from_slice(&1u16.to_be_bytes()); // program 1
    body.extend_from_slice(&(0xE000 | 0x1000u16).to_be_bytes());
    let section = long_section(0x00, 0x0042, 5, &body);

    let pat = Pat::parse(&section).expect("valid PAT");
    assert_eq!(pat.transport_stream_id, 0x0042);
    assert_eq!(pat.version, 5);
    assert!(pat.current);
    assert_eq!(pat.programs.len(), 2);
    assert_eq!(pat.network_pid(), Some(0x0010));
    assert_eq!(pat.pmt_pid(1), Some(0x1000));
    assert_eq!(pat.pmt_pid(99), None);
    assert_eq!(pat.program_count(), 1);
}

#[test]
fn pat_rejects_corrupt_crc() {
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&(0xE000 | 0x1000u16).to_be_bytes());
    let mut section = long_section(0x00, 0x0001, 0, &body);
    // Flip a payload byte after the CRC was computed.
    section[3] ^= 0xFF;
    assert!(matches!(Pat::parse(&section), Err(MpegTsError::Crc { .. })));
}

#[test]
fn pat_rejects_wrong_table_id() {
    let section = long_section(0x02, 0x0001, 0, &[]);
    assert!(matches!(
        Pat::parse(&section),
        Err(MpegTsError::WrongTable {
            expected: 0x00,
            got: 0x02
        })
    ));
}

// ---------------------------------------------------------------------------
// PMT
// ---------------------------------------------------------------------------

/// Build a PMT body: PCR PID, empty program info, then the elementary streams.
fn pmt_body(pcr_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(0xE000 | pcr_pid).to_be_bytes()); // reserved + PCR PID
    body.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved + program_info_length 0
    for &(stype, pid) in streams {
        body.push(stype);
        body.extend_from_slice(&(0xE000 | pid).to_be_bytes()); // reserved + ES PID
        body.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved + ES_info_length 0
    }
    body
}

#[test]
fn pmt_decodes_streams() {
    // H.264 video on 0x0100, AAC audio on 0x0101, SCTE-35 on 0x01F0; PCR=0x0100.
    let body = pmt_body(0x0100, &[(0x1B, 0x0100), (0x0F, 0x0101), (0x86, 0x01F0)]);
    let section = long_section(0x02, 0x0001, 3, &body);

    let pmt = Pmt::parse(&section).expect("valid PMT");
    assert_eq!(pmt.program_number, 0x0001);
    assert_eq!(pmt.pcr_pid, 0x0100);
    assert_eq!(pmt.streams.len(), 3);

    let video = pmt.video_stream().expect("video present");
    assert_eq!(video.stream_type, StreamType::H264);
    assert_eq!(video.pid, 0x0100);

    let audio = pmt.audio_streams();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].stream_type, StreamType::AdtsAac);

    assert_eq!(pmt.scte35_pids(), vec![0x01F0]);
}

#[test]
fn pmt_stream_type_roundtrip() {
    for st in [
        StreamType::Mpeg2Video,
        StreamType::H264,
        StreamType::Hevc,
        StreamType::AdtsAac,
        StreamType::Ac3,
        StreamType::Scte35,
        StreamType::Other(0x77),
    ] {
        assert_eq!(StreamType::from_byte(st.to_byte()), st);
    }
}

// ---------------------------------------------------------------------------
// MPTS program selection
// ---------------------------------------------------------------------------

#[test]
fn program_selection_resolves_pids() {
    // PAT: program 0 -> NIT, program 2 -> PMT PID 0x1200 (a valid 13-bit PID).
    let mut pat_body = Vec::new();
    pat_body.extend_from_slice(&0u16.to_be_bytes());
    pat_body.extend_from_slice(&(0xE000 | 0x0010u16).to_be_bytes());
    pat_body.extend_from_slice(&2u16.to_be_bytes());
    pat_body.extend_from_slice(&(0xE000 | 0x1200u16).to_be_bytes());
    let pat = Pat::parse(&long_section(0x00, 1, 0, &pat_body)).unwrap();

    let pmt = Pmt::parse(&long_section(
        0x02,
        2,
        0,
        &pmt_body(0x0200, &[(0x1B, 0x0200), (0x0F, 0x0201)]),
    ))
    .unwrap();

    let selected = SelectedProgram::resolve(&pat, &pmt, ProgramSelection::ByProgramNumber(2))
        .expect("resolves");
    assert_eq!(selected.program_number, 2);
    assert_eq!(selected.pmt_pid, 0x1200);
    assert_eq!(selected.pcr_pid, 0x0200);
    assert_eq!(selected.video_pid, Some(0x0200));
    assert_eq!(selected.audio_pids, vec![0x0201]);

    let pids = selected.demux_pids();
    assert!(pids.contains(&0x1200));
    assert!(pids.contains(&0x0200));
    assert!(pids.contains(&0x0201));
    // demux_pids is sorted + deduped.
    let mut sorted = pids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(pids, sorted);
}

#[test]
fn program_selection_unknown_program() {
    let mut pat_body = Vec::new();
    pat_body.extend_from_slice(&3u16.to_be_bytes());
    pat_body.extend_from_slice(&(0xE000 | 0x1300u16).to_be_bytes());
    let pat = Pat::parse(&long_section(0x00, 1, 0, &pat_body)).unwrap();
    assert_eq!(
        pat.resolve(ProgramSelection::ByProgramNumber(9)),
        Err(SelectionError::UnknownProgram(9))
    );
    assert_eq!(pat.resolve(ProgramSelection::First).unwrap(), (3, 0x1300));
    assert!(matches!(
        pat.resolve(ProgramSelection::ByIndex(5)),
        Err(SelectionError::IndexOutOfRange { index: 5, count: 1 })
    ));
}

#[test]
fn program_selection_pmt_mismatch() {
    let mut pat_body = Vec::new();
    pat_body.extend_from_slice(&2u16.to_be_bytes());
    pat_body.extend_from_slice(&(0xE000 | 0x1200u16).to_be_bytes());
    let pat = Pat::parse(&long_section(0x00, 1, 0, &pat_body)).unwrap();
    // PMT claims program 7, but we select program 2.
    let pmt = Pmt::parse(&long_section(0x02, 7, 0, &pmt_body(0x0700, &[]))).unwrap();
    assert!(matches!(
        SelectedProgram::resolve(&pat, &pmt, ProgramSelection::ByProgramNumber(2)),
        Err(SelectionError::PmtMismatch {
            pmt: 7,
            selected: 2
        })
    ));
}

// ---------------------------------------------------------------------------
// NIT / SDT / CAT
// ---------------------------------------------------------------------------

#[test]
fn nit_decodes_transport_streams() {
    // network descriptors length 0; one TS entry (TSID 0x0042, ONID 0x2024, no
    // descriptors).
    let mut body = Vec::new();
    body.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved + network_descriptors_length 0
                                                      // transport_stream_loop_length = 6 (one entry of header only).
    body.extend_from_slice(&(0xF000u16 | 6).to_be_bytes());
    body.extend_from_slice(&0x0042u16.to_be_bytes()); // TSID
    body.extend_from_slice(&0x2024u16.to_be_bytes()); // ONID
    body.extend_from_slice(&0xF000u16.to_be_bytes()); // reserved + ts_desc_len 0
    let nit = Nit::parse(&long_section(0x40, 0x0001, 0, &body)).expect("valid NIT");
    assert!(nit.actual);
    assert_eq!(nit.network_id, 0x0001);
    assert_eq!(nit.transport_streams.len(), 1);
    assert_eq!(nit.transport_streams[0].transport_stream_id, 0x0042);
    assert_eq!(nit.transport_streams[0].original_network_id, 0x2024);
}

#[test]
fn sdt_decodes_service_name() {
    // One service id 0x0005, running, free; with a 0x48 service descriptor
    // carrying provider "P" and service "Chan1".
    let mut svc_desc = Vec::new();
    svc_desc.push(0x48); // descriptor tag
    let provider = b"P";
    let name = b"Chan1";
    let desc_body_len = 1 + 1 + provider.len() + 1 + name.len();
    svc_desc.push(desc_body_len as u8); // descriptor length
    svc_desc.push(0x01); // service_type
    svc_desc.push(provider.len() as u8);
    svc_desc.extend_from_slice(provider);
    svc_desc.push(name.len() as u8);
    svc_desc.extend_from_slice(name);

    let mut body = Vec::new();
    body.extend_from_slice(&0x2024u16.to_be_bytes()); // original_network_id
    body.push(0xFF); // reserved future use
                     // service entry:
    body.extend_from_slice(&0x0005u16.to_be_bytes()); // service_id
    body.push(0x03); // EIT flags: schedule+present_following set
                     // status byte: running(4)<<5 | free(0)<<4 | dll top nibble.
    let dll = svc_desc.len();
    let status_byte = (4u8 << 5) | ((dll >> 8) as u8 & 0x0F);
    body.push(status_byte);
    body.push((dll & 0xFF) as u8);
    body.extend_from_slice(&svc_desc);

    let sdt = Sdt::parse(&long_section(0x42, 0x0042, 0, &body)).expect("valid SDT");
    assert!(sdt.actual);
    assert_eq!(sdt.transport_stream_id, 0x0042);
    assert_eq!(sdt.original_network_id, 0x2024);
    assert_eq!(sdt.services.len(), 1);
    let svc = &sdt.services[0];
    assert_eq!(svc.service_id, 0x0005);
    assert!(svc.free_ca_mode_free);
    let (prov, svc_name) = svc.names().expect("names ok").expect("names present");
    assert_eq!(prov, "P");
    assert_eq!(svc_name, "Chan1");
}

#[test]
fn cat_decodes_ca_descriptor() {
    // CAT body is a single descriptor loop: one CA descriptor (tag 0x09),
    // CA_system_id 0x0B00, EMM PID 0x0050.
    let mut body = Vec::new();
    body.push(0x09); // CA descriptor tag
    body.push(0x04); // length
    body.extend_from_slice(&0x0B00u16.to_be_bytes()); // CA_system_id
    body.extend_from_slice(&(0xE000 | 0x0050u16).to_be_bytes()); // reserved + EMM PID
    let cat = Cat::parse(&long_section(0x01, 0xFFFF, 0, &body)).expect("valid CAT");
    let systems = cat.ca_systems().expect("ca systems");
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].ca_system_id, 0x0B00);
    assert_eq!(systems[0].emm_pid, 0x0050);
}

// ---------------------------------------------------------------------------
// TDT / TOT (short form, MJD + BCD)
// ---------------------------------------------------------------------------

#[test]
fn tdt_decodes_known_date() {
    // ETSI EN 300 468 Annex C worked example: 93/10/13 12:45:00.
    // MJD for 1993-10-13 is 49273 = 0xC079; time 12:45:00 BCD = 0x12 0x45 0x00.
    let mut section = Vec::new();
    section.push(0x70); // table_id
    section.push(0x70); // short-form: ssi=0, reserved, section_length hi nibble
    section.push(0x05); // section_length = 5
    section.extend_from_slice(&0xC079u16.to_be_bytes()); // MJD
    section.push(0x12); // hours BCD
    section.push(0x45); // minutes BCD
    section.push(0x00); // seconds BCD
    let tdt = Tdt::parse(&section).expect("valid TDT");
    assert_eq!(tdt.utc.mjd, 0xC079);
    assert_eq!(tdt.utc.hours, 12);
    assert_eq!(tdt.utc.minutes, 45);
    assert_eq!(tdt.utc.seconds, 0);
    // 1993-10-13T12:45:00Z as a Unix timestamp.
    assert_eq!(tdt.utc.to_unix_seconds().unwrap(), 750_516_300);
}

#[test]
fn tdt_rejects_bad_bcd() {
    let mut section = vec![0x70, 0x70, 0x05];
    section.extend_from_slice(&0xC079u16.to_be_bytes());
    section.push(0x1A); // 0xA nibble is not a decimal digit
    section.push(0x00);
    section.push(0x00);
    assert!(matches!(
        Tdt::parse(&section),
        Err(MpegTsError::BadDateTime(_))
    ));
}

#[test]
fn tot_decodes_time_and_offset() {
    // TOT: UTC 1993-10-13 12:45:00 + a local_time_offset_descriptor (+10:00).
    // One region entry is 13 bytes: country(3)+region/polarity(1)+offset(2 BCD)+
    // time_of_change(5)+next_offset(2 BCD).
    let mut ltd = Vec::new();
    ltd.push(0x58); // local_time_offset_descriptor tag
    ltd.push(13); // length (one region entry)
    ltd.extend_from_slice(b"AUS"); // country_code (3)
    ltd.push(0x00); // country_region_id(6)+reserved(1)+polarity(1)=0 (east)
    ltd.push(0x10); // local_time_offset BCD HH=10
    ltd.push(0x00); // MM=00
    ltd.extend_from_slice(&0xC079u16.to_be_bytes()); // time_of_change MJD (2)
    ltd.push(0x00); // time_of_change BCD hh
    ltd.push(0x00); // time_of_change BCD mm
    ltd.push(0x00); // time_of_change BCD ss
    ltd.push(0x10); // next_time_offset BCD HH
    ltd.push(0x00); // next_time_offset BCD MM

    let dll = ltd.len();
    // section after 3-byte header: UTC(5) + dll word(2) + descriptors + CRC(4).
    let section_length = 5 + 2 + dll + 4;
    let mut section = Vec::new();
    section.push(0x73); // table_id
    section.push(0x70 | ((section_length >> 8) as u8 & 0x0F)); // ssi=0
    section.push((section_length & 0xFF) as u8);
    section.extend_from_slice(&0xC079u16.to_be_bytes()); // MJD
    section.push(0x12);
    section.push(0x45);
    section.push(0x00);
    section.extend_from_slice(&(0xF000u16 | dll as u16).to_be_bytes()); // reserved + dll
    section.extend_from_slice(&ltd);
    let section = with_crc(section);

    let tot = Tot::parse(&section).expect("valid TOT");
    assert_eq!(tot.utc.hours, 12);
    assert_eq!(tot.local_offset_minutes().unwrap(), Some(600)); // +10:00 = 600 min
}

#[test]
fn table_id_roundtrip() {
    for id in [
        TableId::ProgramAssociation,
        TableId::ProgramMap,
        TableId::ServiceDescriptionActual,
        TableId::TimeDate,
        TableId::SpliceInfo,
        TableId::Other(0x99),
    ] {
        assert_eq!(TableId::from_byte(id.to_byte()), id);
    }
}
