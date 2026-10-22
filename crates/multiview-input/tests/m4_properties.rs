//! Property tests for the M4 ingest parsers (MPEG-TS PSI/SI, SCTE-35/104, DASH
//! MPD, SRT, WebRTC SDP): on arbitrary / adversarial input they must never panic,
//! never hang, and always return a typed error or a valid value. These run in the
//! DEFAULT (pure-Rust) build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;

use multiview_input::dash::Mpd;
use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::mpegts::{Cat, Nit, Pat, Pmt, Sdt, Tdt, Tot};
use multiview_input::scte::scte104::Scte104Message;
use multiview_input::scte::splice35::SpliceInfoSection;
use multiview_input::srt::StreamId;

proptest! {
    // Wide net: every PSI/SI table parser is total on arbitrary bytes.
    #[test]
    fn psi_parsers_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Pat::parse(&bytes);
        let _ = Pmt::parse(&bytes);
        let _ = Nit::parse(&bytes);
        let _ = Sdt::parse(&bytes);
        let _ = Cat::parse(&bytes);
        let _ = Tdt::parse(&bytes);
        let _ = Tot::parse(&bytes);
    }

    // The SCTE parsers are total on arbitrary bytes.
    #[test]
    fn scte_parsers_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = SpliceInfoSection::parse(&bytes);
        let _ = Scte104Message::parse(&bytes);
    }

    // CRC-32/MPEG-2 is deterministic and order-independent only at the byte level
    // (a structural property): appending the big-endian CRC makes the section
    // self-validate to a running CRC of 0.
    #[test]
    fn crc_self_check(body in proptest::collection::vec(any::<u8>(), 0..256)) {
        let mut section = body.clone();
        let crc = crc32_mpeg2(&body);
        section.extend_from_slice(&crc.to_be_bytes());
        prop_assert_eq!(crc32_mpeg2(&section), 0);
    }

    // A well-formed PAT with arbitrary (program, pid) pairs round-trips: every
    // pair we encode is recovered (PID masked to 13 bits, as on the wire).
    #[test]
    fn pat_roundtrips_programs(
        programs in proptest::collection::vec((any::<u16>(), 0u16..0x2000), 0..20),
        tsid in any::<u16>(),
    ) {
        let mut body = Vec::new();
        for &(pn, pid) in &programs {
            body.extend_from_slice(&pn.to_be_bytes());
            body.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes());
        }
        // Assemble a long-form section.
        let section_length = 5 + body.len() + 4;
        let mut section = vec![
            0x00u8,
            0b1000_0000 | 0b0011_0000 | u8::try_from((section_length >> 8) & 0x0F).unwrap_or(0),
            u8::try_from(section_length & 0xFF).unwrap_or(0),
        ];
        section.extend_from_slice(&tsid.to_be_bytes());
        section.push(0xC1); // version 0, current
        section.push(0x00);
        section.push(0x00);
        section.extend_from_slice(&body);
        let crc = crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        let pat = Pat::parse(&section).expect("self-built PAT validates");
        prop_assert_eq!(pat.transport_stream_id, tsid);
        prop_assert_eq!(pat.programs.len(), programs.len());
        for (got, &(pn, pid)) in pat.programs.iter().zip(programs.iter()) {
            prop_assert_eq!(got.program_number, pn);
            prop_assert_eq!(got.pid, pid & 0x1FFF);
        }
    }

    // The MPD parser is total on arbitrary text.
    #[test]
    fn mpd_parser_never_panics(text in ".{0,2000}") {
        let _ = Mpd::parse(&text);
    }

    // StreamId acceptance is exactly the 0..=512-byte band.
    #[test]
    fn srt_stream_id_bound(s in ".{0,600}") {
        let result = StreamId::new(s.clone());
        prop_assert_eq!(result.is_ok(), s.len() <= StreamId::MAX_BYTES);
    }
}
