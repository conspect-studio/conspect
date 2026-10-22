//! TSL UMD **v4.0** decoder.
//!
//! v4.0 keeps the entire **v3.1 message** and appends **four trailing bytes**:
//! a **CHKSUM**, a **VBC** (version/length byte) and two **XDATA** colour bytes.
//! The full message is a fixed **22 bytes**:
//!
//! ```text
//!   bytes 0..=17 : the complete v3.1 message
//!                  byte 0        : 1 A A A A A A A   address (sync bit + 7-bit address)
//!                  byte 1        : 0 0 b b T T t2 t1 control byte
//!                  bytes 2..=17  : 16 × 7-bit ASCII  label (space padded)
//!   byte 18      : CHKSUM        negated mod-128 sum of bytes 0..=17
//!   byte 19      : VBC           0 0 0 0 0 0 1 0    version 4.0, 2 XDATA bytes
//!   byte 20      : XDATA 1       0 r L L T T R R    Display L colours
//!   byte 21      : XDATA 2       0 r L L T T R R    Display R colours
//! ```
//!
//! * **CHKSUM** is computed so that `(sum of bytes 0..=18) mod 128 == 0` — the
//!   two's complement of the **mod-128** sum of the 18 v3.1 bytes (the spec
//!   keeps CHKSUM below `0x80` so it can never collide with the address byte's
//!   sync bit). Verified on decode before anything else is trusted.
//! * The **control byte** is the v3.1 byte unchanged: bits 0–3 tallies 1–4
//!   (the four parallel tally outputs per the spec), bits 4–5 brightness.
//!   This decoder reads bits 0–1 only, as on/off, and does not model them as
//!   lamps — the colour palette lives in XDATA.
//! * **VBC** (`0x02`) declares minor version 0 and exactly two XDATA bytes.
//! * Each **XDATA** byte packs three 2-bit colour fields: bits 1–0 **RH**,
//!   bits 3–2 **Txt**, bits 5–4 **LH**, with `0` = off / `1` = red / `2` =
//!   green / `3` = amber. Bit 7 must be `0` and is rejected on receive;
//!   bit 6 is unspecified (transmitters send `0`) and is never read.
//!
//! A dual-display unit describes both displays in one message (XDATA 1 = left,
//! XDATA 2 = right); both displays share the one address and the one 16-char
//! label. This decoder models **one display** per message (see [`decode`]), so
//! it reads XDATA 1 and the shared label; XDATA 2 is accepted and discarded —
//! the discard loses only the Display R colours, a documented consequence of
//! the one-display model, not a framing error.
//!
//! Like v3.1, v4.0 defines **no byte-stream framing**: the `0x80` address sync
//! bit delimits serial messages and UDP carries one message per datagram.
//! DLE/STX framing starts with v5.0.

use multiview_core::tally::Brightness;

use super::{TallyLamp, TslError, TslVersion, UmdDisplay, UmdMessage};

/// The fixed on-wire length of a v4.0 message, in bytes.
pub const PACKET_LEN: usize = 22;

/// The length of the embedded v3.1 message (address, control, 16 label bytes).
pub const V31_LEN: usize = 18;

/// The number of ASCII label bytes in a v4.0 message.
pub const TEXT_LEN: usize = 16;

/// CHKSUM modulus: the checksum is computed and validated mod 128 so it
/// always fits below the address byte's sync bit.
const CHKSUM_MOD: u32 = 128;

/// The mandatory VBC value for v4.0 display data: minor version 0 (bits 6–4),
/// two XDATA bytes (bits 3–0), reserved high bit clear.
const VBC: u8 = 0x02;

/// Bit 7 of the address byte — the sync marker that opens every message.
const ADDRESS_SYNC: u8 = 0x80;

/// Mask for the 7-bit display address.
const ADDRESS_MASK: u8 = 0x7F;

/// Bit 7 of the control byte, reserved `0` for display data.
const CTRL_RESERVED_HIGH: u8 = 0x80;

/// Bit 6 of the control byte: set for command (non-display) data.
const CTRL_COMMAND: u8 = 0x40;

/// Shift to the 2-bit brightness field (bits 4–5) of the control byte.
const CTRL_BRIGHTNESS_SHIFT: u8 = 4;

/// Mask (post-shift) for the 2-bit brightness field.
const CTRL_BRIGHTNESS_MASK: u8 = 0b11;

/// Reserved high bits of an XDATA byte (bit 7 must be clear; bit 6 is
/// unspecified and ignored on receive).
const XDATA_BIT7: u8 = 0b1000_0000;

/// Shift to the text tally colour field (bits 3–2) of an XDATA byte.
const XDATA_TEXT_SHIFT: u8 = 2;

/// Shift to the left tally colour field (bits 5–4) of an XDATA byte.
const XDATA_LH_SHIFT: u8 = 4;

/// Mask (post-shift) for any 2-bit colour field of an XDATA byte.
const COLOUR_MASK: u8 = 0b11;

