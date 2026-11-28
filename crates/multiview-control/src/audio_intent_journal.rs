//! Private, append-only durable audio-intent admission primitive (#105, I1-A).
//!
//! The live REST route is deliberately NOT wired here. This module proves the
//! reserve → submit → fsync order and the limits of replay without pretending
//! that a journal is an engine completion receipt.
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use serde::{Deserialize, Serialize};

use crate::{
    command::{Command, CommandSender, OperationId, SubmitError},
    concurrency::{IdempotencyStore, Reservation},
};

const VERSION: u16 = 1;
const MAX_RECORD: usize = 4096;

/// A stable destination route, independent of a live alias binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteIdentity {
    /// Configured, stable audio route destination id.
    pub destination: String,
    /// Stable configured source id, not its current player alias.
    pub source: String,
}

/// Presence, explicit set, or removal of one authored field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FieldIntent<T> {
    /// Leave authored state unchanged.
    Absent,
    /// Author a value (including 0 dB and false).
    Set(T),
    /// Remove the authored value and reveal config-derived state.
    Reset,
}

/// The versioned private record; future payload kinds can be added by tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioIntentRecord {
    /// On-disk format version.
    pub version: u16,
    /// Scoped Idempotency-Key (resource namespace included).
    pub idempotency_key: String,
    /// Exactly the operation id submitted to the bounded command bus.
    pub operation_id: OperationId,
    /// Typed payload for the admitted operation.
    pub payload: IntentPayload,
}

/// Durable typed intent payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum IntentPayload {
    /// One audio route with independent authored gain and mute fields.
    AudioRoute {
        /// Stable source and destination identities.
        route: RouteIdentity,
        /// Gain authorship action in dB.
        gain_db: FieldIntent<f32>,
        /// Mute authorship action.
        mute: FieldIntent<bool>,
    },
}

impl AudioIntentRecord {
    /// Construct a versioned audio-route record.
    #[must_use]
    pub fn audio_route(
        key: String,
        op: OperationId,
        route: RouteIdentity,
        gain_db: FieldIntent<f32>,
        mute: FieldIntent<bool>,
    ) -> Self {
        Self {
            version: VERSION,
            idempotency_key: key,
            operation_id: op,
            payload: IntentPayload::AudioRoute {
                route,
                gain_db,
                mute,
            },
        }
    }
}

/// A complete durable record that can be adopted without re-submission.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedIntent(pub AudioIntentRecord);

/// A typed admission outcome; an fsync error cannot truthfully mean not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Submission and one-record fsync both succeeded; a REST surface may acknowledge.
    Durable(OperationId),
    /// This key already named a committed operation; the bus was not re-submitted.
    Replay(OperationId),
    /// The bus accepted the operation, but fsync failed. It may have applied.
    Indeterminate(OperationId),
}

/// Rejection before bus submission; never interpreted as an applied route.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// The command id and journal id differ; neither is submitted.
    #[error("command and record operation ids differ")]
    OperationMismatch,
    /// The key is not namespaced for audio intent.
    #[error("idempotency key must be scoped to audio intent")]
    InvalidKey,
    /// A record is oversized or contains a non-finite authored gain.
    #[error("audio intent record is invalid")]
    InvalidRecord,
    /// The bounded idempotency store refused a new key.
    #[error("idempotency store at capacity")]
    Capacity,
    /// Another control-plane journal operation is active; retry later.
    #[error("audio intent journal busy; retry later")]
    Busy,
    /// The bounded command bus refused admission.
    #[error("command bus refused admission: {0}")]
    Submit(#[from] SubmitError),
    /// The journal has an unresolved post-submit fsync failure.
    #[error("audio intent journal has indeterminate admission; reconcile before reuse")]
    Poisoned,
    /// The journal could not be opened or read.
    #[error("audio intent journal I/O: {0}")]
    Io(#[from] io::Error),
    /// Replay found an incomplete, corrupt, or unsupported record; fail closed.
    #[error("audio intent journal corrupt at line {line}")]
    Corrupt {
        /// One-based line number; zero reports a conflicting imported key.
        line: usize,
    },
}

/// Storage abstraction makes post-submit fsync failure reproducible in tests.
pub trait RecordSink: Send {
    /// Append one complete record and flush it durably.
    ///
    /// # Errors
    /// An I/O error leaves admission indeterminate; it is never a rollback.
    fn append_sync(&mut self, line: &[u8]) -> io::Result<()>;
}

struct FileSink(File);
impl RecordSink for FileSink {
    fn append_sync(&mut self, line: &[u8]) -> io::Result<()> {
        self.0.write_all(line)?;
        self.0.sync_data()
    }
}

/// The first refused record; preceding checksum-valid records remain adopted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedTail {
    /// One-based line number.
    pub line: usize,
    /// Byte offset of the first invalid record.
    pub offset: u64,
}

