//! Authority-root seam: protected key path, monotonicity and fail-closed rules.
//!
//! Everything here is deterministic: published test-only constants, a fixed
//! clock, and no network.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use conspect_media_crypto::authority::{
    AuthorityEpochRoot, AuthorityRootGuard, AuthorityRootProvider, CounterScope, EpochRootRequest,
    KeySource,
};
use conspect_media_crypto::journal::DurableCounterJournal;
use conspect_media_crypto::{
    Binding, CounterReservation, Error, LeaseFence, LeasePermit, ObjectId, Publisher, Receiver,
    Result,
};
use ed25519_dalek::SigningKey;
use std::sync::Arc;

const ORG: [u8; 16] = [0x77; 16];
const MACHINE: [u8; 16] = [0x88; 16];

/// The published t44 vector constants, reused so byte preservation is checked
/// against the cross-language artifact rather than against itself.
const VERSION: [u8; 16] = [0x11; 16];
const FEED: [u8; 16] = [0x22; 16];
const PUBLISHER: [u8; 16] = [0x33; 16];
const BOOKING: [u8; 16] = [0x44; 16];
const TAKER: [u8; 16] = [0x55; 16];
const ENGINE: [u8; 16] = [0x66; 16];
/// Bytes 0x20..0x3f: the vector's epoch root, disjoint from the signing seed.
const VECTOR_ROOT: [u8; 32] = [
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
];
const OTHER_ROOT: [u8; 32] = [0x5a; 32];
const ISSUED: u64 = 1_800_000_000_120;
const EXPIRY: u64 = 1_800_000_004_000;
const VECTOR_EPOCH: u64 = 360_000_000;
const MG02_WIRE_HEX: &str = concat!(
    "4d473032636f6e73706563742d657863682d7632636f6e73706563742d656e67696e6532",
    "44444444444444444444444444444444",
    "55555555555555555555555555555555",
    "22222222222222222222222222222222",
    "11111111111111111111111111111111",
    "66666666666666666666666666666666",
    "33333333333333333333333333333333",
    "0000000000000001",
    "00000001",
    "0000000015752a00",
    "000001a3185c5078",
    "000001a3185c5fa0",
    "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    "edf1f6c4c577f33379e87399dd5d9964e1d99e4d92bdecbab57b65fcdbc2e3ac",
    "76a272152cfd5e4ae6767eefd37215f7d6d449bbb577f96d992fa8bdadac7b0e",
);
const MC02_WIRE_HEX: &str = concat!(
    "4d433032",
    "11111111111111111111111111111111",
    "22222222222222222222222222222222",
    "33333333333333333333333333333333",
    "0000000015752a00",
    "00000001",
    "0000000000000001",
    "0000000000000001",
    "0000000000000007",
    "000001a3185c51f4",
    "05",
    "766964656f",
    "a0f38185e14a250eba8596d08d4a4036f78d02eea5e6ca6bf999694c063c5c99",
    "f2c52475f0a15f9f740878931b67a47a",
);
const VECTOR_PLAINTEXT: [u8; 32] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The journal refuses a group/other-accessible directory, and a bare
/// `tempdir()` inherits the umask, so widen nothing and set 0700 explicitly —
/// the same setup the crate's own journal test uses.
fn owner_only_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
    }
    dir
}

/// A provider that echoes the request and then applies explicit overrides, so a
/// test can bend exactly one attribute and show that it is checked.
struct TestProvider {
    root: [u8; 32],
    generation: u64,
    epoch: Option<u64>,
    issued: u64,
    expiry: u64,
    pervert: Pervert,
}

#[derive(PartialEq, Eq)]
enum Pervert {
    None,
    Org,
    Version,
    Feed,
    Machine,
    Publisher,
}

impl TestProvider {
    /// The good provider for this suite's fixed vector/clock.
    fn good() -> Self {
        Self {
            root: VECTOR_ROOT,
            generation: 1,
            epoch: None,
            issued: ISSUED,
            expiry: EXPIRY,
            pervert: Pervert::None,
        }
    }
}

