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
use rustix::fs::{self, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
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
    /// Opaque caller correlation (64 lowercase hex, a SHA-256 of caller-chosen
    /// request identity). Journaled with the queued intent BEFORE the producer
    /// or the id is exposed; never carries payload, URLs or credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) correlation: Option<String>,
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

/// A well-formed correlation token: exactly 64 lowercase hex digits.
pub(crate) fn valid_correlation(t: &str) -> bool {
    t.len() == 64
        && t.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

/// Pin the parent directory once; every later open, rename and fsync of the
/// journal is relative to this descriptor, so a replaced path cannot redirect them.
fn pin_parent(path: &Path) -> io::Result<(File, &OsStr)> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path.file_name().ok_or_else(corrupt)?;
    let dir: File = fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into();
    Ok((dir, name))
}
/// `openat(NOFOLLOW)` then `fstat` the descriptor: regular, owned by this
/// process, mode exactly 0600, one link. Nothing is read or published before this.
fn open_private_at(dir: &File, name: &OsStr, flags: OFlags) -> io::Result<File> {
    let file: File = fs::openat(
        dir,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )?
    .into();
    let stat = fs::fstat(&file)?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(corrupt());
    }
    Ok(file)
}

/// Read and verify the journal; `None` when it does not exist.
pub(crate) fn load(path: &Path) -> io::Result<Option<Doc>> {
    let (dir, name) = pin_parent(path)?;
    let file = match open_private_at(&dir, name, OFlags::RDONLY) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(f) => f,
    };
    if file.metadata()?.len() > MAX_BYTES {
        return Err(corrupt());
    }
    let mut raw = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut raw)?;
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
    let (dir, name) = pin_parent(path)?;
    let mut temp_name = name.to_owned();
    temp_name.push(".tmp");
    match fs::unlinkat(&dir, temp_name.as_os_str(), fs::AtFlags::empty()) {
        Ok(()) => {}
        Err(e) if e == rustix::io::Errno::NOENT => {}
        Err(e) => return Err(e.into()),
    }
    // EXCL: only a brand-new inode; fstat proves no other link or owner exists
    // before a single byte is written or the inode can be renamed into place.
    let mut file = open_private_at(
        &dir,
        temp_name.as_os_str(),
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
    )?;
    file.write_all(&bytes)?;
    hook(JournalBoundary::Sync)?;
    file.sync_all()?;
    hook(JournalBoundary::Publish)?;
    // Re-prove the same still-open inode immediately before publication: a link
    // made after creation means the temp is refused, never renamed into place.
    if fs::fstat(&file)?.st_nlink != 1 {
        return Err(corrupt());
    }
    fs::renameat(&dir, temp_name.as_os_str(), &dir, name)?;
    drop(file);
    hook(JournalBoundary::DirectorySync)?;
    dir.sync_all()
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
