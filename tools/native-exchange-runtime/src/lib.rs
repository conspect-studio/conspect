//! Protected native MoQ seam. Run these adapters on the egress/receiver workers, never
//! the output clock. No authority route or paid-delivery flag is enabled by this crate.
pub mod control;
pub mod media;
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
use bytes::Bytes;
use conspect_media_crypto::{Binding, Error, ObjectId, Publisher, Receiver};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

pub type Result<T> = std::result::Result<T, Error>;

fn sibling(path: &std::path::Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path.file_name().ok_or(Error::Invalid)?.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Production callsite: the complete hang object is sealed BEFORE MoQ write_frame.
/// One object per explicit group makes authenticated transport IDs unambiguous.
pub fn publish(
    publisher: &mut Publisher,
    track: &mut moq_net::track::Producer,
    name: &str,
    pts_us: u64,
    captured_ms: u64,
    now_ms: u64,
    payload: Bytes,
) -> Result<ObjectId> {
    let timestamp =
        moq_net::Timestamp::new(pts_us, moq_net::Timescale::MICRO).map_err(|_| Error::Invalid)?;
    let mut group = track.append_group().map_err(|_| Error::Denied)?;
    let id = ObjectId {
        track: name.into(),
        group: group.sequence,
        object: 0,
        captured_ms,
    };
    let mut whole = Vec::new();
    hang::container::Frame { timestamp, payload }
        .encode(&mut whole)
        .map_err(|_| Error::Invalid)?;
    let sealed = publisher.encrypt(&id, now_ms, &whole)?;
    group
        .write_frame(timestamp, Bytes::from(sealed))
        .map_err(|_| Error::Denied)?;
    group.finish().map_err(|_| Error::Denied)?;
    Ok(id)
}

/// Every native decoder/presenter must synchronously discard encoded, decoded,
/// displayed and queued audio on this callback. A network disconnect is not a deny.
pub trait Flush {
    fn flush(&mut self);
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    denied: bool,
    identity: Vec<u8>,
    counters: BTreeMap<String, u64>,
    publisher_generation: u32,
    epoch: u64,
    #[serde(default)]
    grant_expiry_ms: u64,
}

/// Single-owner durable replay/deny state. The file contains NO grants or keys.
/// Exclusive directory ownership is required, like the canonical nonce journal.
pub struct ReceiverRuntime {
    receiver: Receiver,
    binding: Binding,
    state: State,
    path: PathBuf,
    directory: conspect_media_crypto::private_state::PrivateDir,
    expiry_wall_ms: u64,
    last_now_ms: u64,
    expiry_mono: Instant,
    last_decoded: BTreeMap<String, u64>,
    decoded_interval: Option<(u64, u64)>,
    _lock: fs::File,
    healthy: std::cell::Cell<bool>,
}
impl ReceiverRuntime {
    pub fn accept(
        root: &VerifyingKey,
        binding: Binding,
        grant: &[u8],
        now_ms: u64,
        uncertainty_ms: u64,
        path: PathBuf,
    ) -> Result<Self> {
        let receiver = Receiver::accept(root, binding, grant, now_ms, 5000, uncertainty_ms)?;
        let parent = path.parent().ok_or(Error::Invalid)?;
        let directory = conspect_media_crypto::private_state::PrivateDir::open(parent)?;
        let lock = directory.file(&sibling(&path, ".lock")?, true, false)?;
        lock.try_lock().map_err(|_| Error::Denied)?;
        for suffix in [".stage", ".poison", ".active"] {
            if directory.exists(&sibling(&path, suffix)?)? {
                return Err(Error::Denied);
            }
        }
        let mut identity = Vec::new();
        for part in [
            binding.booking,
            binding.taker,
            binding.feed,
            binding.version,
            binding.engine,
            binding.publisher,
        ] {
            identity.extend(part);
        }
        identity.extend(binding.generation.to_be_bytes());
        let mut state: State = match directory.read(&path, 64 * 1024)? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| Error::Denied)?,
            None => State::default(),
        };
        if state.counters.len() > 128 || state.counters.keys().any(|key| key.len() > 128) {
            return Err(Error::Denied);
        }
        if !state.identity.is_empty() && state.identity != identity {
            return Err(Error::Denied);
        }
        let epoch = u64::from_be_bytes(grant[144..152].try_into().map_err(|_| Error::Invalid)?);
        if binding.publisher_generation < state.publisher_generation || epoch < state.epoch {
            return Err(Error::Replay);
        }
        if binding.publisher_generation > state.publisher_generation {
            state.counters.clear();
        }
        state.publisher_generation = binding.publisher_generation;
        state.epoch = epoch;
        state.identity = identity;
        let expiry = u64::from_be_bytes(grant[160..168].try_into().map_err(|_| Error::Invalid)?);
        state.grant_expiry_ms = expiry;
        let mut runtime = Self {
            receiver,
            binding,
            state,
            path,
            directory,
            expiry_wall_ms: expiry.saturating_sub(uncertainty_ms),
            last_now_ms: now_ms,
            expiry_mono: Instant::now()
                + std::time::Duration::from_millis(
                    expiry.saturating_sub(now_ms).saturating_sub(uncertainty_ms),
                ),
            last_decoded: BTreeMap::new(),
            decoded_interval: None,
            _lock: lock,
            healthy: std::cell::Cell::new(true),
        };
        if runtime.state.denied {
            runtime.receiver.deny();
            return Err(Error::Denied);
        }
        runtime.persist()?;
        let marker = runtime
            .directory
            .file(&sibling(&runtime.path, ".active")?, true, true)?;
        marker.sync_all().map_err(|_| Error::Denied)?;
        runtime.directory.sync()?;
        Ok(runtime)
    }
    /// The epoch of the grant this receiver currently holds.
    pub fn epoch(&self) -> u64 {
        self.state.epoch
    }
    /// True when this receiver's grant has lapsed at `now_ms` (awaiting a later grant).
    pub fn grant_lapsed(&self, now_ms: u64) -> bool {
        now_ms >= self.expiry_wall_ms || Instant::now() >= self.expiry_mono
    }
    /// True once a deny (authority refusal, expiry deny, failed transaction) is sticky.
    pub fn is_denied(&self) -> bool {
        self.state.denied || !self.healthy.get()
    }
    /// Install a newly verified grant on the control worker. Reconnect retains
    /// the durable replay journal; a sticky deny can never be cleared here.
    pub fn refresh(
        &mut self,
        root: &VerifyingKey,
        grant: &[u8],
        now_ms: u64,
        uncertainty_ms: u64,
        output: &mut impl Flush,
    ) -> Result<()> {
        if !self.healthy.get() || self.state.denied || now_ms < self.last_now_ms {
            self.deny(output)?;
            return Err(Error::Denied);
        }
        let next = Receiver::accept(root, self.binding, grant, now_ms, 5000, uncertainty_ms)?;
        let epoch = u64::from_be_bytes(grant[144..152].try_into().map_err(|_| Error::Invalid)?);
        if epoch < self.state.epoch {
            return Err(Error::Replay);
        }
        let expiry = u64::from_be_bytes(grant[160..168].try_into().map_err(|_| Error::Invalid)?);
        // Flush native media on epoch transition, even when refreshed before the
        // old timer fires. Never present an old decoded buffer under a new key.
        self.begin()?;
        if epoch == self.state.epoch {
            if let Err(error) = self.receiver.renew(root, grant, now_ms) {
                self.commit()?;
                return Err(error);
            }
            mutation_checkpoint("renew");
        }
        let epoch_changed = epoch != self.state.epoch;
        self.state.epoch = epoch;
        self.state.grant_expiry_ms = expiry;
        if let Err(error) = self.commit() {
            self.receiver.deny();
            self.state.denied = true;
            output.flush();
            return Err(error);
        }
        if epoch_changed {
            // Publish the new receiver only after the durable epoch transaction.
            self.receiver = next;
            output.flush();
            self.last_decoded.clear();
            self.decoded_interval = None;
        }
        self.last_now_ms = now_ms;
        let next_wall_deadline = expiry.saturating_sub(uncertainty_ms);
        let response_deadline = Instant::now()
            + std::time::Duration::from_millis(next_wall_deadline.saturating_sub(now_ms));
        // Replaying a same-expiry grant must not restart a monotonic TTL when
        // the wall clock stalls. Only an actual signed expiry extension adds time.
        let anchored_deadline = self.expiry_mono
            + std::time::Duration::from_millis(
                next_wall_deadline.saturating_sub(self.expiry_wall_ms),
            );
        self.expiry_wall_ms = next_wall_deadline;
        self.expiry_mono = response_deadline.min(anchored_deadline);
        Ok(())
    }
    // Intent is fsynced BEFORE any canonical receiver or replay-state mutation.
    // An unwind caught outside this wrapper cannot turn Drop into clean shutdown.
    fn begin(&self) -> Result<()> {
        if !self.healthy.replace(false) {
            return Err(Error::Denied);
        }
        let marker = self
            .directory
            .file(&sibling(&self.path, ".poison")?, true, true)?;
        marker.sync_all().map_err(|_| Error::Denied)?;
        self.directory.sync()
    }
    fn commit(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.state).map_err(|_| Error::Invalid)?;
        if bytes.len() > 64 * 1024 {
            return Err(Error::Capacity);
        }
        self.directory
            .replace(&sibling(&self.path, ".stage")?, &self.path, &bytes)?;
        self.directory.remove(&sibling(&self.path, ".poison")?)?;
        self.directory.sync()?;
        self.healthy.set(true);
        Ok(())
    }
    fn persist(&self) -> Result<()> {
        self.begin()?;
        self.commit()
    }
    pub fn deny(&mut self, output: &mut impl Flush) -> Result<()> {
        // Close the live fence before invoking a potentially unwinding flush.
        let intent = self.begin();
        self.state.denied = true;
        self.receiver.deny();
        self.decoded_interval = None;
        output.flush();
        intent?;
        self.commit()
    }
    /// Call on an independent expiry timer AND at every decode/presentation/audio
    /// boundary. Queued frames do not retain permission after their grant expires.
    pub fn fence(&mut self, now_ms: u64, output: &mut impl Flush) -> Result<()> {
        if !self.healthy.get() || self.state.denied || now_ms < self.last_now_ms {
            self.deny(output)?;
            return Err(Error::Denied);
        }
        if now_ms >= self.expiry_wall_ms || Instant::now() >= self.expiry_mono {
            // Grant expiry suspends presentation, not the publisher lease. Only a
            // verified later grant can resume it; an authority deny stays sticky.
            self.decoded_interval = None;
            output.flush();
            return Err(Error::Expired);
        }
        self.last_now_ms = now_ms;
        Ok(())
    }
    /// Production receive callsite: authenticate/decrypt BEFORE hang parse/decode.
    /// Persist the authenticated counter before plaintext can enter native queues.
    pub fn open(
        &mut self,
        wire: &[u8],
        id: &ObjectId,
        now_ms: u64,
        output: &mut impl Flush,
    ) -> Result<Vec<u8>> {
        self.fence(now_ms, output)?;
        self.begin()?;
        let plain = match self.receiver.decrypt(wire, id, now_ms) {
            Ok(plain) => plain,
            Err(error) => {
                self.commit()?;
                return Err(error);
            }
        };
        mutation_checkpoint("decrypt");
        let checked = (|| {
            let counter = u64::from_be_bytes(wire[64..72].try_into().map_err(|_| Error::Invalid)?);
            let epoch = u64::from_be_bytes(wire[52..60].try_into().map_err(|_| Error::Invalid)?);
            let generation =
                u32::from_be_bytes(wire[60..64].try_into().map_err(|_| Error::Invalid)?);
            let len = usize::from(wire[96]);
            let track = std::str::from_utf8(&wire[97..97 + len]).map_err(|_| Error::Invalid)?;
            if generation != self.state.publisher_generation
                || epoch < self.state.epoch
                || track != id.track
            {
                return Err(Error::Replay);
            }
            let scope = format!("{}:{}", epoch, track);
            if self
                .state
                .counters
                .get(&scope)
                .is_some_and(|old| counter <= *old)
            {
                return Err(Error::Replay);
            }
            if self.state.counters.len() >= 128 && !self.state.counters.contains_key(&scope) {
                return Err(Error::Capacity);
            }
            self.state.counters.insert(scope, counter);
            Ok(())
        })();

        if let Err(error) = self.commit() {
            self.receiver.deny();
            self.state.denied = true;
            output.flush();
            return Err(error);
        }
        if checked == Err(Error::Capacity) {
            self.deny(output)?;
        }
        checked?;
        Ok(plain)
    }
    /// Commit an advancing decoder observation to the durable spool BEFORE any
    /// HTTP submission. Call only with a decoder-derived interval from decoded().
    pub fn record_decoded<S: conspect_media_crypto::receipt::ReceiptSequence>(
        &mut self,
        receipt_id: [u8; 16],
        observation: conspect_media_crypto::receipt::DecodedObservation,
        adapter: &mut conspect_media_crypto::receipt::ReceiptAdapter<S>,
        spool: &mut conspect_media_crypto::spool::ReceiptSpool,
        output: &mut impl Flush,
    ) -> Result<()> {
        self.fence(observation.observed_at_ms, output)?;
        if !observation.decoded_fresh_advancing
            || self.decoded_interval != Some((observation.media_start_ms, observation.media_end_ms))
        {
            return Err(Error::Invalid);
        }
        self.decoded_interval = None;
        let receipt = adapter.record(receipt_id, observation)?;
        spool.record_and_queue(self.binding.booking, receipt)
    }
    /// Decoder callback ONLY: pts comes from a decoded AVFrame, not an encoded
    /// packet or transport arrival. Return an advancing interval for receipt code.
    pub fn decoded(
        &mut self,
        track: &str,
        media_ms: u64,
        now_ms: u64,
        output: &mut impl Flush,
    ) -> Result<Option<(u64, u64)>> {
        self.fence(now_ms, output)?;
        if track.is_empty()
            || track.len() > 64
            || self.last_decoded.len() >= 64 && !self.last_decoded.contains_key(track)
        {
            return Err(Error::Capacity);
        }
        let old = self.last_decoded.get(track).copied();
        if old.is_some_and(|v| media_ms <= v) {
            return Ok(None);
        }
        self.last_decoded.insert(track.into(), media_ms);
        self.decoded_interval = old.map(|start| (start, media_ms));
        Ok(self.decoded_interval)
    }
}

