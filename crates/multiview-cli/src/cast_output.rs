//! The engine side of a cast output (ADR-0111): the activation gate, the
//! in-memory pre-roll ring, the directory ownership rule and the sink runner
//! that drives a [`CastPackager`](multiview_output::hls::cast::CastPackager).
//!
//! A cast output is a normal packet sink behind the bounded
//! [`BindingDelivery`](crate::binding_delivery): it runs on its own sink
//! thread, which is the only thread that touches the disk. Nothing here runs
//! on the output clock or on the bake consumer (invariants #1 and #10): the
//! control plane talks to the sink through [`CastGate`], whose packet-path
//! state is atomics only, and a sink that stalls loses packets (counted in
//! the gate, declared as a gap in the stream) instead of pacing anyone.
//!
//! While no cast stream is active the sink writes nothing. It keeps the last
//! [`PREROLL_NANOS`] of the output's encoded packets in memory (whole GOPs,
//! bounded by bytes too), so an activation can publish a full playlist
//! window at once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(all(test, feature = "gpl-codecs"))]
mod pipeline_tests;
mod ring;
mod runner;
#[cfg(test)]
mod tests;

pub(crate) use runner::{run_cast_sink, CastSetup};

/// Segments listed per playlist of a cast stream.
pub const CAST_WINDOW: usize = 10;
/// Target segment duration of a cast stream, in nanoseconds (2 s).
pub const CAST_TARGET_SEGMENT_NANOS: i64 = 2_000_000_000;
/// Video segments that must be published before a cast stream is `ready`.
pub const CAST_READY_SEGMENTS: u64 = 5;
/// Media time the pre-roll ring holds, in nanoseconds (20 s).
pub const PREROLL_NANOS: i64 = 20_000_000_000;
/// Payload bytes the pre-roll ring holds at most (32 MiB).
pub const PREROLL_BYTES: usize = 32 * 1024 * 1024;
/// The file that marks a directory as written by one cast output; its
/// content is that output's id.
pub const CAST_DIR_MARKER: &str = ".conspect-cast-output";

/// The system clock as nanoseconds past the Unix epoch (0 before it).
pub(crate) fn unix_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// A process-wide, strictly increasing generation: at least the current Unix
/// time in milliseconds, so a file name of one activation is never reused by
/// a later run or a later process.
fn next_generation() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now_ms = u64::try_from(unix_nanos() / 1_000_000).unwrap_or(0);
    let mut last = LAST.load(Ordering::Acquire);
    loop {
        let next = last.max(now_ms).saturating_add(1);
        match LAST.compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// Why a cast output cannot be activated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CastGateError {
    /// The output is disabled for this run (its directory is not usable);
    /// the text names the fix.
    #[error("{0}")]
    Disabled(String),
    /// The run this gate belongs to has ended.
    #[error("the engine run of this cast output has ended")]
    Ended,
}

/// What the control plane reads from a [`CastGate`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CastGateStatus {
    /// A cast stream is active.
    pub active: bool,
    /// The activation's generation (0 when inactive).
    pub generation: u64,
    /// The master playlist and at least [`CAST_READY_SEGMENTS`] segments are
    /// published for this generation.
    pub ready: bool,
    /// Video segments published for this generation.
    pub video_segments: u64,
    /// Audio segments published for this generation.
    pub audio_segments: u64,
    /// Packets this output's sink did not receive (cumulative for the run).
    pub dropped_packets: u64,
    /// Packager restarts after an error (cumulative for the run).
    pub restarts: u64,
    /// Unix milliseconds of the last published segment (0 = none).
    pub last_publish_unix_ms: u64,
    /// The last error, if any.
    pub last_error: Option<String>,
}

/// A hook the sink thread calls before it feeds the packager (tests only):
/// a test blocks or panics in it to stand in for a stalled or broken disk.
#[cfg(test)]
pub(crate) type CastTestHook = Arc<dyn Fn() + Send + Sync>;

/// The activation gate of one cast output, shared by its sink thread and the
/// control plane.
///
/// The packet path reads and writes atomics only. The one mutex guards the
/// status text; the sink thread takes it only when an error changes and the
/// control plane only when it reads a status, never per packet.
pub struct CastGate {
    /// `generation << 1 | preroll`; 0 = inactive.
    state: AtomicU64,
    /// The generation the progress fields below describe.
    progress_generation: AtomicU64,
    ready: AtomicBool,
    video_segments: AtomicU64,
    audio_segments: AtomicU64,
    /// Shared with the output's delivery binding, which counts every packet
    /// the sink did not receive.
    dropped: Arc<AtomicU64>,
    restarts: AtomicU64,
    last_publish_unix_ms: AtomicU64,
    disabled: AtomicBool,
    ended: AtomicBool,
    last_error: Mutex<Option<String>>,
    #[cfg(test)]
    hook: Mutex<Option<CastTestHook>>,
}

