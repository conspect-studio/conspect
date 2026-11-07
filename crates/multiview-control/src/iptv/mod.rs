//! Managed IPTV publication: M3U import, the published playlist and the
//! filtered XMLTV guide (epg-playlist design, `docs/architecture/epg-playlist.md`).
//!
//! Everything here is management-plane tenancy on tokio. No engine command,
//! channel or lock is touched; fetch/decompress/filter work runs on a blocking
//! thread and is bounded in bytes, entries, depth and time (invariants #1/#10).
//!
//! * [`m3u`] — pure, bounded extended-M3U parser (catalogue import).
//! * [`xmltv`] — bounded streaming XMLTV filter (DTD/entity rejection, id
//!   rewrite, 7-day window).
//! * [`fetch`] — the SSRF-screened fetch seam.
//! * [`IptvService`] — the per-process guide cache + status, the refresher
//!   ([`spawn_refresher`]) that only fetches while publication is enabled, and
//!   the atomically swapped plain+gzip guide snapshot.
pub mod fetch;
pub mod m3u;
pub mod xmltv;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{BufReader, Read, Write};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use multiview_config::device::net_guard::DialPolicy;
use multiview_config::{IptvConfig, IptvEntryKind, Output, Source, SourceKind};
use serde::Serialize;
use sha2::Digest;

use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

use self::fetch::{FetchOutcome, FetchRequest, Fetcher};

/// Base refresh interval per feed (30 min), plus up to [`REFRESH_JITTER_SECS`].
pub const REFRESH_SECS: i64 = 30 * 60;
/// Maximum random jitter added to [`REFRESH_SECS`].
pub const REFRESH_JITTER_SECS: i64 = 5 * 60;
/// Minimum gap between two attempts on one feed, even when the wanted set changes.
pub const MIN_ATTEMPT_GAP_SECS: i64 = 60;
/// A feed's last good data is dropped from the guide after this age.
pub const MAX_STALE_SECS: i64 = 48 * 3600;
/// Maximum compressed (on-the-wire) feed size.
pub const MAX_FEED_COMPRESSED_BYTES: u64 = 50 * 1024 * 1024;
/// Maximum expanded feed size.
pub const MAX_FEED_EXPANDED_BYTES: u64 = 200 * 1024 * 1024;
/// Per-feed fetch timeout.
pub const FEED_TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum distinct feeds refreshed.
pub const MAX_FEEDS: usize = 32;
/// How often the refresher re-reads the publication while idle.
pub const PLAN_POLL: Duration = Duration::from_secs(60);

/// One published playlist entry, resolved against the current configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResolvedEntry {
    /// `output` or `source`.
    pub kind: String,
    /// The referenced id.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Published `tvg-id` (also the guide channel id).
    pub tvg_id: String,
    /// Display name.
    pub name: String,
    /// Logo URL, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Group title.
    pub group: String,
    /// The playable URL: an absolute URL for sources, a server path
    /// (`/hls/…`) for outputs. `None` when not playable (see `unavailable`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Why the entry is not published right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// The upstream XMLTV feed (redacted) this entry's guide comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epg_feed: Option<String>,
}

/// The publication resolved from accepted intent: the section plus each entry
/// resolved, and the guide plan (feed url → provider id → published ids).
#[derive(Debug, Clone, Default)]
pub struct Publication {
    /// The `[iptv]` section (default = disabled).
    pub config: IptvConfig,
    /// Entries in order, resolved.
    pub entries: Vec<ResolvedEntry>,
    /// Feed url → (provider channel id → published ids, private-network allowed).
    pub feeds: BTreeMap<String, FeedPlan>,
}

/// One upstream guide feed's wanted mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedPlan {
    /// Provider channel id → published ids.
    pub wanted: BTreeMap<String, Vec<String>>,
    /// Whether every source using this feed opted into private networks.
    pub allow_private_network: bool,
}

impl FeedPlan {
    fn fingerprint(&self) -> String {
        format!("{:?}|{}", self.wanted, self.allow_private_network)
    }
}

