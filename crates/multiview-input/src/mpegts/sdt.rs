//! Service Description Table parser (DVB, ETSI EN 300 468 §5.2.3).
//!
//! The SDT (`table_id` `0x42` actual TS / `0x46` other TS, on PID `0x0011`)
//! names the services in a transport stream: per service, the
//! EIT-present/schedule flags, a [`RunningStatus`], a free/scrambled flag, and a
//! descriptor loop carrying (most importantly) the `0x48` service descriptor with
//! the provider and service names.

use super::descriptor::Descriptors;
use super::section::SectionHeader;
use super::MpegTsError;

/// The `table_id` of an SDT for the actual transport stream.
pub const TABLE_ID_ACTUAL: u8 = 0x42;

/// The `table_id` of an SDT for another transport stream.
pub const TABLE_ID_OTHER: u8 = 0x46;

/// The descriptor tag of the DVB service descriptor (`0x48`).
pub const SERVICE_DESCRIPTOR_TAG: u8 = 0x48;

/// Fixed bytes before the service loop (`original_network_id` + reserved byte).
const SDT_FIXED_LEN: usize = 3;

/// Fixed per-service header (`service_id` + EIT flags byte + status/free byte +
/// `descriptors_loop_length` word).
const SERVICE_HEADER_LEN: usize = 5;

/// The DVB running status of a service (ETSI EN 300 468 Table 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunningStatus {
    /// Undefined (`0`).
    Undefined,
    /// Not running (`1`).
    NotRunning,
    /// Starts in a few seconds, e.g. for VPS (`2`).
    StartingSoon,
    /// Pausing (`3`).
    Pausing,
    /// Running (`4`).
    Running,
    /// Service off-air (`5`).
    OffAir,
    /// Reserved / unknown code (`6..=7`).
    Reserved(u8),
}

impl RunningStatus {
    /// Decode a 3-bit running-status code (`0..=7`).
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code & 0b0000_0111 {
            0 => Self::Undefined,
            1 => Self::NotRunning,
            2 => Self::StartingSoon,
            3 => Self::Pausing,
            4 => Self::Running,
            5 => Self::OffAir,
            other => Self::Reserved(other),
        }
    }
}

/// One service description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDescription {
    /// The service id (the DVB program number).
    pub service_id: u16,
    /// Whether EIT schedule information is present for this service.
    pub eit_schedule: bool,
    /// Whether EIT present/following information is present.
    pub eit_present_following: bool,
    /// The running status.
    pub running_status: RunningStatus,
    /// Whether the service is free-to-air (`true`) or CA-scrambled (`false`).
    pub free_ca_mode_free: bool,
    /// The raw per-service descriptor-loop bytes.
    pub descriptors: Vec<u8>,
}

impl ServiceDescription {
    /// Parse this service's descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.descriptors)
    }

    /// The (provider name, service name) decoded from the `0x48` service
    /// descriptor, if present. Names are read as bytes mapped to their UTF-8
    /// lossy form (DVB character-set control codes are stripped to ASCII range).
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn names(&self) -> Result<Option<(String, String)>, MpegTsError> {
        let descriptors = self.descriptors()?;
        let Some(desc) = descriptors.find(SERVICE_DESCRIPTOR_TAG) else {
            return Ok(None);
        };
        // service_descriptor: service_type(1), provider_name_length(1),
        // provider_name(N), service_name_length(1), service_name(M).
        let data = desc.data;
        let provider_len = usize::from(*data.get(1).ok_or(MpegTsError::Overrun {
            declared: 2,
            available: data.len(),
        })?);
        let provider_start = 2usize;
        let provider_end =
            provider_start
                .checked_add(provider_len)
                .ok_or(MpegTsError::Overrun {
                    declared: provider_len,
                    available: data.len(),
                })?;
        let provider = data
            .get(provider_start..provider_end)
            .ok_or(MpegTsError::Overrun {
                declared: provider_end,
                available: data.len(),
            })?;
        let name_len = usize::from(*data.get(provider_end).ok_or(MpegTsError::Overrun {
            declared: provider_end.saturating_add(1),
            available: data.len(),
        })?);
        let name_start = provider_end.checked_add(1).ok_or(MpegTsError::Overrun {
            declared: provider_end,
            available: data.len(),
        })?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(MpegTsError::Overrun {
                declared: name_len,
                available: data.len(),
            })?;
        let name = data.get(name_start..name_end).ok_or(MpegTsError::Overrun {
            declared: name_end,
            available: data.len(),
        })?;
        Ok(Some((decode_dvb_string(provider), decode_dvb_string(name))))
    }
}

/// A parsed Service Description Table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sdt {
    /// The transport-stream id (the section's `table_id_extension`).
    pub transport_stream_id: u16,
    /// `true` for the actual TS (`0x42`), `false` for another (`0x46`).
    pub actual: bool,
    /// The original network id.
    pub original_network_id: u16,
    /// The table version.
    pub version: u8,
    /// The services described, in wire order.
    pub services: Vec<ServiceDescription>,
}