impl Default for CastGate {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CastGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CastGate")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl CastGate {
    /// A new, inactive gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            progress_generation: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            video_segments: AtomicU64::new(0),
            audio_segments: AtomicU64::new(0),
            dropped: Arc::new(AtomicU64::new(0)),
            restarts: AtomicU64::new(0),
            last_publish_unix_ms: AtomicU64::new(0),
            disabled: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            last_error: Mutex::new(None),
            #[cfg(test)]
            hook: Mutex::new(None),
        }
    }

    /// Activate the output and return the activation's generation.
    /// Idempotent while active: the running generation is returned and
    /// `preroll` is ignored. Never blocks (one compare-and-swap).
    ///
    /// # Errors
    /// [`CastGateError::Disabled`] when the output's directory is not usable
    /// (the text names the fix), [`CastGateError::Ended`] after the run.
    pub fn activate(&self, preroll: bool) -> Result<u64, CastGateError> {
        // A run that has ended is the first answer: its gate is no longer
        // anyone's to activate, whatever state it ended in.
        if self.ended.load(Ordering::Acquire) {
            return Err(CastGateError::Ended);
        }
        if self.disabled.load(Ordering::Acquire) {
            return Err(CastGateError::Disabled(
                self.last_error()
                    .unwrap_or_else(|| "cast output disabled".to_owned()),
            ));
        }
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if current != 0 {
                return Ok(current >> 1);
            }
            let generation = next_generation();
            let next = (generation << 1) | u64::from(preroll);
            match self
                .state
                .compare_exchange(0, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(generation),
                Err(seen) => current = seen,
            }
        }
    }

    /// Deactivate the output (idempotent). The sink thread closes the stream
    /// and removes its files when it next handles a packet.
    pub fn deactivate(&self) {
        self.state.store(0, Ordering::Release);
    }

    /// The current activation as `(generation, preroll)`; `None` when
    /// inactive. Wait-free; the sink thread calls this per packet.
    #[must_use]
    pub fn activation(&self) -> Option<(u64, bool)> {
        let state = self.state.load(Ordering::Acquire);
        (state != 0).then_some((state >> 1, state & 1 == 1))
    }

    /// The counter the output's delivery binding adds every lost packet to.
    #[must_use]
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    /// Packets this output's sink did not receive so far.
    #[must_use]
    pub fn dropped_packets(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    /// Whether the output is disabled for this run.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    /// The last error text, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A point-in-time status for the control plane. Progress is reported
    /// only for the generation that is active now.
    #[must_use]
    pub fn status(&self) -> CastGateStatus {
        let activation = self.activation();
        let generation = activation.map_or(0, |(generation, _)| generation);
        let current =
            generation != 0 && self.progress_generation.load(Ordering::Acquire) == generation;
        let count = |cell: &AtomicU64| {
            if current {
                cell.load(Ordering::Acquire)
            } else {
                0
            }
        };
        CastGateStatus {
            active: activation.is_some(),
            generation,
            ready: current && self.ready.load(Ordering::Acquire),
            video_segments: count(&self.video_segments),
            audio_segments: count(&self.audio_segments),
            dropped_packets: self.dropped_packets(),
            restarts: self.restarts.load(Ordering::Acquire),
            last_publish_unix_ms: self.last_publish_unix_ms.load(Ordering::Acquire),
            last_error: self.last_error(),
        }
    }

    /// Disable the output for the run with `reason` (the text names the fix)
    /// and deactivate it.
    pub(crate) fn disable(&self, reason: String) {
        self.set_error(Some(reason));
        self.disabled.store(true, Ordering::Release);
        self.deactivate();
    }

    /// The run ended: the output can no longer be activated.
    pub(crate) fn end(&self) {
        self.ended.store(true, Ordering::Release);
        self.deactivate();
        self.ready.store(false, Ordering::Release);
    }

    /// Record (or with `None` clear) the last error.
    pub(crate) fn set_error(&self, error: Option<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// A packager for `generation` starts: its progress is zero.
    pub(crate) fn begin_generation(&self, generation: u64) {
        self.ready.store(false, Ordering::Release);
        self.video_segments.store(0, Ordering::Release);
        self.audio_segments.store(0, Ordering::Release);
        self.progress_generation
            .store(generation, Ordering::Release);
    }

    /// Publish the packager's progress (sink thread, atomics only).
    pub(crate) fn publish_progress(&self, video: u64, audio: u64, master: bool) {
        let before = self.video_segments.swap(video, Ordering::AcqRel);
        self.audio_segments.store(audio, Ordering::Release);
        if video != before {
            let now_ms = u64::try_from(unix_nanos() / 1_000_000).unwrap_or(0);
            self.last_publish_unix_ms.store(now_ms, Ordering::Release);
        }
        self.ready
            .store(master && video >= CAST_READY_SEGMENTS, Ordering::Release);
    }

    /// The packager was replaced after an error.
    pub(crate) fn note_restart(&self) {
        self.restarts.fetch_add(1, Ordering::AcqRel);
        self.ready.store(false, Ordering::Release);
    }

    /// Install the sink-thread hook (tests only).
    #[cfg(test)]
    pub(crate) fn set_test_hook(&self, hook: Option<CastTestHook>) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = hook;
    }

    /// Run the sink-thread hook, if one is installed (tests only).
    #[cfg(test)]
    pub(crate) fn run_test_hook(&self) {
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Why a directory is not this cast output's to write and delete in.
fn foreign_dir(id: &str, dir: &Path) -> String {
    format!(
        "cast output {id:?}: {} holds files this engine did not write; point `dir` at an empty \
         directory or remove `dir`",
        dir.display()
    )
}

/// Whether `dir` carries this output's ownership marker.
fn has_marker(dir: &Path, id: &str) -> bool {
    let marker = dir.join(CAST_DIR_MARKER);
    // `symlink_metadata` does not follow a symlink: a marker must be a
    // regular file this engine wrote.
    std::fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file())
        && std::fs::read_to_string(&marker).is_ok_and(|text| text.trim_end() == id)
}

/// What a cast output found at its directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastDirState {
    /// Nothing exists at the path.
    Absent,
    /// An empty directory.
    Empty,
    /// A directory that carries this output's marker.
    Owned,
}

