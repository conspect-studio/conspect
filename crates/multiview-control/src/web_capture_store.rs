//! Private durable stores beside the capture journal: terms acceptance and idempotency.
//! All methods are blocking; callers run them on a management blocking worker.
//! Records never contain captured content, URLs, credentials or raw idempotency keys.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Maximum retained idempotency records.
pub const LEDGER_CAPACITY: usize = 1024;

/// Maximum bytes of the terms acceptance log; further writes fail `StorageFull`.
pub const TERMS_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum bytes of the idempotency journal accepted at startup or written at runtime.
pub const LEDGER_MAX_BYTES: u64 = 4 * 1024 * 1024;

fn storage_full(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, format!("{what} is full"))
}

/// One durable actor acceptance of site terms, made before a job is accepted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermsAcceptance {
    /// Authenticated API key identity.
    pub key_id: String,
    /// Authorized fill object.
    pub fill: String,
    /// Accepted terms version.
    pub version: String,
    /// Server UTC time, seconds since the Unix epoch.
    pub accepted_at_unix: u64,
}

/// Durable acceptance sink. Production wiring must be [`DurableTerms`].
pub trait TermsRepository: Send + Sync + 'static {
    /// Persist the acceptance before returning.
    /// # Errors
    /// Fails when it cannot be made durable.
    fn record(&self, acceptance: &TermsAcceptance) -> io::Result<()>;
}

/// In-memory repository for tests only.
#[derive(Default)]
pub struct MemoryTerms(pub Mutex<Vec<TermsAcceptance>>);
impl TermsRepository for MemoryTerms {
    fn record(&self, a: &TermsAcceptance) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("terms store poisoned"))?
            .push(a.clone());
        Ok(())
    }
}

/// Append-only private file with fsync per record.
pub struct DurableTerms {
    file: Mutex<(File, u64)>,
}
impl DurableTerms {
    /// Open or create a private regular file; symlinks are refused.
    /// # Errors
    /// Unsafe or unwritable path.
    pub fn open(path: &Path) -> io::Result<Self> {
        let existed = std::fs::symlink_metadata(path).is_ok();
        let file = open_private(path)?;
        if !existed {
            sync_parent(path)?;
        }
        let len = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new((file, len)),
        })
    }
}
impl TermsRepository for DurableTerms {
    fn record(&self, a: &TermsAcceptance) -> io::Result<()> {
        let mut line = serde_json::to_vec(a).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut f = self
            .file
            .lock()
            .map_err(|_| io::Error::other("terms store poisoned"))?;
        let next =
            f.1.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        if next > TERMS_MAX_BYTES {
            return Err(storage_full("terms log"));
        }
        f.0.write_all(&line)?;
        f.0.sync_all()?;
        f.1 = next;
        Ok(())
    }
}

/// Open a 0600 append file, refusing a symlink or non-regular target.
/// # Errors
/// Unsafe or unwritable path.
pub fn open_private(path: &Path) -> io::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => {
            return Err(io::Error::other("capture store path is not a regular file"))
        }
        Ok(_) | Err(_) => {}
    }
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Fsync the parent directory so a create or rename survives power loss.
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

/// Create a fresh private temp file; an existing entry is removed first (a
/// symlink is unlinked, never followed) and `create_new` (`O_EXCL`) fails closed.
fn create_tmp(tmp: &Path) -> io::Result<File> {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(tmp)
}

/// Hex SHA-256.
#[must_use]
pub fn digest_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Outcome of reserving an idempotency key.
#[derive(Debug, PartialEq, Eq)]
pub enum Reserve {
    /// First use; caller must `complete` or `abandon`.
    Fresh,
    /// Same body already accepted with this value.
    Replay(u64),
}