impl Sdt {
    /// Parse an SDT from a complete PSI section. Accepts the actual (`0x42`) or
    /// other (`0x46`) `table_id`.
    ///
    /// # Errors
    ///
    /// * Any [`MpegTsError`] from header / CRC validation.
    /// * [`MpegTsError::Overrun`] when a length field runs past the section.
    pub fn parse(section: &[u8]) -> Result<Self, MpegTsError> {
        let table_id = *section.first().ok_or(MpegTsError::TooShort {
            need: 1,
            got: section.len(),
        })?;
        let actual = match table_id {
            TABLE_ID_ACTUAL => true,
            TABLE_ID_OTHER => false,
            other => {
                return Err(MpegTsError::WrongTable {
                    expected: TABLE_ID_ACTUAL,
                    got: other,
                })
            }
        };
        let parsed = SectionHeader::parse(section, table_id)?;
        let body = parsed.body;
        if body.len() < SDT_FIXED_LEN {
            return Err(MpegTsError::TooShort {
                need: SDT_FIXED_LEN,
                got: body.len(),
            });
        }
        let onid_hi = *body.first().ok_or(short(0))?;
        let onid_lo = *body.get(1).ok_or(short(1))?;
        let original_network_id = (u16::from(onid_hi) << 8) | u16::from(onid_lo);
        // body[2] is a reserved-future-use byte.

        let mut services = Vec::new();
        let mut offset = SDT_FIXED_LEN;
        while offset < body.len() {
            let sid_hi = *body.get(offset).ok_or(overrun(offset, body.len()))?;
            let sid_lo = *body
                .get(offset.saturating_add(1))
                .ok_or(overrun(offset, body.len()))?;
            let flags = *body
                .get(offset.saturating_add(2))
                .ok_or(overrun(offset, body.len()))?;
            let status_byte = *body
                .get(offset.saturating_add(3))
                .ok_or(overrun(offset, body.len()))?;
            let dll_lo = *body
                .get(offset.saturating_add(4))
                .ok_or(overrun(offset, body.len()))?;
            // descriptors_loop_length: top 4 bits are in status_byte, low 8 in dll_lo.
            let descriptors_loop_length =
                (usize::from(status_byte & 0b0000_1111) << 8) | usize::from(dll_lo);

            let eit_present_following = (flags & 0b0000_0001) != 0;
            let eit_schedule = (flags & 0b0000_0010) != 0;
            let running_status = RunningStatus::from_code(status_byte >> 5);
            let free_ca_mode_free = (status_byte & 0b0001_0000) == 0;

            let d_start = offset
                .checked_add(SERVICE_HEADER_LEN)
                .ok_or(overrun(offset, body.len()))?;
            let d_end = d_start
                .checked_add(descriptors_loop_length)
                .ok_or(overrun(descriptors_loop_length, body.len()))?;
            let descriptors = body
                .get(d_start..d_end)
                .ok_or(overrun(d_end, body.len()))?
                .to_vec();

            services.push(ServiceDescription {
                service_id: (u16::from(sid_hi) << 8) | u16::from(sid_lo),
                eit_schedule,
                eit_present_following,
                running_status,
                free_ca_mode_free,
                descriptors,
            });
            offset = d_end;
        }

        Ok(Self {
            transport_stream_id: parsed.header.table_id_extension,
            actual,
            original_network_id,
            version: parsed.header.version,
            services,
        })
    }
}

/// Decode a DVB text string to a Rust `String`, dropping a leading character-set
/// control byte (`0x01..=0x1F`, except `0x10`/`0x1F` multi-byte selectors whose
/// following bytes are also skipped conservatively) and mapping the remainder via
/// UTF-8 lossy. Sufficient for Latin-alphabet service names; full DVB charset
/// tables are out of scope for the pure model.
fn decode_dvb_string(bytes: &[u8]) -> String {
    let mut start = 0usize;
    if let Some(&first) = bytes.first() {
        match first {
            // 0x10 XX XX = ISO/IEC 8859 table selector (3-byte prefix).
            0x10 => start = 3,
            // 0x1F XX = encoding-type id (2-byte prefix).
            0x1F => start = 2,
            // Other control codes 0x01..=0x1F select a single alternate table.
            0x01..=0x1F => start = 1,
            _ => start = 0,
        }
    }
    let payload = bytes.get(start..).unwrap_or(&[]);
    String::from_utf8_lossy(payload).into_owned()
}

/// Build a `TooShort` error for an SDT body offset.
const fn short(offset: usize) -> MpegTsError {
    MpegTsError::TooShort {
        need: offset.saturating_add(1),
        got: offset,
    }
}

/// Build an `Overrun` error.
const fn overrun(declared: usize, available: usize) -> MpegTsError {
    MpegTsError::Overrun {
        declared,
        available,
    }
}
