//! A **Pro-Bel SW-P-08** ("General Switcher / Router Communication Protocol")
//! message codec.
//!
//! SW-P-08 is one of the two openly-published router-control protocols Multiview
//! speaks for **name-following** UMD labels and external **route-follow** tally
//! (broadcast-multiviewer brief §2/§8). This module owns the **pure** message
//! model and the byte-level frame codec; the live TCP/serial connection to a
//! router is behind the off-by-default `router` feature (`super::transport`),
//! so the wire format is exhaustively testable with no sockets.
//!
//! ## Frame format
//!
//! Every message rides a **DLE/STX** frame, exactly as the protocol defines for
//! its byte-stream (serial / TCP) carriage:
//!
//! ```text
//!   DLE STX  <message-bytes>  DLE ETX  <BCC>
//! ```
//!
//! * `DLE`=0x10, `STX`=0x02, `ETX`=0x03.
//! * Any `DLE` byte **inside** the message body is escaped by doubling it
//!   (`DLE DLE`) so it cannot be mistaken for a frame marker — classic
//!   byte-stuffing. The decoder un-stuffs it.
//! * The **BCC** (block check character) is the two's-complement of the 8-bit
//!   sum of the *un-stuffed* message body, so `(sum + bcc) & 0xFF == 0`.
//!
//! ## Message body
//!
//! The body is `<COMMAND> <DATA…>`. This codec models the subset Multiview needs:
//!
//! * **Crosspoint Connect** (`0x02`): route a source to a destination on a level
//!   — the command Multiview *sends* to drive a route, and the basis of route-follow.
//! * **Crosspoint Connected / Tally** (`0x04`): the router's report that a
//!   destination is now fed from a source — the message Multiview *receives* to
//!   follow routes (drives name-following + tally).
//! * **Crosspoint Interrogate** (`0x01`): ask the router what feeds a destination.
//! * **Source/Destination name** (`0x6A`/`0x64` association): the label strings a
//!   router publishes for name-following UMD.
//!
//! Matrix/level/destination/source are carried as the protocol's
//! **multiplexed** byte pairs (a "high"/"low" nibble-packed word) so the full
//! 0..=1023 address range round-trips. The model carries decoded `u16`s and the
//! codec packs/unpacks them.
use serde::{Deserialize, Serialize};

/// Data Link Escape — frames and is byte-stuffed inside the body.
const DLE: u8 = 0x10;
/// Start of Text — opens a frame (after `DLE`).
const STX: u8 = 0x02;
/// End of Text — closes a frame (after `DLE`), before the BCC.
const ETX: u8 = 0x03;

/// A SW-P-08 codec error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SwP08Error {
    /// The frame did not open with `DLE STX`.
    #[error("frame did not start with DLE STX")]
    NoStart,
    /// The frame did not close with `DLE ETX <BCC>`.
    #[error("frame did not end with DLE ETX and a BCC")]
    NoEnd,
    /// A lone `DLE` (not `DLE DLE`, `DLE STX`, or `DLE ETX`) appeared in the body.
    #[error("unescaped DLE byte inside the frame body")]
    StrayDle,
    /// The block-check character did not validate the body.
    #[error("BCC mismatch: computed {computed:#04x}, frame carried {found:#04x}")]
    BadChecksum {
        /// The BCC this codec computed over the decoded body.
        computed: u8,
        /// The BCC byte the frame actually carried.
        found: u8,
    },
    /// The body was too short for the command it claims to be.
    #[error("truncated message body for command {command:#04x}")]
    Truncated {
        /// The command byte that began the truncated body.
        command: u8,
    },
    /// The command byte is not one this codec models.
    #[error("unsupported SW-P-08 command byte {0:#04x}")]
    UnsupportedCommand(u8),
    /// A multiplexed address exceeded the 10-bit (0..=1023) range.
    #[error("address {0} out of the 0..=1023 SW-P-08 range")]
    AddressRange(u16),
}

