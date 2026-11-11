use conspect_media_crypto::{journal::DurableCounterJournal, CounterReservation, Error};
fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
#[test]
fn restart_reserves_nonreused_counters_and_separates_scopes() {
    let dir = private_dir();
    let path = dir.path().join("nonces");
    let mut first = DurableCounterJournal::open(&path).unwrap();
    assert_eq!(first.reserve([1; 16], 2, [3; 16], 4), Ok(0));
    assert_eq!(first.reserve([1; 16], 2, [3; 16], 4), Ok(1));
    drop(first);
    let mut restarted = DurableCounterJournal::open(&path).unwrap();
    assert_eq!(restarted.reserve([1; 16], 2, [3; 16], 4), Ok(2));
    assert_eq!(
        restarted.reserve([1; 16], 2, [3; 16], 3),
        Err(Error::Replay)
    );
    assert_eq!(
        restarted.reserve([1; 16], 1, [3; 16], 5),
        Err(Error::Replay)
    );
    assert_eq!(restarted.reserve([2; 16], 2, [3; 16], 4), Ok(0));
    assert_eq!(restarted.reserve([1; 16], 3, [3; 16], 5), Ok(0));
    assert_eq!(
        restarted.reserve([1; 16], 2, [3; 16], 4),
        Err(Error::Replay)
    );
    assert_eq!(std::fs::metadata(path).unwrap().len(), 4 + 52 * 3);
}
#[test]
fn torn_or_unknown_journal_fails_closed_without_reusing_nonce() {
    let dir = private_dir();
    let path = dir.path().join("nonces");
    let mut journal = DurableCounterJournal::open(&path).unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Ok(0));
    std::fs::write(&path, b"NR02partial").unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Err(Error::Invalid));
    std::fs::write(&path, b"NR01").unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Err(Error::Invalid));
}
#[cfg(unix)]
#[test]
fn permissive_existing_state_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir();
    let path = dir.path().join("nonces");
    let mut journal = DurableCounterJournal::open(&path).unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Ok(0));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Err(Error::Denied));
}
#[test]
fn preexisting_staged_reservation_fails_closed() {
    let dir = private_dir();
    let path = dir.path().join("nonces");
    std::fs::write(dir.path().join("nonces.reservation-next"), b"pending").unwrap();
    let mut journal = DurableCounterJournal::open(&path).unwrap();
    assert_eq!(journal.reserve([1; 16], 2, [3; 16], 4), Err(Error::Crypto));
    assert!(!path.exists());
}