impl AuthorityRootProvider for TestProvider {
    fn current_epoch_root(&self, request: &EpochRootRequest) -> Result<AuthorityEpochRoot> {
        let mut org = request.org;
        let mut version = request.version;
        let mut feed = request.feed;
        let mut machine = request.publisher_machine;
        let mut publisher = request.publisher;
        match self.pervert {
            Pervert::None => {}
            Pervert::Org => org = [0x01; 16],
            Pervert::Version => version = [0x02; 16],
            Pervert::Feed => feed = [0x03; 16],
            Pervert::Machine => machine = [0x04; 16],
            Pervert::Publisher => publisher = [0x05; 16],
        }
        Ok(AuthorityEpochRoot::new(
            org,
            version,
            feed,
            machine,
            publisher,
            self.generation,
            self.epoch.unwrap_or(request.epoch),
            self.issued,
            self.expiry,
            self.root,
        ))
    }
}

/// Starts at a chosen counter, to line up with the published vector.
struct StartAt(u64);

impl CounterReservation for StartAt {
    fn reserve(&mut self, _: [u8; 16], _: u64, _: [u8; 16], _: u32) -> Result<u64> {
        let value = self.0;
        self.0 = self.0.checked_add(1).ok_or(Error::Capacity)?;
        Ok(value)
    }
}

/// Where a [`Probe`] records the scopes it was asked to reserve.
type ScopeLog = std::rc::Rc<std::cell::RefCell<Vec<CounterScope>>>;

/// A reservation that records the scope it was asked to reserve, so a test can
/// show the seal used the authority-bound scope.
struct Probe(ScopeLog, u64);

impl CounterReservation for Probe {
    fn reserve(
        &mut self,
        version: [u8; 16],
        epoch: u64,
        publisher: [u8; 16],
        generation: u32,
    ) -> Result<u64> {
        self.0.borrow_mut().push(CounterScope {
            version,
            publisher,
            epoch,
            generation,
        });
        let value = self.1;
        self.1 = self.1.checked_add(1).ok_or(Error::Capacity)?;
        Ok(value)
    }
}

/// One counter store shared by two publishers, to model a retry.
struct SharedStore(std::rc::Rc<std::cell::RefCell<u64>>);

impl CounterReservation for SharedStore {
    fn reserve(&mut self, _: [u8; 16], _: u64, _: [u8; 16], _: u32) -> Result<u64> {
        let mut next = self.0.borrow_mut();
        let value = *next;
        *next = next.checked_add(1).ok_or(Error::Capacity)?;
        Ok(value)
    }
}

/// A fence that always permits: these tests exercise the protected key path's
/// byte preservation, monotonicity and fail-closed rules, not the exclusive
/// lease. The lease's own deny/expire/revoke behaviour is covered by
/// `tests/authority_http.rs`, and the "authority root without a fence" refusal
/// there proves this fixture is now a required injection rather than a bypass.
struct AlwaysLease;

impl LeaseFence for AlwaysLease {
    fn permit(&self, now_ms: u64) -> Option<LeasePermit> {
        Some(LeasePermit {
            until_monotonic_ms: now_ms,
        })
    }
}

/// A guard with the permit-everything test fence attached.
fn guarded() -> AuthorityRootGuard {
    AuthorityRootGuard::new().with_lease_fence(Arc::new(AlwaysLease))
}

fn from_authority(
    provider: &TestProvider,
    guard: &mut AuthorityRootGuard,
    now: u64,
    reservation: Box<dyn CounterReservation>,
) -> Result<Publisher> {
    Publisher::from_authority(
        provider,
        guard,
        ORG,
        VERSION,
        FEED,
        MACHINE,
        PUBLISHER,
        now,
        reservation,
    )
}

fn binding() -> Binding {
    Binding {
        booking: BOOKING,
        taker: TAKER,
        feed: FEED,
        version: VERSION,
        engine: ENGINE,
        publisher: PUBLISHER,
        generation: 1,
        publisher_generation: 1,
    }
}

fn signer() -> SigningKey {
    let mut seed = [0u8; 32];
    for (i, byte) in seed.iter_mut().enumerate() {
        *byte = u8::try_from(i).unwrap();
    }
    SigningKey::from_bytes(&seed)
}

fn counter_at(wire: &[u8]) -> u64 {
    u64::from_be_bytes(wire[64..72].try_into().unwrap())
}

fn video_keyframe() -> conspect_media_crypto::ObjectId {
    ObjectId {
        track: "video".to_owned(),
        group: 1,
        object: 7,
        captured_ms: 1_800_000_000_500,
    }
}