/// The command byte for a **Crosspoint Interrogate** message.
pub const CMD_INTERROGATE: u8 = 0x01;
/// The command byte for a **Crosspoint Connect** message.
pub const CMD_CONNECT: u8 = 0x02;
/// The command byte for a **Crosspoint Connected/Tally** message.
pub const CMD_CONNECTED: u8 = 0x04;

/// The largest address (matrix/level/dest/source) the multiplexed encoding holds.
pub const MAX_ADDRESS: u16 = 1023;

/// A SW-P-08 message Multiview models.
///
/// Internally tagged on `kind` (never `untagged`, per repo conventions) so it
/// round-trips through JSON for the REST/diagnostic surface as well as the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SwP08Message {
    /// Ask the router which source currently feeds `destination` on `level`.
    Interrogate {
        /// The router matrix index.
        matrix: u16,
        /// The signal level (e.g. video, audio group).
        level: u16,
        /// The destination (output) to interrogate.
        destination: u16,
    },
    /// Command the router to feed `destination` from `source` on `level`.
    Connect {
        /// The router matrix index.
        matrix: u16,
        /// The signal level.
        level: u16,
        /// The destination (output) to drive.
        destination: u16,
        /// The source (input) to route to it.
        source: u16,
    },
    /// The router's report that `destination` is now fed from `source` — the
    /// **route-follow** message Multiview consumes to update tally + UMD labels.
    Connected {
        /// The router matrix index.
        matrix: u16,
        /// The signal level.
        level: u16,
        /// The destination (output) whose feed changed.
        destination: u16,
        /// The source (input) now feeding it.
        source: u16,
    },
}

impl SwP08Message {
    /// The command byte this message encodes to.
    #[must_use]
    pub const fn command(&self) -> u8 {
        match self {
            Self::Interrogate { .. } => CMD_INTERROGATE,
            Self::Connect { .. } => CMD_CONNECT,
            Self::Connected { .. } => CMD_CONNECTED,
        }
    }

    /// The destination this message addresses.
    #[must_use]
    pub const fn destination(&self) -> u16 {
        match *self {
            Self::Interrogate { destination, .. }
            | Self::Connect { destination, .. }
            | Self::Connected { destination, .. } => destination,
        }
    }

    /// Encode this message into the **un-framed body** bytes (command + data).
    ///
    /// Addresses use the protocol's multiplexed (high-byte, low-byte) packing so
    /// the full 0..=1023 range survives. Use [`encode_frame`] to wrap the body in
    /// a DLE/STX frame for the wire.
    ///
    /// # Errors
    ///
    /// [`SwP08Error::AddressRange`] if any address exceeds [`MAX_ADDRESS`].
    pub fn encode_body(&self) -> Result<Vec<u8>, SwP08Error> {
        let mut out = vec![self.command()];
        match *self {
            Self::Interrogate {
                matrix,
                level,
                destination,
            } => {
                out.push(pack_matrix_level(matrix, level)?);
                push_address(&mut out, destination)?;
            }
            Self::Connect {
                matrix,
                level,
                destination,
                source,
            }
            | Self::Connected {
                matrix,
                level,
                destination,
                source,
            } => {
                out.push(pack_matrix_level(matrix, level)?);
                push_address(&mut out, destination)?;
                push_address(&mut out, source)?;
            }
        }
        Ok(out)
    }

