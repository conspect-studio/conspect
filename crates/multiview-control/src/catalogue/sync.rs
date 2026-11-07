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
    for (name, (bytes, _)) in changed {
        store
            .write_file(name, bytes)
            .map_err(|e| format!("persist {name}: {e}"))?;
    }
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
        let etag = if store.has_file(name) {
            prior
                .files
                .get(*name)
                .and_then(|m| m.etag.as_deref())
                .filter(|e| !e.starts_with("import:"))
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