/// Look at `dir` without changing anything: is it the cast output `id`'s to
/// use? Usable = absent, empty, or carrying the marker file
/// [`CAST_DIR_MARKER`] with this output's id. Anything else (other files
/// and no marker, another output's marker, not a directory) is refused.
///
/// # Errors
/// An operator sentence that names the fix.
pub fn inspect_cast_dir(dir: &Path, id: &str) -> Result<CastDirState, String> {
    let io =
        |error: &std::io::Error| format!("cast output {id:?}: reading {}: {error}", dir.display());
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CastDirState::Absent),
        Err(error) => Err(io(&error)),
        // A symlink or a file is never a cast directory.
        Ok(meta) if !meta.is_dir() => Err(foreign_dir(id, dir)),
        Ok(_) if has_marker(dir, id) => Ok(CastDirState::Owned),
        Ok(_) => {
            let mut entries = std::fs::read_dir(dir).map_err(|e| io(&e))?;
            if entries.next().is_some() {
                Err(foreign_dir(id, dir))
            } else {
                Ok(CastDirState::Empty)
            }
        }
    }
}

/// Take ownership of `dir` for the cast output `id`, or say why not: an
/// absent directory is created `0700`, and an absent or empty one gets the
/// marker. A directory [`inspect_cast_dir`] refuses is left untouched.
///
/// # Errors
/// An operator sentence that names the fix.
pub fn claim_cast_dir(dir: &Path, id: &str) -> Result<(), String> {
    let io = |what: &str, error: &std::io::Error| {
        format!("cast output {id:?}: {what} {}: {error}", dir.display())
    };
    match inspect_cast_dir(dir, id)? {
        CastDirState::Owned => return Ok(()),
        CastDirState::Empty => {}
        CastDirState::Absent => {
            std::fs::create_dir_all(dir).map_err(|e| io("creating", &e))?;
            restrict_to_owner(dir);
        }
    }
    std::fs::write(dir.join(CAST_DIR_MARKER), id.as_bytes()).map_err(|e| io("marking", &e))
}

