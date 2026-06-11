//! The common **PSI section** header (ISO/IEC 13818-1 §2.4.4.11) every long-form
//! table rides over, plus the [`TableId`] enumeration.
//!
//! A "long" (syntax-indicator-set) PSI section is laid out as:
//!
//! ```text
//!  table_id                     8  bits
//!  section_syntax_indicator     1  bit   (== 1 for long form)
//!  '0' + 2 reserved bits        3  bits
//!  section_length              12  bits  (bytes following this field)
//!  -- the next bytes are covered by section_length and the CRC --
//!  table_id_extension          16  bits  (TSID / program_number / service id…)
//!  reserved (2) + version (5) + current_next_indicator (1)   8 bits
//!  section_number               8  bits
//!  last_section_number          8  bits
//!  ... table body ...
//!  CRC_32                      32  bits
//! ```
//!
//! Short-form sections (TDT, TOT) clear the syntax indicator and carry no
//! version / section-number fields; those tables parse their own minimal header.

use super::MpegTsError;

/// The fixed length of the part of a long-form PSI header *before* the bytes
/// counted by `section_length` (`table_id` + the syntax/length word).
pub const HEADER_PREFIX_LEN: usize = 3;

/// The length of the long-form fields between `section_length` and the table
/// body (`table_id_extension` + version word + section numbers).
pub const LONG_HEADER_LEN: usize = 5;

/// The length of the trailing CRC-32.
pub const CRC_LEN: usize = 4;

/// The maximum value `section_length` may take (12-bit field, top two bits must
/// be zero per the spec, so the practical cap is `0x03FD` bytes following).
pub const MAX_SECTION_LENGTH: usize = 0x0FFF;

/// Well-known PSI/SI table identifiers (ISO/IEC 13818-1 + ETSI EN 300 468).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableId {
    /// Program Association Table (`0x00`).
    ProgramAssociation,
    /// Conditional Access Table (`0x01`).
    ConditionalAccess,
    /// Program Map Table (`0x02`).
    ProgramMap,
    /// Network Information Table — actual network (`0x40`).
    NetworkInformationActual,
    /// Network Information Table — other network (`0x41`).
    NetworkInformationOther,
    /// Service Description Table — actual transport stream (`0x42`).
    ServiceDescriptionActual,
    /// Service Description Table — other transport stream (`0x46`).
    ServiceDescriptionOther,
    /// Time and Date Table (`0x70`).
    TimeDate,
    /// Time Offset Table (`0x73`).
    TimeOffset,
    /// Splice Information Table — SCTE-35 (`0xFC`).
    SpliceInfo,
    /// Any other / unrecognised table id, preserving the raw byte.
    Other(u8),
}

impl TableId {
    /// Decode a `table_id` byte into a [`TableId`].
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0x00 => Self::ProgramAssociation,
            0x01 => Self::ConditionalAccess,
            0x02 => Self::ProgramMap,
            0x40 => Self::NetworkInformationActual,
            0x41 => Self::NetworkInformationOther,
            0x42 => Self::ServiceDescriptionActual,
            0x46 => Self::ServiceDescriptionOther,
            0x70 => Self::TimeDate,
            0x73 => Self::TimeOffset,
            0xFC => Self::SpliceInfo,
            other => Self::Other(other),
        }
    }

    /// The raw `table_id` byte for this id.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::ProgramAssociation => 0x00,
            Self::ConditionalAccess => 0x01,
            Self::ProgramMap => 0x02,
            Self::NetworkInformationActual => 0x40,
            Self::NetworkInformationOther => 0x41,
            Self::ServiceDescriptionActual => 0x42,
            Self::ServiceDescriptionOther => 0x46,
            Self::TimeDate => 0x70,
            Self::TimeOffset => 0x73,
            Self::SpliceInfo => 0xFC,
            Self::Other(b) => b,
        }
    }
}

/// A parsed long-form PSI section header.
///
/// Built by [`SectionHeader::parse`], which also validates the trailing CRC. The
/// returned `body` slice is exactly the table-specific payload between the long
/// header and the CRC, so each table parser can decode just its own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionHeader {
    /// The decoded `table_id`.
    pub table_id: TableId,
    /// The 16-bit `table_id_extension` (meaning is table-specific: TSID for a
    /// PAT, `program_number` for a PMT, etc.).
    pub table_id_extension: u16,
    /// The 5-bit version number of this table instance.
    pub version: u8,
    /// `current_next_indicator`: `true` when this table is currently applicable.
    pub current: bool,
    /// This section's number within the table.
    pub section_number: u8,
    /// The last section number of the table (for multi-section tables).
    pub last_section_number: u8,
}

/// The result of parsing a section header: the header plus a borrowed slice of
/// the table-specific body (CRC already stripped and validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSection<'a> {
    /// The decoded common header.
    pub header: SectionHeader,
    /// The table-specific body bytes (between the long header and the CRC).
    pub body: &'a [u8],
}