impl Publication {
    /// A fingerprint of everything the assembled guide depends on.
    #[must_use]
    pub fn guide_fingerprint(&self) -> String {
        let mut text = String::new();
        for entry in &self.entries {
            if entry.url.is_some() {
                text.push_str(&entry.tvg_id);
                text.push('\u{1}');
                text.push_str(&entry.name);
                text.push('\u{1}');
                text.push_str(entry.logo.as_deref().unwrap_or(""));
                text.push('\u{2}');
            }
        }
        for (url, feed) in &self.feeds {
            text.push_str(url);
            text.push_str(&feed.fingerprint());
        }
        hex_digest(text.as_bytes())
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    digest
        .iter()
        .take(12)
        .fold(String::with_capacity(24), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The strong `ETag` of a publication document.
#[must_use]
pub fn config_etag(config: &IptvConfig) -> String {
    let json = serde_json::to_vec(config).unwrap_or_default();
    format!("\"iptv-{}\"", hex_digest(&json))
}

/// Remove any `userinfo@` from an absolute URL.
#[must_use]
pub fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end).unwrap_or("");
    let tail = rest.get(authority_end..).unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{tail}")
}

/// The upstream URL of a network source kind, if it has one.
fn source_url(kind: &SourceKind) -> Option<&str> {
    match kind {
        SourceKind::Rtsp { url }
        | SourceKind::Hls { url, .. }
        | SourceKind::Youtube { url }
        | SourceKind::Ts { url }
        | SourceKind::Rtmp { url }
        | SourceKind::Srt { url, .. }
        | SourceKind::Rist { url, .. } => Some(url),
        _ => None,
    }
}

/// Map an output id to its URL-segment-safe HLS mount (mirrors the listener's
/// `/hls/{segment}` mount derivation in `multiview-cli` `hls_mounts`).
fn sanitize_mount_segment(id: &str) -> String {
    let segment: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if segment.is_empty() || segment.chars().all(|c| c == '.') {
        "out".to_owned()
    } else {
        segment
    }
}

/// Derive the public playback path of every HLS/LL-HLS output, keyed by its
/// **resource id**, exactly as the listener mounts them: config order,
/// `Output::id()` sanitised, `-2`, `-3`… on collision, playlist file name.
#[must_use]
pub fn hls_paths(outputs: &[(String, Output)]) -> HashMap<String, String> {
    let mut taken = std::collections::HashSet::new();
    let mut paths = HashMap::new();
    for (resource_id, output) in outputs {
        let (Output::Hls { path, .. } | Output::LlHls { path, .. }) = output else {
            continue;
        };
        let mut segment = sanitize_mount_segment(&output.id());
        if !taken.insert(segment.clone()) {
            let mut n: u32 = 2;
            segment = loop {
                let candidate = format!("{segment}-{n}");
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                n = n.saturating_add(1);
            };
        }
        if let Some(file) = std::path::Path::new(path).file_name() {
            let file = file.to_string_lossy();
            paths.insert(
                resource_id.clone(),
                format!("/hls/{segment}/{}", percent_segment(&file)),
            );
        }
    }
    paths
}

fn percent_segment(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

/// Accepted intent relevant to publication: the `[iptv]` section, sources and
/// `(resource id, output)` pairs.
pub type AcceptedInputs = (Option<IptvConfig>, Vec<Source>, Vec<(String, Output)>);

/// Read accepted intent: the `[iptv]` section, sources and output resources.
///
/// # Errors
/// A corrupt accepted record or store failure.
pub fn accepted_inputs(state: &AppState) -> ControlResult<AcceptedInputs> {
    if let Some(model) = &state.boot_model {
        // The in-memory accepted snapshot (review R3-03): playlist, guide
        // and `GET /api/v1/iptv` never re-read `desired.json`.
        if let Some(desired) = model.accepted().map_err(ControlError::Repository)? {
            let outputs = desired
                .output_ids
                .iter()
                .cloned()
                .zip(desired.config.outputs.iter().cloned())
                .collect();
            return Ok((
                desired.config.iptv.clone(),
                desired.config.sources.clone(),
                outputs,
            ));
        }
    }
    let sources = state
        .sources
        .list()?
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Source>(v.resource.body).ok())
        .collect();
    let outputs = state
        .outputs
        .list()?
        .into_iter()
        .filter_map(|v| {
            serde_json::from_value::<Output>(v.resource.body)
                .ok()
                .map(|o| (v.resource.id, o))
        })
        .collect();
    Ok((state.iptv.fallback_config(state), sources, outputs))
}

/// Resolve the publication from accepted intent.
///
/// # Errors
/// A corrupt accepted record or store failure.
pub fn publication(state: &AppState) -> ControlResult<Publication> {
    let (config, sources, outputs) = accepted_inputs(state)?;
    Ok(resolve(config.unwrap_or_default(), &sources, &outputs))
}

/// Resolve entries and the guide plan (pure).
#[must_use]
#[allow(clippy::too_many_lines)] // a flat per-kind resolution
pub fn resolve(
    config: IptvConfig,
    sources: &[Source],
    outputs: &[(String, Output)],
) -> Publication {
    let hls = hls_paths(outputs);
    let mut entries = Vec::with_capacity(config.entries.len());
    let mut feeds: BTreeMap<String, FeedPlan> = BTreeMap::new();
    let mut private_denied: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in &config.entries {
        let mut resolved = ResolvedEntry {
            kind: match entry.kind {
                IptvEntryKind::Output => "output".into(),
                _ => "source".into(),
            },
            reference: entry.reference.clone(),
            tvg_id: entry
                .tvg_id
                .clone()
                .unwrap_or_else(|| entry.reference.clone()),
            name: entry
                .name
                .clone()
                .unwrap_or_else(|| entry.reference.clone()),
            logo: entry.logo.clone(),
            group: entry.group.clone().unwrap_or_default(),
            url: None,
            unavailable: None,
            epg_feed: None,
        };
        if entry.kind == IptvEntryKind::Output {
            {
                if resolved.group.is_empty() {
                    resolved.group = "Outputs".into();
                }
                match outputs.iter().find(|(id, _)| *id == entry.reference) {
                    None => resolved.unavailable = Some("output no longer exists".into()),
                    Some(_) => match hls.get(&entry.reference) {
                        Some(path) => resolved.url = Some(path.clone()),
                        None => {
                            resolved.unavailable = Some(
                                "output is not an HLS/LL-HLS output with a playlist file".into(),
                            );
                        }
                    },
                }
            }
        } else {
            {
                let Some(source) = sources.iter().find(|s| s.id == entry.reference) else {
                    resolved.unavailable = Some("source no longer exists".into());
                    entries.push(resolved);
                    continue;
                };
                let meta = source.playlist_metadata.as_ref();
                if entry.tvg_id.is_none() {
                    if let Some(id) = meta.and_then(|m| m.tvg_id.clone()) {
                        resolved.tvg_id = id;
                    }
                }
                if entry.name.is_none() {
                    if let Some(name) = meta
                        .and_then(|m| m.name.clone())
                        .or_else(|| source.display_name.clone())
                    {
                        resolved.name = name;
                    }
                }
                if resolved.logo.is_none() {
                    resolved.logo = meta.and_then(|m| m.logo.clone());
                }
                if resolved.group.is_empty() {
                    resolved.group = meta
                        .and_then(|m| m.group.clone())
                        .unwrap_or_else(|| "Sources".into());
                }
                if !entry.allow_direct_source_url {
                    resolved.unavailable =
                        Some("direct source URL publication is not allowed for this entry".into());
                } else if let Some(url) = source_url(&source.kind) {
                    resolved.url = Some(strip_userinfo(url));
                } else {
                    resolved.unavailable = Some("source kind has no network URL".into());
                }
                if resolved.url.is_some() {
                    if let Some(epg) = &source.epg {
                        if epg.mode != multiview_config::EpgMode::Disabled {
                            if let (Some(url), Some(channel)) = (&epg.url, &epg.channel_id) {
                                let feed = feeds.entry(url.clone()).or_insert_with(|| FeedPlan {
                                    allow_private_network: true,
                                    ..FeedPlan::default()
                                });
                                if !epg.allow_private_network {
                                    private_denied.insert(url.clone());
                                }
                                let targets = feed.wanted.entry(channel.clone()).or_default();
                                if !targets.contains(&resolved.tvg_id) {
                                    targets.push(resolved.tvg_id.clone());
                                }
                                resolved.epg_feed = Some(fetch::redact_url(url));
                            }
                        }
                    }
                }
            }
        }
        entries.push(resolved);
    }
    for url in private_denied {
        if let Some(feed) = feeds.get_mut(&url) {
            feed.allow_private_network = false;
        }
    }
    Publication {
        config,
        entries,
        feeds,
    }
}

/// Quote-safe attribute text for an `#EXTINF` line.
fn m3u_attr(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '"' || c.is_control() { '\'' } else { c })
        .collect()
}

