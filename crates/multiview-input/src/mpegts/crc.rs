//! CRC-32/MPEG-2 — the integrity check that closes every MPEG-2 PSI/SI section
//! (ISO/IEC 13818-1 Annex A).
//!
//! The PSI CRC is **CRC-32/MPEG-2**: polynomial `0x04C1_1DB7`, MSB-first, an
//! initial register of `0xFFFF_FFFF`, **no** input/output reflection and **no**
//! final XOR. It is computed over the entire section *including* the table id and
//! section-length header, but *excluding* the trailing four CRC bytes, and the
//! result is appended big-endian. A correct section satisfies the property that
//! running the CRC over the whole section (CRC bytes included) yields `0`.
//!
//! This is a pure, allocation-free, `const`-table implementation that never
//! panics.

/// The CRC-32/MPEG-2 generator polynomial (normal, MSB-first representation).
pub const POLYNOMIAL: u32 = 0x04C1_1DB7;

/// The initial CRC register value mandated by ISO/IEC 13818-1 Annex A.
pub const INIT: u32 = 0xFFFF_FFFF;

/// Precomputed 256-entry lookup table for the byte-wise MSB-first CRC.
const TABLE: [u32; 256] = build_table();

/// Build the 256-entry CRC table at compile time (MSB-first, polynomial
/// [`POLYNOMIAL`]).
// reason: `TryFrom`/`try_into` are not `const`-stable on this toolchain, so a
// const table builder cannot use the checked conversions the lint prefers. Every
// cast here is provably lossless: `i` is bounded `0..256`, so `i as u32` widens
// without loss and `table[i]` indexes within the 256-entry array (also `i`-bound).
// No runtime input touches this function — it runs at compile time.
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i: usize = 0;
    while i < 256 {
        // Seed the register with the byte in the top 8 bits (`i` < 256).
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            // MSB-first: shift up, conditionally XOR the polynomial when the bit
            // that fell off the top was set.
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ POLYNOMIAL
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Compute the CRC-32/MPEG-2 over `bytes`, starting from [`INIT`].
///
/// Returns the register without any final XOR (per the MPEG-2 variant). To
/// validate a complete section, compute over the *entire* section (including the
/// trailing four CRC bytes) and check the result is `0`; to generate the CRC for
/// a section body, compute over the body (excluding the CRC bytes) and append the
/// result big-endian.
#[must_use]
pub fn crc32_mpeg2(bytes: &[u8]) -> u32 {
    let mut crc = INIT;
    for &byte in bytes {
        // index = (top byte of register) XOR input byte. Mask to the low 8 bits
        // and narrow with `TryFrom` so no `as` cast is needed; the mask makes the
        // conversion infallible (default to 0 to stay total).
        let top = u8::try_from((crc >> 24) & 0xFF).unwrap_or(0);
        let index = usize::from(top ^ byte);
        let entry = match TABLE.get(index) {
            Some(&e) => e,
            // Unreachable: `index` is a `u8`-derived value in `0..=255`, always a
            // valid index into the 256-entry table. Default to the identity so the
            // function stays total and panic-free.
            None => 0,
        };
        crc = (crc << 8) ^ entry;
    }
    crc
}

/// Validate that `section` (which **must** include its trailing four CRC bytes)
/// carries a correct CRC-32/MPEG-2.
///
/// Returns `Ok(())` when the running CRC over the whole section is `0` (the
/// self-checking property of an appended CRC), otherwise an
/// [`crate::mpegts::MpegTsError::Crc`] carrying both the section's stored CRC and
/// the value computed over the body.
///
/// # Errors
///
/// * [`crate::mpegts::MpegTsError::TooShort`] if `section` is under four bytes.
/// * [`crate::mpegts::MpegTsError::Crc`] if the CRC does not validate.
pub fn validate(section: &[u8]) -> Result<(), crate::mpegts::MpegTsError> {
    use crate::mpegts::MpegTsError;
    if section.len() < 4 {
        return Err(MpegTsError::TooShort {
            need: 4,
            got: section.len(),
        });
    }
    let body_len = section.len().saturating_sub(4);
    let body = section.get(..body_len).ok_or(MpegTsError::TooShort {
        need: 4,
        got: section.len(),
    })?;
    let crc_bytes = section.get(body_len..).ok_or(MpegTsError::TooShort {
        need: 4,
        got: section.len(),
    })?;
    let carried = read_be_u32(crc_bytes)?;
    let computed = crc32_mpeg2(body);
    if carried == computed {
        Ok(())
    } else {
        Err(MpegTsError::Crc { carried, computed })
    }
}

/// Read a big-endian `u32` from the first four bytes of `bytes`.
fn read_be_u32(bytes: &[u8]) -> Result<u32, crate::mpegts::MpegTsError> {
    use crate::mpegts::MpegTsError;
    let b0 = *bytes
        .first()
        .ok_or(MpegTsError::TooShort { need: 4, got: 0 })?;
    let b1 = *bytes
        .get(1)
        .ok_or(MpegTsError::TooShort { need: 4, got: 1 })?;
    let b2 = *bytes
        .get(2)
        .ok_or(MpegTsError::TooShort { need: 4, got: 2 })?;
    let b3 = *bytes
        .get(3)
        .ok_or(MpegTsError::TooShort { need: 4, got: 3 })?;
    Ok((u32::from(b0) << 24) | (u32::from(b1) << 16) | (u32::from(b2) << 8) | u32::from(b3))
}