impl SectionHeader {
    /// Parse and CRC-validate a long-form PSI section.
    ///
    /// `expected` is the `table_id` byte the caller's table requires; a mismatch
    /// returns [`MpegTsError::WrongTable`]. The CRC is validated over the section
    /// as bounded by `section_length`.
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::TooShort`] when the buffer cannot hold the declared
    ///   section.
    /// * [`MpegTsError::Syntax`] when the `section_syntax_indicator` is unset or a
    ///   fixed bit is wrong.
    /// * [`MpegTsError::WrongTable`] when the `table_id` is not `expected`.
    /// * [`MpegTsError::Crc`] when the trailing CRC does not validate.
    pub fn parse(bytes: &[u8], expected: u8) -> Result<ParsedSection<'_>, MpegTsError> {
        if bytes.len() < HEADER_PREFIX_LEN {
            return Err(MpegTsError::TooShort {
                need: HEADER_PREFIX_LEN,
                got: bytes.len(),
            });
        }
        let table_id_byte = *bytes.first().ok_or(MpegTsError::TooShort {
            need: HEADER_PREFIX_LEN,
            got: bytes.len(),
        })?;
        if table_id_byte != expected {
            return Err(MpegTsError::WrongTable {
                expected,
                got: table_id_byte,
            });
        }

        let b1 = *bytes.get(1).ok_or(MpegTsError::TooShort {
            need: HEADER_PREFIX_LEN,
            got: bytes.len(),
        })?;
        let b2 = *bytes.get(2).ok_or(MpegTsError::TooShort {
            need: HEADER_PREFIX_LEN,
            got: bytes.len(),
        })?;

        let syntax_indicator = (b1 & 0b1000_0000) != 0;
        if !syntax_indicator {
            return Err(MpegTsError::Syntax(
                "section_syntax_indicator is 0 (short-form section parsed as long-form)",
            ));
        }
        // Bit 6 ('0') is private/reserved; for PSI tables it must be 0.
        if (b1 & 0b0100_0000) != 0 {
            return Err(MpegTsError::Syntax(
                "private_indicator/'0' bit set on a PSI section",
            ));
        }

        let section_length = (usize::from(b1 & 0b0000_1111) << 8) | usize::from(b2);
        if section_length > MAX_SECTION_LENGTH {
            return Err(MpegTsError::Syntax("section_length exceeds 0x0FFF"));
        }

        // Total section size = prefix (3) + the bytes counted by section_length.
        let total = HEADER_PREFIX_LEN
            .checked_add(section_length)
            .ok_or(MpegTsError::Overrun {
                declared: section_length,
                available: bytes.len(),
            })?;
        let section = bytes.get(..total).ok_or(MpegTsError::TooShort {
            need: total,
            got: bytes.len(),
        })?;

        // The CRC covers the whole section; validate before trusting any field.
        super::crc::validate(section)?;

        // After the prefix come the long-form header fields, then the body, then
        // the 4-byte CRC. `section_length` counts everything after the prefix
        // (long header + body + CRC).
        if section_length < LONG_HEADER_LEN.saturating_add(CRC_LEN) {
            return Err(MpegTsError::TooShort {
                need: LONG_HEADER_LEN.saturating_add(CRC_LEN),
                got: section_length,
            });
        }

        let tide_hi = *section.get(3).ok_or(MpegTsError::TooShort {
            need: 4,
            got: section.len(),
        })?;
        let tide_lo = *section.get(4).ok_or(MpegTsError::TooShort {
            need: 5,
            got: section.len(),
        })?;
        let table_id_extension = (u16::from(tide_hi) << 8) | u16::from(tide_lo);

        let version_byte = *section.get(5).ok_or(MpegTsError::TooShort {
            need: 6,
            got: section.len(),
        })?;
        let version = (version_byte >> 1) & 0b0001_1111;
        let current = (version_byte & 0b0000_0001) != 0;

        let section_number = *section.get(6).ok_or(MpegTsError::TooShort {
            need: 7,
            got: section.len(),
        })?;
        let last_section_number = *section.get(7).ok_or(MpegTsError::TooShort {
            need: 8,
            got: section.len(),
        })?;

        // Body sits between the long header (prefix + 5) and the CRC (last 4).
        let body_start = HEADER_PREFIX_LEN.saturating_add(LONG_HEADER_LEN);
        let body_end = total.saturating_sub(CRC_LEN);
        let body = section
            .get(body_start..body_end)
            .ok_or(MpegTsError::Overrun {
                declared: body_start,
                available: total,
            })?;

        Ok(ParsedSection {
            header: SectionHeader {
                table_id: TableId::from_byte(table_id_byte),
                table_id_extension,
                version,
                current,
                section_number,
                last_section_number,
            },
            body,
        })
    }
}
