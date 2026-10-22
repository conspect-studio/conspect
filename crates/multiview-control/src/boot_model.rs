//! The Boot / Loaded / Running configuration model (ADR-W024).
//!
//! * **Boot** is the config file `multiview run` started with — the watched,
//!   hand-edited cold-start baseline.
//! * **Loaded** is the immutable snapshot of Boot taken at process start,
//!   held in memory ([`BootModel::loaded`]) and persisted to
//!   `<config-dir>/.multiview/loaded.toml` ([`persist_loaded`]).
//! * **Running** is Loaded + every live change since. It is continuously
//!   persisted — debounced, atomic-rename, machine-written, NEVER watched —
//!   to `<config-dir>/.multiview/active.toml`, composed via the SAME document
//!   composition `GET /api/v1/config/export` uses ([`persist_running_now`]).
//!   The trigger is the ONE audit choke point: every successful mutation
//!   passes [`crate::AppState::audit`], which fires the coalescing
//!   `running_changed` notify the [`spawn_running_persist`] task waits on.
//!
//! Cold start: `[control] start = "resume"` loads a valid `active.toml` as
//! the starting Running state ([`load_resume_config`]); invalid/missing falls
//! back to boot with a warning (the caller surfaces the reason). Loaded stays
//! the boot snapshot in both modes.
//!
//! Isolation (invariants #1/#10): everything here is control-plane tenancy on
//! tokio — composition reads read-mostly stores, persistence writes files,
//! and the notify is a one-permit coalescing signal that can never queue,
//! grow, or block. The render thread never sees any of it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::{MultiviewConfig, StartMode};

use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// The state directory name under the boot config file's directory.
const STATE_DIR: &str = ".multiview";
/// The persisted Loaded snapshot's file name.
const LOADED_FILE: &str = "loaded.toml";
/// The persisted Running state's file name.
const ACTIVE_FILE: &str = "active.toml";

/// The run's Boot/Loaded/Running model (ADR-W024): the boot path, the
/// immutable Loaded snapshot, the cold-start policy, and whether this run
/// actually resumed (with the honest fallback reason when it could not).
#[derive(Debug)]
pub struct BootModel {
    /// The boot config file path (the watch target and the promote target).
    boot_path: PathBuf,
    /// The immutable Loaded snapshot (the boot document at process start).
    loaded: MultiviewConfig,
    /// The `[control] start` policy the boot file declared.
    start: StartMode,
    /// Whether this run started from a valid persisted `active.toml`.
    resumed: bool,
    /// Why a `start = "resume"` run fell back to boot, if it did.
    resume_fallback: Option<String>,
    /// Unix milliseconds of the last successful `active.toml` write
    /// (`0` = never written this run).
    active_written_ms: AtomicI64,
    /// Whether a shed (partial) revert-to-start raised the
    /// `config-file-apply-incomplete` warning (ADR-W024 §5): a revert that
    /// later completes clears exactly the instance this run's revert raised
    /// — never the watcher's own latched instance.
    revert_incomplete: AtomicBool,
    /// The ticket of the last `active.toml` content actually written, under
    /// the lock that serializes EVERY boot-model file write (review M2): an
    /// aborted persist task's `spawn_blocking` write keeps running detached,
    /// so the next writer must both wait for it (single-writer on the
    /// deterministic temp names) and out-order it (a stale composition must
    /// never overwrite newer content).
    write_serial: std::sync::Mutex<u64>,
    /// The monotonically increasing write-ticket source (taken at compose
    /// time, compared in [`write_active_serialized`]).
    write_tickets: AtomicU64,
    /// The **adopted running configuration** (MAJOR-B / panel round 4): the
    /// config the engine has actually ADOPTED, which `active.toml` is rendered
    /// from. The live resource stores deliberately hold REQUESTED state (a
    /// shed/non-live REST mutation still commits the store and answers `2xx` +
    /// `restart`, ADR-W018), so `active.toml` cannot be composed from them —
    /// it is this snapshot, advanced by applying each ADOPTED delta (never by
    /// recomposing the store), so an unrelated landed mutation can never adopt a
    /// prior shed's change and the snapshot always equals exactly what the
    /// engine runs. Mutated and read ONLY while the caller holds
    /// [`crate::AppState::lock_config_mutation`], so the inner mutex is plain
    /// interior mutability (the async lock provides the ordering, not this).
    last_adopted: std::sync::Mutex<MultiviewConfig>,
}

