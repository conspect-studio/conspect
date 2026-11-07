//! Persistent, bounded server-side logo cache: the browser never loads
//! third-party images.
//!
//! Logos are fetched through the shared screened `iptv::fetch` transport (image
//! content types only, ≤ 512 KiB each) and kept on disk under
//! `<config-dir>/.multiview/catalogues/iptv-org/logos/`:
//!
//! * key `sha256(url)`; `<key>.bin` holds the bytes, `<key>.json` a small
//!   sidecar (content type, `fetched_at`, upstream `ETag`/`Last-Modified`,
//!   status, failure count and the earliest retry time). Writes are atomic
//!   (temp file + rename; the sidecar is the commit marker).
//! * bounded at 256 MiB and 20,000 files; least-recently-used entries are
//!   evicted (access tracked in memory, recovered from sidecar mtimes at start).
//! * a small in-memory hot set serves repeat requests without touching disk.
//! * fresh for 30 days; afterwards the cached copy keeps being served while a
//!   background conditional request revalidates it. An upstream failure never
//!   evicts a good copy.
//! * failures are negative-cached in the sidecar: 404/410/DNS/other permanent
//!   refusals for 7 days; 429/5xx/timeouts 15 minutes, doubling per consecutive
//!   failure up to 24 h.
//! * politeness: at most 2 concurrent fetches per host and 8 globally; a caller
//!   that waits more than 5 s for a slot gets [`Logo::Busy`].
//!
//! Control plane only; nothing here touches the output clock.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use multiview_config::device::net_guard::DialPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::iptv::fetch::{FetchOutcome, FetchRequest, Fetcher};
use crate::state::AppState;

const MAX_LOGO_BYTES: usize = 512 * 1024;
const MAX_LOGO_BYTES_U64: u64 = 512 * 1024;
/// Total on-disk bound (bytes, sidecars included).
pub const MAX_DISK_BYTES: u64 = 256 * 1024 * 1024;
/// Total on-disk bound (files, sidecars included).
pub const MAX_DISK_FILES: u64 = 20_000;
/// How long a cached copy is served without revalidation.
pub const FRESH_SECS: u64 = 30 * 24 * 3600;
/// Negative-cache window for permanent failures (404/410/DNS/…).
pub const PERMANENT_SECS: u64 = 7 * 24 * 3600;
/// First transient-failure window; doubles per consecutive failure.
pub const TRANSIENT_BASE_SECS: u64 = 15 * 60;
/// Transient-failure window ceiling.
pub const TRANSIENT_MAX_SECS: u64 = 24 * 3600;
const PER_HOST: usize = 2;
const GLOBAL: usize = 8;
const SLOT_WAIT: Duration = Duration::from_secs(5);
const HOT_ENTRIES: usize = 128;
const HOT_BYTES: usize = 16 * 1024 * 1024;
/// Descriptive `User-Agent` lives in `iptv::fetch` (names the software and repo).
const ALLOWED: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/svg+xml",
];

/// A cached logo image.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    /// The image bytes.
    pub bytes: Vec<u8>,
    /// Its (allow-listed) content type.
    pub content_type: String,
    /// Strong `ETag` (quoted) derived from the bytes.
    pub etag: String,
}

/// The answer for one logo request.
#[derive(Debug, Clone)]
pub enum Logo {
    /// The image.
    Image(Arc<Image>),
    /// Known missing (negative cached, oversized, wrong type): 204, cacheable.
    Missing,
    /// No fetch slot became free in time: 204, not cacheable; retry later.
    Busy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    content_type: Option<String>,
    fetched_at: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    /// Last upstream status (200 ok, HTTP code, 0 for transport failures).
    status: u16,
    has_body: bool,
    /// Strong `ETag` we serve for the stored bytes.
    body_etag: Option<String>,
    fail_count: u32,
    retry_after: u64,
    last_error: Option<String>,
}

struct Entry {
    meta: Meta,
    image: Option<Arc<Image>>,
}

struct Idx {
    tick: u64,
    bytes: u64,
    files: u64,
}

