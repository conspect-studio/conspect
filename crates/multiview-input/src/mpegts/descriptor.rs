//! MPEG-2 / DVB **descriptor** loops (ISO/IEC 13818-1 §2.6, ETSI EN 300 468 §6).
//!
//! Descriptors are the extensible `tag` + `length` + `payload` triples that hang
//! off the program loops of a [`super::pmt`], the transport-stream loop of a
//! [`super::nit`], the service loop of an [`super::sdt`], and elsewhere. This
//! module parses a descriptor **loop** into a bounded list of borrowed
//! [`Descriptor`]s without interpreting their payloads (each table extracts the
//! few descriptors it cares about).

use super::MpegTsError;

/// One raw descriptor: an 8-bit tag, plus its payload bytes (the 8-bit length is
/// consumed during parsing and reflected by `data.len()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor<'a> {
    /// The descriptor tag (e.g. `0x48` service descriptor, `0x09` CA descriptor).
    pub tag: u8,
    /// The descriptor payload (exactly `descriptor_length` bytes).
    pub data: &'a [u8],
}

/// A parsed descriptor loop: an in-order, bounded collection of [`Descriptor`]s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Descriptors<'a> {
    items: Vec<Descriptor<'a>>,
}

/// Upper bound on the number of descriptors parsed from a single loop.
///
/// A descriptor loop is itself length-bounded (≤ 4093 bytes) and each descriptor
/// is ≥ 2 bytes, so a loop can hold at most ~2046 descriptors; this cap is a
/// belt-and-braces guard so a crafted loop cannot force an unbounded `Vec`.
pub const MAX_DESCRIPTORS: usize = 4096;

impl<'a> Descriptors<'a> {
    /// Parse a descriptor loop occupying the whole of `bytes`.
    ///
    /// Walks `tag` + `length` + payload triples until `bytes` is exhausted.
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::Overrun`] when a descriptor's declared length runs past
    ///   the end of the loop.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, MpegTsError> {
        let mut items = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            if items.len() >= MAX_DESCRIPTORS {
                return Err(MpegTsError::Overrun {
                    declared: items.len(),
                    available: MAX_DESCRIPTORS,
                });
            }
            let tag = *bytes.get(offset).ok_or(MpegTsError::Overrun {
                declared: offset.saturating_add(1),
                available: bytes.len(),
            })?;
            let len_index = offset.checked_add(1).ok_or(MpegTsError::Overrun {
                declared: offset,
                available: bytes.len(),
            })?;
            let length = usize::from(*bytes.get(len_index).ok_or(MpegTsError::Overrun {
                declared: len_index.saturating_add(1),
                available: bytes.len(),
            })?);
            let data_start = len_index.checked_add(1).ok_or(MpegTsError::Overrun {
                declared: len_index,
                available: bytes.len(),
            })?;
            let data_end = data_start.checked_add(length).ok_or(MpegTsError::Overrun {
                declared: length,
                available: bytes.len(),
            })?;
            let data = bytes
                .get(data_start..data_end)
                .ok_or(MpegTsError::Overrun {
                    declared: data_end,
                    available: bytes.len(),
                })?;
            items.push(Descriptor { tag, data });
            offset = data_end;
        }
        Ok(Self { items })
    }

    /// The descriptors in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[Descriptor<'a>] {
        &self.items
    }

    /// The number of descriptors parsed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the loop carried no descriptors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The first descriptor with the given `tag`, if any.
    #[must_use]
    pub fn find(&self, tag: u8) -> Option<&Descriptor<'a>> {
        self.items.iter().find(|d| d.tag == tag)
    }

    /// Decode the **`ISO_639_language_descriptor`** (tag `0x0A`) carried in this
    /// loop, if present (ISO/IEC 13818-1 §2.6.18).
    ///
    /// Returns the per-entry ISO-639 language + [`AudioType`] role that an audio
    /// elementary stream carries — the language libav's container metadata
    /// frequently misses for an MPEG-TS PMT.
    #[must_use]
    pub fn iso_639_language(&self) -> Option<IsoLanguageDescriptor> {
        self.find(ISO_639_LANGUAGE_TAG)
            .map(|d| IsoLanguageDescriptor::decode(d.data))
    }

    /// Decode the DVB **`subtitling_descriptor`** (tag `0x59`) carried in this
    /// loop, if present (ETSI EN 300 468 §6.2.41).
    #[must_use]
    pub fn subtitling(&self) -> Option<SubtitlingDescriptor> {
        self.find(SUBTITLING_TAG)
            .map(|d| SubtitlingDescriptor::decode(d.data))
    }

    /// Decode the DVB **`teletext_descriptor`** (tag `0x56`) carried in this loop,
    /// if present (ETSI EN 300 468 §6.2.43 / VBI variant §6.2.47).
    #[must_use]
    pub fn teletext(&self) -> Option<TeletextDescriptor> {
        self.find(TELETEXT_TAG)
            .map(|d| TeletextDescriptor::decode(d.data))
    }

    /// Whether this loop carries a DVB **`AC-3_descriptor`** (`0x6A`) or an
    /// **`enhanced_AC-3_descriptor`** (`0x7A`) — the DVB signalling that a private
    /// elementary stream is a (E-)AC-3 audio track (ETSI EN 300 468 Annex D).
    #[must_use]
    pub fn has_ac3(&self) -> bool {
        self.find(AC3_TAG).is_some() || self.find(ENHANCED_AC3_TAG).is_some()
    }
}

/// Descriptor tag of the `ISO_639_language_descriptor` (ISO/IEC 13818-1 §2.6.18).
pub const ISO_639_LANGUAGE_TAG: u8 = 0x0A;
/// Descriptor tag of the DVB `teletext_descriptor` (ETSI EN 300 468 §6.2.43).
pub const TELETEXT_TAG: u8 = 0x56;
/// Descriptor tag of the DVB `subtitling_descriptor` (ETSI EN 300 468 §6.2.41).
pub const SUBTITLING_TAG: u8 = 0x59;
/// Descriptor tag of the DVB `AC-3_descriptor` (ETSI EN 300 468 Annex D).
pub const AC3_TAG: u8 = 0x6A;
/// Descriptor tag of the DVB `enhanced_AC-3_descriptor` (ETSI EN 300 468 Annex D).
pub const ENHANCED_AC3_TAG: u8 = 0x7A;

/// The `audio_type` role byte of an [`IsoLanguageEntry`] (ISO/IEC 13818-1
/// Table 2-60).
///
/// `#[non_exhaustive]` so further registered roles can be added without a
/// breaking change; an unrecognised value is preserved as [`AudioType::Reserved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioType {
    /// `0x00` — undefined (the normal main programme audio).
    Undefined,
    /// `0x01` — clean effects (music & effects, no dialogue).
    CleanEffects,
    /// `0x02` — hearing impaired (audio for the hard of hearing).
    HearingImpaired,
    /// `0x03` — visual impaired commentary (audio description).
    VisualImpairedCommentary,
    /// Any other / reserved value, preserving the raw byte.
    Reserved(u8),
}

