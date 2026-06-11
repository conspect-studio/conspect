//! Network Information Table parser (DVB, ETSI EN 300 468 §5.2.1).
//!
//! The NIT (`table_id` `0x40` actual / `0x41` other network, on PID `0x0010`)
//! describes the physical network: a network-level descriptor loop, then a
//! transport-stream loop where each entry carries a `transport_stream_id` /
//! `original_network_id` pair and its own descriptor loop (delivery-system,
//! service-list, etc.).

use super::descriptor::Descriptors;
use super::section::SectionHeader;
use super::MpegTsError;

/// The `table_id` of a NIT describing the actual network.
pub const TABLE_ID_ACTUAL: u8 = 0x40;

/// The `table_id` of a NIT describing another network.
pub const TABLE_ID_OTHER: u8 = 0x41;

/// Fixed bytes before the network descriptor loop (`network_descriptors_length`
/// word, the top 4 bits reserved).
const NETWORK_DESC_LEN_FIELD: usize = 2;

/// Fixed bytes for the `transport_stream_loop_length` word.
const TS_LOOP_LEN_FIELD: usize = 2;

/// Fixed per-transport-stream header (TSID + ONID + `ts_descriptors_length`
/// word).
const TS_ENTRY_HEADER: usize = 6;

/// One transport-stream entry within a NIT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportStreamInfo {
    /// The transport-stream id.
    pub transport_stream_id: u16,
    /// The original network id.
    pub original_network_id: u16,
    /// The raw per-transport-stream descriptor-loop bytes.
    pub descriptors: Vec<u8>,
}

impl TransportStreamInfo {
    /// Parse this entry's descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.descriptors)
    }
}

/// A parsed Network Information Table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nit {
    /// The network id (carried in the section's `table_id_extension`).
    pub network_id: u16,
    /// `true` for the actual network (`0x40`), `false` for another (`0x41`).
    pub actual: bool,
    /// The table version.
    pub version: u8,
    /// The raw network-level descriptor-loop bytes.
    pub network_descriptors: Vec<u8>,
    /// The transport streams described by this network, in wire order.
    pub transport_streams: Vec<TransportStreamInfo>,
}

impl Nit {
    /// Parse a NIT from a complete PSI section. Accepts either the actual
    /// (`0x40`) or other (`0x41`) `table_id`.
    ///
    /// # Errors
    ///
    /// * Any [`MpegTsError`] from header / CRC validation (the actual/other
    ///   `table_id` is auto-detected).
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

        if body.len() < NETWORK_DESC_LEN_FIELD {
            return Err(MpegTsError::TooShort {
                need: NETWORK_DESC_LEN_FIELD,
                got: body.len(),
            });
        }
        let nd_hi = *body.first().ok_or(short(0))?;
        let nd_lo = *body.get(1).ok_or(short(1))?;
        let network_descriptors_length =
            (usize::from(nd_hi & 0b0000_1111) << 8) | usize::from(nd_lo);
        let nd_start = NETWORK_DESC_LEN_FIELD;
        let nd_end = nd_start
            .checked_add(network_descriptors_length)
            .ok_or(overrun(network_descriptors_length, body.len()))?;
        let network_descriptors = body
            .get(nd_start..nd_end)
            .ok_or(overrun(nd_end, body.len()))?
            .to_vec();

        // transport_stream_loop_length word.
        let tll_start = nd_end;
        let tll_hi = *body.get(tll_start).ok_or(overrun(tll_start, body.len()))?;
        let tll_lo = *body
            .get(tll_start.saturating_add(1))
            .ok_or(overrun(tll_start, body.len()))?;
        let ts_loop_length = (usize::from(tll_hi & 0b0000_1111) << 8) | usize::from(tll_lo);

        let loop_start = tll_start
            .checked_add(TS_LOOP_LEN_FIELD)
            .ok_or(overrun(tll_start, body.len()))?;
        let loop_end = loop_start
            .checked_add(ts_loop_length)
            .ok_or(overrun(ts_loop_length, body.len()))?;
        let loop_bytes = body
            .get(loop_start..loop_end)
            .ok_or(overrun(loop_end, body.len()))?;

        let mut transport_streams = Vec::new();
        let mut offset = 0usize;
        while offset < loop_bytes.len() {
            let tsid_hi = *loop_bytes
                .get(offset)
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let tsid_lo = *loop_bytes
                .get(offset.saturating_add(1))
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let onid_hi = *loop_bytes
                .get(offset.saturating_add(2))
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let onid_lo = *loop_bytes
                .get(offset.saturating_add(3))
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let desc_len_hi = *loop_bytes
                .get(offset.saturating_add(4))
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let desc_len_lo = *loop_bytes
                .get(offset.saturating_add(5))
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let ts_desc_len =
                (usize::from(desc_len_hi & 0b0000_1111) << 8) | usize::from(desc_len_lo);

            let d_start = offset
                .checked_add(TS_ENTRY_HEADER)
                .ok_or(overrun(offset, loop_bytes.len()))?;
            let d_end = d_start
                .checked_add(ts_desc_len)
                .ok_or(overrun(ts_desc_len, loop_bytes.len()))?;
            let descriptors = loop_bytes
                .get(d_start..d_end)
                .ok_or(overrun(d_end, loop_bytes.len()))?
                .to_vec();

            transport_streams.push(TransportStreamInfo {
                transport_stream_id: (u16::from(tsid_hi) << 8) | u16::from(tsid_lo),
                original_network_id: (u16::from(onid_hi) << 8) | u16::from(onid_lo),
                descriptors,
            });
            offset = d_end;
        }

        Ok(Self {
            network_id: parsed.header.table_id_extension,
            actual,
            version: parsed.header.version,
            network_descriptors,
            transport_streams,
        })
    }

    /// Parse the network-level descriptor loop.
    ///
    /// # Errors
    ///
    /// Propagates any [`MpegTsError`] from descriptor-loop parsing.
    pub fn network_descriptors(&self) -> Result<Descriptors<'_>, MpegTsError> {
        Descriptors::parse(&self.network_descriptors)
    }
}

/// Build a `TooShort` error for a NIT body offset.
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