/// Boot adoption result. A degraded journal refuses new durable admissions,
/// while unrelated control operations and the output clock remain independent.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayReport {
    /// Complete validated and imported prefix, in journal order.
    pub accepted_prefix: Vec<ReplayedIntent>,
    /// Exact first invalid record, if any.
    pub degraded: Option<DegradedTail>,
    /// Whether this journal may admit new work after recovery.
    pub admitted: bool,
}

/// One private control-plane journal, serialized off the output clock.
pub struct AudioIntentJournal {
    path: PathBuf,
    /// Serialize reserve, submit, fsync and replay off the output clock.
    gate: Mutex<()>,
    sink: Mutex<Box<dyn RecordSink>>,
    keys: IdempotencyStore,
    indeterminate: AtomicBool,
    uncertain: Mutex<Option<(String, OperationId)>>,
    replayed: AtomicBool,
    degraded: Mutex<Option<DegradedTail>>,
}

impl AudioIntentJournal {
    /// Open or create the journal. Call [`Self::replay`] before admitting work.
    ///
    /// # Errors
    /// File creation or opening can fail.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let path = path.into();
        // Creation changes the directory entry. Sync the new empty file first,
        // then its parent; a later append extends only the existing file and
        // needs only the file's sync_data. Rotation/rename is not implemented.
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(new_file) => {
                new_file.sync_all()?;
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                File::open(parent)?.sync_all()?;
                new_file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                OpenOptions::new().append(true).open(&path)?
            }
            Err(error) => return Err(JournalError::Io(error)),
        };
        // Reopen in append mode so concurrent descriptors cannot overwrite
        // earlier records. The gate serializes admissions within this instance.
        drop(file);
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            path,
            gate: Mutex::new(()),
            sink: Mutex::new(Box::new(FileSink(file))),
            keys: IdempotencyStore::new(),
            indeterminate: AtomicBool::new(false),
            uncertain: Mutex::new(None),
            replayed: AtomicBool::new(false),
            degraded: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn with_sink(path: PathBuf, sink: Box<dyn RecordSink>) -> Self {
        Self {
            path,
            gate: Mutex::new(()),
            sink: Mutex::new(sink),
            keys: IdempotencyStore::new(),
            indeterminate: AtomicBool::new(false),
            uncertain: Mutex::new(None),
            replayed: AtomicBool::new(false),
            degraded: Mutex::new(None),
        }
    }

    /// Return the recorded operation for one feature-scoped key, if known.
    #[must_use]
    pub fn operation_for(&self, key: &str) -> Option<OperationId> {
        if !key.starts_with("audio-intent:") {
            return None;
        }
        self.keys.peek(key)
    }

    /// Return the first bad journal offset for a status surface or startup log.
    #[must_use]
    pub fn degraded_tail(&self) -> Option<DegradedTail> {
        self.degraded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Submit through an existing command; the command type is incidental to
    /// the durability proof and does not claim audio routing works yet.
    ///
    /// # Errors
    /// Typed preconditions, capacity, or bus refusal before submission.
    pub fn submit_with_journal(
        &self,
        sender: &CommandSender,
        key: &str,
        payload: IntentPayload,
        make_command: impl FnOnce(OperationId) -> Command,
    ) -> Result<Admission, JournalError> {
        // At most ONE control-plane caller enters. No waiting request queue,
        // no lock reachable from the engine clock or audio bake path.
        let _gate = self.gate.try_lock().map_err(|_| JournalError::Busy)?;
        if self.indeterminate.load(Ordering::Acquire) {
            if let Some((prior_key, op)) = self
                .uncertain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                if prior_key == key {
                    return Ok(Admission::Indeterminate(op.clone()));
                }
            }
            return Err(JournalError::Poisoned);
        }
        if !self.replayed.load(Ordering::Acquire) {
            return Err(JournalError::Poisoned);
        }
        if !key.starts_with("audio-intent:") {
            return Err(JournalError::InvalidKey);
        }
        if !valid_payload(&payload) {
            return Err(JournalError::InvalidRecord);
        }
        let op = match self.keys.reserve(Some(key)) {
            Reservation::Fresh(op) => op,
            Reservation::Replay(op) => return Ok(Admission::Replay(op)),
            Reservation::Capacity => return Err(JournalError::Capacity),
        };
        let command = make_command(op.clone());
        if command.operation_id() != &op {
            self.keys.release(Some(key), &op);
            return Err(JournalError::OperationMismatch);
        }
        let record = AudioIntentRecord {
            version: VERSION,
            idempotency_key: key.to_owned(),
            operation_id: op.clone(),
            payload,
        };
        let bytes = match encode_record(&record) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.keys.release(Some(key), &op);
                return Err(error);
            }
        };
        if let Err(error) = sender.try_submit(command) {
            self.keys.release(Some(key), &op);
            return Err(JournalError::Submit(error));
        }
        let mut sink = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sink.append_sync(&bytes).is_ok() {
            Ok(Admission::Durable(op))
        } else {
            // The write may have completed partially. Stop all admissions
            // until boot recovery or an operator reconciles the journal.
            *self
                .uncertain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((key.to_owned(), op.clone()));
            self.indeterminate.store(true, Ordering::Release);
            Ok(Admission::Indeterminate(op))
        }
    }

    /// Replay the valid prefix and report a bad tail with its exact byte offset.
    /// An absent record is UNKNOWN; no operation is fabricated or re-submitted.
    /// Admission is enabled only after the entire file has been validated.
    ///
    /// # Errors
    /// Reading the file can fail; such an error never enables admissions.
    pub fn replay(&self) -> Result<ReplayReport, JournalError> {
        let _gate = self.gate.try_lock().map_err(|_| JournalError::Busy)?;
        self.replayed.store(false, Ordering::Release);
        let file = File::open(&self.path)?;
        let mut records = Vec::new();
        let mut reader = BufReader::new(file);
        let mut distinct = std::collections::HashMap::<String, OperationId>::new();
        let mut fresh_keys = 0usize;
        let mut line_number = 0;
        let mut offset = 0u64;
        let mut degraded = None;
        loop {
            let mut line = Vec::new();
            let size = reader.read_until(b'\n', &mut line)?;
            if size == 0 {
                break;
            }
            line_number += 1;
            let invalid = size > MAX_RECORD || line.last() != Some(&b'\n');
            if !invalid {
                line.pop();
            }
            let record = if invalid { None } else { decode_record(&line) };
            let record = record.filter(|r| {
                r.version == VERSION
                    && r.idempotency_key.starts_with("audio-intent:")
                    && valid_payload(&r.payload)
            });
            let Some(record) = record else {
                degraded = Some(DegradedTail {
                    line: line_number,
                    offset,
                });
                break;
            };
            if distinct
                .get(&record.idempotency_key)
                .is_some_and(|prior| prior != &record.operation_id)
                || self
                    .keys
                    .peek(&record.idempotency_key)
                    .is_some_and(|prior| prior != record.operation_id)
            {
                degraded = Some(DegradedTail {
                    line: line_number,
                    offset,
                });
                break;
            }
            let already_in_pass = distinct.contains_key(&record.idempotency_key);
            let already_imported = self.keys.peek(&record.idempotency_key).is_some();
            if !already_in_pass && !already_imported {
                let (used, cap) = self.keys.usage();
                if used.saturating_add(fresh_keys) >= cap {
                    degraded = Some(DegradedTail {
                        line: line_number,
                        offset,
                    });
                    break;
                }
                fresh_keys += 1;
            }
            distinct.insert(record.idempotency_key.clone(), record.operation_id.clone());
            records.push(ReplayedIntent(record));
            offset = offset.saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
        }
        for ReplayedIntent(record) in &records {
            self.keys
                .import_audio_intent(&record.idempotency_key, record.operation_id.clone())
                .map_err(|_| JournalError::Corrupt { line: 0 })?;
        }
        let admitted = degraded.is_none() && !self.indeterminate.load(Ordering::Acquire);
        self.degraded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&degraded);
        self.replayed.store(admitted, Ordering::Release);
        Ok(ReplayReport {
            accepted_prefix: records,
            degraded,
            admitted,
        })
    }
}