/// Render the published extended-M3U playlist. `base` is
/// `scheme://host[:port]` used for server-relative output paths and the guide URL.
#[must_use]
pub fn render_playlist(publication: &Publication, base: &str) -> String {
    let mut out =
        format!("#EXTM3U url-tvg=\"{base}/iptv/guide.xml\" x-tvg-url=\"{base}/iptv/guide.xml\"\n");
    for entry in &publication.entries {
        let Some(url) = &entry.url else { continue };
        let url = if url.starts_with('/') {
            format!("{base}{url}")
        } else {
            url.clone()
        };
        if url.chars().any(char::is_control) {
            continue;
        }
        let _ = write!(
            out,
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\"",
            m3u_attr(&entry.tvg_id),
            m3u_attr(&entry.name)
        );
        if let Some(logo) = &entry.logo {
            let _ = write!(out, " tvg-logo=\"{}\"", m3u_attr(logo));
        }
        let _ = write!(out, " group-title=\"{}\"", m3u_attr(&entry.group));
        let name: String = entry
            .name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        out.push(',');
        out.push_str(&name);
        out.push('\n');
        out.push_str(&url);
        out.push('\n');
    }
    out
}

/// One feed's operator-visible status.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FeedStatus {
    /// The feed URL with credentials and query removed.
    pub url: String,
    /// Unix seconds of the last attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<i64>,
    /// Unix seconds of the last successful fetch (or 304).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success: Option<i64>,
    /// Unix seconds of the next scheduled attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt: Option<i64>,
    /// The last error, if the last attempt failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Channels kept from this feed.
    pub channels: usize,
    /// Programmes kept from this feed.
    pub programmes: usize,
}

