//! **SCTE-35** splice-information and **SCTE-104 / SMPTE ST 2010** ad-cue parsers
//! (pure), emitting typed [`CueEvent`]s.
//!
//! Broadcast streams carry ad-insertion and program-boundary markers as cue
//! messages. Multiview ingests them so the monitoring / overlay subsystems can show
//! "SCTE cue" indicators and the management plane can drive scheduled automation
//! (broadcast-multiviewer brief §5, §8). Two on-wire forms are parsed here:
//!
//! * [`splice35`] — the **SCTE-35** `splice_info_section` (ANSI/SCTE 35), carried
//!   in an MPEG-TS PID of stream-type `0x86` (or in ST 2110-40 ANC). It defines
//!   the `splice_insert`, `time_signal`, and segmentation-descriptor commands
//!   with `pts_time` (33-bit) and break durations.
//! * [`scte104`] — the **SCTE-104** message (ANSI/SCTE 104), the operational
//!   trigger an automation system sends to an encoder, carried over the network
//!   in SMPTE **ST 2010** and into VANC via **ST 2031**. Its `splice_request_data`
//!   maps onto the same [`CueEvent`] vocabulary as SCTE-35.
//!
//! Both are **pure** byte-slice → value codecs: no sockets, bounded allocation,
//! panic-free. A malformed message surfaces as a typed [`ScteError`].

pub mod decode;
pub mod scte104;
pub mod splice35;

pub use scte104::Scte104Message;
pub use splice35::{
    NormalizedCueEvent, SegmentationDescriptor, SegmentationUpid, SpliceCommand, SpliceInfoSection,
    SpliceInsert, TimeSignal,
};

/// Errors raised while parsing a SCTE-35 / SCTE-104 cue message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScteError {
    /// The buffer was shorter than the message structure required.
    #[error("scte message too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the parser required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// A fixed/reserved field did not hold the value the spec mandates.
    #[error("scte syntax error: {0}")]
    Syntax(&'static str),

    /// The SCTE-35 section's trailing CRC-32 did not validate.
    #[error("scte-35 crc mismatch: carried {carried:#010x}, computed {computed:#010x}")]
    Crc {
        /// The CRC carried in the section.
        carried: u32,
        /// The CRC computed over the section body.
        computed: u32,
    },

    /// A length / count field declared more (or fewer) bytes than the buffer
    /// holds.
    #[error("scte length overruns message: declared {declared}, available {available}")]
    Overrun {
        /// Bytes declared by a length/count field.
        declared: usize,
        /// Bytes actually available.
        available: usize,
    },

    /// An unsupported / reserved splice command or message op-id was encountered.
    #[error("scte unsupported command/op: {0:#04x}")]
    Unsupported(u16),
}

/// The kind of program/break boundary a cue marks, normalised across SCTE-35 and
/// SCTE-104.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CueKind {
    /// The start of an advertising / splice-out break (return to the break feed).
    SpliceOut,
    /// The end of an advertising / splice-in break (return to the program feed).
    SpliceIn,
    /// A bare timing signal with no immediate in/out semantics (a `time_signal`
    /// carrying segmentation descriptors, or an immediate splice with no
    /// out-of-network flag).
    TimeSignal,
}

/// A normalised cue event emitted by either parser.
///
/// This is the vocabulary the rest of Multiview consumes: the realtime API surfaces
/// it as an event and the overlay subsystem renders a "SCTE cue" indicator. It is
/// derived from the wire message, never the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueEvent {
    /// What boundary this cue marks.
    pub kind: CueKind,
    /// The unique splice event id (SCTE-35 `splice_event_id`, SCTE-104 event id).
    pub event_id: u32,
    /// The splice presentation time on the program 90 kHz clock, if specified
    /// (`None` for an *immediate* splice, which applies at the current point).
    pub pts_time_90k: Option<u64>,
    /// The break duration in 90 kHz ticks, if the message carried one.
    pub break_duration_90k: Option<u64>,
    /// Whether the cue is a cancellation of a previously-signalled event.
    pub cancel: bool,
}

impl CueEvent {
    /// The break duration in nanoseconds, if present (90 kHz → ns, exact integer
    /// rational `1_000_000_000 / 90_000` reduced to `100_000 / 9`).
    #[must_use]
    pub fn break_duration_ns(self) -> Option<i64> {
        self.break_duration_90k.and_then(ticks_90k_to_ns)
    }

