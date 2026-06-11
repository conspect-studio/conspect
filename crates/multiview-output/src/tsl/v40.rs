//! TSL UMD **v4.0** encoder.
//!
//! Produces the **19-byte** v4.0 core message (address + control + 16 ASCII +
//! checksum), and — for TCP/byte-stream transports — the `DLE STX`-framed form.
//! See the matching decoder docs in `multiview-input::tsl::v40` for the bit layout
//! and checksum definition.

use multiview_core::tally::Brightness;

use super::{TslError, UmdDisplay, UmdMessage};

/// The on-wire length of a bare (UDP) v4.0 core message, in bytes.
pub const CORE_LEN: usize = 19;

/// The number of ASCII label bytes in a v4.0 message.
pub const TEXT_LEN: usize = 16;

/// DLE (Data Link Escape) — the first framing byte on TCP/byte-stream.
pub const DLE: u8 = 0x10;

/// STX (Start of Text) — the second framing byte on TCP/byte-stream.
pub const STX: u8 = 0x02;

/// Bit 7 of the address byte — the sync marker.
const ADDRESS_SYNC: u8 = 0x80;

/// Mask for the 7-bit display address.
const ADDRESS_MASK: u8 = 0x7F;

/// Bit shift to the right-hand tally field (bits 2–3).
const RH_SHIFT: u8 = 2;

/// Bit shift to the text tally field (bits 4–5).
const TEXT_SHIFT: u8 = 4;

/// Bit shift to the brightness field (bits 6–7).
const BRIGHTNESS_SHIFT: u8 = 6;

/// Encode the single display of a v4.0 [`UmdMessage`] to its 19-byte core
/// message (for UDP).
///
/// # Errors
///
/// * [`TslError::DisplayCount`] if the message has no displays.
/// * [`TslError::LabelTooLong`] / [`TslError::NonRepresentable`] for label
///   problems.
pub fn encode(message: &UmdMessage) -> Result<[u8; CORE_LEN], TslError> {
    let display = message
        .displays
        .first()
        .ok_or(TslError::DisplayCount { count: 0, max: 1 })?;
    encode_display(display)
}

/// Encode one [`UmdDisplay`] to a v4.0 core message.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_display(display: &UmdDisplay) -> Result<[u8; CORE_LEN], TslError> {
    let address = ADDRESS_SYNC | (low_addr(display.index) & ADDRESS_MASK);
    let brightness = brightest(display);
    let control = display.left.color_code()
        | (display.right.color_code() << RH_SHIFT)
        | (display.text_tally.color_code() << TEXT_SHIFT)
        | (brightness.level() << BRIGHTNESS_SHIFT);

    let mut core = [0x20u8; CORE_LEN]; // space-padded label region
    *core.get_mut(0).ok_or(overflow())? = address;
    *core.get_mut(1).ok_or(overflow())? = control;
    write_ascii(&mut core, 2, &display.text)?;
    // Checksum over the first 18 bytes; the 19th holds it.
    let body = core.get(..CORE_LEN - 1).ok_or(overflow())?;
    let sum = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    *core.get_mut(CORE_LEN - 1).ok_or(overflow())? = sum.wrapping_neg();
    Ok(core)
}

/// Encode one display wrapped in **DLE/STX** framing for a TCP/byte-stream
/// transport (`DLE STX` + 19-byte core). Returns a heap `Vec` because the framed
/// length is fixed but distinct from [`CORE_LEN`].
///
/// # Errors
///
/// As [`encode_display`].
pub fn encode_display_framed(display: &UmdDisplay) -> Result<Vec<u8>, TslError> {
    let core = encode_display(display)?;
    let mut out = Vec::with_capacity(2 + CORE_LEN);
    out.push(DLE);
    out.push(STX);
    out.extend_from_slice(&core);
    Ok(out)
}

/// The brightest lit lamp's brightness (v4.0 carries a single shared
/// brightness); defaults to full when nothing is lit.
fn brightest(display: &UmdDisplay) -> Brightness {
    [display.left, display.text_tally, display.right]
        .iter()
        .filter(|lamp| lamp.is_lit())
        .map(|lamp| lamp.brightness)
        .max_by_key(|b| b.level())
        .unwrap_or(Brightness::FULL)
}

/// Truncate a 16-bit index to the 7-bit address space without an `as` cast.
fn low_addr(index: u16) -> u8 {
    u8::try_from(index & u16::from(ADDRESS_MASK)).unwrap_or(0)
}

/// Write a label into the fixed-width ASCII field, space-padding; rejects
/// over-long or non-ASCII text.
fn write_ascii(core: &mut [u8; CORE_LEN], offset: usize, text: &str) -> Result<(), TslError> {
    if text.chars().count() > TEXT_LEN {
        return Err(TslError::LabelTooLong {
            len: text.chars().count(),
            max: TEXT_LEN,
        });
    }
    for (i, ch) in text.chars().enumerate() {
        if !ch.is_ascii() {
            return Err(TslError::NonRepresentable {
                encoding: "v4.0 ASCII",
                ch,
            });
        }
        let byte = u8::try_from(u32::from(ch)).map_err(|_| TslError::NonRepresentable {
            encoding: "v4.0 ASCII",
            ch,
        })?;
        *core.get_mut(offset + i).ok_or(overflow())? = byte;
    }
    Ok(())
}

/// Stand-in error for a slice index that cannot occur given the fixed buffer and
/// bounded loops — used only to keep the code free of `unwrap`/`panic`.
fn overflow() -> TslError {
    TslError::PacketTooLong {
        len: CORE_LEN,
        max: CORE_LEN,
    }
}
