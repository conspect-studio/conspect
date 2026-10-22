//! Config-file watch (ADR-W020): hot-reload the impacted parts of the boot
//! config when the file changes on disk — through the SAME apply machinery the
//! Web/API uses — and change NOTHING when the new document is invalid.
//!
//! The watcher is a control-plane tokio task (never on the render thread):
//! it polls the config **path** (re-`stat` each tick, so a write-temp +
//! `rename(2)` lands as a normal change) and acts only on a fingerprint that
//! is **stable across two consecutive polls** (editors multi-write; the file
//! must settle first). On a debounced change it reparses + revalidates the
//! WHOLE document:
//!
//! * **invalid** ⇒ `tracing::warn!`, a latched `config-file-invalid` health
//!   warning on the engine's drop-oldest publisher (the UI banner + `GET
//!   /api/v1/health` surface), a `last_rejected` watch-status record — and
//!   **no state change anywhere** (the run keeps the last-good document);
//! * **valid** ⇒ a pure per-section [`ConfigDiff`] against the RUNNING
//!   baseline, applied through the one machinery: synthetic source
//!   adds/edits and any source removal ride `UpsertSource`/`RemoveSource` on
//!   the bounded command bus (ADR-W018), a layout/cells change rides the
//!   shared resolve+solve+Class-1-gate and `ApplyLayout` (ADR-W019), and
//!   every section without a live path reseeds its control store (the UI's
//!   truth) and latches a `config-file-requires-restart` warning naming it.
//!   The new file then becomes the baseline.
//!
//! Isolation (invariants #1 + #10): every engine submission is the
//! non-blocking `try_submit`. A full bus sheds the submission — the watcher
//! then leaves the baseline AND the applied fingerprint **un-advanced**,
//! records a partial-apply rejection, raises the interim
//! `config-file-apply-incomplete` warning, and **re-applies the whole
//! (idempotent) change on a later poll** until every command lands; the
//! warning clears when the apply completes. Every publish is the drop-oldest
//! event broadcast, and every store touched is read-mostly control-plane
//! state. Nothing here can pace, stall, or back-pressure the output clock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::{ConfigDiff, MultiviewConfig, OverlayChange, SourceChange};
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

use crate::{
    resolve_layout_document, AppState, AuditAction, Command, LayoutInput, OperationId,
    ResourceInput, ResourceRepository,
};

/// The audit-log actor every file-applied change is recorded under.
const ACTOR: &str = "config-file";

/// How many consecutive missing-file polls before the watcher reports the
/// file as gone (a transient `ENOENT` mid-rename is normal and silent).
const MISSING_POLLS_BEFORE_REPORT: u32 = 5;

/// Watcher tuning. The default polls once per second — instant enough for a
/// hand edit, one `stat(2)` per second of cost, and no native watcher
/// dependency (ADR-W020 §1).
#[derive(Debug, Clone)]
pub struct WatchOptions {
    poll_interval: Duration,
    initial_observed: Option<String>,
    handle: Option<ConfigWatchHandle>,
    /// A one-shot slot holding the loop-side end of a manual poll driver, when
    /// a test installed one via [`WatchOptions::with_manual_poll`]. [`spawn`]
    /// takes it out (it is `None` in production), so the loop is driven tick by
    /// tick from the test instead of by the wall clock. Wrapped in
    /// `Arc<Mutex<Option<…>>>` only so `WatchOptions` stays `Clone` — the
    /// production path never constructs it and never pays for it.
    manual_poll: Option<Arc<std::sync::Mutex<Option<PollGate>>>>,
    /// A one-shot slot ([`WatchOptions::with_post_probe_interpose`]) holding
    /// content the loop writes to the watched path exactly once, right after a
    /// successful [`probe`] and BEFORE the settled read/apply. It exists only so
    /// a test can interpose a concurrent external writer precisely in the
    /// probe→apply window (the TOCTOU the content fingerprint must be immune
    /// to). Because arming it makes the watcher WRITE the config file, the whole
    /// seam is gated behind the test-only `_test-seams` feature: compiled out of
    /// the default build and the `multiview-cli` release presets, and — even if
    /// the feature is enabled — inert until a caller explicitly arms it via
    /// [`with_post_probe_interpose`](WatchOptions::with_post_probe_interpose),
    /// which no production code does. No shipped configuration writes the file.
    #[cfg(feature = "_test-seams")]
    interpose: Option<Arc<std::sync::Mutex<Option<Vec<u8>>>>>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            initial_observed: None,
            handle: None,
            manual_poll: None,
            #[cfg(feature = "_test-seams")]
            interpose: None,
        }
    }
}

impl WatchOptions {
    /// Override the poll interval (tests run the watcher at tens of
    /// milliseconds; production keeps the 1 s default).
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Seed the loop's last-observed CONTENT with the boot-file text the run
    /// loaded at start (ADR-W024 §4): a settled observation whose content still
    /// equals the last observed content is adopted without applying — under
    /// `start = "resume"` the watcher's baseline is the RESUMED document while
    /// the file still holds the boot document, and the UNCHANGED file must
    /// never clobber the resumed Running state. An edit landing in the boot
    /// window IS a content change against this seed and still applies. Without
    /// a seed the first settled poll diffs whatever content it finds against
    /// the baseline (the ADR-W020 boot-window semantics, unchanged).
    #[must_use]
    pub fn with_initial_observed(mut self, content: String) -> Self {
        self.initial_observed = Some(content);
        self
    }

    /// Use a pre-created watch handle (ADR-W024 MAJOR-C1): the binary creates
    /// the handle and installs it into `AppState` BEFORE the router serves, so
    /// a promote in the startup window already finds the suppression seam, then
    /// hands the SAME handle here so [`spawn`] drives it. Absent ⇒ [`spawn`]
    /// creates and installs a fresh one (tests that spawn directly).
    #[must_use]
    pub fn with_handle(mut self, handle: ConfigWatchHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    /// Drive the poll loop by a **manual tick** instead of the wall clock —
    /// the deterministic test seam (task #131). Returns a [`ManualPoll`] the
    /// test fires: every [`ManualPoll::poll_once`] runs EXACTLY one poll
    /// iteration and resolves only once that iteration has fully completed and
    /// the loop is parked again, so a settled-change assertion never races the
    /// debounce/poll cadence. A two-poll debounce is two `poll_once().await`s,
    /// not a `sleep` — no real time elapses, nothing flakes under load.
    ///
    /// Production never calls this: [`spawn`] finds no manual driver and uses
    /// the [`with_poll_interval`](Self::with_poll_interval) wall-clock timer,
    /// byte-for-byte unchanged. When a manual driver IS installed the
    /// `poll_interval` is ignored (the test owns the cadence).
    #[must_use]
    pub fn with_manual_poll(mut self) -> (Self, ManualPoll) {
        let (manual, gate) = manual_poll_pair();
        self.manual_poll = Some(Arc::new(std::sync::Mutex::new(Some(gate))));
        (self, manual)
    }

    /// Install the **post-probe interpose** test seam and return its handle.
    /// Between two [`ManualPoll::poll_once`]s, call
    /// [`PostProbeInterpose::interpose_next`] to schedule a single write the
    /// loop performs on its NEXT poll, right after [`probe`] and BEFORE the
    /// settled read/apply — modelling an external writer that races the poll
    /// exactly in the probe→apply window. It exists to prove the applied
    /// fingerprint always matches the content actually applied: a probe-then-
    /// reread would apply the racer's bytes yet record the probe's fingerprint.
    ///
    /// Test-only (`_test-seams`): the seam WRITES the watched file, so it is
    /// compiled out of the default build and the `multiview-cli` release presets
    /// (and, even when the feature is on, does nothing until armed here). Composes
    /// with [`with_manual_poll`](Self::with_manual_poll): the interposed write
    /// lands mid-poll, so a plain `poll_once` still drives it.
    #[cfg(feature = "_test-seams")]
    #[must_use]
    pub fn with_post_probe_interpose(mut self) -> (Self, PostProbeInterpose) {
        let slot = Arc::new(std::sync::Mutex::new(None));
        self.interpose = Some(Arc::clone(&slot));
        (self, PostProbeInterpose { slot })
    }
}

/// The test-side handle of the post-probe interpose seam
/// ([`WatchOptions::with_post_probe_interpose`]). Held by the test; a cheap
/// `Arc` clone shares the one-shot slot with the loop. Test-only
/// (`_test-seams`) — compiled out of the default build and the release presets.
#[cfg(feature = "_test-seams")]
#[derive(Debug, Clone)]
pub struct PostProbeInterpose {
    /// The shared one-shot slot the loop drains post-probe (see
    /// [`WatchOptions::interpose`]).
    slot: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

#[cfg(feature = "_test-seams")]
impl PostProbeInterpose {
    /// Schedule `content` to be written to the watched path once, on the loop's
    /// next poll, in the probe→apply window. One-shot: the loop takes and
    /// clears it, so it fires on exactly the next poll and no later one.
    pub fn interpose_next(&self, content: &str) {
        *lock_interpose(&self.slot) = Some(content.as_bytes().to_vec());
    }
}

/// Lock the interpose one-shot slot, recovering from a poisoned lock (a
/// panicked test thread must not wedge the watcher). Test-only (`_test-seams`).
#[cfg(feature = "_test-seams")]
fn lock_interpose(
    slot: &std::sync::Mutex<Option<Vec<u8>>>,
) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The test-side handle of the manual poll driver ([`WatchOptions::with_manual_poll`]):
/// fire [`ManualPoll::poll_once`] to run exactly one watcher poll iteration and
/// await its completion. Single-owner (held by the test); not `Clone`.
#[derive(Debug)]
pub struct ManualPoll {
    /// Requests one poll iteration of the loop (capacity-1 rendezvous).
    tick: tokio::sync::mpsc::Sender<()>,
    /// Resolves when the loop has finished that iteration and re-parked.
    ack: tokio::sync::mpsc::Receiver<()>,
}

impl ManualPoll {
    /// Run **exactly one** poll iteration of the watcher and wait until it has
    /// fully completed (the loop is parked waiting for the next tick). Drives
    /// the debounce deterministically: write the file, then `poll_once().await`
    /// to register the candidate fingerprint and `poll_once().await` again to
    /// cross the two-poll settle gate and apply — no wall-clock `sleep`, no
    /// poll/`SETTLE` race.
    ///
    /// Returns `true` when the iteration ran and acknowledged; `false` once the
    /// watcher loop has exited (e.g. after [`ConfigWatchHandle::stop`] or run
    /// teardown), so a test that polls past stop observes a clean end rather
    /// than hanging.
    pub async fn poll_once(&mut self) -> bool {
        if self.tick.send(()).await.is_err() {
            // The loop dropped its receiver — it has exited.
            return false;
        }
        // The loop sends the ack at the TOP of its next `wait_for_tick`, i.e.
        // only after the iteration body for this tick fully completed and it is
        // parked again — so awaiting it is a true round-trip, never a race.
        self.ack.recv().await.is_some()
    }
}

/// The loop-side end of the manual poll driver: receive a tick, and ack the
/// PREVIOUS iteration's completion. Held inside [`PollDriver::Manual`]; not
/// `Clone` (a single loop owns it).
#[derive(Debug)]
struct PollGate {
    /// Receives a poll request from [`ManualPoll::poll_once`].
    tick: tokio::sync::mpsc::Receiver<()>,
    /// Signals that the previous iteration completed and the loop is parked.
    ack: tokio::sync::mpsc::Sender<()>,
    /// Whether an iteration ran since the last ack (so the first park sends no
    /// spurious ack — the bootstrap has nothing to acknowledge yet).
    owe_ack: bool,
}

/// Build a connected [`ManualPoll`] (test side) + [`PollGate`] (loop side)
/// rendezvous pair. Capacity-1 channels make every tick a strict hand-off.
fn manual_poll_pair() -> (ManualPoll, PollGate) {
    let (tick_tx, tick_rx) = tokio::sync::mpsc::channel(1);
    let (ack_tx, ack_rx) = tokio::sync::mpsc::channel(1);
    (
        ManualPoll {
            tick: tick_tx,
            ack: ack_rx,
        },
        PollGate {
            tick: tick_rx,
            ack: ack_tx,
            owe_ack: false,
        },
    )
}

/// How the loop waits between polls: the wall-clock interval (production), or a
/// manual tick fired by a test ([`WatchOptions::with_manual_poll`]). The ONLY
/// behavioural difference is WHEN a poll iteration starts — every iteration's
/// body is identical, so a manual-driven test exercises the exact production
/// poll/debounce/apply path, just on a deterministic clock.
enum PollDriver {
    /// Sleep `poll_interval` before each poll (the production path).
    Interval(Duration),
    /// Wait for the test to fire the next poll tick (the deterministic seam).
    Manual(PollGate),
}

impl PollDriver {
    /// Derive the driver from the options: a manual driver if a test installed
    /// one (taken out of its one-shot slot), else the wall-clock interval.
    fn from_options(options: &WatchOptions) -> Self {
        if let Some(slot) = options.manual_poll.as_ref() {
            if let Some(gate) = lock_gate_slot(slot).take() {
                return Self::Manual(gate);
            }
        }
        Self::Interval(options.poll_interval)
    }

