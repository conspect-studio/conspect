//! TSL UMD **v3.1** encoder.
//!
//! Produces the fixed **18-byte** v3.1 packet (address byte + control byte + 16
//! ASCII label bytes). See the matching decoder docs in
//! `multiview-input::tsl::v31` for the full bit layout. v3.1 has only on/off tally
//! (no colour palette and no text tally), so any lit lamp encodes as the tally
//! bit set; the text tally is not representable and is dropped.

use multiview_core::tally::Brightness;

use super::{TslError, UmdDisplay, UmdMessage};

/// The fixed on-wire length of a v3.1 packet, in bytes.
pub const PACKET_LEN: usize = 18;

/// The number of ASCII label bytes in a v3.1 packet.
pub const TEXT_LEN: usize = 16;

/// Bit 7 of the address byte — the sync marker.
const ADDRESS_SYNC: u8 = 0x80;

/// Mask for the 7-bit display address.
const ADDRESS_MASK: u8 = 0x7F;

/// Bit 0 of the control byte: tally 1 (left lamp).
const CTRL_TALLY1: u8 = 0b0000_0001;

/// Bit 1 of the control byte: tally 2 (right lamp).
const CTRL_TALLY2: u8 = 0b0000_0010;

/// Shift to the 2-bit brightness field (bits 4–5).
const CTRL_BRIGHTNESS_SHIFT: u8 = 4;

/// Encode the single display of a v3.1 [`UmdMessage`] to its 18-byte packet.
///
/// Only the first display is encoded (v3.1 is one display per packet); the
/// caller is responsible for splitting a multi-display message into one packet
/// each if needed.
///
/// # Errors
///
/// * [`TslError::DisplayCount`] if the message has no displays.
/// * [`TslError::LabelTooLong`] if the label exceeds 16 characters.
/// * [`TslError::NonRepresentable`] if the label has a non-ASCII character.
pub fn encode(message: &UmdMessage) -> Result<[u8; PACKET_LEN], TslError> {
    let display = message
        .displays
        .first()
        .ok_or(TslError::DisplayCount { count: 0, max: 1 })?;
    encode_display(display)
}

/// Encode one [`UmdDisplay`] to a v3.1 packet.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_display(display: &UmdDisplay) -> Result<[u8; PACKET_LEN], TslError> {
    let address = ADDRESS_SYNC | (low_addr(display.index) & ADDRESS_MASK);

    let brightness = brightest(display);
    let mut control = brightness.level() << CTRL_BRIGHTNESS_SHIFT;
    if display.left.is_lit() {
        control |= CTRL_TALLY1;
    }
    if display.right.is_lit() {
        control |= CTRL_TALLY2;
    }

    let mut packet = [0x20u8; PACKET_LEN]; // space-padded label by default
    *packet.get_mut(0).ok_or(unreachable_len())? = address;
    *packet.get_mut(1).ok_or(unreachable_len())? = control;
    write_ascii(&mut packet, 2, &display.text, "v3.1 ASCII")?;
    Ok(packet)
}

/// The brightest of a display's lit lamps (v3.1 carries a single shared
/// brightness); defaults to full if nothing is lit.
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

/// Write a label into a fixed-width ASCII field starting at `offset`,
/// space-padding the remainder; rejects over-long or non-ASCII text.
fn write_ascii(
    packet: &mut [u8; PACKET_LEN],
    offset: usize,
    text: &str,
    encoding: &'static str,
) -> Result<(), TslError> {
    if text.chars().count() > TEXT_LEN {
        return Err(TslError::LabelTooLong {
            len: text.chars().count(),
            max: TEXT_LEN,
        });
    }
    for (i, ch) in text.chars().enumerate() {
        if !ch.is_ascii() {
            return Err(TslError::NonRepresentable { encoding, ch });
        }
        let byte =
            u8::try_from(u32::from(ch)).map_err(|_| TslError::NonRepresentable { encoding, ch })?;
        *packet.get_mut(offset + i).ok_or(unreachable_len())? = byte;
    }
    Ok(())
}

/// A label-too-long error standing in for an index that cannot occur given the
/// fixed-size buffer and bounded loop above — used only to keep the code free of
/// `unwrap`/`panic`.
fn unreachable_len() -> TslError {
    TslError::PacketTooLong {
        len: PACKET_LEN,
        max: PACKET_LEN,
    }
}