impl AudioType {
    /// Decode an `audio_type` byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0x00 => Self::Undefined,
            0x01 => Self::CleanEffects,
            0x02 => Self::HearingImpaired,
            0x03 => Self::VisualImpairedCommentary,
            other => Self::Reserved(other),
        }
    }

    /// A short human-readable role label for the roles that carry one
    /// (hearing-impaired / visual-impaired / clean-effects), else [`None`] for
    /// the undefined / reserved main-programme case.
    ///
    /// Used to surface an accessibility role as a track title hint when the
    /// container declared none.
    #[must_use]
    pub const fn role_label(self) -> Option<&'static str> {
        match self {
            Self::CleanEffects => Some("clean effects"),
            Self::HearingImpaired => Some("hearing impaired"),
            Self::VisualImpairedCommentary => Some("visual impaired"),
            Self::Undefined | Self::Reserved(_) => None,
        }
    }
}

/// Upper bound on entries decoded from one repeating descriptor body, so a
/// crafted descriptor cannot force an unbounded `Vec`. A descriptor body is
/// length-bounded to 255 bytes and each entry here is ≥ 2 bytes.
const MAX_ENTRIES: usize = 128;

/// One `(language, audio_type)` entry of an [`IsoLanguageDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoLanguageEntry {
    /// The 3-letter ISO-639-2 language code (lossy-UTF-8 decoded, e.g. `"eng"`).
    pub language: String,
    /// The audio role of this language.
    pub audio_type: AudioType,
}

/// A decoded `ISO_639_language_descriptor`: a list of `(language, audio_type)`
/// entries (ISO/IEC 13818-1 §2.6.18).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IsoLanguageDescriptor {
    /// The per-language entries, in wire order.
    pub entries: Vec<IsoLanguageEntry>,
}