    /// Park until the next poll should run. Returns `true` to proceed with a
    /// poll, or `false` when a manual driver's test side has been dropped (all
    /// senders gone) — the loop then exits cleanly. The interval path always
    /// proceeds.
    async fn wait_for_tick(&mut self) -> bool {
        match self {
            Self::Interval(interval) => {
                tokio::time::sleep(*interval).await;
                true
            }
            Self::Manual(gate) => {
                // Ack the iteration that just finished (the loop is now parked),
                // so the test's matching `poll_once` resolves — but only if one
                // actually ran (no spurious bootstrap ack). A dropped ack
                // receiver (test gone) is ignored; the recv below then ends it.
                if gate.owe_ack {
                    let _ = gate.ack.send(()).await;
                    gate.owe_ack = false;
                }
                match gate.tick.recv().await {
                    Some(()) => {
                        gate.owe_ack = true;
                        true
                    }
                    // Every `ManualPoll` sender dropped — end the loop.
                    None => false,
                }
            }
        }
    }
}

/// Lock the manual-poll one-shot slot, recovering from a poisoned lock (a
/// panicked test thread must not wedge spawn).
fn lock_gate_slot(
    slot: &std::sync::Mutex<Option<PollGate>>,
) -> std::sync::MutexGuard<'_, Option<PollGate>> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The most banked expected-write tokens kept at once (a stuck writer must
/// not grow memory; older banked tokens are shed oldest-first).
const MAX_EXPECTED_TOKENS: usize = 8;

/// One banked expected-write announcement (ADR-W024 §6): the announced
/// content's hash and whether its writer confirmed the write landed. An
/// announcement never confirmed (the write still in flight) survives an
/// interleaving edit and suppresses exactly its content when it finally lands
/// (review m3, pinned).
#[derive(Debug, Clone, Copy)]
struct ExpectedToken {
    /// The announced content's hash.
    hash: u64,
    /// Whether the announcing writer confirmed the write landed on disk.
    landed: bool,
}

/// The cloneable handle [`spawn`] returns: self-write suppression for the
/// promote-to-boot lane ([`ConfigWatchHandle::expect_write`]) and a stop flag
/// for run teardown.
#[derive(Debug, Clone)]
pub struct ConfigWatchHandle {
    /// Banked expected (server-side) write announcements; each suppresses one
    /// reload of EXACTLY that content (review m3 — a banked token must never
    /// eat an unrelated external edit).
    expected: Arc<std::sync::Mutex<std::collections::VecDeque<ExpectedToken>>>,
    /// Raised by [`ConfigWatchHandle::stop`]; the loop exits on its next poll.
    stop: Arc<AtomicBool>,
}

impl Default for ConfigWatchHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigWatchHandle {
    /// Create a fresh, un-spawned watch handle (ADR-W024 MAJOR-C1): the binary
    /// installs this into `AppState` BEFORE the router serves a request, then
    /// hands it to [`spawn`], so a `POST /config/promote` in the startup window
    /// always finds the suppression seam (never skips it and lets the watcher
    /// later re-apply the promoted file as an external edit).
    #[must_use]
    pub fn new() -> Self {
        Self {
            expected: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Mark one **expected** write of exactly `content`: when a debounced file
    /// change carries this content it is adopted as the new baseline WITHOUT
    /// applying anything (the server-side writer — e.g. a promote-to-boot
    /// flow — already applied the state it serialized). Call immediately
    /// before writing the file; confirm success with
    /// [`ConfigWatchHandle::mark_write_landed`] or release a failure with
    /// [`ConfigWatchHandle::release_write`]. The token is content-paired: an
    /// unrelated external edit landing first is still applied normally
    /// (review m3).
    pub fn expect_write(&self, content: &str) {
        let mut tokens = lock_tokens(&self.expected);
        if tokens.len() >= MAX_EXPECTED_TOKENS {
            tokens.pop_front();
        }
        tokens.push_back(ExpectedToken {
            hash: content_hash(content),
            landed: false,
        });
    }

    /// Confirm that the announced write of exactly `content` LANDED on disk
    /// (review B1 (2)). File writes are ordered: once a landed write is
    /// superseded by a different settled content it can never be the next
    /// settled observation, so the watcher drains its token — a stale token
    /// must never eat a much later REAL edit that restores the same bytes
    /// (`git checkout`, editor undo). An announcement never confirmed keeps
    /// the in-flight semantics (review m3, pinned): it survives interleaving
    /// edits and suppresses exactly its content when it finally settles.
    pub fn mark_write_landed(&self, content: &str) {
        let hash = content_hash(content);
        let mut tokens = lock_tokens(&self.expected);
        if let Some(token) = tokens.iter_mut().rev().find(|t| t.hash == hash) {
            token.landed = true;
        }
    }

    /// Release a banked expected-write token for exactly `content` (review
    /// B1 (3)): a server-side writer whose announced write FAILED must call
    /// this so the token cannot eat a later REAL external edit that happens
    /// to carry the same content. Returns whether a token was released.
    #[must_use = "whether a token was actually released is diagnostic; ignore it explicitly"]
    pub fn release_write(&self, content: &str) -> bool {
        let hash = content_hash(content);
        let mut tokens = lock_tokens(&self.expected);
        match tokens.iter().position(|t| t.hash == hash) {
            Some(index) => {
                tokens.remove(index);
                true
            }
            None => false,
        }
    }

    /// Stop the watcher (it exits on its next poll tick and marks the
    /// watch-status inactive).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Consume the banked token matching `text`'s content, if one exists —
    /// together with every OLDER banked token (review B1 (2)): writes are
    /// ordered, so announcements older than this settled write were
    /// superseded by it and must never eat a later real edit.
    fn consume_expected_for(&self, text: &str) -> bool {
        let hash = content_hash(text);
        let mut tokens = lock_tokens(&self.expected);
        match tokens.iter().position(|t| t.hash == hash) {
            Some(index) => {
                tokens.drain(..=index);
                true
            }
            None => false,
        }
    }

    /// Drain every LANDED token (review B1 (2)): called when a settled
    /// observation matches no banked announcement — an announcement whose
    /// write already landed was necessarily superseded by this settled
    /// content (writes are ordered), so its token is stale. Unlanded
    /// announcements (writes still in flight) are kept (review m3, pinned).
    fn drain_landed(&self) {
        let mut tokens = lock_tokens(&self.expected);
        tokens.retain(|t| !t.landed);
    }
}

/// Lock the token queue, recovering from a poisoned lock (a panicked banker
/// must not wedge the watcher).
fn lock_tokens(
    tokens: &std::sync::Mutex<std::collections::VecDeque<ExpectedToken>>,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<ExpectedToken>> {
    match tokens.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The ONE seam every server-side boot-file writer announces itself through
/// (ADR-W024 §6): call this immediately before atomically writing `content`
/// to the watched path, so the watcher adopts exactly that write as its new
/// baseline instead of re-applying it. The token is content-paired (review
/// m3): an unrelated external edit landing first is still applied normally.
/// Pair with [`confirm_server_write`] on success or
/// [`ConfigWatchHandle::release_write`] on failure.
pub fn expect_server_write(handle: &ConfigWatchHandle, content: &str) {
    handle.expect_write(content);
}

/// Confirm a previously announced server-side write of exactly `content`
/// landed on disk (review B1 (2)): call this immediately after the atomic
/// write succeeds, so a token whose write is later superseded by a different
/// settled content is drained instead of lingering to eat a future real edit
/// carrying the same bytes.
pub fn confirm_server_write(handle: &ConfigWatchHandle, content: &str) {
    handle.mark_write_landed(content);
}

/// A stable in-process hash of a write's content (`DefaultHasher` — this is a
/// self-match between our own writer and our own watcher, not an adversarial
/// boundary).
fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// A stable in-process hash of the watched file's raw bytes, folded into the
/// [`Fingerprint`] so change detection does not depend on filesystem `stat`
/// granularity. Not an adversarial boundary (whoever writes the config file
/// already controls it), so `DefaultHasher` is ample; hashing BYTES (not a
/// `&str`) keeps a non-UTF-8 file detectable as a change — the settled UTF-8
/// decode in [`watch_loop`] does the UTF-8 gate.
fn fingerprint_content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// The content axis of a [`Fingerprint`]: a hash of the file's bytes when they
/// could be read this poll, or `Unreadable` when the path exists (its `stat`
/// succeeded) but its bytes could not be read — a permissions problem, or a
/// swap racing the stat. A distinct, typed state rather than an in-band hash
/// value: two `Unreadable` polls compare equal (so they settle to the "cannot
/// be read" reject in [`watch_loop`]), and a real file's `Readable` hash can
/// never alias "unreadable" — there is no sentinel value to collide with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentFingerprint {
    /// A stable hash of the file's bytes ([`fingerprint_content_hash`]).
    Readable(u64),
    /// The path exists (its `stat` succeeded) but its bytes could not be read
    /// this poll.
    Unreadable,
}

/// Spawn the config-file watcher over `path` on the control-plane tokio
/// runtime. `baseline` is the document the run booted with (the currently
/// RUNNING state); `state` is the router's `AppState` (the one set of stores,
/// the bounded command bus, the drop-oldest publisher, the audit log, and the
/// watch-status slot).
pub fn spawn(
    path: PathBuf,
    baseline: MultiviewConfig,
    state: AppState,
    options: WatchOptions,
) -> ConfigWatchHandle {
    // The binary installs the handle into `AppState` BEFORE serving (MAJOR-C1)
    // and passes it via `with_handle`; when absent (tests that spawn directly)
    // a fresh handle is created and installed here.
    let handle = options.handle.clone().unwrap_or_default();
    // Install the handle into the shared state so the `promote` route (ADR-W024
    // §6) can announce its boot-file write through this watcher's suppression
    // seam. Idempotent: re-installing the same handle the binary already
    // installed before serving is a no-op-equivalent (last-writer-wins).
    state.install_watch_handle(handle.clone());
    state.config_watch.mark_active(&path.display().to_string());
    tracing::info!(
        path = %path.display(),
        poll = ?options.poll_interval,
        "watching the config file for external changes (ADR-W020)"
    );
    // The watcher starts with NO applied fingerprint (review M2): the first
    // SETTLED poll re-reads the file and diffs it against the boot baseline,
    // so an edit landing between the boot-time `load_validated` and this
    // spawn is APPLIED — never silently adopted. An unchanged file settles to
    // an empty diff and is adopted with no commands and no warnings.
    let task_handle = handle.clone();
    tokio::spawn(watch_loop(path, baseline, state, options, task_handle));
    handle
}

/// Marks the watch-status inactive when the watcher task ends — via
/// [`ConfigWatchHandle::stop`], run teardown, or an unexpected panic
/// unwinding the task (review m2): the UI must never show "watching" for a
/// dead watcher.
struct ActiveGuard(Arc<crate::ConfigWatchStatus>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.mark_inactive();
    }
}

/// A content-change fingerprint of the watched path: length + mtime + inode +
/// a hash of the file's bytes ([`ContentFingerprint`]). All four axes come from
/// a SINGLE fd opened once per poll (see [`probe`]): `len` + `content` (and the
/// bytes carried to the apply) are ONE read of that fd, and mtime/inode are an
/// `fstat` of the SAME fd. The fd pins one inode, so a rename can never pair one
/// file's metadata with another file's bytes. It is NOT a point-in-time snapshot
/// of every axis, though: the `fstat` runs after the read, so a same-inode
/// in-place rewrite landing between them can move mtime past the bytes just read.
/// That is benign — `content` (coherent with the applied bytes) is the
/// authoritative discriminator, not mtime. The byte hash makes detection
/// filesystem-independent — a same-length in-place rewrite (identical
/// len/mtime/inode, e.g. a shell `>` redirect or a non-atomic editor landing
/// within the filesystem's mtime granularity) still produces a distinct
/// fingerprint, so the already-applied short-circuit in [`watch_loop`] cannot
/// silently drop it. Re-opening the PATH each poll lands a write-temp +
/// `rename(2)` as a change (new inode); a rename that does not alter the bytes
/// then adopts without re-applying (the `last_observed == text` arm).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    inode: u64,
    content: ContentFingerprint,
}

/// The watched path as this poll read it: its change [`Fingerprint`] plus the
/// exact bytes the `len`/`content` axes were computed from. The content axis and
/// these carried bytes are ONE coherent fd read — the settled apply in
/// [`watch_loop`] uses THESE bytes, never a second read that could observe
/// different content — so the fingerprint recorded as `applied` always matches
/// the content actually applied (the probe→apply TOCTOU is closed by
/// construction). (mtime/inode are an `fstat` of the same fd, not a point-in-time
/// snapshot of the bytes; see [`Fingerprint`].)
struct Probed {
    /// The change fingerprint (`len` + `content` derived from `bytes`).
    fingerprint: Fingerprint,
    /// The bytes read this poll, or the read error's message when the path was
    /// present but its bytes could not be read (`Err(_)` iff
    /// `fingerprint.content == ContentFingerprint::Unreadable`). The error is
    /// carried, not the fingerprint's `Unreadable`, so the settled reject keeps
    /// the errno detail the old settled read surfaced — the message never enters
    /// the fingerprint, so a varying error string can't unsettle the debounce.
    bytes: Result<Vec<u8>, String>,
}

/// The file's inode — the identity axis that makes a write-temp + `rename(2)`
/// read as a change — on Unix; `0` on non-Unix, where the content hash carries
/// change detection alone. Taken from an `fstat` of the open fd, or the fallback
/// path `stat` when the file could not be opened.
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0_u64
    }
}

/// Probe the path this poll by opening it ONCE: read that fd (deriving the
/// change [`Fingerprint`]'s `len` + `content`) and `fstat` the SAME fd for
/// mtime/inode. `len` + `content` and the carried apply-bytes are one coherent
/// read; the fd pins one inode, so mtime/inode belong to the same file as those
/// bytes (a rename cannot pair another file's metadata with them). It is not a
/// point-in-time snapshot of every axis — the `fstat` follows the read (see
/// [`Fingerprint`]) — but that is benign: `content` drives detection and is
/// coherent with the applied bytes. Returns [`None`] only when the path is
/// (transiently) missing (open AND `stat` both fail); a present-but-unreadable
/// path (open denied, or opened but unreadable — e.g. a directory, `EISDIR`)
/// yields a fingerprint with [`ContentFingerprint::Unreadable`] and the read/open
/// error in place of bytes. `tokio::fs` so the open/read/`fstat` ride the
/// blocking pool, never the control-plane reactor (review m6).
///
/// Opening once — rather than a path `stat`, then a separate path read to hash,
/// then a third read to apply — makes `len`/`content` and the carried `bytes` one
/// coherent read: a same-length in-place rewrite is still a distinct fingerprint,
/// AND the settled apply uses the very bytes the fingerprint identifies, so no
/// concurrent write can make the applied content diverge from the recorded
/// fingerprint. The config file is small and this runs once per ~1–2 s poll, so
/// the read is negligible; it reads the RAW bytes — a non-UTF-8 file still
/// fingerprints and settles to the UTF-8-decode reject in [`watch_loop`].
async fn probe(path: &Path) -> Option<Probed> {
    use tokio::io::AsyncReadExt as _;
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(open_error) => {
            // Not openable. If its `stat` also fails the path is genuinely
            // (transiently) missing → `None`; otherwise it is present but
            // unreadable this poll (e.g. a permissions change), which settles to
            // the "cannot be read" reject rather than the already-applied
            // short-circuit — the open error is carried for that message.
            let meta = tokio::fs::metadata(path).await.ok()?;
            return Some(Probed {
                fingerprint: Fingerprint {
                    len: meta.len(),
                    modified: meta.modified().ok(),
                    inode: inode_of(&meta),
                    content: ContentFingerprint::Unreadable,
                },
                bytes: Err(open_error.to_string()),
            });
        }
    };
    // Read the fd to end (`len` + `content` + the bytes carried to the apply),
    // then `fstat` the SAME fd for mtime/inode. A read that fails on an opened fd
    // (e.g. the path is a directory, `EISDIR`) yields the typed `Unreadable`
    // content and carries the read error. A fresh valid fd's `fstat` does not
    // fail in practice; if it did, the content hash still drives detection, so
    // the stat axes fall back to empty rather than mis-reporting the file gone.
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes).await;
    let meta = file.metadata().await.ok();
    let modified = meta.as_ref().and_then(|m| m.modified().ok());
    let inode = meta.as_ref().map_or(0_u64, inode_of);
    match read {
        Ok(_) => {
            // `usize` → `u64` never truncates on a 64-/32-bit target; the
            // fallback is unreachable and only avoids an `as` cast.
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let content = ContentFingerprint::Readable(fingerprint_content_hash(&bytes));
            Some(Probed {
                fingerprint: Fingerprint {
                    len,
                    modified,
                    inode,
                    content,
                },
                bytes: Ok(bytes),
            })
        }
        Err(read_error) => Some(Probed {
            fingerprint: Fingerprint {
                len: meta.as_ref().map_or(0_u64, std::fs::Metadata::len),
                modified,
                inode,
                content: ContentFingerprint::Unreadable,
            },
            bytes: Err(read_error.to_string()),
        }),
    }
}

