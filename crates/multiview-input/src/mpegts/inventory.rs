//! MPEG-TS PMT → [`StreamInventory`] fold-in (RT-2, ADR-0034 §3).
//!
//! The general libav demux path (RT-1) already surfaces every container stream,
//! but libav's MPEG-TS metadata frequently **misses** the per-elementary-stream
//! language and accessibility role that the PMT's descriptor loops carry, and it
//! does not classify a DVB-subtitle / teletext private stream as a subtitle. This
//! module folds the **richer** PMT signalling into the same typed
//! [`StreamInventory`] so a TS/SRT input yields one unified inventory regardless
//! of container:
//!
//! * each [`ElementaryStream`] is mapped to its canonical [`StreamKind`] using its
//!   [`StreamType`] **and** its ES-info descriptor loop (a `PrivatePes` stream is
//!   a subtitle when it carries a `subtitling`/`teletext` descriptor, audio when
//!   it carries an AC-3 descriptor);
//! * the `ISO_639` / subtitling / teletext descriptors fill the
//!   [`StreamDescriptor::language`] (validated onto [`Bcp47`]) and an
//!   accessibility role hint onto the title;
//! * SCTE-35 PIDs are folded in as [`StreamKind::Data`]\([`DataKind::Scte35`]\),
//!   **reconciled** with any SCTE-35 row the general-demux path already produced so
//!   a TS input neither double-lists nor misses SCTE-35 (see [`reconcile_scte35`]).
//!
//! Every TS descriptor binds to a **hard**, PID-keyed [`StableStreamId`] (the PID
//! is the genuinely stable identity of an elementary stream in a multiplex), so a
//! crosspoint survives a PMT-version bump / reorder.
//!
//! Pure, libav-free byte logic; runs in the default build, off the engine.

use multiview_core::stream::{
    Bcp47, DataKind, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};

use super::descriptor::Descriptors;
use super::pmt::{ElementaryStream, Pmt, StreamType};
use super::MpegTsError;

/// Fold a parsed [`Pmt`] into a typed [`StreamInventory`] (RT-2).
///
/// Every elementary stream becomes a [`StreamDescriptor`] keyed by a hard,
/// PID-scoped [`StableStreamId`]; the ES-info descriptor loop fills the language
/// (onto a validated [`Bcp47`]) and an accessibility role hint. SCTE-35 PIDs are
/// classified as [`StreamKind::Data`]\([`DataKind::Scte35`]\).
///
/// # Errors
///
/// Returns [`MpegTsError`] only if an ES-info descriptor loop is malformed (it
/// runs past the section). A well-formed PMT never errors.
pub fn pmt_inventory(pmt: &Pmt) -> Result<StreamInventory, MpegTsError> {
    let mut streams = Vec::with_capacity(pmt.streams.len());
    for es in &pmt.streams {
        streams.push(elementary_to_descriptor(es)?);
    }
    Ok(StreamInventory::from_streams(streams))
}

/// Reconcile the PSI-discovered SCTE-35 PIDs (from
/// [`super::selection::SelectedProgram::scte35_pids`]) with an existing inventory
/// — typically one already carrying SCTE-35 rows from the general-demux path —
/// so each SCTE-35 PID appears **exactly once** (no double-list, no miss).
///
/// For each PID in `scte35_pids` the canonical, hard, PID-keyed id is
/// [`StableStreamId::from_ts_pid`]`(Data(Scte35), pid)`. Reconciliation:
///
/// * a PID already present as a `Data(Scte35)` row under that hard id is left
///   untouched (no double-list);
/// * a PID present only as a general-demux `Data(Scte35)` row under a *different*
///   (soft) id is **re-keyed** to the hard PID id and de-duplicated;
/// * a PID present in the PSI but absent from the inventory is **added** (no miss).
///
/// The operation is idempotent: applying it twice with the same PID set is a
/// no-op after the first.
#[must_use]
pub fn reconcile_scte35(mut inventory: StreamInventory, scte35_pids: &[u16]) -> StreamInventory {
    for &pid in scte35_pids {
        let hard_id = StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), pid);

        // Is this exact hard PID id already present? Then nothing to do.
        let already = inventory.streams.iter().any(|s| s.id == hard_id);
        if already {
            continue;
        }

        // Add the canonical hard PID row. Any soft general-demux SCTE-35 row for
        // the *same physical PID* is not removed here (a soft id is an opaque
        // hash that does not name the PID, so it cannot be matched by PID); it is
        // dropped in the `dedup_scte35` pass below, which — when the PSI reported
        // any SCTE PID — treats the hard PID rows as authoritative and discards
        // the soft general SCTE rows in their favour.
        inventory.streams.push(StreamDescriptor::new(
            hard_id,
            StreamKind::Data(DataKind::Scte35),
            "scte_35",
            StreamDetail::Passthrough,
        ));
    }

    // Collapse SCTE-35 rows so there is exactly one per distinct id, preferring
    // the hard PID-keyed rows: when the PSI reported any SCTE PID, the hard rows
    // are authoritative, so soft general SCTE rows are dropped in their favour.
    dedup_scte35(&mut inventory, !scte35_pids.is_empty());
    inventory
}

