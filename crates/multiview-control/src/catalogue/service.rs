//! The catalogue service: owns the current [`Snapshot`], the cached index body,
//! the sync status and the refresh/import entry points. Control plane only —
//! nothing here is reachable from the data plane (invariants #1/#10).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;

use super::gazetteer::Gazetteer;
use super::index::{Flag, IndexRow, Places, Snapshot};
use super::store::{self, Store};
use super::sync::{self, Fetcher, SyncOutcome};

/// Sync lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Kill switch on (`enabled = false`).
    Disabled,
    /// Nothing synced yet and no attempt running.
    Empty,
    /// A sync/import is running.
    Syncing,
    /// A snapshot is loaded and the last attempt succeeded (wire token `idle`,
    /// the web contract's name for "nothing running").
    #[serde(rename = "idle")]
    Ready,
    /// The last attempt failed (a previous snapshot, if any, is still served).
    Error,
}

/// `GET .../status` body.
#[derive(Debug, Clone, Serialize)]
pub struct StatusBody {
    /// Whether the catalogue is enabled.
    pub enabled: bool,
    /// Lifecycle state.
    pub state: SyncState,
    /// Time of the last successful sync/import (RFC 3339).
    pub last_sync_at: Option<String>,
    /// Same instant as `last_sync_at` under the web contract's name.
    pub synced_at: Option<String>,
    /// The last attempt's error as a list (empty when it succeeded).
    pub errors: Vec<String>,
    /// Time of the last attempt (RFC 3339).
    pub last_attempt_at: Option<String>,
    /// Last attempt's error, if it failed.
    pub last_error: Option<String>,
    /// `sync` or `import`.
    pub origin: Option<String>,
    /// Content identity of the loaded snapshot (the index `ETag` base).
    pub etag: Option<String>,
    /// Sync interval in seconds.
    pub interval_secs: u64,
    /// Row counts per upstream file / index.
    pub counts: BTreeMap<String, u64>,
    /// Per-file upstream `ETag` + row count.
    pub files: BTreeMap<String, store::FileMeta>,
    /// Link-status tally over the caller's visible sources (filled by the route).
    pub links: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
struct Inner {
    snapshot: Option<Arc<Snapshot>>,
    index_gz: Option<Arc<Vec<u8>>>,
    manifest: Option<store::Manifest>,
    last_attempt_at: Option<String>,
    last_error: Option<String>,
    syncing: bool,
}

/// Changed upstream files: name → (JSON bytes, `ETag`).
pub type ChangedFiles = BTreeMap<String, (Vec<u8>, Option<String>)>;

/// The catalogue handle shared through `AppState`.
pub struct Catalogue {
    enabled: bool,
    interval: Duration,
    store: Store,
    gazetteer: Gazetteer,
    fetcher: Arc<dyn Fetcher>,
    inner: RwLock<Inner>,
    gate: tokio::sync::Mutex<()>,
    /// Link tracker state (slice D).
    pub links: super::links::LinkTracker,
}

impl std::fmt::Debug for Catalogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalogue")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// `<config-dir>/.multiview/catalogues/iptv-org` for a boot config path.
#[must_use]
pub fn catalogue_dir(boot_path: &std::path::Path) -> PathBuf {
    crate::boot_model::state_dir_for(boot_path)
        .join("catalogues")
        .join("iptv-org")
}

fn now() -> String {
    store::now_rfc3339()
}

impl Catalogue {
    /// Build a catalogue rooted at `dir`, loading any persisted snapshot.
    #[must_use]
    pub fn new(
        dir: PathBuf,
        cfg: &multiview_config::IptvOrgConfig,
        fetcher: Arc<dyn Fetcher>,
    ) -> Arc<Self> {
        let store = Store::new(dir);
        let gazetteer = Gazetteer::embedded();
        let me = Arc::new(Self {
            enabled: cfg.enabled,
            interval: cfg.sync_interval(),
            store,
            gazetteer,
            fetcher,
            inner: RwLock::new(Inner::default()),
            gate: tokio::sync::Mutex::new(()),
            links: super::links::LinkTracker::new(),
        });
        if me.enabled {
            match sync::load_persisted(&me.store, &me.gazetteer) {
                Ok(built) => me.install(built, None),
                Err(reason) => tracing::debug!(%reason, "no persisted iptv-org catalogue"),
            }
        }
        me
    }