/// Fire the post-probe interpose test seam, if one is installed: write the
/// scheduled bytes to the watched path once (draining the one-shot slot). This
/// models a concurrent external writer landing between a poll's fingerprint
/// probe and its settled read/apply. Test-only (`_test-seams`): the whole seam,
/// this function included, is compiled out of the default build and the release
/// presets, so no shipped configuration of the watcher has a code path that
/// writes the config file.
#[cfg(feature = "_test-seams")]
async fn maybe_interpose(options: &WatchOptions, path: &Path) {
    let Some(slot) = options.interpose.as_ref() else {
        return;
    };
    let Some(bytes) = lock_interpose(slot).take() else {
        return;
    };
    if let Err(error) = tokio::fs::write(path, bytes).await {
        tracing::error!(
            path = %path.display(),
            %error,
            "post-probe interpose write failed (test seam)"
        );
    }
}

/// The poll loop: debounce (a new fingerprint must be observed on two
/// consecutive polls), suppress expected writes, and hand a settled change to
/// [`apply_change`].
// reason: one cohesive poll/debounce state machine — the fingerprints, the
// last-observed content, the expected-write suppression, and the two warning
// latches are read-modify-written across a single linear sequence per tick;
// splitting it scatters that state across helpers and obscures the ordering
// that makes it correct (the resume/suppression interplay especially).
#[allow(clippy::too_many_lines)]
async fn watch_loop(
    path: PathBuf,
    mut baseline: MultiviewConfig,
    state: AppState,
    options: WatchOptions,
    handle: ConfigWatchHandle,
) {
    // Inactive-on-drop: stop, teardown, and panic all flip the status
    // (review m2).
    let _active = ActiveGuard(Arc::clone(&state.config_watch));
    // The fingerprint of the content `baseline` reflects. `None` at start
    // (review M2): the first settled poll diffs the file against the boot
    // baseline, closing the boot-load → watcher-spawn window.
    let mut applied: Option<Fingerprint> = None;
    // The fingerprint of a latched-REJECTED content (invalid / unreadable /
    // a buggy expected write): handled once, never re-warned each poll —
    // and, unlike `applied`, never treated as "the file matches the running
    // configuration".
    let mut rejected: Option<Fingerprint> = None;
    let mut candidate: Option<Fingerprint> = None;
    let mut missing_polls: u32 = 0;
    let mut missing_reported = false;
    // Whether a `config-file-invalid` warning is currently raised (cleared on
    // the next valid apply, or when the file is back at the applied content).
    let mut invalid_active = false;
    // Whether a `config-file-apply-incomplete` warning is currently raised
    // (review M1: engine command(s) shed on a full bus; cleared when the
    // retried apply completes).
    let mut incomplete_active = false;
    // The last settled file CONTENT the loop adopted. Seeded with the
    // boot-load text under `start = "resume"` (ADR-W024 §4): the watcher's
    // baseline is then the RESUMED document while the file still holds the
    // boot document, so a settled observation whose content still equals this
    // seed is adopted WITHOUT applying — the unchanged boot file never
    // clobbers the resumed Running state. An edit landing in the boot window
    // differs from the seed and still applies.
    let mut last_observed: Option<String> = options.initial_observed.clone();
    // How the loop waits between polls: the wall-clock interval (production) or
    // a test's manual tick (the deterministic seam, task #131). Either way the
    // poll/debounce/apply body below is identical.
    let mut driver = PollDriver::from_options(&options);
    loop {
        if !driver.wait_for_tick().await {
            // A manual driver's test side was dropped — end the loop (the same
            // clean exit as `stop`, so a `poll_once` past teardown sees `false`
            // rather than hanging on a never-arriving ack).
            tracing::debug!(path = %path.display(), "config-file watcher poll driver closed");
            return;
        }
        if handle.stop.load(Ordering::Acquire) {
            tracing::debug!(path = %path.display(), "config-file watcher stopped");
            return;
        }
        let Some(Probed {
            fingerprint: now,
            bytes: now_bytes,
        }) = probe(&path).await
        else {
            // Mid-rename ENOENT is normal; a file that STAYS missing is
            // reported once (the running configuration is unchanged).
            candidate = None;
            missing_polls = missing_polls.saturating_add(1);
            if missing_polls >= MISSING_POLLS_BEFORE_REPORT && !missing_reported {
                missing_reported = true;
                reject(
                    &state,
                    &path,
                    "the watched config file is missing",
                    &mut invalid_active,
                );
            }
            continue;
        };
        missing_polls = 0;
        missing_reported = false;
        // TEST-ONLY seam (the `_test-seams` feature, compiled out of the default
        // build and the release presets): interpose a concurrent external writer
        // here — after this poll's
        // fingerprint probe, before its settled read/apply — to exercise the
        // probe→apply TOCTOU. Absent from the production binary entirely.
        #[cfg(feature = "_test-seams")]
        maybe_interpose(&options, &path).await;
        if applied.as_ref() == Some(&now) {
            candidate = None;
            // The file is present at the already-applied content, so any
            // latched condition has resolved without new content to apply
            // (review m5: e.g. the file was renamed away — warned missing —
            // and renamed back).
            if invalid_active {
                clear_invalid(
                    &state,
                    &path,
                    "the file is back at the already-applied content",
                    &mut invalid_active,
                );
            }
            if incomplete_active {
                // A partial (shed) apply was pending and the ORIGINAL content
                // returned: nothing is pending for the engine any more, but the
                // stores may have followed the abandoned content. Under the
                // config-mutation lock (ordered against the persister + REST
                // mutations), re-converge the stores to the running baseline and
                // clear the warning. The adopted SNAPSHOT needs no update here:
                // per-section adoption (MAJOR-B round 5) only ever recorded the
                // sections the engine actually applied LIVE, so a shed left it at
                // the adopted state — there is no global gate to unstick.
                let _mutation = state.lock_config_mutation().await;
                resync_all_stores(&state, ACTOR, &baseline);
                clear_apply_incomplete(&state, &path, &mut incomplete_active);
                state.running_changed.notify_one();
            }
            continue;
        }
        if rejected.as_ref() == Some(&now) {
            // Already rejected and warned exactly once; latched until the
            // content changes again.
            candidate = None;
            continue;
        }
        if candidate.as_ref() != Some(&now) {
            // First sighting of this fingerprint: wait one more poll for the
            // writer to settle (editors multi-write).
            candidate = Some(now);
            continue;
        }
        // Stable across two polls: act on the SAME bytes the fingerprint was
        // built from — carried out of `probe`, never a second read — so the
        // content applied is exactly the content the recorded fingerprint
        // identifies (no probe→apply TOCTOU). The expected-write check is
        // content-paired (review m3).
        candidate = None;
        // `probe` found the path present but its bytes unreadable
        // (`ContentFingerprint::Unreadable`): reject, exactly as a failed settled
        // read did.
        let bytes = match now_bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                rejected = Some(now);
                reject(
                    &state,
                    &path,
                    &format!("the file cannot be read: {error}"),
                    &mut invalid_active,
                );
                continue;
            }
        };
        // `probe` read these bytes and folded their hash into `now.content`;
        // decode them here (the UTF-8 gate). A non-UTF-8 file is rejected,
        // exactly as the old settled `read_to_string` did.
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => {
                rejected = Some(now);
                reject(
                    &state,
                    &path,
                    &format!("the file is not valid UTF-8: {error}"),
                    &mut invalid_active,
                );
                continue;
            }
        };
        if handle.consume_expected_for(&text) {
            if adopt_expected_text(&path, &text, &mut baseline) {
                applied = Some(now);
                rejected = None;
                last_observed = Some(text);
            } else {
                rejected = Some(now);
            }
            continue;
        }
        // This settled content matches no banked announcement: any
        // announcement whose write already LANDED was superseded by it
        // (writes are ordered) — drain those tokens so a stale one can never
        // eat a later real edit carrying the same bytes (review B1 (2)).
        // Unlanded announcements (writes still in flight) survive (m3, pinned).
        handle.drain_landed();
        if last_observed.as_deref() == Some(text.as_str()) {
            // ADR-W024 §4: the CONTENT is unchanged — a touch/identical
            // rewrite, or under `start = "resume"` the unchanged boot file
            // observed for the first time (a different inode/mtime than the
            // resumed baseline, but the same bytes). Nothing to apply; the
            // latched conditions resolve exactly as in the fingerprint-match
            // arm above.
            applied = Some(now);
            rejected = None;
            if invalid_active {
                clear_invalid(
                    &state,
                    &path,
                    "the file is back at the last-observed content",
                    &mut invalid_active,
                );
            }
            if incomplete_active {
                // MAJOR-B round 5: a prior shed was pending and the content
                // reverted to the last-observed (adopted) state — under the
                // config-mutation lock, re-converge the stores and clear the
                // warning. The adopted snapshot needs no update (per-section
                // adoption already kept it at the adopted state through the
                // shed).
                let _mutation = state.lock_config_mutation().await;
                resync_all_stores(&state, ACTOR, &baseline);
                clear_apply_incomplete(&state, &path, &mut incomplete_active);
                state.running_changed.notify_one();
            }
            continue;
        }
        // MAJOR-B round 4: the whole mutate → submit → adopt-snapshot of a
        // file-watch apply runs UNDER the config-mutation lock, so the persister
        // (which takes the same lock) can never compose mid-apply and the
        // adopted snapshot advances atomically with the engine submit — the same
        // discipline the REST mutation handlers and the revert route follow.
        // `apply_change` itself never re-acquires the lock (only the watcher and
        // the revert route, which already holds it, are callers), so there is no
        // re-entrancy.
        let apply_result = {
            let _mutation = state.lock_config_mutation().await;
            apply_change(
                &path,
                &text,
                &mut baseline,
                &state,
                &mut invalid_active,
                &mut incomplete_active,
            )
        };
        match apply_result {
            ApplyResult::Settled => {
                applied = Some(now);
                rejected = None;
                last_observed = Some(text);
            }
            ApplyResult::RejectedInvalid => {
                rejected = Some(now);
            }
            ApplyResult::Retry => {
                // Review M1: engine command(s) were shed — leave `applied`,
                // the baseline AND `last_observed` un-advanced so a later poll
                // re-reads and re-applies the whole (idempotent) change.
            }
        }
    }
}

