//! Player-owned current-binding journal. Only startup and the player ingest
//! executor access disk; the output clock never accesses this module.
use super::{Generation, SlotReference, SlotSnapshot};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    player: String,
    generation: u64,
    operation: Option<u64>,
    #[serde(default)]
    expected: Option<u64>,
    reference: Option<Reference>,
    /// Absent in records written before two-phase commit: those are Committed.
    #[serde(default = "committed_present_default")]
    committed_present: bool,
    /// Prepared (not yet A/V-committed) intent. Never a boot binding.
    #[serde(default)]
    prepared: Option<Prepared>,
}
fn committed_present_default() -> bool {
    true
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    generation: u64,
    operation: Option<u64>,
    reference: Option<Reference>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Reference {
    Asset { id: String },
    Artifact { id: String, generation: u64 },
}
/// One per-player file beneath the configured engine state directory.
#[derive(Clone)]
pub struct BindingJournal {
    path: PathBuf,
    player: String,
    /// Test-only seam run after the new record is fsynced and before it becomes
    /// visible; lets a test inject a newer request deterministically mid-write.
    #[cfg(test)]
    pub(crate) precommit_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}
impl BindingJournal {
    /// Hold the stable lock inode until all native/audio owners have exited.
    /// Never unlink this file: doing so would let two boots lock different inodes.
    ///
    /// # Errors
    /// Returns the I/O error when the owner file cannot be created or is
    /// already locked by another runtime.
    pub fn claim_runtime(&self) -> io::Result<std::sync::Arc<std::fs::File>> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("journal parent missing"))?;
        std::fs::create_dir_all(parent)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("owner"))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(io::Error::from)?;
        Ok(std::sync::Arc::new(file))
    }
    /// Encode identity as hex, never as a caller-controlled path component.
    #[must_use]
    pub fn new(root: &Path, player: &str) -> Self {
        let key = player
            .as_bytes()
            .iter()
            .fold(String::new(), |mut acc, byte| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{byte:02x}");
                acc
            });
        Self {
            path: root.join("player-bindings").join(format!("{key}.json")),
            player: player.to_owned(),
            #[cfg(test)]
            precommit_hook: None,
        }
    }
    /// Absence alone permits the configured default. Every other error fails closed.
    ///
    /// # Errors
    /// Returns the I/O or decode error for an unreadable or invalid journal.
    pub fn load(&self) -> io::Result<Option<SlotSnapshot>> {
        self.recover()
            .map(|record| record.map(|(snapshot, _)| snapshot))
    }
    /// Recover the binding plus the detach identity whose intent survived a crash.
    ///
    /// # Errors
    /// Returns the I/O or decode error for an unreadable or invalid journal.
    pub fn recover(&self) -> io::Result<Option<(SlotSnapshot, Option<super::DetachRequest>)>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if record.version != 1 || record.player != self.player {
            return Err(io::Error::other("invalid player binding journal identity"));
        }
        if !record.committed_present {
            // Only a prepared intent exists: fail closed to "nothing committed".
            return if record.prepared.is_some() {
                Ok(None)
            } else {
                Err(io::Error::other("invalid player binding journal identity"))
            };
        }
        if record.generation == 0 {
            return Err(io::Error::other("invalid player binding journal identity"));
        }
        let operation = record
            .operation
            .zip(record.expected)
            .map(|(operation, expected)| super::DetachRequest {
                operation,
                expected: Generation(expected),
            });
        Ok(Some((
            SlotSnapshot {
                generation: Generation(record.generation),
                reference: record.reference.map(|r| match r {
                    Reference::Asset { id } => SlotReference::Asset { id },
                    Reference::Artifact { id, generation } => {
                        SlotReference::Artifact { id, generation }
                    }
                }),
            },
            operation,
        )))
    }
    /// Atomic durable precommit record. Failure leaves the live slot untouched.
    ///
    /// # Errors
    /// Returns the I/O error when the record cannot be durably written.
    pub fn store(&self, snapshot: &SlotSnapshot, operation: Option<u64>) -> io::Result<()> {
        self.store_intent(snapshot, operation, None)
    }
    /// Store an unload intent with its exact CAS identity for duplicate recovery.
    ///
    /// # Errors
    /// Returns the I/O error when the record cannot be durably written.
    pub fn store_detach(
        &self,
        snapshot: &SlotSnapshot,
        request: super::DetachRequest,
    ) -> io::Result<()> {
        self.store_intent(snapshot, Some(request.operation), Some(request.expected.0))
    }
    /// Two-phase prepare. Persists a **Prepared** record: the previous
    /// committed binding is retained verbatim and `snapshot` is recorded only as
    /// intent, so a crash anywhere before [`Self::commit`] recovers the last
    /// committed binding (or nothing), never `snapshot`. After the prepared
    /// record is durable, `claim` runs (post-fsync supersession / atomic commit
    /// claim). When it declines, the prior record is restored and `Ok(false)` is
    /// returned.
    ///
    /// # Errors
    /// Returns the I/O or decode error when the record cannot be written.
    pub fn store_if_current(
        &self,
        snapshot: &SlotSnapshot,
        operation: Option<u64>,
        claim: impl FnOnce() -> bool,
    ) -> io::Result<bool> {
        let prior = match std::fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let mut record = match &prior {
            Some(bytes) => {
                let record: Record = serde_json::from_slice(bytes).map_err(io::Error::other)?;
                if record.version != 1 || record.player != self.player {
                    return Err(io::Error::other("invalid player binding journal identity"));
                }
                record
            }
            None => Record {
                version: 1,
                player: self.player.clone(),
                generation: 0,
                operation: None,
                expected: None,
                reference: None,
                committed_present: false,
                prepared: None,
            },
        };
        record.prepared = Some(Prepared {
            generation: snapshot.generation.0,
            operation,
            reference: snapshot.reference.clone().map(Into::into),
        });
        self.write_record(&record)?;
        if claim() {
            return Ok(true);
        }
        match prior {
            Some(bytes) => self.write_bytes(&bytes)?,
            None => {
                std::fs::remove_file(&self.path)?;
                std::fs::File::open(self.path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
            }
        }
        Ok(false)
    }
    /// Finalize a prepared binding as **Committed**, after the actual video and
    /// audio commit. Durable before returning; never called on the output clock.
    ///
    /// # Errors
    /// Returns the I/O error when the record cannot be durably written.
    pub fn commit(&self, snapshot: &SlotSnapshot) -> io::Result<()> {
        self.store_intent(snapshot, None, None)
    }
    fn write_record(&self, record: &Record) -> io::Result<()> {
        self.write_bytes(&serde_json::to_vec(record).map_err(io::Error::other)?)
    }
    fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("journal parent missing"))?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        #[cfg(test)]
        if let Some(hook) = &self.precommit_hook {
            hook();
        }
        temp.persist(&self.path).map_err(|e| e.error)?;
        std::fs::File::open(parent)?.sync_all()
    }
    fn store_intent(
        &self,
        snapshot: &SlotSnapshot,
        operation: Option<u64>,
        expected: Option<u64>,
    ) -> io::Result<()> {
        self.write_record(&Record {
            version: 1,
            player: self.player.clone(),
            generation: snapshot.generation.0,
            operation,
            expected,
            reference: snapshot.reference.clone().map(Into::into),
            committed_present: true,
            prepared: None,
        })
    }
}
impl From<SlotReference> for Reference {
    fn from(r: SlotReference) -> Self {
        match r {
            SlotReference::Asset { id } => Self::Asset { id },
            SlotReference::Artifact { id, generation } => Self::Artifact { id, generation },
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precommit_failure_keeps_existing_record_and_checks_identity() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-1");
        let snapshot = SlotSnapshot {
            generation: Generation(3),
            reference: Some(SlotReference::Asset {
                id: "clip-a".into(),
            }),
        };
        journal.store(&snapshot, None).unwrap();
        assert_eq!(journal.load().unwrap(), Some(snapshot.clone()));
        let blocked = dir.path().join("not-directory");
        std::fs::write(&blocked, b"file").unwrap();
        assert!(BindingJournal::new(&blocked, "vt-1")
            .store(&snapshot, None)
            .is_err());
        assert_eq!(journal.load().unwrap(), Some(snapshot));
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal.path).unwrap()).unwrap();
        record["player"] = "other".into();
        std::fs::write(&journal.path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(journal.load().is_err());
    }
    #[test]
    fn newer_request_during_fsync_restores_prior_record_and_never_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = BindingJournal::new(dir.path(), "vt-1");
        let asset = |id: &str, g| SlotSnapshot {
            generation: Generation(g),
            reference: Some(SlotReference::Asset { id: id.into() }),
        };
        let old = asset("old", 1);
        journal.store(&old, None).unwrap();
        let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2));
        let hook_latest = std::sync::Arc::clone(&latest);
        // B (generation 3) arrives while A (generation 2) is being fsynced.
        journal.precommit_hook = Some(std::sync::Arc::new(move || {
            hook_latest.store(3, std::sync::atomic::Ordering::Release);
        }));
        let a = asset("a", 2);
        let committed = journal
            .store_if_current(&a, None, || {
                latest.load(std::sync::atomic::Ordering::Acquire) == 2
            })
            .unwrap();
        assert!(!committed, "superseded A must not commit");
        assert_eq!(
            journal.load().unwrap(),
            Some(old.clone()),
            "prior durable binding restored"
        );
        // B then commits normally and is the durable winner.
        journal.precommit_hook = None;
        let b = asset("b", 3);
        assert!(journal
            .store_if_current(&b, None, || latest
                .load(std::sync::atomic::Ordering::Acquire)
                == 3)
            .unwrap());
        assert_eq!(
            journal.load().unwrap(),
            Some(old),
            "claimed B is only Prepared until the A/V commit finalizes it"
        );
        journal.commit(&b).unwrap();
        assert_eq!(journal.load().unwrap(), Some(b));
    }
    #[test]
    fn superseded_first_ever_record_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-9");
        let a = SlotSnapshot {
            generation: Generation(1),
            reference: Some(SlotReference::Asset { id: "a".into() }),
        };
        assert!(!journal.store_if_current(&a, None, || false).unwrap());
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn detach_operation_recovery_requires_runtime_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-1");
        let request = super::super::DetachRequest {
            operation: 9,
            expected: Generation(5),
        };
        let snapshot = SlotSnapshot {
            generation: Generation(6),
            reference: None,
        };
        journal.store_detach(&snapshot, request).unwrap();
        let (binding, operation) = journal.recover().unwrap().unwrap();
        let port = super::super::DetachPort::recovered(operation.unwrap(), binding.generation);
        let ticket = port.submit(request).unwrap();
        assert_eq!(ticket.acknowledgement(), None);
        assert!(port.take().is_some());
        assert!(port.take().is_none());
    }
    #[test]
    fn unloaded_survives_restart_and_corruption_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt/1");
        assert_eq!(journal.load().unwrap(), None);
        let unloaded = SlotSnapshot {
            generation: Generation(9),
            reference: None,
        };
        journal.store(&unloaded, Some(42)).unwrap();
        assert_eq!(journal.load().unwrap(), Some(unloaded));
        std::fs::write(&journal.path, b"broken").unwrap();
        assert!(journal.load().is_err());
    }
    fn asset(id: &str, g: u64) -> SlotSnapshot {
        SlotSnapshot {
            generation: Generation(g),
            reference: Some(SlotReference::Asset { id: id.into() }),
        }
    }
    #[test]
    fn crash_between_prepare_and_commit_recovers_old_binding() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-1");
        let old = asset("old", 1);
        journal.store(&old, None).unwrap();
        // "Crash": claim succeeds, process dies before commit().
        assert!(journal
            .store_if_current(&asset("new", 2), None, || true)
            .unwrap());
        assert_eq!(journal.recover().unwrap().unwrap().0, old);
        journal.commit(&asset("new", 2)).unwrap();
        assert_eq!(journal.load().unwrap(), Some(asset("new", 2)));
    }
    #[test]
    fn prepared_first_ever_load_recovers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-1");
        assert!(journal
            .store_if_current(&asset("new", 1), None, || true)
            .unwrap());
        assert_eq!(journal.load().unwrap(), None);
    }
    #[test]
    fn crash_in_prepare_fsync_window_recovers_old() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = BindingJournal::new(dir.path(), "vt-1");
        journal.store(&asset("old", 1), None).unwrap();
        let path = journal.path.clone();
        let player = journal.player.clone();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let hook_seen = std::sync::Arc::clone(&seen);
        journal.precommit_hook = Some(std::sync::Arc::new(move || {
            // Crash point: temp fsynced, rename not yet done.
            let probe = BindingJournal {
                path: path.clone(),
                player: player.clone(),
                precommit_hook: None,
            };
            *hook_seen.lock().unwrap() = Some(probe.load().unwrap());
        }));
        journal
            .store_if_current(&asset("new", 2), None, || true)
            .unwrap();
        assert_eq!(seen.lock().unwrap().clone().unwrap(), Some(asset("old", 1)));
    }
    #[test]
    fn legacy_v1_record_without_new_fields_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = BindingJournal::new(dir.path(), "vt-1");
        std::fs::create_dir_all(journal.path.parent().unwrap()).unwrap();
        std::fs::write(
            &journal.path,
            br#"{"version":1,"player":"vt-1","generation":4,"operation":null,"reference":{"kind":"Asset","id":"x"}}"#,
        )
        .unwrap();
        assert_eq!(journal.load().unwrap(), Some(asset("x", 4)));
    }
}
