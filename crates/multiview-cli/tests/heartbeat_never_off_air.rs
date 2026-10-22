//! CONSPECT-3 never-off-air chaos gate (extends the CONSPECT-2 gate to the
//! heartbeat task; ADR-0096, invariants #1/#10).
//!
//! THE SACRED CONSTRAINT: the device-licensing heartbeat is best-effort and
//! **physically incapable of touching the output clock**. These tests run the
//! real software output clock WHILE a heartbeat task against a misbehaving
//! in-process server is **SIGKILL'd (aborted) / stalled / partitioned**, and
//! assert the output clock still emits exactly one frame per tick and never
//! falters, and the last-good lease state is unchanged afterwards.
//!
//! The store is seeded EMPTY here (last-good == no lease) — a hostile heartbeat
//! that aborts or only ever errors can install nothing and remove nothing, so
//! the store stays empty and the output clock is untouched. The
//! crate-level tests (`multiview-licence`'s `heartbeat_client` suite) cover the
//! seeded-lease "withhold keeps last-good Active" path against a real fake that
//! mints verified leases; this gate is specifically the engine-clock isolation
//! property, with no extra dev-dependencies pulled into the cli.
//!
//! Everything runs under a hard `tokio::time::timeout` so a hung loop fails CI
//! fast rather than hanging it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use multiview_cli::run::SoftwareEngine;
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource};
use multiview_licence::heartbeat::{
    ActivateResponse, DeactivateResponse, DeviceChallenge, DeviceIdentity, HeartbeatClient,
    HeartbeatConfig, HeartbeatError, HeartbeatResponse, LicenceServer, LicensingKeys, PinnedRoot,
    RebindResponse,
};
use multiview_licence::{EnforcementLevel, LeaseStore};

/// The live production ECDSA-P256 root point (base64url uncompressed) — a valid
/// point so `PinnedRoot` parses; the hostile server never returns a lease, so it
/// is never actually used to verify anything here.
const ROOT_PUB_B64URL: &str =
    "BN4f6BIHOFZmFqXp9YM1U65bJTGpOOob1I9X8C_FpfJOWanTCs4Z3c-l8C1wqH4g8Rl01VNkQNC78XixViLiwRY";

/// A small 1x1 software config that builds + runs deterministically.
fn small_config() -> MultiviewConfig {
    let toml = r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/hb_gate.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    let cfg = MultiviewConfig::load_from_toml(toml).expect("parse");
    cfg.validate().expect("validate");
    cfg
}

/// A misbehaving in-process licence server: it either stalls forever or returns
/// a partition error. It never returns a valid lease — the point is that a
/// HOSTILE heartbeat cannot touch the output clock or the last-good state.
struct HostileServer {
    /// When true, every call hangs until cancelled (a stall / black hole).
    stall: AtomicBool,
    /// Set true once a stalled call is genuinely parked in flight (so a test can
    /// wait for a real concurrent stall rather than racing).
    in_flight: AtomicBool,
}

impl HostileServer {
    fn stalling() -> Self {
        Self {
            stall: AtomicBool::new(true),
            in_flight: AtomicBool::new(false),
        }
    }
    fn partitioning() -> Self {
        Self {
            stall: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
        }
    }
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }
    async fn maybe_stall(&self) {
        if self.stall.load(Ordering::SeqCst) {
            // Hang "forever" — the test aborts the task; never resolves on its own.
            self.in_flight.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    }
}

