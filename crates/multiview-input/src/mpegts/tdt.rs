//! Time and Date Table parser (DVB, ETSI EN 300 468 §5.2.5).
//!
//! The TDT (`table_id` `0x70`, on PID `0x0014`) is a **short-form** section: no
//! syntax indicator, no version, no CRC — just a 5-byte UTC field carrying the
//! network's current time as a Modified Julian Date (16-bit) plus a BCD
//! hours/minutes/seconds triple (ETSI EN 300 468 Annex C).

use super::MpegTsError;

/// The `table_id` of a Time and Date Table.
pub const TABLE_ID: u8 = 0x70;

/// The fixed total length of a TDT section (header 3 + 5-byte UTC field).
pub const SECTION_LEN: usize = 8;

/// The decoded broadcast UTC time carried by a TDT (or the time portion of a
/// [`super::tot`]).
///
/// Stored as the broken-down calendar fields the DVB clock carries; the
/// [`DvbTime::to_unix_seconds`] helper converts to a Unix timestamp for the
/// internal clock model. The fields are validated to be in range at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DvbTime {
    /// The Modified Julian Date (days since 1858-11-17).
    pub mjd: u16,
    /// Hours `0..=23` (decoded from BCD).
    pub hours: u8,
    /// Minutes `0..=59` (decoded from BCD).
    pub minutes: u8,
    /// Seconds `0..=59` (decoded from BCD).
    pub seconds: u8,
}

impl DvbTime {
    /// Decode a 5-byte UTC field (2-byte MJD + 3-byte BCD time).
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::TooShort`] when fewer than five bytes are supplied.
    /// * [`MpegTsError::BadDateTime`] when a BCD nibble is not a decimal digit or
    ///   the decoded time is out of range.
    pub fn parse(bytes: &[u8]) -> Result<Self, MpegTsError> {
        let mjd_hi = *bytes.first().ok_or(MpegTsError::TooShort {
            need: 5,
            got: bytes.len(),
        })?;
        let mjd_lo = *bytes.get(1).ok_or(MpegTsError::TooShort {
            need: 5,
            got: bytes.len(),
        })?;
        let mjd = (u16::from(mjd_hi) << 8) | u16::from(mjd_lo);
        let hours = bcd(*bytes.get(2).ok_or(MpegTsError::TooShort {
            need: 5,
            got: bytes.len(),
        })?)?;
        let minutes = bcd(*bytes.get(3).ok_or(MpegTsError::TooShort {
            need: 5,
            got: bytes.len(),
        })?)?;
        let seconds = bcd(*bytes.get(4).ok_or(MpegTsError::TooShort {
            need: 5,
            got: bytes.len(),
        })?)?;
        if hours > 23 {
            return Err(MpegTsError::BadDateTime("hours > 23"));
        }
        if minutes > 59 {
            return Err(MpegTsError::BadDateTime("minutes > 59"));
        }
        if seconds > 59 {
            return Err(MpegTsError::BadDateTime("seconds > 59"));
        }
        Ok(Self {
            mjd,
            hours,
            minutes,
            seconds,
        })
    }

    /// Convert to a Unix timestamp (seconds since 1970-01-01T00:00:00Z).
    ///
    /// The MJD epoch (1858-11-17) is `40587` days before the Unix epoch, so the
    /// Unix day count is `mjd - 40587`. All arithmetic is checked.
    ///
    /// # Errors
    ///
    /// [`MpegTsError::BadDateTime`] when the MJD predates the Unix epoch (the
    /// resulting timestamp would be negative, which Multiview's clock model rejects)
    /// or the arithmetic would overflow.
    pub fn to_unix_seconds(self) -> Result<i64, MpegTsError> {
        const MJD_UNIX_EPOCH: i64 = 40587;
        const SECONDS_PER_DAY: i64 = 86_400;
        let days = i64::from(self.mjd)
            .checked_sub(MJD_UNIX_EPOCH)
            .ok_or(MpegTsError::BadDateTime("mjd arithmetic overflow"))?;
        if days < 0 {
            return Err(MpegTsError::BadDateTime("mjd predates the unix epoch"));
        }
        let day_seconds = days
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(MpegTsError::BadDateTime("mjd*86400 overflow"))?;
        // hours/minutes/seconds are validated in range, so the time-of-day is at
        // most 86_399; compute it in plain i64 arithmetic (cannot overflow).
        let tod = (i64::from(self.hours) * 3600)
            + (i64::from(self.minutes) * 60)
            + i64::from(self.seconds);
        day_seconds
            .checked_add(tod)
            .ok_or(MpegTsError::BadDateTime("unix-seconds overflow"))
    }
}

/// A parsed Time and Date Table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tdt {
    /// The decoded network UTC time.
    pub utc: DvbTime,
}

impl Tdt {
    /// Parse a TDT from a complete short-form section.
    ///
    /// Validates the `table_id` and the short-form framing (syntax indicator
    /// clear, declared length `0x0005`), then decodes the UTC field.
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::WrongTable`] when the `table_id` is not `0x70`.
    /// * [`MpegTsError::TooShort`] when the section is under eight bytes.
    /// * [`MpegTsError::Syntax`] when the short-form framing is wrong.
    /// * [`MpegTsError::BadDateTime`] from UTC decoding.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        let utc = parse_short_form(section, TABLE_ID)?;
        Ok(Self { utc })
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
    // hi*10 + lo is at most 99, fits a u8.
    Ok(hi.saturating_mul(10).saturating_add(lo))
}

/// Validate a short-form (TDT/TOT) section header and return the 5-byte UTC
/// field decoded into a [`DvbTime`]. Shared by TDT and TOT (TOT carries
/// additional descriptors after the UTC field).
pub(super) fn parse_short_form(section: &[u8], expected: u8) -> Result<DvbTime, MpegTsError> {
    if section.len() < SECTION_LEN {
        return Err(MpegTsError::TooShort {
            need: SECTION_LEN,
            got: section.len(),
        });
    }
    let table_id = *section.first().ok_or(MpegTsError::TooShort {
        need: SECTION_LEN,
        got: section.len(),
    })?;
    if table_id != expected {
        return Err(MpegTsError::WrongTable {
            expected,
            got: table_id,
        });
    }
    let b1 = *section.get(1).ok_or(MpegTsError::TooShort {
        need: SECTION_LEN,
        got: section.len(),
    })?;
    // Short form: section_syntax_indicator == 0.
    if (b1 & 0b1000_0000) != 0 {
        return Err(MpegTsError::Syntax(
            "section_syntax_indicator set on a short-form (TDT/TOT) section",
        ));
    }
    let utc_bytes = section.get(3..SECTION_LEN).ok_or(MpegTsError::TooShort {
        need: SECTION_LEN,
        got: section.len(),
    })?;
    DvbTime::parse(utc_bytes)
}
