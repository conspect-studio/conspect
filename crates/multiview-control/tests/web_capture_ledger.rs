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

fn fill(ledger: &Ledger, n: usize) {
    for i in 0..n {
        assert_eq!(
            ledger.reserve("a", "submit", &format!("k{i}"), "b"),
            Ok(Reserve::Fresh)
        );
    }
}

#[test]
fn startup_fails_when_pending_exceeds_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    {
        let l = Ledger::durable(&file).unwrap();
        fill(&l, LEDGER_CAPACITY);
    }
    // Append a further pending record directly, as a crashed older build could.
    let rec = r#"{"actor":"a","op":"submit","key":"extra","digest":"b","value":null}"#;
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(rec);
    text.push('\n');
    std::fs::write(&file, text).unwrap();
    assert!(Ledger::durable(&file).is_err());
}

#[test]
fn tombstone_survives_restart_and_frees_slot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    {
        let l = Ledger::durable(&file).unwrap();
        fill(&l, LEDGER_CAPACITY);
        assert_eq!(l.reserve("a", "submit", "over", "b"), Err(false));
        l.abandon("a", "submit", "k0").unwrap();
        assert_eq!(l.reserve("a", "submit", "over", "b"), Ok(Reserve::Fresh));
    }
    let r = Ledger::durable(&file).unwrap();
    // k0 is released (fresh again once a slot frees); "over" is still pending.
    assert_eq!(r.reserve("a", "submit", "over", "b"), Err(false));
    r.abandon("a", "submit", "over").unwrap();
    assert_eq!(r.reserve("a", "submit", "k0", "b"), Ok(Reserve::Fresh));
}

#[test]
fn repeated_definite_failures_never_consume_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let l = Ledger::durable(&dir.path().join("ledger")).unwrap();
    for i in 0..(LEDGER_CAPACITY * 3) {
        let k = format!("f{i}");
        assert_eq!(l.reserve("a", "submit", &k, "b"), Ok(Reserve::Fresh));
        l.abandon("a", "submit", &k).unwrap();
    }
    assert_eq!(l.reserve("a", "submit", "ok", "b"), Ok(Reserve::Fresh));
}

#[test]
fn uncertain_pending_is_not_released_without_abandon() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    let l = Ledger::durable(&file).unwrap();
    assert_eq!(l.reserve("a", "submit", "u", "b"), Ok(Reserve::Fresh));
    drop(l);
    let r = Ledger::durable(&file).unwrap();
    assert_eq!(r.reserve("a", "submit", "u", "b"), Err(false));
    r.complete("a", "submit", "u", 7).unwrap();
    drop(r);
    let r = Ledger::durable(&file).unwrap();
    assert_eq!(r.reserve("a", "submit", "u", "b"), Ok(Reserve::Replay(7)));
}

#[test]
fn corrupt_journal_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    std::fs::write(&file, "not json\n").unwrap();
    assert!(Ledger::durable(&file).is_err());
}

#[test]
fn oversize_journal_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ledger");
    std::fs::write(&file, vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
    assert!(Ledger::durable(&file).is_err());
}