/// Drop redundant SCTE-35 rows. When `prefer_hard` is set (the PSI reported at
/// least one SCTE PID), soft-keyed general SCTE-35 rows are removed in favour of
/// the authoritative hard PID rows; hard rows are always de-duplicated by id.
fn dedup_scte35(inventory: &mut StreamInventory, prefer_hard: bool) {
    use std::collections::HashSet;
    let mut seen: HashSet<StableStreamId> = HashSet::new();
    inventory.streams.retain(|s| {
        let is_scte = s.kind == StreamKind::Data(DataKind::Scte35);
        if !is_scte {
            return true;
        }
        let is_hard = matches!(s.id.tier(), multiview_core::stream::StabilityTier::Hard);
        if prefer_hard && !is_hard {
            // A soft general SCTE row, superseded by the PSI hard rows.
            return false;
        }
        // De-duplicate by id (a repeated PSI PID, or a repeated soft id).
        seen.insert(s.id.clone())
    });
}

/// Map one [`ElementaryStream`] to its canonical [`StreamDescriptor`].
fn elementary_to_descriptor(es: &ElementaryStream) -> Result<StreamDescriptor, MpegTsError> {
    let descs = es.descriptors()?;
    let kind = classify(es.stream_type, &descs);

    let id = StableStreamId::from_ts_pid(kind, es.pid);
    let language = descriptor_language(kind, &descs);
    let (title, default) = role_hint(kind, &descs);
    let detail = detail_for(kind, &descs);
    let codec = codec_name(es.stream_type, kind);

    Ok(StreamDescriptor::new(id, kind, codec, detail)
        .with_language(language)
        .with_title(title)
        .with_default(default))
}

/// Classify an elementary stream into the canonical [`StreamKind`] using both its
/// [`StreamType`] and its ES-info descriptor loop.
///
/// `PrivatePes` (0x06) is the DVB carrier for subtitles, teletext, and AC-3
/// audio — only the descriptor loop disambiguates which.
fn classify(stream_type: StreamType, descs: &Descriptors<'_>) -> StreamKind {
    if stream_type.is_video() {
        return StreamKind::Video;
    }
    if stream_type.is_audio() {
        return StreamKind::Audio;
    }
    match stream_type {
        StreamType::Scte35 => StreamKind::Data(DataKind::Scte35),
        StreamType::PrivatePes | StreamType::PrivateSections | StreamType::Other(_) => {
            if descs.subtitling().is_some() || descs.teletext_subtitle_present() {
                StreamKind::Subtitle
            } else if descs.has_ac3() {
                StreamKind::Audio
            } else {
                // Unknown private essence: keep it routable as a generic
                // passthrough rather than dropping it.
                StreamKind::Data(DataKind::Klv)
            }
        }
        // Any remaining declared type that is neither audio nor video and not a
        // private/SCTE carrier: a generic passthrough.
        _ => StreamKind::Data(DataKind::Klv),
    }
}