impl LicenceServer for HostileServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn fetch_challenge(&self, _org: &str) -> Result<DeviceChallenge, HeartbeatError> {
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn heartbeat(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn activate(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<ActivateResponse, HeartbeatError> {
        // The first-contact ACTIVATE path is just as hostile (stall / partition) —
        // it must NOT be able to stall the output clock either (ADR-I008, inv #10).
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn rebind(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<RebindResponse, HeartbeatError> {
        // The REBIND lifecycle POST is just as hostile — it must NOT stall the clock
        // (ADR-I009, inv #1/#10).
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn deactivate(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<DeactivateResponse, HeartbeatError> {
        // The DEACTIVATE lifecycle POST is just as hostile — it must NOT stall the
        // clock (ADR-I009, inv #1/#10).
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
}

fn dummy_root() -> PinnedRoot {
    PinnedRoot::from_base64url(ROOT_PUB_B64URL).expect("the production root point parses")
}

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_test".to_owned(),
        instance_id: "inst_test".to_owned(),
        binding_id: Some("ib_test".to_owned()),
        fingerprint_digest: "0".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_test".to_owned(),
        instance_discriminator_hash: "disc_test".to_owned(),
        instance_discriminator_digest: "1".repeat(64),
        app_version: "test".to_owned(),
        device_public_key_b64url: "a2V5".to_owned(),
    }
}

fn client(server: Arc<HostileServer>, store: Arc<LeaseStore>) -> HeartbeatClient<HostileServer> {
    HeartbeatClient::new(
        server,
        store,
        dummy_root(),
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            min_interval: Duration::from_millis(1),
            ..HeartbeatConfig::default()
        },
        identity(),
    )
}

/// THE GATE: a SIGKILL'd (aborted) heartbeat task cannot touch the output clock
/// or the lease store. We build a compliant engine, spawn a heartbeat loop, ABORT
/// it mid-flight (the in-process analogue of SIGKILL), and drive the engine — one
/// frame per tick, never faltered, and the store state is unchanged.
#[tokio::test]
async fn a_sigkilled_heartbeat_task_never_touches_the_output_clock() {
    let store = Arc::new(LeaseStore::new());
    assert!(
        store.status().is_none(),
        "store starts empty (last-good = none)"
    );

    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // Spawn a stalling heartbeat loop, then abort it (SIGKILL analogue).
    let server = Arc::new(HostileServer::stalling());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    handle.abort();
    let _ = handle.await; // joins Cancelled.

    // The engine drives to completion regardless — one frame per tick, no falter.
    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 30),
    )
    .await
    .expect("the engine run must not hang")
    .expect("the engine drives regardless of the heartbeat task");
    assert_eq!(report.frames, 30, "one frame per tick");
    assert_eq!(report.ticks, 30);
    assert!(!report.faltered, "the output clock never falters");

    // A dead heartbeat task installs nothing and removes nothing.
    assert!(
        store.status().is_none(),
        "the store is unchanged by the dead heartbeat task"
    );
}

/// A partitioned (always-erroring) heartbeat loop, running CONCURRENTLY with the
/// output clock for the whole run, cannot slow it or change the store.
#[tokio::test]
async fn a_partitioned_heartbeat_loop_runs_alongside_the_clock_without_effect() {
    let store = Arc::new(LeaseStore::new());

    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // A heartbeat loop that errors every cycle (partition), churning the whole
    // time the engine runs. It can only ever WITHHOLD a lease — never install or
    // remove one.
    let server = Arc::new(HostileServer::partitioning());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang under heartbeat churn")
    .expect("the engine drives regardless");
    assert_eq!(
        report.frames, 50,
        "one frame per tick under heartbeat churn"
    );
    assert!(!report.faltered, "the output clock never falters");

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "a partitioned heartbeat never changes the store (withholds, never tightens)"
    );
}

/// THE IN-FLIGHT STALL GATE: the output clock keeps ticking WHILE a heartbeat
/// call is genuinely BLACK-HOLED in flight (not aborted, not erroring — parked
/// forever mid-call). This is the strongest invariant-#10 proof: a stalled
/// licence-server call cannot back-pressure the output clock.
#[tokio::test]
async fn the_output_clock_ticks_while_a_heartbeat_call_is_stalled_in_flight() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // Start a heartbeat whose first server call black-holes forever.
    let server = Arc::new(HostileServer::stalling());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    // Wait until the heartbeat call is genuinely parked in flight (not a race).
    let parked = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the heartbeat call must reach an in-flight stall"
    );
    assert!(
        !handle.is_finished(),
        "the heartbeat is stalled (has not returned)"
    );

    // WHILE the heartbeat is stalled in flight, drive the output clock. It must
    // emit exactly one frame per tick and never falter.
    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang while a heartbeat call is stalled")
    .expect("the engine drives regardless of an in-flight stalled heartbeat");
    assert_eq!(
        report.frames, 50,
        "one frame per tick while a heartbeat call is stalled in flight"
    );
    assert!(
        !report.faltered,
        "the output clock never falters during the stall"
    );
    assert!(
        !handle.is_finished(),
        "the heartbeat is STILL stalled while the clock ran the whole way"
    );

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "the stalled heartbeat installed/removed nothing"
    );
}

