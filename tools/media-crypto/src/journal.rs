//! Crash-safe, single-host counter reservations. Stores no secret key material.
use crate::{CounterReservation, Error, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"NR02";
const RECORD: usize = 52;
const MAX_RECORDS: usize = 4_096;

/// Persist a counter reservation before the caller can encrypt. A separate
/// stable lock inode serializes rename transactions across processes. Mount on a
/// filesystem whose fsync/rename/directory-fsync durability is reliable.
pub struct DurableCounterJournal {
    state: PathBuf,
    lock: File,
    directory: crate::private_state::PrivateDir,
}
impl DurableCounterJournal {
    /// Open a dedicated owner-controlled state path. Never place it on tmpfs.
    /// Do not share a state path with another untrusted process or delete it
    /// while the publisher holds an epoch key.
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().ok_or(Error::Invalid)?;
        let directory = crate::private_state::PrivateDir::open(parent)?;
        let mut name = path.file_name().ok_or(Error::Invalid)?.to_os_string();
        name.push(".lock");
        let lock = directory.file(Path::new(&name), true, false)?;
        Ok(Self {
            state: path.to_path_buf(),
            lock,
            directory,
        })
    }
}
#[derive(Clone, Copy)]
struct Entry {
    version: [u8; 16],
    publisher: [u8; 16],
    epoch: u64,
    generation: u32,
    next: u64,
}
fn decode(bytes: &[u8]) -> Result<Vec<Entry>> {
    if bytes.len() < 4
        || &bytes[..4] != MAGIC
        || !(bytes.len() - 4).is_multiple_of(RECORD)
        || (bytes.len() - 4) / RECORD > MAX_RECORDS
    {
        return Err(Error::Invalid);
    }
    let mut entries = Vec::new();
    let (chunks, remainder) = bytes[4..].as_chunks::<RECORD>();
    if !remainder.is_empty() {
        return Err(Error::Invalid);
    }
    for chunk in chunks {
        let entry = Entry {
            version: chunk[..16].try_into().map_err(|_| Error::Invalid)?,
            publisher: chunk[16..32].try_into().map_err(|_| Error::Invalid)?,
            epoch: u64::from_be_bytes(chunk[32..40].try_into().map_err(|_| Error::Invalid)?),
            generation: u32::from_be_bytes(chunk[40..44].try_into().map_err(|_| Error::Invalid)?),
            next: u64::from_be_bytes(chunk[44..52].try_into().map_err(|_| Error::Invalid)?),
        };
        if entry.next == 0
            || entries.iter().any(|e: &Entry| {
                e.version == entry.version
                    && e.publisher == entry.publisher
                    && e.epoch == entry.epoch
                    && e.generation == entry.generation
            })
        {
            return Err(Error::Invalid);
        }
        entries.push(entry);
    }
    Ok(entries)
}
impl CounterReservation for DurableCounterJournal {
    fn reserve(
        &mut self,
        version: [u8; 16],
        epoch: u64,
        publisher: [u8; 16],
        generation: u32,
    ) -> Result<u64> {
        self.lock.lock().map_err(|_| Error::Crypto)?;
        let result = self.reserve_locked(version, epoch, publisher, generation);
        let unlock = self.lock.unlock().map_err(|_| Error::Crypto);
        unlock?;
        result
    }
}
impl DurableCounterJournal {
    fn reserve_locked(
        &mut self,
        version: [u8; 16],
        epoch: u64,
        publisher: [u8; 16],
        generation: u32,
    ) -> Result<u64> {
        let bytes = self
            .directory
            .read(&self.state, (4 + RECORD * MAX_RECORDS) as u64)?
            .unwrap_or_else(|| MAGIC.to_vec());
        let mut entries = decode(&bytes)?;
        if entries.iter().any(|e| {
            e.version == version
                && e.publisher == publisher
                && (generation < e.generation || epoch < e.epoch)
        }) {
            return Err(Error::Replay);
        }
        let counter = match entries.iter_mut().find(|e| {
            e.version == version
                && e.publisher == publisher
                && e.epoch == epoch
                && e.generation == generation
        }) {
            Some(entry) => {
                let counter = entry.next;
                entry.next = counter.checked_add(1).ok_or(Error::Capacity)?;
                counter
            }
            None => {
                if entries.len() >= MAX_RECORDS {
                    return Err(Error::Capacity);
                }
                entries.push(Entry {
                    version,
                    publisher,
                    epoch,
                    generation,
                    next: 1,
                });
                0
            }
        };
        let mut out = Vec::with_capacity(4 + entries.len() * RECORD);
        out.extend(MAGIC);
        for entry in entries {
            out.extend(entry.version);
            out.extend(entry.publisher);
            out.extend(entry.epoch.to_be_bytes());
            out.extend(entry.generation.to_be_bytes());
            out.extend(entry.next.to_be_bytes());
        }
        // A process crash may leave an unused .next file. create_new refuses
        // to overwrite it, so reserve fails closed until an operator checks it.
        let next_path = self.state.with_extension("reservation-next");
        self.directory
            .replace(&next_path, &self.state, &out)
            .map_err(|_| Error::Crypto)?;
        Ok(counter)
    }
}