#[derive(Default)]
struct State {
    index: HashMap<String, Idx>,
    order: BTreeMap<u64, String>,
    total_bytes: u64,
    total_files: u64,
    next_tick: u64,
    hot: HashMap<String, Arc<Entry>>,
    hot_order: VecDeque<String>,
    hot_bytes: usize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The persistent logo cache over one directory.
pub struct LogoCache {
    dir: PathBuf,
    fetcher: Arc<dyn Fetcher>,
    policy: Arc<DialPolicy>,
    now: Clock,
    slot_wait: Duration,
    max_bytes: u64,
    max_files: u64,
    global: Arc<tokio::sync::Semaphore>,
    hosts: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    state: Mutex<State>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    revalidating: Mutex<HashSet<String>>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn key_hash(url: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(url.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn body_etag(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = Sha256::digest(bytes);
    let mut s = String::from("\"");
    for b in d.iter().take(16) {
        let _ = write!(s, "{b:02x}");
    }
    s.push('"');
    s
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailKind {
    Permanent,
    Transient,
}

/// Classify a fetcher error string into a negative-cache class and the HTTP
/// status it carried (0 when none).
fn classify(msg: &str) -> (FailKind, u16) {
    let lower = msg.to_ascii_lowercase();
    if let Some(at) = msg.find("HTTP ") {
        let code: u16 = msg
            .get(at + 5..at + 8)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let kind = match code {
            408 | 429 | 500..=599 => FailKind::Transient,
            400..=499 => FailKind::Permanent,
            _ => FailKind::Transient,
        };
        return (kind, code);
    }
    let permanent = [
        "dns error",
        "failed to lookup",
        "name or service not known",
        "no address associated",
        "nodename nor servname",
        "failed to resolve",
        "body exceeds",
        "only http",
        "has no host",
        "credentials",
        "too long",
    ];
    if permanent.iter().any(|p| lower.contains(p)) {
        return (FailKind::Permanent, 0);
    }
    (FailKind::Transient, 0)
}

fn transient_delay(prior_failures: u32) -> u64 {
    TRANSIENT_BASE_SECS
        .saturating_mul(1u64 << prior_failures.min(16))
        .min(TRANSIENT_MAX_SECS)
}

struct Permits {
    _host: tokio::sync::OwnedSemaphorePermit,
    _global: tokio::sync::OwnedSemaphorePermit,
}

impl LogoCache {
    /// Open (and scan) the cache at `dir` over the given transport.
    #[must_use]
    pub fn new(dir: PathBuf, fetcher: Arc<dyn Fetcher>, policy: Arc<DialPolicy>) -> Arc<Self> {
        Self::with_options(dir, fetcher, policy, Arc::new(unix_now))
    }

    /// As [`Self::new`] with an injected clock (unix seconds).
    #[must_use]
    pub fn with_options(
        dir: PathBuf,
        fetcher: Arc<dyn Fetcher>,
        policy: Arc<DialPolicy>,
        now: Clock,
    ) -> Arc<Self> {
        let cache = Self {
            dir,
            fetcher,
            policy,
            now,
            slot_wait: SLOT_WAIT,
            max_bytes: MAX_DISK_BYTES,
            max_files: MAX_DISK_FILES,
            global: Arc::new(tokio::sync::Semaphore::new(GLOBAL)),
            hosts: Mutex::new(HashMap::new()),
            state: Mutex::new(State::default()),
            flights: Mutex::new(HashMap::new()),
            revalidating: Mutex::new(HashSet::new()),
        };
        cache.scan();
        Arc::new(cache)
    }

    /// Test/ops seam: override the slot wait and disk bounds, then re-enforce them.
    #[must_use]
    pub fn with_limits(
        mut self: Arc<Self>,
        slot_wait: Duration,
        max_bytes: u64,
        max_files: u64,
    ) -> Arc<Self> {
        if let Some(me) = Arc::get_mut(&mut self) {
            me.slot_wait = slot_wait;
            me.max_bytes = max_bytes;
            me.max_files = max_files;
            let victims = me.evict_locked_for(None);
            remove_files(&me.dir, &victims);
        }
        self
    }

    /// Rebuild the in-memory index from the directory; LRU order from sidecar mtimes.
    fn scan(&self) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut found: Vec<(std::time::SystemTime, String, u64, u64)> = Vec::new();
        let mut bins: HashSet<String> = HashSet::new();
        let mut jsons: HashSet<String> = HashSet::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
            } else if let Some(h) = name.strip_suffix(".bin") {
                bins.insert(h.to_owned());
            } else if let Some(h) = name.strip_suffix(".json") {
                jsons.insert(h.to_owned());
            }
        }
        for h in bins.difference(&jsons) {
            let _ = std::fs::remove_file(self.dir.join(format!("{h}.bin")));
        }
        for h in &jsons {
            let Ok(md) = std::fs::metadata(self.dir.join(format!("{h}.json"))) else {
                continue;
            };
            let mut bytes = md.len();
            let mut files = 1;
            if bins.contains(h) {
                bytes +=
                    std::fs::metadata(self.dir.join(format!("{h}.bin"))).map_or(0, |m| m.len());
                files += 1;
            }
            found.push((
                md.modified().unwrap_or(std::time::UNIX_EPOCH),
                h.clone(),
                bytes,
                files,
            ));
        }
        found.sort();
        {
            let mut st = lock(&self.state);
            for (_, h, bytes, files) in found {
                let tick = st.next_tick;
                st.next_tick += 1;
                st.order.insert(tick, h.clone());
                st.total_bytes += bytes;
                st.total_files += files;
                st.index.insert(h, Idx { tick, bytes, files });
            }
        }
        let victims = self.evict_locked_for(None);
        remove_files(&self.dir, &victims);
    }

    /// Evict LRU entries (never `keep`) until both bounds hold; returns the
    /// victims whose files the caller removes.
    fn evict_locked_for(&self, keep: Option<&str>) -> Vec<String> {
        let mut st = lock(&self.state);
        let mut victims = Vec::new();
        while st.total_bytes > self.max_bytes || st.total_files > self.max_files {
            let Some((tick, h)) = st
                .order
                .iter()
                .find(|(_, h)| Some(h.as_str()) != keep)
                .map(|(t, h)| (*t, h.clone()))
            else {
                break;
            };
            st.order.remove(&tick);
            if let Some(i) = st.index.remove(&h) {
                st.total_bytes = st.total_bytes.saturating_sub(i.bytes);
                st.total_files = st.total_files.saturating_sub(i.files);
            }
            Self::hot_remove(&mut st, &h);
            victims.push(h);
        }
        victims
    }

    fn hot_remove(st: &mut State, h: &str) {
        if let Some(e) = st.hot.remove(h) {
            st.hot_bytes = st
                .hot_bytes
                .saturating_sub(e.image.as_ref().map_or(0, |i| i.bytes.len()));
            st.hot_order.retain(|k| k != h);
        }
    }

    fn hot_insert(st: &mut State, h: &str, entry: &Arc<Entry>) {
        Self::hot_remove(st, h);
        st.hot_bytes += entry.image.as_ref().map_or(0, |i| i.bytes.len());
        st.hot.insert(h.to_owned(), Arc::clone(entry));
        st.hot_order.push_back(h.to_owned());
        while st.hot_order.len() > HOT_ENTRIES || st.hot_bytes > HOT_BYTES {
            let Some(old) = st.hot_order.pop_front() else {
                break;
            };
            if let Some(e) = st.hot.remove(&old) {
                st.hot_bytes = st
                    .hot_bytes
                    .saturating_sub(e.image.as_ref().map_or(0, |i| i.bytes.len()));
            }
        }
    }

    fn touch(st: &mut State, h: &str) {
        let tick = st.next_tick;
        if let Some(i) = st.index.get_mut(h) {
            let old = std::mem::replace(&mut i.tick, tick);
            st.next_tick += 1;
            st.order.remove(&old);
            st.order.insert(tick, h.to_owned());
        }
    }

    /// Hot set, then disk. `None` when nothing is cached for `h`.
    async fn lookup(&self, h: &str) -> Option<Arc<Entry>> {
        {
            let mut st = lock(&self.state);
            if let Some(e) = st.hot.get(h).cloned() {
                Self::touch(&mut st, h);
                return Some(e);
            }
            if !st.index.contains_key(h) {
                return None;
            }
        }
        let dir = self.dir.clone();
        let key = h.to_owned();
        let loaded = tokio::task::spawn_blocking(move || read_entry(&dir, &key))
            .await
            .ok()
            .flatten();
        let mut st = lock(&self.state);
        if let Some(entry) = loaded {
            let entry = Arc::new(entry);
            Self::hot_insert(&mut st, h, &entry);
            Self::touch(&mut st, h);
            Some(entry)
        } else {
            // Unreadable or corrupt: forget it (files are dropped on next write).
            if let Some(i) = st.index.remove(h) {
                st.order.remove(&i.tick);
                st.total_bytes = st.total_bytes.saturating_sub(i.bytes);
                st.total_files = st.total_files.saturating_sub(i.files);
            }
            drop(st);
            remove_files(&self.dir, &[h.to_owned()]);
            None
        }
    }

    /// Serve (or fetch) the logo at `url`.
    pub async fn get(self: &Arc<Self>, url: &str) -> Logo {
        let h = key_hash(url);
        if let Some(e) = self.lookup(&h).await {
            let now = (self.now)();
            if let Some(img) = &e.image {
                if now.saturating_sub(e.meta.fetched_at) >= FRESH_SECS && now >= e.meta.retry_after
                {
                    self.spawn_revalidate(&h, url, &e);
                }
                return Logo::Image(Arc::clone(img));
            }
            if now < e.meta.retry_after {
                return Logo::Missing;
            }
        }
        // Single flight per key so a page of identical logos fetches once.
        let flight = Arc::clone(
            lock(&self.flights)
                .entry(h.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        );
        let guard = flight.lock().await;
        let prior = self.lookup(&h).await;
        let now = (self.now)();
        let out = match &prior {
            Some(e) if e.image.is_some() => e.image.clone().map_or(Logo::Missing, Logo::Image),
            Some(e) if now < e.meta.retry_after => Logo::Missing,
            _ => self.refresh(&h, url, prior.as_ref()).await,
        };
        drop(guard);
        {
            let mut flights = lock(&self.flights);
            if flights.get(&h).is_some_and(|f| Arc::strong_count(f) <= 2) {
                flights.remove(&h);
            }
        }
        out
    }

    fn spawn_revalidate(self: &Arc<Self>, h: &str, url: &str, prior: &Arc<Entry>) {
        if !lock(&self.revalidating).insert(h.to_owned()) {
            return;
        }
        let me = Arc::clone(self);
        let (h, url, prior) = (h.to_owned(), url.to_owned(), Arc::clone(prior));
        tokio::spawn(async move {
            let _ = me.refresh(&h, &url, Some(&prior)).await;
            lock(&me.revalidating).remove(&h);
        });
    }

    async fn acquire(&self, url: &str) -> Option<Permits> {
        let host = host_of(url);
        let host_sem = Arc::clone(
            lock(&self.hosts)
                .entry(host)
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(PER_HOST))),
        );
        let global = Arc::clone(&self.global);
        let both = async move {
            let h = host_sem.acquire_owned().await.ok()?;
            let g = global.acquire_owned().await.ok()?;
            Some(Permits {
                _host: h,
                _global: g,
            })
        };
        tokio::time::timeout(self.slot_wait, both).await.ok()?
    }