// ===========================================================================
// Device ACTIVATE / enrolment never-off-air gate (ADR-I008).
//
// The first-contact activate path (`activate_once`) calls `fetch_keys` → verify →
// `fetch_challenge` → sign → `server.activate(...)`. To prove a stalled/partitioned
// ACTIVATE POST cannot back-pressure the output clock, the chaos server must let
// `fetch_keys` (which runs `TrustedKeys::verify`) and `fetch_challenge` SUCCEED, then
// stall/error ONLY on `activate` — so the chaos genuinely bites the activate POST,
// not an earlier call. That needs a real attested keyset (a fabricated ECDSA-P256
// root signs an Ed25519 lease intermediate) + a real device signer for the PoP.
// ===========================================================================

/// A fixed, deterministic Ed25519 device signer (a known seed) — so `activate_once`
/// passes its signer guard and builds a real PoP proof. The cli's non-test code has
/// no signer/RNG; this is dev-only.
struct FixedDeviceSigner {
    key: ed25519_dalek::SigningKey,
}
impl multiview_licence::heartbeat::DeviceSigner for FixedDeviceSigner {
    fn public_key_raw(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.key.sign(message).to_bytes()
    }
}

/// A fabricated, root-attested keyset rooted at an ECDSA-P256 key we control — so a
/// served `LicensingKeys` actually passes `TrustedKeys::verify` against this root.
/// Minimal: one Ed25519 `lease` intermediate, valid now, no revocations. (No lease is
/// minted — the activate POST stalls/errors before issuing one.)
struct FabRoot {
    root: p256::ecdsa::SigningKey,
    intermediate: ed25519_dalek::SigningKey,
}
const FAB_KID: &str = "intermediate-fab";
const FAB_VALID_FROM: i64 = 1_700_000_000_000;
const FAB_VALID_UNTIL: i64 = 1_900_000_000_000;
const FAB_ISSUED_AT: i64 = 1_780_000_000_000;
impl FabRoot {
    fn new() -> Self {
        Self {
            root: p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).expect("p256 root"),
            intermediate: ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
        }
    }
    fn pinned_root(&self) -> PinnedRoot {
        let vk = p256::ecdsa::VerifyingKey::from(&self.root);
        PinnedRoot::from_sec1_bytes(vk.to_encoded_point(false).as_bytes()).expect("fab root parse")
    }
    fn root_sign(&self, msg: &[u8]) -> String {
        use p256::ecdsa::signature::Signer as _;
        let sig: p256::ecdsa::Signature = self.root.sign(msg);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
    }
    fn keys(&self) -> LicensingKeys {
        use multiview_licence::heartbeat::{canonical_key_preimage, canonical_revocation_preimage};
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        let pre = canonical_key_preimage(
            FAB_KID,
            "lease",
            "conspect.key-attestation.v1",
            &pubkey,
            FAB_VALID_FROM,
            FAB_VALID_UNTIL,
        );
        let root_sig = self.root_sign(&pre);
        let rev_pre =
            canonical_revocation_preimage(FAB_ISSUED_AT, "conspect.key-revocation.v1", &[]);
        let rev_sig = self.root_sign(&rev_pre);
        let root_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            p256::ecdsa::VerifyingKey::from(&self.root)
                .to_encoded_point(false)
                .as_bytes(),
        );
        let intermediate_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(self.intermediate.verifying_key().to_bytes());
        let json = serde_json::json!({
            "version": 1,
            "root": { "kid": "root", "algorithm": "ecdsa-p256-sha256", "public_key": root_pub,
                "public_key_encoding": "base64url-uncompressed-p256-point" },
            "attestation_contract": {
                "key_statement": "conspect.key-attestation.v1",
                "revocation_statement": "conspect.key-revocation.v1",
                "encoding": "deterministic-cbor-rfc8949-section-4.2.1",
                "key_pre_image": ["key_id","key_type","statement","public_key","valid_from","valid_until"],
                "revocation_pre_image": ["issued_at","statement","revoked_key_ids"],
                "field_order": "canonical", "signature": "ecdsa-p256-sha256-raw-r-s-base64url",
                "public_key_encoding": "raw-32-byte-ed25519-point", "time_unit": "epoch-milliseconds" },
            "lease_keys": [{ "kid": FAB_KID, "key_type": "lease", "algorithm": "ed25519",
                "public_key": intermediate_pub, "valid_from": FAB_VALID_FROM,
                "valid_until": FAB_VALID_UNTIL, "status": "current", "root_sig": root_sig }],
            "update_keys": [],
            "revocation": { "statement": "conspect.key-revocation.v1", "issued_at": FAB_ISSUED_AT,
                "revoked_key_ids": [], "root_revocation_sig": rev_sig }
        });
        serde_json::from_value(json).expect("fabricated keyset must parse")
    }
}