impl Drop for ReceiverRuntime {
    fn drop(&mut self) {
        // A clean shutdown removes the durable active marker. On a crash or
        // failed transaction it stays, so reconnect cannot resurrect an old allow.
        if self.healthy.get() && !std::thread::panicking() {
            if let Ok(marker) = sibling(&self.path, ".active") {
                if self.directory.remove(&marker).is_ok() {
                    let _ = self.directory.sync();
                }
            }
        }
    }
}

/// Control-worker lease hook. Never invoke this on the output clock. The
/// canonical lease closes its cheap per-seal AND per-grant fence on any failure.
pub fn renew_publisher_lease(
    lease: &conspect_media_crypto::authority_http::PublisherLease,
    monotonic_ms: u64,
) -> Result<()> {
    if lease.needs_renewal(monotonic_ms) {
        lease.renew()?;
    }
    Ok(())
}

/// Bounded next-epoch wait. A caller schedules this instant on its control worker;
/// it must not spin or retry on the media/output clock.
pub fn next_grant_attempt(now_ms: u64, deadline_ms: u64, attempts: u8) -> Result<u64> {
    let next = (now_ms / 5000)
        .checked_add(1)
        .and_then(|v| v.checked_mul(5000))
        .ok_or(Error::Invalid)?;
    if attempts >= 2 || next > deadline_ms {
        return Err(Error::Expired);
    }
    Ok(next)
}

