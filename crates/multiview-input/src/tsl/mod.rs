//! TSL UMD protocol **decoders** (v3.1 / v4.0 / v5.0).
//!
//! The TSL UMD ("Under-Monitor Display") protocol family is the open de-facto
//! standard for carrying per-tile label text and tally lamps from an external
//! switcher / router / automation system into a multiviewer
//! (broadcast-multiviewer brief §2). This module hosts the **ingest** side: pure
//! byte-slice → typed [`UmdMessage`] decoders. The matching wire **encoders**
//! live in `multiview-output`'s `tsl` module.
//!
//! These are **pure codecs**: byte-in, value-out, no I/O. The UDP/TCP/serial
//! sockets that carry them are a later integration (see the crate roadmap) — by
//! keeping the codecs socket-free they stay exhaustively property-testable with
//! golden on-wire vectors and round-trip identity tests, and they never touch the
//! engine hot path.
//!
//! ## The three protocol generations
//!
//! * [`v31`] — TSL UMD **v3.1**. A fixed **18-byte** packet (one display): an
//!   address byte (sync bit + 7-bit address), a control byte (two on/off tally
//!   bits + a 2-bit brightness), and a fixed **16-char ASCII** label. Carried one
//!   packet per serial frame or one packet per UDP datagram. No framing, no
//!   checksum.
//! * [`v40`] — TSL UMD **v4.0**. Extends v3.1 with a richer control byte giving
//!   **three** 4-state colour tallies (left / text / right; `0` = off, `1` = red,
//!   `2` = green, `3` = amber) plus 2-bit brightness, a per-display **checksum**,
//!   and a screen byte. Still a fixed 16-char ASCII label. On a TCP/byte-stream
//!   transport it is wrapped in **DLE/STX** framing.
//! * [`v50`] — TSL UMD **v5.0**. Native 16-bit IP protocol: a packet header with
//!   a 16-bit byte count and screen address, then one or more **variable-length**
//!   displays, each with a 16-bit index, a 16-bit control word (L / R / Text
//!   tally + brightness + a UTF-16LE text flag) and a length-prefixed
//!   **ASCII or UTF-16LE** label. Screen / index `0xFFFF` is the broadcast
//!   address. **DLE/STX byte-stuffing** is applied on TCP/byte-stream transports
//!   only (never on UDP).
//!
//! ## Tally mapping
//!
//! Wire tally colour codes map onto [`multiview_core::tally::TallyColor`] and the
//! 2-bit brightness onto [`multiview_core::tally::Brightness`] via the
//! [`TallyLamp`] helper, so the rest of Multiview never sees raw wire codes.

pub mod v31;
pub mod v40;
pub mod v50;

use multiview_core::tally::{Brightness, TallyColor};

/// Errors raised while decoding a TSL UMD wire packet.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change. These
/// convert into [`crate::Error::Tsl`] at the crate boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TslError {
    /// The buffer was shorter than the protocol's minimum packet length.
    #[error("tsl packet too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// The minimum number of bytes the decoder required.
        need: usize,
        /// The number of bytes actually supplied.
        got: usize,
    },

    /// The buffer was longer than the protocol's maximum packet length.
    #[error("tsl packet too long: at most {max} bytes, got {got}")]
    TooLong {
        /// The maximum number of bytes the protocol permits.
        max: usize,
        /// The number of bytes actually supplied.
        got: usize,
    },

    /// A fixed-position framing/sync marker was not where the spec requires it.
    #[error("tsl framing error: {0}")]
    Framing(&'static str),

    /// A per-display or per-packet checksum did not match the computed value.
    #[error("tsl checksum mismatch: expected {expected:#04x}, computed {computed:#04x}")]
    Checksum {
        /// The checksum byte carried on the wire.
        expected: u8,
        /// The checksum the decoder computed over the packet body.
        computed: u8,
    },

    /// A length field declared more (or fewer) bytes than the buffer holds.
    #[error("tsl length mismatch: declared {declared} bytes, buffer holds {available}")]
    Length {
        /// The byte count declared by a length/count field on the wire.
        declared: usize,
        /// The number of bytes actually available after the field.
        available: usize,
    },

    /// A UTF-16LE label had an odd byte length (not a whole number of code units).
    #[error("tsl utf-16 text has odd byte length {0}")]
    OddUtf16Len(usize),
}

/// A decoded TSL UMD message: one screen's worth of one or more displays.
///
/// v3.1 and v4.0 always decode to exactly one [`UmdDisplay`]; v5.0 may carry
/// several. The `screen` address is `0` for the address-per-packet v3.1/v4.0
/// generations (which have no separate screen concept beyond the address byte)
/// and the explicit 16-bit screen for v5.0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmdMessage {
    /// The TSL protocol generation this message was decoded from.
    pub version: TslVersion,
    /// The 16-bit screen address (`0xFFFF` = broadcast in v5.0).
    pub screen: u16,
    /// The displays carried in this message (at least one).
    pub displays: Vec<UmdDisplay>,
}

/// The TSL UMD protocol generation a message was decoded from / will be encoded
/// to.
///
/// Serialised **tagged** per repo conventions (this enum is data-only, but the
/// rule is kept for consistency); `#[non_exhaustive]` so a future generation can
/// be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TslVersion {
    /// TSL UMD v3.1 (18-byte fixed packet).
    V31,
    /// TSL UMD v4.0 (per-display checksum, 4-state colour tally).
    V40,
    /// TSL UMD v5.0 (16-bit IP, variable-length displays).
    V50,
}

/// One display element: its address, three tally lamps, and label text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmdDisplay {
    /// The display index / address within its screen.
    pub index: u16,
    /// The left-hand tally lamp.
    pub left: TallyLamp,
    /// The text / centre tally lamp.
    pub text_tally: TallyLamp,
    /// The right-hand tally lamp.
    pub right: TallyLamp,
    /// The label text (already decoded from ASCII or UTF-16LE).
    pub text: String,
}

/// A single tally lamp: a [`TallyColor`] at a [`Brightness`].
///
/// This is the codec-level bridge to [`multiview_core::tally`]: wire colour codes
/// (`0..=3`) and brightness codes (`0..=3`) decode into this pair, and it encodes
/// back to those codes. v3.1's on/off tally bits map to [`TallyColor::Red`] /
/// [`TallyColor::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TallyLamp {
    /// The lamp colour.
    pub color: TallyColor,
    /// The lamp brightness.
    pub brightness: Brightness,
}

impl TallyLamp {
    /// An unlit lamp at full brightness.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            color: TallyColor::Off,
            brightness: Brightness::FULL,
        }
    }

    /// Build a lamp from a 2-bit wire colour code (`0..=3`) and a brightness.
    ///
    /// Returns [`None`] if `code` is outside `0..=3`.
    #[must_use]
    pub fn from_wire(code: u8, brightness: Brightness) -> Option<Self> {
        TallyColor::from_tsl_code(code).map(|color| Self { color, brightness })
    }

    /// The 2-bit wire colour code (`0..=3`) for this lamp's colour.
    #[must_use]
    pub const fn color_code(self) -> u8 {
        self.color.tsl_code()
    }

    /// Whether this lamp is lit (any colour other than off).
    #[must_use]
    pub const fn is_lit(self) -> bool {
        self.color.is_lit()
    }
}

impl Default for TallyLamp {
    fn default() -> Self {
        Self::off()
    }
}