    /// One upstream (conditional) fetch + persist. The result reflects what a
    /// caller should now be served.
    async fn refresh(&self, h: &str, url: &str, prior: Option<&Arc<Entry>>) -> Logo {
        let Some(permits) = self.acquire(url).await else {
            return Logo::Busy;
        };
        let (etag, last_modified) = prior
            .filter(|p| p.image.is_some())
            .map_or((None, None), |p| {
                (p.meta.etag.clone(), p.meta.last_modified.clone())
            });
        let request = FetchRequest {
            url: url.to_owned(),
            max_bytes: MAX_LOGO_BYTES_U64,
            timeout: Duration::from_secs(10),
            etag,
            last_modified,
        };
        let result = self.fetcher.fetch(request, Arc::clone(&self.policy)).await;
        drop(permits);
        let now = (self.now)();
        match result {
            Ok(FetchOutcome::Body {
                mut file,
                content_type,
                etag,
                last_modified,
                ..
            }) => {
                let ctype = content_type.unwrap_or_default();
                if !ALLOWED.contains(&ctype.as_str()) {
                    return self
                        .fail(
                            h,
                            prior,
                            FailKind::Permanent,
                            415,
                            format!("content type {ctype:?} is not an allowed image type"),
                        )
                        .await;
                }
                let mut body = Vec::new();
                if let Err(e) = (&mut file)
                    .take(MAX_LOGO_BYTES_U64 + 1)
                    .read_to_end(&mut body)
                {
                    return self
                        .fail(h, prior, FailKind::Transient, 0, format!("spool: {e}"))
                        .await;
                }
                if body.len() > MAX_LOGO_BYTES {
                    return self
                        .fail(
                            h,
                            prior,
                            FailKind::Permanent,
                            413,
                            "logo exceeds the 512 KiB cap".to_owned(),
                        )
                        .await;
                }
                let meta = Meta {
                    content_type: Some(ctype.clone()),
                    fetched_at: now,
                    etag,
                    last_modified,
                    status: 200,
                    has_body: true,
                    body_etag: Some(body_etag(&body)),
                    ..Meta::default()
                };
                let image = Arc::new(Image {
                    etag: body_etag(&body),
                    content_type: ctype,
                    bytes: body,
                });
                self.commit(h, meta, Some(image)).await
            }
            Ok(FetchOutcome::NotModified) => {
                if let Some(p) = prior.filter(|p| p.image.is_some()) {
                    let mut meta = p.meta.clone();
                    meta.fetched_at = now;
                    meta.status = 200;
                    meta.fail_count = 0;
                    meta.retry_after = 0;
                    meta.last_error = None;
                    self.commit(h, meta, p.image.clone()).await
                } else {
                    self.fail(
                        h,
                        prior,
                        FailKind::Transient,
                        304,
                        "unexpected 304 without a cached copy".to_owned(),
                    )
                    .await
                }
            }
            Err(msg) => {
                let (kind, status) = classify(&msg);
                self.fail(h, prior, kind, status, msg).await
            }
        }
    }