    /// Whether the kill switch is off.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Configured sync interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn install(&self, built: sync::Built, error: Option<String>) {
        let index_gz = index_body(&built.snapshot, None).and_then(|b| store::gzip(&b).ok());
        let mut g = self.write();
        g.snapshot = Some(Arc::new(built.snapshot));
        g.index_gz = index_gz.map(Arc::new);
        g.manifest = Some(built.manifest);
        g.last_error = error;
    }

    /// The current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.read().snapshot.clone()
    }

    /// The cached gzip'd index body and its `ETag`.
    #[must_use]
    pub fn cached_index(&self) -> Option<(Arc<Vec<u8>>, String)> {
        let g = self.read();
        let gz = g.index_gz.clone()?;
        let etag = g.snapshot.as_ref()?.etag.clone();
        Some((gz, etag))
    }

    /// Places with counts.
    #[must_use]
    pub fn places(&self) -> Option<(Places, String)> {
        let s = self.snapshot()?;
        Some((s.places.clone(), s.etag.clone()))
    }

    /// Resolve a logo key to its upstream URL.
    #[must_use]
    pub fn logo_url(&self, key: &str) -> Option<String> {
        self.snapshot()?.logos.get(key).cloned()
    }

    /// The status body (links tally left empty for the route to fill).
    #[must_use]
    pub fn status(&self) -> StatusBody {
        let g = self.read();
        let state = if !self.enabled {
            SyncState::Disabled
        } else if g.syncing {
            SyncState::Syncing
        } else if g.last_error.is_some() {
            SyncState::Error
        } else if g.snapshot.is_some() {
            SyncState::Ready
        } else {
            SyncState::Empty
        };
        let mut counts = BTreeMap::new();
        let mut files = BTreeMap::new();
        if let Some(m) = &g.manifest {
            for (name, meta) in &m.files {
                counts.insert(name.clone(), meta.rows);
            }
            files = m.files.clone();
        }
        if let Some(s) = &g.snapshot {
            counts.insert("index".to_owned(), u64::try_from(s.rows.len()).unwrap_or(0));
        }
        StatusBody {
            enabled: self.enabled,
            state,
            last_sync_at: g.manifest.as_ref().and_then(|m| m.synced_at.clone()),
            synced_at: g.manifest.as_ref().and_then(|m| m.synced_at.clone()),
            errors: g.last_error.clone().into_iter().collect(),
            last_attempt_at: g.last_attempt_at.clone(),
            last_error: g.last_error.clone(),
            origin: g.manifest.as_ref().and_then(|m| m.origin.clone()),
            etag: g.snapshot.as_ref().map(|s| s.etag.clone()),
            interval_secs: self.interval.as_secs(),
            counts,
            files,
            links: BTreeMap::new(),
        }
    }

    fn begin(&self) {
        let mut g = self.write();
        g.syncing = true;
        g.last_attempt_at = Some(now());
    }

    fn fail(&self, reason: String) {
        let mut g = self.write();
        g.syncing = false;
        g.last_error = Some(reason);
    }

    fn done(&self) {
        self.write().syncing = false;
    }

    /// Sync from the upstream host. A failed run keeps the previous snapshot.
    ///
    /// # Errors
    ///
    /// A message (also recorded in `status.last_error`).
    pub async fn refresh(self: &Arc<Self>) -> Result<SyncOutcome, String> {
        if !self.enabled {
            return Err("the iptv-org catalogue is disabled".to_owned());
        }
        let _gate = self.gate.lock().await;
        self.begin();
        let changed = match sync::fetch_changed(self.fetcher.as_ref(), &self.store).await {
            Ok(c) => c,
            Err(e) => {
                self.fail(e.clone());
                return Err(e);
            }
        };
        self.apply(changed, "sync").await
    }

    /// Import an offline bundle: `{ "streams": [...], "channels": [...], ... }`
    /// (optionally gzip'd). Replaces the named files; the rest stay as persisted.
    ///
    /// # Errors
    ///
    /// A message (also recorded in `status.last_error`).
    pub async fn import_bundle(self: &Arc<Self>, body: Vec<u8>) -> Result<SyncOutcome, String> {
        if !self.enabled {
            return Err("the iptv-org catalogue is disabled".to_owned());
        }
        let _gate = self.gate.lock().await;
        self.begin();
        let parsed = tokio::task::spawn_blocking(move || -> Result<ChangedFiles, String> {
            let json = store::maybe_gunzip(body, 4 * store::MAX_DOWNLOAD_BYTES)
                .map_err(|e| e.to_string())?;
            let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&json)
                .map_err(|e| format!("bundle is not a JSON object: {e}"))?;
            let mut out = BTreeMap::new();
            for (name, value) in obj {
                if !super::model::FILES.iter().any(|(n, _)| *n == name) {
                    return Err(format!("unknown bundle member {name:?}"));
                }
                let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
                out.insert(name, (bytes, None));
            }
            if out.is_empty() {
                return Err("bundle has no members".to_owned());
            }
            Ok(out)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        match parsed {
            Ok(changed) => self.apply(changed, "import").await,
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }

    async fn apply(
        self: &Arc<Self>,
        changed: BTreeMap<String, (Vec<u8>, Option<String>)>,
        origin: &'static str,
    ) -> Result<SyncOutcome, String> {
        if changed.is_empty() && self.snapshot().is_some() {
            self.done();
            // A clean no-op clears a stale error.
            self.write().last_error = None;
            return Ok(SyncOutcome::Unchanged);
        }
        let names: Vec<String> = changed.keys().cloned().collect();
        let me = Arc::clone(self);
        let built = tokio::task::spawn_blocking(move || {
            sync::assemble(&me.store, &me.gazetteer, &changed, origin)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        match built {
            Ok(b) => {
                let me = Arc::clone(self);
                tokio::task::spawn_blocking(move || me.install(b, None))
                    .await
                    .map_err(|e| e.to_string())?;
                self.done();
                Ok(SyncOutcome::Updated { changed: names })
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }
}

struct IndexView<'a> {
    rows: &'a [IndexRow],
    added: Option<&'a HashSet<String>>,
}

impl Serialize for IndexView<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.rows.iter().map(|r| {
            let mut row = r.clone();
            if self.added.is_some_and(|a| a.contains(&r.k)) {
                row.flags.push(Flag::AlreadyAdded);
            }
            row
        }))
    }
}

/// Serialize the index envelope
/// `{schema, generated_at, categories[{id,name}], languages[{code,name}], streams[]}`.
/// `added` marks rows `already_added` (per-caller); `None` produces the shared
/// cacheable body.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn index_body(snapshot: &Snapshot, added: Option<&HashSet<String>>) -> Option<Vec<u8>> {
    #[derive(Serialize)]
    struct Cat<'a> {
        id: &'a str,
        name: &'a str,
    }
    #[derive(Serialize)]
    struct Lang<'a> {
        code: &'a str,
        name: &'a str,
    }
    #[derive(Serialize)]
    struct Envelope<'a> {
        schema: u32,
        generated_at: &'a str,
        categories: Vec<Cat<'a>>,
        languages: Vec<Lang<'a>>,
        streams: IndexView<'a>,
    }
    serde_json::to_vec(&Envelope {
        schema: snapshot.schema,
        generated_at: &snapshot.synced_at,
        categories: snapshot
            .categories
            .iter()
            .map(|(id, name)| Cat { id, name })
            .collect(),
        languages: snapshot
            .languages
            .iter()
            .map(|(code, name)| Lang { code, name })
            .collect(),
        streams: IndexView {
            rows: &snapshot.rows,
            added,
        },
    })
    .ok()
}

/// Group `rows` by `channel@feed` (alternatives share a group) — used by the link
/// tracker and tests.
#[must_use]
pub fn group_by_feed(rows: &[IndexRow]) -> HashMap<(String, String), Vec<&IndexRow>> {
    let mut m: HashMap<(String, String), Vec<&IndexRow>> = HashMap::new();
    for r in rows {
        if let Some(ch) = &r.ch {
            m.entry((ch.clone(), r.feed.clone().unwrap_or_default()))
                .or_default()
                .push(r);
        }
    }
    m
}