/// The guide snapshot served at `/iptv/guide.xml`.
#[derive(Debug, Clone, Default)]
pub struct GuideSnapshot {
    /// Plain XMLTV.
    pub plain: bytes::Bytes,
    /// Gzip of [`plain`](Self::plain).
    pub gzip: bytes::Bytes,
    /// Strong `ETag` (of the plain body).
    pub etag: String,
    /// Unix seconds when assembled.
    pub generated_at: i64,
    /// The publication fingerprint this snapshot was assembled for.
    pub fingerprint: String,
}

#[derive(Debug, Default)]
struct FeedCache {
    fingerprint: String,
    fragment: Option<Arc<xmltv::Fragment>>,
    etag: Option<String>,
    last_modified: Option<String>,
    status: FeedStatus,
    next_due: i64,
}

/// The store-only (no boot model) publication slot.
#[derive(Debug, Clone, Default)]
enum Fallback {
    /// Never written: read the startup base document.
    #[default]
    Unset,
    /// Written by `PUT /api/v1/iptv`.
    Set(Option<IptvConfig>),
}

/// The per-process IPTV runtime: fetch seam, feed cache, snapshot, wake signal.
pub struct IptvService {
    fetcher: Arc<dyn Fetcher>,
    feeds: Mutex<HashMap<String, FeedCache>>,
    snapshot: RwLock<Option<Arc<GuideSnapshot>>>,
    wake: tokio::sync::Notify,
    /// Store-only deployments (no boot model): the in-memory section, once set.
    fallback: RwLock<Fallback>,
    assemble_lock: Mutex<()>,
}

impl std::fmt::Debug for IptvService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IptvService").finish_non_exhaustive()
    }
}

impl Default for IptvService {
    fn default() -> Self {
        Self::new(fetch::default_fetcher())
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The base refresh interval for `config` in seconds (`[iptv]
/// guide_refresh_minutes`, default 30 min).
#[must_use]
pub fn refresh_secs(config: &IptvConfig) -> i64 {
    i64::from(config.guide_refresh_minutes()).saturating_mul(60)
}

/// Random jitter for a `base`-second interval: up to [`REFRESH_JITTER_SECS`],
/// and never more than a sixth of the interval, so a short interval stays
/// close to what the operator set.
fn jitter(base: i64) -> i64 {
    let span = REFRESH_JITTER_SECS.min(base / 6).max(1);
    let random = uuid::Uuid::new_v4().as_u128();
    i64::try_from(random % u128::try_from(span).unwrap_or(1)).unwrap_or(0)
}

impl IptvService {
    /// A service over `fetcher`.
    #[must_use]
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            fetcher,
            feeds: Mutex::new(HashMap::new()),
            snapshot: RwLock::new(None),
            wake: tokio::sync::Notify::new(),
            fallback: RwLock::new(Fallback::Unset),
            assemble_lock: Mutex::new(()),
        }
    }

    /// The fetch seam (shared with M3U import).
    #[must_use]
    pub fn fetcher(&self) -> Arc<dyn Fetcher> {
        Arc::clone(&self.fetcher)
    }

