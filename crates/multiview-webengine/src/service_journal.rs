//! Durable journal of the capture service: id/generation high-water marks, job
//! intents and results, and the latest-good table.
//!
//! The journal holds ONLY identities, counters, error classes and artifact
//! metadata (id, owner, fill) — never a capture document, a password, a path or
//! a URL — and is bounded by the service's `history`, `max_fills` and the
//! high-water counters, so it cannot grow with the number of jobs ever run.
//! Every update is an atomic whole-file replace: write a private temp file,
//! `fsync` it, rename over the journal, `fsync` the directory. The in-memory
//! image is replaced only after all three steps succeeded, so a failed write
//! never makes memory claim more than the disk proves. The file carries a
//! SHA-256 line over its body; a torn, truncated or edited file fails closed.
//! If a failure happens after the rename, the journal re-reads the file and
//! adopts what is actually there (still reporting the error).
//! Use a journal path dedicated to one store (the store's own lock guards the
//! store, not this file).
use crate::artifact::ArtifactId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Largest accepted journal file; anything bigger is corrupt by construction.
const MAX_BYTES: u64 = 1 << 20;
const VERSION: u32 = 1;

/// Fault-injection boundaries of one journal write; the default hook does nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalBoundary {
    /// Before the temp file `fsync`.
    Sync,
    /// Before the rename that publishes the new journal.
    Publish,
    /// Before the directory `fsync` that makes the rename durable.
    DirectorySync,
}
/// Hook type for [`JournalBoundary`] fault injection.
pub type JournalHook = fn(JournalBoundary) -> io::Result<()>;

/// Persisted job state. `Running` is the intent record written before launch;
/// it is resolved against the artifact store at the next boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Rec {
    Queued,
    Running,
    Succeeded { artifact: ArtifactId },
    Failed { kind: String },
    Cancelled,
    Uncertain,
}
impl Rec {
    /// Queued or running: not yet resolved, never pruned.
    pub(crate) fn open(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}
/// One job intent/result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JobRec {
    pub(crate) id: u64,
    pub(crate) owner: String,
    pub(crate) fill: String,
    pub(crate) generation: u64,
    /// Artifact id reserved by the store for this launch, journaled before any
    /// producer exists or writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reserved: Option<ArtifactId>,
    #[serde(flatten)]
    pub(crate) state: Rec,
}
/// One latest-good entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GoodRec {
    pub(crate) owner: String,
    pub(crate) fill: String,
    pub(crate) artifact: ArtifactId,
    pub(crate) generation: u64,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) job: u64,
}
/// The whole persisted document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Doc {
    pub(crate) v: u32,
    /// Exclusive: no job id `>=` this was ever exposed.
    pub(crate) job_high: u64,
    /// Exclusive: no generation `>=` this was ever launched.
    pub(crate) generation_high: u64,
    /// Oldest first.
    pub(crate) jobs: Vec<JobRec>,
    pub(crate) good: Vec<GoodRec>,
}
impl Doc {
    pub(crate) fn new() -> Self {
        Self {
            v: VERSION,
            job_high: 1,
            generation_high: 1,
            jobs: Vec::new(),
            good: Vec::new(),
        }
    }
    /// Drop the oldest resolved jobs beyond `history`. Counters are untouched,
    /// so pruning never lets an id or generation be reused.
    pub(crate) fn prune(&mut self, history: usize) {
        let resolved = self.jobs.iter().filter(|j| !j.state.open()).count();
        let mut excess = resolved.saturating_sub(history);
        self.jobs.retain(|j| {
            if excess > 0 && !j.state.open() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "capture journal invalid")
}
fn hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Stable on-disk name of an error class.
pub(crate) fn kind_name(kind: io::ErrorKind) -> &'static str {
    use io::ErrorKind as K;
    match kind {
        K::NotFound => "NotFound",
        K::PermissionDenied => "PermissionDenied",
        K::TimedOut => "TimedOut",
        K::Interrupted => "Interrupted",
        K::StorageFull => "StorageFull",
        K::InvalidInput => "InvalidInput",
        K::InvalidData => "InvalidData",
        K::WouldBlock => "WouldBlock",
        K::UnexpectedEof => "UnexpectedEof",
        K::BrokenPipe => "BrokenPipe",
        _ => "Other",
    }
}
/// Inverse of [`kind_name`]; unknown names become `Other`.
pub(crate) fn kind_from(name: &str) -> io::ErrorKind {
    use io::ErrorKind as K;
    match name {
        "NotFound" => K::NotFound,
        "PermissionDenied" => K::PermissionDenied,
        "TimedOut" => K::TimedOut,
        "Interrupted" => K::Interrupted,
        "StorageFull" => K::StorageFull,
        "InvalidInput" => K::InvalidInput,
        "InvalidData" => K::InvalidData,
        "WouldBlock" => K::WouldBlock,
        "UnexpectedEof" => K::UnexpectedEof,
        "BrokenPipe" => K::BrokenPipe,
        _ => K::Other,
    }
}

/// Read and verify the journal; `None` when it does not exist.
pub(crate) fn load(path: &Path) -> io::Result<Option<Doc>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(m) if !m.is_file() || m.len() > MAX_BYTES => return Err(corrupt()),
        Ok(_) => {}
    }
    let mut raw = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut raw)?;
    if u64::try_from(raw.len()).map_or(true, |n| n > MAX_BYTES) {
        return Err(corrupt());
    }
    let split = raw.iter().position(|b| *b == b'\n').ok_or_else(corrupt)?;
    let (check, body) = raw.split_at(split);
    let body = body.get(1..).ok_or_else(corrupt)?;
    if check != hex(body).as_bytes() {
        return Err(corrupt());
    }
    let doc: Doc = serde_json::from_slice(body).map_err(|_| corrupt())?;
    if doc.v != VERSION {
        return Err(corrupt());
    }
    Ok(Some(doc))
}

