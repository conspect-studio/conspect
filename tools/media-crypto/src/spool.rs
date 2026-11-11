//! Bounded crash-safe receipt spool. Stable bytes survive retries and restarts.
//! No HTTP, bearer token, secrets, or billing live here.
use crate::{
    receipt::{DecodedObservation, Receipt},
    Error, Result,
};
use std::fs::File;
use std::path::{Path, PathBuf};
// RS05 adds durable ACK accounting; older records fail closed, never reinterpret.
const MAGIC: &[u8; 4] = b"RS05";
const RECORD: usize = 103;
const CAP: usize = 64;
const PREFIX: usize = 4 + 16 + 8 + 8 + 8 + 1 + 8 + 8;
/// Durable booking cursor and pending receipts. One spool per booking/run.
pub struct ReceiptSpool {
    path: PathBuf,
    lock: File,
    directory: crate::private_state::PrivateDir,
}
#[derive(Clone)]
struct State {
    booking: [u8; 16],
    run: u64,
    last_end: u64,
    last_observed: u64,
    items: Vec<[u8; RECORD]>,
    accounting: Option<(u64, u64)>,
}
impl ReceiptSpool {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().ok_or(Error::Invalid)?;
        let directory = crate::private_state::PrivateDir::open(parent)?;
        let mut name = path.file_name().ok_or(Error::Invalid)?.to_os_string();
        name.push(".lock");
        let lock = directory.file(Path::new(&name), true, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            lock,
            directory,
        })
    }
    /// Atomically persist the receipt and interval cursor BEFORE returning it
    /// for HTTP. `sequence` must have been reserved durably first; gaps after a
    /// crash are legal. A full spool rejects new evidence without cursor advance.
    pub fn record_and_queue(&mut self, booking: [u8; 16], receipt: Receipt) -> Result<()> {
        self.with_locked(booking, receipt.run_generation, |state| {
            let o = receipt.observation;
            if receipt.receipt_id == [0; 16]
                || o.media_start_ms >= o.media_end_ms
                || o.media_end_ms > o.observed_at_ms
                || o.media_end_ms - o.media_start_ms > 5_000
                || o.observed_at_ms - o.media_end_ms > 15_000
            {
                return Err(Error::Invalid);
            }
            let bytes = encode(booking, receipt);
            if let Some(existing) = state
                .items
                .iter()
                .find(|item| item[16..32] == bytes[16..32])
            {
                return if *existing == bytes {
                    Ok(())
                } else {
                    Err(Error::Replay)
                };
            }
            if state.items.len() >= CAP {
                return Err(Error::Capacity);
            }
            if (state.last_end != 0 && o.media_start_ms < state.last_end)
                || (state.last_observed != 0 && o.observed_at_ms < state.last_observed)
                || state.items.iter().any(|item| item[40..48] == bytes[40..48])
            {
                return Err(Error::Replay);
            }
            state.items.push(bytes);
            state.last_end = o.media_end_ms;
            state.last_observed = o.observed_at_ms;
            Ok(())
        })
    }
    /// Remove only an acknowledged receipt with exact ID/sequence. Cursor stays.
    pub fn acknowledge(
        &mut self,
        booking: [u8; 16],
        run: u64,
        id: [u8; 16],
        sequence: u64,
    ) -> Result<()> {
        self.with_locked(booking, run, |state| {
            let Some(index) = state
                .items
                .iter()
                .position(|item| item[16..32] == id && item[40..48] == sequence.to_be_bytes())
            else {
                return Err(Error::Invalid);
            };
            state.items.remove(index);
            Ok(())
        })
    }
    /// Validate server accounting and remove the exact pending receipt in one
    /// durable transaction. Restart cannot forget an acknowledged total.
    pub fn acknowledge_accounting(
        &mut self,
        receipt: Receipt,
        booking: [u8; 16],
        total: u64,
        charged: u64,
        reserved: u64,
    ) -> Result<()> {
        self.with_locked(booking, receipt.run_generation, |state| {
            if charged > total
                || state.accounting.is_some_and(|(old_total, old_reserved)| {
                    total.checked_sub(old_total) != Some(charged) || reserved > old_reserved
                })
            {
                return Err(Error::Replay);
            }
            let index = state
                .items
                .iter()
                .position(|item| *item == encode(booking, receipt))
                .ok_or(Error::Invalid)?;
            state.items.remove(index);
            state.accounting = Some((total, reserved));
            Ok(())
        })
    }
    /// Return exact stable pending payloads for retry, never regenerate IDs.
    pub fn pending(&mut self, booking: [u8; 16], run: u64) -> Result<Vec<Receipt>> {
        self.lock.lock().map_err(|_| Error::Crypto)?;
        let result = self
            .load(booking, run)
            .map(|state| state.items.iter().map(decode).collect());
        self.lock.unlock().map_err(|_| Error::Crypto)?;
        result
    }
    fn with_locked(
        &mut self,
        booking: [u8; 16],
        run: u64,
        edit: impl FnOnce(&mut State) -> Result<()>,
    ) -> Result<()> {
        self.lock.lock().map_err(|_| Error::Crypto)?;
        let result = (|| {
            let mut state = self.load(booking, run)?;
            edit(&mut state)?;
            self.store(&state)
        })();
        self.lock.unlock().map_err(|_| Error::Crypto)?;
        result
    }
    fn load(&self, booking: [u8; 16], run: u64) -> Result<State> {
        let bytes = match self
            .directory
            .read(&self.path, (PREFIX + CAP * RECORD) as u64)?
        {
            Some(bytes) => bytes,
            None => {
                return Ok(State {
                    booking,
                    run,
                    last_end: 0,
                    last_observed: 0,
                    items: Vec::new(),
                    accounting: None,
                });
            }
        };
        if bytes.len() < PREFIX
            || &bytes[..4] != MAGIC
            || bytes[4..20] != booking
            || bytes[20..28] != run.to_be_bytes()
            || !(bytes.len() - PREFIX).is_multiple_of(RECORD)
            || (bytes.len() - PREFIX) / RECORD > CAP
        {
            return Err(Error::Invalid);
        }
        let last_end = u64::from_be_bytes(bytes[28..36].try_into().map_err(|_| Error::Invalid)?);
        let last_observed =
            u64::from_be_bytes(bytes[36..44].try_into().map_err(|_| Error::Invalid)?);
        let accounting = match bytes[44] {
            0 if bytes[45..61] == [0; 16] => None,
            1 => Some((
                u64::from_be_bytes(bytes[45..53].try_into().map_err(|_| Error::Invalid)?),
                u64::from_be_bytes(bytes[53..61].try_into().map_err(|_| Error::Invalid)?),
            )),
            _ => return Err(Error::Invalid),
        };
        let (records, tail) = bytes[PREFIX..].as_chunks::<RECORD>();
        if !tail.is_empty()
            || records
                .iter()
                .any(|record| record[..16] != booking || record[32..40] != run.to_be_bytes())
        {
            return Err(Error::Invalid);
        }
        Ok(State {
            booking,
            run,
            last_end,
            last_observed,
            items: records.to_vec(),
            accounting,
        })
    }
    fn store(&self, state: &State) -> Result<()> {
        let mut bytes = Vec::with_capacity(PREFIX + state.items.len() * RECORD);
        bytes.extend(MAGIC);
        bytes.extend(state.booking);
        bytes.extend(state.run.to_be_bytes());
        bytes.extend(state.last_end.to_be_bytes());
        bytes.extend(state.last_observed.to_be_bytes());
        bytes.push(u8::from(state.accounting.is_some()));
        let (total, reserved) = state.accounting.unwrap_or((0, 0));
        bytes.extend(total.to_be_bytes());
        bytes.extend(reserved.to_be_bytes());
        for item in &state.items {
            bytes.extend(item);
        }
        let next = self.path.with_extension("receipt-next");
        self.directory
            .replace(&next, &self.path, &bytes)
            .map_err(|_| Error::Crypto)?;
        Ok(())
    }
}
fn encode(booking: [u8; 16], r: Receipt) -> [u8; RECORD] {
    let mut out = [0u8; RECORD];
    out[..16].copy_from_slice(&booking);
    out[16..32].copy_from_slice(&r.receipt_id);
    out[32..40].copy_from_slice(&r.run_generation.to_be_bytes());
    out[40..48].copy_from_slice(&r.sequence.to_be_bytes());
    let o = r.observation;
    for (index, number) in [
        (48, o.media_start_ms),
        (56, o.media_end_ms),
        (64, o.observed_at_ms),
        (72, o.bitrate_bps.unwrap_or(u64::MAX)),
    ] {
        out[index..index + 8].copy_from_slice(&number.to_be_bytes());
    }
    for (index, number) in [
        (80, o.queue_age_ms.unwrap_or(u32::MAX)),
        (84, o.width),
        (88, o.height),
        (92, o.fps_milli),
        (96, o.jitter_ms.unwrap_or(u32::MAX)),
    ] {
        out[index..index + 4].copy_from_slice(&number.to_be_bytes());
    }
    out[100] = u8::from(o.decoded_fresh_advancing);
    out[101] = o.backpressure.map(u8::from).unwrap_or(2);
    out[102] = u8::from(r.usable);
    out
}
fn decode(record: &[u8; RECORD]) -> Receipt {
    let bytes = record.as_slice();
    let arr =
        |start: usize| -> [u8; 16] { bytes[start..start + 16].try_into().expect("fixed record") };
    let u64at = |start: usize| -> u64 {
        u64::from_be_bytes(bytes[start..start + 8].try_into().expect("fixed record"))
    };
    let u32at = |start: usize| -> u32 {
        u32::from_be_bytes(bytes[start..start + 4].try_into().expect("fixed record"))
    };
    Receipt {
        receipt_id: arr(16),
        run_generation: u64at(32),
        sequence: u64at(40),
        observation: DecodedObservation {
            media_start_ms: u64at(48),
            media_end_ms: u64at(56),
            observed_at_ms: u64at(64),
            bitrate_bps: (u64at(72) != u64::MAX).then(|| u64at(72)),
            queue_age_ms: (u32at(80) != u32::MAX).then(|| u32at(80)),
            width: u32at(84),
            height: u32at(88),
            fps_milli: u32at(92),
            jitter_ms: (u32at(96) != u32::MAX).then(|| u32at(96)),
            decoded_fresh_advancing: bytes[100] == 1,
            backpressure: (bytes[101] != 2).then_some(bytes[101] == 1),
        },
        usable: bytes[102] == 1,
    }
}
