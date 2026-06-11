//! RT-2 (ADR-0034 §3): the two native discovery paths that carry **richer**
//! per-stream metadata than libav surfaces — MPEG-TS PMT and HLS master
//! playlists — fold into the same `StreamInventory` the libav path established.
//!
//! These run in the DEFAULT (pure-Rust) build: the PMT-descriptor decoders, the
//! PMT→inventory fold-in, the SCTE-35 reconciliation, and the HLS-rendition
//! fold-in are all socket-free byte/parse logic. Byte fixtures for the
//! descriptors are hand-assembled here — no live TS needed.
//!
//! Integration tests do not inherit `clippy.toml`'s test relaxations, so the
//! allow header is mandatory.
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

use multiview_core::stream::{
    Bcp47, DataKind, StabilityTier, StableStreamId, StreamDescriptor, StreamDetail,
    StreamInventory, StreamKind,
};
use multiview_input::hls::MasterPlaylist;
use multiview_input::inventory::{merge_hls, merge_ts};
use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::mpegts::descriptor::{AudioType, Descriptors};
use multiview_input::mpegts::inventory::{pmt_inventory, reconcile_scte35};
use multiview_input::mpegts::Pmt;

// ---------------------------------------------------------------------------
// On-wire section assembly helpers (copied from mpegts_psi.rs fixture style).
// ---------------------------------------------------------------------------

/// Append the big-endian CRC-32/MPEG-2 to a section body.
fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32_mpeg2(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    body
}

/// Build a long-form PSI section (header + body + CRC); computes `section_length`.
fn long_section(table_id: u8, table_id_ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
    let section_length = 5 + body.len() + 4;
    let mut out = Vec::new();
    out.push(table_id);
    let b1 = 0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F);
    out.push(b1);
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&table_id_ext.to_be_bytes());
    let version_byte = 0b1100_0000 | ((version & 0x1F) << 1) | 0x01;
    out.push(version_byte);
    out.push(0x00);
    out.push(0x00);
    out.extend_from_slice(body);
    with_crc(out)
}

/// One descriptor: tag, length-prefixed payload.
fn descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag, payload.len() as u8];
    out.extend_from_slice(payload);
    out
}

/// An `ISO_639_language_descriptor` (tag 0x0A): `[lang(3), audio_type(1)]*`.
fn iso639(lang: &str, audio_type: u8) -> Vec<u8> {
    let mut payload = lang.as_bytes()[..3].to_vec();
    payload.push(audio_type);
    descriptor(0x0A, &payload)
}

/// A `subtitling_descriptor` (tag 0x59): `[lang(3), type(1), comp_page(2), anc_page(2)]*`.
fn subtitling(lang: &str, sub_type: u8, comp: u16, anc: u16) -> Vec<u8> {
    let mut payload = lang.as_bytes()[..3].to_vec();
    payload.push(sub_type);
    payload.extend_from_slice(&comp.to_be_bytes());
    payload.extend_from_slice(&anc.to_be_bytes());
    descriptor(0x59, &payload)
}

/// A `teletext_descriptor` (tag 0x56): `[lang(3), type5|mag3(1), page(1)]*`.
fn teletext(lang: &str, tele_type: u8, magazine: u8, page: u8) -> Vec<u8> {
    let mut payload = lang.as_bytes()[..3].to_vec();
    // teletext_type (5 bits) << 3 | magazine_number (3 bits)
    payload.push((tele_type << 3) | (magazine & 0x07));
    payload.push(page);
    descriptor(0x56, &payload)
}

/// A DVB AC-3_descriptor (tag 0x6A), empty payload (flags all absent).
fn ac3() -> Vec<u8> {
    descriptor(0x6A, &[])
}

/// Build a PMT body: PCR PID, empty program info, then ES entries each carrying
/// its own descriptor-loop bytes.
fn pmt_body(pcr_pid: u16, streams: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(0xE000 | pcr_pid).to_be_bytes());
    body.extend_from_slice(&0xF000u16.to_be_bytes());
    for (stype, pid, descs) in streams {
        body.push(*stype);
        body.extend_from_slice(&(0xE000 | *pid).to_be_bytes());
        let es_info_len = descs.len() as u16;
        body.extend_from_slice(&(0xF000 | es_info_len).to_be_bytes());
        body.extend_from_slice(descs);
    }
    body
}

// ===========================================================================
// (a) typed ES-descriptor decoders — language + role from byte fixtures.
// ===========================================================================

