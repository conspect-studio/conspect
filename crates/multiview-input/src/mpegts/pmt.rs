//! Program Map Table parser (ISO/IEC 13818-1 §2.4.4.8).
//!
//! A PMT (`table_id` `0x02`, on the PID the [`super::pat`] assigned to its
//! program) describes one program: the PID carrying the Program Clock Reference,
//! a program-level descriptor loop, and one entry per elementary stream
//! (video / audio / data) with its [`StreamType`], PID, and per-stream
//! descriptor loop.

use super::descriptor::Descriptors;
use super::section::SectionHeader;
use super::MpegTsError;

/// The `table_id` of a Program Map Table.
pub const TABLE_ID: u8 = 0x02;

/// The fixed PMT body fields before the program-info descriptor loop
/// (PCR PID word + `program_info_length` word).
const PMT_FIXED_LEN: usize = 4;

/// The fixed per-elementary-stream header (`stream_type` + PID word +
/// `ES_info_length`
/// word) before each stream's descriptor loop.
const ES_HEADER_LEN: usize = 5;

/// A PID value meaning "no PCR for this program" (all-ones, ISO/IEC 13818-1).
pub const NO_PCR_PID: u16 = 0x1FFF;

/// Well-known elementary `stream_type` values (ISO/IEC 13818-1 Table 2-34 +
/// registered amendments). Unrecognised values preserve the raw byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamType {
    /// ISO/IEC 11172-2 (MPEG-1) video.
    Mpeg1Video,
    /// ISO/IEC 13818-2 (MPEG-2) video.
    Mpeg2Video,
    /// ISO/IEC 11172-3 (MPEG-1) audio.
    Mpeg1Audio,
    /// ISO/IEC 13818-3 (MPEG-2) audio.
    Mpeg2Audio,
    /// ISO/IEC 13818-1 private sections.
    PrivateSections,
    /// ISO/IEC 13818-1 PES-carried private data.
    PrivatePes,
    /// ISO/IEC 13818-7 ADTS AAC audio.
    AdtsAac,
    /// ISO/IEC 14496-3 LATM AAC audio.
    LatmAac,
    /// ISO/IEC 14496-2 (MPEG-4 part 2) video.
    Mpeg4Video,
    /// ITU-T H.264 / ISO/IEC 14496-10 AVC video.
    H264,
    /// ITU-T H.265 / ISO/IEC 23008-2 HEVC video.
    Hevc,
    /// ITU-T H.266 / VVC video.
    Vvc,
    /// SMPTE 302M PCM audio.
    Smpte302mAudio,
    /// Dolby AC-3 audio (ATSC).
    Ac3,
    /// Dolby E-AC-3 (Enhanced AC-3) audio.
    EAc3,
    /// SCTE-35 splice-information sections.
    Scte35,
    /// Any other / unrecognised stream type, preserving the raw byte.
    Other(u8),
}

impl StreamType {
    /// Decode a `stream_type` byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0x01 => Self::Mpeg1Video,
            0x02 => Self::Mpeg2Video,
            0x03 => Self::Mpeg1Audio,
            0x04 => Self::Mpeg2Audio,
            0x05 => Self::PrivateSections,
            0x06 => Self::PrivatePes,
            0x0F => Self::AdtsAac,
            0x10 => Self::Mpeg4Video,
            0x11 => Self::LatmAac,
            0x1B => Self::H264,
            0x24 => Self::Hevc,
            0x33 => Self::Vvc,
            0x81 => Self::Ac3,
            0x82 => Self::Smpte302mAudio,
            0x86 => Self::Scte35,
            0x87 => Self::EAc3,
            other => Self::Other(other),
        }
    }

    /// The raw `stream_type` byte.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Mpeg1Video => 0x01,
            Self::Mpeg2Video => 0x02,
            Self::Mpeg1Audio => 0x03,
            Self::Mpeg2Audio => 0x04,
            Self::PrivateSections => 0x05,
            Self::PrivatePes => 0x06,
            Self::AdtsAac => 0x0F,
            Self::Mpeg4Video => 0x10,
            Self::LatmAac => 0x11,
            Self::H264 => 0x1B,
            Self::Hevc => 0x24,
            Self::Vvc => 0x33,
            Self::Ac3 => 0x81,
            Self::Smpte302mAudio => 0x82,
            Self::EAc3 => 0x87,
            Self::Scte35 => 0x86,
            Self::Other(b) => b,
        }
    }

    /// Whether this stream type denotes a video essence.
    #[must_use]
    pub const fn is_video(self) -> bool {
        matches!(
            self,
            Self::Mpeg1Video
                | Self::Mpeg2Video
                | Self::Mpeg4Video
                | Self::H264
                | Self::Hevc
                | Self::Vvc
        )
    }

    /// Whether this stream type denotes an audio essence.
    #[must_use]
    pub const fn is_audio(self) -> bool {
        matches!(
            self,
            Self::Mpeg1Audio
                | Self::Mpeg2Audio
                | Self::AdtsAac
                | Self::LatmAac
                | Self::Ac3
                | Self::EAc3
                | Self::Smpte302mAudio
        )
    }
}