#[derive(Clone, Serialize, Deserialize)]
struct Rec {
    actor: String,
    op: String,
    key: String,
    digest: String,
    value: Option<u64>,
    /// Durable abandon marker: removes the pending reservation on replay.
    #[serde(default)]
    tombstone: bool,
}
type Id = (String, String, String);
#[derive(Default)]
struct State {
    map: HashMap<Id, Rec>,
    order: VecDeque<Id>,
}
impl State {
    fn apply(&mut self, r: Rec) {
        let id = (r.actor.clone(), r.op.clone(), r.key.clone());
        if r.tombstone {
            self.remove(&id);
        } else if self.map.insert(id.clone(), r).is_none() {
            self.order.push_back(id);
        }
    }
    fn remove(&mut self, id: &Id) {
        if self.map.remove(id).is_some() {
            self.order.retain(|x| x != id);
        }
    }
    fn pending(&self) -> usize {
        self.map.values().filter(|r| r.value.is_none()).count()
    }
    /// Evict oldest completed records until `len() < limit`; never a pending one.
    /// Returns false when only pending records remain above the limit.
    fn make_room(&mut self, limit: usize) -> bool {
        while self.order.len() >= limit {
            let Some(pos) = self
                .order
                .iter()
                .position(|id| self.map.get(id).is_some_and(|r| r.value.is_some()))
            else {
                return false;
            };
            if let Some(old) = self.order.remove(pos) {
                self.map.remove(&old);
            }
        }
        true
    }
    fn snapshot(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(r) = self.map.get(id) {
                serde_json::to_writer(&mut out, r).map_err(io::Error::other)?;
                out.push(b'\n');
            }
        }
        Ok(out)
    }
}
struct Log {
    file: File,
    len: u64,
    path: PathBuf,
}
impl Log {
    /// Durably append one record, compacting to `state` when the bound is reached.
    fn append(&mut self, state: &State, rec: &Rec) -> io::Result<()> {
        let mut line = serde_json::to_vec(rec).map_err(io::Error::other)?;
        line.push(b'\n');
        if self
            .len
            .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
            > LEDGER_MAX_BYTES
        {
            let snap = state.snapshot()?;
            if u64::try_from(snap.len().saturating_add(line.len())).unwrap_or(u64::MAX)
                > LEDGER_MAX_BYTES
            {
                return Err(storage_full("idempotency journal"));
            }
            self.file = write_snapshot(&self.path, &snap)?;
            self.len = u64::try_from(snap.len()).unwrap_or(u64::MAX);
        }
        self.file.write_all(&line)?;
        self.file.sync_all()?;
        self.len += u64::try_from(line.len()).unwrap_or(u64::MAX);
        Ok(())
    }
}
/// Atomically replace `path` with `bytes` (fsync file, rename, fsync parent) and
/// return a fresh append handle.
fn write_snapshot(path: &Path, bytes: &[u8]) -> io::Result<File> {
    let tmp: PathBuf = path.with_extension("tmp");
    {
        let mut t = create_tmp(&tmp)?;
        t.write_all(bytes)?;
        t.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    sync_parent(path)?;
    open_private(path)
}
/// Bounded actor+operation+key map. Pending reservations are never evicted.
pub struct Ledger {
    inner: Mutex<State>,
    file: Option<Mutex<Log>>,
}
impl Ledger {
    /// Volatile ledger (tests).
    #[must_use]
    pub fn memory() -> Self {
        Self {
            inner: Mutex::default(),
            file: None,
        }
    }
    /// Replay every record and tombstone, bound memory, compact and reopen a
    /// private durable ledger. Lock order is always `inner` then `file`.
    /// # Errors
    /// Unsafe path, corrupt or oversize journal, more than [`LEDGER_CAPACITY`]
    /// pending reservations, or I/O failure.
    pub fn durable(path: &Path) -> io::Result<Self> {
        let mut state = State::default();
        if std::fs::symlink_metadata(path).is_ok() {
            drop(open_private(path)?); // validates regular file
            let f = File::open(path)?;
            if f.metadata()?.len() > LEDGER_MAX_BYTES {
                return Err(io::Error::other("capture idempotency journal is too large"));
            }
            // Bounded read: the length check above plus `take` guard growth races.
            let mut text = String::new();
            f.take(LEDGER_MAX_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|_| io::Error::other("capture idempotency journal is corrupt"))?;
            if u64::try_from(text.len()).unwrap_or(u64::MAX) > LEDGER_MAX_BYTES {
                return Err(io::Error::other("capture idempotency journal is too large"));
            }
            for line in text.lines() {
                let r: Rec = serde_json::from_str(line)
                    .map_err(|_| io::Error::other("capture idempotency journal is corrupt"))?;
                state.apply(r);
            }
            if state.pending() > LEDGER_CAPACITY {
                return Err(io::Error::other(
                    "capture idempotency journal has more pending reservations than capacity",
                ));
            }
            // Same order as runtime: oldest completed first, never pending.
            let limit = LEDGER_CAPACITY + 1;
            if !state.make_room(limit) {
                return Err(io::Error::other(
                    "capture idempotency journal over capacity",
                ));
            }
            let snap = state.snapshot()?;
            drop(write_snapshot(path, &snap)?);
        } else {
            drop(open_private(path)?);
            sync_parent(path)?;
        }
        let file = open_private(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            inner: Mutex::new(state),
            file: Some(Mutex::new(Log {
                file,
                len,
                path: path.to_owned(),
            })),
        })
    }
    fn id(actor: &str, op: &str, key: &str) -> Id {
        (actor.to_owned(), op.to_owned(), digest_hex(key.as_bytes()))
    }
    fn persist(&self, state: &State, rec: &Rec) -> io::Result<()> {
        match &self.file {
            Some(f) => f
                .lock()
                .map_err(|_| io::Error::other("ledger poisoned"))?
                .append(state, rec),
            None => Ok(()),
        }
    }
    /// Reserve `(actor, op, key)` for `digest`.
    /// # Errors
    /// `Err(true)` divergent body for a used key; `Err(false)` same key in flight,
    /// ledger full of pending reservations, or the reservation could not be stored.
    pub fn reserve(&self, actor: &str, op: &str, key: &str, digest: &str) -> Result<Reserve, bool> {
        let id = Self::id(actor, op, key);
        let mut g = self.inner.lock().map_err(|_| true)?;
        if let Some(r) = g.map.get(&id) {
            if r.digest != digest {
                return Err(true);
            }
            return r.value.map_or(Err(false), |v| Ok(Reserve::Replay(v)));
        }
        let rec = Rec {
            actor: id.0.clone(),
            op: id.1.clone(),
            key: id.2.clone(),
            digest: digest.to_owned(),
            value: None,
            tombstone: false,
        };
        // Never evict pending intent: it may already have started a durable
        // capture. Completed records go oldest first, only if room is needed.
        if g.order.len() >= LEDGER_CAPACITY {
            let mut probe = State {
                map: g.map.clone(),
                order: g.order.clone(),
            };
            if !probe.make_room(LEDGER_CAPACITY) {
                return Err(false);
            }
            // Persist the reservation first (compaction sees the evicted view).
            self.persist(&probe, &rec).map_err(|_| false)?;
            *g = probe;
        } else {
            self.persist(&g, &rec).map_err(|_| false)?;
        }
        g.order.push_back(id.clone());
        g.map.insert(id, rec);
        Ok(Reserve::Fresh)
    }
    /// Record the result durably before the caller acknowledges.
    /// # Errors
    /// Missing reservation or durable write failure.
    pub fn complete(&self, actor: &str, op: &str, key: &str, value: u64) -> io::Result<()> {
        let id = Self::id(actor, op, key);
        let mut g = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("ledger poisoned"))?;
        let Some(r) = g.map.get(&id) else {
            return Err(io::Error::other("idempotency reservation missing"));
        };
        let mut rec = r.clone();
        rec.value = Some(value);
        self.persist(&g, &rec)?;
        g.map.insert(id, rec);
        Ok(())
    }
    /// Durably drop an unfinished reservation so the client may retry. The
    /// tombstone is fsynced BEFORE the in-memory removal; on error the
    /// reservation stays pending (fail closed). Completed records are untouched.
    /// # Errors
    /// Durable write failure.
    pub fn abandon(&self, actor: &str, op: &str, key: &str) -> io::Result<()> {
        let id = Self::id(actor, op, key);
        let mut g = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("ledger poisoned"))?;
        let Some(r) = g.map.get(&id) else {
            return Ok(());
        };
        if r.value.is_some() {
            return Ok(());
        }
        let mut tomb = r.clone();
        tomb.tombstone = true;
        self.persist(&g, &tomb)?;
        g.remove(&id);
        Ok(())
    }
}
