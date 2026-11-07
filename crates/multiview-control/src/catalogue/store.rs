//! Atomic on-disk snapshot persistence under
//! `<config-dir>/.multiview/catalogues/iptv-org/`.
//!
//! One gzip file per upstream file (`<name>.json.gz`) plus `manifest.json`
//! (schema version, per-file ETag/row-count, sync time, origin). Every write is
//! `tmp + rename` in the same directory, so a crash never leaves a half-written
//! file, and the manifest is written **last** — a partially applied sync is never
//! the one the next boot loads.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use super::model::{RawCatalogue, FILES};

/// Hard cap on one decompressed upstream file when loading from disk (a corrupt
/// or hostile file cannot balloon memory).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

const MANIFEST: &str = "manifest.json";

/// Hard cap on a downloaded/imported upstream file.
pub const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Current UTC time as RFC 3339 (seconds). The control crate's chrono has no clock
/// feature, so this reads `SystemTime` and lets chrono only format the instant.
#[must_use]
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    unix_rfc3339(secs)
}

/// Unix seconds as RFC 3339.
#[must_use]
pub fn unix_rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Lowercase hex of the first `n` bytes of `bytes`.
#[must_use]
pub fn hex_prefix(bytes: &[u8], n: usize) -> String {
    use std::fmt::Write as _;
    bytes.iter().take(n).fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Current unix seconds.
#[must_use]
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Per-file bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Upstream `ETag` (or `import:<hash>` for an offline import).
    #[serde(default)]
    pub etag: Option<String>,
    /// Rows parsed.
    #[serde(default)]
    pub rows: u64,
}

/// `manifest.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Snapshot schema version.
    #[serde(default)]
    pub schema: u32,
    /// RFC 3339 time of the last successful sync/import.
    #[serde(default)]
    pub synced_at: Option<String>,
    /// `sync` or `import`.
    #[serde(default)]
    pub origin: Option<String>,
    /// Per-file metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

/// A catalogue directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// A store rooted at `dir` (created on first write).
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json.gz"))
    }

    /// Whether `name` is persisted.
    #[must_use]
    pub fn has_file(&self, name: &str) -> bool {
        self.file_path(name).is_file()
    }

    /// Load the manifest (`None` when absent or unreadable).
    #[must_use]
    pub fn load_manifest(&self) -> Option<Manifest> {
        let bytes = std::fs::read(self.dir.join(MANIFEST)).ok()?;
        let m: Manifest = serde_json::from_slice(&bytes).ok()?;
        (m.schema == super::index::SCHEMA_VERSION).then_some(m)
    }

    fn atomic_write(&self, final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(final_path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Persist one upstream file (gzip) atomically.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn write_file(&self, name: &str, json: &[u8]) -> std::io::Result<()> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(json)?;
        let gz = enc.finish()?;
        self.atomic_write(&self.file_path(name), &gz)
    }

    /// Persist the manifest atomically (call **after** the files).
    ///
    /// # Errors
    ///
    /// I/O or serialization errors.
    pub fn write_manifest(&self, manifest: &Manifest) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(manifest).map_err(std::io::Error::other)?;
        self.atomic_write(&self.dir.join(MANIFEST), &bytes)
    }

    /// Read one persisted file's JSON bytes (bounded decompression).
    ///
    /// # Errors
    ///
    /// I/O errors, or `InvalidData` when the file inflates past [`MAX_FILE_BYTES`].
    pub fn read_file(&self, name: &str) -> std::io::Result<Vec<u8>> {
        let gz = std::fs::read(self.file_path(name))?;
        let mut out = Vec::new();
        GzDecoder::new(gz.as_slice())
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut out)?;
        if u64::try_from(out.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "persisted catalogue file exceeds the size cap",
            ));
        }
        Ok(out)
    }

    /// Load every persisted file into a [`RawCatalogue`].
    ///
    /// # Errors
    ///
    /// A message naming the first missing-required or unparseable file.
    pub fn load_raw(&self) -> Result<RawCatalogue, String> {
        let mut raw = RawCatalogue::default();
        for (name, required) in FILES {
            if !self.has_file(name) {
                if *required {
                    return Err(format!("required catalogue file {name:?} is not persisted"));
                }
                continue;
            }
            let bytes = self.read_file(name).map_err(|e| format!("{name}: {e}"))?;
            raw.set_file(name, &bytes)
                .map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(raw)
    }
}

/// Gzip `bytes` (for the cached index body).
///
/// # Errors
///
/// I/O errors from the encoder.
pub fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(bytes)?;
    enc.finish()
}

/// Gunzip when `bytes` carry the gzip magic, else return them unchanged; bounded.
///
/// # Errors
///
/// I/O errors, or `InvalidData` past `limit`.
pub fn maybe_gunzip(bytes: Vec<u8>, limit: u64) -> std::io::Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        GzDecoder::new(bytes.as_slice())
            .take(limit + 1)
            .read_to_end(&mut out)?;
        if u64::try_from(out.len()).unwrap_or(u64::MAX) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "decompressed body exceeds the size cap",
            ));
        }
        Ok(out)
    } else {
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_and_manifest_round_trip_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("cat"));
        assert!(store.load_manifest().is_none());
        store
            .write_file("streams", b"[{\"url\":\"https://a/b.m3u8\"}]")
            .unwrap();
        let mut m = Manifest {
            schema: super::super::index::SCHEMA_VERSION,
            ..Manifest::default()
        };
        m.files.insert(
            "streams".into(),
            FileMeta {
                etag: Some("\"x\"".into()),
                rows: 1,
            },
        );
        store.write_manifest(&m).unwrap();
        assert_eq!(store.load_manifest().unwrap(), m);
        assert_eq!(
            store.read_file("streams").unwrap(),
            b"[{\"url\":\"https://a/b.m3u8\"}]"
        );
        // No stray temp files remain.
        let names: Vec<_> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn decompression_bomb_is_refused() {
        let big = vec![b' '; 1024];
        let gz = gzip(&big).unwrap();
        assert!(maybe_gunzip(gz, 100).is_err());
    }
}
