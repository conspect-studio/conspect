//! Catalogue sync + offline import: fetch (conditional GET), validate, persist
//! atomically, rebuild the joined snapshot. All network access goes through the
//! [`Fetcher`] seam; the real reqwest implementation lives in `net` behind the
//! `catalogue` feature and is bound to the fixed upstream host.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use sha2::{Digest, Sha256};

use super::gazetteer::Gazetteer;
use super::index::{build_snapshot, Snapshot, SCHEMA_VERSION};
use super::model::{RawCatalogue, FILES};
use super::store::{now_rfc3339, FileMeta, Manifest, Store};

/// A boxed `Send` future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The fixed upstream API base (compile-time constant; no configuration).
pub const UPSTREAM_BASE: &str = "https://iptv-org.github.io/api/";

/// One conditional-GET outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// `304 Not Modified`.
    NotModified,
    /// `200` with a body.
    Body {
        /// Response bytes (already bounded by the fetcher).
        bytes: Vec<u8>,
        /// `ETag` header, if any.
        etag: Option<String>,
    },
}

/// Fetches one named upstream file with `If-None-Match`.
pub trait Fetcher: Send + Sync {
    /// Fetch `name` (`streams`, `channels`, …).
    fn fetch<'a>(
        &'a self,
        name: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchOutcome, String>>;
}

/// What a sync/import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Every file answered 304 and a snapshot already existed.
    Unchanged,
    /// A new snapshot was built.
    Updated {
        /// Files that changed.
        changed: Vec<String>,
    },
}

/// A built snapshot plus the manifest persisted for it.
pub struct Built {
    /// The joined snapshot.
    pub snapshot: Snapshot,
    /// The manifest written to disk.
    pub manifest: Manifest,
}

fn content_etag(files: &BTreeMap<String, FileMeta>) -> String {
    let mut h = Sha256::new();
    for (name, meta) in files {
        h.update(name.as_bytes());
        h.update(b"=");
        h.update(meta.etag.as_deref().unwrap_or("").as_bytes());
        h.update(b";");
    }
    super::store::hex_prefix(&h.finalize(), 8)
}

/// Merge `changed` files over the persisted ones, validate, persist atomically
/// (files first, manifest last) and join. Blocking (CPU + disk): run it on
/// `spawn_blocking`.
///
/// # Errors
///
/// A message; on error nothing in the manifest changed (a previous snapshot stays).
pub fn assemble(
    store: &Store,
    gazetteer: &Gazetteer,
    changed: &BTreeMap<String, (Vec<u8>, Option<String>)>,
    origin: &str,
) -> Result<Built, String> {
    let prior = store.load_manifest().unwrap_or_default();
    let mut raw = RawCatalogue::default();
    let mut files = prior.files.clone();
    for (name, required) in FILES {
        if let Some((bytes, etag)) = changed.get(*name) {
            raw.set_file(name, bytes)
                .map_err(|e| format!("{name}: {e}"))?;
            let etag = etag.clone().or_else(|| {
                let mut h = Sha256::new();
                h.update(bytes);
                Some(format!(
                    "import:{}",
                    super::store::hex_prefix(&h.finalize(), 8)
                ))
            });
            files.insert(
                (*name).to_owned(),
                FileMeta {
                    etag,
                    rows: u64::try_from(raw.len_of(name)).unwrap_or(0),
                    digest: Some(super::store::digest_of(bytes)),
                },
            );
        } else if store.has_file(name) {
            let bytes = store.read_file(name).map_err(|e| format!("{name}: {e}"))?;
            raw.set_file(name, &bytes)
                .map_err(|e| format!("{name}: {e}"))?;
        } else if *required {
            return Err(format!("required catalogue file {name:?} is missing"));
        }
    }
    if raw.streams.is_empty() {
        return Err("streams file has no rows; refusing to publish an empty catalogue".to_owned());
    }
    // Stage every changed file first (dropping `staged` on error removes the temp
    // files and leaves the previous generation untouched), then rename them all.
    let mut staged = Vec::with_capacity(changed.len());
    for (name, (bytes, _)) in changed {
        staged.push(
            store
                .stage_file(name, bytes)
                .map_err(|e| format!("persist {name}: {e}"))?,
        );
    }
    store
        .commit_staged(staged)
        .map_err(|e| format!("persist files: {e}"))?;
    let synced_at = now_rfc3339();
    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        synced_at: Some(synced_at.clone()),
        origin: Some(origin.to_owned()),
        files,
    };
    store
        .write_manifest(&manifest)
        .map_err(|e| format!("persist manifest: {e}"))?;
    let snapshot = build_snapshot(&raw, gazetteer, content_etag(&manifest.files), synced_at);
    Ok(Built { snapshot, manifest })
}

