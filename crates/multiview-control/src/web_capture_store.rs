//! Private durable stores beside the capture journal: terms acceptance and idempotency.
//! All methods are blocking; callers run them on a management blocking worker.
//! Records never contain captured content, URLs, credentials or raw idempotency keys.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Maximum retained idempotency records.
pub const LEDGER_CAPACITY: usize = 1024;

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
    file: Mutex<File>,
}
impl DurableTerms {
    /// Open or create a private regular file; symlinks are refused.
    /// # Errors
    /// Unsafe or unwritable path.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: Mutex::new(open_private(path)?),
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
        f.write_all(&line)?;
        f.sync_all()
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
}
type Id = (String, String, String);
/// Bounded actor+operation+key map. No lock is held across the guarded operation.
pub struct Ledger {
    inner: Mutex<(HashMap<Id, Rec>, VecDeque<Id>)>,
    file: Option<Mutex<File>>,
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
    /// Load, compact and reopen a private durable ledger.
    /// # Errors
    /// Unsafe path or I/O failure.
    pub fn durable(path: &Path) -> io::Result<Self> {
        let mut map = HashMap::new();
        let mut order = VecDeque::new();
        if std::fs::symlink_metadata(path).is_ok() {
            drop(open_private(path)?); // validates regular file
            for line in io::BufReader::new(File::open(path)?).lines() {
                let Ok(r) = serde_json::from_str::<Rec>(&line?) else {
                    continue;
                };
                let id = (r.actor.clone(), r.op.clone(), r.key.clone());
                if map.insert(id.clone(), r).is_none() {
                    order.push_back(id);
                }
                while order.len() > LEDGER_CAPACITY {
                    if let Some(old) = order.pop_front() {
                        map.remove(&old);
                    }
                }
            }
            let tmp: PathBuf = path.with_extension("tmp");
            let _ = std::fs::remove_file(&tmp);
            {
                let mut t = open_private(&tmp)?;
                for id in &order {
                    if let Some(r) = map.get(id) {
                        let mut l = serde_json::to_vec(r).map_err(io::Error::other)?;
                        l.push(b'\n');
                        t.write_all(&l)?;
                    }
                }
                t.sync_all()?;
            }
            std::fs::rename(&tmp, path)?;
        }
        Ok(Self {
            inner: Mutex::new((map, order)),
            file: Some(Mutex::new(open_private(path)?)),
        })
    }
    fn id(actor: &str, op: &str, key: &str) -> Id {
        (actor.to_owned(), op.to_owned(), digest_hex(key.as_bytes()))
    }
    /// Reserve `(actor, op, key)` for `digest`.
    /// # Errors
    /// `Err(true)` divergent body for a used key; `Err(false)` same key in flight.
    pub fn reserve(&self, actor: &str, op: &str, key: &str, digest: &str) -> Result<Reserve, bool> {
        let id = Self::id(actor, op, key);
        let mut g = self.inner.lock().map_err(|_| true)?;
        if let Some(r) = g.0.get(&id) {
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
        };
        // Never evict uncertain/in-flight intent: it may already have started a
        // durable capture. Retrying such a key must fail closed, not duplicate it.
        while g.1.len() >= LEDGER_CAPACITY {
            let Some(position) =
                g.1.iter()
                    .position(|old| g.0.get(old).is_some_and(|r| r.value.is_some()))
            else {
                return Err(false);
            };
            let Some(old) = g.1.remove(position) else {
                return Err(false);
            };
            g.0.remove(&old);
        }
        if let Some(file) = &self.file {
            let mut line = serde_json::to_vec(&rec).map_err(|_| false)?;
            line.push(b'\n');
            let mut file = file.lock().map_err(|_| false)?;
            file.write_all(&line)
                .and_then(|()| file.sync_all())
                .map_err(|_| false)?;
        }
        g.1.push_back(id.clone());
        g.0.insert(id, rec);
        Ok(Reserve::Fresh)
    }
    /// Record the result durably before the caller acknowledges.
    /// # Errors
    /// Durable write failure.
    pub fn complete(&self, actor: &str, op: &str, key: &str, value: u64) -> io::Result<()> {
        let id = Self::id(actor, op, key);
        let rec = {
            let mut g = self
                .inner
                .lock()
                .map_err(|_| io::Error::other("ledger poisoned"))?;
            let Some(r) = g.0.get(&id) else {
                return Err(io::Error::other("idempotency reservation missing"));
            };
            let mut record = r.clone();
            record.value = Some(value);
            record
        };
        if let Some(f) = &self.file {
            let mut l = serde_json::to_vec(&rec).map_err(io::Error::other)?;
            l.push(b'\n');
            let mut f = f.lock().map_err(|_| io::Error::other("ledger poisoned"))?;
            f.write_all(&l)?;
            f.sync_all()?;
        }
        self.inner
            .lock()
            .map_err(|_| io::Error::other("ledger poisoned"))?
            .0
            .insert(id, rec);
        Ok(())
    }
    /// Drop an unfinished reservation so the client may retry.
    pub fn abandon(&self, actor: &str, op: &str, key: &str) {
        let id = Self::id(actor, op, key);
        if let Ok(mut g) = self.inner.lock() {
            if g.0.get(&id).is_some_and(|r| r.value.is_none()) {
                g.0.remove(&id);
                g.1.retain(|x| *x != id);
            }
        }
    }
}