#[test]
fn authority_mode_binds_the_scope_before_any_seal() {
    let provider = TestProvider::good();
    let mut guard = guarded();
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut publisher = Publisher::from_authority(
        &provider,
        &mut guard,
        ORG,
        VERSION,
        FEED,
        MACHINE,
        PUBLISHER,
        ISSUED,
        Box::new(Probe(std::rc::Rc::clone(&log), 0)),
    )
    .expect("a good authority context is accepted");

    assert_eq!(publisher.key_source(), KeySource::Authority);
    assert_eq!(
        publisher.bound_scope(),
        CounterScope {
            version: VERSION,
            publisher: PUBLISHER,
            epoch: VECTOR_EPOCH,
            generation: 1,
        },
        "the authority's generation and epoch are the bound scope"
    );
    assert_eq!(
        guard.observed_generation(VERSION, PUBLISHER),
        Some(1),
        "the guard records what it accepted"
    );

    let wire = publisher
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("seal");
    assert_eq!(wire.len(), 150, "MC02 wire length is unchanged");
    assert_eq!(
        log.borrow().as_slice(),
        [CounterScope {
            version: VERSION,
            publisher: PUBLISHER,
            epoch: VECTOR_EPOCH,
            generation: 1,
        }],
        "the durable reservation is scoped to the authority-bound identity, before the seal"
    );
}

#[test]
fn authority_root_reproduces_the_published_mg02_bytes() {
    let mut guard = guarded();
    let provider = TestProvider::good();
    let mut publisher =
        from_authority(&provider, &mut guard, ISSUED, Box::new(StartAt(0))).expect("publisher");
    let wire = publisher
        .grant(&signer(), binding(), true, EXPIRY, ISSUED)
        .expect("grant");
    assert_eq!(
        hex(&wire),
        MG02_WIRE_HEX,
        "the protected path must emit the published MG02 bytes unchanged"
    );
}

#[test]
fn authority_root_reproduces_the_published_mc02_bytes() {
    let mut guard = guarded();
    let provider = TestProvider::good();
    let mut publisher =
        from_authority(&provider, &mut guard, ISSUED, Box::new(StartAt(1))).expect("publisher");
    let wire = publisher
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("seal");
    assert_eq!(
        hex(&wire),
        MC02_WIRE_HEX,
        "the protected path must emit the published MC02 bytes unchanged"
    );
}

#[test]
fn a_same_root_retry_continues_the_counter_instead_of_resetting_it() {
    // One durable store, two publishers at the SAME generation and epoch: this is
    // the retry the authority contract describes.
    let store = std::rc::Rc::new(std::cell::RefCell::new(0u64));
    let provider = TestProvider::good();
    let mut guard = guarded();
    let mut first = from_authority(
        &provider,
        &mut guard,
        ISSUED,
        Box::new(SharedStore(std::rc::Rc::clone(&store))),
    )
    .expect("first");
    let a = first
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("a");
    let b = first
        .encrypt(
            &ObjectId {
                track: "video".to_owned(),
                group: 1,
                object: 8,
                captured_ms: 1_800_000_000_600,
            },
            1_800_000_000_661,
            &VECTOR_PLAINTEXT,
        )
        .expect("b");
    assert_eq!(counter_at(&a), 0);
    assert_eq!(counter_at(&b), 1);

    // Retry: same committed root, same generation, same epoch.
    let mut retry = from_authority(
        &provider,
        &mut guard,
        ISSUED,
        Box::new(SharedStore(std::rc::Rc::clone(&store))),
    )
    .expect("retry");
    let c = retry
        .encrypt(
            &ObjectId {
                track: "video".to_owned(),
                group: 1,
                object: 9,
                captured_ms: 1_800_000_000_700,
            },
            1_800_000_000_761,
            &VECTOR_PLAINTEXT,
        )
        .expect("c");
    assert_eq!(
        counter_at(&c),
        2,
        "a same-root retry must continue the counter, never restart it"
    );
}

