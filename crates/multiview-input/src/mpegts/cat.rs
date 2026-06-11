//! Conditional Access Table parser (ISO/IEC 13818-1 §2.4.4.6).
//!
//! The CAT (`table_id` `0x01`, on PID `0x0001`) is a single descriptor loop of
//! CA descriptors (`tag 0x09`) that map a `CA_system_id` to the PID carrying its
//! Entitlement Management Messages (EMMs). It is the entry point for descrambling
//! the whole transport stream.

use super::descriptor::Descriptors;
use super::section::SectionHeader;
use super::MpegTsError;

/// The `table_id` of a Conditional Access Table.
pub const TABLE_ID: u8 = 0x01;

/// The descriptor tag of a CA descriptor (`0x09`).
pub const CA_DESCRIPTOR_TAG: u8 = 0x09;

/// One CA-system → EMM PID mapping extracted from a CA descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaSystem {
    /// The `CA_system_id` identifying the conditional-access provider.
    pub ca_system_id: u16,
    /// The PID carrying this system's EMMs.
    pub emm_pid: u16,
}

/// A parsed Conditional Access Table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cat {
    /// The table version.
    pub version: u8,
    /// `current_next_indicator`.
    pub current: bool,
    /// The raw descriptor-loop bytes (CA descriptors).
    pub descriptors: Vec<u8>,
}

impl Cat {
    /// Parse a CAT from a complete PSI section (header + descriptor loop + CRC).
    ///
    /// # Errors
    ///
    /// Any [`MpegTsError`] from header / CRC validation.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        let parsed = SectionHeader::parse(section, TABLE_ID)?;
        Ok(Self {
            version: parsed.header.version,
            current: parsed.header.current,
            descriptors: parsed.body.to_vec(),
        })
    }

    /// Parse the CA descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.descriptors)
    }

    /// Extract the CA-system → EMM-PID mappings from the CA descriptors.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn ca_systems(&self) -> Result<Vec<CaSystem>, MpegTsError> {
        let descriptors = self.descriptors()?;
        let mut out = Vec::new();
        for desc in descriptors.as_slice() {
            if desc.tag != CA_DESCRIPTOR_TAG {
                continue;
            }
            // CA descriptor: CA_system_id(2), reserved(3) + CA_PID(13), then
            // private data.
            let data = desc.data;
            let Some(&sid_hi) = data.first() else {
                continue;
            };
            let Some(&sid_lo) = data.get(1) else {
                continue;
            };
            let Some(&pid_hi) = data.get(2) else {
                continue;
            };
            let Some(&pid_lo) = data.get(3) else {
                continue;
            };
            out.push(CaSystem {
                ca_system_id: (u16::from(sid_hi) << 8) | u16::from(sid_lo),
                emm_pid: (u16::from(pid_hi & 0b0001_1111) << 8) | u16::from(pid_lo),
            });
        }
        Ok(out)
    }
}