impl BootModel {
    /// Build the model for a run started from `boot_path` whose Loaded
    /// snapshot is `loaded`, under cold-start policy `start`. `resumed` /
    /// `resume_fallback` record what the resume resolution actually did.
    /// `running` is the resolved starting Running config — the engine adopts it
    /// at startup, so it seeds the adopted snapshot `active.toml` is rendered
    /// from (MAJOR-B round 4).
    #[must_use]
    pub fn new(
        boot_path: PathBuf,
        loaded: MultiviewConfig,
        running: MultiviewConfig,
        start: StartMode,
        resumed: bool,
        resume_fallback: Option<String>,
    ) -> Self {
        Self {
            boot_path,
            loaded,
            start,
            resumed,
            resume_fallback,
            active_written_ms: AtomicI64::new(0),
            revert_incomplete: AtomicBool::new(false),
            write_serial: std::sync::Mutex::new(0),
            write_tickets: AtomicU64::new(0),
            last_adopted: std::sync::Mutex::new(running),
        }
    }

    /// The boot config file path.
    #[must_use]
    pub fn boot_path(&self) -> &Path {
        &self.boot_path
    }

    /// The immutable Loaded snapshot (the revert-to-start target).
    #[must_use]
    pub fn loaded(&self) -> &MultiviewConfig {
        &self.loaded
    }

    /// The `[control] start` cold-start policy.
    #[must_use]
    pub fn start(&self) -> StartMode {
        self.start
    }

    /// Whether this run started from a valid persisted `active.toml`.
    #[must_use]
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Why a `start = "resume"` run fell back to boot, if it did.
    #[must_use]
    pub fn resume_fallback(&self) -> Option<&str> {
        self.resume_fallback.as_deref()
    }

