//! The lease-directory **watcher** (CONSPECT-1, brief §2 of the CONSPECT-1
//! scope).
//!
//! Watches a directory (default `/var/lib/conspect/licence/`, config-overridable
//! for tests) for a dropped lease/binding file. A simple **poll** loop picks a
//! file up within seconds — dependency-free and dead-simple, which meets the
//! "within seconds" requirement without pulling a filesystem-notify crate
//! (notify-class crates add a supply-chain edge; the poll is preferred per the
//! brief). On each poll it reads the candidate files, verifies each via
//! [`crate::verify::verify_signed_lease`] against the pinned key, installs a
//! valid one into the [`LeaseStore`], and **WARNs + ignores** anything
//! invalid/stale/garbage (never crash, never stall — bad-inputs-are-the-purpose).
//!
//! # Never off air (invariant #1 / #10)
//!
//! The watcher does control-plane filesystem I/O only. It holds **no** engine
//! handle and touches **no** engine channel; the worst a malformed file can do
//! is get logged and skipped. The poll step is the testable unit
//! ([`LeaseDirectoryWatcher::poll_once`]); [`LeaseDirectoryWatcher::run`] is a
//! thin self-contained loop the cli can spawn on a background thread. Neither can
//! back-pressure the engine.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::store::{system_now, InstallError, LeaseBinding, LeaseStore};
use crate::verify::PinnedKey;

/// The default lease directory (brief §2 of the CONSPECT-1 scope). Linux
/// `/var/lib`-rooted; the path is overridable for tests + other platforms.
pub const DEFAULT_LEASE_DIR: &str = "/var/lib/conspect/licence/";

/// The file extensions the watcher considers a candidate lease/binding file.
/// Other files in the directory are ignored.
const CANDIDATE_EXTENSIONS: &[&str] = &["binding", "lease", "cbor"];

/// The outcome of a single poll, so the caller (and tests) can observe what the
/// watcher did without reading logs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollOutcome {
    /// No new candidate file activated a lease this poll (either none present,
    /// or every candidate was already seen).
    NothingNew,
    /// A valid binding was verified + installed; the serial it activated.
    Installed {
        /// The serial of the activated lease.
        serial: String,
    },
    /// At least one candidate was rejected (signature/fingerprint/stale/garbage);
    /// it was logged and skipped, the store left untouched. Carries how many
    /// candidates were rejected this poll.
    Rejected {
        /// How many candidate files were rejected this poll.
        count: usize,
    },
}

/// The poll-driven lease-directory watcher.
pub struct LeaseDirectoryWatcher {
    /// The watched directory.
    dir: PathBuf,
    /// The pinned verifying key every dropped file is checked against.
    pinned: PinnedKey,
    /// The store a valid file installs into.
    store: Arc<LeaseStore>,
    /// Paths already processed (by path + length + mtime fingerprint) so an
    /// unchanged file is not re-installed every poll — idempotency.
    seen: RwLock<HashSet<String>>,
}