/// Adopt an **expected** (server-side) write as the new baseline without
/// applying anything — the writer already applied the state it serialized. A
/// write that does not parse/validate is still warned (a buggy writer must
/// never be silent), and the baseline is kept. Returns whether the write was
/// adopted.
fn adopt_expected_text(path: &Path, text: &str, baseline: &mut MultiviewConfig) -> bool {
    match parse_validated(text) {
        Ok(next) => {
            tracing::debug!(
                path = %path.display(),
                "expected (self) config write adopted as the new baseline; no reload"
            );
            *baseline = next;
            true
        }
        Err(reason) => {
            tracing::warn!(
                path = %path.display(),
                reason = %reason,
                "an EXPECTED config write does not validate — the writer is buggy; \
                 keeping the previous baseline"
            );
            false
        }
    }
}

/// Parse + validate the whole document text, with a human-readable reason on
/// any failure.
fn parse_validated(text: &str) -> Result<MultiviewConfig, String> {
    let next = MultiviewConfig::load_from_toml(text)
        .map_err(|e| format!("the document does not parse: {e}"))?;
    next.validate()
        .map_err(|e| format!("the document does not validate: {e}"))?;
    Ok(next)
}

/// How the loop must advance its fingerprints after [`apply_change`].
enum ApplyResult {
    /// Fully handled — applied (or identical content adopted); the content's
    /// fingerprint becomes the `applied` baseline fingerprint.
    Settled,
    /// The document was invalid: warned + latched once; the fingerprint is
    /// remembered as rejected (never as applied).
    RejectedInvalid,
    /// Only PARTIALLY applied — engine command(s) shed on a full bus (review
    /// M1). Neither the baseline nor any fingerprint advanced: a later poll
    /// re-reads and re-applies the whole (idempotent) change.
    Retry,
}

