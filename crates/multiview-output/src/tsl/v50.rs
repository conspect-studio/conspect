//! TSL UMD **v5.0** encoder.
//!
//! Assembles a v5.0 packet (PBC + VER + FLAGS + SCREEN, then one or more
//! variable-length displays) and, for TCP/byte-stream transports, the
//! DLE/STX-framed, byte-stuffed form. See the matching decoder docs in
//! `multiview-input::tsl::v50` for the full layout. All multi-byte integers are
//! little-endian.
//!
//! Text is encoded as 7-bit ASCII by default; pass `unicode = true` to encode
//! every label as UTF-16LE (and set the packet's `FLAGS` Unicode bit). The flag
//! is per packet, matching the decoder, so an encode∘decode round trip is the
//! identity.

use multiview_core::tally::Brightness;

use super::{TslError, UmdDisplay, UmdMessage};

/// The maximum permitted v5.0 packet size, in bytes.
pub const MAX_PACKET_LEN: usize = 2048;

/// FLAGS bit 0: packet text is UTF-16LE rather than 7-bit ASCII.
pub const FLAG_UNICODE: u8 = 0b0000_0001;

/// The v5.0 protocol version byte.
pub const VERSION: u8 = 0;

/// DLE (Data Link Escape) — opens framing and is the stuffing escape on TCP.
pub const DLE: u8 = 0x10;

/// STX (Start of Text) — the second framing byte on TCP/byte-stream.
pub const STX: u8 = 0x02;

/// ETX (End of Text) — closes a `DLE`-stuffed frame.
pub const ETX: u8 = 0x03;

/// Shift to the right tally field (bits 2–3).
const RH_SHIFT: u16 = 2;

/// Shift to the text tally field (bits 4–5).
const TEXT_SHIFT: u16 = 4;

/// Shift to the brightness field (bits 6–7).
const BRIGHTNESS_SHIFT: u16 = 6;

/// Mask for a single 7-bit ASCII byte.
const ASCII_MASK: u8 = 0x7F;

/// Encode a v5.0 [`UmdMessage`] to a raw (UDP) packet.
///
/// When `unicode` is `true`, every label is encoded as UTF-16LE and the packet's
/// Unicode flag is set; otherwise labels are 7-bit ASCII.
///
/// # Errors
///
/// * [`TslError::DisplayCount`] if the message has no displays.
/// * [`TslError::NonRepresentable`] if a label has a non-ASCII char while
///   `unicode` is `false`.
/// * [`TslError::PacketTooLong`] if the assembled packet exceeds
///   [`MAX_PACKET_LEN`].
pub fn encode(message: &UmdMessage, unicode: bool) -> Result<Vec<u8>, TslError> {
    if message.displays.is_empty() {
        return Err(TslError::DisplayCount {
            count: 0,
            max: usize::MAX,
        });
    }

    // Assemble everything that follows the PBC field, then prepend PBC.
    let mut after_pbc = Vec::new();
    after_pbc.push(VERSION);
    after_pbc.push(if unicode { FLAG_UNICODE } else { 0 });
    after_pbc.extend_from_slice(&message.screen.to_le_bytes());

    for display in &message.displays {
        after_pbc.extend_from_slice(&display.index.to_le_bytes());
        after_pbc.extend_from_slice(&control_word(display).to_le_bytes());
        let text = encode_text(&display.text, unicode)?;
        let len = u16::try_from(text.len()).map_err(|_| TslError::PacketTooLong {
            len: text.len(),
            max: usize::from(u16::MAX),
        })?;
        after_pbc.extend_from_slice(&len.to_le_bytes());
        after_pbc.extend_from_slice(&text);
    }

    let pbc = u16::try_from(after_pbc.len()).map_err(|_| TslError::PacketTooLong {
        len: after_pbc.len(),
        max: usize::from(u16::MAX),
    })?;
    let mut packet = Vec::with_capacity(2 + after_pbc.len());
    packet.extend_from_slice(&pbc.to_le_bytes());
    packet.extend_from_slice(&after_pbc);

    if packet.len() > MAX_PACKET_LEN {
        return Err(TslError::PacketTooLong {
            len: packet.len(),
            max: MAX_PACKET_LEN,
        });
    }
    Ok(packet)
}

/// Encode a v5.0 packet **DLE/STX-framed and byte-stuffed** for a TCP/byte-stream
/// transport: `DLE STX` + stuffed payload (`0x10` → `0x10 0x10`) + `DLE ETX`.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_stuffed(message: &UmdMessage, unicode: bool) -> Result<Vec<u8>, TslError> {
    let raw = encode(message, unicode)?;
    let mut out = Vec::with_capacity(raw.len() + 6);
    out.push(DLE);
    out.push(STX);
    for &b in &raw {
        out.push(b);
        if b == DLE {
            out.push(DLE); // stuff a literal DLE
        }
    }
    out.push(DLE);
    out.push(ETX);
    Ok(out)
}

/// Build the 16-bit control word (tally colours + brightness) for a display.
fn control_word(display: &UmdDisplay) -> u16 {
    let brightness = brightest(display);
    u16::from(display.left.color_code())
        | (u16::from(display.right.color_code()) << RH_SHIFT)
        | (u16::from(display.text_tally.color_code()) << TEXT_SHIFT)
        | (u16::from(brightness.level()) << BRIGHTNESS_SHIFT)
}

/// The brightest lit lamp's brightness; defaults to full when nothing is lit.
fn brightest(display: &UmdDisplay) -> Brightness {
    [display.left, display.text_tally, display.right]
        .iter()
        .filter(|lamp| lamp.is_lit())
        .map(|lamp| lamp.brightness)
        .max_by_key(|b| b.level())
        .unwrap_or(Brightness::FULL)
}

/// Encode a label as UTF-16LE (when `unicode`) or 7-bit ASCII.
fn encode_text(text: &str, unicode: bool) -> Result<Vec<u8>, TslError> {
    if unicode {
        let mut out = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(out)
    } else {
        let mut out = Vec::with_capacity(text.len());
        for ch in text.chars() {
            if !ch.is_ascii() {
                return Err(TslError::NonRepresentable {
                    encoding: "v5.0 ASCII",
                    ch,
                });
            }
            let byte = u8::try_from(u32::from(ch)).map_err(|_| TslError::NonRepresentable {
                encoding: "v5.0 ASCII",
                ch,
            })?;
            out.push(byte & ASCII_MASK);
        }
        Ok(out)
    }
}
