//! Program Association Table parser (ISO/IEC 13818-1 §2.4.4.3).
//!
//! The PAT (always on PID `0x0000`, `table_id` `0x00`) is the root of the PSI
//! tree: it lists every program in the transport stream and, for each, the PID
//! carrying that program's [`super::pmt`]. Program number `0` is special — its
//! "PMT PID" actually points at the DVB Network Information Table.

use super::section::SectionHeader;
use super::MpegTsError;

/// The `table_id` of a Program Association Table.
pub const TABLE_ID: u8 = 0x00;

/// Bytes per program-association loop entry (`program_number` + PID word).
const ENTRY_LEN: usize = 4;

/// The reserved "network PID" program number whose mapping points at the NIT.
pub const NETWORK_PROGRAM_NUMBER: u16 = 0x0000;

/// One PAT entry: a program number and the PID where its definition lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramAssociation {
    /// The program number (`0` is the reserved network-PID mapping).
    pub program_number: u16,
    /// The PID of this program's PMT, or — when `program_number == 0` — the
    /// Network Information Table PID.
    pub pid: u16,
}

impl ProgramAssociation {
    /// Whether this entry is the reserved network-PID mapping (program `0`).
    #[must_use]
    pub const fn is_network(self) -> bool {
        self.program_number == NETWORK_PROGRAM_NUMBER
    }
}

/// A parsed Program Association Table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pat {
    /// The transport-stream id (carried in the section's `table_id_extension`).
    pub transport_stream_id: u16,
    /// The table version (`0..=31`).
    pub version: u8,
    /// `current_next_indicator`.
    pub current: bool,
    /// The program → PID associations, in wire order.
    pub programs: Vec<ProgramAssociation>,
}

impl Pat {
    /// Parse a PAT from a complete PSI section (header + body + CRC).
    ///
    /// # Errors
    ///
    /// * Any [`MpegTsError`] from header / CRC validation.
    /// * [`MpegTsError::Overrun`] when the program loop is not a whole number of
    ///   4-byte entries.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        let parsed = SectionHeader::parse(section, TABLE_ID)?;
        let body = parsed.body;
        if body.len() % ENTRY_LEN != 0 {
            return Err(MpegTsError::Overrun {
                declared: body.len(),
                available: body.len().saturating_sub(body.len() % ENTRY_LEN),
            });
        }
        let mut programs = Vec::with_capacity(body.len() / ENTRY_LEN);
        let mut offset = 0usize;
        while offset < body.len() {
            let pn_hi = *body.get(offset).ok_or(short(offset))?;
            let pn_lo = *body.get(offset.saturating_add(1)).ok_or(short(offset))?;
            let pid_hi = *body.get(offset.saturating_add(2)).ok_or(short(offset))?;
            let pid_lo = *body.get(offset.saturating_add(3)).ok_or(short(offset))?;
            let program_number = (u16::from(pn_hi) << 8) | u16::from(pn_lo);
            // Top 3 bits of the PID word are reserved '1' bits.
            let pid = (u16::from(pid_hi & 0b0001_1111) << 8) | u16::from(pid_lo);
            programs.push(ProgramAssociation {
                program_number,
                pid,
            });
            offset = offset.saturating_add(ENTRY_LEN);
        }
        Ok(Self {
            transport_stream_id: parsed.header.table_id_extension,
            version: parsed.header.version,
            current: parsed.header.current,
            programs,
        })
    }

    /// The PMT PID for a given program number, if the program is present and is
    /// not the reserved network mapping.
    #[must_use]
    pub fn pmt_pid(&self, program_number: u16) -> Option<u16> {
        self.programs
            .iter()
            .find(|p| p.program_number == program_number && !p.is_network())
            .map(|p| p.pid)
    }

    /// The Network Information Table PID, if the PAT carries the program-`0`
    /// mapping.
    #[must_use]
    pub fn network_pid(&self) -> Option<u16> {
        self.programs.iter().find(|p| p.is_network()).map(|p| p.pid)
    }

    /// The non-network program count (the number of real programs in the TS).
    #[must_use]
    pub fn program_count(&self) -> usize {
        self.programs.iter().filter(|p| !p.is_network()).count()
    }
}

/// Build a `TooShort` error for a PAT body offset.
const fn short(offset: usize) -> MpegTsError {
    MpegTsError::TooShort {
        need: offset.saturating_add(ENTRY_LEN),
        got: offset,
    }
}