/// One elementary stream of a program: its type, PID, and the raw bytes of its
/// per-stream descriptor loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementaryStream {
    /// The stream type.
    pub stream_type: StreamType,
    /// The PID carrying this elementary stream's packets.
    pub pid: u16,
    /// The raw ES-info descriptor-loop bytes (parse with [`Descriptors::parse`]).
    pub descriptors: Vec<u8>,
}

impl ElementaryStream {
    /// Parse this stream's ES-info descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.descriptors)
    }
}

/// A parsed Program Map Table for one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pmt {
    /// The program number (carried in the section's `table_id_extension`).
    pub program_number: u16,
    /// The table version.
    pub version: u8,
    /// `current_next_indicator`.
    pub current: bool,
    /// The PID carrying the Program Clock Reference (or [`NO_PCR_PID`]).
    pub pcr_pid: u16,
    /// The raw program-info descriptor-loop bytes.
    pub program_info: Vec<u8>,
    /// The elementary streams of this program, in wire order.
    pub streams: Vec<ElementaryStream>,
}

impl Pmt {
    /// Parse a PMT from a complete PSI section (header + body + CRC).
    ///
    /// # Errors
    ///
    /// * Any [`MpegTsError`] from header / CRC validation.
    /// * [`MpegTsError::Overrun`] when a length field runs past the section, or
    ///   an elementary-stream loop is malformed.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        let parsed = SectionHeader::parse(section, TABLE_ID)?;
        let body = parsed.body;
        if body.len() < PMT_FIXED_LEN {
            return Err(MpegTsError::TooShort {
                need: PMT_FIXED_LEN,
                got: body.len(),
            });
        }
        let pcr_hi = *body.first().ok_or(short(0))?;
        let pcr_lo = *body.get(1).ok_or(short(1))?;
        let pcr_pid = (u16::from(pcr_hi & 0b0001_1111) << 8) | u16::from(pcr_lo);

        let prog_info_hi = *body.get(2).ok_or(short(2))?;
        let prog_info_lo = *body.get(3).ok_or(short(3))?;
        // Top 4 bits reserved + top 2 bits of length are '00'.
        let program_info_length =
            (usize::from(prog_info_hi & 0b0000_1111) << 8) | usize::from(prog_info_lo);

        let pi_start = PMT_FIXED_LEN;
        let pi_end = pi_start
            .checked_add(program_info_length)
            .ok_or(overrun(program_info_length, body.len()))?;
        let program_info = body
            .get(pi_start..pi_end)
            .ok_or(overrun(pi_end, body.len()))?
            .to_vec();

        let mut streams = Vec::new();
        let mut offset = pi_end;
        while offset < body.len() {
            let st = *body.get(offset).ok_or(overrun(offset, body.len()))?;
            let pid_hi = *body
                .get(offset.saturating_add(1))
                .ok_or(overrun(offset, body.len()))?;
            let pid_lo = *body
                .get(offset.saturating_add(2))
                .ok_or(overrun(offset, body.len()))?;
            let pid = (u16::from(pid_hi & 0b0001_1111) << 8) | u16::from(pid_lo);
            let esil_hi = *body
                .get(offset.saturating_add(3))
                .ok_or(overrun(offset, body.len()))?;
            let esil_lo = *body
                .get(offset.saturating_add(4))
                .ok_or(overrun(offset, body.len()))?;
            let es_info_length = (usize::from(esil_hi & 0b0000_1111) << 8) | usize::from(esil_lo);

            let desc_start = offset
                .checked_add(ES_HEADER_LEN)
                .ok_or(overrun(offset, body.len()))?;
            let desc_end = desc_start
                .checked_add(es_info_length)
                .ok_or(overrun(es_info_length, body.len()))?;
            let descriptors = body
                .get(desc_start..desc_end)
                .ok_or(overrun(desc_end, body.len()))?
                .to_vec();

            streams.push(ElementaryStream {
                stream_type: StreamType::from_byte(st),
                pid,
                descriptors,
            });
            offset = desc_end;
        }

        Ok(Self {
            program_number: parsed.header.table_id_extension,
            version: parsed.header.version,
            current: parsed.header.current,
            pcr_pid,
            program_info,
            streams,
        })
    }

    /// Parse the program-level descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn program_descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.program_info)
    }

    /// The first video elementary stream, if the program carries one.
    #[must_use]
    pub fn video_stream(&self) -> Option<&ElementaryStream> {
        self.streams.iter().find(|s| s.stream_type.is_video())
    }

    /// All audio elementary streams of this program.
    #[must_use]
    pub fn audio_streams(&self) -> Vec<&ElementaryStream> {
        self.streams
            .iter()
            .filter(|s| s.stream_type.is_audio())
            .collect()
    }

    /// The PID(s) carrying SCTE-35 splice information for this program, if any.
    #[must_use]
    pub fn scte35_pids(&self) -> Vec<u16> {
        self.streams
            .iter()
            .filter(|s| matches!(s.stream_type, StreamType::Scte35))
            .map(|s| s.pid)
            .collect()
    }
}

/// Build a `TooShort` error for a PMT body offset.
const fn short(offset: usize) -> MpegTsError {
    MpegTsError::TooShort {
        need: offset.saturating_add(1),
        got: offset,
    }
}

/// Build an `Overrun` error.
const fn overrun(declared: usize, available: usize) -> MpegTsError {
    MpegTsError::Overrun {
        declared,
        available,
    }
}