    /// Decode a message from an **un-framed body** (command + data).
    ///
    /// # Errors
    ///
    /// * [`SwP08Error::Truncated`] if the body is too short for its command.
    /// * [`SwP08Error::UnsupportedCommand`] for an unmodelled command byte.
    pub fn decode_body(body: &[u8]) -> Result<Self, SwP08Error> {
        let (&command, rest) = body
            .split_first()
            .ok_or(SwP08Error::Truncated { command: 0 })?;
        match command {
            CMD_INTERROGATE => {
                let ml = *rest.first().ok_or(SwP08Error::Truncated { command })?;
                let (matrix, level) = unpack_matrix_level(ml);
                let destination =
                    read_address(rest.get(1..3).ok_or(SwP08Error::Truncated { command })?);
                Ok(Self::Interrogate {
                    matrix,
                    level,
                    destination,
                })
            }
            CMD_CONNECT | CMD_CONNECTED => {
                let ml = *rest.first().ok_or(SwP08Error::Truncated { command })?;
                let (matrix, level) = unpack_matrix_level(ml);
                let destination =
                    read_address(rest.get(1..3).ok_or(SwP08Error::Truncated { command })?);
                let source = read_address(rest.get(3..5).ok_or(SwP08Error::Truncated { command })?);
                if command == CMD_CONNECT {
                    Ok(Self::Connect {
                        matrix,
                        level,
                        destination,
                        source,
                    })
                } else {
                    Ok(Self::Connected {
                        matrix,
                        level,
                        destination,
                        source,
                    })
                }
            }
            other => Err(SwP08Error::UnsupportedCommand(other)),
        }
    }
}

/// Pack a matrix (0..=15) and level (0..=15) into the protocol's single
/// multiplexed byte (matrix in the high nibble, level in the low nibble).
fn pack_matrix_level(matrix: u16, level: u16) -> Result<u8, SwP08Error> {
    if matrix > 0x0F {
        return Err(SwP08Error::AddressRange(matrix));
    }
    if level > 0x0F {
        return Err(SwP08Error::AddressRange(level));
    }
    // Both operands are <= 0x0F, so the shift and OR fit in a u8.
    let matrix = u8::try_from(matrix).unwrap_or(0);
    let level = u8::try_from(level).unwrap_or(0);
    Ok((matrix << 4) | level)
}

/// Unpack a multiplexed matrix/level byte.
fn unpack_matrix_level(byte: u8) -> (u16, u16) {
    (u16::from((byte >> 4) & 0x0F), u16::from(byte & 0x0F))
}

/// Append a 10-bit address as the protocol's two-byte (high, low) word.
fn push_address(out: &mut Vec<u8>, address: u16) -> Result<(), SwP08Error> {
    if address > MAX_ADDRESS {
        return Err(SwP08Error::AddressRange(address));
    }
    // address <= 1023, so the high byte holds bits 8..=9 and the low byte 0..=7.
    let high = u8::try_from((address >> 8) & 0x03).unwrap_or(0);
    let low = u8::try_from(address & 0xFF).unwrap_or(0);
    out.push(high);
    out.push(low);
    Ok(())
}

/// Read a two-byte (high, low) address word. A short slice reads as the bytes
/// present (callers guarantee length via `get(..)` first).
fn read_address(bytes: &[u8]) -> u16 {
    let high = u16::from(bytes.first().copied().unwrap_or(0)) & 0x03;
    let low = u16::from(bytes.get(1).copied().unwrap_or(0));
    (high << 8) | low
}

/// Compute the SW-P-08 block-check character over a message body: the
/// two's-complement of the 8-bit sum, so `(sum + bcc) & 0xFF == 0`.
#[must_use]
pub fn bcc(body: &[u8]) -> u8 {
    let sum = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    sum.wrapping_neg()
}

/// Wrap a message body in a DLE/STX frame with byte-stuffing and a trailing BCC.
///
/// The BCC is computed over the **un-stuffed** body; DLE bytes in the body are
/// then doubled so the frame is self-delimiting.
#[must_use]
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let check = bcc(body);
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(DLE);
    out.push(STX);
    for &byte in body {
        if byte == DLE {
            out.push(DLE);
        }
        out.push(byte);
    }
    out.push(DLE);
    out.push(ETX);
    out.push(check);
    out
}