/// Handle one settled external change: whole-document validate, per-section
/// diff, apply through the one machinery, advance the baseline — unless a
/// command was shed, in which case NOTHING advances and the apply is retried
/// on a later poll (review M1).
fn apply_change(
    path: &Path,
    text: &str,
    baseline: &mut MultiviewConfig,
    state: &AppState,
    invalid_active: &mut bool,
    incomplete_active: &mut bool,
) -> ApplyResult {
    let next = match parse_validated(text) {
        Ok(next) => next,
        Err(reason) => {
            reject(state, path, &reason, invalid_active);
            return ApplyResult::RejectedInvalid;
        }
    };
    let diff = ConfigDiff::between(baseline, &next);
    if diff.is_empty() {
        // A touch / rewrite with identical content: adopt silently. If a
        // partial (shed) apply was pending, the file was REVERTED to the
        // running baseline: nothing is pending for the engine any more, but
        // the stores may have followed the abandoned content — re-converge
        // them to the file before clearing the interim warning.
        if *incomplete_active {
            resync_all_stores(state, ACTOR, &next);
            clear_apply_incomplete(state, path, incomplete_active);
        }
        *baseline = next;
        clear_invalid(
            state,
            path,
            "a subsequent valid write applied",
            invalid_active,
        );
        // MAJOR-B round 5: an identical (empty-diff) rewrite changes nothing the
        // engine runs, so the adopted snapshot is already correct — no wholesale
        // adoption (that was the round-4 over-adoption). Re-fire so a pending
        // persist (e.g. after the incomplete re-converge above) captures the
        // already-correct snapshot.
        state.running_changed.notify_one();
        tracing::debug!(path = %path.display(), "config file rewritten with identical content");
        return ApplyResult::Settled;
    }
    let outcome = apply_document_diff(state, ACTOR, &diff, &next);
    // The document is valid either way — the invalid latch clears even when
    // the apply is incomplete (the conditions are independent).
    clear_invalid(
        state,
        path,
        "a subsequent valid write applied",
        invalid_active,
    );
    if outcome.shed > 0 {
        // Review M1: NEVER claim "applied" while a command was shed. Leave
        // the baseline un-advanced, record the partial apply as a rejection,
        // raise the interim warning, and let the next poll retry the whole
        // (idempotent) apply. Restart-pending accounting also waits for the
        // completed retry, so the status never gets ahead of the engine.
        state.config_watch.record_rejected(
            now_ms(state),
            &format!(
                "partially applied: {} engine command(s) shed on a full command bus; \
                 the watcher retries the whole change on its next poll",
                outcome.shed
            ),
        );
        publish_apply_incomplete(state, path, outcome.shed, incomplete_active);
        // MAJOR-B round 4: a command was SHED — the engine did NOT adopt this
        // document, so the adopted snapshot is left UNCHANGED (it still reflects
        // the last fully-adopted document). The idempotent retry on a later poll
        // adopts it when it lands. active.toml therefore never captures the
        // mid-retry store state.
        return ApplyResult::Retry;
    }
    *baseline = next;
    clear_apply_incomplete(state, path, incomplete_active);
    // MAJOR-B round 5: the apply landed completely. The adopted snapshot was
    // ALREADY advanced PER-SECTION inside `apply_document_diff` for exactly the
    // sections the engine applied LIVE (synthetic sources via UpsertSource,
    // layout via ApplyLayout) — restart-only sections (a non-synthetic source,
    // a Class-2 canvas/layout, outputs/probes/devices/sync_groups/audio) applied
    // NOTHING to the snapshot, so they never leak into `active.toml`. No
    // wholesale adoption here (that was the round-4 over-adoption). Re-fire so
    // the debounced persister captures the per-section-updated snapshot.
    state.running_changed.notify_one();
    state
        .config_watch
        .record_applied(now_ms(state), &outcome.summary);
    if !outcome.restart.is_empty() {
        state
            .config_watch
            .add_restart_pending(outcome.restart.iter().cloned());
        // Warn with the LATCHED union (every section pending since start), so
        // the single coalesced warning entry stays complete.
        let pending = state.config_watch.snapshot().restart_pending;
        publish_requires_restart(state, &pending);
    }
    tracing::info!(
        path = %path.display(),
        summary = %outcome.summary,
        "applied an external config-file change (ADR-W020)"
    );
    ApplyResult::Settled
}

/// What one document apply did: the human summary, the restart-only sections,
/// and how many engine commands were shed on a full bus (review M1: any shed
/// means the apply is incomplete and must be retried).
///
/// Returned by the public [`apply_document_diff`] so the revert-to-start route
/// (ADR-W024) can report the per-section summary, the `restart_only` names, and
/// the shed-aware partial-apply outcome.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// A human-readable, semicolon-joined summary of every section the apply
    /// touched (and the restart-pending tail, if any). The watcher records
    /// this on `watch-status`.
    pub summary: String,
    /// The individual per-section summary parts (the un-joined form of
    /// [`summary`](Self::summary), without the restart-pending tail) — the
    /// revert-to-start route (ADR-W024) reports these as its `summary` array.
    pub parts: Vec<String>,
    /// The sections that cannot hot-apply and need a restart (the route reports
    /// these as `restart_only`).
    pub restart: BTreeSet<String>,
    /// How many engine commands were shed on a full command bus: `0` means the
    /// apply landed completely; `>0` means it is partial and must be retried.
    pub shed: u32,
}

/// Apply a validated per-section diff through the one apply machinery:
/// engine commands for the live-appliable parts, store reseeds for everything
/// the UI mirrors, restart-section accounting for the rest.
///
/// The `actor` names who the resulting audit entries are recorded under: the
/// config-file watcher passes [`ACTOR`] (`"config-file"`); the
/// revert-to-start route (ADR-W024) passes the authenticated principal. This
/// is the single diff→apply implementation behind both the file watcher and
/// the revert route — the dependency arrow is `cli → control`, so it lives
/// here where the route can reach it.
pub fn apply_document_diff(
    state: &AppState,
    actor: &str,
    diff: &ConfigDiff,
    next: &MultiviewConfig,
) -> ApplyOutcome {
    let mut restart: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    let mut shed: u32 = 0;

    // 1. Sources FIRST (FIFO bus): a layout rebinding to a just-added source
    //    must find its store registered in the same frame-boundary pass.
    if !diff.sources.is_empty() {
        parts.push(apply_source_changes(state, diff, &mut restart, &mut shed));
        resync_store(state, actor, &state.sources, &desired_sources(next));
    }

    // 1b. A pure SOURCE REORDER (task #130) has NO live engine seam: source
    //     declaration order affects only the cold-start test-pattern palette
    //     index (`run.rs`), never a live render path (cells bind by `input_id`;
    //     the live source stores are id-keyed and `list()` is id-sorted, so the
    //     order is not even representable in the store/`active.toml`). Unlike the
    //     overlay reorder (a live `ReorderOverlays`), there is NO `ReorderSources`
    //     command — that would be theater. So it is reported honestly as a
    //     RESTART-pending (Class-2) delta: it applies on the next restart (which
    //     re-reads the boot file's order), never silently dropped. The store
    //     resync above (if any content changed) is order-agnostic; nothing live
    //     is submitted for the reorder itself.
    if diff.sources_reordered {
        restart.insert("sources".to_owned());
        parts.push("sources reordered (applies on restart)".to_owned());
    }

    // 2. Canvas: the pinned signal is Class-2 (ADR-R004) and the cosmetic
    //    axes have no live render path either — restart, never silently.
    if diff.canvas_signal_changed || diff.canvas_cosmetic_changed {
        restart.insert("canvas".to_owned());
        if diff.canvas_signal_changed {
            tracing::warn!(
                "config file changed the canvas geometry/cadence — a Class-2 change \
                 (the output canvas is pinned for the session, ADR-R004); it applies on restart"
            );
        } else {
            tracing::warn!(
                "config file changed canvas pixel-format/background/colour; it applies on restart"
            );
        }
        parts.push("canvas changed".to_owned());
    }

    // 3. Layout/cells: the SAME resolve+solve+Class-1-gate as the
    //    apply-layout route, then the same ApplyLayout command (ADR-W019).
    if diff.layout_changed {
        parts.push(apply_layout_change(
            state,
            actor,
            diff,
            next,
            &mut restart,
            &mut shed,
        ));
    }

    // 3b. Overlays: mirror the REST overlay routes (ADR-W024 round 6 / F2,
    //     baseline-derived round 7). An overlay is LIVE-SHEDDABLE — on an
    //     overlay-capable run the REST routes submit `UpsertOverlay`/
    //     `RemoveOverlay` and adopt per landed delta, so the watcher MUST do the
    //     same (the round-5 watcher treated overlays as restart-only here,
    //     diverging from REST and leaking a live file-edit out of `active.toml`).
    //     `apply_overlay_changes` derives the per-overlay delta from
    //     `diff.overlays` — the FILE-BASELINE `ConfigDiff` (the same shape and
    //     stability `diff.sources` has), which is STABLE across shed retries;
    //     it never reads the control store (a store-derived delta self-erases
    //     once `resync_store` reseeds the store to `next`, dropping a shed edit
    //     — the round-7 lost-update). The loop below skips `"overlays"`.
    if diff.changed_sections.contains("overlays") {
        parts.push(apply_overlay_changes(
            state,
            actor,
            diff,
            next,
            &mut restart,
            &mut shed,
        ));
    }

    // 4. Every other changed section: reseed its store where one exists (the
    //    UI's truth follows the file) + restart accounting (no live path yet).
    for section in &diff.changed_sections {
        // Overlays are handled live above (3b) — never as a generic restart-only
        // reseed here (that would double-account and miss the live adopt path).
        if *section == "overlays" {
            continue;
        }
        restart.insert((*section).to_owned());
        match *section {
            "outputs" => resync_store(state, actor, &state.outputs, &desired_outputs(next)),
            "probes" => resync_store(state, actor, &state.probes, &desired_probes(next)),
            "devices" => {
                resync_store(state, actor, &state.devices, &desired_devices(next));
                for device in &next.devices {
                    state.device_status.ensure(&device.id);
                }
            }
            "sync_groups" => {
                resync_store(state, actor, &state.sync_groups, &desired_sync_groups(next));
            }
            "audio" => resync_audio(state, actor, next),
            // ADR-W024 round 6 (F1): salvos + tally_profiles are store-backed
            // running state composed INTO `active.toml` — reseed the definition
            // store so a boot-file edit follows the file (the same path a
            // boot-file `outputs` edit rides). A pure store edit (no engine
            // command), restart-only for the engine's recall (the definition
            // takes effect on the next arm/take), so the store == adopted and
            // `active.toml` composes it straight from the store.
            "salvos" => resync_salvos(state, actor, next),
            "tally_profiles" => resync_tally_profiles(state, actor, next),
            // No store is boot-seeded for these; the file itself is the
            // durable truth and the restart warning names them.
            _ => {}
        }
        parts.push(format!("{section} changed"));
    }

    // `parts` (the per-section summary parts) is reported verbatim by the
    // revert route; `summary` is the joined human string (with the
    // restart-pending tail) the watcher records on `watch-status`.
    let section_parts = parts.clone();
    if !restart.is_empty() {
        let names: Vec<&str> = restart.iter().map(String::as_str).collect();
        parts.push(format!("restart pending: {}", names.join(", ")));
    }
    ApplyOutcome {
        summary: parts.join("; "),
        parts: section_parts,
        restart,
        shed,
    }
}