    /// Record a failure in the sidecar. A good cached copy is kept and served.
    async fn fail(
        &self,
        h: &str,
        prior: Option<&Arc<Entry>>,
        kind: FailKind,
        status: u16,
        msg: String,
    ) -> Logo {
        let now = (self.now)();
        let prior_failures = prior.map_or(0, |p| p.meta.fail_count);
        let delay = match kind {
            FailKind::Permanent => PERMANENT_SECS,
            FailKind::Transient => transient_delay(prior_failures),
        };
        let mut meta = prior.map(|p| p.meta.clone()).unwrap_or_default();
        meta.status = status;
        meta.fail_count = prior_failures.saturating_add(1);
        meta.retry_after = now.saturating_add(delay);
        meta.last_error = Some(msg.chars().take(200).collect());
        if prior.and_then(|p| p.image.as_ref()).is_none() {
            meta.has_body = false;
            meta.fetched_at = now;
        }
        let image = prior.and_then(|p| p.image.clone());
        self.commit(h, meta, image).await
    }

    /// Persist `meta` (+ body when `image` is new bytes) and update index/hot set.
    async fn commit(&self, h: &str, meta: Meta, image: Option<Arc<Image>>) -> Logo {
        let write_body = image.as_ref().is_some_and(|i| {
            // Bytes only need rewriting when they changed (or never existed).
            let st = lock(&self.state);
            st.hot
                .get(h)
                .and_then(|e| e.image.as_ref())
                .is_none_or(|old| old.etag != i.etag)
        });
        let dir = self.dir.clone();
        let key = h.to_owned();
        let m = meta.clone();
        let img = image.clone();
        let written = tokio::task::spawn_blocking(move || {
            write_entry(
                &dir,
                &key,
                &m,
                img.as_deref().filter(|_| write_body).map(|i| &i.bytes[..]),
            )
        })
        .await;
        let entry = Arc::new(Entry {
            meta,
            image: image.clone(),
        });
        let victims = {
            let mut st = lock(&self.state);
            if let Ok(Ok((bytes, files))) = written {
                if let Some(old) = st.index.remove(h) {
                    st.order.remove(&old.tick);
                    st.total_bytes = st.total_bytes.saturating_sub(old.bytes);
                    st.total_files = st.total_files.saturating_sub(old.files);
                }
                let tick = st.next_tick;
                st.next_tick += 1;
                st.order.insert(tick, h.to_owned());
                st.total_bytes += bytes;
                st.total_files += files;
                st.index.insert(h.to_owned(), Idx { tick, bytes, files });
            }
            Self::hot_insert(&mut st, h, &entry);
            drop(st);
            self.evict_locked_for(Some(h))
        };
        remove_files(&self.dir, &victims);
        image.map_or(Logo::Missing, Logo::Image)
    }
}

fn remove_files(dir: &Path, hashes: &[String]) {
    for h in hashes {
        let _ = std::fs::remove_file(dir.join(format!("{h}.json")));
        let _ = std::fs::remove_file(dir.join(format!("{h}.bin")));
    }
}

fn read_entry(dir: &Path, h: &str) -> Option<Entry> {
    let json = dir.join(format!("{h}.json"));
    let meta: Meta = serde_json::from_slice(&std::fs::read(&json).ok()?).ok()?;
    let image = if meta.has_body {
        let bytes = std::fs::read(dir.join(format!("{h}.bin"))).ok()?;
        Some(Arc::new(Image {
            etag: meta.body_etag.clone().unwrap_or_else(|| body_etag(&bytes)),
            content_type: meta.content_type.clone()?,
            bytes,
        }))
    } else {
        None
    };
    // Persist recency for the next start (best effort).
    if let Ok(f) = std::fs::File::options().write(true).open(&json) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Some(Entry { meta, image })
}

/// Atomically write `<h>.bin` (when `body`) then the `<h>.json` commit marker.
/// Returns `(bytes, files)` now on disk for the entry.
fn write_entry(
    dir: &Path,
    h: &str,
    meta: &Meta,
    body: Option<&[u8]>,
) -> std::io::Result<(u64, u64)> {
    std::fs::create_dir_all(dir)?;
    let mut files = 1;
    let mut bytes = 0;
    if let Some(b) = body {
        let mut t = tempfile::Builder::new().prefix(".tmp").tempfile_in(dir)?;
        t.write_all(b)?;
        t.persist(dir.join(format!("{h}.bin")))
            .map_err(|e| e.error)?;
    }
    if meta.has_body {
        files += 1;
        bytes += std::fs::metadata(dir.join(format!("{h}.bin")))?.len();
    } else {
        let _ = std::fs::remove_file(dir.join(format!("{h}.bin")));
    }
    let json = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
    let mut t = tempfile::Builder::new().prefix(".tmp").tempfile_in(dir)?;
    t.write_all(&json)?;
    t.persist(dir.join(format!("{h}.json")))
        .map_err(|e| e.error)?;
    bytes += u64::try_from(json.len()).unwrap_or(u64::MAX);
    Ok((bytes, files))
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<LogoCache>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Arc<LogoCache>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install `cache` as the one serving `dir` (integration-test seam: lets a test
/// drive the route over a scripted transport instead of the network).
#[doc(hidden)]
pub fn register(dir: &Path, cache: Arc<LogoCache>) {
    lock(registry()).insert(dir.to_owned(), cache);
}

/// Serve the logo at `url` from the cache rooted at `dir` (the catalogue's
/// `logos/` directory), over the shared screened fetcher.
pub async fn serve(state: &AppState, dir: &Path, url: &str) -> Logo {
    let cache = {
        let mut reg = lock(registry());
        Arc::clone(reg.entry(dir.to_owned()).or_insert_with(|| {
            LogoCache::new(
                dir.to_owned(),
                crate::iptv::fetch::default_fetcher(),
                Arc::clone(&state.dial_policy),
            )
        }))
    };
    cache.get(url).await
}

#[cfg(test)]
mod tests;
