use conspect_media_crypto::{
    receipt::{DecodedObservation, Receipt},
    spool::ReceiptSpool,
    Error,
};
fn private() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
fn receipt(sequence: u64, id: u8, start: u64) -> Receipt {
    Receipt {
        receipt_id: [id; 16],
        sequence,
        run_generation: 8,
        usable: true,
        observation: DecodedObservation {
            media_start_ms: start,
            media_end_ms: start + 1_000,
            observed_at_ms: start + 2_000,
            decoded_fresh_advancing: true,
            queue_age_ms: Some(1),
            bitrate_bps: Some(1),
            width: 1,
            height: 1,
            fps_milli: 1,
            jitter_ms: Some(1),
            backpressure: Some(false),
        },
    }
}
#[test]
fn restart_and_ack_keep_stable_retry_payload_and_interval_cursor() {
    let dir = private();
    let path = dir.path().join("receipts");
    let mut spool = ReceiptSpool::open(&path).unwrap();
    let first = receipt(1, 1, 10_000);
    spool.record_and_queue([1; 16], first).unwrap();
    drop(spool);
    let mut restarted = ReceiptSpool::open(&path).unwrap();
    assert_eq!(restarted.pending([1; 16], 8).unwrap(), vec![first]);
    assert_eq!(restarted.record_and_queue([1; 16], first), Ok(()));
    assert_eq!(
        restarted.record_and_queue([1; 16], receipt(2, 2, 10_000)),
        Err(Error::Replay)
    );
    restarted.acknowledge([1; 16], 8, [1; 16], 1).unwrap();
    assert!(restarted.pending([1; 16], 8).unwrap().is_empty());
    assert_eq!(
        restarted.record_and_queue([1; 16], receipt(3, 3, 10_000)),
        Err(Error::Replay)
    );
    restarted
        .record_and_queue([1; 16], receipt(3, 3, 11_000))
        .unwrap();
    assert_eq!(restarted.pending([1; 16], 8).unwrap().len(), 1);
}
#[test]
fn full_spool_rejects_new_evidence_without_advancing_cursor() {
    let dir = private();
    let path = dir.path().join("receipts");
    let mut spool = ReceiptSpool::open(&path).unwrap();
    for i in 0..64u8 {
        spool
            .record_and_queue(
                [1; 16],
                receipt(u64::from(i), i + 1, 10_000 + 1_000 * u64::from(i)),
            )
            .unwrap();
    }
    assert_eq!(
        spool.record_and_queue([1; 16], receipt(64, 65, 74_000)),
        Err(Error::Capacity)
    );
    spool.acknowledge([1; 16], 8, [1; 16], 0).unwrap();
    assert_eq!(
        spool.record_and_queue([1; 16], receipt(64, 65, 10_000)),
        Err(Error::Replay)
    );
    spool
        .record_and_queue([1; 16], receipt(64, 65, 74_000))
        .unwrap();
}
#[test]
fn crashed_stage_and_wrong_identity_fail_closed() {
    let dir = private();
    let path = dir.path().join("receipts");
    let mut spool = ReceiptSpool::open(&path).unwrap();
    std::fs::write(dir.path().join("receipts.receipt-next"), b"stage").unwrap();
    assert_eq!(
        spool.record_and_queue([1; 16], receipt(1, 1, 10_000)),
        Err(Error::Crypto)
    );
    assert!(!path.exists());
    std::fs::remove_file(dir.path().join("receipts.receipt-next")).unwrap();
    spool
        .record_and_queue([1; 16], receipt(1, 1, 10_000))
        .unwrap();
    assert!(matches!(spool.pending([2; 16], 8), Err(Error::Invalid)));
    assert!(matches!(spool.pending([1; 16], 9), Err(Error::Invalid)));
}

#[test]
fn accounting_highwater_survives_restart_and_pending_retry() {
    let dir = private();
    let path = dir.path().join("accounting");
    let mut s = ReceiptSpool::open(&path).unwrap();
    let first = receipt(1, 1, 10_000);
    s.record_and_queue([1; 16], first).unwrap();
    s.acknowledge_accounting(first, [1; 16], 10, 10, 90)
        .unwrap();
    let second = receipt(2, 2, 11_000);
    s.record_and_queue([1; 16], second).unwrap();
    drop(s);
    let mut s = ReceiptSpool::open(&path).unwrap();
    assert_eq!(s.pending([1; 16], 8).unwrap(), vec![second]);
    for (total, charged, reserved) in [(9, 0, 90), (11, 2, 89), (11, 1, 91)] {
        assert!(s
            .acknowledge_accounting(second, [1; 16], total, charged, reserved)
            .is_err());
        assert_eq!(s.pending([1; 16], 8).unwrap(), vec![second]);
    }
    s.acknowledge_accounting(second, [1; 16], 11, 1, 89)
        .unwrap();
    assert!(s.pending([1; 16], 8).unwrap().is_empty());
    drop(s);
    let mut s = ReceiptSpool::open(&path).unwrap();
    assert!(s.pending([1; 16], 8).unwrap().is_empty());
    assert!(s
        .acknowledge_accounting(second, [1; 16], 11, 0, 89)
        .is_err());
    assert!(s.record_and_queue([1; 16], second).is_err());
}