// Test-only injection is thread-local: no production environment-variable backdoor.
#[cfg(test)]
type MutationHook = std::cell::RefCell<Option<Box<dyn Fn(&str)>>>;
#[cfg(test)]
thread_local! { static MUTATION_HOOK: MutationHook = const { std::cell::RefCell::new(None) }; }
fn mutation_checkpoint(_stage: &str) {
    #[cfg(test)]
    MUTATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(_stage);
        }
    });
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use conspect_media_crypto::CounterReservation;
    use ed25519_dalek::SigningKey;
    struct Counter(u64);
    impl CounterReservation for Counter {
        fn reserve(&mut self, _: [u8; 16], _: u64, _: [u8; 16], _: u32) -> Result<u64> {
            let n = self.0;
            self.0 += 1;
            Ok(n)
        }
    }
    #[derive(Default)]
    struct Output(usize);
    impl Flush for Output {
        fn flush(&mut self) {
            self.0 += 1;
        }
    }
    fn fixture() -> (SigningKey, Binding, Vec<u8>, Vec<u8>, ObjectId, Vec<u8>) {
        let signer = SigningKey::from_bytes(&[42; 32]);
        let b = Binding {
            booking: [1; 16],
            taker: [2; 16],
            feed: [9; 16],
            version: [3; 16],
            engine: [4; 16],
            publisher: [5; 16],
            generation: 0,
            publisher_generation: 11,
        };
        let mut p = Publisher::new(
            b.feed,
            b.version,
            b.publisher,
            b.publisher_generation,
            10_000,
            Box::new(Counter(0)),
        )
        .unwrap();
        let g = p.grant(&signer, b, true, 12_000, 10_000).unwrap();
        let renewed = p.grant(&signer, b, true, 15_000, 10_000).unwrap();
        let id = ObjectId {
            track: "video".into(),
            group: 1,
            object: 0,
            captured_ms: 10_000,
        };
        let wire = p.encrypt(&id, 10_000, b"one presentation").unwrap();
        (signer, b, g, renewed, id, wire)
    }
    #[test]
    fn caught_unwind_before_persist_never_reopens_old_counter_or_renewal() {
        for operation in ["decrypt", "renew"] {
            let dir = tempfile::tempdir().unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let path = dir.path().join("receiver");
            let (signer, b, g, renewed, id, wire) = fixture();
            let mut r =
                ReceiverRuntime::accept(&signer.verifying_key(), b, &g, 10_000, 0, path.clone())
                    .unwrap();
            let old_deadline = r.expiry_wall_ms;
            let old_state = fs::read(&path).unwrap();
            let mut output = Output::default();
            MUTATION_HOOK.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |stage| {
                    assert_eq!(stage, operation);
                    panic!("actual mutation/persist boundary");
                }))
            });
            let mut presentations = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if operation == "decrypt" {
                    let _plain = r.open(&wire, &id, 10_000, &mut output).unwrap();
                    presentations += 1;
                } else {
                    r.refresh(&signer.verifying_key(), &renewed, 10_000, 0, &mut output)
                        .unwrap();
                }
            }));
            MUTATION_HOOK.with(|h| *h.borrow_mut() = None);
            assert!(result.is_err());
            assert_eq!(presentations, 0);
            assert_eq!(
                r.expiry_wall_ms, old_deadline,
                "uncommitted renewal never extends presentation deadline"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                old_state,
                "panic happened before state commit"
            );
            assert!(sibling(&path, ".poison").unwrap().exists());
            assert!(!r.healthy.get());
            assert!(r.fence(10_000, &mut output).is_err());
            drop(r); // explicitly outside catch_unwind: thread is no longer panicking
            assert!(sibling(&path, ".active").unwrap().exists());
            assert!(
                ReceiverRuntime::accept(&signer.verifying_key(), b, &g, 10_000, 0, path).is_err()
            );
        }
    }
    #[test]
    fn killed_process_boundary_child() {
        let Some(path) = std::env::var_os("RECEIVER_KILL_TEST_PATH") else {
            return;
        };
        let operation = std::env::var("RECEIVER_KILL_TEST_STAGE").unwrap();
        let (signer, b, g, renewed, id, wire) = fixture();
        let mut r = ReceiverRuntime::accept(
            &signer.verifying_key(),
            b,
            &g,
            10_000,
            0,
            PathBuf::from(path),
        )
        .unwrap();
        MUTATION_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |stage| {
                assert_eq!(stage, operation);
                use std::io::Write;
                println!("MUTATION_READY");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }))
        });
        if std::env::var("RECEIVER_KILL_TEST_STAGE").unwrap() == "decrypt" {
            r.open(&wire, &id, 10_000, &mut Output::default()).unwrap();
        } else {
            r.refresh(
                &signer.verifying_key(),
                &renewed,
                10_000,
                0,
                &mut Output::default(),
            )
            .unwrap();
        }
        panic!("parent must kill child inside transaction");
    }
    #[test]
    fn sigkill_leaves_durable_active_and_denies_old_counter_restart() {
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        for stage in ["decrypt", "renew"] {
            let dir = tempfile::tempdir().unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let path = dir.path().join("receiver");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "transaction_tests::killed_process_boundary_child",
                    "--nocapture",
                ])
                .env("RECEIVER_KILL_TEST_PATH", &path)
                .env("RECEIVER_KILL_TEST_STAGE", stage)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert_ne!(
                    reader.read_line(&mut line).unwrap(),
                    0,
                    "child exited before boundary"
                );
                if line.contains("MUTATION_READY") {
                    break;
                }
                line.clear();
            }
            assert!(sibling(&path, ".active").unwrap().exists());
            assert!(sibling(&path, ".poison").unwrap().exists());
            child.kill().unwrap();
            assert_eq!(child.wait().unwrap().signal(), Some(9));
            assert!(
                sibling(&path, ".active").unwrap().exists(),
                "SIGKILL never executes Drop"
            );
            let (signer, b, g, _, _, _) = fixture();
            assert!(
                ReceiverRuntime::accept(&signer.verifying_key(), b, &g, 10_000, 0, path).is_err()
            );
        }
    }
}