impl IsoLanguageDescriptor {
    /// Decode the descriptor body (4 bytes per entry: `lang(3) + audio_type(1)`).
    ///
    /// A trailing partial entry (fewer than 4 bytes) is ignored rather than
    /// erroring — the surrounding inventory fold-in must never fail a probe over
    /// a malformed descriptor.
    #[must_use]
    pub fn decode(body: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while let Some(chunk) = body.get(offset..offset.saturating_add(4)) {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            // `chunk` is exactly 4 bytes here.
            let language = lang_code(chunk.get(0..3).unwrap_or_default());
            let audio_type = AudioType::from_byte(*chunk.get(3).unwrap_or(&0));
            entries.push(IsoLanguageEntry {
                language,
                audio_type,
            });
            offset = offset.saturating_add(4);
        }
        Self { entries }
    }

    /// The first entry's language + role, the common single-entry case.
    #[must_use]
    pub fn first(&self) -> Option<&IsoLanguageEntry> {
        self.entries.first()
    }
}

/// One entry of a [`SubtitlingDescriptor`] (one DVB subtitle stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitlingEntry {
    /// The 3-letter ISO-639-2 language code (e.g. `"eng"`).
    pub language: String,
    /// The `subtitling_type` byte (ETSI EN 300 468 Table 26; e.g. `0x10` =
    /// DVB subtitles for a normal display, `0x20` = for the hard of hearing).
    pub subtitling_type: u8,
    /// The composition page id.
    pub composition_page_id: u16,
    /// The ancillary page id.
    pub ancillary_page_id: u16,
}

impl SubtitlingEntry {
    /// Whether the `subtitling_type` denotes a hard-of-hearing subtitle
    /// (`0x20..=0x25`, ETSI EN 300 468 Table 26).
    #[must_use]
    pub const fn is_hard_of_hearing(&self) -> bool {
        matches!(self.subtitling_type, 0x20..=0x25)
    }
}

/// A decoded DVB `subtitling_descriptor` (ETSI EN 300 468 §6.2.41).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitlingDescriptor {
    /// The per-subtitle entries, in wire order.
    pub entries: Vec<SubtitlingEntry>,
}

impl SubtitlingDescriptor {
    /// Decode the descriptor body (8 bytes per entry: `lang(3) + type(1) +
    /// composition_page(2) + ancillary_page(2)`). A trailing partial entry is
    /// ignored.
    #[must_use]
    pub fn decode(body: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while let Some(chunk) = body.get(offset..offset.saturating_add(8)) {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let language = lang_code(chunk.get(0..3).unwrap_or_default());
            let subtitling_type = *chunk.get(3).unwrap_or(&0);
            let composition_page_id = be_u16(chunk.get(4), chunk.get(5));
            let ancillary_page_id = be_u16(chunk.get(6), chunk.get(7));
            entries.push(SubtitlingEntry {
                language,
                subtitling_type,
                composition_page_id,
                ancillary_page_id,
            });
            offset = offset.saturating_add(8);
        }
        Self { entries }
    }

    /// The first subtitle entry, the common single-entry case.
    #[must_use]
    pub fn first(&self) -> Option<&SubtitlingEntry> {
        self.entries.first()
    }
}

/// One entry of a [`TeletextDescriptor`] (one teletext page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeletextEntry {
    /// The 3-letter ISO-639-2 language code (e.g. `"eng"`).
    pub language: String,
    /// The `teletext_type` (5 bits; e.g. `0x02` = teletext subtitle page).
    pub teletext_type: u8,
    /// The teletext magazine number (3 bits, `1..=8`, `0` meaning magazine 8).
    pub magazine_number: u8,
    /// The teletext page number (BCD-encoded on the wire; carried verbatim).
    pub page_number: u8,
}

impl TeletextEntry {
    /// Whether the `teletext_type` denotes a subtitle page (`0x02`) or a
    /// subtitle page for the hard of hearing (`0x05`) (ETSI EN 300 468 Table 100).
    #[must_use]
    pub const fn is_subtitle(&self) -> bool {
        matches!(self.teletext_type, 0x02 | 0x05)
    }
}

/// A decoded DVB `teletext_descriptor` (ETSI EN 300 468 §6.2.43).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeletextDescriptor {
    /// The per-page entries, in wire order.
    pub entries: Vec<TeletextEntry>,
}

impl TeletextDescriptor {
    /// Decode the descriptor body (5 bytes per entry: `lang(3) +
    /// type5|magazine3(1) + page(1)`). A trailing partial entry is ignored.
    #[must_use]
    pub fn decode(body: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while let Some(chunk) = body.get(offset..offset.saturating_add(5)) {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let language = lang_code(chunk.get(0..3).unwrap_or_default());
            let type_mag = *chunk.get(3).unwrap_or(&0);
            // teletext_type is the high 5 bits, magazine_number the low 3.
            let teletext_type = type_mag >> 3;
            let magazine_number = type_mag & 0x07;
            let page_number = *chunk.get(4).unwrap_or(&0);
            entries.push(TeletextEntry {
                language,
                teletext_type,
                magazine_number,
                page_number,
            });
            offset = offset.saturating_add(5);
        }
        Self { entries }
    }

