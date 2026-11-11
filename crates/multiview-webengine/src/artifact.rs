//! Durable, local-only job output handoff. Management-worker I/O only.
//!
//! A runner writes through `begin`/`Write`, closes its encoder, then calls
//! `complete`. Never import a caller path or a URL. No fetch/capture runner
//! exists in this crate yet. This API does not change `JobQueue` transitions.
//! The root is a private service directory, not writable by untrusted users.

use rustix::fs::{self, AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

type Result<T> = io::Result<T>;
fn invalid() -> io::Error {
    io::Error::other("artifact unavailable or invalid")
}

/// Admission includes committed bytes, orphan bytes and the full new reservation.
#[derive(Clone, Copy)]
pub struct Limits {
    /// Maximum occupied and reserved bytes, including metadata.
    pub bytes: u64,
    /// Maximum occupied and reserved physical files.
    pub count: usize,
}
/// Opaque identity; contains no path or source URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ArtifactId(String);
impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}
impl ArtifactId {
    /// Parse a wire ID without accepting path syntax.
    /// # Errors
    /// Rejects all noncanonical identities.
    pub fn parse(id: &str) -> Result<Self> {
        if id.len() != 17 || !id.starts_with('a') || !id[1..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        Ok(Self(id.to_owned()))
    }
    /// Return the opaque wire identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// Trusted job provenance. Deliberately excludes free-form URLs and credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    /// Authorization owner object, never a URL.
    pub owner: String,
    /// Fill object that produced these bytes.
    pub fill: String,
    /// Producer job identity.
    pub job: String,
    /// Nonzero generation of this fill output.
    pub generation: u64,
}
/// Completed local object and its trusted producer facts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    /// Stable identity, independent of fill refresh.
    pub id: ArtifactId,
    /// Producer identity and authorization owner.
    pub provenance: Provenance,
    /// Complete content length.
    pub bytes: u64,
    /// SHA-256 of completed bytes.
    pub sha256: String,
}
struct Entry {
    record: Artifact,
    pins: Arc<AtomicUsize>,
}
/// Fault-injection boundaries; the default hook performs no work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// Before content fsync.
    BytesSync,
    /// Before content rename.
    BytesPublish,
    /// Before content directory fsync.
    BytesDirectorySync,
    /// Before manifest fsync.
    MetadataSync,
    /// Before manifest rename.
    MetadataPublish,
    /// Before durable commit directory fsync.
    CommitDirectorySync,
}
/// No output-clock caller may own or invoke this store. One process owns a root.
pub struct ArtifactStore {
    root: File,
    lock: File,
    limits: Limits,
    used: u64,
    count: usize,
    next: u64,
    entries: BTreeMap<ArtifactId, Entry>,
    hook: fn(Boundary) -> Result<()>,
}
fn open(root: &File, name: &str, flags: OFlags) -> Result<File> {
    Ok(fs::openat(
        root,
        name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )?
    .into())
}
/// Validate an already-opened descriptor: regular, owned by this process, no
/// group/other access, and exactly one directory entry. Every check is `fstat` on
/// the fd, never a second path lookup.
pub(crate) fn check_private(file: &File) -> Result<()> {
    let stat = fs::fstat(file)?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
        || stat.st_nlink != 1
    {
        return Err(invalid());
    }
    Ok(())
}
fn read_regular(root: &File, name: &str) -> Result<File> {
    let file = open(root, name, OFlags::RDONLY)?;
    check_private(&file)?;
    Ok(file)
}
impl ArtifactStore {
    /// Open an existing private root. Locks out concurrent stores; recover only
    /// durable manifests. Unknown/partial files consume quota but are never read.
    /// # Errors
    /// Returns I/O, locking, quota, or corrupt-index errors.
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        let root: File = fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let mut lock = open(&root, "lock", OFlags::CREATE | OFlags::RDWR)?;
        check_private(&lock)?;
        fs::flock(&lock, fs::FlockOperation::NonBlockingLockExclusive)?;
        let mut watermark = String::new();
        Read::by_ref(&mut lock)
            .take(32)
            .read_to_string(&mut watermark)?;
        let next = if watermark.is_empty() {
            1
        } else if watermark.len() == 20 {
            watermark.parse::<u64>().map_err(|_| invalid())?
        } else {
            return Err(invalid());
        };
        let mut store = Self {
            root,
            lock,
            limits,
            used: 0,
            count: 0,
            next,
            entries: BTreeMap::new(),
            hook: |_| Ok(()),
        };
        // Names come from the opened root fd (never a path walk); every actual open
        // is fd-relative, no-follow, single-component, and `fstat`-validated.
        let mut dir = fs::Dir::read_from(&store.root)?;
        while let Some(entry) = dir.read() {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                return Err(invalid());
            };
            let name = name.to_owned();
            let name = name.as_str();
            if name == "." || name == ".." || name == "lock" {
                continue;
            }
            let prefix = name.split('.').next().ok_or_else(invalid)?;
            let id = ArtifactId::parse(prefix)?;
            let seq = u64::from_str_radix(&prefix[1..], 16).map_err(|_| invalid())?;
            store.next = store.next.max(seq.checked_add(1).ok_or_else(invalid)?);
            let file = read_regular(&store.root, name)?;
            let size = file.metadata()?.len();
            store.used = store.used.checked_add(size).ok_or_else(invalid)?;
            store.count += 1;
            if store.used > limits.bytes || store.count > limits.count {
                return Err(invalid());
            }
            if Path::new(name).extension().is_some_and(|ext| ext == "json") {
                if size > 4096 {
                    return Err(invalid());
                }
                let record: Artifact = serde_json::from_reader(file).map_err(|_| invalid())?;
                if record.id != id {
                    return Err(invalid());
                }
                // Missing/corrupt content fails closed at pin, not by a refetch.
                store.entries.insert(
                    id,
                    Entry {
                        record,
                        pins: Arc::new(AtomicUsize::new(0)),
                    },
                );
            }
        }
        Ok(store)
    }
    /// Install deterministic failure hooks on a management test instance.
    pub fn set_boundary_hook(&mut self, hook: fn(Boundary) -> Result<()>) {
        self.hook = hook;
    }
    /// Authorize owner before any storage mutation. Reservations cover content
    /// plus a bounded manifest; count is physical files, not merely logical jobs.
    /// # Errors
    /// Returns denied, stale-generation, quota, or I/O errors.
    pub fn begin(
        &mut self,
        provenance: Provenance,
        max_bytes: u64,
        allow: impl FnOnce(&Provenance) -> bool,
    ) -> Result<Pending<'_>> {
        if !allow(&provenance) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if provenance.generation == 0
            || [&provenance.owner, &provenance.fill, &provenance.job]
                .iter()
                .any(|s| s.is_empty() || s.len() > 128 || s.contains("://"))
        {
            return Err(invalid());
        }
        if self.entries.values().any(|e| {
            e.record.provenance.owner == provenance.owner
                && e.record.provenance.fill == provenance.fill
                && e.record.provenance.generation >= provenance.generation
        }) {
            return Err(invalid());
        }
        let reserved = max_bytes.checked_add(4096).ok_or_else(invalid)?;
        if max_bytes == 0
            || self
                .used
                .checked_add(reserved)
                .is_none_or(|n| n > self.limits.bytes)
            || self.count + 3 > self.limits.count
        {
            return Err(io::ErrorKind::StorageFull.into());
        }
        let id = ArtifactId(format!("a{:016x}", self.next));
        self.next = self.next.checked_add(1).ok_or_else(invalid)?;
        // Reserve identity durably before exposing it, even if all objects are
        // later deleted. A torn watermark fails closed on the next open.
        self.lock.seek(SeekFrom::Start(0))?;
        self.lock
            .write_all(format!("{:020}", self.next).as_bytes())?;
        self.lock.sync_all()?;
        self.root.sync_all()?;
        let file = open(
            &self.root,
            &format!("{}.part", id.0),
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
        )?;
        self.used += reserved;
        self.count += 3;
        Ok(Pending {
            store: self,
            id,
            provenance,
            file,
            max_bytes,
            written: 0,
            reserved,
            published: false,
        })
    }
    /// Authorize before existence lookup or filesystem access. Hash validation
    /// rejects partial, missing, or modified content. No network API exists.
    /// # Errors
    /// Returns denied, unavailable, corrupt-content, or I/O errors.
    pub fn pin(
        &self,
        id: &ArtifactId,
        owner: &str,
        allow: impl FnOnce(&ArtifactId) -> bool,
    ) -> Result<Lease> {
        if !allow(id) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let entry = self
            .entries
            .get(id)
            .filter(|e| e.record.provenance.owner == owner)
            .ok_or_else(invalid)?;
        let mut file = read_regular(&self.root, &format!("{}.data", id.0))?;
        if file.metadata()?.len() != entry.record.bytes {
            return Err(invalid());
        }
        let mut digest = Sha256::new();
        io::copy(&mut file, &mut DigestWriter(&mut digest))?;
        if format!("{:x}", digest.finalize()) != entry.record.sha256 {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(0))?;
        entry.pins.fetch_add(1, Ordering::SeqCst);
        Ok(Lease {
            _lock: self.lock.try_clone()?,
            file,
            record: entry.record.clone(),
            pins: entry.pins.clone(),
        })
    }
    /// Read-only metadata snapshot of committed artifacts. `owner_allowed` runs
    /// on each owner before that artifact is considered; no content is read, no
    /// path or URL is returned and nothing is mutated or collected.
    #[must_use]
    pub fn snapshot(&self, owner_allowed: impl Fn(&str) -> bool) -> Vec<Artifact> {
        self.entries
            .values()
            .filter(|e| owner_allowed(&e.record.provenance.owner))
            .map(|e| e.record.clone())
            .collect()
    }
    /// Explicit cleanup only. Refresh never deletes an old generation.
    /// # Errors
    /// Returns denied, unavailable, pinned, or I/O errors.
    pub fn remove(
        &mut self,
        id: &ArtifactId,
        owner: &str,
        allow: impl FnOnce(&ArtifactId) -> bool,
    ) -> Result<()> {
        if !allow(id) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let entry = self
            .entries
            .get(id)
            .filter(|e| e.record.provenance.owner == owner)
            .ok_or_else(invalid)?;
        if entry.pins.load(Ordering::SeqCst) != 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let manifest = format!("{}.json", id.0);
        let metadata_bytes = read_regular(&self.root, &manifest)?.metadata()?.len();
        let content_bytes = entry.record.bytes;
        fs::unlinkat(&self.root, manifest, AtFlags::empty())?;
        self.root.sync_all()?;
        self.entries.remove(id);
        fs::unlinkat(&self.root, format!("{}.data", id.0), AtFlags::empty())?;
        self.root.sync_all()?;
        // Recount on restart handles uncertain deletion failures conservatively.
        self.used -= content_bytes + metadata_bytes;
        self.count -= 2;
        Ok(())
    }
}
struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, b: &[u8]) -> Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
/// Runner-owned write transaction. No path or mutable file handle escapes.
pub struct Pending<'a> {
    store: &'a mut ArtifactStore,
    id: ArtifactId,
    provenance: Provenance,
    file: File,
    max_bytes: u64,
    written: u64,
    reserved: u64,
    published: bool,
}
impl Write for Pending<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if u64::try_from(buf.len()).map_err(|_| invalid())? > self.max_bytes - self.written {
            return Err(io::ErrorKind::StorageFull.into());
        }
        let n = self.file.write(buf)?;
        self.written += u64::try_from(n).map_err(|_| invalid())?;
        Ok(n)
    }
    fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }
}
impl Pending<'_> {
    /// Identity to retain for reconciliation before completion.
    #[must_use]
    pub fn id(&self) -> &ArtifactId {
        &self.id
    }
    /// Commit only after successful producer completion. An error after
    /// publication is uncertain: reopen and reconcile the reserved ID.
    /// # Errors
    /// Returns incomplete-content or I/O errors, including uncertain commits.
    pub fn complete(mut self) -> Result<Artifact> {
        if self.written == 0 {
            return Err(invalid());
        }
        (self.store.hook)(Boundary::BytesSync)?;
        self.file.sync_all()?;
        let part = format!("{}.part", self.id.0);
        let data = format!("{}.data", self.id.0);
        (self.store.hook)(Boundary::BytesPublish)?;
        // Mark uncertain before the first persistent publication. Drop must not
        // erase a possibly committed object after any later failure.
        self.published = true;
        fs::renameat(&self.store.root, &part, &self.store.root, &data)?;
        (self.store.hook)(Boundary::BytesDirectorySync)?;
        self.store.root.sync_all()?;
        let mut file = read_regular(&self.store.root, &data)?;
        let mut hash = Sha256::new();
        io::copy(&mut file, &mut DigestWriter(&mut hash))?;
        let record = Artifact {
            id: self.id.clone(),
            provenance: self.provenance.clone(),
            bytes: self.written,
            sha256: format!("{:x}", hash.finalize()),
        };
        let json = serde_json::to_vec(&record).map_err(|_| invalid())?;
        if json.len() > 4096 {
            return Err(invalid());
        }
        let temp = format!("{}.meta", self.id.0);
        let mut metadata = open(
            &self.store.root,
            &temp,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
        )?;
        metadata.write_all(&json)?;
        (self.store.hook)(Boundary::MetadataSync)?;
        metadata.sync_all()?;
        (self.store.hook)(Boundary::MetadataPublish)?;
        fs::renameat(
            &self.store.root,
            &temp,
            &self.store.root,
            format!("{}.json", self.id.0),
        )?;
        (self.store.hook)(Boundary::CommitDirectorySync)?;
        self.store.root.sync_all()?;
        self.store.entries.insert(
            self.id.clone(),
            Entry {
                record: record.clone(),
                pins: Arc::new(AtomicUsize::new(0)),
            },
        );
        self.store.used -=
            self.reserved - self.written - u64::try_from(json.len()).map_err(|_| invalid())?;
        self.store.count -= 1;
        Ok(record)
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.published
            && fs::unlinkat(
                &self.store.root,
                format!("{}.part", self.id.0),
                AtFlags::empty(),
            )
            .is_ok()
        {
            self.store.used -= self.reserved;
            self.store.count -= 3;
        }
    }
}
/// A read-only local capability. Hold it throughout downstream copy/commit.
/// Leases do not survive process death; durable downstream references need their
/// own journal before they permit artifact cleanup.
pub struct Lease {
    // Retain process ownership if a lease outlives its store handle.
    _lock: File,
    file: File,
    record: Artifact,
    pins: Arc<AtomicUsize>,
}
impl Lease {
    /// Completed identity and provenance, never a local path.
    #[must_use]
    pub fn artifact(&self) -> &Artifact {
        &self.record
    }
}
impl Read for Lease {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.file.read(buf)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.pins.fetch_sub(1, Ordering::SeqCst);
    }
}