fn valid_payload(payload: &IntentPayload) -> bool {
    match payload {
        IntentPayload::AudioRoute { route, gain_db, .. } => {
            !route.destination.is_empty()
                && !route.source.is_empty()
                && !matches!(gain_db, FieldIntent::Set(value) if !value.is_finite() || !(-96.0..=24.0).contains(value))
        }
    }
}

fn encode_record(record: &AudioIntentRecord) -> Result<Vec<u8>, JournalError> {
    let bytes = serde_json::to_vec(record).map_err(|_| JournalError::InvalidRecord)?;
    if bytes.len() + 10 > MAX_RECORD {
        return Err(JournalError::InvalidRecord);
    }
    let crc = crc32(&bytes);
    let mut line = format!("{crc:08x}:").into_bytes();
    line.extend_from_slice(&bytes);
    line.push(b'\n');
    Ok(line)
}

fn decode_record(line: &[u8]) -> Option<AudioIntentRecord> {
    let (prefix, body) = line.split_at_checked(9)?;
    if prefix.get(8) != Some(&b':') {
        return None;
    }
    let crc = u32::from_str_radix(std::str::from_utf8(prefix.get(..8)?).ok()?, 16).ok()?;
    if crc != crc32(body) {
        return None;
    }
    serde_json::from_slice(body).ok()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Read only the fully committed journal file path for a boot replay.
#[must_use]
pub fn journal_path(root: &Path) -> PathBuf {
    root.join("audio-intent.v1.journal")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::command::command_bus;

    fn payload() -> IntentPayload {
        IntentPayload::AudioRoute {
            route: RouteIdentity {
                destination: "bus-a".into(),
                source: "source-a".into(),
            },
            gain_db: FieldIntent::Set(-6.0),
            mute: FieldIntent::Set(false),
        }
    }

    struct BlockedSync {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        path: PathBuf,
    }
    impl RecordSink for BlockedSync {
        fn append_sync(&mut self, line: &[u8]) -> io::Result<()> {
            if self.started.send(()).is_ok() {
                self.release.recv().map_err(io::Error::other)?;
            }
            let mut file = OpenOptions::new().append(true).open(&self.path)?;
            file.write_all(line)?;
            file.sync_data()
        }
    }

    struct FailAfterWrite {
        path: PathBuf,
        write: bool,
    }
    impl RecordSink for FailAfterWrite {
        fn append_sync(&mut self, line: &[u8]) -> io::Result<()> {
            if self.write {
                std::fs::write(&self.path, line)?;
            }
            Err(io::Error::other("injected fsync failure"))
        }
    }

    #[test]
    fn newly_created_journal_has_durable_parent_entry_and_existing_open_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        assert!(!path.exists());
        let journal = AudioIntentJournal::open(&path).unwrap();
        assert!(
            path.exists(),
            "creation syncs new file then parent before returning"
        );
        assert!(journal.replay().unwrap().admitted);
        let (tx, _rx) = command_bus(2);
        let admission = journal
            .submit_with_journal(&tx, "audio-intent:created", payload(), |op| {
                Command::Start { op }
            })
            .unwrap();
        assert!(matches!(admission, Admission::Durable(_)));
        drop(journal);
        let reopened = AudioIntentJournal::open(&path).unwrap();
        assert_eq!(reopened.replay().unwrap().accepted_prefix.len(), 1);
        // This is a structural test, not a power-loss injection: sync_all()
        // on the parent appears only in the create_new branch above.
    }

    #[test]
    fn record_size_boundary_round_trips_smallest_and_largest_and_refuses_one_over() {
        let op = OperationId::new();
        let key = "audio-intent:x".to_owned();
        let route = RouteIdentity {
            destination: "d".into(),
            source: "s".into(),
        };
        let smallest = AudioIntentRecord::audio_route(
            key.clone(),
            op.clone(),
            route.clone(),
            FieldIntent::Absent,
            FieldIntent::Absent,
        );
        let bytes = encode_record(&smallest).unwrap();
        assert_eq!(decode_record(&bytes[..bytes.len() - 1]), Some(smallest));
        let mut largest = AudioIntentRecord::audio_route(
            key,
            op,
            route,
            FieldIntent::Absent,
            FieldIntent::Absent,
        );
        let base = encode_record(&largest).unwrap().len();
        let padding = MAX_RECORD - base;
        largest.idempotency_key.push_str(&"x".repeat(padding));
        let encoded = encode_record(&largest).unwrap();
        assert_eq!(encoded.len(), MAX_RECORD);
        assert_eq!(
            decode_record(&encoded[..encoded.len() - 1]),
            Some(largest.clone())
        );
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, encoded).unwrap();
        let journal = AudioIntentJournal::open(&path).unwrap();
        let report = journal.replay().unwrap();
        assert!(report.admitted);
        assert_eq!(report.accepted_prefix.len(), 1);
        largest.idempotency_key.push('x');
        assert!(matches!(
            encode_record(&largest),
            Err(JournalError::InvalidRecord)
        ));
    }

    #[test]
    fn replay_twice_is_idempotent_near_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let op = OperationId::new();
        let route = RouteIdentity {
            destination: "bus-a".into(),
            source: "source-a".into(),
        };
        for i in 0..2_100 {
            let record = AudioIntentRecord::audio_route(
                format!("audio-intent:{i}"),
                op.clone(),
                route.clone(),
                FieldIntent::Set(-6.0),
                FieldIntent::Absent,
            );
            file.write_all(&encode_record(&record).unwrap()).unwrap();
        }
        file.sync_data().unwrap();
        let journal = AudioIntentJournal::open(&path).unwrap();
        let first = journal.replay().unwrap();
        assert!(first.admitted);
        assert_eq!(first.accepted_prefix.len(), 2_100);
        let second = journal.replay().unwrap();
        assert_eq!(second, first, "replaying an imported prefix is a no-op");
        assert_eq!(journal.keys.usage().0, 2_100);
    }

    #[test]
    fn durable_submit_replays_recorded_key_without_resubmission() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        let journal = AudioIntentJournal::open(&path).unwrap();
        let (tx, mut rx) = command_bus(2);
        let key = "audio-intent:tenant:bus-a:one";
        assert!(journal.replay().unwrap().admitted);
        let first = journal
            .submit_with_journal(&tx, key, payload(), |op| Command::Start { op })
            .unwrap();
        let Admission::Durable(op) = first else {
            panic!("must fsync before durable outcome")
        };
        assert_eq!(rx.try_drain().first().unwrap().operation_id(), &op);
        drop(journal);
        let restarted = AudioIntentJournal::open(&path).unwrap();
        let records = restarted.replay().unwrap();
        assert!(records.admitted);
        assert_eq!(records.accepted_prefix.len(), 1);
        assert_eq!(records.accepted_prefix[0].0.operation_id, op);
        assert_eq!(restarted.operation_for(key), Some(op.clone()));
        assert_eq!(
            restarted
                .submit_with_journal(&tx, key, payload(), |op| Command::Start { op })
                .unwrap(),
            Admission::Replay(op)
        );
        assert!(rx.try_drain().is_empty(), "replay must not resubmit");
    }

    #[test]
    fn refused_submission_never_creates_phantom_journal_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        let journal = AudioIntentJournal::open(&path).unwrap();
        let (tx, _rx) = command_bus(1);
        journal.replay().unwrap();
        tx.try_submit(Command::Start {
            op: OperationId::new(),
        })
        .unwrap();
        let outcome = journal.submit_with_journal(&tx, "audio-intent:full", payload(), |op| {
            Command::Stop { op }
        });
        assert!(matches!(
            outcome,
            Err(JournalError::Submit(SubmitError::Full))
        ));
        assert_eq!(journal.replay().unwrap().accepted_prefix.len(), 0);
        assert_eq!(journal.operation_for("audio-intent:full"), None);
    }

    #[test]
    fn surviving_record_after_failed_fsync_is_indeterminate_then_adopted_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, []).unwrap();
        let journal = AudioIntentJournal::with_sink(
            path.clone(),
            Box::new(FailAfterWrite {
                path: path.clone(),
                write: true,
            }),
        );
        let (tx, mut rx) = command_bus(2);
        journal.replay().unwrap();
        let key = "audio-intent:survived";
        let result = journal
            .submit_with_journal(&tx, key, payload(), |op| Command::Start { op })
            .unwrap();
        let Admission::Indeterminate(op) = result else {
            panic!("fsync failure must be indeterminate")
        };
        assert_eq!(rx.try_drain().first().unwrap().operation_id(), &op);
        assert_eq!(
            journal
                .submit_with_journal(&tx, key, payload(), |op| Command::Start { op })
                .unwrap(),
            Admission::Indeterminate(op.clone())
        );
        assert!(matches!(
            journal.submit_with_journal(&tx, "audio-intent:later", payload(), |op| {
                Command::Start { op }
            }),
            Err(JournalError::Poisoned)
        ));
        let restarted = AudioIntentJournal::open(&path).unwrap();
        assert_eq!(restarted.replay().unwrap().accepted_prefix.len(), 1);
        assert_eq!(
            restarted
                .submit_with_journal(&tx, key, payload(), |op| Command::Start { op })
                .unwrap(),
            Admission::Replay(op)
        );
        assert_eq!(rx.try_drain(), Vec::<Command>::new());
    }

    #[test]
    fn absent_record_after_failed_fsync_is_unknown_and_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, []).unwrap();
        let journal = AudioIntentJournal::with_sink(
            path.clone(),
            Box::new(FailAfterWrite {
                path: path.clone(),
                write: false,
            }),
        );
        let (tx, mut rx) = command_bus(2);
        journal.replay().unwrap();
        let result = journal
            .submit_with_journal(&tx, "audio-intent:missing", payload(), |op| {
                Command::Start { op }
            })
            .unwrap();
        assert!(matches!(result, Admission::Indeterminate(_)));
        assert!(
            !rx.try_drain().is_empty(),
            "engine may have applied the command"
        );
        // A missing record cannot prove not-applied or at-most-once on retry.
        std::fs::write(&path, []).unwrap();
        let restarted = AudioIntentJournal::open(&path).unwrap();
        assert_eq!(
            restarted.replay().unwrap().accepted_prefix,
            Vec::<ReplayedIntent>::new()
        );
        assert_eq!(restarted.operation_for("audio-intent:missing"), None);
    }

    #[test]
    fn bad_tail_imports_valid_prefix_but_refuses_new_admissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        let good = AudioIntentRecord::audio_route(
            "audio-intent:good".into(),
            OperationId::new(),
            RouteIdentity {
                destination: "bus-a".into(),
                source: "source-a".into(),
            },
            FieldIntent::Set(-6.0),
            FieldIntent::Set(false),
        );
        let mut bytes = encode_record(&good).unwrap();
        bytes.extend_from_slice(b"bad-record");
        std::fs::write(&path, bytes).unwrap();
        let journal = AudioIntentJournal::open(&path).unwrap();
        let report = journal.replay().unwrap();
        assert_eq!(report.accepted_prefix.len(), 1);
        assert_eq!(
            journal.operation_for("audio-intent:good"),
            Some(good.operation_id.clone())
        );
        assert_eq!(
            report.degraded,
            Some(DegradedTail {
                line: 2,
                offset: u64::try_from(encode_record(&good).unwrap().len()).unwrap()
            })
        );
        assert!(!report.admitted);
        assert!(matches!(
            journal.submit_with_journal(&command_bus(2).0, "audio-intent:new", payload(), |op| {
                Command::Start { op }
            }),
            Err(JournalError::Poisoned)
        ));
        let mut corrupted = encode_record(&good).unwrap();
        corrupted[12] ^= 1;
        std::fs::write(&path, corrupted).unwrap();
        let fresh = AudioIntentJournal::open(&path).unwrap();
        let report = fresh.replay().unwrap();
        assert_eq!(report.accepted_prefix, Vec::<ReplayedIntent>::new());
        assert_eq!(report.degraded, Some(DegradedTail { line: 1, offset: 0 }));
        assert_eq!(fresh.operation_for("audio-intent:good"), None);
    }

    #[test]
    fn concurrent_submit_and_replay_cannot_interleave_slow_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, []).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let journal = std::sync::Arc::new(AudioIntentJournal::with_sink(
            path.clone(),
            Box::new(BlockedSync {
                started: started_tx,
                release: release_rx,
                path: path.clone(),
            }),
        ));
        journal.replay().unwrap();
        let (tx, mut rx) = command_bus(3);
        let worker = {
            let journal = journal.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                journal.submit_with_journal(&tx, "audio-intent:first", payload(), |op| {
                    Command::Start { op }
                })
            })
        };
        started_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        // The command has reached the bus, but the first journal record has not
        // completed fsync. A second call cannot jump ahead and write a record.
        let submitted = rx.try_drain();
        assert_eq!(submitted.len(), 1);
        assert!(matches!(
            journal.submit_with_journal(&tx, "audio-intent:second", payload(), |op| {
                Command::Stop { op }
            }),
            Err(JournalError::Busy)
        ));
        assert!(matches!(journal.replay(), Err(JournalError::Busy)));
        assert_eq!(rx.try_drain(), Vec::<Command>::new());
        release_tx.send(()).unwrap();
        drop(started_rx); // only the first fsync is held; later writes proceed
        assert!(matches!(worker.join().unwrap(), Ok(Admission::Durable(_))));
        // The only admitted command's record is first. A later command can
        // submit only after its predecessor has fsynced.
        let second = journal
            .submit_with_journal(&tx, "audio-intent:second", payload(), |op| Command::Stop {
                op,
            })
            .unwrap();
        assert!(matches!(second, Admission::Durable(_)));
        let submitted_second = rx.try_drain();
        assert_eq!(submitted_second.len(), 1);
        let report = journal.replay().unwrap();
        assert_eq!(report.accepted_prefix.len(), 2);
        assert_eq!(
            report.accepted_prefix[0].0.operation_id,
            *submitted[0].operation_id()
        );
        assert_eq!(
            report.accepted_prefix[1].0.operation_id,
            *submitted_second[0].operation_id()
        );
        assert!(report.admitted);
    }

    #[test]
    fn degraded_journal_does_not_block_unrelated_command_bus() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, b"truncated").unwrap();
        let journal = AudioIntentJournal::open(&path).unwrap();
        let report = journal.replay().unwrap();
        assert_eq!(report.degraded, Some(DegradedTail { line: 1, offset: 0 }));
        assert!(!report.admitted);
        assert_eq!(journal.degraded_tail(), report.degraded);
        let (tx, mut rx) = command_bus(2);
        let op = OperationId::new();
        tx.try_submit(Command::Start { op: op.clone() }).unwrap();
        assert_eq!(
            rx.try_drain().first().unwrap().operation_id(),
            &op,
            "the unrelated engine command bus continues to drain"
        );
    }

    #[test]
    fn closure_operation_mismatch_is_rejected_before_submit() {
        let dir = tempfile::tempdir().unwrap();
        let journal = AudioIntentJournal::open(journal_path(dir.path())).unwrap();
        let (tx, mut rx) = command_bus(2);
        journal.replay().unwrap();
        let result = journal.submit_with_journal(&tx, "audio-intent:mismatch", payload(), |_| {
            Command::Start {
                op: OperationId::new(),
            }
        });
        assert!(matches!(result, Err(JournalError::OperationMismatch)));
        assert_eq!(rx.try_drain(), Vec::<Command>::new());
        assert_eq!(journal.operation_for("audio-intent:mismatch"), None);
    }
}
