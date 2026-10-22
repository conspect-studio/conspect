//! TSL UMD **v5.0** decoder.
//!
//! v5.0 is the native 16-bit IP protocol. A packet carries a header, then one or
//! more **variable-length** displays. All multi-byte integers are
//! **little-endian**:
//!
//! ```text
//!   PBC    : u16   packet byte count — number of bytes that FOLLOW this field
//!   VER    : u8    protocol version (0)
//!   FLAGS  : u8    bit 0 = packet text is UTF-16LE (else 7-bit ASCII)
//!   SCREEN : u16   screen address (0xFFFF = broadcast)
//!   ── then 1..N display messages ──
//!     INDEX   : u16   display index (0xFFFF = broadcast)
//!     CONTROL : u16   tally + brightness (see below)
//!     LENGTH  : u16   text length in BYTES
//!     TEXT    : LENGTH bytes (ASCII, or UTF-16LE when FLAGS bit 0 is set)
//! ```
//!
//! The 16-bit **CONTROL** word:
//!
//! * bits 0–1 — **right** tally colour (`0` off / `1` red / `2` green / `3` amber),
//! * bits 2–3 — **text** tally colour,
//! * bits 4–5 — **left** tally colour,
//! * bits 6–7 — **brightness** (`0..=3`),
//! * bits 8–14 — reserved (send `0`, ignore on receive),
//! * bit 15 — **control data**: the DMSG carries control (not display) data;
//!   such DMSGs are skipped by this decoder.
//!
//! The maximum packet size is **2048 bytes**; the number of displays is bounded
//! by that ceiling, not by a fixed count.
//!
//! On a **TCP / byte-stream** transport the packet is **DLE/STX byte-stuffed**:
//! `DLE STX` (`0xFE 0x02`) opens a packet and every literal `0xFE` in the
//! payload is doubled to `0xFE 0xFE`. There is **no terminator** — the frame
//! length is known from the PBC field, so the stuffing covers the whole
//! segment. On **UDP** the packet is sent raw. Use [`decode`] for a raw
//! (UDP) packet and [`decode_stuffed`] to remove the framing/stuffing first.

use multiview_core::tally::Brightness;

use super::{TallyLamp, TslError, TslVersion, UmdDisplay, UmdMessage};

/// The maximum permitted v5.0 packet size, in bytes.
pub const MAX_PACKET_LEN: usize = 2048;

/// The fixed header length after the PBC field (VER + FLAGS + SCREEN).
const HEADER_AFTER_PBC: usize = 4;

/// The fixed per-display prefix length (INDEX + CONTROL + LENGTH).
const DISPLAY_PREFIX: usize = 6;

/// The broadcast screen / index address.
pub const BROADCAST: u16 = 0xFFFF;

/// FLAGS bit 0: packet text is UTF-16LE rather than 7-bit ASCII.
pub const FLAG_UNICODE: u8 = 0b0000_0001;

/// DLE (Data Link Escape) — opens framing and is the stuffing escape on TCP.
pub const DLE: u8 = 0xFE;

/// STX (Start of Text) — the second framing byte on TCP/byte-stream.
pub const STX: u8 = 0x02;

/// Bit 15 of the CONTROL word: the DMSG carries control (not display) data.
const CONTROL_DATA: u16 = 0b1000_0000_0000_0000;

/// Post-shift mask for any 2-bit control field.
const FIELD_MASK: u16 = 0b11;

/// Shift to the text tally field (bits 2–3).
const TEXT_SHIFT: u16 = 2;

/// Shift to the left tally field (bits 4–5).
const LH_SHIFT: u16 = 4;

/// Shift to the brightness field (bits 6–7).
const BRIGHTNESS_SHIFT: u16 = 6;

/// Mask for a single 7-bit ASCII byte.
const ASCII_MASK: u8 = 0x7F;