/// `chmod 0700` (best effort; a no-op off Unix).
fn restrict_to_owner(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o700);
        if let Err(error) = std::fs::set_permissions(dir, mode) {
            tracing::warn!(dir = %dir.display(), %error, "cast output: could not restrict its directory to the owner");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Remove the stream files of the cast output `id` from `dir`: only when the
/// directory still carries this output's marker, and only files the packager
/// writes (never the marker, never anything else).
pub(crate) fn purge_owned(dir: &Path, id: &str) {
    if has_marker(dir, id) {
        multiview_output::hls::cast::purge_dir(dir);
    }
}

/// Check `dir` for the cast output behind `gate` when a run starts. A
/// directory this output owns is purged of a previous run's stream files;
/// an absent or empty one is left as it is (it is claimed on activation).
/// An unusable one disables the output for the run (one warning; nothing is
/// written or deleted): it never fails the run.
pub(crate) fn prepare_cast_dir(dir: &Path, id: &str, gate: &CastGate) {
    match inspect_cast_dir(dir, id) {
        Ok(CastDirState::Owned) => multiview_output::hls::cast::purge_dir(dir),
        Ok(CastDirState::Absent | CastDirState::Empty) => {}
        Err(reason) => {
            tracing::warn!(output = id, %reason, "cast output disabled for this run");
            gate.disable(reason);
        }
    }
}

/// The default directory of the cast output `id` under the engine's state
/// directory: `<state dir>/cast/<id>`.
#[must_use]
pub fn default_cast_dir(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("cast").join(id)
}

/// One cast output of the running engine, as the control plane sees it.
#[derive(Debug, Clone)]
pub struct CastOutputEntry {
    /// The output's config id.
    pub id: String,
    /// The directory its stream is written to while active.
    pub dir: PathBuf,
    /// Its activation gate.
    pub gate: Arc<CastGate>,
    /// The encoded picture width (the output's format, else the canvas).
    pub width: u32,
    /// The encoded picture height.
    pub height: u32,
    /// The output cadence in frames per second, as an exact rational.
    pub fps: multiview_core::time::Rational,
    /// The encoder that feeds it (for example `libx264`).
    pub codec: String,
}

/// The cast outputs of one run, in configured order.
pub type CastOutputs = Arc<Vec<CastOutputEntry>>;

/// Where a run publishes its [`CastOutputs`] for the control plane: filled
/// when the run starts, emptied when it ends.
pub type CastOutputsSlot = Arc<multiview_framestore::SwapCell<Arc<Vec<CastOutputEntry>>>>;

/// An empty [`CastOutputsSlot`].
#[must_use]
pub fn cast_outputs_slot() -> CastOutputsSlot {
    Arc::new(multiview_framestore::SwapCell::new(Arc::new(Vec::new())))
}

/// Empties a [`CastOutputsSlot`] and ends every gate in it when the run that
/// filled it returns (normally, with an error, or by unwinding), so the
/// control plane never activates an output of a run that is gone.
pub(crate) struct CastRunGuard(pub(crate) CastOutputsSlot);

impl Drop for CastRunGuard {
    fn drop(&mut self) {
        for entry in self.0.load().iter() {
            entry.gate.end();
        }
        self.0.store(Arc::new(Vec::new()));
    }
}

/// The control plane's [`CastEngine`](multiview_control::cast_engine::CastEngine)
/// over a run's [`CastOutputsSlot`]. Every call is one lock-free slot load
/// plus atomics: it never waits on the engine.
pub struct CastEngineAdapter(pub CastOutputsSlot);

impl CastEngineAdapter {
    fn entry(
        &self,
        id: &str,
    ) -> Result<CastOutputEntry, multiview_control::cast_engine::CastEngineError> {
        use multiview_control::cast_engine::CastEngineError;
        let outputs = self.0.load();
        if outputs.is_empty() {
            return Err(CastEngineError::NotRunning);
        }
        outputs
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .ok_or_else(|| CastEngineError::UnknownOutput(id.to_owned()))
    }
}

impl multiview_control::cast_engine::CastEngine for CastEngineAdapter {
    fn outputs(&self) -> Vec<multiview_control::cast_engine::CastOutputStatus> {
        self.0
            .load()
            .iter()
            .map(|entry| {
                let status = entry.gate.status();
                multiview_control::cast_engine::CastOutputStatus {
                    id: entry.id.clone(),
                    dir: entry.dir.clone(),
                    active: status.active,
                    ready: status.ready,
                    generation: status.generation,
                    video_segments: status.video_segments,
                    dropped_packets: status.dropped_packets,
                    last_error: status.last_error,
                    width: entry.width,
                    height: entry.height,
                    fps_num: u32::try_from(entry.fps.num).unwrap_or(0),
                    fps_den: u32::try_from(entry.fps.den).unwrap_or(0),
                }
            })
            .collect()
    }

    fn activate(
        &self,
        id: &str,
        preroll: bool,
    ) -> Result<u64, multiview_control::cast_engine::CastEngineError> {
        use multiview_control::cast_engine::CastEngineError;
        self.entry(id)?
            .gate
            .activate(preroll)
            .map_err(|error| match error {
                CastGateError::Disabled(reason) => CastEngineError::Unavailable(reason),
                CastGateError::Ended => CastEngineError::NotRunning,
            })
    }

    fn deactivate(&self, id: &str) -> Result<(), multiview_control::cast_engine::CastEngineError> {
        self.entry(id)?.gate.deactivate();
        Ok(())
    }
}

fn installed() -> &'static Mutex<Option<CastOutputsSlot>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<CastOutputsSlot>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or with `None` clear) the run's cast-outputs slot for the
/// control plane (mirrors `output_routing_control::install_output_routing`).
pub fn install_cast_outputs(slot: Option<CastOutputsSlot>) {
    *installed().lock().unwrap_or_else(PoisonError::into_inner) = slot;
}

/// The installed cast-outputs slot, if any.
#[must_use]
pub fn installed_cast_outputs() -> Option<CastOutputsSlot> {
    installed()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}