impl LeaseDirectoryWatcher {
    /// A watcher over `dir`, checking dropped files against `pinned` and
    /// installing valid ones into `store`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, pinned: PinnedKey, store: Arc<LeaseStore>) -> Self {
        Self {
            dir: dir.into(),
            pinned,
            store,
            seen: RwLock::new(HashSet::new()),
        }
    }

    /// A watcher over the default [`DEFAULT_LEASE_DIR`].
    #[must_use]
    pub fn with_default_dir(pinned: PinnedKey, store: Arc<LeaseStore>) -> Self {
        Self::new(DEFAULT_LEASE_DIR, pinned, store)
    }

    /// The directory this watcher polls.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Poll the directory once: read every not-yet-seen candidate file, verify +
    /// install a valid one, WARN + skip anything invalid. Returns what happened.
    ///
    /// `now` is the instant the install is evaluated at (staleness/validity); the
    /// caller passes the sampled wall clock. This method never panics and never
    /// blocks on anything but a bounded directory read.
    pub fn poll_once(&self, now: DateTime<Utc>) -> PollOutcome {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) => {
                // A missing/unreadable directory is not fatal — the lease may
                // simply not have been dropped yet. WARN once-ish and carry on.
                tracing::debug!(dir = %self.dir.display(), %err, "lease directory not readable yet");
                return PollOutcome::NothingNew;
            }
        };

        let mut installed: Option<String> = None;
        let mut rejected = 0_usize;

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_candidate(&path) {
                continue;
            }
            let Some(token) = seen_token(&path) else {
                continue;
            };
            // Skip files we have already processed (unchanged).
            if self.already_seen(&token) {
                continue;
            }
            self.mark_seen(token);

            match self.try_install(&path, now) {
                Ok(serial) => {
                    tracing::info!(path = %path.display(), %serial, "installed a dropped lease");
                    // Keep the newest install as the reported outcome; continue
                    // scanning so all candidates are marked seen this poll.
                    installed = Some(serial);
                }
                Err(reason) => {
                    rejected += 1;
                    tracing::warn!(
                        path = %path.display(),
                        %reason,
                        "ignoring an invalid dropped lease file (never off air)"
                    );
                }
            }
        }

        match (installed, rejected) {
            (Some(serial), _) => PollOutcome::Installed { serial },
            (None, 0) => PollOutcome::NothingNew,
            (None, count) => PollOutcome::Rejected { count },
        }
    }

    /// Run the poll loop forever on the current thread, sleeping `interval`
    /// between polls and reading the wall clock for each install. Intended to be
    /// spawned on a dedicated `std::thread` by the cli (CONSPECT-10); it never
    /// returns. Self-contained (no async runtime) so the leaf crate stays
    /// dependency-light. Sampling `Utc::now` here is the only system-clock read
    /// in the crate, and it is off the engine hot loop.
    pub fn run(&self, interval: Duration) -> ! {
        loop {
            let _ = self.poll_once(system_now());
            std::thread::sleep(interval);
        }
    }

    /// Read, decode, verify, and install the file at `path`. The reason string on
    /// failure is the typed rejection rendered for the log.
    fn try_install(&self, path: &Path, now: DateTime<Utc>) -> Result<String, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
        let binding =
            LeaseBinding::from_bytes(&bytes).map_err(|e| format!("decode failed: {e}"))?;
        self.store
            .install_binding(&binding, &self.pinned, now)
            .map(|lease| lease.serial)
            .map_err(|err| render_install_error(&err))
    }

    /// Whether `token` was already processed.
    fn already_seen(&self, token: &str) -> bool {
        self.seen.read().is_ok_and(|s| s.contains(token))
    }

    /// Record `token` as processed (best-effort; a poisoned lock just means a
    /// possible duplicate install attempt next poll, which the store dedups by
    /// staleness anyway).
    fn mark_seen(&self, token: String) {
        if let Ok(mut s) = self.seen.write() {
            s.insert(token);
        }
    }
}

/// Whether `path` is a candidate lease file (by extension).
fn is_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| CANDIDATE_EXTENSIONS.contains(&ext))
}

/// A change-detection token for a file: its path + length + mtime. An unchanged
/// file yields the same token, so it is processed at most once; a rewritten file
/// (new mtime/length) is re-processed. `None` if the metadata is unreadable.
fn seen_token(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some(format!("{}|{len}|{mtime}", path.display()))
}

/// Render a typed [`InstallError`] as a stable reason slug for the WARN log.
fn render_install_error(err: &InstallError) -> String {
    match err {
        InstallError::SignatureInvalid => "signature_invalid".to_owned(),
        InstallError::FingerprintMismatch { score, threshold } => {
            format!("fingerprint_mismatch (score {score} < {threshold})")
        }
        InstallError::Stale { active, incoming } => {
            format!("lease_stale (incoming {incoming} older than active {active})")
        }
    }
}