/// Rebuild the snapshot from what is on disk (boot path).
///
/// # Errors
///
/// A message when nothing usable is persisted.
pub fn load_persisted(store: &Store, gazetteer: &Gazetteer) -> Result<Built, String> {
    let manifest = store
        .load_manifest()
        .ok_or("no persisted catalogue manifest")?;
    let raw = store.load_raw()?;
    let synced_at = manifest.synced_at.clone().unwrap_or_else(now_rfc3339);
    let snapshot = build_snapshot(&raw, gazetteer, content_etag(&manifest.files), synced_at);
    Ok(Built { snapshot, manifest })
}

/// Conditional-GET every upstream file. All-or-nothing: any fetch error aborts and
/// returns it (the caller keeps the previous snapshot).
///
/// # Errors
///
/// The first fetch error.
pub async fn fetch_changed(
    fetcher: &dyn Fetcher,
    store: &Store,
) -> Result<BTreeMap<String, (Vec<u8>, Option<String>)>, String> {
    let prior = store.load_manifest().unwrap_or_default();
    let mut changed = BTreeMap::new();
    for (name, required) in FILES {
        // Only offer an ETag for a file we really hold, so a 304 can never leave a hole.
        // ...and only when the on-disk bytes still match the digest recorded with
        // that ETag, so a torn or mixed generation is refetched, never 304'd.
        let etag = if store.has_file(name) {
            prior.files.get(*name).and_then(|m| {
                let digest = m.digest.as_deref()?;
                let etag = m.etag.as_deref().filter(|e| !e.starts_with("import:"))?;
                (store.file_digest(name).as_deref() == Some(digest)).then_some(etag)
            })
        } else {
            None
        };
        let outcome = match fetcher.fetch(name, etag).await {
            Ok(f) => f,
            // An optional file (guides) that cannot be outcome must not fail the sync.
            Err(_) if !*required && !store.has_file(name) => continue,
            Err(e) => return Err(format!("{name}: {e}")),
        };
        match outcome {
            FetchOutcome::NotModified => {}
            FetchOutcome::Body { bytes, etag } => {
                changed.insert((*name).to_owned(), (bytes, etag));
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/iptv-org/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn generation(suffix: &str, tag: &str) -> BTreeMap<String, (Vec<u8>, Option<String>)> {
        ["streams", "channels", "feeds"]
            .iter()
            .map(|n| {
                let mut b = fixture(n);
                b.extend_from_slice(suffix.as_bytes());
                ((*n).to_owned(), (b, Some(tag.to_owned())))
            })
            .collect()
    }

    fn snapshot_of_dir(store: &Store) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(store.dir())
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn mid_write_failure_leaves_the_previous_generation_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let g = Gazetteer::default();
        assemble(&store, &g, &generation("\n", "\"v1\""), "o").unwrap();
        let before = snapshot_of_dir(&store);

        store.fail_stage_at(1);
        let err = assemble(&store, &g, &generation("\n\n", "\"v2\""), "o").err();
        assert!(err.is_some_and(|e| e.contains("injected")), "must fail");

        assert_eq!(
            snapshot_of_dir(&store),
            before,
            "files, manifest and temp files are exactly the previous generation"
        );
    }

    struct Recorder(Mutex<Vec<(String, Option<String>)>>);
    impl Fetcher for Recorder {
        fn fetch<'a>(
            &'a self,
            name: &'a str,
            etag: Option<&'a str>,
        ) -> BoxFuture<'a, Result<FetchOutcome, String>> {
            self.0
                .lock()
                .unwrap()
                .push((name.to_owned(), etag.map(str::to_owned)));
            Box::pin(async { Ok(FetchOutcome::NotModified) })
        }
    }

    #[tokio::test]
    async fn if_none_match_is_only_sent_when_the_disk_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assemble(
            &store,
            &Gazetteer::default(),
            &generation("\n", "\"v1\""),
            "o",
        )
        .unwrap();
        // New bytes landed on disk but the manifest still describes the old ones.
        store.write_file("channels", b"[]").unwrap();
        let rec = Recorder(Mutex::new(Vec::new()));
        fetch_changed(&rec, &store).await.unwrap();
        let calls = rec.0.lock().unwrap().clone();
        let etag = |n: &str| calls.iter().find(|c| c.0 == n).unwrap().1.clone();
        assert_eq!(etag("streams").as_deref(), Some("\"v1\""));
        assert_eq!(etag("feeds").as_deref(), Some("\"v1\""));
        assert_eq!(
            etag("channels"),
            None,
            "digest mismatch => unconditional GET"
        );
    }
}