/// Adopt a LANDED source into the boot-model snapshot, if a boot model is wired
/// (ADR-W024 round 5). Called from the file-watch apply ONLY when an
/// `UpsertSource` actually lands on the bus — never wholesale.
fn adopt_source_if_modeled(state: &AppState, source: multiview_config::Source) {
    if let Some(model) = state.boot_model.as_ref() {
        model.adopt_source(source);
    }
}

/// Drop a LANDED-removed source from the boot-model snapshot, if a boot model is
/// wired (ADR-W024 round 5). Called from the file-watch apply ONLY when a
/// `RemoveSource` actually lands.
fn unadopt_source_if_modeled(state: &AppState, id: &str) {
    if let Some(model) = state.boot_model.as_ref() {
        model.unadopt_source(id);
    }
}

/// Apply the source diff exactly as the sources routes do (ADR-W018):
/// synthetic add/edit ⇒ live `UpsertSource`; a synthetic→decoded kind change
/// ⇒ live `RemoveSource` (stop the stale generator) + restart for the new
/// document; decoded add/edit ⇒ restart, honestly; any removal ⇒ live
/// `RemoveSource` (bound tiles ride their `on_loss` slate).
fn apply_source_changes(
    state: &AppState,
    diff: &ConfigDiff,
    restart: &mut BTreeSet<String>,
    shed: &mut u32,
) -> String {
    let mut described: Vec<String> = Vec::new();
    for change in &diff.sources {
        match change {
            SourceChange::Added(source) => {
                described.push(format!("{} added", source.id));
                if source.kind.is_synthetic() {
                    if submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: source.clone(),
                        },
                    ) {
                        // ADR-W024 round 5: a LANDED synthetic add is adopted —
                        // record it in the snapshot per-section (never wholesale).
                        adopt_source_if_modeled(state, (**source).clone());
                    } else {
                        *shed = shed.saturating_add(1);
                    }
                } else {
                    restart.insert("sources".to_owned());
                    tracing::warn!(
                        source = %source.id,
                        "config file added a non-synthetic source; it applies on restart \
                         (consistent with the API's restart semantics)"
                    );
                }
            }
            SourceChange::Changed { previous, next } => {
                described.push(format!("{} changed", next.id));
                if next.kind.is_synthetic() {
                    if submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: next.clone(),
                        },
                    ) {
                        adopt_source_if_modeled(state, (**next).clone());
                    } else {
                        *shed = shed.saturating_add(1);
                    }
                } else {
                    if previous.kind.is_synthetic() {
                        // Mirror the sources route: stop the stale generator
                        // now; a frozen synthetic pretending to be the new
                        // URL would be dishonest.
                        if submit(
                            state,
                            Command::RemoveSource {
                                op: OperationId::new(),
                                id: next.id.clone(),
                            },
                        ) {
                            // The LANDED stop drops the old synthetic from the
                            // snapshot; the new decoded source is restart-only
                            // (NOT adopted live), so it does not enter here.
                            unadopt_source_if_modeled(state, &next.id);
                        } else {
                            *shed = shed.saturating_add(1);
                        }
                    }
                    restart.insert("sources".to_owned());
                    tracing::warn!(
                        source = %next.id,
                        "config file changed a source to a non-synthetic kind; it applies on restart"
                    );
                }
            }
            SourceChange::Removed(id) => {
                described.push(format!("{id} removed"));
                if submit(
                    state,
                    Command::RemoveSource {
                        op: OperationId::new(),
                        id: id.clone(),
                    },
                ) {
                    unadopt_source_if_modeled(state, id);
                } else {
                    *shed = shed.saturating_add(1);
                }
            }
        }
    }
    format!("sources: {}", described.join(", "))
}

/// Adopt a LANDED overlay upsert into the boot-model snapshot, if a boot model
/// is wired (ADR-W024 round 6 / F2). Called from the file-watch overlay apply
/// ONLY when an `UpsertOverlay` actually lands on the bus — never wholesale.
fn adopt_overlay_if_modeled(state: &AppState, overlay: multiview_config::Overlay) {
    if let Some(model) = state.boot_model.as_ref() {
        model.adopt_overlay(overlay);
    }
}

/// Drop a LANDED-removed overlay from the boot-model snapshot, if a boot model
/// is wired (ADR-W024 round 6 / F2). Called ONLY when a `RemoveOverlay` lands.
fn unadopt_overlay_if_modeled(state: &AppState, id: &str) {
    if let Some(model) = state.boot_model.as_ref() {
        model.unadopt_overlay(id);
    }
}

/// Apply the overlays diff exactly as the REST overlay routes do (ADR-W024
/// round 6 / F2). The round-5 watcher treated `overlays` as a restart-only
/// store reseed, diverging from the REST routes (which submit
/// `UpsertOverlay`/`RemoveOverlay` and adopt per landed delta on an
/// overlay-capable run) and so leaking a live file-edit out of `active.toml`.
///
/// Parity with `overlays.rs::live_apply_upsert`/`live_apply_remove`:
/// * With NO live overlay capability (`state.live_apply.overlays` is `None` —
///   the store-only / software-path posture), every overlay change is
///   restart-only: reseed the store and leave `overlays` in the restart set.
///   Nothing is adopted (the engine has no live overlay seam), so `active.toml`
///   does not capture it — correct, because the engine only takes it on restart.
/// * With a capability, EVERY changed/added overlay rides `UpsertOverlay` and
///   each removal rides `RemoveOverlay` on the bounded bus (the working-set
///   mirror stays coherent; `renders` decides only the REST `X-Multiview-Apply`
///   header, not adoption). A LANDED command adopts/unadopts the snapshot
///   per-section; a SHED command counts toward `shed` (the whole apply retries)
///   and keeps the section restart-pending. The caller's `restart` set already
///   carries `overlays`; a fully-landed live apply REMOVES it (nothing pending).
///
/// The per-overlay delta comes from `diff.overlays` — the FILE BASELINE diff
/// ([`OverlayChange`], computed by `ConfigDiff::between(baseline, next)`), which
/// is STABLE across shed retries, exactly like the source path. Round 6 derived
/// it from the mutated control store and reseeded the store to `next` before the
/// retry resolved; on a shed the file baseline did not advance, so the next
/// retry saw the store already == `next`, submitted nothing, falsely reported
/// `all_landed`, and silently dropped the shed overlay edit (round-7 fix).
///
/// The store is reseeded last so the UI overlay list follows the file in every
/// case (matching `seed_resources` / the other resync paths).
fn apply_overlay_changes(
    state: &AppState,
    actor: &str,
    diff: &ConfigDiff,
    next: &MultiviewConfig,
    restart: &mut BTreeSet<String>,
    shed: &mut u32,
) -> String {
    let capability = state.live_apply.overlays.clone();
    let mut described: Vec<String> = Vec::new();
    let mut all_landed = capability.is_some();

    if capability.is_some() {
        // The delta is the BASELINE-derived per-overlay change list, stable
        // across retries — never the mutated store. Every added/changed overlay
        // rides `UpsertOverlay` (every kind, so the engine's working set stays
        // coherent — ADR-W022); every removal rides `RemoveOverlay`.
        for change in &diff.overlays {
            match change {
                OverlayChange::Added(overlay) | OverlayChange::Changed(overlay) => {
                    described.push(format!("{} upserted", overlay.id));
                    if submit(
                        state,
                        Command::UpsertOverlay {
                            op: OperationId::new(),
                            overlay: overlay.clone(),
                        },
                    ) {
                        adopt_overlay_if_modeled(state, (**overlay).clone());
                    } else {
                        *shed = shed.saturating_add(1);
                        all_landed = false;
                    }
                }
                OverlayChange::Removed(id) => {
                    described.push(format!("{id} removed"));
                    if submit(
                        state,
                        Command::RemoveOverlay {
                            op: OperationId::new(),
                            id: id.clone(),
                        },
                    ) {
                        unadopt_overlay_if_modeled(state, id);
                    } else {
                        *shed = shed.saturating_add(1);
                        all_landed = false;
                    }
                }
            }
        }

        // Task #130: a pure draw-order REORDER of equal-z overlays is invisible
        // to the per-id delta above (`diff.overlays` is empty for it), yet
        // declaration order is the equal-`z` draw-order tie-break. `UpsertOverlay`
        // edits the engine's working mirror IN PLACE by id, so re-submitting
        // upserts can never re-sequence it — a dedicated `ReorderOverlays`
        // (a pure permutation) does. `diff.overlays_reordered` is baseline-derived
        // (stable across shed retries, like `diff.overlays`); the `order` is the
        // file's full desired id sequence, submitted AFTER any add/remove so the
        // set is complete before it is re-sequenced. A shed counts toward the M1
        // retry and keeps the section restart-pending. No adopted-snapshot change:
        // a reorder alters neither which overlays exist nor their content (the
        // ADR-W024 snapshot tracks content, adopted per-id above), only draw order.
        if diff.overlays_reordered {
            let order: Vec<String> = next.overlays.iter().map(|o| o.id.clone()).collect();
            described.push("draw order re-sequenced".to_owned());
            if !submit(
                state,
                Command::ReorderOverlays {
                    op: OperationId::new(),
                    order,
                },
            ) {
                *shed = shed.saturating_add(1);
                all_landed = false;
            }
        }
    }

    // The UI overlay list follows the file in every case (capability or not).
    resync_store(state, actor, &state.overlays, &desired_overlays(next));

    if all_landed {
        // Every overlay command landed live (and was adopted): the section is
        // fully applied, so it is NOT restart-pending.
        restart.remove("overlays");
        format!("overlays applied live: {}", described.join(", "))
    } else if capability.is_some() {
        // A shed left the apply incomplete; keep it restart-pending (the
        // watcher retries the whole apply on its next poll, review M1).
        restart.insert("overlays".to_owned());
        format!(
            "overlays partially applied (shed; retried): {}",
            described.join(", ")
        )
    } else {
        // No live overlay seam on this run path: honest restart-only.
        restart.insert("overlays".to_owned());
        "overlays changed (applies on restart)".to_owned()
    }
}

