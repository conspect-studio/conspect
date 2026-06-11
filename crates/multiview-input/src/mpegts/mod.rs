//! MPEG-2 Transport Stream **PSI/SI** section parsers (ISO/IEC 13818-1 + ETSI EN
//! 300 468), plus a Multi-Program Transport Stream (MPTS) program-selection
//! model.
//!
//! A transport stream multiplexes one or more programs as 188-byte packets
//! identified by a 13-bit PID. The **Program Specific Information** (PSI) and DVB
//! **Service Information** (SI) tables describe what the stream contains:
//!
//! * [`pat`] — Program Association Table (PID `0x0000`): maps each program number
//!   to the PID carrying its [`pmt`] (Program Map Table).
//! * [`pmt`] — Program Map Table: the elementary streams (video/audio/data) of one
//!   program, their stream types and PIDs, plus the PCR PID.
//! * [`nit`] — Network Information Table (DVB, ETSI EN 300 468): the physical
//!   network / transport-stream descriptors.
//! * [`sdt`] — Service Description Table (DVB): per-service name / provider /
//!   running status.
//! * [`cat`] — Conditional Access Table (PID `0x0001`): CA-system → EMM PID
//!   descriptors.
//! * [`tdt`] / [`tot`] — Time & Date / Time Offset Tables (DVB): the network UTC
//!   clock (MJD + BCD) and local-time-offset descriptors.
//!
//! Every table rides in a **PSI section** with a common header and a trailing
//! CRC-32/MPEG-2 ([`crc`]). All parsers here are **pure** byte-slice → typed
//! value codecs: no sockets, no `unsafe`, bounded allocation, and they never
//! panic on malformed input — a short, mis-CRC'd, or self-inconsistent section
//! surfaces as a typed [`MpegTsError`].
//!
//! ## Isolation (invariants #1 / #10)
//!
//! These parsers run off the output clock entirely — they decorate ingest with
//! program structure and timing metadata that feeds the last-good stores; nothing
//! here blocks or paces the engine.

pub mod cat;
pub mod crc;
pub mod descriptor;
pub mod inventory;
pub mod nit;
pub mod pat;
pub mod pmt;
pub mod sdt;
pub mod section;
pub mod selection;
pub mod tdt;
pub mod tot;

pub use cat::Cat;
pub use descriptor::{
    AudioType, Descriptor, Descriptors, IsoLanguageDescriptor, IsoLanguageEntry,
    SubtitlingDescriptor, SubtitlingEntry, TeletextDescriptor, TeletextEntry,
};
pub use inventory::{pmt_inventory, reconcile_scte35};
pub use nit::{Nit, TransportStreamInfo};
pub use pat::{Pat, ProgramAssociation};
pub use pmt::{ElementaryStream, Pmt, StreamType};
pub use sdt::{RunningStatus, Sdt, ServiceDescription};
pub use section::{SectionHeader, TableId};
pub use selection::{ProgramSelection, SelectedProgram};
pub use tdt::Tdt;
pub use tot::Tot;

/// Unified error type for the MPEG-TS PSI/SI parsers.
///
/// Each table parser shares this `#[non_exhaustive]` enum so an ingest pipeline
/// can return one type; it converts into [`crate::Error`] at the crate boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MpegTsError {
    /// The buffer was shorter than the section structure required.
    #[error("psi section too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the parser required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// The `section_syntax_indicator` / fixed bits did not match the spec, or a
    /// reserved/`'1'` bit pattern was violated.
    #[error("psi section syntax error: {0}")]
    Syntax(&'static str),

    /// The section's trailing CRC-32/MPEG-2 did not match the computed value.
    #[error("psi section crc mismatch: carried {carried:#010x}, computed {computed:#010x}")]
    Crc {
        /// The CRC carried in the section's final four bytes.
        carried: u32,
        /// The CRC computed over the section (header + payload).
        computed: u32,
    },

    /// The `table_id` did not match the table this parser decodes.
    #[error("psi table_id mismatch: expected {expected:#04x}, got {got:#04x}")]
    WrongTable {
        /// The `table_id` this parser requires.
        expected: u8,
        /// The `table_id` actually present.
        got: u8,
    },

    /// A length / loop field declared more (or fewer) bytes than the section
    /// holds.
    #[error("psi length field overruns section: declared {declared}, available {available}")]
    Overrun {
        /// Bytes declared by a length/count field on the wire.
        declared: usize,
        /// Bytes actually available after the field.
        available: usize,
    },

    /// A Modified Julian Date / BCD time value was outside the encodable range.
    #[error("dvb time/date value out of range: {0}")]
    BadDateTime(&'static str),
}

/// The PID carrying the [`pat`] (Program Association Table); fixed at `0x0000`.
pub const PAT_PID: u16 = 0x0000;

/// The PID carrying the [`cat`] (Conditional Access Table); fixed at `0x0001`.
pub const CAT_PID: u16 = 0x0001;

/// The PID carrying the DVB [`nit`] actual-network table; fixed at `0x0010`.
pub const NIT_PID: u16 = 0x0010;

/// The PID carrying the DVB [`sdt`] / BAT tables; fixed at `0x0011`.
pub const SDT_PID: u16 = 0x0011;

/// The PID carrying the DVB [`tdt`] / [`tot`] tables; fixed at `0x0014`.
pub const TDT_TOT_PID: u16 = 0x0014;
