//! Typed, transport-neutral observations for exchange receipt reporting.
//! This adapter neither asserts decoding nor sends HTTP/bills a booking.
use crate::{Error, Result};

/// A fresh decoded-media observation from the receiver (never from ciphertext
/// delivery alone). The native decoder must supply measured values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedObservation {
    pub media_start_ms: u64,
    pub media_end_ms: u64,
    pub observed_at_ms: u64,
    pub decoded_fresh_advancing: bool,
    pub queue_age_ms: Option<u32>,
    pub bitrate_bps: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub fps_milli: u32,
    pub jitter_ms: Option<u32>,
    pub backpressure: Option<bool>,
}
/// Receipt interval payload. `usable` is false during held/slate/outage;
/// stationary pixels with a fresh advancing timestamp may still be usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub receipt_id: [u8; 16],
    pub sequence: u64,
    pub run_generation: u64,
    pub observation: DecodedObservation,
    pub usable: bool,
}
/// Durable sequence reservation; the implementation MUST persist the next
/// sequence before returning and bind it to booking/run generation.
pub trait ReceiptSequence {
    fn reserve(&mut self, booking: [u8; 16], run_generation: u64) -> Result<u64>;
}
/// A restart-stable sequence cursor backed by the crash-safe reservation
/// journal. This reuses its separate domain, never an actual media nonce scope.
pub struct DurableReceiptSequence(pub crate::journal::DurableCounterJournal);
impl ReceiptSequence for DurableReceiptSequence {
    fn reserve(&mut self, booking: [u8; 16], run_generation: u64) -> Result<u64> {
        use crate::CounterReservation;
        const RECEIPT_DOMAIN: [u8; 16] = *b"receipt-seq-v2!!";
        self.0.reserve(booking, run_generation, RECEIPT_DOMAIN, 0)
    }
}
/// The caller chooses an opaque receipt ID from a trusted source and persists
/// the result with `ReceiptSpool::record_and_queue` BEFORE sending it over HTTP.
/// A returned `Receipt` alone is NOT queued or billable; a crash may skip its
/// reserved sequence. The spool atomically fences overlap and stable retries.
/// A changed payload with the same ID/sequence is a server-side conflict.
pub struct ReceiptAdapter<S: ReceiptSequence> {
    booking: [u8; 16],
    run_generation: u64,
    sequence: S,
    last_end_ms: Option<u64>,
    last_observed_at_ms: Option<u64>,
}
impl<S: ReceiptSequence> ReceiptAdapter<S> {
    pub fn new(booking: [u8; 16], run_generation: u64, sequence: S) -> Self {
        Self {
            booking,
            run_generation,
            sequence,
            last_end_ms: None,
            last_observed_at_ms: None,
        }
    }
    /// Accept a bounded interval and reserve its sequence. The authority still
    /// validates org/booking/engine and performs idempotent billing reconciliation.
    pub fn record(
        &mut self,
        receipt_id: [u8; 16],
        observed: DecodedObservation,
    ) -> Result<Receipt> {
        if receipt_id == [0; 16]
            || observed.media_start_ms >= observed.media_end_ms
            || observed.media_end_ms > observed.observed_at_ms
            || observed.media_end_ms - observed.media_start_ms > 5_000
            || observed.observed_at_ms - observed.media_end_ms > 15_000
            || self
                .last_end_ms
                .is_some_and(|last| observed.media_start_ms < last)
            || self
                .last_observed_at_ms
                .is_some_and(|last| observed.observed_at_ms < last)
        {
            return Err(Error::Invalid);
        }
        let sequence = self.sequence.reserve(self.booking, self.run_generation)?;
        self.last_end_ms = Some(observed.media_end_ms);
        self.last_observed_at_ms = Some(observed.observed_at_ms);
        Ok(Receipt {
            receipt_id,
            sequence,
            run_generation: self.run_generation,
            usable: observed.decoded_fresh_advancing,
            observation: observed,
        })
    }
}
