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
}
impl BindingJournal {
    /// Hold the stable lock inode until all native/audio owners have exited.
    /// Never unlink this file: doing so would let two boots lock different inodes.
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
    pub fn new(root: &Path, player: &str) -> Self {
        let key: String = player
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            path: root.join("player-bindings").join(format!("{key}.json")),
            player: player.to_owned(),
        }
    }
    /// Absence alone permits the configured default. Every other error fails closed.
    pub fn load(&self) -> io::Result<Option<SlotSnapshot>> {
        self.recover()
            .map(|record| record.map(|(snapshot, _)| snapshot))
    }
    /// Recover the binding plus the detach identity whose intent survived a crash.
    pub fn recover(&self) -> io::Result<Option<(SlotSnapshot, Option<super::DetachRequest>)>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if record.version != 1 || record.player != self.player || record.generation == 0 {
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
    pub fn store(&self, snapshot: &SlotSnapshot, operation: Option<u64>) -> io::Result<()> {
        self.store_intent(snapshot, operation, None)
    }
    /// Store an unload intent with its exact CAS identity for duplicate recovery.
    pub fn store_detach(
        &self,
        snapshot: &SlotSnapshot,
        request: super::DetachRequest,
    ) -> io::Result<()> {
        self.store_intent(snapshot, Some(request.operation), Some(request.expected.0))
    }
    fn store_intent(
        &self,
        snapshot: &SlotSnapshot,
        operation: Option<u64>,
        expected: Option<u64>,
    ) -> io::Result<()> {
        let record = Record {
            version: 1,
            player: self.player.clone(),
            generation: snapshot.generation.0,
            operation,
            expected,
            reference: snapshot.reference.clone().map(|r| match r {
                SlotReference::Asset { id } => Reference::Asset { id },
                SlotReference::Artifact { id, generation } => {
                    Reference::Artifact { id, generation }
                }
            }),
        };
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("journal parent missing"))?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        std::fs::File::open(parent)?.sync_all()
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
}