    /// The state directory (`<config-dir>/.multiview`).
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        state_dir_for(&self.boot_path)
    }

    /// The persisted Running state path (`<config-dir>/.multiview/active.toml`).
    #[must_use]
    pub fn active_path(&self) -> PathBuf {
        self.state_dir().join(ACTIVE_FILE)
    }

    /// The persisted Loaded snapshot path (`<config-dir>/.multiview/loaded.toml`).
    #[must_use]
    pub fn loaded_path(&self) -> PathBuf {
        self.state_dir().join(LOADED_FILE)
    }

    /// Unix milliseconds of the last successful `active.toml` write this run,
    /// or [`None`] when nothing has been persisted yet.
    #[must_use]
    pub fn active_written_ms(&self) -> Option<i64> {
        match self.active_written_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Record a successful `active.toml` write at `now_ms`.
    fn record_active_written(&self, now_ms: i64) {
        self.active_written_ms.store(now_ms, Ordering::Release);
    }

    /// Latch that a shed (partial) revert raised the
    /// `config-file-apply-incomplete` warning (ADR-W024 §5).
    pub fn note_revert_incomplete(&self) {
        self.revert_incomplete.store(true, Ordering::Release);
    }

    /// Take (and clear) the shed-revert latch: `true` when a previous revert
    /// raised the incomplete warning that a now-completed revert may clear.
    #[must_use]
    pub fn take_revert_incomplete(&self) -> bool {
        self.revert_incomplete.swap(false, Ordering::AcqRel)
    }

    /// Lock the adopted-snapshot, recovering from poisoning (a panicked holder
    /// must never wedge persistence). Callers already hold
    /// [`crate::AppState::lock_config_mutation`], so this never contends.
    fn lock_adopted(&self) -> std::sync::MutexGuard<'_, MultiviewConfig> {
        match self.last_adopted.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Apply an ADOPTED source upsert to the adopted snapshot (MAJOR-B round 4):
    /// the engine landed an `UpsertSource`, so the running config now carries
    /// this source. Replaces an existing source of the same id, else appends.
    /// Call ONLY while holding the config-mutation lock.
    pub fn adopt_source(&self, source: multiview_config::Source) {
        let mut snap = self.lock_adopted();
        match snap.sources.iter_mut().find(|s| s.id == source.id) {
            Some(existing) => *existing = source,
            None => snap.sources.push(source),
        }
    }

    /// Apply an ADOPTED source removal to the adopted snapshot (MAJOR-B round
    /// 4): the engine landed a `RemoveSource`. Call ONLY while holding the
    /// config-mutation lock.
    pub fn unadopt_source(&self, id: &str) {
        self.lock_adopted().sources.retain(|s| s.id != id);
    }

    /// Apply an ADOPTED overlay upsert to the adopted snapshot (MAJOR-B round
    /// 4). Replaces an existing overlay of the same id, else appends. Call ONLY
    /// while holding the config-mutation lock.
    pub fn adopt_overlay(&self, overlay: multiview_config::Overlay) {
        let mut snap = self.lock_adopted();
        match snap.overlays.iter_mut().find(|o| o.id == overlay.id) {
            Some(existing) => *existing = overlay,
            None => snap.overlays.push(overlay),
        }
    }

    /// Apply an ADOPTED overlay removal to the adopted snapshot (MAJOR-B round
    /// 4). Call ONLY while holding the config-mutation lock.
    pub fn unadopt_overlay(&self, id: &str) {
        self.lock_adopted().overlays.retain(|o| o.id != id);
    }

    /// Apply an ADOPTED layout to the snapshot (MAJOR-B round 5): the engine
    /// landed an `ApplyLayout`, so the running canvas/layout/cells are now this
    /// layout's. `ApplyLayout` is a LIVE sheddable command, so the working-
    /// layout store can hold a requested-but-unadopted layout (a landed
    /// `PUT /layouts/{id}` + a shed apply); this is called ONLY when the
    /// `ApplyLayout` lands, so the snapshot's canvas/layout/cells stay at the
    /// adopted layout. Call ONLY while holding the config-mutation lock.
    pub fn adopt_layout(
        &self,
        canvas: multiview_config::Canvas,
        layout: multiview_config::Layout,
        cells: Vec<multiview_config::Cell>,
    ) {
        let mut snap = self.lock_adopted();
        snap.canvas = canvas;
        snap.layout = layout;
        snap.cells = cells;
    }

    /// The ADOPTED sources — exactly the sources the engine is running (MAJOR-B
    /// round 4). `persist_running_now` overrides the store-composed config's
    /// `sources` with these so `active.toml` reflects adopted, not requested,
    /// source state (a shed/restart-pending REST source change keeps the store
    /// but is not adopted, so it does not leak into `active.toml`). Call ONLY
    /// while holding the config-mutation lock.
    #[must_use]
    pub fn adopted_sources(&self) -> Vec<multiview_config::Source> {
        self.lock_adopted().sources.clone()
    }

    /// The ADOPTED overlays — exactly the overlays the engine is running
    /// (MAJOR-B round 4). See [`adopted_sources`](Self::adopted_sources).
    #[must_use]
    pub fn adopted_overlays(&self) -> Vec<multiview_config::Overlay> {
        self.lock_adopted().overlays.clone()
    }

    /// The ADOPTED canvas/layout/cells — exactly the layout the engine is
    /// running (MAJOR-B round 5). `persist_running_now` overrides the
    /// store-composed config's canvas/layout/cells with these so a
    /// shed/unadopted `apply-layout` (the working-layout store changed but the
    /// `ApplyLayout` never landed) does not leak into `active.toml`.
    #[must_use]
    pub fn adopted_layout(
        &self,
    ) -> (
        multiview_config::Canvas,
        multiview_config::Layout,
        Vec<multiview_config::Cell>,
    ) {
        let snap = self.lock_adopted();
        (snap.canvas.clone(), snap.layout.clone(), snap.cells.clone())
    }

    /// Take the next monotonically increasing write ticket (call at compose
    /// time; pass to [`write_active_serialized`]).
    #[must_use]
    pub fn next_write_ticket(&self) -> u64 {
        self.write_tickets.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Lock the boot-model write serial, recovering from poisoning (a
    /// panicked writer must never wedge persistence).
    fn lock_writes(&self) -> std::sync::MutexGuard<'_, u64> {
        match self.write_serial.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The state directory for a boot config at `boot_path`
/// (`<config-dir>/.multiview`; a bare filename resolves against `.`).
#[must_use]
pub fn state_dir_for(boot_path: &Path) -> PathBuf {
    match boot_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(STATE_DIR),
        _ => PathBuf::from(".").join(STATE_DIR),
    }
}

/// Load the persisted Running state (`active.toml`) next to `boot_path` as a
/// resume candidate: it must read, parse, AND validate.
///
/// # Errors
///
/// A human-readable reason naming the failure (missing/unreadable file, TOML
/// parse error, validation error) — the caller warns with it and falls back
/// to the boot document.
pub fn load_resume_config(boot_path: &Path) -> Result<MultiviewConfig, String> {
    let active = state_dir_for(boot_path).join(ACTIVE_FILE);
    let text = std::fs::read_to_string(&active).map_err(|e| {
        format!(
            "the persisted Running state {} cannot be read: {e}",
            active.display()
        )
    })?;
    let config = MultiviewConfig::load_from_toml(&text).map_err(|e| {
        format!(
            "the persisted Running state {} does not parse as TOML: {e}",
            active.display()
        )
    })?;
    config.validate().map_err(|e| {
        format!(
            "the persisted Running state {} does not validate: {e}",
            active.display()
        )
    })?;
    Ok(config)
}

/// Write `content` to `path` atomically and securely: an EXCLUSIVE,
/// randomly-named same-directory temp file (`O_EXCL`, mode `0600`) →
/// `fsync(2)` → `rename(2)` over the destination → `fsync` the directory.
/// Readers always observe either the old or the new content, and a successful
/// write leaves no temp residue.
///
/// **Security (MAJOR-A / panel round 2).** The state files carry plaintext
/// credentials (WebRTC ICE `password`, `static_auth_secret`, WHIP tokens), so
/// the temp is created with [`tempfile::NamedTempFile::new_in`] — `O_EXCL` + an
/// **unpredictable** name + `0600` on Unix. That defeats both attacks a
/// deterministic `.<name>.tmp` name allowed: a pre-existing world-readable temp
/// inode (the secrets are never written into it), and an attacker-planted
/// symlink at the temp path (it is never opened/followed). The final mode is
/// clamped to a `0600` floor (tightened to a stricter mode the destination
/// already carries). On a fresh deployment the destination directory is assumed
/// already validated by [`create_state_dir`] (it fails closed on an insecure
/// dir before any write reaches here).
///
/// # Errors
///
/// Any I/O error from create/write/chmod/sync/persist (a destination with no
/// parent directory is [`std::io::ErrorKind::InvalidInput`]).
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if path.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("atomic write target {} has no file name", path.display()),
        ));
    }
    // Exclusive, unpredictable, 0600 temp in the SAME directory (so the final
    // rename is atomic on one filesystem). `NamedTempFile` uses `O_EXCL` + a
    // random name + 0600 on Unix — a planted file at any guessable name is
    // never touched, and a planted symlink is never followed.
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    // Clamp the final mode to the 0600 owner-only floor (or a stricter mode the
    // destination already carries, e.g. 0400) BEFORE the content becomes
    // visible at `path`.
    set_owner_only_mode(tmp.as_file(), atomic_target_mode(path))?;
    // Durability before visibility: the rename must never expose a file whose
    // bytes are still only in the page cache.
    tmp.as_file().sync_all()?;
    // `persist` is the atomic `rename(2)`; on failure the temp is cleaned up by
    // `NamedTempFile`'s drop (no residue).
    tmp.persist(path).map_err(|e| e.error)?;
    // fsync the directory so the rename itself survives a power cut. A
    // filesystem that cannot fsync a directory handle (rare) degrades to the
    // rename's own guarantees rather than failing the (already-visible) write.
    if let Ok(dir_handle) = std::fs::File::open(&dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

/// The owner-only (0600) floor for a state-file write, tightened further to a
/// stricter mode the destination already carries (MAJOR-A): a first create
/// lands at 0600; an existing `chmod 0400` is preserved. Non-Unix targets are
/// not a deploy platform (no Windows), so the mode is advisory there.
#[cfg(unix)]
fn atomic_target_mode(dest: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    const OWNER_ONLY: u32 = 0o600;
    match std::fs::metadata(dest) {
        // Preserve a destination that is already STRICTER than owner-only
        // (e.g. 0400); otherwise clamp to the 0600 floor (never widen, never
        // leave the umask default on the secret-bearing file).
        Ok(meta) => OWNER_ONLY & meta.permissions().mode(),
        Err(_) => OWNER_ONLY,
    }
}

/// Set the temp file's permission mode (MAJOR-A): the secrets become visible at
/// the destination only after this clamp is applied.
#[cfg(unix)]
fn set_owner_only_mode(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// Non-Unix fallback (not a deploy platform): the mode is advisory off Unix.
#[cfg(not(unix))]
fn atomic_target_mode(_dest: &Path) -> u32 {
    0o600
}

/// Non-Unix fallback: no-op mode set (mode is advisory off Unix).
#[cfg(not(unix))]
fn set_owner_only_mode(_file: &std::fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Create the boot-model state directory (`<config-dir>/.multiview`) with
/// owner-only 0700 permissions, and FAIL CLOSED on an insecure existing one
/// (MAJOR-A / panel round 2): the dir holds `loaded.toml` / `active.toml`,
/// which carry plaintext credentials, so a group/world-writable dir (where an
/// attacker could swap the state files) — or one we do not own — is refused
/// rather than written into. Refusing does NOT take output off air (this is
/// control-plane persistence; the output clock is untouched). `DirBuilder.mode`
/// applies only to the dir it CREATES, so an existing dir is validated, never
/// silently loosened or tightened.
///
/// # Errors
///
/// [`std::io::ErrorKind::PermissionDenied`] when an existing state dir is
/// group/world-writable or not owned by this process; any I/O error from the
/// create/stat.
fn create_state_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // Validate the (possibly pre-existing) directory: it must not be
        // group/world-writable and must be owned by us — otherwise an attacker
        // could replace the secret-bearing state files.
        let meta = std::fs::metadata(dir)?;
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "the state dir {} is group/world-writable (mode {:o}); refusing to \
                     persist credential-bearing state files there",
                    dir.display(),
                    mode & 0o7777
                ),
            ));
        }
        // `rustix::process::geteuid` is a SAFE wrapper (this crate forbids
        // `unsafe`); no `libc` FFI here.
        let our_uid = rustix::process::geteuid().as_raw();
        if meta.uid() != our_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "the state dir {} is owned by uid {} (not this process's {}); refusing \
                     to persist credential-bearing state files there",
                    dir.display(),
                    meta.uid(),
                    our_uid
                ),
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Persist the Loaded snapshot to `loaded.toml` (atomic, machine-written
/// canonical TOML), creating the state directory on demand.
///
/// # Errors
///
/// A human-readable reason on render or I/O failure — the caller warns and
/// continues (the in-memory Loaded snapshot is authoritative; the file is the
/// forensic copy).
pub fn persist_loaded(model: &BootModel) -> Result<(), String> {
    let toml = model
        .loaded
        .to_toml()
        .map_err(|e| format!("rendering the Loaded snapshot: {e}"))?;
    let dir = model.state_dir();
    create_state_dir(&dir).map_err(|e| format!("creating the state dir {}: {e}", dir.display()))?;
    write_atomic(&model.loaded_path(), &toml)
        .map_err(|e| format!("writing {}: {e}", model.loaded_path().display()))
}

/// Write `toml` to `active.toml` under the model's write lock, skipping the
/// write when a NEWER ticket already landed (review M2). Every boot-model
/// file write serializes on this lock: an aborted persist task's
/// `spawn_blocking` write keeps running detached on the blocking pool, so
/// the lock makes concurrent writers single-file on the deterministic temp
/// name, and the ticket check makes content monotonic — a stale composition
/// can never overwrite newer state. Returns whether this content was
/// actually written.
///
/// # Errors
///
/// Any I/O error from `create_dir_all` or the atomic write. Blocking I/O —
/// call from `spawn_blocking` on async paths.
pub fn write_active_serialized(
    model: &BootModel,
    ticket: u64,
    toml: &str,
) -> std::io::Result<bool> {
    let mut last = model.lock_writes();
    if *last > ticket {
        // A newer composition already landed; this one is stale.
        return Ok(false);
    }
    create_state_dir(&model.state_dir())?;
    write_atomic(&model.active_path(), toml)?;
    *last = ticket;
    Ok(true)
}

/// Write `toml` to the BOOT file under the model's write lock, on the
/// blocking pool (reviews m1 + M2): the promote route's write never parks
/// the control-plane reactor and never interleaves with another boot-model
/// file write on a deterministic temp name.
///
/// # Errors
///
/// Any I/O error from the atomic write (or a failed blocking task).
pub async fn write_boot_file(model: Arc<BootModel>, toml: String) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let _serial = model.lock_writes();
        write_atomic(model.boot_path(), &toml)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("the boot-file write task failed: {e}")))?
}