#[test]
fn iso639_descriptor_decodes_language_and_audio_role() {
    // A single ISO_639_language_descriptor: French, audio_type 0x03
    // (visual-impaired commentary).
    let loop_bytes = iso639("fra", 0x03);
    let descs = Descriptors::parse(&loop_bytes).expect("parse loop");
    let iso = descs.iso_639_language().expect("an iso-639 descriptor");
    assert_eq!(iso.entries.len(), 1);
    assert_eq!(iso.entries[0].language, "fra");
    assert_eq!(
        iso.entries[0].audio_type,
        AudioType::VisualImpairedCommentary
    );
}

#[test]
fn iso639_descriptor_decodes_hearing_impaired_role() {
    let loop_bytes = iso639("deu", 0x02);
    let descs = Descriptors::parse(&loop_bytes).expect("parse");
    let iso = descs.iso_639_language().expect("iso-639");
    assert_eq!(iso.entries[0].language, "deu");
    assert_eq!(iso.entries[0].audio_type, AudioType::HearingImpaired);
}

#[test]
fn subtitling_descriptor_decodes_language_and_type() {
    let loop_bytes = subtitling("spa", 0x10, 0x0001, 0x0002);
    let descs = Descriptors::parse(&loop_bytes).expect("parse");
    let sub = descs.subtitling().expect("a subtitling descriptor");
    assert_eq!(sub.entries.len(), 1);
    assert_eq!(sub.entries[0].language, "spa");
    assert_eq!(sub.entries[0].subtitling_type, 0x10);
    assert_eq!(sub.entries[0].composition_page_id, 0x0001);
    assert_eq!(sub.entries[0].ancillary_page_id, 0x0002);
}

#[test]
fn teletext_descriptor_decodes_language_and_page() {
    // English teletext subtitle (type 0x02), magazine 1, page 0x88.
    let loop_bytes = teletext("eng", 0x02, 1, 0x88);
    let descs = Descriptors::parse(&loop_bytes).expect("parse");
    let txt = descs.teletext().expect("a teletext descriptor");
    assert_eq!(txt.entries.len(), 1);
    assert_eq!(txt.entries[0].language, "eng");
    assert_eq!(txt.entries[0].teletext_type, 0x02);
    assert_eq!(txt.entries[0].magazine_number, 1);
    assert_eq!(txt.entries[0].page_number, 0x88);
}

#[test]
fn ac3_descriptor_is_detected() {
    let loop_bytes = ac3();
    let descs = Descriptors::parse(&loop_bytes).expect("parse");
    assert!(descs.has_ac3(), "DVB AC-3 descriptor present");
}

// ===========================================================================
// (a, continued) PMT fold-in — descriptors fill language + role + default.
// ===========================================================================