/// A chaos server for the ACTIVATE gate: `fetch_keys` + `fetch_challenge` SUCCEED (so
/// `activate_once` reaches the activate POST), and `activate` is the ONLY hostile call
/// — it stalls in flight forever, or (partition mode) errors every time. `heartbeat`
/// is unused on this path (a fresh device activates first).
struct ChaosActivateServer {
    kit: FabRoot,
    /// When true, `activate` blocks in flight forever (a real black hole), marking
    /// `in_flight` so the test can await a genuine concurrent stall.
    stall_activate: AtomicBool,
    in_flight: AtomicBool,
}
impl ChaosActivateServer {
    fn stalling() -> Self {
        Self {
            kit: FabRoot::new(),
            stall_activate: AtomicBool::new(true),
            in_flight: AtomicBool::new(false),
        }
    }
    fn partitioning() -> Self {
        Self {
            kit: FabRoot::new(),
            stall_activate: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
        }
    }
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }
    fn pinned_root(&self) -> PinnedRoot {
        self.kit.pinned_root()
    }
}
impl LicenceServer for ChaosActivateServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
        Ok(self.kit.keys())
    }
    async fn fetch_challenge(&self, _org: &str) -> Result<DeviceChallenge, HeartbeatError> {
        // A generously-future nonce (the client reads the real wall clock) + a
        // server-assigned instanceId so `activate_once` can build a real request.
        Ok(DeviceChallenge::new(
            "9f3a2b6e1c0e9b41a4c92d7e3b8f10c25e6a7d3f49b0c81e2f5a6d7c8b9e0f1a".to_owned(),
            i64::MAX,
            "ib_chaos_0001".to_owned(),
        ))
    }
    async fn heartbeat(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        // Not exercised on the activate-first path; fail closed if ever reached.
        Err(HeartbeatError::Transport(
            "chaos: heartbeat not used".to_owned(),
        ))
    }
    async fn activate(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<ActivateResponse, HeartbeatError> {
        // THE hostile call: the activate POST either black-holes in flight forever
        // (stall) or errors (partition). Either way it must NOT stall the clock.
        if self.stall_activate.load(Ordering::SeqCst) {
            self.in_flight.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        Err(HeartbeatError::Transport(
            "chaos: activate partitioned".to_owned(),
        ))
    }
    async fn rebind(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<RebindResponse, HeartbeatError> {
        // The REBIND POST stalls/errors just like activate (the lifecycle chaos gate
        // drives this directly via rebind_once).
        if self.stall_activate.load(Ordering::SeqCst) {
            self.in_flight.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        Err(HeartbeatError::Transport(
            "chaos: rebind partitioned".to_owned(),
        ))
    }
    async fn deactivate(
        &self,
        _org: &str,
        _body: Vec<u8>,
        _idem: &str,
        _pop_header: &str,
    ) -> Result<DeactivateResponse, HeartbeatError> {
        // The DEACTIVATE POST stalls/errors just like activate.
        if self.stall_activate.load(Ordering::SeqCst) {
            self.in_flight.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        Err(HeartbeatError::Transport(
            "chaos: deactivate partitioned".to_owned(),
        ))
    }
}

/// A FRESH, un-bound device identity (no binding) — the first-contact ACTIVATE case.
fn fresh_identity() -> DeviceIdentity {
    DeviceIdentity {
        binding_id: None,
        ..identity()
    }
}

/// An ENROLLING client: a fresh device with activate ENABLED **and a real device
/// signer**, so `run_once` takes the first-contact ACTIVATE path and `activate_once`
/// runs through its signer guard + `fetch_keys`/`fetch_challenge` to actually REACH
/// `server.activate(...)` (the call the chaos gate protects).
fn enrolling_client(
    server: Arc<ChaosActivateServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<ChaosActivateServer> {
    let pinned = server.pinned_root();
    let signer: Arc<dyn multiview_licence::heartbeat::DeviceSigner> = Arc::new(FixedDeviceSigner {
        key: ed25519_dalek::SigningKey::from_bytes(&[0x5a; 32]),
    });
    HeartbeatClient::new(
        server,
        store,
        pinned,
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            min_interval: Duration::from_millis(1),
            enable_activate: true,
            ..HeartbeatConfig::default()
        },
        fresh_identity(),
    )
    .with_signer(signer)
}

/// THE ACTIVATE-PATH IN-FLIGHT STALL GATE (ADR-I008): a fresh device's first-contact
/// ACTIVATE **POST**, black-holed in flight, cannot back-pressure the output clock.
/// The chaos server lets `fetch_keys`/`fetch_challenge` succeed and stalls ONLY on
/// `server.activate(...)`, so the stall genuinely bites the activate call this gate
/// protects (inv #1/#10) — not an earlier no-op.
#[tokio::test]
async fn the_output_clock_ticks_while_an_activate_call_is_stalled_in_flight() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // A fresh device that ENROLS — fetch_keys/fetch_challenge succeed, then its
    // activate POST black-holes.
    let server = Arc::new(ChaosActivateServer::stalling());
    let hb = enrolling_client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    // Wait until the ACTIVATE call is genuinely parked in flight (not a race) — proof
    // the client reached `server.activate(...)`, past fetch_keys + fetch_challenge.
    let parked = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the ACTIVATE POST must reach an in-flight stall (past fetch_keys/challenge)"
    );
    assert!(
        !handle.is_finished(),
        "the activate is stalled (not returned)"
    );

    // WHILE the activate POST is stalled in flight, drive the output clock — one frame
    // per tick, never faltered.
    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang while an activate POST is stalled")
    .expect("the engine drives regardless of an in-flight stalled activate POST");
    assert_eq!(
        report.frames, 50,
        "one frame per tick while an activate POST is stalled in flight"
    );
    assert!(
        !report.faltered,
        "the output clock never falters during the activate-POST stall"
    );
    assert!(
        !handle.is_finished(),
        "the activate POST is STILL stalled while the clock ran the whole way"
    );

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "the stalled activate installed nothing (a fresh device stays last-good/empty)"
    );
}

/// A partitioned ACTIVATE POST (the `server.activate(...)` call errors every cycle,
/// after `fetch_keys`/`fetch_challenge` succeed) churning alongside the output clock
/// cannot slow it or install anything — the fresh-device fail-closed path keeps the
/// engine on air (ADR-I008, inv #1/#10).
#[tokio::test]
async fn a_partitioned_activate_loop_runs_alongside_the_clock_without_effect() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    let server = Arc::new(ChaosActivateServer::partitioning());
    let hb = enrolling_client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang under activate-POST churn")
    .expect("the engine drives regardless");
    assert_eq!(
        report.frames, 50,
        "one frame per tick under activate-POST churn"
    );
    assert!(!report.faltered, "the output clock never falters");

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "a partitioned activate POST never installs a lease (fresh device stays empty)"
    );
}