/// Persist the CURRENT Running state to `active.toml`, now: compose the
/// running document via the SAME composition the export route uses,
/// deserialize + validate it (so an `active.toml` that exists always
/// round-trips `MultiviewConfig::validate`), render canonical TOML, and write
/// it atomically. A run without a boot model persists nothing.
///
/// The composition reads read-mostly stores in place; the file I/O rides
/// [`tokio::task::spawn_blocking`] (review m1) through
/// [`write_active_serialized`] (review M2: locked + ticket-ordered, so a
/// detached stale write can neither interleave with nor overwrite a newer
/// one).
///
/// # Errors
///
/// Any composition/validation/render fault as the export route would surface
/// it, or [`ControlError::Repository`] for an I/O failure. Callers on the
/// persist task treat every error as fail-soft (warn + skip).
pub async fn persist_running_now(state: &AppState) -> ControlResult<()> {
    let Some(model) = state.boot_model.as_ref() else {
        return Ok(());
    };
    // MAJOR-B (round 4): acquire the SAME mutation lock every live-apply path
    // holds, so this can never compose mid-mutation (the happens-before the
    // round-3 separate atomics lacked) — the persister blocks until any
    // in-flight mutate→submit→adopt sequence fully resolves, then reads a
    // settled adopted snapshot.
    let _mutation = state.lock_config_mutation().await;
    // Compose the running document from the stores WITHOUT validating (the live
    // store can transiently fail whole-document validation — e.g. a cell
    // referencing a source a shed-but-still-running DELETE removed from the
    // store), then OVERRIDE the divergence-prone collections (sources, overlays)
    // with the ADOPTED snapshot: the stores hold REQUESTED state (a shed/non-
    // live REST mutation keeps the store, ADR-W018), so `active.toml` must
    // reflect what the engine actually runs, not what was requested. Only AFTER
    // the override do we validate — the adopted document (what the engine
    // runs) must always be valid.
    let document = crate::routes::config::compose_document_unredacted(state)?;
    let mut config: MultiviewConfig =
        serde_path_to_error::deserialize(&document).map_err(|err| {
            let path = err.path().to_string();
            ControlError::Validation(format!(
                "the running document does not compose at `{path}`: {}",
                err.into_inner()
            ))
        })?;
    config.sources = model.adopted_sources();
    config.overlays = model.adopted_overlays();
    // Layout/cells are also live-sheddable (via `ApplyLayout`), so override
    // canvas/layout/cells from the adopted snapshot too (MAJOR-B round 5).
    let (canvas, layout, cells) = model.adopted_layout();
    config.canvas = canvas;
    config.layout = layout;
    config.cells = cells;
    config.validate().map_err(|e| {
        ControlError::Validation(format!("the adopted running config does not validate: {e}"))
    })?;
    let toml = config
        .to_toml()
        .map_err(|e| ControlError::Repository(format!("TOML render failed: {e}")))?;
    // The ticket is taken AFTER composing, so ticket order tracks content
    // freshness: a slower, older composition loses to a newer one.
    let ticket = model.next_write_ticket();
    let task_model = Arc::clone(model);
    let written =
        tokio::task::spawn_blocking(move || write_active_serialized(&task_model, ticket, &toml))
            .await
            .map_err(|e| ControlError::Repository(format!("the persist write task failed: {e}")))?
            .map_err(|e| {
                ControlError::Repository(format!("writing {}: {e}", model.active_path().display()))
            })?;
    if written {
        model.record_active_written(state.ack_now().as_nanos().div_euclid(1_000_000));
    }
    Ok(())
}

