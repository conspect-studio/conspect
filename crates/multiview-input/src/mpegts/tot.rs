//! Time Offset Table parser (DVB, ETSI EN 300 468 §5.2.6).
//!
//! The TOT (`table_id` `0x73`, on PID `0x0014`) is a short-form section like the
//! [`super::tdt`] but extends it: after the 5-byte UTC field it carries a
//! descriptor loop (length-prefixed) whose `local_time_offset_descriptor`
//! (`tag 0x58`) gives the country's offset from UTC and the next
//! daylight-saving change. The TOT *does* carry a trailing CRC-32.

use super::descriptor::Descriptors;
use super::tdt::DvbTime;
use super::MpegTsError;

/// The `table_id` of a Time Offset Table.
pub const TABLE_ID: u8 = 0x73;

/// The descriptor tag of a `local_time_offset_descriptor` (`0x58`).
pub const LOCAL_TIME_OFFSET_TAG: u8 = 0x58;

/// The fixed bytes of a TOT before its descriptor loop: header (3) + UTC (5) +
/// `descriptors_loop_length` word (2).
const TOT_PREFIX_LEN: usize = 10;

/// A parsed Time Offset Table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tot {
    /// The decoded network UTC time.
    pub utc: DvbTime,
    /// The raw descriptor-loop bytes (local-time-offset etc.).
    pub descriptors: Vec<u8>,
}

impl Tot {
    /// Parse a TOT from a complete short-form section (UTC + descriptor loop +
    /// CRC).
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::WrongTable`] when the `table_id` is not `0x73`.
    /// * [`MpegTsError::TooShort`] when the section is too small to hold the
    ///   header, UTC field, and descriptors-loop-length word.
    /// * [`MpegTsError::Syntax`] / [`MpegTsError::Crc`] from framing / CRC
    ///   validation.
    /// * [`MpegTsError::Overrun`] when the descriptor-loop length runs past the
    ///   section.
    /// * [`MpegTsError::BadDateTime`] from UTC decoding.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        if section.len() < TOT_PREFIX_LEN {
            return Err(MpegTsError::TooShort {
                need: TOT_PREFIX_LEN,
                got: section.len(),
            });
        }
        let table_id = *section.first().ok_or(MpegTsError::TooShort {
            need: TOT_PREFIX_LEN,
            got: section.len(),
        })?;
        if table_id != TABLE_ID {
            return Err(MpegTsError::WrongTable {
                expected: TABLE_ID,
                got: table_id,
            });
        }
        let b1 = *section.get(1).ok_or(MpegTsError::TooShort {
            need: TOT_PREFIX_LEN,
            got: section.len(),
        })?;
        if (b1 & 0b1000_0000) != 0 {
            return Err(MpegTsError::Syntax(
                "section_syntax_indicator set on a TOT (short-form) section",
            ));
        }
        let b2 = *section.get(2).ok_or(MpegTsError::TooShort {
            need: TOT_PREFIX_LEN,
            got: section.len(),
        })?;
        let section_length = (usize::from(b1 & 0b0000_1111) << 8) | usize::from(b2);
        let total = 3usize
            .checked_add(section_length)
            .ok_or(MpegTsError::Overrun {
                declared: section_length,
                available: section.len(),
            })?;
        let full = section.get(..total).ok_or(MpegTsError::TooShort {
            need: total,
            got: section.len(),
        })?;
        // The TOT carries a CRC over the whole section.
        super::crc::validate(full)?;

        let utc_bytes = full.get(3..8).ok_or(MpegTsError::TooShort {
            need: 8,
            got: full.len(),
        })?;
        let utc = DvbTime::parse(utc_bytes)?;

        let dll_hi = *full.get(8).ok_or(MpegTsError::TooShort {
            need: 9,
            got: full.len(),
        })?;
        let dll_lo = *full.get(9).ok_or(MpegTsError::TooShort {
            need: 10,
            got: full.len(),
        })?;
        let descriptors_loop_length =
            (usize::from(dll_hi & 0b0000_1111) << 8) | usize::from(dll_lo);

        let d_start = TOT_PREFIX_LEN;
        let d_end = d_start
            .checked_add(descriptors_loop_length)
            .ok_or(MpegTsError::Overrun {
                declared: descriptors_loop_length,
                available: full.len(),
            })?;
        let descriptors = full
            .get(d_start..d_end)
            .ok_or(MpegTsError::Overrun {
                declared: d_end,
                available: full.len(),
            })?
            .to_vec();

        Ok(Self { utc, descriptors })
    }

    /// Parse the descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.descriptors)
    }

    /// The local-time-offset in minutes east of UTC from the first
    /// `local_time_offset_descriptor`, if present. The offset is BCD `HHMM` with
    /// a polarity bit (`1` = west/negative).
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn local_offset_minutes(&self) -> Result<Option<i16>, MpegTsError> {
        let descriptors = self.descriptors()?;
        let Some(desc) = descriptors.find(LOCAL_TIME_OFFSET_TAG) else {
            return Ok(None);
        };
        // country_code(3), country_region_id(6)+reserved(1)+polarity(1),
        // local_time_offset(2 BCD HHMM), ...
        let data = desc.data;
        let region_byte = *data.get(3).ok_or(MpegTsError::Overrun {
            declared: 4,
            available: data.len(),
        })?;
        let polarity_west = (region_byte & 0b0000_0001) != 0;
        let off_hi = *data.get(4).ok_or(MpegTsError::Overrun {
            declared: 5,
            available: data.len(),
        })?;
        let off_lo = *data.get(5).ok_or(MpegTsError::Overrun {
            declared: 6,
            available: data.len(),
        })?;
        let hours = bcd(off_hi)?;
        let minutes = bcd(off_lo)?;
        if minutes > 59 {
            return Err(MpegTsError::BadDateTime("local offset minutes > 59"));
        }
        let magnitude = i16::from(hours)
            .checked_mul(60)
            .and_then(|h| h.checked_add(i16::from(minutes)))
            .ok_or(MpegTsError::BadDateTime("local offset overflow"))?;
        Ok(Some(if polarity_west { -magnitude } else { magnitude }))
    }
}

/// Decode a BCD-encoded byte (two decimal digits) into a `u8`.
fn bcd(byte: u8) -> Result<u8, MpegTsError> {
    let hi = byte >> 4;
    let lo = byte & 0x0F;
    if hi > 9 || lo > 9 {
        return Err(MpegTsError::BadDateTime(
            "bcd nibble is not a decimal digit",
        ));
    }
    Ok(hi.saturating_mul(10).saturating_add(lo))
}
