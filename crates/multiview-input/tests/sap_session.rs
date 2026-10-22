//! SAP **discovered-session table** tests (RFC 2974 lifecycle; ADR-0041 §3/§4/§8,
//! brief §3/§9).
//!
//! The table is a bounded, fixed-capacity, drop-oldest inventory of **untrusted**
//! discovered sessions keyed on `(msg-id hash, originating source)`. It records
//! announcements, refreshes on re-announcement (learning the observed period),
//! **ignores inbound `T=1` deletions** against tracked sessions (a spoof/hijack
//! vector), bounds one origin's share, drops the oldest past capacity (never
//! grows — inv #10), and implicitly purges sessions unseen for
//! `max(10 × observed-period, 1 h)`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;
use std::time::Duration;

use multiview_input::sap::packet::{SapMessageType, SapPacket};
use multiview_input::sap::session::{ObserveOutcome, SapSessionTable, SessionKey};
use proptest::prelude::*;

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn hash(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn announce(h: u16, origin: IpAddr, sdp: &str) -> SapPacket {
    SapPacket {
        message_type: SapMessageType::Announcement,
        msg_id_hash: hash(h),
        origin,
        payload_type: None,
        payload: sdp.as_bytes().to_vec(),
    }
}

fn delete(h: u16, origin: IpAddr) -> SapPacket {
    SapPacket {
        message_type: SapMessageType::Deletion,
        msg_id_hash: hash(h),
        origin,
        payload_type: None,
        payload: Vec::new(),
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn announcement_records_a_discovered_session() {
    let table = SapSessionTable::new();
    let out = table.observe(&announce(1, v4(239, 255, 0, 1), "v=0 alpha"), secs(0));
    assert_eq!(out, ObserveOutcome::Inserted);
    let inv = table.inventory();
    assert_eq!(inv.len(), 1);
    let s = &inv[0];
    assert_eq!(
        s.key,
        SessionKey {
            msg_id_hash: hash(1),
            origin: v4(239, 255, 0, 1)
        }
    );
    assert_eq!(s.sdp, b"v=0 alpha".to_vec());
    assert_eq!(s.announcements, 1);
    assert_eq!(s.observed_period, None, "no period until a re-announcement");
    assert_eq!(s.first_seen, secs(0));
    assert_eq!(s.last_seen, secs(0));
}

#[test]
fn re_announcement_refreshes_and_records_the_observed_period() {
    let table = SapSessionTable::new();
    let o = v4(239, 255, 0, 2);
    table.observe(&announce(7, o, "v=0 first"), secs(0));
    let out = table.observe(&announce(7, o, "v=0 second"), secs(30));
    assert_eq!(out, ObserveOutcome::Refreshed);
    let inv = table.inventory();
    assert_eq!(inv.len(), 1, "same (hash,origin) is one session, not two");
    let s = &inv[0];
    assert_eq!(s.announcements, 2);
    assert_eq!(s.observed_period, Some(secs(30)), "period = inter-arrival");
    assert_eq!(s.last_seen, secs(30));
    assert_eq!(s.first_seen, secs(0), "first_seen is stable across refresh");
    assert_eq!(s.sdp, b"v=0 second".to_vec(), "content is refreshed");
}

#[test]
fn distinct_hash_or_distinct_origin_are_distinct_sessions() {
    let table = SapSessionTable::new();
    table.observe(&announce(1, v4(239, 255, 0, 1), "v=0 a"), secs(0));
    table.observe(&announce(2, v4(239, 255, 0, 1), "v=0 b"), secs(0)); // same origin, new hash
    table.observe(&announce(1, v4(239, 255, 0, 9), "v=0 c"), secs(0)); // same hash, new origin
    assert_eq!(table.inventory().len(), 3);
}

#[test]
fn inbound_deletion_is_ignored_against_a_tracked_session() {
    // ADR-0041 §8 / brief §9: a spoofed T=1 must never evict a tracked session
    // (a third party could otherwise hijack/deny any announcement).
    let table = SapSessionTable::new();
    let o = v4(239, 255, 0, 3);
    table.observe(&announce(5, o, "v=0 x"), secs(0));
    let out = table.observe(&delete(5, o), secs(1));
    assert_eq!(out, ObserveOutcome::DeletionIgnored);
    assert_eq!(
        table.inventory().len(),
        1,
        "the session survives a spoofed deletion (expire by timeout only)"
    );
}

#[test]
fn inbound_deletion_for_an_unknown_session_creates_nothing() {
    let table = SapSessionTable::new();
    let out = table.observe(&delete(9, v4(239, 255, 0, 4)), secs(0));
    assert_eq!(out, ObserveOutcome::DeletionIgnored);
    assert!(
        table.inventory().is_empty(),
        "a deletion never records a session"
    );
}

#[test]
fn global_capacity_drops_the_oldest_and_never_grows() {
    let table = SapSessionTable::with_limits(2, 8);
    table.observe(&announce(1, v4(10, 0, 0, 1), "v=0 a"), secs(0)); // oldest
    table.observe(&announce(2, v4(10, 0, 0, 2), "v=0 b"), secs(1));
    table.observe(&announce(3, v4(10, 0, 0, 3), "v=0 c"), secs(2)); // evicts hash 1
    let inv = table.inventory();
    assert_eq!(inv.len(), 2, "never grows past the global capacity");
    assert!(
        !inv.iter().any(|s| s.key.msg_id_hash == hash(1)),
        "the oldest (by last-seen) is evicted"
    );
    assert!(inv.iter().any(|s| s.key.msg_id_hash == hash(3)));
}

#[test]
fn per_origin_cap_bounds_one_source_share() {
    // One origin cannot monopolise the table by flooding distinct hashes.
    let table = SapSessionTable::with_limits(64, 2);
    let o = v4(203, 0, 113, 7);
    table.observe(&announce(1, o, "v=0 a"), secs(0));
    table.observe(&announce(2, o, "v=0 b"), secs(1));
    table.observe(&announce(3, o, "v=0 c"), secs(2)); // over per-origin cap → oldest of o out
    let inv = table.inventory();
    let from_o = inv.iter().filter(|s| s.key.origin == o).count();
    assert_eq!(from_o, 2, "one origin's share is capped");
    assert!(
        !inv.iter().any(|s| s.key.msg_id_hash == hash(1)),
        "the oldest from that origin is dropped"
    );
}

#[test]
fn purge_removes_stale_sessions_but_keeps_recent_ones() {
    let table = SapSessionTable::new();
    let stale = v4(239, 255, 1, 1);
    let fresh = v4(239, 255, 1, 2);
    table.observe(&announce(1, stale, "v=0 a"), secs(0)); // no period → 1 h floor
    table.observe(&announce(2, fresh, "v=0 b"), secs(0));
    table.observe(&announce(2, fresh, "v=0 b"), secs(3600)); // fresh refreshed at 1 h
    table.purge(secs(3601)); // stale age = 3601 s > 1 h → purge; fresh age = 1 s → keep
    let inv = table.inventory();
    assert_eq!(inv.len(), 1);
    assert_eq!(inv[0].key.origin, fresh);
}

#[test]
fn purge_threshold_is_ten_periods_when_that_exceeds_one_hour() {
    let table = SapSessionTable::new();
    let o = v4(239, 255, 2, 1);
    // Re-announce every 10 min → period 600 s → threshold = max(6000 s, 1 h) = 6000 s.
    table.observe(&announce(1, o, "v=0 a"), secs(0));
    table.observe(&announce(1, o, "v=0 a"), secs(600));
    table.purge(secs(600 + 5000)); // age 5000 s < 6000 s → kept
    assert_eq!(
        table.inventory().len(),
        1,
        "within 10 observed periods the session is kept"
    );
    table.purge(secs(600 + 6001)); // age 6001 s > 6000 s → purged
    assert!(
        table.inventory().is_empty(),
        "beyond 10 observed periods the session is purged"
    );
}

#[test]
fn concurrent_observers_do_not_lose_sessions() {
    // P2-F2: observe() is a load -> clone -> mutate -> store read-modify-write.
    // A `with_table`-shared table is written by multiple concurrent tasks (a
    // dual-stack v4 + v6 listener; the purge tick). ArcSwap makes the STORE
    // atomic, not the whole RMW: two writers can each clone the SAME snapshot and
    // the second store clobbers the first, silently losing a session. Hammer one
    // shared table from many threads with wholly-distinct sessions and assert
    // every one survives — a shortfall is a lost update.
    use std::sync::{Arc, Barrier};

    const THREADS: usize = 8;
    const PER_THREAD: u16 = 250;
    let total = THREADS * usize::from(PER_THREAD);

    // Caps well above the total, so nothing is dropped for capacity/per-origin
    // reasons — any shortfall is a clobbered insert, not a bounded eviction.
    let table = Arc::new(SapSessionTable::with_limits(
        total + 16,
        usize::from(PER_THREAD) + 16,
    ));
    let barrier = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Each thread owns a distinct origin; hashes 1..=PER_THREAD are
                // distinct within it, so every (hash, origin) key is unique.
                let origin = v4(198, 51, 100, u8::try_from(t).unwrap());
                barrier.wait();
                for h in 1..=PER_THREAD {
                    table.observe(&announce(h, origin, "v=0\r\n"), secs(0));
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("worker thread panicked");
    }

    assert_eq!(
        table.len(),
        total,
        "every distinct session from every concurrent writer must survive — a \
         shortfall means one observe() read-modify-write clobbered a peer (P2-F2)"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Bounded memory (inv #10): for ANY sequence of observations the inventory
    /// never exceeds the configured global capacity.
    #[test]
    fn inventory_never_exceeds_capacity(
        ops in prop::collection::vec((1u16..64, 0u8..16, 0u64..100), 0..300)
    ) {
        let cap = 8usize;
        let table = SapSessionTable::with_limits(cap, cap);
        for (h, origin_byte, t) in ops {
            table.observe(&announce(h, v4(10, 0, 0, origin_byte), "v=0"), secs(t));
            prop_assert!(table.inventory().len() <= cap);
        }
    }
}