/// Resolve the validated language for a stream from its descriptor loop.
///
/// Audio reads the `ISO_639_language_descriptor`; subtitle/teletext reads the
/// subtitling / teletext descriptor. Data / timecode carry no language.
fn descriptor_language(kind: StreamKind, descs: &Descriptors<'_>) -> Option<Bcp47> {
    let raw: Option<String> = match kind {
        StreamKind::Audio => descs
            .iso_639_language()
            .and_then(|d| d.first().map(|e| e.language.clone())),
        StreamKind::Subtitle => descs
            .subtitling()
            .and_then(|d| d.first().map(|e| e.language.clone()))
            .or_else(|| {
                descs
                    .teletext()
                    .and_then(|d| d.first().map(|e| e.language.clone()))
            })
            // A teletext-subtitle private stream may also carry an ISO_639 tag.
            .or_else(|| {
                descs
                    .iso_639_language()
                    .and_then(|d| d.first().map(|e| e.language.clone()))
            }),
        // Video / data / timecode: PMT descriptors carry no routing language.
        _ => None,
    };
    raw.as_deref().and_then(|r| Bcp47::parse(r).ok())
}

/// Build a `(title, default)` pair for a stream: an accessibility role hint as a
/// title, and a default flag for hard-of-hearing-tagged subtitles.
fn role_hint(kind: StreamKind, descs: &Descriptors<'_>) -> (Option<String>, bool) {
    match kind {
        StreamKind::Audio => {
            let role = descs
                .iso_639_language()
                .and_then(|d| d.first().and_then(|e| e.audio_type.role_label()));
            (role.map(str::to_owned), false)
        }
        StreamKind::Subtitle => {
            // A hard-of-hearing subtitling type / teletext subtitle for the HoH
            // is surfaced as a role title hint.
            let hoh = descs
                .subtitling()
                .and_then(|d| {
                    d.first()
                        .map(super::descriptor::SubtitlingEntry::is_hard_of_hearing)
                })
                .unwrap_or(false);
            if hoh {
                (Some("hearing impaired".to_owned()), false)
            } else {
                (None, false)
            }
        }
        _ => (None, false),
    }
}

/// Build the kind-specific [`StreamDetail`].
///
/// The PMT carries no coded geometry / channel layout (that needs a decode), so
/// video / audio detail is the zero-valued shape; subtitle forced-ness comes from
/// the subtitling type; everything else is passthrough.
fn detail_for(kind: StreamKind, descs: &Descriptors<'_>) -> StreamDetail {
    match kind {
        StreamKind::Video => StreamDetail::Video {
            width: 0,
            height: 0,
            frame_rate: None,
        },
        StreamKind::Audio => StreamDetail::Audio {
            channels: 0,
            sample_rate: 0,
        },
        StreamKind::Subtitle => StreamDetail::Subtitle {
            forced: subtitle_forced(descs),
        },
        StreamKind::Data(_) | StreamKind::Timecode(_) | _ => StreamDetail::Passthrough,
    }
}

/// Whether a subtitle stream is "forced". DVB does not carry an explicit forced
/// flag in the subtitling descriptor; a teletext-subtitle type `0x05` (essential
/// for the hard of hearing) is the closest signal, otherwise `false`.
fn subtitle_forced(descs: &Descriptors<'_>) -> bool {
    descs
        .teletext()
        .and_then(|d| d.first().map(|e| e.teletext_type == 0x05))
        .unwrap_or(false)
}

/// A container-style codec descriptor name for an elementary stream, mirroring
/// the libav `codec_id_name`s the general-demux path produces so the two
/// discovery surfaces line up.
fn codec_name(stream_type: StreamType, kind: StreamKind) -> &'static str {
    match stream_type {
        StreamType::Mpeg1Video => "mpeg1video",
        StreamType::Mpeg2Video => "mpeg2video",
        StreamType::Mpeg4Video => "mpeg4",
        StreamType::H264 => "h264",
        StreamType::Hevc => "hevc",
        StreamType::Vvc => "vvc",
        StreamType::Mpeg1Audio => "mp1",
        StreamType::Mpeg2Audio => "mp2",
        StreamType::AdtsAac | StreamType::LatmAac => "aac",
        StreamType::Ac3 => "ac3",
        StreamType::EAc3 => "eac3",
        StreamType::Smpte302mAudio => "smpte_302m",
        StreamType::Scte35 => "scte_35",
        StreamType::PrivatePes | StreamType::PrivateSections | StreamType::Other(_) => match kind {
            StreamKind::Subtitle => "dvb_subtitle",
            StreamKind::Audio => "ac3",
            _ => "data",
        },
    }
}