#[test]
fn pmt_inventory_fills_audio_language_and_role_from_descriptors() {
    // Program: H.264 video on 0x0100, two AAC audio tracks with ISO-639 langs,
    // one a hearing-impaired track; a DVB-subtitle PID with a subtitling desc.
    let video = (0x1Bu8, 0x0100u16, Vec::<u8>::new());
    let audio_eng = (0x0Fu8, 0x0101u16, iso639("eng", 0x00));
    let audio_deu_hi = (0x0Fu8, 0x0102u16, iso639("deu", 0x02));
    let subs_spa = (0x06u8, 0x0103u16, subtitling("spa", 0x10, 0x0001, 0x0002));
    let body = pmt_body(
        0x0100,
        &[
            video.clone(),
            audio_eng.clone(),
            audio_deu_hi.clone(),
            subs_spa.clone(),
        ],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("valid PMT");

    let inv = pmt_inventory(&pmt).expect("fold PMT into inventory");

    // Audio languages came from the ISO-639 descriptors (libav often misses
    // these). `Bcp47` validates the ISO-639-2 code as-is — it does not down-map
    // alpha-3 to alpha-2 (a lossy conversion it does not perform), so "eng"/"deu"
    // are the normalised tags.
    let mut audio_langs: Vec<String> = inv
        .audio_tracks()
        .filter_map(|s| s.language.as_ref().map(|l| l.as_str().to_owned()))
        .collect();
    audio_langs.sort();
    assert_eq!(audio_langs, vec!["deu".to_owned(), "eng".to_owned()]);

    // The hearing-impaired German track is flagged via title role.
    let deu = inv
        .audio_tracks()
        .find(|s| s.language.as_ref().map(Bcp47::as_str) == Some("deu"))
        .expect("german audio");
    assert_eq!(
        deu.title.as_deref(),
        Some("hearing impaired"),
        "audio_type role surfaced as a title hint"
    );

    // The subtitle's language came from the subtitling descriptor.
    let sub = inv.subtitle_tracks().next().expect("a subtitle stream");
    assert_eq!(sub.language.as_ref().map(Bcp47::as_str), Some("spa"));
    assert!(sub.kind.is_subtitle());

    // Every TS descriptor is PID-keyed (hard tier), so the ids survive a
    // PMT-version bump / reorder.
    assert!(
        inv.streams
            .iter()
            .all(|s| s.id.tier() == StabilityTier::Hard),
        "TS PMT ids are PID-keyed hard ids"
    );
    let v = inv.video().next().expect("video");
    assert_eq!(v.id, StableStreamId::from_ts_pid(StreamKind::Video, 0x0100));
}

#[test]
fn pmt_inventory_classifies_scte35_as_data() {
    // Video + an SCTE-35 PID (stream_type 0x86).
    let body = pmt_body(
        0x0100,
        &[(0x1B, 0x0100, Vec::new()), (0x86, 0x01F0, Vec::new())],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("PMT");
    let inv = pmt_inventory(&pmt).expect("fold");

    let scte: Vec<_> = inv
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(scte.len(), 1, "exactly one SCTE-35 data stream");
    assert_eq!(
        scte[0].id,
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x01F0)
    );
}

// ===========================================================================
// (b) SCTE-35 reconciliation — one Data(Scte35) per PID, no double, no miss.
// ===========================================================================

#[test]
fn scte35_reconciliation_dedupes_against_general_demux_row() {
    // The general-demux path already produced a Data(Scte35) row for PID 0x01F0
    // (libav saw an `scte_35` codec) AND the PSI selection.rs path reports the
    // same PID. Reconciliation must yield EXACTLY ONE Data(Scte35) for 0x01F0
    // (no double-list) and keep it under the stable PID-keyed id.
    let pmt_body_bytes = pmt_body(
        0x0100,
        &[(0x1B, 0x0100, Vec::new()), (0x86, 0x01F0, Vec::new())],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &pmt_body_bytes)).expect("PMT");
    let base = pmt_inventory(&pmt).expect("fold");

    // Simulate the general-demux SCTE row being present too: fold the same PID
    // in again via reconciliation. The selection.rs scte35_pids carries 0x01F0.
    let reconciled = reconcile_scte35(base.clone(), &[0x01F0]);
    let scte: Vec<_> = reconciled
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(
        scte.len(),
        1,
        "the PSI PID and the existing row reconcile to ONE Data(Scte35)"
    );
    assert_eq!(
        scte[0].id,
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x01F0),
        "kept under the stable PID-keyed id"
    );
}

#[test]
fn scte35_reconciliation_adds_a_missing_psi_pid() {
    // The general-demux base had NO SCTE-35 row, but selection.rs found the PID
    // in the PMT (PSI). Reconciliation must ADD it (no miss).
    let body = pmt_body(0x0100, &[(0x1B, 0x0100, Vec::new())]);
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("PMT");
    let base = pmt_inventory(&pmt).expect("fold");
    assert_eq!(
        base.by_kind(StreamKind::is_data).count(),
        0,
        "base has no data stream"
    );

    let reconciled = reconcile_scte35(base, &[0x0200]);
    let scte: Vec<_> = reconciled
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(scte.len(), 1, "the missing PSI SCTE PID was added");
    assert_eq!(
        scte[0].id,
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x0200)
    );
}

