//! TSL UMD **v4.0** decoder.
//!
//! v4.0 extends v3.1 with a colour tally palette, a third (text) tally, a screen
//! byte and a per-display checksum. The **core message** is **19 bytes**:
//!
//! ```text
//!   byte 0       : 1 A A A A A A A     address byte (sync bit + 7-bit address)
//!   byte 1       : b b T T R R L L     control byte (VBC)
//!   bytes 2..18  : 16 × 7-bit ASCII    label (space padded)
//!   byte 18      : CHKSUM              two's-complement of the byte sum
//! ```
//!
//! The **control byte** packs four 2-bit fields (per the brief's LH / Text / RH
//! colour tally, `0` = off / `1` = red / `2` = green / `3` = amber):
//!
//! * bits 0–1 — **left-hand** (LH) tally colour,
//! * bits 2–3 — **right-hand** (RH) tally colour,
//! * bits 4–5 — **text** tally colour,
//! * bits 6–7 — **brightness** (`0..=3`).
//!
//! The **checksum** is computed so the 8-bit sum of all 19 bytes is zero (i.e.
//! `CHKSUM = (-sum(byte0..18)) & 0xFF`).
//!
//! On a **TCP / byte-stream** transport the message is additionally wrapped in
//! **DLE/STX** framing (`0x10 0x02 … core …`); UDP carries the bare core. Use
//! [`decode`] for the bare core (UDP) and [`decode_framed`] to strip the
//! DLE/STX wrapper first.

use multiview_core::tally::Brightness;

use super::{TallyLamp, TslError, TslVersion, UmdDisplay, UmdMessage};

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

/// Post-shift mask for any 2-bit field of the control byte.
const FIELD_MASK: u8 = 0b11;

/// Bit shift to the right-hand tally field (bits 2–3).
const RH_SHIFT: u8 = 2;

/// Bit shift to the text tally field (bits 4–5).
const TEXT_SHIFT: u8 = 4;

/// Bit shift to the brightness field (bits 6–7).
const BRIGHTNESS_SHIFT: u8 = 6;

/// Decode a bare (UDP) TSL UMD **v4.0** core message.
///
/// `bytes` must be exactly [`CORE_LEN`] (19) bytes: address, control, 16 ASCII
/// label bytes, checksum. The returned [`UmdMessage`] holds exactly one display.
///
/// # Errors
///
/// * [`TslError::TooShort`] / [`TslError::TooLong`] if `bytes.len() != 19`.
/// * [`TslError::Framing`] if the address byte's sync bit is clear.
/// * [`TslError::Checksum`] if the trailing checksum does not validate.
pub fn decode(bytes: &[u8]) -> Result<UmdMessage, TslError> {
    if bytes.len() < CORE_LEN {
        return Err(TslError::TooShort {
            need: CORE_LEN,
            got: bytes.len(),
        });
    }
    if bytes.len() > CORE_LEN {
        return Err(TslError::TooLong {
            max: CORE_LEN,
            got: bytes.len(),
        });
    }

    let body = bytes.get(..CORE_LEN - 1).ok_or(TslError::TooShort {
        need: CORE_LEN,
        got: bytes.len(),
    })?;
    let expected = *bytes.get(CORE_LEN - 1).ok_or(TslError::TooShort {
        need: CORE_LEN,
        got: bytes.len(),
    })?;
    let computed = checksum(body);
    if computed != expected {
        return Err(TslError::Checksum { expected, computed });
    }

    let address = *body.first().ok_or(TslError::TooShort {
        need: CORE_LEN,
        got: bytes.len(),
    })?;
    if address & ADDRESS_SYNC == 0 {
        return Err(TslError::Framing(
            "v4.0 address byte missing sync bit (0x80)",
        ));
    }
    let index = u16::from(address & ADDRESS_MASK);

    let control = *body.get(1).ok_or(TslError::TooShort {
        need: CORE_LEN,
        got: bytes.len(),
    })?;
    let brightness = Brightness::new((control >> BRIGHTNESS_SHIFT) & FIELD_MASK);
    let left = lamp(control & FIELD_MASK, brightness)?;
    let right = lamp((control >> RH_SHIFT) & FIELD_MASK, brightness)?;
    let text_tally = lamp((control >> TEXT_SHIFT) & FIELD_MASK, brightness)?;

    let text_bytes = body.get(2..CORE_LEN - 1).ok_or(TslError::TooShort {
        need: CORE_LEN,
        got: bytes.len(),
    })?;
    let text = decode_ascii(text_bytes);

    Ok(UmdMessage {
        version: TslVersion::V40,
        screen: 0,
        displays: vec![UmdDisplay {
            index,
            left,
            text_tally,
            right,
            text,
        }],
    })
}

/// Decode a **DLE/STX-framed** (TCP/byte-stream) v4.0 message.
///
/// Strips the leading `DLE STX` and decodes the remaining [`CORE_LEN`] bytes via
/// [`decode`].
///
/// # Errors
///
/// * [`TslError::TooShort`] if there are fewer than `2 + CORE_LEN` bytes.
/// * [`TslError::Framing`] if the first two bytes are not `DLE STX`.
/// * Any error from [`decode`] on the unwrapped core.
pub fn decode_framed(bytes: &[u8]) -> Result<UmdMessage, TslError> {
    let need = 2 + CORE_LEN;
    if bytes.len() < need {
        return Err(TslError::TooShort {
            need,
            got: bytes.len(),
        });
    }
    let (dle, stx) = (
        *bytes.first().ok_or(TslError::Framing("missing DLE"))?,
        *bytes.get(1).ok_or(TslError::Framing("missing STX"))?,
    );
    if dle != DLE || stx != STX {
        return Err(TslError::Framing("v4.0 frame must open with DLE STX"));
    }
    let core = bytes.get(2..).ok_or(TslError::Framing("truncated frame"))?;
    decode(core)
}

/// The v4.0 checksum: the value that makes the 8-bit sum of `body || checksum`
/// zero — i.e. `(-sum) mod 256`.
#[must_use]
pub fn checksum(body: &[u8]) -> u8 {
    let sum = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    sum.wrapping_neg()
}

/// Decode a 2-bit colour code at the given brightness into a [`TallyLamp`].
fn lamp(code: u8, brightness: Brightness) -> Result<TallyLamp, TslError> {
    TallyLamp::from_wire(code, brightness)
        .ok_or(TslError::Framing("v4.0 tally colour code out of range"))
}

/// Decode a fixed-length 7-bit-ASCII label, trimming trailing space padding.
fn decode_ascii(bytes: &[u8]) -> String {
    let text: String = bytes
        .iter()
        .map(|&b| char::from(b & ADDRESS_MASK))
        .collect();
    text.trim_end_matches(' ').to_owned()
}