/// Stop the debounced Running persister at run teardown (review M2): abort
/// the task, await it, and run one final best-effort [`persist_running_now`]
/// to capture changes younger than the debounce.
///
/// `task.await` returns once the TASK is terminated — but a `spawn_blocking`
/// write the task started keeps running detached on the blocking pool, so
/// awaiting the task alone is NOT a single-writer guarantee. The guarantee
/// comes from [`write_active_serialized`]: every active-file write holds the
/// model's write lock (no interleaving on the deterministic temp name) and
/// carries a compose-time ticket (the final persist's newer content can
/// never be overwritten by the detached stale write). A persist failure is
/// warned and teardown continues (fail-soft).
pub async fn finish_running_persist(task: tokio::task::JoinHandle<()>, state: &AppState) {
    task.abort();
    let _ = task.await;
    if let Err(error) = persist_running_now(state).await {
        tracing::warn!(
            error = %error,
            "the final running-state persist at shutdown was skipped (fail-soft)"
        );
    }
}

/// Spawn the debounced Running persister (ADR-W024 §3): persist the starting
/// Running state once (a stale `active.toml` from a previous run never
/// outlives the run that supersedes it), then wait on the `running_changed`
/// notify the audit choke point fires, sleep the `debounce` (at most one
/// write per window), and persist. Every failure is a `tracing::warn!` and a
/// skipped write — the task never exits on error (fail-soft) and nothing it
/// does can back-pressure the engine (invariant #10).
///
/// The caller stops the returned handle at teardown via
/// [`finish_running_persist`] (abort → await → one final best-effort persist
/// capturing changes younger than the debounce).
#[must_use]
pub fn spawn_running_persist(state: AppState, debounce: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = persist_running_now(&state).await {
            tracing::warn!(
                error = %error,
                "running-state persist: the startup write was skipped (fail-soft)"
            );
        }
        loop {
            state.running_changed.notified().await;
            tokio::time::sleep(debounce).await;
            if let Err(error) = persist_running_now(&state).await {
                tracing::warn!(
                    error = %error,
                    "running-state persist: this write was skipped (fail-soft)"
                );
            }
        }
    })
}