/// Decode a raw (UDP) TSL UMD **v5.0** packet.
///
/// # Errors
///
/// * [`TslError::TooShort`] if the buffer is shorter than the header or any
///   declared length runs past the end of the buffer.
/// * [`TslError::TooLong`] if the buffer exceeds [`MAX_PACKET_LEN`].
/// * [`TslError::Length`] if the PBC byte count does not match the bytes present,
///   or a display's text [`TslError::Length`] runs past the packet.
/// * [`TslError::Framing`] if a tally colour code is out of range.
/// * [`TslError::OddUtf16Len`] if a UTF-16LE label has an odd byte length.
pub fn decode(bytes: &[u8]) -> Result<UmdMessage, TslError> {
    if bytes.len() > MAX_PACKET_LEN {
        return Err(TslError::TooLong {
            max: MAX_PACKET_LEN,
            got: bytes.len(),
        });
    }
    // PBC (u16 LE): bytes that follow the PBC field.
    let pbc = read_u16(bytes, 0)?;
    let declared = usize::from(pbc);
    let available = bytes.len().saturating_sub(2);
    if declared != available {
        return Err(TslError::Length {
            declared,
            available,
        });
    }
    if available < HEADER_AFTER_PBC {
        return Err(TslError::TooShort {
            need: 2 + HEADER_AFTER_PBC,
            got: bytes.len(),
        });
    }

    let _ver = read_u8(bytes, 2)?;
    let flags = read_u8(bytes, 3)?;
    let unicode = flags & FLAG_UNICODE != 0;
    let screen = read_u16(bytes, 4)?;

    let mut offset = 2 + HEADER_AFTER_PBC; // first display
    let mut displays = Vec::new();
    while offset < bytes.len() {
        let index = read_u16(bytes, offset)?;
        let control = read_u16(bytes, offset + 2)?;
        let text_len = usize::from(read_u16(bytes, offset + 4)?);
        let text_start = offset + DISPLAY_PREFIX;
        let text_end = text_start.checked_add(text_len).ok_or(TslError::Length {
            declared: text_len,
            available: bytes.len().saturating_sub(text_start),
        })?;
        let text_bytes = bytes.get(text_start..text_end).ok_or(TslError::Length {
            declared: text_len,
            available: bytes.len().saturating_sub(text_start),
        })?;

        if control & CONTROL_DATA != 0 {
            // A control-data DMSG: not a display. Skip it entirely — it
            // produces no [`UmdDisplay`] — and keep scanning. The spec leaves
            // the control-data body undefined; this decoder assumes it keeps
            // the INDEX/CONTROL/LENGTH/TEXT layout so the walk stays in sync,
            // and the length bounds above reject a desync instead of
            // mis-decoding a display.
            offset = text_end;
            continue;
        }

        let brightness = Brightness::new(low_byte((control >> BRIGHTNESS_SHIFT) & FIELD_MASK));
        let right = lamp(control & FIELD_MASK, brightness)?;
        let text_tally = lamp((control >> TEXT_SHIFT) & FIELD_MASK, brightness)?;
        let left = lamp((control >> LH_SHIFT) & FIELD_MASK, brightness)?;
        let text = decode_text(text_bytes, unicode)?;

        displays.push(UmdDisplay {
            index,
            left,
            text_tally,
            right,
            text,
        });
        offset = text_end;
    }

    if displays.is_empty() {
        return Err(TslError::Framing(
            "v5.0 packet contains no display DMSGs (all skipped or empty)",
        ));
    }

    Ok(UmdMessage {
        version: TslVersion::V50,
        screen,
        displays,
    })
}

/// Decode a **DLE/STX-framed, byte-stuffed** (TCP/byte-stream) v5.0 packet.
///
/// Verifies the leading `DLE STX` (`0xFE 0x02`), then un-stuffs the **entire
/// remaining segment** — every literal `DLE` in the payload is doubled to
/// `DLE DLE`. v5.0 defines **no terminator**, and PBC is **not consulted
/// while un-stuffing**: this is a whole-segment API, so the caller must hand
/// it exactly one stuffed frame; once un-stuffed, the packet's own PBC check
/// rejects any trailing bytes.
///
/// # Errors
///
/// * [`TslError::Framing`] if the segment does not open with `DLE STX`, or a
///   `DLE` in the payload is followed by anything other than a second `DLE`.
/// * Any error from [`decode`] on the un-stuffed packet.
pub fn decode_stuffed(bytes: &[u8]) -> Result<UmdMessage, TslError> {
    let mut it = bytes.iter().copied();
    match (it.next(), it.next()) {
        (Some(DLE), Some(STX)) => {}
        _ => return Err(TslError::Framing("v5.0 stream must open with DLE STX")),
    }
    let mut raw = Vec::with_capacity(bytes.len());
    while let Some(b) = it.next() {
        if b == DLE {
            match it.next() {
                // Stuffed literal DLE.
                Some(DLE) => raw.push(DLE),
                // Any other escape (or a dangling trailing DLE) is invalid:
                // there is no terminator, so a DLE is always doubled.
                _ => {
                    return Err(TslError::Framing(
                        "v5.0 invalid DLE escape in stuffed payload",
                    ))
                }
            }
        } else {
            raw.push(b);
        }
    }
    decode(&raw)
}

/// Read a little-endian `u16` at `offset`, or [`TslError::TooShort`].
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, TslError> {
    let lo = read_u8(bytes, offset)?;
    let hi = read_u8(bytes, offset + 1)?;
    Ok(u16::from(lo) | (u16::from(hi) << 8))
}

/// Read a single byte at `offset`, or [`TslError::TooShort`].
fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, TslError> {
    bytes.get(offset).copied().ok_or(TslError::TooShort {
        need: offset + 1,
        got: bytes.len(),
    })
}

/// Truncate a small `u16` field (already masked to `0..=3`) to a `u8` without an
/// `as` cast.
fn low_byte(value: u16) -> u8 {
    u8::try_from(value & 0xFF).unwrap_or(0)
}

/// Decode a 2-bit colour code at the given brightness into a [`TallyLamp`].
fn lamp(code: u16, brightness: Brightness) -> Result<TallyLamp, TslError> {
    TallyLamp::from_wire(low_byte(code), brightness)
        .ok_or(TslError::Framing("v5.0 tally colour code out of range"))
}

/// Decode a label, as UTF-16LE when `unicode`, else 7-bit ASCII.
fn decode_text(bytes: &[u8], unicode: bool) -> Result<String, TslError> {
    if unicode {
        if bytes.len() % 2 != 0 {
            return Err(TslError::OddUtf16Len(bytes.len()));
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| {
                let lo = pair.first().copied().unwrap_or(0);
                let hi = pair.get(1).copied().unwrap_or(0);
                u16::from(lo) | (u16::from(hi) << 8)
            })
            .collect();
        // Replace any unpaired surrogates rather than failing the whole packet.
        Ok(String::from_utf16_lossy(&units))
    } else {
        Ok(bytes.iter().map(|&b| char::from(b & ASCII_MASK)).collect())
    }
}