#[test]
fn a_restart_with_the_same_authority_root_restores_the_journal_counter() {
    let dir = owner_only_tempdir();
    let state = dir.path().join("counters.nr02");
    let provider = TestProvider::good();

    // Process 1.
    let mut guard = guarded();
    let journal = DurableCounterJournal::open(&state).expect("journal");
    let mut first =
        from_authority(&provider, &mut guard, ISSUED, Box::new(journal)).expect("first");
    let a = first
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("a");
    assert_eq!(counter_at(&a), 0);
    drop(first);

    // Process 2: a fresh provider and a fresh guard, the same committed root.
    let mut guard2 = guarded();
    let journal2 = DurableCounterJournal::open(&state).expect("journal");
    let mut second =
        from_authority(&provider, &mut guard2, ISSUED, Box::new(journal2)).expect("second");
    let b = second
        .encrypt(
            &ObjectId {
                track: "video".to_owned(),
                group: 1,
                object: 8,
                captured_ms: 1_800_000_000_600,
            },
            1_800_000_000_661,
            &VECTOR_PLAINTEXT,
        )
        .expect("b");
    assert_eq!(
        counter_at(&b),
        1,
        "the durable journal must resume the counter after a restart"
    );

    // And both objects open under the same grant: the root really is the same.
    let mut signer_publisher =
        from_authority(&provider, &mut guarded(), ISSUED, Box::new(StartAt(0)))
            .expect("grant publisher");
    let grant = signer_publisher
        .grant(&signer(), binding(), true, EXPIRY, ISSUED)
        .expect("grant");
    let mut receiver = Receiver::accept(
        &signer().verifying_key(),
        binding(),
        &grant,
        ISSUED,
        1_000,
        20,
    )
    .expect("accept");
    assert_eq!(
        receiver
            .decrypt(&a, &video_keyframe(), 1_800_000_000_560)
            .expect("decrypt a"),
        VECTOR_PLAINTEXT
    );
    assert_eq!(
        receiver
            .decrypt(
                &b,
                &ObjectId {
                    track: "video".to_owned(),
                    group: 1,
                    object: 8,
                    captured_ms: 1_800_000_000_600,
                },
                1_800_000_000_661
            )
            .expect("decrypt b"),
        VECTOR_PLAINTEXT
    );
}

#[test]
fn a_stale_or_future_epoch_context_is_refused() {
    let mut guard = guarded();
    let stale = TestProvider {
        epoch: Some(VECTOR_EPOCH - 1),
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&stale, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Expired)
    );
    let future = TestProvider {
        epoch: Some(VECTOR_EPOCH + 1),
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&future, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Expired),
        "a future epoch must never be prefetched"
    );
}

#[test]
fn a_late_or_expired_context_fails_closed() {
    let mut guard = guarded();
    // Expiry exactly at now is already expired.
    let late = TestProvider {
        expiry: ISSUED,
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&late, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Expired)
    );
    // An issuance from the future is not a valid context.
    let ahead = TestProvider {
        issued: ISSUED + 1,
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&ahead, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Expired)
    );
}

#[test]
fn an_identity_mismatch_from_the_provider_is_refused() {
    for pervert in [
        Pervert::Org,
        Pervert::Version,
        Pervert::Feed,
        Pervert::Machine,
        Pervert::Publisher,
    ] {
        let mut guard = guarded();
        let provider = TestProvider {
            pervert,
            ..TestProvider::good()
        };
        assert_eq!(
            from_authority(&provider, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
            Some(Error::Denied),
            "a context for another identity must be refused"
        );
    }
}

#[test]
fn a_second_root_for_the_same_scope_is_equivocation() {
    let mut guard = guarded();
    let first = TestProvider::good();
    from_authority(&first, &mut guard, ISSUED, Box::new(StartAt(0))).expect("first root");
    let second = TestProvider {
        root: OTHER_ROOT,
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&second, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Denied),
        "the same identity, epoch and generation with a different root is equivocation"
    );
}

#[test]
fn a_generation_rollback_is_refused() {
    let mut guard = guarded();
    let high = TestProvider {
        generation: 5,
        ..TestProvider::good()
    };
    from_authority(&high, &mut guard, ISSUED, Box::new(StartAt(0))).expect("generation 5");
    let low = TestProvider {
        generation: 4,
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&low, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::Replay),
        "a generation below the observed one is a rollback"
    );
}

#[test]
fn a_generation_rollback_is_also_refused_durably_at_seal() {
    // Defence in depth: even if a caller bypasses the guard (fresh process, no
    // local memory), the durable journal refuses the older generation scope.
    let dir = owner_only_tempdir();
    let state = dir.path().join("rollback.nr02");
    let high = TestProvider {
        generation: 5,
        ..TestProvider::good()
    };
    let mut publisher = from_authority(
        &high,
        &mut guarded(),
        ISSUED,
        Box::new(DurableCounterJournal::open(&state).expect("journal")),
    )
    .expect("generation 5");
    publisher
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("seal at generation 5");

    let low = TestProvider {
        generation: 4,
        ..TestProvider::good()
    };
    let mut rolled_back = from_authority(
        &low,
        &mut guarded(),
        ISSUED,
        Box::new(DurableCounterJournal::open(&state).expect("journal")),
    )
    .expect("the fresh guard cannot see the rollback");
    assert_eq!(
        rolled_back
            .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
            .err(),
        Some(Error::Replay),
        "the durable journal refuses the older generation"
    );
}