    /// The first teletext entry, the common single-entry case.
    #[must_use]
    pub fn first(&self) -> Option<&TeletextEntry> {
        self.entries.first()
    }
}

/// Decode a 3-byte ISO-639 language code lossily to an owned `String`.
///
/// The bytes are ASCII letters in a well-formed descriptor; lossy UTF-8 decoding
/// keeps the function total (never panics) on a malformed code.
fn lang_code(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Assemble a big-endian `u16` from two optional bytes (a missing byte is `0`).
fn be_u16(hi: Option<&u8>, lo: Option<&u8>) -> u16 {
    (u16::from(*hi.unwrap_or(&0)) << 8) | u16::from(*lo.unwrap_or(&0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso639_decodes_multiple_entries_in_order() {
        // Two entries: eng/undefined then qaa/clean-effects.
        let body = b"eng\x00qaa\x01";
        let d = IsoLanguageDescriptor::decode(body);
        assert_eq!(d.entries.len(), 2);
        assert_eq!(d.entries[0].language, "eng");
        assert_eq!(d.entries[0].audio_type, AudioType::Undefined);
        assert_eq!(d.entries[1].language, "qaa");
        assert_eq!(d.entries[1].audio_type, AudioType::CleanEffects);
    }

    #[test]
    fn iso639_ignores_a_trailing_partial_entry() {
        // 4 valid bytes + 2 trailing bytes (a partial entry, < 4).
        let body = b"eng\x00xy";
        let d = IsoLanguageDescriptor::decode(body);
        assert_eq!(
            d.entries.len(),
            1,
            "the partial entry is dropped, not errored"
        );
        assert_eq!(d.entries[0].language, "eng");
    }

    #[test]
    fn iso639_empty_body_decodes_to_no_entries() {
        assert!(IsoLanguageDescriptor::decode(&[]).entries.is_empty());
    }

    #[test]
    fn audio_type_reserved_preserves_the_raw_byte() {
        assert_eq!(AudioType::from_byte(0x42), AudioType::Reserved(0x42));
        assert_eq!(AudioType::Reserved(0x42).role_label(), None);
        assert_eq!(AudioType::Undefined.role_label(), None);
        assert_eq!(
            AudioType::HearingImpaired.role_label(),
            Some("hearing impaired")
        );
    }

    #[test]
    fn subtitling_decodes_pages_and_hoh_flag() {
        // lang=deu, type=0x20 (HoH), comp=0x0102, anc=0x0304.
        let body = b"deu\x20\x01\x02\x03\x04";
        let d = SubtitlingDescriptor::decode(body);
        let e = d.first().expect("one entry");
        assert_eq!(e.language, "deu");
        assert_eq!(e.subtitling_type, 0x20);
        assert_eq!(e.composition_page_id, 0x0102);
        assert_eq!(e.ancillary_page_id, 0x0304);
        assert!(e.is_hard_of_hearing());
    }

    #[test]
    fn teletext_splits_type_and_magazine() {
        // type=0x05 (HoH subtitle), magazine=3 → (0x05<<3)|3 = 0x2B; page=0x77.
        let body = b"eng\x2B\x77";
        let d = TeletextDescriptor::decode(body);
        let e = d.first().expect("one entry");
        assert_eq!(e.teletext_type, 0x05);
        assert_eq!(e.magazine_number, 3);
        assert_eq!(e.page_number, 0x77);
        assert!(e.is_subtitle());
    }

    #[test]
    fn accessors_pull_descriptors_out_of_a_loop() {
        // Build a loop with an ISO-639 + AC-3 descriptor.
        let mut loop_bytes = vec![ISO_639_LANGUAGE_TAG, 4, b'e', b'n', b'g', 0x00];
        loop_bytes.extend_from_slice(&[AC3_TAG, 0]);
        let descs = Descriptors::parse(&loop_bytes).expect("parse");
        assert!(descs.iso_639_language().is_some());
        assert!(descs.has_ac3());
        assert!(descs.subtitling().is_none());
        assert!(descs.teletext().is_none());
    }

    #[test]
    fn enhanced_ac3_descriptor_is_detected_as_ac3() {
        let descs = Descriptors::parse(&[ENHANCED_AC3_TAG, 0]).expect("parse");
        assert!(
            descs.has_ac3(),
            "E-AC-3 descriptor counts as AC-3 signalling"
        );
    }
}