// ===========================================================================
// Device REBIND + DEACTIVATE never-off-air gate (ADR-I009).
//
// The one-shot lifecycle ops (`rebind_once` / `deactivate_once`) call fetch_keys →
// verify → fetch_challenge → sign → server.rebind/deactivate(...). To prove a
// stalled/partitioned lifecycle POST cannot back-pressure the output clock, the
// chaos server lets fetch_keys + fetch_challenge SUCCEED and stalls ONLY on the
// lifecycle POST (reusing ChaosActivateServer, whose rebind/deactivate honour the
// same stall knob). A BOUND device drives the one-shot op directly.
// ===========================================================================

/// A BOUND device (it already holds `ib_test`) driving a one-shot lifecycle op with
/// a real device signer, so `rebind_once`/`deactivate_once` reach the lifecycle POST.
fn lifecycle_client(
    server: Arc<ChaosActivateServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<ChaosActivateServer> {
    let pinned = server.pinned_root();
    let signer: Arc<dyn multiview_licence::heartbeat::DeviceSigner> = Arc::new(FixedDeviceSigner {
        key: ed25519_dalek::SigningKey::from_bytes(&[0x5a; 32]),
    });
    HeartbeatClient::new(
        server,
        store,
        pinned,
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            min_interval: Duration::from_millis(1),
            ..HeartbeatConfig::default()
        },
        identity(), // bound: binding_id = Some("ib_test")
    )
    .with_signer(signer)
}