fn write_atomic(path: &Path, doc: &Doc, hook: JournalHook) -> io::Result<()> {
    let body = serde_json::to_vec(doc).map_err(|_| corrupt())?;
    let mut bytes = hex(&body).into_bytes();
    bytes.push(b'\n');
    bytes.extend_from_slice(&body);
    if u64::try_from(bytes.len()).map_or(true, |n| n > MAX_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "capture journal too large",
        ));
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    hook(JournalBoundary::Sync)?;
    file.sync_all()?;
    drop(file);
    hook(JournalBoundary::Publish)?;
    std::fs::rename(&temp, path)?;
    hook(JournalBoundary::DirectorySync)?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

/// The durable image plus its file. Memory equals the last fully durable write.
pub(crate) struct Journal {
    path: PathBuf,
    pub(crate) doc: Doc,
    hook: JournalHook,
    history: usize,
}
impl Journal {
    pub(crate) fn new(path: &Path, doc: Doc, hook: JournalHook, history: usize) -> Self {
        Self {
            path: path.to_owned(),
            doc,
            hook,
            history,
        }
    }
    /// Apply `change` to a copy, persist it durably, and only then adopt it.
    pub(crate) fn update(&mut self, change: impl FnOnce(&mut Doc)) -> io::Result<()> {
        let mut next = self.doc.clone();
        change(&mut next);
        next.prune(self.history);
        match write_atomic(&self.path, &next, self.hook) {
            Ok(()) => {
                self.doc = next;
                Ok(())
            }
            Err(e) => {
                // A failure after the rename (directory fsync) leaves the new
                // document visible though unproven durable. Memory must follow
                // the file, never the stale pre-write image, so a retry cannot
                // erase it. The caller still sees the error (uncertain).
                if let Ok(Some(actual)) = load(&self.path) {
                    self.doc = actual;
                }
                Err(e)
            }
        }
    }
}