#[test]
fn scte35_reconciliation_idempotent_on_repeat() {
    // Running reconciliation twice with the same PID set is stable (no growth).
    let body = pmt_body(
        0x0100,
        &[(0x1B, 0x0100, Vec::new()), (0x86, 0x01F0, Vec::new())],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("PMT");
    let once = reconcile_scte35(pmt_inventory(&pmt).expect("fold"), &[0x01F0]);
    let twice = reconcile_scte35(once.clone(), &[0x01F0]);
    assert_eq!(
        once.by_kind(StreamKind::is_data).count(),
        twice.by_kind(StreamKind::is_data).count(),
        "reconciliation is idempotent"
    );
    assert_eq!(twice.by_kind(StreamKind::is_data).count(), 1);
}

// ===========================================================================
// (c) HLS AUDIO + SUBTITLES rendition fold-in via the shared resolver.
// ===========================================================================

const HLS_MASTER: &str = "#EXTM3U\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/eng/index.m3u8\"\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Espanol\",LANGUAGE=\"es\",DEFAULT=NO,AUTOSELECT=YES,URI=\"audio/spa/index.m3u8\"\n\
    #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subs/eng/index.m3u8\"\n\
    #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Francais\",LANGUAGE=\"fr\",DEFAULT=NO,AUTOSELECT=YES,FORCED=YES,URI=\"subs/fra/index.m3u8\"\n\
    #EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"aud\",SUBTITLES=\"subs\"\n\
    video/variant.m3u8\n";

#[test]
fn hls_master_folds_audio_and_subtitle_renditions_with_languages() {
    let master = MasterPlaylist::parse(HLS_MASTER).expect("parse master");
    let inv = master.stream_inventory();

    let mut audio_langs: Vec<String> = inv
        .audio_tracks()
        .filter_map(|s| s.language.as_ref().map(|l| l.as_str().to_owned()))
        .collect();
    audio_langs.sort();
    assert_eq!(audio_langs, vec!["en".to_owned(), "es".to_owned()]);

    let mut sub_langs: Vec<String> = inv
        .subtitle_tracks()
        .filter_map(|s| s.language.as_ref().map(|l| l.as_str().to_owned()))
        .collect();
    sub_langs.sort();
    assert_eq!(sub_langs, vec!["en".to_owned(), "fr".to_owned()]);

    // The forced French subtitle carries its forced flag through the detail.
    let fr = inv
        .subtitle_tracks()
        .find(|s| s.language.as_ref().map(Bcp47::as_str) == Some("fr"))
        .expect("french subs");
    assert_eq!(
        fr.detail,
        multiview_core::stream::StreamDetail::Subtitle { forced: true }
    );

    // The DEFAULT English audio is flagged default.
    let en_audio = inv
        .audio_tracks()
        .find(|s| s.language.as_ref().map(Bcp47::as_str) == Some("en"))
        .expect("english audio");
    assert!(en_audio.default, "DEFAULT=YES audio is default");
}

#[test]
fn hls_rendition_ids_are_hard_group_plus_name() {
    let master = MasterPlaylist::parse(HLS_MASTER).expect("parse");
    let inv = master.stream_inventory();
    assert!(
        inv.streams
            .iter()
            .all(|s| s.id.tier() == StabilityTier::Hard),
        "HLS ids are group+name hard ids"
    );
    let en_audio = inv
        .audio_tracks()
        .find(|s| s.language.as_ref().map(Bcp47::as_str) == Some("en"))
        .expect("english audio");
    assert_eq!(
        en_audio.id,
        StableStreamId::from_hls(StreamKind::Audio, "aud", "English")
    );
}

// ===========================================================================
// (d) empty-NAME renditions get synthesised, non-colliding stable ids.
// ===========================================================================

#[test]
fn hls_empty_name_renditions_get_non_colliding_synthesised_ids() {
    // Two AUDIO renditions in the SAME group with NO NAME. RFC 8216 requires a
    // non-empty NAME, but real playlists omit it; an empty NAME would make both
    // renditions collide on the SAME (group_id + "") stable id. The fold-in must
    // synthesise a non-empty NAME so the two ids are DISTINCT.
    let master_text = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",LANGUAGE=\"en\",URI=\"a0.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",LANGUAGE=\"es\",URI=\"a1.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"aud\"\n\
        v.m3u8\n";
    let master = MasterPlaylist::parse(master_text).expect("parse");
    let inv = master.stream_inventory();

    let audio: Vec<_> = inv.audio_tracks().collect();
    assert_eq!(audio.len(), 2, "both name-less audio renditions survive");
    assert_ne!(
        audio[0].id, audio[1].id,
        "synthesised NAMEs avoid the empty-name id collision"
    );
    // Both remain HARD-tier hls ids.
    assert!(audio.iter().all(|s| s.id.tier() == StabilityTier::Hard));
}

// ===========================================================================
// (4) ONE unified StreamInventory through the same merge surface, whether the
//     container is TS/SRT (PMT enrichment) or HLS (rendition fold-in).
// ===========================================================================

/// A minimal general-demux base inventory: a soft-keyed audio row that LACKS a
/// language (the exact thing libav's TS metadata misses), to be enriched by the
/// PMT merge.
fn general_base_audio_no_lang() -> StreamInventory {
    let id = StableStreamId::from_general(StreamKind::Audio, 0, "aac", None, None);
    StreamInventory::from_streams(vec![StreamDescriptor::new(
        id,
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels: 2,
            sample_rate: 48_000,
        },
    )])
}