// Bridge helper: a teletext descriptor whose first entry is a subtitle page.
impl Descriptors<'_> {
    /// Whether a `teletext_descriptor` is present **and** its first page is a
    /// subtitle page (`teletext_type` `0x02`/`0x05`) — the signal that a
    /// `PrivatePes` stream carrying teletext is a routable subtitle.
    #[must_use]
    fn teletext_subtitle_present(&self) -> bool {
        self.teletext()
            .and_then(|d| d.first().map(super::descriptor::TeletextEntry::is_subtitle))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2-byte tag+len descriptor wrapper.
    fn desc(tag: u8, payload: &[u8]) -> Vec<u8> {
        let len = u8::try_from(payload.len()).unwrap_or(0);
        let mut v = vec![tag, len];
        v.extend_from_slice(payload);
        v
    }

    fn es(stream_type: u8, pid: u16, descs: Vec<u8>) -> ElementaryStream {
        ElementaryStream {
            stream_type: StreamType::from_byte(stream_type),
            pid,
            descriptors: descs,
        }
    }

    #[test]
    fn private_pes_with_subtitling_descriptor_classifies_as_subtitle() {
        // PrivatePes (0x06) carrying a subtitling_descriptor → Subtitle, spa.
        let stream = es(0x06, 0x0200, desc(0x59, b"spa\x10\x00\x01\x00\x02"));
        let d = stream.descriptors().expect("descs");
        assert_eq!(classify(StreamType::PrivatePes, &d), StreamKind::Subtitle);
        assert_eq!(
            descriptor_language(StreamKind::Subtitle, &d)
                .as_ref()
                .map(Bcp47::as_str),
            Some("spa")
        );
    }

    #[test]
    fn private_pes_with_ac3_descriptor_classifies_as_audio() {
        let stream = es(0x06, 0x0201, desc(0x6A, &[]));
        let d = stream.descriptors().expect("descs");
        assert_eq!(classify(StreamType::PrivatePes, &d), StreamKind::Audio);
    }

    #[test]
    fn private_pes_with_teletext_subtitle_classifies_as_subtitle() {
        // teletext type 0x02 (subtitle), magazine 1, page 0x88.
        let stream = es(0x06, 0x0202, desc(0x56, b"fra\x11\x88"));
        let d = stream.descriptors().expect("descs");
        assert_eq!(classify(StreamType::PrivatePes, &d), StreamKind::Subtitle);
        assert_eq!(
            descriptor_language(StreamKind::Subtitle, &d)
                .as_ref()
                .map(Bcp47::as_str),
            Some("fra")
        );
    }

    #[test]
    fn private_pes_without_known_descriptors_is_generic_data() {
        let stream = es(0x06, 0x0203, Vec::new());
        let empty = stream.descriptors().expect("descs");
        assert_eq!(
            classify(StreamType::PrivatePes, &empty),
            StreamKind::Data(DataKind::Klv)
        );
    }

    #[test]
    fn audio_role_hint_surfaces_visual_impaired() {
        // visual impaired commentary (audio_type 0x03).
        let stream = es(0x0F, 0x0101, desc(0x0A, b"eng\x03"));
        let d = stream.descriptors().expect("descs");
        let (title, _) = role_hint(StreamKind::Audio, &d);
        assert_eq!(title.as_deref(), Some("visual impaired"));
    }

    #[test]
    fn reconcile_adds_then_is_idempotent() {
        let base = StreamInventory::new();
        let once = reconcile_scte35(base, &[0x0100]);
        assert_eq!(
            once.by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
                .count(),
            1
        );
        let twice = reconcile_scte35(once, &[0x0100]);
        assert_eq!(
            twice
                .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
                .count(),
            1,
            "no growth on repeat"
        );
    }

    #[test]
    fn reconcile_empty_psi_set_drops_no_existing_rows() {
        // With no PSI SCTE PIDs, a pre-existing soft general SCTE row is kept
        // (prefer_hard is off), so we never silently drop the general-demux row.
        let soft = StreamDescriptor::new(
            StableStreamId::from_general(
                StreamKind::Data(DataKind::Scte35),
                0,
                "scte_35",
                None,
                None,
            ),
            StreamKind::Data(DataKind::Scte35),
            "scte_35",
            StreamDetail::Passthrough,
        );
        let inv = StreamInventory::from_streams(vec![soft]);
        let out = reconcile_scte35(inv, &[]);
        assert_eq!(
            out.by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
                .count(),
            1,
            "an empty PSI set never drops the general-demux SCTE row"
        );
    }
}