    /// Wake the refresher (publication changed).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn fallback_config(&self, state: &AppState) -> Option<IptvConfig> {
        if let Fallback::Set(stored) = self
            .fallback
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return stored;
        }
        state
            .base_document
            .as_deref()
            .and_then(|doc| doc.get("iptv"))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// Replace the store-only fallback section (no boot model).
    pub fn set_fallback(&self, config: Option<IptvConfig>) {
        *self
            .fallback
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Fallback::Set(config);
    }

    /// The current snapshot, if any.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<GuideSnapshot>> {
        self.snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Per-feed status for the given publication, in feed order.
    #[must_use]
    pub fn status(&self, publication: &Publication) -> Vec<FeedStatus> {
        let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        publication
            .feeds
            .keys()
            .map(|url| {
                feeds.get(url).map_or_else(
                    || FeedStatus {
                        url: fetch::redact_url(url),
                        ..FeedStatus::default()
                    },
                    |cache| cache.status.clone(),
                )
            })
            .collect()
    }

    /// The snapshot for `publication`, re-assembled from cached fragments
    /// (no fetch) when it no longer matches — so a deleted or edited entry is
    /// never served from a stale snapshot.
    ///
    /// # Errors
    /// Snapshot compression failure.
    pub fn current_snapshot(
        &self,
        publication: &Publication,
    ) -> std::io::Result<Arc<GuideSnapshot>> {
        let fingerprint = publication.guide_fingerprint();
        if let Some(snapshot) = self.snapshot() {
            if snapshot.fingerprint == fingerprint {
                return Ok(snapshot);
            }
        }
        self.wake();
        self.assemble(publication, now_unix())
    }

    /// Assemble and atomically swap a snapshot for `publication`.
    ///
    /// # Errors
    /// Snapshot compression failure.
    pub fn assemble(
        &self,
        publication: &Publication,
        now: i64,
    ) -> std::io::Result<Arc<GuideSnapshot>> {
        let _serial = self
            .assemble_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let fragments: Vec<Arc<xmltv::Fragment>> = {
            let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
            publication
                .feeds
                .keys()
                .filter_map(|url| feeds.get(url))
                .filter(|cache| {
                    cache
                        .status
                        .last_success
                        .is_some_and(|at| now - at <= MAX_STALE_SECS)
                })
                .filter_map(|cache| cache.fragment.clone())
                .collect()
        };
        let mut plain = Vec::with_capacity(4096);
        plain.extend_from_slice(
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"multiview\">\n",
        );
        let mut channels_done = std::collections::HashSet::new();
        let published: Vec<&ResolvedEntry> = publication
            .entries
            .iter()
            .filter(|e| e.url.is_some())
            .collect();
        for entry in &published {
            if !channels_done.insert(entry.tvg_id.clone()) {
                continue;
            }
            let upstream = fragments
                .iter()
                .filter_map(|f| f.targets.get(&entry.tvg_id))
                .find(|t| !t.channel.is_empty());
            if let Some(target) = upstream {
                plain.extend_from_slice(&target.channel);
            } else {
                plain.extend_from_slice(
                    format!(
                        "<channel id=\"{}\"><display-name>{}</display-name>{}</channel>\n",
                        xmltv::escape(&entry.tvg_id),
                        xmltv::escape(&entry.name),
                        entry
                            .logo
                            .as_deref()
                            .map_or_else(String::new, |logo| format!(
                                "<icon src=\"{}\"/>",
                                xmltv::escape(logo)
                            )),
                    )
                    .as_bytes(),
                );
            }
        }
        let mut programmes_done = std::collections::HashSet::new();
        for entry in &published {
            if !programmes_done.insert(entry.tvg_id.clone()) {
                continue;
            }
            // First feed that supplies programmes for this id wins (no duplicates).
            if let Some(target) = fragments
                .iter()
                .filter_map(|f| f.targets.get(&entry.tvg_id))
                .find(|t| t.programme_count > 0)
            {
                plain.extend_from_slice(&target.programmes);
            }
        }
        plain.extend_from_slice(b"</tv>\n");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&plain)?;
        let gzip = encoder.finish()?;
        let snapshot = Arc::new(GuideSnapshot {
            etag: format!("\"guide-{}\"", hex_digest(&plain)),
            plain: bytes::Bytes::from(plain),
            gzip: bytes::Bytes::from(gzip),
            generated_at: now,
            fingerprint: publication.guide_fingerprint(),
        });
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// One refresh pass at `now`: fetch every due feed of an **enabled**
    /// publication (sequentially), then re-assemble. A disabled publication
    /// fetches nothing and drops all cached guide data.
    ///
    /// Returns the number of fetch attempts made.
    #[allow(clippy::too_many_lines)] // one sequential per-feed pass; split hides the cache protocol
    pub async fn refresh_once(
        &self,
        publication: &Publication,
        dial_policy: &Arc<DialPolicy>,
        now: i64,
    ) -> usize {
        if !publication.config.enabled {
            self.feeds
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            *self
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner) = None;
            return 0;
        }
        // Forget feeds no longer published.
        {
            let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
            feeds.retain(|url, _| publication.feeds.contains_key(url));
        }
        let mut attempts = 0;
        for (index, (url, plan)) in publication.feeds.iter().enumerate() {
            let fingerprint = plan.fingerprint();
            let (due, conditional) = {
                let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
                let cache = feeds.entry(url.clone()).or_insert_with(|| FeedCache {
                    status: FeedStatus {
                        url: fetch::redact_url(url),
                        ..FeedStatus::default()
                    },
                    ..FeedCache::default()
                });
                if index >= MAX_FEEDS {
                    cache.status.error = Some(format!(
                        "more than {MAX_FEEDS} distinct guide feeds; not fetched"
                    ));
                    continue;
                }
                let gap_ok = cache
                    .status
                    .last_attempt
                    .is_none_or(|at| now - at >= MIN_ATTEMPT_GAP_SECS);
                let changed = cache.fingerprint != fingerprint;
                let due = gap_ok && (changed || now >= cache.next_due);
                let conditional =
                    (!changed).then(|| (cache.etag.clone(), cache.last_modified.clone()));
                (due, conditional)
            };
            if !due {
                continue;
            }
            attempts += 1;
            let policy = if plan.allow_private_network {
                Arc::clone(dial_policy)
            } else {
                Arc::new(DialPolicy::deny_all_allowlistable())
            };
            let (etag, last_modified) = conditional.unwrap_or((None, None));
            let request = FetchRequest {
                url: url.clone(),
                max_bytes: MAX_FEED_COMPRESSED_BYTES,
                timeout: FEED_TIMEOUT,
                etag,
                last_modified,
            };
            let result = match self.fetcher.fetch(request, policy).await {
                Ok(FetchOutcome::NotModified) => Ok(None),
                Ok(FetchOutcome::Body {
                    file,
                    etag,
                    last_modified,
                    ..
                }) => {
                    let wanted: HashMap<String, Vec<String>> = plan
                        .wanted
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let window = xmltv::Window::seven_days_from(now);
                    tokio::task::spawn_blocking(move || filter_spooled(file, &wanted, window))
                        .await
                        .map_err(|e| format!("filter task failed: {e}"))
                        .and_then(|r| r)
                        .map(|fragment| Some((fragment, etag, last_modified)))
                }
                Err(error) => Err(error),
            };
            let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(cache) = feeds.get_mut(url) else {
                continue;
            };
            cache.status.last_attempt = Some(now);
            let base = refresh_secs(&publication.config);
            cache.next_due = now + base + jitter(base);
            cache.status.next_attempt = Some(cache.next_due);
            match result {
                Ok(None) => {
                    cache.status.last_success = Some(now);
                    cache.status.error = None;
                }
                Ok(Some((fragment, etag, last_modified))) => {
                    cache.status.channels = fragment.channel_count;
                    cache.status.programmes = fragment.programme_count;
                    cache.status.last_success = Some(now);
                    cache.status.error = None;
                    cache.fragment = Some(Arc::new(fragment));
                    cache.fingerprint = fingerprint;
                    cache.etag = etag;
                    cache.last_modified = last_modified;
                }
                Err(error) => {
                    tracing::warn!(feed = %cache.status.url, %error, "IPTV guide refresh failed; keeping the last good guide");
                    cache.status.error = Some(error);
                }
            }
        }
        if let Err(error) = self.assemble(publication, now) {
            tracing::warn!(%error, "IPTV guide assembly failed; keeping the previous snapshot");
        }
        attempts
    }
}

