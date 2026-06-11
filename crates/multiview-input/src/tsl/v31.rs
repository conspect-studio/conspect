//! TSL UMD **v3.1** decoder.
//!
//! v3.1 is the original serial protocol, also carried one packet per UDP
//! datagram. Each packet is a fixed **18 bytes** describing exactly one display:
//!
//! ```text
//!   byte 0      : 1 A A A A A A A      address byte (sync bit + 7-bit address)
//!   byte 1      : 0 b b R R R t2 t1    control byte
//!   bytes 2..18 : 16 × 7-bit ASCII     label (space padded)
//! ```
//!
//! * The **address byte** has bit 7 set as a sync marker; the low 7 bits are the
//!   display address `0..=126` (`0x80..=0xFE`). `0x7F`/address `127` is the
//!   reserved "all-call".
//! * The **control byte** carries `tally 1` (bit 0, conventionally the left lamp)
//!   and `tally 2` (bit 1, the right lamp) as on/off flags, and a 2-bit
//!   **brightness** in bits 4–5. v3.1 has no colour palette and no separate text
//!   tally, so the lit lamps decode to [`TallyColor::Red`] (the only colour the
//!   wire can express) and the text tally is always off.
//! * The 16 **label** bytes are 7-bit ASCII (top bit clear), space-padded.
//!
//! The address byte's sync bit (bit 7 = 1) and the control byte's reserved high
//! bit (bit 7 = 0) are the only framing this generation has; both are checked.

use multiview_core::tally::{Brightness, TallyColor};

use super::{TallyLamp, TslError, TslVersion, UmdDisplay, UmdMessage};

/// The fixed on-wire length of a v3.1 packet, in bytes.
pub const PACKET_LEN: usize = 18;

/// The number of ASCII label bytes in a v3.1 packet.
pub const TEXT_LEN: usize = 16;

/// Bit 7 of the address byte — the sync marker that opens every v3.1 packet.
const ADDRESS_SYNC: u8 = 0x80;

/// Mask for the 7-bit display address in the address byte.
const ADDRESS_MASK: u8 = 0x7F;

/// Bit 0 of the control byte: tally 1 (left lamp) on/off.
const CTRL_TALLY1: u8 = 0b0000_0001;

/// Bit 1 of the control byte: tally 2 (right lamp) on/off.
const CTRL_TALLY2: u8 = 0b0000_0010;

/// Shift to the 2-bit brightness field (bits 4–5) of the control byte.
const CTRL_BRIGHTNESS_SHIFT: u8 = 4;

/// Mask (post-shift) for the 2-bit brightness field.
const CTRL_BRIGHTNESS_MASK: u8 = 0b11;

/// Bit 7 of the control byte, which the spec reserves as `0`.
const CTRL_RESERVED_HIGH: u8 = 0x80;

/// Decode a single TSL UMD **v3.1** packet.
///
/// `bytes` must be exactly [`PACKET_LEN`] (18) bytes — the address byte, the
/// control byte, then 16 ASCII label bytes. The returned [`UmdMessage`] always
/// holds exactly one [`UmdDisplay`], with [`UmdMessage::screen`] set to `0` (v3.1
/// has no screen concept).
///
/// # Errors
///
/// * [`TslError::TooShort`] / [`TslError::TooLong`] if `bytes.len()` is not 18.
/// * [`TslError::Framing`] if the address byte's sync bit is clear or the control
///   byte's reserved high bit is set.
pub fn decode(bytes: &[u8]) -> Result<UmdMessage, TslError> {
    if bytes.len() < PACKET_LEN {
        return Err(TslError::TooShort {
            need: PACKET_LEN,
            got: bytes.len(),
        });
    }
    if bytes.len() > PACKET_LEN {
        return Err(TslError::TooLong {
            max: PACKET_LEN,
            got: bytes.len(),
        });
    }

    let address = *bytes.first().ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if address & ADDRESS_SYNC == 0 {
        return Err(TslError::Framing(
            "v3.1 address byte missing sync bit (0x80)",
        ));
    }
    let index = u16::from(address & ADDRESS_MASK);

    let control = *bytes.get(1).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if control & CTRL_RESERVED_HIGH != 0 {
        return Err(TslError::Framing("v3.1 control byte reserved high bit set"));
    }

    let brightness = Brightness::new((control >> CTRL_BRIGHTNESS_SHIFT) & CTRL_BRIGHTNESS_MASK);
    let left = on_off_lamp(control & CTRL_TALLY1 != 0, brightness);
    let right = on_off_lamp(control & CTRL_TALLY2 != 0, brightness);

    let text_bytes = bytes.get(2..PACKET_LEN).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    let text = decode_ascii(text_bytes);

    Ok(UmdMessage {
        version: TslVersion::V31,
        screen: 0,
        displays: vec![UmdDisplay {
            index,
            left,
            text_tally: TallyLamp::off(),
            right,
            text,
        }],
    })
}

/// Map a v3.1 on/off tally bit to a lamp: lit ⇒ red, clear ⇒ off.
fn on_off_lamp(lit: bool, brightness: Brightness) -> TallyLamp {
    TallyLamp {
        color: if lit {
            TallyColor::Red
        } else {
            TallyColor::Off
        },
        brightness,
    }
}

/// Decode a fixed-length 7-bit-ASCII label, masking the top bit and trimming the
/// trailing space padding the protocol uses.
fn decode_ascii(bytes: &[u8]) -> String {
    let text: String = bytes
        .iter()
        .map(|&b| char::from(b & ADDRESS_MASK))
        .collect();
    text.trim_end_matches(' ').to_owned()
}