/// Apply a layout/cells change through the shared route machinery
/// ([`resolve_layout_document`] → `ApplyLayout`), and reseed the working
/// layout in the layouts repository so the UI editor follows the file.
fn apply_layout_change(
    state: &AppState,
    actor: &str,
    diff: &ConfigDiff,
    next: &MultiviewConfig,
    restart: &mut BTreeSet<String>,
    shed: &mut u32,
) -> String {
    let id = state
        .working_layout_id
        .clone()
        .unwrap_or_else(|| "working".to_owned());
    let Some(body) = working_layout_body(next) else {
        // Serialising plain derived config types cannot fail in practice;
        // surfaced honestly rather than panicking on the watcher task.
        restart.insert("layout".to_owned());
        return "layout changed (could not serialize; applies on restart)".to_owned();
    };
    reseed_working_layout(state, actor, &id, &body);
    if diff.canvas_signal_changed {
        // The Class-2 canvas change already rides the restart surface; a live
        // apply against the pinned canvas would be refused anyway.
        restart.insert("layout".to_owned());
        tracing::warn!(
            "config file changed the layout alongside a Class-2 canvas change; \
             both apply on restart"
        );
        return "layout changed (Class-2 canvas; applies on restart)".to_owned();
    }
    match resolve_layout_document(&id, &body, state.running_canvas.as_ref()) {
        Ok(resolved) => {
            // Review M1: claim "applied live" ONLY when the command actually
            // landed on the bus; a shed leaves the apply incomplete and the
            // watcher retries it on a later poll.
            if submit(
                state,
                Command::ApplyLayout {
                    op: OperationId::new(),
                    layout: id,
                    document: Some(Box::new(resolved)),
                },
            ) {
                // ADR-W024 round 5: the ApplyLayout LANDED — the engine now runs
                // `next`'s canvas/layout/cells, so adopt them into the snapshot
                // (per-section; never wholesale). A shed leaves the snapshot at
                // the prior adopted layout (the retry adopts when it lands).
                if let Some(model) = state.boot_model.as_ref() {
                    model.adopt_layout(
                        next.canvas.clone(),
                        next.layout.clone(),
                        next.cells.clone(),
                    );
                }
                "layout applied live".to_owned()
            } else {
                *shed = shed.saturating_add(1);
                "layout apply shed (retried on the next poll)".to_owned()
            }
        }
        Err(error) => {
            // The document validated as a whole, so this is the Class-1 gate
            // (or an unseeded running canvas failing closed) — restart-only.
            restart.insert("layout".to_owned());
            tracing::warn!(
                error = %error,
                "config file layout change cannot apply live; it applies on restart"
            );
            "layout changed (held; applies on restart)".to_owned()
        }
    }
}

/// The authored `{canvas, layout, cells}` working-layout body (the exact
/// shape `seed_resources` seeds and the apply-layout route resolves).
fn working_layout_body(config: &MultiviewConfig) -> Option<serde_json::Value> {
    let canvas = to_body(&config.canvas)?;
    let layout = to_body(&config.layout)?;
    let cells = to_body(&config.cells)?;
    Some(serde_json::json!({
        "canvas": canvas,
        "layout": layout,
        "cells": cells,
    }))
}

/// Update (or recreate) the working layout in the layouts repository so the
/// UI layout editor reflects the file immediately.
fn reseed_working_layout(state: &AppState, actor: &str, id: &str, body: &serde_json::Value) {
    let input = LayoutInput {
        name: id.to_owned(),
        body: body.clone(),
    };
    let updated = state
        .repository
        .update_layout(id, input.clone())
        .or_else(|_| state.repository.create_layout(id, input));
    match updated {
        Ok(_) => {
            state.audit(
                actor,
                AuditAction::Update,
                crate::repository::LAYOUT_KIND,
                id,
                Some(body.clone()),
            );
        }
        Err(error) => {
            tracing::warn!(
                layout = %id,
                error = %error,
                "could not reseed the working layout from the config file"
            );
        }
    }
}

/// Bring a resource store's contents in line with the file's desired state:
/// create/update/delete by id, audit-logged as actor `config-file`. A store
/// fault is warned and skipped — a flaky store must never wedge the watcher.
fn resync_store(
    state: &AppState,
    actor: &str,
    store: &Arc<dyn ResourceRepository>,
    desired: &[(String, ResourceInput)],
) {
    let existing: Vec<String> = match store.list() {
        Ok(list) => list.into_iter().map(|v| v.resource.id).collect(),
        Err(error) => {
            tracing::warn!(kind = store.kind(), error = %error, "config-file resync: list failed");
            return;
        }
    };
    for (id, input) in desired {
        let result = if existing.iter().any(|e| e == id) {
            store.update(id, input.clone()).map(|_| AuditAction::Update)
        } else {
            store.create(id, input.clone()).map(|_| AuditAction::Create)
        };
        match result {
            Ok(action) => state.audit(actor, action, store.kind(), id, Some(input.body.clone())),
            Err(error) => {
                tracing::warn!(kind = store.kind(), id = %id, error = %error, "config-file resync: write failed");
            }
        }
    }
    for id in existing {
        if !desired.iter().any(|(want, _)| want == &id) {
            match store.delete(&id) {
                Ok(()) => state.audit(actor, AuditAction::Delete, store.kind(), &id, None),
                Err(error) => {
                    tracing::warn!(kind = store.kind(), id = %id, error = %error, "config-file resync: delete failed");
                }
            }
        }
    }
}

/// Replace the audio-routing singleton from the file's `[audio]` block (a
/// bounded CAS loop over the versioned store). A REMOVED block cannot be
/// expressed on the singleton store today; the restart warning covers it.
fn resync_audio(state: &AppState, actor: &str, next: &MultiviewConfig) {
    let Some(routing) = next.audio.clone() else {
        tracing::warn!(
            "config file removed the [audio] block; the audio-routing store keeps its last \
             value until restart"
        );
        return;
    };
    let mut version = state.audio_routing.version();
    for _ in 0_u8..4 {
        match state.audio_routing.replace_if(version, routing.clone()) {
            Ok(_) => {
                state.audit(
                    actor,
                    AuditAction::Update,
                    crate::AUDIO_ROUTING_KIND,
                    crate::AUDIO_ROUTING_ID,
                    to_body(&routing),
                );
                return;
            }
            Err(actual) => version = actual,
        }
    }
    tracing::warn!("config-file resync: the audio-routing store kept changing; giving up");
}

/// Bring the salvo DEFINITION store in line with the file's `[[salvos]]`
/// (ADR-W024 round 6 / F1): create/update by id, delete the absent — audited as
/// `actor`, mirroring [`resync_store`] for the typed [`crate::salvo_store::SalvoRepository`].
/// A salvo definition is a pure control-plane store edit (no engine command),
/// so the store IS the adopted state; `active.toml` is composed straight from
/// it. The engine's salvo RECALL applies the new definition on the next
/// arm/take (restart-class for the running recall), so the section stays in the
/// restart-pending set, but the durable definition follows the file at once.
fn resync_salvos(state: &AppState, actor: &str, next: &MultiviewConfig) {
    use crate::salvo_store::SALVO_KIND;
    let existing: Vec<String> = match state.salvos.list() {
        Ok(list) => list.into_iter().map(|v| v.salvo.id).collect(),
        Err(error) => {
            tracing::warn!(error = %error, "config-file resync: listing salvos failed");
            return;
        }
    };
    for salvo in &next.salvos {
        let id = salvo.id.clone();
        let result = if existing.iter().any(|e| e == &id) {
            state.salvos.update(&id, salvo.clone())
        } else {
            state.salvos.create(salvo.clone())
        };
        match result {
            Ok(versioned) => state.audit(
                actor,
                AuditAction::Update,
                SALVO_KIND,
                &id,
                to_body(&versioned.salvo),
            ),
            Err(error) => {
                tracing::warn!(id = %id, error = %error, "config-file resync: salvo write failed");
            }
        }
    }
    for id in existing {
        if !next.salvos.iter().any(|s| s.id == id) {
            match state.salvos.delete(&id) {
                Ok(()) => state.audit(actor, AuditAction::Delete, SALVO_KIND, &id, None),
                Err(error) => {
                    tracing::warn!(id = %id, error = %error, "config-file resync: salvo delete failed");
                }
            }
        }
    }
}

/// Bring the tally-profile store in line with the file's `[[tally_profiles]]`
/// (ADR-W024 round 6 / F1): create/update by id, delete the absent — audited as
/// `actor`. Like salvos, a profile is a pure control-plane store edit (no engine
/// command), so the store IS the adopted state composed into `active.toml`.
fn resync_tally_profiles(state: &AppState, actor: &str, next: &MultiviewConfig) {
    use crate::tally_state::TALLY_PROFILE_KIND;
    let existing: Vec<String> = match state.tally_profiles.list() {
        Ok(list) => list.into_iter().map(|v| v.profile.id).collect(),
        Err(error) => {
            tracing::warn!(error = %error, "config-file resync: listing tally profiles failed");
            return;
        }
    };
    for profile in &next.tally_profiles {
        let id = profile.id.clone();
        match state.tally_profiles.put(profile.clone()) {
            Ok(versioned) => state.audit(
                actor,
                AuditAction::Update,
                TALLY_PROFILE_KIND,
                &id,
                to_body(&versioned.profile),
            ),
            Err(error) => {
                tracing::warn!(id = %id, error = %error, "config-file resync: tally-profile write failed");
            }
        }
    }
    for id in existing {
        if !next.tally_profiles.iter().any(|p| p.id == id) {
            match state.tally_profiles.delete(&id) {
                Ok(()) => state.audit(actor, AuditAction::Delete, TALLY_PROFILE_KIND, &id, None),
                Err(error) => {
                    tracing::warn!(id = %id, error = %error, "config-file resync: tally-profile delete failed");
                }
            }
        }
    }
}