/// A `Read` that fails once more than `limit` bytes were read.
struct CappedRead<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for CappedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let n64 = u64::try_from(n).unwrap_or(u64::MAX);
        if n64 > self.remaining {
            return Err(std::io::Error::other(format!(
                "expanded guide exceeds {MAX_FEED_EXPANDED_BYTES} bytes"
            )));
        }
        self.remaining -= n64;
        Ok(n)
    }
}

/// Decompress (gzip by magic) and filter a spooled feed body. Blocking.
///
/// # Errors
/// An operator-facing reason.
pub fn filter_spooled<S: std::hash::BuildHasher>(
    mut file: std::fs::File,
    wanted: &HashMap<String, Vec<String>, S>,
    window: xmltv::Window,
) -> Result<xmltv::Fragment, String> {
    use std::io::Seek;
    let mut magic = [0u8; 2];
    let read = file.read(&mut magic).map_err(|e| e.to_string())?;
    file.rewind().map_err(|e| e.to_string())?;
    let gzip = read == 2 && magic == [0x1f, 0x8b];
    let reader: Box<dyn Read> = if gzip {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let capped = CappedRead {
        inner: reader,
        remaining: MAX_FEED_EXPANDED_BYTES,
    };
    xmltv::filter_xmltv(
        BufReader::with_capacity(64 * 1024, capped),
        wanted,
        window,
        xmltv::FilterLimits::default(),
    )
    .map_err(|e| e.to_string())
}

/// Spawn the guide refresher: a single control-plane task that sleeps while
/// publication is disabled (re-reading accepted intent every [`PLAN_POLL`] or
/// on a wake) and, while enabled, refreshes due feeds and swaps the snapshot.
#[must_use = "the refresher runs until the handle is aborted or the runtime stops"]
pub fn spawn_refresher(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match publication(&state) {
                Ok(publication) => {
                    let now = now_unix();
                    state
                        .iptv
                        .refresh_once(&publication, &state.dial_policy, now)
                        .await;
                }
                Err(error) => tracing::warn!(%error, "IPTV refresher cannot read accepted intent"),
            }
            let _ = tokio::time::timeout(PLAN_POLL, state.iptv.wake.notified()).await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use multiview_config::IptvEntry;

    fn source(json: serde_json::Value) -> Source {
        serde_json::from_value(json).unwrap()
    }

    fn sample() -> (IptvConfig, Vec<Source>, Vec<(String, Output)>) {
        let config: IptvConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "entries": [
                {"kind": "output", "ref": "program", "name": "Program"},
                {"kind": "source", "ref": "cnn", "allow_direct_source_url": true},
                {"kind": "source", "ref": "bars", "allow_direct_source_url": true},
                {"kind": "output", "ref": "gone"},
            ]
        }))
        .unwrap();
        let sources = vec![
            source(serde_json::json!({
                "id": "cnn", "kind": "hls", "url": "https://user:pw@cdn.example/cnn/index.m3u8?t=1",
                "epg": {"mode": "manual", "url": "https://epg.example/g.xml", "channel_id": "464857"},
                "playlist_metadata": {"tvg_id": "CNN.us", "name": "CNN \"US\"", "group": "News"}
            })),
            source(serde_json::json!({"id": "bars", "kind": "bars"})),
        ];
        let outputs = vec![(
            "program".to_owned(),
            serde_json::from_value(serde_json::json!({
                "kind": "hls", "id": "program", "path": "/var/hls/program.m3u8", "codec": "h264", "segment_ms": 2000
            }))
            .unwrap(),
        )];
        (config, sources, outputs)
    }

    #[test]
    fn playlist_renders_outputs_and_sources_without_credentials() {
        let (config, sources, outputs) = sample();
        let publication = resolve(config, &sources, &outputs);
        let text = render_playlist(&publication, "https://mv.example:8443");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "#EXTM3U url-tvg=\"https://mv.example:8443/iptv/guide.xml\" x-tvg-url=\"https://mv.example:8443/iptv/guide.xml\""
        );
        assert_eq!(
            lines[1],
            "#EXTINF:-1 tvg-id=\"program\" tvg-name=\"Program\" group-title=\"Outputs\",Program"
        );
        assert_eq!(lines[2], "https://mv.example:8443/hls/program/program.m3u8");
        assert_eq!(
            lines[3],
            "#EXTINF:-1 tvg-id=\"CNN.us\" tvg-name=\"CNN 'US'\" group-title=\"News\",CNN \"US\""
        );
        assert_eq!(lines[4], "https://cdn.example/cnn/index.m3u8?t=1");
        assert_eq!(lines.len(), 5, "{text}");
        assert!(!text.contains("pw@"));
        let bars = &publication.entries[2];
        assert!(bars
            .unavailable
            .as_deref()
            .unwrap()
            .contains("no network URL"));
        assert!(publication.entries[3].unavailable.is_some());
        assert_eq!(
            publication.feeds["https://epg.example/g.xml"].wanted["464857"],
            vec!["CNN.us".to_owned()]
        );
    }

    #[test]
    fn source_without_permission_is_not_published() {
        let (mut config, sources, outputs) = sample();
        config.entries = vec![serde_json::from_value::<IptvEntry>(
            serde_json::json!({"kind": "source", "ref": "cnn"}),
        )
        .unwrap()];
        let publication = resolve(config, &sources, &outputs);
        assert!(publication.entries[0].url.is_none());
        assert!(publication.feeds.is_empty());
    }

    #[test]
    fn hls_paths_mirror_listener_mounts() {
        let outputs: Vec<(String, Output)> = [
            (
                "output-0",
                r#"{"kind":"hls","path":"/a/x.m3u8","codec":"h264","segment_ms":1000,"id":"a b"}"#,
            ),
            (
                "output-1",
                r#"{"kind":"hls","path":"/b/y.m3u8","codec":"h264","segment_ms":1000,"id":"a-b"}"#,
            ),
        ]
        .iter()
        .map(|(id, json)| ((*id).to_owned(), serde_json::from_str(json).unwrap()))
        .collect();
        let paths = hls_paths(&outputs);
        assert_eq!(paths["output-0"], "/hls/a-b/x.m3u8");
        assert_eq!(paths["output-1"], "/hls/a-b-2/y.m3u8");
    }

    #[test]
    fn userinfo_is_stripped() {
        assert_eq!(strip_userinfo("rtsp://u:p@cam/s?x=@y"), "rtsp://cam/s?x=@y");
        assert_eq!(strip_userinfo("https://h/p"), "https://h/p");
    }

    struct FakeFetcher {
        body: Mutex<Result<Vec<u8>, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, request: FetchRequest, _policy: Arc<DialPolicy>) -> fetch::FetchFuture<'_> {
            self.calls.lock().unwrap().push(request.url);
            let body = self.body.lock().unwrap().clone();
            Box::pin(async move {
                let body = body?;
                let mut file = tempfile::tempfile().map_err(|e| e.to_string())?;
                file.write_all(&body).map_err(|e| e.to_string())?;
                std::io::Seek::rewind(&mut file).map_err(|e| e.to_string())?;
                Ok(FetchOutcome::Body {
                    file,
                    len: u64::try_from(body.len()).unwrap(),
                    etag: None,
                    last_modified: None,
                    content_type: None,
                })
            })
        }
    }

    const NOW: i64 = 1_793_000_000;

    fn guide_xml() -> Vec<u8> {
        // 2026-10-27 07:00 → 08:00 UTC covers NOW (07:33:20).
        br#"<?xml version="1.0"?><tv><channel id="464857"><display-name>CNN</display-name></channel>
<programme start="20261027070000 +0000" stop="20261027080000 +0000" channel="464857"><title>Live</title></programme>
<programme start="20261027070000 +0000" stop="20261027080000 +0000" channel="1"><title>Other</title></programme></tv>"#
            .to_vec()
    }

    #[tokio::test]
    async fn the_background_refresh_follows_guide_refresh_minutes() {
        let (mut config, sources, outputs) = sample();
        config.guide_refresh_minutes = Some(10);
        let publication = resolve(config, &sources, &outputs);
        let fake = Arc::new(FakeFetcher {
            body: Mutex::new(Ok(guide_xml())),
            calls: Mutex::new(Vec::new()),
        });
        let service = IptvService::new(fake.clone());
        let policy = Arc::new(DialPolicy::allow_lan());
        // Fetched without any guide request: the refresher runs while enabled.
        assert_eq!(service.refresh_once(&publication, &policy, NOW).await, 1);
        let due = service.status(&publication)[0].next_attempt.unwrap();
        // 10 min plus at most a sixth of it as jitter.
        assert!((NOW + 600..=NOW + 700).contains(&due), "{}", due - NOW);
        assert_eq!(
            service.refresh_once(&publication, &policy, NOW + 599).await,
            0
        );
        assert_eq!(
            service.refresh_once(&publication, &policy, NOW + 701).await,
            1
        );
    }

    #[tokio::test]
    async fn refresher_fetches_filters_and_keeps_last_good() {
        let (config, sources, outputs) = sample();
        let publication = resolve(config, &sources, &outputs);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&guide_xml()).unwrap();
        let fake = Arc::new(FakeFetcher {
            body: Mutex::new(Ok(gz.finish().unwrap())),
            calls: Mutex::new(Vec::new()),
        });
        let service = IptvService::new(fake.clone());
        let policy = Arc::new(DialPolicy::allow_lan());
        assert_eq!(service.refresh_once(&publication, &policy, NOW).await, 1);
        let snap = service.snapshot().unwrap();
        let text = String::from_utf8(snap.plain.to_vec()).unwrap();
        assert!(
            text.contains(r#"<channel id="CNN.us"><display-name>CNN</display-name></channel>"#),
            "{text}"
        );
        assert!(text.contains(r#"channel="CNN.us""#));
        assert!(!text.contains("Other"));
        // The output entry gets a synthesized channel.
        assert!(text
            .contains(r#"<channel id="program"><display-name>Program</display-name></channel>"#));
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(&snap.gzip[..])
            .read_to_end(&mut plain)
            .unwrap();
        assert_eq!(plain, snap.plain.to_vec());
        let status = service.status(&publication);
        assert_eq!(status[0].programmes, 1);
        assert_eq!(status[0].error, None);

        // Not due again within the 30-minute interval.
        assert_eq!(
            service.refresh_once(&publication, &policy, NOW + 600).await,
            0
        );

        // A failing refresh keeps the last good guide and reports the error.
        *fake.body.lock().unwrap() = Ok(b"<!DOCTYPE tv><tv/>".to_vec());
        assert_eq!(
            service
                .refresh_once(
                    &publication,
                    &policy,
                    NOW + REFRESH_SECS + REFRESH_JITTER_SECS
                )
                .await,
            1
        );
        let again = service.snapshot().unwrap();
        assert_eq!(again.plain, snap.plain);
        assert!(service.status(&publication)[0]
            .error
            .as_deref()
            .unwrap()
            .contains("DOCTYPE"));
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn disabled_publication_never_fetches() {
        let (mut config, sources, outputs) = sample();
        config.enabled = false;
        let publication = resolve(config, &sources, &outputs);
        let fake = Arc::new(FakeFetcher {
            body: Mutex::new(Ok(guide_xml())),
            calls: Mutex::new(Vec::new()),
        });
        let service = IptvService::new(fake.clone());
        let policy = Arc::new(DialPolicy::allow_lan());
        assert_eq!(service.refresh_once(&publication, &policy, NOW).await, 0);
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(service.snapshot().is_none());
    }

    #[tokio::test]
    async fn removing_an_entry_reassembles_without_refetch() {
        let (config, sources, outputs) = sample();
        let publication = resolve(config.clone(), &sources, &outputs);
        let fake = Arc::new(FakeFetcher {
            body: Mutex::new(Ok(guide_xml())),
            calls: Mutex::new(Vec::new()),
        });
        let service = IptvService::new(fake.clone());
        service
            .refresh_once(&publication, &Arc::new(DialPolicy::allow_lan()), NOW)
            .await;
        let mut smaller = config;
        smaller.entries.retain(|e| e.reference != "cnn");
        let next = resolve(smaller, &sources, &outputs);
        let snap = service.current_snapshot(&next).unwrap();
        let text = String::from_utf8(snap.plain.to_vec()).unwrap();
        assert!(!text.contains("CNN"), "{text}");
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn expanded_size_is_capped() {
        // A gzip bomb: 1 MiB of zeros compresses tiny; cap check via CappedRead.
        let mut capped = CappedRead {
            inner: std::io::repeat(0).take(1024),
            remaining: 100,
        };
        let mut sink = Vec::new();
        assert!(capped.read_to_end(&mut sink).is_err());
    }
}
