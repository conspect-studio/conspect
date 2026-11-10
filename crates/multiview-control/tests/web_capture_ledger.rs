//! Crash-safe pending idempotency receipts must never create a second capture.
#![allow(clippy::unwrap_used)]
use multiview_control::web_capture_store::{Ledger, Reserve, LEDGER_CAPACITY};
#[test]
fn durable_pending_receipt_survives_restart_and_refuses_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    {
        let ledger = Ledger::durable(&file).unwrap();
        assert_eq!(
            ledger.reserve("actor", "submit", "key", "body"),
            Ok(Reserve::Fresh)
        );
    }
    let recovered = Ledger::durable(&file).unwrap();
    assert_eq!(
        recovered.reserve("actor", "submit", "key", "body"),
        Err(false)
    );
}
#[test]
fn full_ledger_never_evicts_an_inflight_capture() {
    let ledger = Ledger::memory();
    for n in 0..LEDGER_CAPACITY {
        assert_eq!(
            ledger.reserve("actor", "submit", &format!("k{n}"), "body"),
            Ok(Reserve::Fresh)
        );
    }
    assert_eq!(
        ledger.reserve("actor", "submit", "excess", "body"),
        Err(false)
    );
    assert_eq!(ledger.reserve("actor", "submit", "k0", "body"), Err(false));
    assert!(ledger
        .complete("actor", "submit", "not-reserved", 42)
        .is_err());
}