/// Decode a single TSL UMD **v4.0** message.
///
/// `bytes` must be exactly [`PACKET_LEN`] (22) bytes. The returned
/// [`UmdMessage`] always holds exactly one [`UmdDisplay`] — index from the
/// 7-bit address, text from the 16 ASCII bytes, colours from **XDATA 1**
/// (display L), brightness from the control byte. XDATA 2 (display R) is
/// validated only for its reserved-high-bit shape and otherwise discarded;
/// a true dual-display unit whose two displays disagree therefore loses the
/// right display's colours (see the module docs).
///
/// # Errors
///
/// * [`TslError::TooShort`] / [`TslError::TooLong`] if `bytes.len() != 22`.
/// * [`TslError::Framing`] if the address byte's sync bit is clear, the
///   control byte marks command data or a reserved bit is set, or the VBC
///   byte is not `0x02`.
/// * [`TslError::Checksum`] if byte 18 is not the mod-128 complement of the
///   sum of bytes 0..18.
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

    let v31 = bytes.get(..V31_LEN).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    let checksum = checksum(v31);
    let wire = *bytes.get(V31_LEN).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if checksum != wire {
        return Err(TslError::Checksum {
            expected: wire,
            computed: checksum,
        });
    }

    let address = *bytes.first().ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if address & ADDRESS_SYNC == 0 {
        return Err(TslError::Framing(
            "v4.0 address byte missing sync bit (0x80)",
        ));
    }
    let index = u16::from(address & ADDRESS_MASK);

    let control = *bytes.get(1).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if control & CTRL_RESERVED_HIGH != 0 {
        return Err(TslError::Framing("v4.0 control byte reserved high bit set"));
    }
    if control & CTRL_COMMAND != 0 {
        return Err(TslError::Framing(
            "v4.0 control byte marks command data (bit 6); only display data is decoded",
        ));
    }

    let vbc = *bytes.get(V31_LEN + 1).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if vbc != VBC {
        return Err(TslError::Framing(
            "v4.0 VBC must be 0x02 (minor version 0, 2 XDATA bytes)",
        ));
    }

    let xdata1 = *bytes.get(V31_LEN + 2).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if xdata1 & XDATA_BIT7 != 0 {
        return Err(TslError::Framing("v4.0 XDATA byte 1 reserved high bit set"));
    }
    let xdata2 = *bytes.get(V31_LEN + 3).ok_or(TslError::TooShort {
        need: PACKET_LEN,
        got: bytes.len(),
    })?;
    if xdata2 & XDATA_BIT7 != 0 {
        return Err(TslError::Framing("v4.0 XDATA byte 2 reserved high bit set"));
    }
    // Bit 6 of each XDATA byte is unspecified on receive: masked off above.

    let brightness = Brightness::new((control >> CTRL_BRIGHTNESS_SHIFT) & CTRL_BRIGHTNESS_MASK);
    // The v3.1 tally bits (bits 0–1, read as on/off) drive the four parallel
    // tally outputs and are not part of this model's lamps: the colour palette
    // lives in XDATA. Colours come from XDATA 1 (display L).

    let left = lamp((xdata1 >> XDATA_LH_SHIFT) & COLOUR_MASK, brightness)?;
    let right = lamp(xdata1 & COLOUR_MASK, brightness)?;
    let text_tally = lamp((xdata1 >> XDATA_TEXT_SHIFT) & COLOUR_MASK, brightness)?;

    let text_bytes = bytes.get(2..V31_LEN).ok_or(TslError::TooShort {
        need: PACKET_LEN,
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

/// The v4.0 CHKSUM for a full 22-byte message: the value that makes the sum
/// of the 18 v3.1 bytes plus CHKSUM itself divisible by 128 — i.e. the
/// two's-complement (mod 128) of the v3.1 byte sum. Always `0..=0x7F`.
#[must_use]
pub fn checksum(bytes_0_to_18: &[u8]) -> u8 {
    debug_assert!(bytes_0_to_18.len() >= V31_LEN, "need the 18 v3.1 bytes");
    let sum = bytes_0_to_18
        .iter()
        .take(V31_LEN)
        .fold(0u32, |acc, &b| acc + u32::from(b));
    u8::try_from((CHKSUM_MOD - (sum % CHKSUM_MOD)) % CHKSUM_MOD).unwrap_or(0)
}

/// Decode a 2-bit colour code at the given brightness into a [`TallyLamp`].
fn lamp(code: u8, brightness: Brightness) -> Result<TallyLamp, TslError> {
    TallyLamp::from_wire(code, brightness)
        .ok_or(TslError::Framing("v4.0 tally colour code out of range"))
}

/// Decode a fixed-length 7-bit-ASCII label, masking the top bit and trimming
/// the trailing space padding the protocol uses.
fn decode_ascii(bytes: &[u8]) -> String {
    let text: String = bytes
        .iter()
        .map(|&b| char::from(b & ADDRESS_MASK))
        .collect();
    text.trim_end_matches(' ').to_owned()
}
