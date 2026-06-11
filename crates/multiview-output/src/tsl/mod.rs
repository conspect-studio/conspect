//! TSL UMD protocol **encoders** (v3.1 / v4.0 / v5.0).
//!
//! This is the **egress** half of Multiview's TSL UMD support: pure typed
//! [`UmdMessage`] → on-wire bytes. The matching wire **decoders** live in
//! `multiview-input`'s `tsl` module. The two halves share an identical value model
//! (defined independently in each crate — `multiview-input` and `multiview-output` do
//! not, and must not, depend on one another) so that an encode∘decode round trip
//! is the identity for any representable message.
//!
//! Encoders are **pure**: value-in, bytes-out, no I/O. The UDP/TCP/serial
//! sockets that carry them are a later integration (the codecs are socket-free so
//! they stay golden-vector and round-trip testable, and they never touch the
//! engine hot path).
//!
//! See the [`v31`], [`v40`] and [`v50`] modules for the per-generation wire
//! layouts (they mirror the decoder docs in `multiview-input::tsl`). Tally is taken
//! from [`multiview_core::tally`] via the [`TallyLamp`] helper and written back to
//! the 2-bit wire codes.

pub mod v31;
pub mod v40;
pub mod v50;

use multiview_core::tally::{Brightness, TallyColor};

/// Errors raised while encoding a TSL UMD message to wire bytes.
///
/// Marked `#[non_exhaustive]`. These convert into [`crate::Error::Output`] at the
/// crate boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TslError {
    /// A label was longer than the fixed field the protocol generation allows
    /// (v3.1 / v4.0 are fixed at 16 ASCII characters).
    #[error("tsl label too long: {len} chars exceeds the {max}-char field")]
    LabelTooLong {
        /// The supplied label length, in characters.
        len: usize,
        /// The maximum the field can hold.
        max: usize,
    },

    /// A label contained a character the chosen encoding cannot represent
    /// (e.g. a non-ASCII char for an ASCII-only generation/flag).
    #[error("tsl label not representable in {encoding}: {ch:?}")]
    NonRepresentable {
        /// The encoding that rejected the character.
        encoding: &'static str,
        /// The offending character.
        ch: char,
    },

    /// The message had no displays, or more than the protocol/size ceiling
    /// allows.
    #[error("tsl display count {count} is invalid (must be 1..={max})")]
    DisplayCount {
        /// The number of displays supplied.
        count: usize,
        /// The maximum the protocol/size ceiling allows.
        max: usize,
    },

    /// The assembled packet exceeded the protocol's maximum size.
    #[error("tsl packet too long: {len} bytes exceeds the {max}-byte ceiling")]
    PacketTooLong {
        /// The assembled packet length, in bytes.
        len: usize,
        /// The maximum the protocol permits.
        max: usize,
    },
}

/// A TSL UMD message to encode: one screen's worth of one or more displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmdMessage {
    /// The TSL protocol generation to encode to.
    pub version: TslVersion,
    /// The 16-bit screen address (`0xFFFF` = broadcast in v5.0; ignored by
    /// v3.1/v4.0, which address per display).
    pub screen: u16,
    /// The displays to encode (at least one).
    pub displays: Vec<UmdDisplay>,
}

/// The TSL UMD protocol generation to encode to.
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
    /// The label text (encoded to ASCII or UTF-16LE on the wire).
    pub text: String,
}

/// A single tally lamp: a [`TallyColor`] at a [`Brightness`].
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
