use conspect_media_crypto::{
    journal::DurableCounterJournal,
    receipt::{DecodedObservation, DurableReceiptSequence, ReceiptAdapter, ReceiptSequence},
    Error, Result,
};
use std::{cell::RefCell, rc::Rc};
struct Shared(Rc<RefCell<u64>>);
impl ReceiptSequence for Shared {
    fn reserve(&mut self, booking: [u8; 16], run_generation: u64) -> Result<u64> {
        if booking != [1; 16] || run_generation != 8 {
            return Err(Error::Denied);
        }
        let mut next = self.0.borrow_mut();
        let current = *next;
        *next = next.checked_add(1).ok_or(Error::Capacity)?;
        Ok(current)
    }
}
fn observation() -> DecodedObservation {
    DecodedObservation {
        media_start_ms: 10_000,
        media_end_ms: 11_000,
        observed_at_ms: 12_000,
        decoded_fresh_advancing: true,
        queue_age_ms: Some(100),
        bitrate_bps: Some(2_000_000),
        width: 1920,
        height: 1080,
        fps_milli: 29_970,
        jitter_ms: Some(5),
        backpressure: Some(false),
    }
}
#[test]
fn decoded_pts_not_pixels_controls_usable_and_sequence_survives_recreate() {
    let cursor = Rc::new(RefCell::new(1));
    let mut adapter = ReceiptAdapter::new([1; 16], 8, Shared(cursor.clone()));
    let fresh = adapter.record([9; 16], observation()).unwrap();
    assert_eq!(fresh.sequence, 1);
    assert!(
        fresh.usable,
        "stationary pixels may have advancing decoded PTS"
    );
    let mut held = observation();
    held.media_start_ms = 11_000;
    held.media_end_ms = 12_000;
    held.observed_at_ms = 13_000;
    held.decoded_fresh_advancing = false;
    let stalled = adapter.record([10; 16], held).unwrap();
    assert!(!stalled.usable);
    assert_eq!(stalled.sequence, 2);
    let mut restart = ReceiptAdapter::new([1; 16], 8, Shared(cursor));
    assert_eq!(restart.record([11; 16], observation()).unwrap().sequence, 3);
}
#[test]
fn persistent_receipt_sequence_survives_restart_and_fences_run_generation() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.path().join("receipt-counters");
    let mut first = ReceiptAdapter::new(
        [1; 16],
        8,
        DurableReceiptSequence(DurableCounterJournal::open(&path).unwrap()),
    );
    assert_eq!(first.record([9; 16], observation()).unwrap().sequence, 0);
    drop(first);
    let mut next = ReceiptAdapter::new(
        [1; 16],
        8,
        DurableReceiptSequence(DurableCounterJournal::open(&path).unwrap()),
    );
    assert_eq!(next.record([10; 16], observation()).unwrap().sequence, 1);
    let mut newer = ReceiptAdapter::new(
        [1; 16],
        9,
        DurableReceiptSequence(DurableCounterJournal::open(&path).unwrap()),
    );
    assert_eq!(newer.record([11; 16], observation()).unwrap().sequence, 0);
    let mut stale = ReceiptAdapter::new(
        [1; 16],
        8,
        DurableReceiptSequence(DurableCounterJournal::open(&path).unwrap()),
    );
    assert_eq!(stale.record([12; 16], observation()), Err(Error::Replay));
}
#[test]
fn rejects_overlap_future_stale_long_intervals_and_wrong_booking() {
    let cursor = Rc::new(RefCell::new(0));
    let mut adapter = ReceiptAdapter::new([1; 16], 8, Shared(cursor.clone()));
    for invalid in [
        DecodedObservation {
            media_end_ms: 10_000,
            ..observation()
        },
        DecodedObservation {
            media_end_ms: 9_999,
            ..observation()
        },
        DecodedObservation {
            media_end_ms: 16_000,
            observed_at_ms: 16_000,
            ..observation()
        },
        DecodedObservation {
            observed_at_ms: 26_001,
            ..observation()
        },
    ] {
        assert_eq!(adapter.record([9; 16], invalid), Err(Error::Invalid));
    }
    assert_eq!(adapter.record([0; 16], observation()), Err(Error::Invalid));
    assert_eq!(
        *cursor.borrow(),
        0,
        "invalid receipt never consumes sequence"
    );
    adapter.record([9; 16], observation()).unwrap();
    assert_eq!(adapter.record([10; 16], observation()), Err(Error::Invalid));
    assert_eq!(*cursor.borrow(), 1);
    let mut wrong = ReceiptAdapter::new([2; 16], 8, Shared(cursor));
    assert_eq!(wrong.record([8; 16], observation()), Err(Error::Denied));
}