#[test]
fn ts_merge_enriches_base_audio_language_and_reconciles_scte() {
    // A general-demux base (a language-less audio row, decoded channel layout)
    // plus the PMT (which carries the ISO-639 language + an SCTE-35 PID). The
    // unified TS surface overlays the language WITHOUT losing the decoded detail,
    // and reconciles the SCTE PID to exactly one Data(Scte35).
    let body = pmt_body(
        0x0100,
        &[
            (0x1B, 0x0100, Vec::new()),
            (0x0F, 0x0101, iso639("fra", 0x00)),
            (0x86, 0x01F0, Vec::new()),
        ],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("PMT");

    let unified = merge_ts(general_base_audio_no_lang(), &pmt);

    // The base audio row gained the PMT language but kept its decoded layout.
    let audio = unified.audio_tracks().next().expect("audio");
    assert_eq!(audio.language.as_ref().map(Bcp47::as_str), Some("fra"));
    assert_eq!(audio.detail.audio_layout(), Some((2, 48_000)));

    // Exactly one SCTE-35 data stream, PID-keyed.
    let scte: Vec<_> = unified
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(scte.len(), 1);
    assert_eq!(
        scte[0].id,
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x01F0)
    );
}

#[test]
fn ts_merge_dedupes_scte_against_a_general_demux_scte_row() {
    // The base ALREADY carries a soft general-demux SCTE-35 row (libav saw the
    // scte_35 codec) for the same physical PID the PMT reports. The unified TS
    // surface must NOT double-list it: exactly one Data(Scte35) survives.
    let soft_scte = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Data(DataKind::Scte35), 0, "scte_35", None, None),
        StreamKind::Data(DataKind::Scte35),
        "scte_35",
        StreamDetail::Passthrough,
    );
    let mut base = general_base_audio_no_lang();
    base.streams.push(soft_scte);

    let body = pmt_body(
        0x0100,
        &[(0x1B, 0x0100, Vec::new()), (0x86, 0x01F0, Vec::new())],
    );
    let pmt = Pmt::parse(&long_section(0x02, 0x0001, 1, &body)).expect("PMT");
    let unified = merge_ts(base, &pmt);

    let scte: Vec<_> = unified
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(
        scte.len(),
        1,
        "the soft general SCTE row and the PSI PID reconcile to ONE"
    );
    assert_eq!(
        scte[0].id,
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x01F0),
        "kept under the stable hard PID id, not the soft general id"
    );
}

#[test]
fn hls_merge_appends_renditions_onto_the_base() {
    // The general-demux base (the variant's muxed video+audio) plus the master's
    // alternate AUDIO/SUBTITLES renditions → one unified inventory carrying them
    // all, through the SAME merge surface as the TS path.
    let base = general_base_audio_no_lang();
    let master = MasterPlaylist::parse(HLS_MASTER).expect("parse");
    let unified = merge_hls(base, &master);

    // The base audio plus the two AUDIO renditions = 3 audio rows; the 2 subs.
    assert_eq!(unified.audio_tracks().count(), 3, "base + 2 HLS audio");
    assert_eq!(unified.subtitle_tracks().count(), 2, "2 HLS subtitle");
    // The HLS-sourced rows are hard group+name ids.
    let hls_audio = unified
        .audio_tracks()
        .find(|s| s.language.as_ref().map(Bcp47::as_str) == Some("es"))
        .expect("spanish HLS audio");
    assert_eq!(hls_audio.id.tier(), StabilityTier::Hard);
}

#[test]
fn hls_empty_name_id_is_stable_across_reparse() {
    // The synthesised NAME must be DETERMINISTIC (derived from group + ordinal),
    // so the same playlist parsed twice yields the SAME ids — otherwise a
    // crosspoint bound to a name-less rendition would not survive a re-probe.
    let master_text = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",LANGUAGE=\"en\",URI=\"a0.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",LANGUAGE=\"es\",URI=\"a1.m3u8\"\n";
    let a = MasterPlaylist::parse(master_text).expect("parse");
    let b = MasterPlaylist::parse(master_text).expect("re-parse");
    let ia: Vec<_> = a
        .stream_inventory()
        .audio_tracks()
        .map(|s| s.id.clone())
        .collect();
    let ib: Vec<_> = b
        .stream_inventory()
        .audio_tracks()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(ia, ib, "synthesised ids are deterministic across re-parse");
}
