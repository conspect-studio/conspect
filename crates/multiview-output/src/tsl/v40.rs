//! TSL UMD **v4.0** encoder.
//!
//! Produces the fixed **22-byte** v4.0 message: the complete 18-byte v3.1
//! message plus a **CHKSUM**, a **VBC** byte and two **XDATA** colour bytes.
//! See the matching decoder docs in `multiview-input::tsl::v40` for the byte
//! and bit layout. Like v3.1, v4.0 defines **no** byte-stream framing: the
//! `0x80` address sync bit delimits serial messages and UDP carries one
//! message per datagram (DLE/STX framing starts with v5.0).
//!
//! The encoder mirrors **XDATA 1** into **XDATA 2** (this model encodes one
//! display per message; a true dual-display unit with differing left/right
//! displays is not representable here).

use multiview_core::tally::Brightness;

use super::{TslError, UmdDisplay, UmdMessage};

/// The fixed on-wire length of a v4.0 message, in bytes.
pub const PACKET_LEN: usize = 22;

/// The length of the embedded v3.1 message (address, control, 16 label bytes).
pub const V31_LEN: usize = 18;

/// The number of ASCII label bytes in a v4.0 message.
pub const TEXT_LEN: usize = 16;

/// The mandatory VBC value for v4.0 display data: minor version 0 (bits 6–4),
/// two XDATA bytes (bits 3–0).
const VBC: u8 = 0x02;

/// Bit 7 of the address byte — the sync marker.
const ADDRESS_SYNC: u8 = 0x80;

/// Mask for the 7-bit display address.
const ADDRESS_MASK: u8 = 0x7F;

/// Bit shift to the brightness field (bits 4–5) of the control byte.
const CTRL_BRIGHTNESS_SHIFT: u8 = 4;

/// Bit 0 of the control byte: tally 1 on/off.
const CTRL_TALLY1: u8 = 0b0000_0001;

/// Bit 1 of the control byte: tally 2 on/off.
const CTRL_TALLY2: u8 = 0b0000_0010;

/// Shift to the text tally colour field (bits 3–2) of an XDATA byte.
const XDATA_TEXT_SHIFT: u8 = 2;

/// Shift to the left tally colour field (bits 5–4) of an XDATA byte.
const XDATA_LH_SHIFT: u8 = 4;

/// Encode the single display of a v4.0 [`UmdMessage`] to its 22-byte message.
///
/// # Errors
///
/// * [`TslError::DisplayCount`] if the message has no displays.
/// * [`TslError::LabelTooLong`] / [`TslError::NonRepresentable`] for label
///   problems.
pub fn encode(message: &UmdMessage) -> Result<[u8; PACKET_LEN], TslError> {
    let display = message
        .displays
        .first()
        .ok_or(TslError::DisplayCount { count: 0, max: 1 })?;
    encode_display(display)
}

/// Encode one [`UmdDisplay`] to a v4.0 message.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_display(display: &UmdDisplay) -> Result<[u8; PACKET_LEN], TslError> {
    let address = ADDRESS_SYNC | (low_addr(display.index) & ADDRESS_MASK);
    let brightness = brightest(display);
    // Control byte: reserved bit 7 clear, bit 6 clear (display data),
    // brightness in bits 4–5, tally 1/2 on/off flags in bits 0–1.
    let mut control = brightness.level() << CTRL_BRIGHTNESS_SHIFT;
    if display.left.is_lit() {
        control |= CTRL_TALLY1;
    }
    if display.right.is_lit() {
        control |= CTRL_TALLY2;
    }
    // XDATA byte: bit 7 clear, bit 6 (reserved) sent as 0, LH/Txt/RH colour
    // codes in bits 5–4 / 3–2 / 1–0.
    let xdata = (display.left.color_code() << XDATA_LH_SHIFT)
        | (display.text_tally.color_code() << XDATA_TEXT_SHIFT)
        | display.right.color_code();

    let mut packet = [0x20u8; PACKET_LEN]; // space-padded label region
    *packet.get_mut(0).ok_or(overflow())? = address;
    *packet.get_mut(1).ok_or(overflow())? = control;
    write_ascii(&mut packet, 2, &display.text)?;
    // CHKSUM: mod-128 complement of the sum of the 18 v3.1 bytes, then VBC
    // and the two XDATA bytes (display L mirrored into display R).
    let v31 = packet.get(..V31_LEN).ok_or(overflow())?;
    *packet.get_mut(V31_LEN).ok_or(overflow())? = checksum(v31);
    *packet.get_mut(V31_LEN + 1).ok_or(overflow())? = VBC;
    *packet.get_mut(V31_LEN + 2).ok_or(overflow())? = xdata;
    *packet.get_mut(V31_LEN + 3).ok_or(overflow())? = xdata;
    Ok(packet)
}

/// The v4.0 CHKSUM for the 18 v3.1 bytes: the value that makes their sum plus
/// CHKSUM divisible by 128 — the two's-complement (mod 128) of the byte sum.
#[must_use]
pub fn checksum(v31_bytes: &[u8]) -> u8 {
    let sum = v31_bytes
        .iter()
        .take(V31_LEN)
        .fold(0u32, |acc, &b| acc + u32::from(b));
    u8::try_from((128 - (sum % 128)) % 128).unwrap_or(0)
}

/// The brightest lit lamp's brightness (the control byte carries a single
/// shared brightness); defaults to full when nothing is lit.
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
fn write_ascii(packet: &mut [u8; PACKET_LEN], offset: usize, text: &str) -> Result<(), TslError> {
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
        *packet.get_mut(offset + i).ok_or(overflow())? = byte;
    }
    Ok(())
}

/// Stand-in error for a slice index that cannot occur given the fixed buffer and
/// bounded loops — used only to keep the code free of `unwrap`/`panic`.
fn overflow() -> TslError {
    TslError::PacketTooLong {
        len: PACKET_LEN,
        max: PACKET_LEN,
    }
}