/// THE REBIND-PATH IN-FLIGHT STALL GATE (ADR-I009): a bound device's one-shot REBIND
/// POST, black-holed in flight, cannot back-pressure the output clock.
#[tokio::test]
async fn the_output_clock_ticks_while_a_rebind_call_is_stalled_in_flight() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    let server = Arc::new(ChaosActivateServer::stalling());
    let hb = lifecycle_client(Arc::clone(&server), Arc::clone(&store));
    // Drive the one-shot rebind concurrently — it black-holes in the rebind POST.
    let handle = tokio::spawn(async move { hb.rebind_once("lic_test").await });

    let parked = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the REBIND POST must reach an in-flight stall"
    );
    assert!(
        !handle.is_finished(),
        "the rebind is stalled (not returned)"
    );

    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang while a rebind POST is stalled")
    .expect("the engine drives regardless of an in-flight stalled rebind POST");
    assert_eq!(
        report.frames, 50,
        "one frame per tick while a rebind POST is stalled"
    );
    assert!(
        !report.faltered,
        "the output clock never falters during the rebind stall"
    );
    assert!(!handle.is_finished(), "the rebind POST is STILL stalled");

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "the stalled rebind installed nothing"
    );
}

/// THE DEACTIVATE-PATH IN-FLIGHT STALL GATE (ADR-I009): a bound device's one-shot
/// DEACTIVATE POST, black-holed in flight, cannot back-pressure the output clock.
#[tokio::test]
async fn the_output_clock_ticks_while_a_deactivate_call_is_stalled_in_flight() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    let server = Arc::new(ChaosActivateServer::stalling());
    let hb = lifecycle_client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.deactivate_once().await });

    let parked = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the DEACTIVATE POST must reach an in-flight stall"
    );
    assert!(
        !handle.is_finished(),
        "the deactivate is stalled (not returned)"
    );

    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang while a deactivate POST is stalled")
    .expect("the engine drives regardless of an in-flight stalled deactivate POST");
    assert_eq!(
        report.frames, 50,
        "one frame per tick while a deactivate POST is stalled"
    );
    assert!(
        !report.faltered,
        "the output clock never falters during the deactivate stall"
    );
    assert!(
        !handle.is_finished(),
        "the deactivate POST is STILL stalled"
    );

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "the stalled deactivate touched nothing"
    );
}