/// The sources store's desired contents for `config` (mirrors `seed_resources`).
fn desired_sources(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .sources
        .iter()
        .filter_map(|source| {
            Some((
                source.id.clone(),
                ResourceInput {
                    name: source
                        .display_name
                        .clone()
                        .unwrap_or_else(|| source.id.clone()),
                    body: to_body(source)?,
                },
            ))
        })
        .collect()
}

/// The outputs store's desired contents: stable index-derived `output-{n}`
/// ids in config order, named by the `kind` tag (mirrors `seed_resources`).
fn desired_outputs(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .outputs
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            let body = to_body(output)?;
            let kind = body
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("output")
                .to_owned();
            Some((
                format!("output-{index}"),
                ResourceInput { name: kind, body },
            ))
        })
        .collect()
}

/// The overlays store's desired contents (id-named, mirrors `seed_resources`).
fn desired_overlays(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .overlays
        .iter()
        .filter_map(|overlay| {
            Some((
                overlay.id.clone(),
                ResourceInput {
                    name: overlay.id.clone(),
                    body: to_body(overlay)?,
                },
            ))
        })
        .collect()
}

/// The probes store's desired contents (id-named, mirrors `seed_resources`).
fn desired_probes(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .probes
        .iter()
        .filter_map(|probe| {
            Some((
                probe.id.clone(),
                ResourceInput {
                    name: probe.id.clone(),
                    body: to_body(probe)?,
                },
            ))
        })
        .collect()
}

/// The devices store's desired contents (mirrors `seed_resources`).
fn desired_devices(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .devices
        .iter()
        .filter_map(|device| {
            Some((
                device.id.clone(),
                ResourceInput {
                    name: device
                        .display_name
                        .clone()
                        .unwrap_or_else(|| device.id.clone()),
                    body: to_body(device)?,
                },
            ))
        })
        .collect()
}

/// The sync-groups store's desired contents (mirrors `seed_resources`).
fn desired_sync_groups(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .sync_groups
        .iter()
        .filter_map(|group| {
            Some((
                group.id.clone(),
                ResourceInput {
                    name: group.id.clone(),
                    body: to_body(group)?,
                },
            ))
        })
        .collect()
}

/// Serialize a config value to its canonical JSON body, warning (never
/// panicking) on the in-practice-impossible failure.
fn to_body(value: &impl serde::Serialize) -> Option<serde_json::Value> {
    match serde_json::to_value(value) {
        Ok(body) => Some(body),
        Err(error) => {
            tracing::warn!(error = %error, "config-file resync: serializing a config value failed");
            None
        }
    }
}

/// Submit a command on the bounded, non-blocking bus (invariant #10). Returns
/// whether it landed; a full/closed bus sheds with a warning — the caller
/// counts the shed and the watcher retries the whole apply on a later poll
/// (review M1).
fn submit(state: &AppState, command: Command) -> bool {
    let kind = command.kind();
    match state.commands.try_submit(command) {
        Ok(_op) => true,
        Err(error) => {
            tracing::warn!(
                command = kind,
                error = %error,
                "config-file apply: the engine command bus shed this change; \
                 the watcher retries the whole apply on its next poll"
            );
            false
        }
    }
}

/// Record + surface an INVALID file write: warn, watch-status `last_rejected`,
/// and the latched `config-file-invalid` health warning — and change nothing.
fn reject(state: &AppState, path: &Path, reason: &str, invalid_active: &mut bool) {
    tracing::warn!(
        path = %path.display(),
        reason = %reason,
        "ignoring an INVALID config-file write; the running configuration is unchanged"
    );
    state.config_watch.record_rejected(now_ms(state), reason);
    *invalid_active = true;
    state
        .engine
        .publish_event(Event::HealthWarningRaised(invalid_warning(
            path,
            reason,
            now_nanos(state),
            true,
        )));
}

/// Clear a previously-raised `config-file-invalid` warning: the invalid
/// condition resolved, either because a subsequent valid write applied or
/// because the file is back at the already-applied content (review m5).
fn clear_invalid(state: &AppState, path: &Path, reason: &str, invalid_active: &mut bool) {
    if !*invalid_active {
        return;
    }
    *invalid_active = false;
    state
        .engine
        .publish_event(Event::HealthWarningCleared(invalid_warning(
            path,
            reason,
            now_nanos(state),
            false,
        )));
}

/// Raise (or refresh) the interim `config-file-apply-incomplete` warning: a
/// valid change was only PARTIALLY applied because `shed` engine command(s)
/// were shed on a full bus (review M1). The watcher retries the whole apply
/// on its next poll and clears this when it completes.
fn publish_apply_incomplete(
    state: &AppState,
    path: &Path,
    shed: u32,
    incomplete_active: &mut bool,
) {
    *incomplete_active = true;
    state
        .engine
        .publish_event(Event::HealthWarningRaised(apply_incomplete_warning(
            path,
            shed,
            now_nanos(state),
            true,
        )));
}

/// Clear a previously-raised `config-file-apply-incomplete` warning: the
/// retried apply completed (or the file reverted to the running baseline, so
/// nothing is pending for the engine any more).
fn clear_apply_incomplete(state: &AppState, path: &Path, incomplete_active: &mut bool) {
    if !*incomplete_active {
        return;
    }
    *incomplete_active = false;
    state
        .engine
        .publish_event(Event::HealthWarningCleared(apply_incomplete_warning(
            path,
            0,
            now_nanos(state),
            false,
        )));
}

/// Build the `config-file-apply-incomplete` warning (raise and clear share
/// the shape; the store coalesces on the code).
fn apply_incomplete_warning(path: &Path, shed: u32, since: i64, active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::ConfigFileApplyIncomplete,
        severity: WarningSeverity::Warning,
        subsystem: "config".to_owned(),
        message: format!(
            "A valid change to the config file {} is only PARTIALLY applied: {shed} engine \
             command(s) were shed on a full command bus; the watcher retries the whole change \
             on its next poll.",
            path.display()
        ),
        remediation: "No action needed — the watcher retries automatically and clears this \
                      warning when the apply completes; investigate a persistently full \
                      command bus if it does not."
            .to_owned(),
        since,
        active,
    }
}

/// Re-converge every store-backed section (and the working layout) to
/// `config`, audited under `actor`. Used when a partial (shed) apply is
/// abandoned: the watcher passes [`ACTOR`] when a reverted file returns to the
/// running baseline (ADR-W020 §5); the revert-to-start route (ADR-W024 §5)
/// passes the requesting principal to roll the stores back to the pre-revert
/// Running document so a shed revert applies nothing durable and its retry's
/// diff is non-empty again. Idempotent.
pub(crate) fn resync_all_stores(state: &AppState, actor: &str, config: &MultiviewConfig) {
    resync_store(state, actor, &state.sources, &desired_sources(config));
    resync_store(state, actor, &state.outputs, &desired_outputs(config));
    resync_store(state, actor, &state.overlays, &desired_overlays(config));
    resync_store(state, actor, &state.probes, &desired_probes(config));
    resync_store(state, actor, &state.devices, &desired_devices(config));
    for device in &config.devices {
        state.device_status.ensure(&device.id);
    }
    resync_store(
        state,
        actor,
        &state.sync_groups,
        &desired_sync_groups(config),
    );
    if config.audio.is_some() {
        resync_audio(state, actor, config);
    }
    // ADR-W024 round 6 (F1): salvos + tally_profiles are store-backed running
    // state composed into `active.toml`, so revert-to-start MUST roll their
    // definition stores back to the target document too — otherwise a runtime
    // salvo/tally edit would survive a revert (the store would keep the drift).
    resync_salvos(state, actor, config);
    resync_tally_profiles(state, actor, config);
    let id = state
        .working_layout_id
        .clone()
        .unwrap_or_else(|| "working".to_owned());
    if let Some(body) = working_layout_body(config) {
        reseed_working_layout(state, actor, &id, &body);
    }
}

/// Build the `config-file-invalid` warning (raise and clear share the shape;
/// the store coalesces on the code).
fn invalid_warning(path: &Path, reason: &str, since: i64, active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::ConfigFileInvalid,
        severity: WarningSeverity::Warning,
        subsystem: "config".to_owned(),
        message: format!(
            "The config file {} changed on disk but the new document is invalid ({reason}); \
             NOTHING was applied — the run keeps the last-good configuration.",
            path.display()
        ),
        remediation: "Fix the file (run `multiview validate` against it); the next valid \
                      write applies and clears this warning."
            .to_owned(),
        since,
        active,
    }
}

/// Raise (or refresh) the latched `config-file-requires-restart` warning
/// naming every pending section.
fn publish_requires_restart(state: &AppState, pending: &[String]) {
    let sections = pending.join(", ");
    tracing::warn!(
        sections = %sections,
        "config file changed sections that only apply on RESTART; the running process \
         may differ from the file until then"
    );
    // Review m7: the warning is LATCHED (a later revert cannot un-ring the
    // bell for state the engine never adopted), so the message says "changed
    // since boot" / "may differ" — the file's CURRENT content might have been
    // reverted since the change was seen.
    state
        .engine
        .publish_event(Event::HealthWarningRaised(HealthWarning {
            code: WarningCode::ConfigFileRequiresRestart,
            severity: WarningSeverity::Warning,
            subsystem: "config".to_owned(),
            message: format!(
                "Section(s) [{sections}] of the config file changed since boot in ways that \
                 cannot hot-apply; the running process may differ from the file until a restart."
            ),
            remediation: "Restart multiview to apply these sections (live-appliable changes \
                          were already applied)."
                .to_owned(),
            since: now_nanos(state),
            active: true,
        }));
}

/// The control plane's clock as Unix nanoseconds (the same `AckClock` the
/// audit log stamps with — injectable in tests).
fn now_nanos(state: &AppState) -> i64 {
    state.ack_now().as_nanos()
}

/// The control plane's clock as Unix milliseconds.
fn now_ms(state: &AppState) -> i64 {
    now_nanos(state).div_euclid(1_000_000)
}
