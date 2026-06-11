//! Multi-Program Transport Stream (MPTS) **program selection**.
//!
//! A single transport stream may multiplex many programs (an MPTS). Multiview
//! ingests one program per tile, so given a parsed [`super::pat`] and the
//! [`super::pmt`] for a chosen program, this model resolves the concrete set of
//! PIDs to demux: the PMT PID, the PCR PID, the chosen video PID, and the chosen
//! audio PID(s). It is a **pure** decision model — the actual demux filter that
//! consumes it lives in the libav adapter.

use super::pat::Pat;
use super::pmt::{Pmt, StreamType};

/// How a program is chosen from an MPTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgramSelection {
    /// Select by explicit program number (the DVB service id).
    ByProgramNumber(u16),
    /// Select the program at the given zero-based index among the non-network
    /// PAT entries, in wire order.
    ByIndex(usize),
    /// Select the first (lowest-index) program (the common single-program case).
    First,
}

/// The concrete PID set resolved for one selected program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProgram {
    /// The selected program number.
    pub program_number: u16,
    /// The PID carrying this program's PMT.
    pub pmt_pid: u16,
    /// The PCR PID (may equal the video PID).
    pub pcr_pid: u16,
    /// The chosen video PID, if the program carries video.
    pub video_pid: Option<u16>,
    /// The video stream type, if a video PID was chosen.
    pub video_type: Option<StreamType>,
    /// The audio PIDs, in wire order.
    pub audio_pids: Vec<u16>,
    /// PIDs carrying SCTE-35 splice information for this program.
    pub scte35_pids: Vec<u16>,
}

/// Reasons program selection can fail (no panics — every failure is typed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SelectionError {
    /// The PAT carries no (non-network) programs to select from.
    #[error("transport stream carries no selectable programs")]
    NoPrograms,
    /// The requested program number was not present in the PAT.
    #[error("program number {0} not present in the PAT")]
    UnknownProgram(u16),
    /// The requested program index was out of range.
    #[error("program index {index} out of range ({count} programs)")]
    IndexOutOfRange {
        /// The index requested.
        index: usize,
        /// The number of programs available.
        count: usize,
    },
    /// The PMT supplied does not match the PAT entry for the selected program.
    #[error("pmt program_number {pmt} does not match the selected program {selected}")]
    PmtMismatch {
        /// The program number carried by the PMT.
        pmt: u16,
        /// The program number that was selected from the PAT.
        selected: u16,
    },
}

impl Pat {
    /// Resolve the selected program's program number and PMT PID from this PAT.
    ///
    /// # Errors
    ///
    /// * [`SelectionError::NoPrograms`] when the PAT has no real programs.
    /// * [`SelectionError::UnknownProgram`] / [`SelectionError::IndexOutOfRange`]
    ///   when the selection does not resolve.
    pub fn resolve(&self, selection: ProgramSelection) -> Result<(u16, u16), SelectionError> {
        let real: Vec<_> = self.programs.iter().filter(|p| !p.is_network()).collect();
        if real.is_empty() {
            return Err(SelectionError::NoPrograms);
        }
        match selection {
            ProgramSelection::ByProgramNumber(pn) => real
                .iter()
                .find(|p| p.program_number == pn)
                .map(|p| (p.program_number, p.pid))
                .ok_or(SelectionError::UnknownProgram(pn)),
            ProgramSelection::ByIndex(index) => real
                .get(index)
                .map(|p| (p.program_number, p.pid))
                .ok_or(SelectionError::IndexOutOfRange {
                    index,
                    count: real.len(),
                }),
            ProgramSelection::First => real
                .first()
                .map(|p| (p.program_number, p.pid))
                .ok_or(SelectionError::NoPrograms),
        }
    }
}

impl SelectedProgram {
    /// Build the concrete PID set for `selection` from a [`Pat`] and the matching
    /// [`Pmt`].
    ///
    /// The caller is expected to have demuxed the PMT from the PID the PAT
    /// assigned (see [`Pat::resolve`]); this validates the PMT's program number
    /// matches and then picks the video PID (first video stream) and audio PIDs.
    ///
    /// # Errors
    ///
    /// * Any [`SelectionError`] from PAT resolution.
    /// * [`SelectionError::PmtMismatch`] when `pmt`'s program number does not
    ///   match the selected program.
    pub fn resolve(
        pat: &Pat,
        pmt: &Pmt,
        selection: ProgramSelection,
    ) -> Result<Self, SelectionError> {
        let (program_number, pmt_pid) = pat.resolve(selection)?;
        if pmt.program_number != program_number {
            return Err(SelectionError::PmtMismatch {
                pmt: pmt.program_number,
                selected: program_number,
            });
        }
        let video = pmt.video_stream();
        let audio_pids = pmt.audio_streams().iter().map(|s| s.pid).collect();
        Ok(Self {
            program_number,
            pmt_pid,
            pcr_pid: pmt.pcr_pid,
            video_pid: video.map(|s| s.pid),
            video_type: video.map(|s| s.stream_type),
            audio_pids,
            scte35_pids: pmt.scte35_pids(),
        })
    }

    /// The full set of PIDs the demuxer must pass for this program (PMT, PCR,
    /// video, audio, SCTE-35), de-duplicated and sorted.
    #[must_use]
    pub fn demux_pids(&self) -> Vec<u16> {
        let mut pids = vec![self.pmt_pid, self.pcr_pid];
        if let Some(v) = self.video_pid {
            pids.push(v);
        }
        pids.extend_from_slice(&self.audio_pids);
        pids.extend_from_slice(&self.scte35_pids);
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}