/// Decode one DLE/STX frame back into its message body, validating the BCC.
///
/// Returns the un-stuffed, un-framed body (command + data) on success.
///
/// # Errors
///
/// * [`SwP08Error::NoStart`] / [`SwP08Error::NoEnd`] for missing frame markers.
/// * [`SwP08Error::StrayDle`] for an unescaped `DLE` in the body.
/// * [`SwP08Error::BadChecksum`] if the BCC does not validate the body.
pub fn decode_frame(frame: &[u8]) -> Result<Vec<u8>, SwP08Error> {
    let mut iter = frame.iter().copied();
    if iter.next() != Some(DLE) || iter.next() != Some(STX) {
        return Err(SwP08Error::NoStart);
    }
    let mut body = Vec::new();
    loop {
        let byte = iter.next().ok_or(SwP08Error::NoEnd)?;
        if byte == DLE {
            match iter.next().ok_or(SwP08Error::NoEnd)? {
                // Doubled DLE -> one literal DLE in the body.
                DLE => body.push(DLE),
                // DLE ETX <BCC> closes the frame.
                ETX => {
                    let found = iter.next().ok_or(SwP08Error::NoEnd)?;
                    let computed = bcc(&body);
                    if computed != found {
                        return Err(SwP08Error::BadChecksum { computed, found });
                    }
                    return Ok(body);
                }
                _ => return Err(SwP08Error::StrayDle),
            }
        } else {
            body.push(byte);
        }
    }
}

/// Encode a message all the way to a framed, on-wire byte string.
///
/// # Errors
///
/// [`SwP08Error::AddressRange`] if any address exceeds [`MAX_ADDRESS`].
pub fn encode_message(message: &SwP08Message) -> Result<Vec<u8>, SwP08Error> {
    Ok(encode_frame(&message.encode_body()?))
}