    /// The presentation time in nanoseconds, if present.
    #[must_use]
    pub fn pts_time_ns(self) -> Option<i64> {
        self.pts_time_90k.and_then(ticks_90k_to_ns)
    }
}

/// Convert a 90 kHz tick count to nanoseconds with exact integer arithmetic
/// (`ns = ticks * 100_000 / 9`), returning [`None`] on overflow.
fn ticks_90k_to_ns(ticks: u64) -> Option<i64> {
    let ns = u128::from(ticks).checked_mul(100_000)?.checked_div(9)?;
    i64::try_from(ns).ok()
}

/// The standard SCTE-35 2023r1 Table 23 `segmentation_type_id` name, or
/// [`None`] for a reserved/unassigned id. Names match the ANSI/SCTE 35 table
/// verbatim so an operator can cross-reference the spec directly.
#[must_use]
pub fn segmentation_type_name(type_id: u8) -> Option<&'static str> {
    Some(match type_id {
        0x10 => "Program Start",
        0x11 => "Program End",
        0x12 => "Program Early Termination",
        0x13 => "Program Breakaway",
        0x14 => "Program Resumption",
        0x17 => "Program Overlap Start",
        0x18 => "Program Blackout Override",
        0x19 => "Program Start - In Progress",
        0x20 => "Chapter Start",
        0x21 => "Chapter End",
        0x22 => "Break Start",
        0x23 => "Break End",
        0x24 => "Opening Credit Start",
        0x25 => "Opening Credit End",
        0x26 => "Closing Credit Start",
        0x27 => "Closing Credit End",
        0x30 => "Provider Advertisement Start",
        0x31 => "Provider Advertisement End",
        0x32 => "Distributor Advertisement Start",
        0x33 => "Distributor Advertisement End",
        0x34 => "Provider Placement Opportunity Start",
        0x35 => "Provider Placement Opportunity End",
        0x36 => "Distributor Placement Opportunity Start",
        0x37 => "Distributor Placement Opportunity End",
        0x38 => "Provider Overlay Placement Opportunity Start",
        0x39 => "Provider Overlay Placement Opportunity End",
        0x3A => "Distributor Overlay Placement Opportunity Start",
        0x3B => "Distributor Overlay Placement Opportunity End",
        0x3C => "Provider Promo Start",
        0x3D => "Provider Promo End",
        0x3E => "Distributor Promo Start",
        0x3F => "Distributor Promo End",
        0x40 => "Unscheduled Event Start",
        0x41 => "Unscheduled Event End",
        0x42 => "Alternate Content Opportunity Start",
        0x43 => "Alternate Content Opportunity End",
        0x44 => "Provider Ad Block Start",
        0x45 => "Provider Ad Block End",
        0x46 => "Distributor Ad Block Start",
        0x47 => "Distributor Ad Block End",
        0x50 => "Network Start",
        0x51 => "Network End",
        _ => return None,
    })
}

/// The standard SCTE-35 2023r1 Table 22 `segmentation_upid_type` name, for the
/// readable-identity types this adapter surfaces as a short label suffix.
/// Returns [`None`] for a type that is not a human-readable identity (e.g.
/// binary EIDR, UUID) or is unassigned.
#[must_use]
fn segmentation_upid_type_name(upid_type: u8) -> Option<&'static str> {
    Some(match upid_type {
        0x03 => "Ad-ID",
        0x08 => "TI",
        0x09 => "ADI",
        0x0C => "MPU",
        0x0E => "ADS",
        0x0F => "URI",
        _ => return None,
    })
}

/// A short, printable suffix derived from a readable `segmentation_upid`
/// (ADI, URI, TI, ADS/MPU, `AiringID`), bounded to 48 bytes including the type
/// prefix. Returns [`None`] when the UPID type is not human-readable or the
/// bytes are not printable text (binary identities stay internal, never
/// mis-rendered as text).
#[must_use]
pub fn segmentation_upid_label(upid: &splice35::SegmentationUpid) -> Option<String> {
    let kind = segmentation_upid_type_name(upid.upid_type)?;
    if upid.value.is_empty() || !upid.value.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return None;
    }
    let text = std::str::from_utf8(&upid.value).ok()?;
    let mut label = format!("{kind}:{text}");
    label.truncate(48);
    Some(label)
}