#[test]
fn a_fresh_generation_starts_a_fresh_scope_with_a_fresh_root() {
    // The documented mitigation for an undetectable journal restore: the
    // authority issues a NEW generation, which is a new nonce scope AND a new
    // root, so no (key, nonce) pair can repeat.
    let dir = owner_only_tempdir();
    let state = dir.path().join("fresh.nr02");
    let v5 = TestProvider {
        generation: 5,
        ..TestProvider::good()
    };
    let mut publisher = from_authority(
        &v5,
        &mut guarded(),
        ISSUED,
        Box::new(DurableCounterJournal::open(&state).expect("journal")),
    )
    .expect("generation 5");
    let old = publisher
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("seal");

    let v6 = TestProvider {
        generation: 6,
        root: OTHER_ROOT,
        ..TestProvider::good()
    };
    let mut next = from_authority(
        &v6,
        &mut guarded(),
        ISSUED,
        Box::new(DurableCounterJournal::open(&state).expect("journal")),
    )
    .expect("generation 6");
    assert_eq!(next.bound_scope().generation, 6);
    let fresh = next
        .encrypt(&video_keyframe(), 1_800_000_000_560, &VECTOR_PLAINTEXT)
        .expect("seal");
    assert_eq!(counter_at(&fresh), 0, "a new generation is a new scope");
    assert_ne!(
        counter_at(&old),
        u64::MAX,
        "sanity: the previous scope really had a counter"
    );
    assert_eq!(
        next.key_source(),
        KeySource::Authority,
        "still the protected path"
    );
}

#[test]
fn a_server_generation_beyond_u32_is_refused_not_truncated() {
    let mut guard = guarded();
    let overflow = TestProvider {
        generation: u64::from(u32::MAX) + 1,
        ..TestProvider::good()
    };
    assert_eq!(
        from_authority(&overflow, &mut guard, ISSUED, Box::new(StartAt(0))).err(),
        Some(Error::GenerationOverflow),
        "the u32 wire field must never be silently truncated"
    );
    // The largest representable generation still works.
    let maximum = TestProvider {
        generation: u64::from(u32::MAX),
        ..TestProvider::good()
    };
    let publisher =
        from_authority(&maximum, &mut guard, ISSUED, Box::new(StartAt(0))).expect("u32::MAX fits");
    assert_eq!(publisher.bound_scope().generation, u32::MAX);
}

#[test]
fn debug_output_never_prints_the_epoch_root() {
    let mut guard = guarded();
    let provider = TestProvider::good();
    let publisher =
        from_authority(&provider, &mut guard, ISSUED, Box::new(StartAt(0))).expect("publisher");
    let request = EpochRootRequest {
        org: ORG,
        version: VERSION,
        feed: FEED,
        publisher_machine: MACHINE,
        publisher: PUBLISHER,
        epoch: VECTOR_EPOCH,
        now_ms: ISSUED,
    };
    let context = provider.current_epoch_root(&request).expect("context");
    // `Publisher` intentionally implements no Debug at all, so it cannot leak a
    // root: the compiler refuses `{publisher:?}`. Its public accessors are safe.
    let _ = (publisher.key_source(), publisher.bound_scope());
    let rendered = format!("{context:?} {request:?} {:?}", publisher.key_source());
    let root_hex = hex(&VECTOR_ROOT);
    assert!(
        !rendered.contains(&root_hex),
        "the epoch root must never appear in Debug output"
    );
    assert!(
        !rendered.contains("2021222324"),
        "no prefix of the root may leak either"
    );
    assert!(
        format!("{context:?}").contains("redacted"),
        "the redaction must be visible, got {context:?}"
    );
    // Errors carry no payload at all, so they cannot leak a root.
    for error in [
        Error::Denied,
        Error::Expired,
        Error::Replay,
        Error::Capacity,
        Error::GenerationOverflow,
    ] {
        assert!(!format!("{error:?}").contains(&root_hex));
    }
}

#[test]
fn the_local_constructor_is_marked_test_only() {
    let publisher = Publisher::new(FEED, VERSION, PUBLISHER, 1, ISSUED, Box::new(StartAt(0)))
        .expect("local publisher");
    assert_eq!(
        publisher.key_source(),
        KeySource::LocalTestOnly,
        "the RNG path must be distinguishable from the protected path"
    );
}