/// Decode a framed, on-wire byte string back into a message.
///
/// # Errors
///
/// Any [`SwP08Error`] from frame validation or body decoding.
pub fn decode_message(frame: &[u8]) -> Result<SwP08Message, SwP08Error> {
    SwP08Message::decode_body(&decode_frame(frame)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        bcc, decode_frame, decode_message, encode_frame, encode_message, SwP08Error, SwP08Message,
        DLE, ETX, STX,
    };

    #[test]
    fn bcc_is_twos_complement_of_the_body_sum() {
        // (sum + bcc) & 0xFF == 0 for any body.
        for body in [
            vec![0x02u8, 0x00, 0x00, 0x05],
            vec![0xFF, 0x01],
            vec![],
            vec![0x10, 0x10, 0x10],
        ] {
            let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            let check = bcc(&body);
            assert_eq!(sum.wrapping_add(check), 0, "body {body:?}");
        }
    }

    #[test]
    fn frame_round_trips_a_plain_body() {
        let body = vec![0x02u8, 0x00, 0x00, 0x05, 0x00, 0x09];
        let frame = encode_frame(&body);
        assert_eq!(frame[0], DLE);
        assert_eq!(frame[1], STX);
        assert_eq!(decode_frame(&frame).unwrap(), body);
    }

    #[test]
    fn frame_byte_stuffs_and_unstuffs_dle_in_the_body() {
        // A body containing a literal DLE must be doubled on the wire and
        // recovered exactly on decode.
        let body = vec![0x02u8, DLE, 0x07, DLE, DLE];
        let frame = encode_frame(&body);
        // Each DLE in the body became two DLEs in the frame (plus the two
        // framing DLEs at start and the closing DLE ETX).
        let dle_count = frame
            .iter()
            .fold(0usize, |acc, &b| acc + usize::from(b == DLE));
        // 1 (start) + 3 body DLEs doubled = 6 + 1 (DLE before ETX) = 8.
        assert_eq!(dle_count, 1 + 3 * 2 + 1);
        assert_eq!(decode_frame(&frame).unwrap(), body);
    }

    #[test]
    fn decode_frame_rejects_bad_start() {
        let err = decode_frame(&[0x00, STX, 0x01, DLE, ETX, 0x00]).unwrap_err();
        assert_eq!(err, SwP08Error::NoStart);
    }

    #[test]
    fn decode_frame_rejects_a_corrupted_bcc() {
        let body = vec![0x02u8, 0x00, 0x00, 0x05, 0x00, 0x09];
        let mut frame = encode_frame(&body);
        // Corrupt the trailing BCC.
        let last = frame.len() - 1;
        frame[last] = frame[last].wrapping_add(1);
        let err = decode_frame(&frame).unwrap_err();
        assert!(matches!(err, SwP08Error::BadChecksum { .. }), "{err:?}");
    }

    #[test]
    fn decode_frame_rejects_a_stray_dle() {
        // DLE followed by a non-DLE/STX/ETX byte is a framing violation.
        let bad = vec![DLE, STX, 0x02, DLE, 0x99, DLE, ETX, 0x00];
        let err = decode_frame(&bad).unwrap_err();
        assert_eq!(err, SwP08Error::StrayDle);
    }

    #[test]
    fn connect_message_round_trips_through_the_full_wire_codec() {
        let msg = SwP08Message::Connect {
            matrix: 1,
            level: 3,
            destination: 300,
            source: 1000,
        };
        let frame = encode_message(&msg).unwrap();
        assert_eq!(decode_message(&frame).unwrap(), msg);
    }

    #[test]
    fn connected_message_round_trips_and_keeps_high_addresses() {
        // 1023 is the top of the range; its high bits (0b11) must survive the
        // multiplexed two-byte packing.
        let msg = SwP08Message::Connected {
            matrix: 0,
            level: 0,
            destination: 1023,
            source: 1023,
        };
        let frame = encode_message(&msg).unwrap();
        match decode_message(&frame).unwrap() {
            SwP08Message::Connected {
                destination,
                source,
                ..
            } => {
                assert_eq!(destination, 1023);
                assert_eq!(source, 1023);
            }
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[test]
    fn interrogate_message_round_trips() {
        let msg = SwP08Message::Interrogate {
            matrix: 2,
            level: 1,
            destination: 42,
        };
        let frame = encode_message(&msg).unwrap();
        assert_eq!(decode_message(&frame).unwrap(), msg);
    }

    #[test]
    fn encode_rejects_an_out_of_range_address() {
        let msg = SwP08Message::Connect {
            matrix: 0,
            level: 0,
            destination: 5000,
            source: 0,
        };
        let err = encode_message(&msg).unwrap_err();
        assert_eq!(err, SwP08Error::AddressRange(5000));
    }

    #[test]
    fn decode_body_rejects_a_truncated_connect() {
        // Command byte for Connect but no data following it.
        let err = SwP08Message::decode_body(&[super::CMD_CONNECT]).unwrap_err();
        assert!(matches!(err, SwP08Error::Truncated { .. }), "{err:?}");
    }

    #[test]
    fn decode_body_rejects_an_unknown_command() {
        let err = SwP08Message::decode_body(&[0xAB, 0x00]).unwrap_err();
        assert_eq!(err, SwP08Error::UnsupportedCommand(0xAB));
    }

    #[test]
    fn command_byte_matches_the_variant() {
        assert_eq!(
            SwP08Message::Interrogate {
                matrix: 0,
                level: 0,
                destination: 0
            }
            .command(),
            super::CMD_INTERROGATE
        );
        assert_eq!(
            SwP08Message::Connect {
                matrix: 0,
                level: 0,
                destination: 0,
                source: 0
            }
            .command(),
            super::CMD_CONNECT
        );
    }

    #[test]
    fn message_serialises_tagged_for_json_diagnostics() {
        let msg = SwP08Message::Connect {
            matrix: 1,
            level: 0,
            destination: 7,
            source: 9,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["kind"], "connect");
        assert_eq!(json["destination"], 7);
        let back: SwP08Message = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }
}
